//! The daemon: unix socket listener, request handling, event insertion,
//! prediction/narration wiring.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::time::timeout;

use crate::config::{self, Config};
use crate::proto::{self, Reply, Request};
use crate::rate::RateLimiter;
use crate::{canned, echo, family, features, level, narrator, predictor, redact, store};

struct Pending {
    family: String,
    cmd_redacted: String,
    cwd_id: String,
    p_fail: f64,
    predictor: &'static str,
    level: u8,
    arg_count: i64,
    has_pipe: bool,
    has_redirect: bool,
    git_dirty: Option<i64>,
}

struct State {
    conn: Connection,
    cfg: Config,
    pending: Option<Pending>,
    limiter: RateLimiter,
    bank: canned::Bank,
    predictor: predictor::Client,
    ids: features::IdMaps,
    priors: features::PriorTracker,
    labeled_total: u64,
    since_refit: u32,
    post_buffer: Arc<Mutex<Option<String>>>,
    echo_buffer: Arc<Mutex<HashMap<(String, String), String>>>,
}

impl State {
    fn new(cfg: Config) -> Result<Self> {
        let conn = store::open(&config::db_path())?;
        if let Err(e) = store::prune(&conn, cfg.general.retention_days) {
            tracing::warn!(error = %e, "retention prune failed");
        }
        let ids = features::IdMaps::from_conn(&conn)?;
        let priors = features::PriorTracker::from_conn(&conn, &cfg.predictor.fail_exclude_codes)?;
        let labeled_total = features::count_labeled(&conn, &cfg.predictor.fail_exclude_codes)?;
        let predictor = predictor::Client::new(&cfg);
        Ok(Self {
            conn,
            cfg,
            pending: None,
            limiter: RateLimiter::default(),
            bank: canned::Bank::load(),
            predictor,
            ids,
            priors,
            labeled_total,
            since_refit: 0,
            post_buffer: Arc::new(Mutex::new(None)),
            echo_buffer: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Warm the sidecar with the recent history (FR-22: refit on startup).
    fn startup(&self) {
        if !self.predictor.enabled() {
            return;
        }
        if predictor::ensure_sidecar().is_none() {
            tracing::warn!("sidecar directory not found; predictions fall back to baseline");
            return;
        }
        self.request_refit();
    }

    fn request_refit(&self) {
        self.predictor.refit(
            config::db_path(),
            self.cfg.predictor.train_window,
            self.cfg.predictor.fail_exclude_codes.clone(),
        );
    }

    fn handle(&mut self, line: &str) -> String {
        let req: Request = match serde_json::from_str(line.trim()) {
            Ok(req) => req,
            Err(_) => return proto::encode(&Reply::error("bad_request")),
        };
        if req.v != 1 {
            return proto::encode(&Reply::error("version"));
        }
        match req.kind.as_str() {
            "pre" => proto::encode(&self.pre(&req)),
            "post" => proto::encode(&self.post(&req)),
            _ => proto::encode(&Reply::error("type")),
        }
    }

    fn pre(&mut self, req: &Request) -> Reply {
        let Some(cmd) = req.cmd.as_deref() else {
            return Reply::error("cmd");
        };
        let cwd = req.cwd.as_deref().unwrap_or("");
        let ts = req.ts.unwrap_or_else(level::now_ts);
        self.pending = None;

        if config::is_off() {
            return Reply::pre(None, 0, 0.0);
        }
        if redact::secret_guard_with(cmd, &self.cfg.general.secret_patterns) {
            return Reply::pre(None, 0, 0.0);
        }

        let (family, args) = family::split_family(cmd);
        if family.is_empty() {
            return Reply::pre(None, 0, 0.0);
        }
        let redacted = redact::redact(cmd);
        let cwd_id = store::cwd_id(cwd);
        let _ = store::upsert_dir(&self.conn, &cwd_id, cwd, ts);

        let last = store::last_event(&self.conn).ok().flatten();
        let streak = match &last {
            Some(event) if event.exit_code != 0 => event.fail_streak.max(0),
            _ => 0,
        };
        let secs_since_prev = last.as_ref().map(|event| (ts - event.ts).max(0));
        let (hour, dow) = level::local_hour_dow(ts);
        let arg_count = args.len() as i64;
        let has_pipe = args.iter().any(|t| t.contains('|'));
        let has_redirect = args.iter().any(|t| t.contains('>') || t.contains('<'));

        let input = features::RowInput {
            family: family.clone(),
            cwd_id: cwd_id.clone(),
            hour: hour as i64,
            dow: dow as i64,
            prev_exit: last.as_ref().map(|event| event.exit_code),
            fail_streak: streak,
            secs_since_prev,
            git_dirty: req.git_dirty.map(|d| d as i64),
            arg_count,
            has_pipe,
            has_redirect,
        };
        self.priors.begin(ts);
        let vector = features::vector(&input, &self.ids, &self.priors.counts(&family, &cwd_id));
        let baseline_p = vector[features::FAMILY_PRIOR];

        let use_tabpfn = self.predictor.enabled()
            && !self.predictor.degraded()
            && self.labeled_total >= self.cfg.predictor.min_rows as u64;
        let (p_fail, predictor_mode) = if use_tabpfn {
            match self
                .predictor
                .request(predictor::Key::new(&family, &cwd_id, hour), vector.to_vec())
            {
                Some(p) => (p, "tabpfn"),
                None => (baseline_p, "baseline"),
            }
        } else {
            (baseline_p, "baseline")
        };

        let lvl = level::level(p_fail, streak.max(0) as u32, hour, &self.cfg.levels);
        let speaking = !config::is_muted() && lvl > 0;
        let mut whisper: Option<String> = None;
        if speaking && self.limiter.allow(lvl) {
            if let Ok(Some(days)) = echo::should_echo(&self.conn, &family, &cwd_id) {
                // FR-50/51: an echo replaces the generic pre-whisper.
                let key = (family.clone(), cwd_id.clone());
                whisper = self
                    .echo_buffer
                    .lock()
                    .ok()
                    .and_then(|mut map| map.remove(&key));
                if whisper.is_none() {
                    whisper = self.bank.line(0, canned::Kind::Echo);
                    let facts = narrator::Facts {
                        family: family.clone(),
                        p_fail,
                        fail_streak: streak,
                        hour,
                        level: lvl,
                        days_since_success: Some(days),
                        kind: narrator::Kind::Echo,
                        persona: self.cfg.general.persona.clone(),
                    };
                    self.spawn_echo_narration(key, facts);
                }
            } else {
                whisper = self.bank.line(lvl, canned::Kind::Pre);
            }
        }

        self.pending = Some(Pending {
            family,
            cmd_redacted: redacted,
            cwd_id,
            p_fail,
            predictor: predictor_mode,
            level: lvl,
            arg_count,
            has_pipe,
            has_redirect,
            git_dirty: req.git_dirty.map(|d| d as i64),
        });
        Reply::pre(whisper, lvl, p_fail)
    }

    fn post(&mut self, req: &Request) -> Reply {
        let Some(pending) = self.pending.take() else {
            return Reply::post(None, 0);
        };
        if config::is_off() {
            return Reply::post(None, 0);
        }

        let ts = req.ts.unwrap_or_else(level::now_ts);
        let exit = req.exit_code.unwrap_or(0) as i64;
        let prev = store::last_event(&self.conn).ok().flatten();
        let excluded = self.cfg.predictor.fail_exclude_codes.contains(&exit);
        let (fail_streak, secs_since_prev) = match &prev {
            Some(prev) => {
                let streak = if exit == 0 {
                    0
                } else if excluded {
                    prev.fail_streak
                } else {
                    prev.fail_streak + 1
                };
                (streak, Some((ts - prev.ts).max(0)))
            }
            None => (if exit == 0 { 0 } else { 1 }, None),
        };
        let (hour, dow) = level::local_hour_dow(ts);
        let row = store::EventRow {
            ts,
            cwd_id: pending.cwd_id.clone(),
            family: pending.family.clone(),
            cmd_redacted: pending.cmd_redacted.clone(),
            exit_code: exit,
            duration_ms: req.duration_ms.unwrap_or(0),
            hour: hour as i64,
            dow: dow as i64,
            prev_exit: prev.as_ref().map(|event| event.exit_code),
            fail_streak,
            secs_since_prev,
            git_dirty: pending.git_dirty,
            arg_count: pending.arg_count,
            has_pipe: pending.has_pipe,
            has_redirect: pending.has_redirect,
            p_fail_pred: Some(pending.p_fail),
            predictor: Some(pending.predictor.into()),
        };
        if let Err(e) = store::insert_event(&self.conn, &row) {
            tracing::warn!(error = %e, "event insert failed");
        } else if !excluded {
            self.priors
                .observe(ts, &pending.family, &pending.cwd_id, exit != 0);
            self.labeled_total += 1;
            self.since_refit += 1;
            if self.since_refit >= self.cfg.predictor.refit_every {
                self.since_refit = 0;
                self.request_refit();
            }
        }

        let lvl = pending.level;
        let mut whisper = None;
        if !config::is_muted() && lvl > 0 && self.limiter.allow(lvl) {
            whisper = self
                .post_buffer
                .lock()
                .ok()
                .and_then(|mut slot| slot.take());
            if whisper.is_none() {
                whisper = self.bank.line(lvl, canned::Kind::Post);
            }
        } else if lvl == 0 {
            // Drop any line generated for a calmer moment; it would be stale.
            let _ = self.post_buffer.lock().map(|mut slot| slot.take());
        }

        if !config::is_muted() && lvl > 0 {
            // FR-34: pre-generate the line for the next prompt asynchronously.
            let days = echo::days_since_success(&self.conn, &pending.family, &pending.cwd_id)
                .unwrap_or(None);
            self.spawn_post_narration(narrator::Facts {
                family: pending.family,
                p_fail: pending.p_fail,
                fail_streak,
                hour,
                level: lvl,
                days_since_success: days,
                kind: narrator::Kind::Post,
                persona: self.cfg.general.persona.clone(),
            });
        }
        Reply::post(whisper, lvl)
    }

    fn spawn_post_narration(&self, facts: narrator::Facts) {
        let cfg = self.cfg.narrator.clone();
        let buffer = self.post_buffer.clone();
        std::thread::spawn(move || {
            if let Some(line) = narrator::generate(&facts, &cfg) {
                if let Ok(mut slot) = buffer.lock() {
                    *slot = Some(line);
                }
            }
        });
    }

    fn spawn_echo_narration(&self, key: (String, String), facts: narrator::Facts) {
        let cfg = self.cfg.narrator.clone();
        let buffer = self.echo_buffer.clone();
        std::thread::spawn(move || {
            if let Some(line) = narrator::generate(&facts, &cfg) {
                if let Ok(mut map) = buffer.lock() {
                    if map.len() >= 64 {
                        map.clear();
                    }
                    map.insert(key, line);
                }
            }
        });
    }
}

struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_file(config::data_dir().join("poltergeistd.pid"));
    }
}

pub fn run() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let cfg = Config::load_or_default();
    config::ensure_socket_dir()?;
    let socket = config::socket_path();

    if socket.exists() {
        if config::socket_alive(&socket) {
            bail!("daemon already running at {}", socket.display());
        }
        let _ = std::fs::remove_file(&socket);
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;

    runtime.block_on(async move {
        let listener =
            UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        std::fs::create_dir_all(config::data_dir())?;
        std::fs::write(
            config::data_dir().join("poltergeistd.pid"),
            std::process::id().to_string(),
        )?;
        let _guard = SocketGuard(socket.clone());
        let mut state = State::new(cfg)?;
        state.startup();
        tracing::info!(socket = %socket.display(), "poltergeistd listening");

        loop {
            let (stream, _) = listener.accept().await?;
            handle_conn(stream, &mut state).await;
        }
    })
}

async fn handle_conn(stream: UnixStream, state: &mut State) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    match timeout(Duration::from_millis(500), reader.read_line(&mut line)).await {
        Ok(Ok(n)) if n > 0 => {}
        _ => return,
    }
    let reply = state.handle(&line);
    let mut stream = reader.into_inner();
    let _ = stream.write_all(reply.as_bytes()).await;
    let _ = stream.flush().await;
}
