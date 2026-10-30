use std::env;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// `$XDG_*_HOME` or the conventional fallback under `$HOME`.
fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(fallback))
}

pub fn home() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

pub fn config_path() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config")
        .join("poltergeist")
        .join("config.toml")
}

pub fn data_dir() -> PathBuf {
    xdg_dir("XDG_DATA_HOME", ".local/share").join("poltergeist")
}

pub fn db_path() -> PathBuf {
    data_dir().join("events.db")
}

pub fn demo_db_path() -> PathBuf {
    data_dir().join("demo.db")
}

pub fn socket_path() -> PathBuf {
    match env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir)
            .join("poltergeist")
            .join("poltergeist.sock"),
        None => PathBuf::from(format!("/tmp/poltergeist-{}.sock", uid())),
    }
}

pub fn sidecar_socket_path() -> PathBuf {
    match env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir).join("poltergeist").join("sidecar.sock"),
        None => PathBuf::from(format!("/tmp/poltergeist-{}-sidecar.sock", uid())),
    }
}

/// Create the socket directory and refuse a world-writable, non-sticky one.
/// `/tmp` fallback is sticky, so the per-uid socket name is safe there.
pub fn ensure_socket_dir() -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let sock = socket_path();
    let dir = sock.parent().context("socket path has no parent")?;
    std::fs::create_dir_all(dir)?;
    let meta = std::fs::metadata(dir)?;
    let mode = meta.mode() & 0o777;
    if mode & 0o002 != 0 && mode & 0o1000 == 0 {
        bail!("refusing world-writable socket directory {}", dir.display());
    }
    if dir != Path::new("/tmp") {
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

pub fn socket_alive(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

fn marker(name: &str) -> PathBuf {
    data_dir().join(name)
}

pub fn is_muted() -> bool {
    marker("muted").exists()
}

pub fn is_off() -> bool {
    marker("off").exists()
}

pub fn set_marker(name: &str, on: bool) -> Result<()> {
    let path = marker(name);
    if on {
        std::fs::create_dir_all(data_dir())?;
        std::fs::write(&path, b"1")?;
    } else if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    #[serde(default = "General::default")]
    pub general: General,
    #[serde(default = "Levels::default")]
    pub levels: Levels,
    #[serde(default = "Predictor::default")]
    pub predictor: Predictor,
    #[serde(default = "Narrator::default")]
    pub narrator: Narrator,
    #[serde(default = "Observability::default")]
    pub observability: Observability,
}

impl Config {
    pub fn load() -> Result<Self> {
        Self::load_from(&config_path())
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn load_or_default() -> Self {
        Self::load().unwrap_or_default()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct General {
    #[serde(default = "d_glyph")]
    pub glyph: String,
    #[serde(default = "d_persona")]
    pub persona: String,
    #[serde(default = "d_retention")]
    pub retention_days: u32,
    /// Regexes matched against the trimmed command before it is sent.
    #[serde(default = "d_ignore")]
    pub ignore: Vec<String>,
    /// Extra secret regexes; a match drops the event.
    #[serde(default)]
    pub secret_patterns: Vec<String>,
}

fn d_glyph() -> String {
    "👻".into()
}
fn d_persona() -> String {
    "d43m0n".into()
}
fn d_retention() -> u32 {
    365
}
fn d_ignore() -> Vec<String> {
    vec!["^ls$".into(), "^cd( |$)".into(), "^clear$".into()]
}

impl Default for General {
    fn default() -> Self {
        Self {
            glyph: d_glyph(),
            persona: d_persona(),
            retention_days: d_retention(),
            ignore: d_ignore(),
            secret_patterns: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Levels {
    #[serde(default = "d_l1")]
    pub l1: f64,
    #[serde(default = "d_l2")]
    pub l2: f64,
    #[serde(default = "d_night_start")]
    pub l3_night_start: u32,
    #[serde(default = "d_night_end")]
    pub l3_night_end: u32,
    #[serde(default = "d_l3")]
    pub l3_p: f64,
    #[serde(default = "d_streak_l2")]
    pub streak_l2: u32,
}

fn d_l1() -> f64 {
    0.30
}
fn d_l2() -> f64 {
    0.60
}
fn d_night_start() -> u32 {
    0
}
fn d_night_end() -> u32 {
    5
}
fn d_l3() -> f64 {
    0.85
}
fn d_streak_l2() -> u32 {
    3
}

impl Default for Levels {
    fn default() -> Self {
        Self {
            l1: d_l1(),
            l2: d_l2(),
            l3_night_start: d_night_start(),
            l3_night_end: d_night_end(),
            l3_p: d_l3(),
            streak_l2: d_streak_l2(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Predictor {
    #[serde(default = "d_mode")]
    pub mode: String,
    #[serde(default = "d_train_window")]
    pub train_window: u32,
    #[serde(default = "d_min_rows")]
    pub min_rows: u32,
    #[serde(default = "d_refit")]
    pub refit_every: u32,
    #[serde(default = "d_exclude")]
    pub fail_exclude_codes: Vec<i64>,
    #[serde(default = "d_predict_timeout")]
    pub predict_timeout_ms: u64,
}

fn d_mode() -> String {
    "auto".into()
}
fn d_train_window() -> u32 {
    2000
}
fn d_min_rows() -> u32 {
    200
}
fn d_refit() -> u32 {
    50
}
fn d_exclude() -> Vec<i64> {
    vec![130, 148]
}
fn d_predict_timeout() -> u64 {
    300
}

impl Default for Predictor {
    fn default() -> Self {
        Self {
            mode: d_mode(),
            train_window: d_train_window(),
            min_rows: d_min_rows(),
            refit_every: d_refit(),
            fail_exclude_codes: d_exclude(),
            predict_timeout_ms: d_predict_timeout(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Narrator {
    #[serde(default = "d_backend")]
    pub backend: String,
    #[serde(default = "d_endpoint")]
    pub endpoint: String,
    #[serde(default = "d_model")]
    pub model: String,
    #[serde(default = "d_narrator_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "d_max_words")]
    pub max_words: usize,
}

fn d_backend() -> String {
    "ollama".into()
}
fn d_endpoint() -> String {
    "http://127.0.0.1:11434".into()
}
fn d_model() -> String {
    "gemma3:4b".into()
}
fn d_narrator_timeout() -> u64 {
    800
}
fn d_max_words() -> usize {
    12
}

impl Default for Narrator {
    fn default() -> Self {
        Self {
            backend: d_backend(),
            endpoint: d_endpoint(),
            model: d_model(),
            timeout_ms: d_narrator_timeout(),
            max_words: d_max_words(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Observability {
    #[serde(default)]
    pub sentry: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_srs() {
        let cfg = Config::default();
        assert_eq!(cfg.general.glyph, "👻");
        assert_eq!(cfg.levels.l1, 0.30);
        assert_eq!(cfg.levels.streak_l2, 3);
        assert_eq!(cfg.predictor.fail_exclude_codes, vec![130, 148]);
        assert_eq!(cfg.narrator.max_words, 12);
        assert!(!cfg.observability.sentry);
    }

    #[test]
    fn partial_toml_keeps_defaults() {
        let dir = std::env::temp_dir().join(format!("poltergeist-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "[general]\nglyph = \"*\"\n\n[levels]\nl1 = 0.4\n").unwrap();
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.general.glyph, "*");
        assert_eq!(cfg.general.persona, "d43m0n");
        assert_eq!(cfg.levels.l1, 0.4);
        assert_eq!(cfg.levels.l2, 0.60);
        assert_eq!(cfg.narrator.model, "gemma3:4b");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
