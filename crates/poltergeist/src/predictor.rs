//! Predictor client (SRS 5.3, 6.3): baseline fallback, Unix-socket sidecar,
//! prediction cache, degraded flag and sidecar supervision.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::config::{self, Config};
use crate::features::{self, Counts};

/// Baseline predictor (FR-24): per-family Beta(1, 1) smoothed failure rate.
pub fn baseline_p(family: Counts, global: Counts) -> f64 {
    if family.n > 0 {
        family.smoothed()
    } else {
        global.smoothed()
    }
}

/// Cache key for FR-20: `(family, cwd_id, hour_bucket)`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub family: String,
    pub cwd_id: String,
    pub hour_bucket: u8,
}

impl Key {
    pub fn new(family: &str, cwd_id: &str, hour: u32) -> Self {
        Self {
            family: family.to_string(),
            cwd_id: cwd_id.to_string(),
            hour_bucket: (hour.min(23) / 3) as u8,
        }
    }
}

enum Job {
    Predict {
        key: Key,
        features: Vec<f64>,
    },
    Refit {
        db: PathBuf,
        limit: u32,
        exclude: Vec<i64>,
    },
}

/// Async predictor. `predict` only reads the cache; misses are sent to a
/// worker thread and never block the hook path (FR-21).
pub struct Client {
    cache: Arc<Mutex<HashMap<Key, f64>>>,
    degraded: Arc<AtomicBool>,
    tx: Option<Sender<Job>>,
}

impl Client {
    pub fn new(cfg: &Config) -> Self {
        let cache = Arc::new(Mutex::new(HashMap::new()));
        let degraded = Arc::new(AtomicBool::new(true));
        if cfg.predictor.mode == "baseline" {
            return Self {
                cache,
                degraded,
                tx: None,
            };
        }
        let (tx, rx) = mpsc::channel();
        let socket = config::sidecar_socket_path();
        let timeout = Duration::from_millis(cfg.predictor.predict_timeout_ms.max(1));
        let worker_cache = cache.clone();
        let worker_degraded = degraded.clone();
        std::thread::spawn(move || worker(rx, worker_cache, worker_degraded, socket, timeout));
        Self {
            cache,
            degraded,
            tx: Some(tx),
        }
    }

    pub fn enabled(&self) -> bool {
        self.tx.is_some()
    }

    pub fn degraded(&self) -> bool {
        self.degraded.load(Ordering::Relaxed)
    }

    pub fn cached(&self, key: &Key) -> Option<f64> {
        self.cache.lock().ok().and_then(|c| c.get(key).copied())
    }

    /// Cache hit or `None`. A miss is queued for the sidecar.
    pub fn request(&self, key: Key, features: Vec<f64>) -> Option<f64> {
        if let Some(hit) = self.cached(&key) {
            return Some(hit);
        }
        if let Some(tx) = &self.tx {
            let _ = tx.send(Job::Predict { key, features });
        }
        None
    }

    /// Ask the worker to rebuild the sidecar model from the event store.
    pub fn refit(&self, db: PathBuf, limit: u32, exclude: Vec<i64>) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Job::Refit { db, limit, exclude });
        }
    }
}

fn worker(
    rx: Receiver<Job>,
    cache: Arc<Mutex<HashMap<Key, f64>>>,
    degraded: Arc<AtomicBool>,
    socket: PathBuf,
    predict_timeout: Duration,
) {
    let mut conn: Option<BufReader<UnixStream>> = None;
    let mut last_health = Instant::now();
    loop {
        let job = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(job) => job,
            Err(RecvTimeoutError::Timeout) => {
                if last_health.elapsed() >= Duration::from_secs(5) {
                    last_health = Instant::now();
                    match call(
                        &mut conn,
                        &socket,
                        &json!({"type":"health"}),
                        predict_timeout,
                    ) {
                        Ok(reply) => degraded.store(
                            reply["degraded"].as_bool().unwrap_or(false),
                            Ordering::Relaxed,
                        ),
                        Err(_) => {
                            degraded.store(true, Ordering::Relaxed);
                            conn = None;
                        }
                    }
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };

        match job {
            Job::Predict { key, features } => {
                let msg = json!({"type":"predict","features":features});
                match call(&mut conn, &socket, &msg, predict_timeout) {
                    Ok(reply) => match reply["p_fail"].as_f64() {
                        Some(p) => {
                            degraded.store(false, Ordering::Relaxed);
                            if let Ok(mut cache) = cache.lock() {
                                if cache.len() > 4096 {
                                    cache.clear();
                                }
                                cache.insert(key, p.clamp(0.0, 1.0));
                            }
                        }
                        None => degraded.store(true, Ordering::Relaxed),
                    },
                    Err(_) => {
                        degraded.store(true, Ordering::Relaxed);
                        conn = None;
                    }
                }
            }
            Job::Refit { db, limit, exclude } => {
                let db_conn = match crate::store::open(&db) {
                    Ok(conn) => conn,
                    Err(e) => {
                        tracing::warn!(error = %e, "refit: opening the event store failed");
                        continue;
                    }
                };
                let rows = match features::training_samples(&db_conn, limit as usize, &exclude) {
                    Ok(samples) => samples,
                    Err(e) => {
                        tracing::warn!(error = %e, "refit: reading history failed");
                        continue;
                    }
                };
                if rows.len() < 2 {
                    continue;
                }
                let labels: Vec<i64> = rows.iter().map(|s| s.label).collect();
                let features: Vec<[f64; features::N_FEATURES]> =
                    rows.iter().map(|s| s.features).collect();
                let msg = json!({"type":"refit","rows":features,"labels":labels});
                match call(&mut conn, &socket, &msg, Duration::from_secs(300)) {
                    Ok(reply) => {
                        let ok = reply["degraded"].as_bool() == Some(false);
                        degraded.store(!ok, Ordering::Relaxed);
                        if ok {
                            tracing::info!(rows = rows.len(), "sidecar refit ok");
                            if let Ok(mut cache) = cache.lock() {
                                cache.clear();
                            }
                        } else {
                            tracing::warn!(
                                error = reply["error"].as_str().unwrap_or("unknown"),
                                "sidecar refit degraded"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "sidecar refit failed");
                        degraded.store(true, Ordering::Relaxed);
                        conn = None;
                    }
                }
            }
        }
    }
}

/// One newline-delimited JSON round trip. Reconnects on demand.
fn call(
    conn: &mut Option<BufReader<UnixStream>>,
    socket: &Path,
    msg: &Value,
    timeout: Duration,
) -> Result<Value> {
    if conn.is_none() {
        let stream = UnixStream::connect(socket)
            .with_context(|| format!("connecting {}", socket.display()))?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(Duration::from_millis(500)))?;
        *conn = Some(BufReader::new(stream));
    }
    let reader = conn.as_mut().expect("connected above");
    reader.get_mut().set_read_timeout(Some(timeout))?;
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    reader.get_mut().write_all(line.as_bytes())?;
    reader.get_mut().flush()?;
    let mut reply = String::new();
    if reader.read_line(&mut reply)? == 0 {
        bail!("sidecar closed the connection");
    }
    serde_json::from_str(&reply).context("decoding sidecar reply")
}

/// Directory containing `sidecar/pyproject.toml`, if discoverable.
pub fn sidecar_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("POLTERGEIST_SIDECAR_DIR") {
        let dir = PathBuf::from(dir);
        if dir.join("pyproject.toml").exists() {
            return Some(dir);
        }
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            roots.push(parent.to_path_buf());
        }
    }
    for root in roots {
        for anc in root.ancestors().take(6) {
            let candidate = anc.join("sidecar");
            if candidate.join("pyproject.toml").exists() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Spawn the sidecar once; the caller polls the socket for readiness.
pub fn ensure_sidecar() -> Option<PathBuf> {
    let socket = config::sidecar_socket_path();
    let dir = sidecar_dir()?;
    if config::socket_alive(&socket) {
        return Some(dir);
    }
    if let Some(parent) = socket.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let python = std::env::var("POLTERGEIST_PYTHON").unwrap_or_else(|_| "python3".into());
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(config::data_dir().join("sidecar.log"))
        .ok();
    let mut cmd = Command::new(python);
    cmd.current_dir(&dir)
        .arg("-m")
        .arg("poltergeist_sidecar")
        .arg("--socket")
        .arg(&socket)
        .stdin(Stdio::null());
    match log {
        Some(log) => {
            let err = log.try_clone().ok();
            cmd.stdout(log);
            if let Some(err) = err {
                cmd.stderr(err);
            }
        }
        None => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    match cmd.spawn() {
        Ok(_) => Some(dir),
        Err(e) => {
            tracing::warn!(error = %e, "could not spawn sidecar");
            None
        }
    }
}

/// Refit the sidecar on `train` and score `test` (used by `poltergeist eval`).
/// Starts the sidecar if needed and waits briefly for it to accept connections.
pub fn eval_tabpfn(
    train: &[[f64; features::N_FEATURES]],
    labels: &[i64],
    test: &[[f64; features::N_FEATURES]],
) -> Result<Vec<f64>> {
    let socket = config::sidecar_socket_path();
    let spawned = ensure_sidecar();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !config::socket_alive(&socket) {
        if Instant::now() >= deadline {
            bail!(
                "sidecar unavailable (socket {}){}",
                socket.display(),
                if spawned.is_none() {
                    "; sidecar/ not found or python3 missing"
                } else {
                    ""
                }
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let mut conn = None;
    let msg = json!({"type":"refit","rows":train,"labels":labels});
    let reply = call(&mut conn, &socket, &msg, Duration::from_secs(300))?;
    if reply["degraded"].as_bool().unwrap_or(true) {
        bail!(
            "tabpfn not available: {}",
            reply["error"].as_str().unwrap_or("sidecar degraded")
        );
    }

    let mut out = Vec::with_capacity(test.len());
    for chunk in test.chunks(512) {
        let rows: Vec<[f64; features::N_FEATURES]> = chunk.to_vec();
        let reply = call(
            &mut conn,
            &socket,
            &json!({"type":"predict_batch","rows":rows}),
            Duration::from_secs(120),
        )?;
        if reply["degraded"].as_bool().unwrap_or(true) {
            bail!(
                "tabpfn prediction failed: {}",
                reply["error"].as_str().unwrap_or("sidecar degraded")
            );
        }
        let arr = reply["p_fails"]
            .as_array()
            .ok_or_else(|| anyhow!("malformed predict_batch reply"))?;
        for value in arr {
            out.push(
                value
                    .as_f64()
                    .ok_or_else(|| anyhow!("sidecar returned a null prediction"))?,
            );
        }
    }
    if out.len() != test.len() {
        bail!(
            "sidecar returned {} predictions for {} rows",
            out.len(),
            test.len()
        );
    }
    Ok(out)
}

/// True when the sidecar socket accepts a connection.
pub fn sidecar_up() -> bool {
    config::socket_alive(&config::sidecar_socket_path())
}

/// Sidecar health probe (FR-93). `Err` means the process is unreachable or
/// speaking the wrong protocol; `degraded` means it is up but cannot predict.
#[derive(Clone, Debug, Default)]
pub struct Health {
    pub degraded: bool,
    pub model: Option<String>,
    pub version: Option<String>,
    pub rows: i64,
    pub error: Option<String>,
}

pub fn health() -> Result<Health> {
    let mut conn = None;
    let reply = call(
        &mut conn,
        &config::sidecar_socket_path(),
        &json!({"type": "health"}),
        Duration::from_millis(500),
    )?;
    if reply["type"].as_str() != Some("ok") {
        bail!("unexpected sidecar health reply");
    }
    Ok(Health {
        degraded: reply["degraded"].as_bool().unwrap_or(true),
        model: reply["model"].as_str().map(str::to_string),
        version: reply["version"].as_str().map(str::to_string),
        rows: reply["rows"].as_i64().unwrap_or(0),
        error: reply["error"].as_str().map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_uses_beta_smoothing() {
        let none = Counts { n: 0, fails: 0 };
        assert!((baseline_p(none, none) - 0.5).abs() < 1e-12);
        assert!((baseline_p(Counts { n: 1, fails: 1 }, none) - 2.0 / 3.0).abs() < 1e-12);
        assert!((baseline_p(Counts { n: 1, fails: 0 }, none) - 1.0 / 3.0).abs() < 1e-12);
        assert!((baseline_p(Counts { n: 8, fails: 2 }, none) - 3.0 / 10.0).abs() < 1e-12);
    }

    #[test]
    fn baseline_falls_back_to_global_for_unseen_family() {
        let global = Counts { n: 10, fails: 5 };
        assert!((baseline_p(Counts::default(), global) - 6.0 / 12.0).abs() < 1e-12);
    }

    #[test]
    fn cache_key_uses_three_hour_buckets() {
        assert_eq!(Key::new("cargo", "c", 0).hour_bucket, 0);
        assert_eq!(Key::new("cargo", "c", 2).hour_bucket, 0);
        assert_eq!(Key::new("cargo", "c", 3).hour_bucket, 1);
        assert_eq!(Key::new("cargo", "c", 23).hour_bucket, 7);
    }

    #[test]
    fn baseline_mode_disables_worker() {
        let cfg = Config::default();
        let client = Client::new(&cfg);
        assert!(client.enabled());
        assert!(client.cached(&Key::new("cargo", "c", 12)).is_none());
        assert!(client
            .request(Key::new("cargo", "c", 12), vec![0.0])
            .is_none());
    }
}
