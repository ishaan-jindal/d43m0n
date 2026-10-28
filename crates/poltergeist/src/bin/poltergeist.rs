//! `poltergeist` CLI and the tiny hook client.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use poltergeist::{config, daemon, eval, level, predictor, proto, redact, store};

const HOOK_BUDGET: Duration = Duration::from_millis(40);
const ZSH_SNIPPET: &str = include_str!("../../../../shell/zsh.zsh");
const BASH_SNIPPET: &str = include_str!("../../../../shell/bash.bash");
const FISH_SNIPPET: &str = include_str!("../../../../shell/fish.fish");

#[derive(Parser)]
#[command(
    name = "poltergeist",
    version,
    about = "A local-only terminal haunting"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print the shell snippet (`poltergeist init fish | source` for fish).
    Init { shell: String },
    /// Manage the daemon.
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
    /// Internal hook entry point.
    #[command(hide = true)]
    Hook {
        #[command(subcommand)]
        cmd: HookCmd,
    },
    /// Show event count, failure rate and top families.
    Stats {
        /// Read the demo database instead of the real one.
        #[arg(long)]
        demo: bool,
    },
    /// Diagnose local setup.
    Doctor,
    /// List dormant directories.
    Seance {
        #[arg(long, default_value_t = 90)]
        days: u32,
        #[arg(long)]
        narrate: bool,
        /// Read the demo database instead of the real one.
        #[arg(long)]
        demo: bool,
    },
    /// Evaluate predictors, optionally with the latency benchmark.
    Eval {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        latency: bool,
        /// Evaluate the demo database instead of the real one.
        #[arg(long)]
        demo: bool,
    },
    /// Seed the demo database or replay a session.
    Demo {
        #[arg(long)]
        seed: bool,
        #[arg(long)]
        run: bool,
        #[arg(long, default_value_t = 1500)]
        count: usize,
    },
    /// Silence whispers without stopping logging.
    Mute,
    /// Resume whispers.
    Unmute,
    /// Stop logging and whispers.
    Off,
    /// Resume logging and whispers.
    On,
    /// Delete stored data.
    Purge {
        #[arg(long)]
        dirs: bool,
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand)]
enum DaemonCmd {
    Start {
        #[arg(long)]
        foreground: bool,
    },
    Stop,
    Status,
}

#[derive(Subcommand)]
enum HookCmd {
    Pre {
        #[arg(long, allow_hyphen_values = true)]
        cmd: String,
        #[arg(long)]
        cwd: String,
        #[arg(long)]
        ts: Option<i64>,
        #[arg(long)]
        git_dirty: Option<bool>,
    },
    Post {
        #[arg(long = "exit")]
        exit: i32,
        #[arg(long, default_value_t = 0)]
        duration_ms: i64,
        #[arg(long)]
        ts: Option<i64>,
    },
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let is_hook = args.get(1).map(|a| a == "hook").unwrap_or(false);

    let code = match Cli::try_parse_from(&args) {
        Ok(cli) => match run(cli) {
            Ok(()) => 0,
            Err(e) => {
                if !is_hook {
                    eprintln!("poltergeist: {e:#}");
                }
                1
            }
        },
        Err(e) => {
            if !is_hook {
                let _ = e.print();
            }
            2
        }
    };
    std::process::exit(if is_hook { 0 } else { code });
}

fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Init { shell } => init(&shell),
        Cmd::Daemon { cmd } => match cmd {
            DaemonCmd::Start { foreground } => daemon_start(foreground),
            DaemonCmd::Stop => daemon_stop(),
            DaemonCmd::Status => daemon_status(),
        },
        Cmd::Hook { cmd } => {
            match cmd {
                HookCmd::Pre {
                    cmd,
                    cwd,
                    ts,
                    git_dirty,
                } => hook_pre(&cmd, &cwd, ts, git_dirty),
                HookCmd::Post {
                    exit,
                    duration_ms,
                    ts,
                } => hook_post(exit, duration_ms, ts),
            }
            Ok(())
        }
        Cmd::Stats { demo } => stats(demo),
        Cmd::Doctor => doctor(),
        Cmd::Seance {
            days,
            narrate,
            demo,
        } => {
            let db = if demo {
                config::demo_db_path()
            } else {
                config::db_path()
            };
            poltergeist::seance::run(&db, days, narrate)
        }
        Cmd::Eval {
            json,
            latency,
            demo,
        } => {
            let db = if demo {
                config::demo_db_path()
            } else {
                config::db_path()
            };
            eval::run(&db, json, latency)
        }
        Cmd::Demo { seed, run, count } => demo(seed, run, count),
        Cmd::Mute => marker("muted", true),
        Cmd::Unmute => marker("muted", false),
        Cmd::Off => marker("off", true),
        Cmd::On => {
            config::set_marker("off", false)?;
            config::set_marker("muted", false)
        }
        Cmd::Purge { dirs, all } => purge(dirs, all),
    }
}

fn marker(name: &str, on: bool) -> Result<()> {
    config::set_marker(name, on)?;
    println!("{}: {}", name, if on { "enabled" } else { "disabled" });
    Ok(())
}

/* ----------------------------- hook client ----------------------------- */

fn hook_pre(cmd: &str, cwd: &str, ts: Option<i64>, git_dirty: Option<bool>) {
    if cmd.starts_with(' ') || cmd.trim().is_empty() {
        return;
    }
    let cfg = config::Config::load_or_default();
    if !cfg.general.ignore.is_empty() {
        let trimmed = cmd.trim();
        for pattern in &cfg.general.ignore {
            if let Ok(re) = regex::Regex::new(pattern) {
                if re.is_match(trimmed) {
                    return;
                }
            }
        }
    }
    let req = proto::Request {
        v: 1,
        kind: "pre".into(),
        cmd: Some(cmd.to_string()),
        cwd: Some(cwd.to_string()),
        ts: Some(ts.unwrap_or_else(level::now_ts)),
        git_dirty,
        ..Default::default()
    };
    send(&req, &cfg, "pre_reply");
}

fn hook_post(exit: i32, duration_ms: i64, ts: Option<i64>) {
    let cfg = config::Config::load_or_default();
    let req = proto::Request {
        v: 1,
        kind: "post".into(),
        exit_code: Some(exit),
        duration_ms: Some(duration_ms),
        ts: Some(ts.unwrap_or_else(level::now_ts)),
        ..Default::default()
    };
    send(&req, &cfg, "post_reply");
}

/// One request, one reply, hard 40 ms budget. Every failure is silent.
fn send(req: &proto::Request, cfg: &config::Config, expected: &str) {
    let deadline = Instant::now() + HOOK_BUDGET;
    let Ok(mut stream) = UnixStream::connect(config::socket_path()) else {
        return;
    };
    let _ = stream.set_write_timeout(Some(remaining(deadline)));
    let Ok(mut line) = serde_json::to_string(req) else {
        return;
    };
    line.push('\n');
    if stream.write_all(line.as_bytes()).is_err() {
        return;
    }
    let _ = stream.set_read_timeout(Some(remaining(deadline)));
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    if reader.read_line(&mut response).is_err() {
        return;
    }
    let Ok(reply) = serde_json::from_str::<proto::Reply>(&response) else {
        return;
    };
    if reply.v != 1 || reply.kind != expected {
        return;
    }
    if let Some(whisper) = reply.whisper {
        // The shell snippet owns presentation (glyph, dim, stderr); the hook
        // client only prints the validated raw whisper on stdout.
        if let Some(text) = redact::validate_whisper(&whisper, cfg.narrator.max_words) {
            println!("{text}");
        }
    }
}

fn remaining(deadline: Instant) -> Duration {
    deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(1))
}

/* ------------------------------- daemon -------------------------------- */

fn daemon_start(foreground: bool) -> Result<()> {
    if foreground {
        return daemon::run();
    }
    if config::socket_alive(&config::socket_path()) {
        println!(
            "daemon: already running ({})",
            config::socket_path().display()
        );
        return Ok(());
    }
    if spawn_daemon()? {
        println!("daemon: started ({})", config::socket_path().display());
    } else {
        bail!(
            "daemon did not start; see {}",
            config::data_dir().join("daemon.log").display()
        );
    }
    Ok(())
}

fn spawn_daemon() -> Result<bool> {
    if config::socket_alive(&config::socket_path()) {
        return Ok(true);
    }
    let daemon_bin = std::env::current_exe()
        .context("locating current executable")?
        .with_file_name("poltergeistd");
    if !daemon_bin.exists() {
        bail!("{} not found next to the CLI", daemon_bin.display());
    }
    std::fs::create_dir_all(config::data_dir())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(config::data_dir().join("daemon.log"))?;
    let err = log.try_clone()?;
    Command::new(&daemon_bin)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(err)
        .spawn()
        .with_context(|| format!("spawning {}", daemon_bin.display()))?;

    let deadline = Instant::now() + Duration::from_millis(1_000);
    while Instant::now() < deadline {
        if config::socket_alive(&config::socket_path()) {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(false)
}

fn daemon_stop() -> Result<()> {
    let pid_path = config::data_dir().join("poltergeistd.pid");
    let pid = std::fs::read_to_string(&pid_path)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok());

    if let Some(pid) = pid {
        // SAFETY: kill is signal delivery to a pid we wrote ourselves.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_millis(1_000);
        while Instant::now() < deadline && config::socket_alive(&config::socket_path()) {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    let _ = std::fs::remove_file(&pid_path);
    let _ = std::fs::remove_file(config::socket_path());
    println!("daemon: stopped");
    Ok(())
}

fn daemon_status() -> Result<()> {
    let socket = config::socket_path();
    if config::socket_alive(&socket) {
        println!("daemon: running ({})", socket.display());
    } else if socket.exists() {
        println!("daemon: stale socket ({})", socket.display());
    } else {
        println!("daemon: not running");
    }
    Ok(())
}

/* -------------------------------- stats -------------------------------- */

fn stats(demo: bool) -> Result<()> {
    let cfg = config::Config::load_or_default();
    let path = if demo {
        config::demo_db_path()
    } else {
        config::db_path()
    };
    let (total, failures, families, labeled) = if path.exists() {
        let conn = store::open(&path)?;
        let stats = store::stats(&conn)?;
        let labeled = Some(poltergeist::features::count_labeled(
            &conn,
            &cfg.predictor.fail_exclude_codes,
        )?);
        (stats.total, stats.failures, stats.families, labeled)
    } else {
        (0, 0, Vec::new(), Some(0))
    };
    let pct = if total > 0 {
        failures as f64 * 100.0 / total as f64
    } else {
        0.0
    };
    println!("events: {total}");
    println!("failures: {failures} ({pct:.1}%)");
    println!("predictor: {}", predictor_status(&cfg, labeled));
    if !families.is_empty() {
        println!("top families:");
        for (family, count) in families {
            println!("  {family}: {count}");
        }
    }
    Ok(())
}

/// FR-91: only claim TabPFN when the sidecar is healthy *and* `import tabpfn`
/// works. Anything else is baseline, with the reason attached.
fn predictor_status(cfg: &config::Config, labeled: Option<u64>) -> String {
    if cfg.predictor.mode == "baseline" {
        return "baseline (configured)".to_string();
    }
    if !predictor::sidecar_up() {
        return "baseline (degraded: sidecar not running)".to_string();
    }
    match predictor::health() {
        Err(_) => "baseline (degraded: sidecar not responding)".to_string(),
        Ok(h) => {
            if let Some(error) = h.error {
                return format!("baseline (degraded: {error})");
            }
            if h.degraded {
                return "baseline (degraded: model not fitted yet)".to_string();
            }
            if !command_ok("python3", &["-c", "import tabpfn"], Duration::from_secs(3)) {
                return "baseline (degraded: tabpfn not importable)".to_string();
            }
            if let Some(n) = labeled {
                if n < cfg.predictor.min_rows as u64 {
                    return format!(
                        "baseline (degraded: {n} labeled events, need {})",
                        cfg.predictor.min_rows
                    );
                }
            }
            "tabpfn (sidecar healthy)".to_string()
        }
    }
}

/* -------------------------------- demo --------------------------------- */

fn demo(seed: bool, run: bool, count: usize) -> Result<()> {
    if seed {
        let path = config::demo_db_path();
        for suffix in ["", "-wal", "-shm"] {
            let mut p = path.as_os_str().to_owned();
            p.push(suffix);
            let _ = std::fs::remove_file(std::path::PathBuf::from(p));
        }
        let conn = store::open(&path)?;
        // Prefer the bundled realistic history (FR-70); fall back to synthetic.
        let from_csv = match store::demo_csv_path() {
            Some(csv) => match std::fs::read_to_string(&csv)
                .map_err(anyhow::Error::from)
                .and_then(|text| store::seed_demo_csv(&conn, &text))
            {
                Ok(n) if n > 0 => Some(n),
                Ok(_) => None,
                Err(e) => {
                    eprintln!(
                        "demo: {} unusable ({e:#}); using synthetic seed",
                        csv.display()
                    );
                    None
                }
            },
            None => None,
        };
        let seeded = match from_csv {
            Some(n) => n,
            None => store::seed_demo(&conn, count)?,
        };
        println!("demo: seeded {seeded} events into {}", path.display());
        return Ok(());
    }
    if run {
        return poltergeist::demo::run(&config::demo_db_path());
    }
    bail!("choose --seed or --run")
}

/* -------------------------------- purge -------------------------------- */

fn purge(dirs: bool, all: bool) -> Result<()> {
    if all {
        let path = config::db_path();
        for suffix in ["", "-wal", "-shm"] {
            let mut p = path.as_os_str().to_owned();
            p.push(suffix);
            let _ = std::fs::remove_file(std::path::PathBuf::from(p));
        }
        println!("purge: removed {}", path.display());
        return Ok(());
    }
    if dirs {
        let path = config::db_path();
        if path.exists() {
            let conn = store::open(&path)?;
            store::purge(&conn)?;
        }
        println!("purge: cleared events and dirs");
        return Ok(());
    }
    bail!("choose --dirs or --all")
}

/* --------------------------------- init -------------------------------- */

fn init(shell: &str) -> Result<()> {
    let _ = spawn_daemon();
    match shell {
        // Byte-identical to shell/*.zsh|bash|fish so `init` output and the checked-in
        // snippets cannot drift apart.
        "zsh" => print!("{ZSH_SNIPPET}"),
        "bash" => print!("{BASH_SNIPPET}"),
        "fish" => print!("{FISH_SNIPPET}"),
        other => bail!("unsupported shell '{other}' (use zsh, bash or fish)"),
    }
    Ok(())
}

/* -------------------------------- doctor ------------------------------- */

fn doctor() -> Result<()> {
    // FR-93: diagnose, report fixes, never a stack trace. Always exit 0.
    if let Err(e) = doctor_inner() {
        println!("[!!] doctor: {e:#}");
    }
    Ok(())
}

fn doctor_inner() -> Result<()> {
    let cfg = config::Config::load_or_default();
    let socket = config::socket_path();

    let running = config::socket_alive(&socket);
    let daemon_detail = if running {
        format!("running ({})", socket.display())
    } else if socket.exists() {
        format!("stale socket ({})", socket.display())
    } else {
        "not running".to_string()
    };
    row(
        Some(running),
        "daemon",
        &daemon_detail,
        "poltergeist daemon start",
    );

    let sidecar = config::sidecar_socket_path();
    if !sidecar.exists() {
        row(
            Some(false),
            "sidecar",
            "not running",
            "poltergeist daemon start (spawns the sidecar)",
        );
    } else {
        match predictor::health() {
            Ok(h) => {
                if let Some(error) = &h.error {
                    row(
                        Some(false),
                        "sidecar",
                        &format!("up, predictor degraded: {error}"),
                        "pip install tabpfn (see README for weights/license)",
                    );
                } else if h.degraded {
                    row(
                        None,
                        "sidecar",
                        "up, model not fitted yet",
                        "send one command, then re-run doctor",
                    );
                } else {
                    let model = h.model.as_deref().unwrap_or("tabpfn");
                    let version = h.version.as_deref().unwrap_or("unknown");
                    row(
                        Some(true),
                        "sidecar",
                        &format!("healthy ({model} {version}, {} rows)", h.rows),
                        "",
                    );
                }
            }
            Err(_) => row(
                Some(false),
                "sidecar",
                "socket present but health probe failed",
                "poltergeist daemon stop && poltergeist daemon start",
            ),
        }
    }

    let tabpfn = command_ok("python3", &["-c", "import tabpfn"], Duration::from_secs(3));
    row(
        Some(tabpfn),
        "tabpfn import",
        if tabpfn {
            "importable"
        } else {
            "not importable"
        },
        "pip install tabpfn (see README for weights/license)",
    );

    match ollama_models(&cfg.narrator.endpoint) {
        Ok(models) => {
            row(
                Some(true),
                "ollama",
                &format!("reachable ({})", cfg.narrator.endpoint),
                "",
            );
            let model = &cfg.narrator.model;
            let present = models.iter().any(|m| {
                m == model
                    || m.starts_with(&format!("{model}:"))
                    || m.split(':').next() == Some(model.as_str())
            });
            row(
                Some(present),
                "model",
                if present {
                    model
                } else {
                    "configured model not installed"
                },
                &format!("ollama pull {model}"),
            );
        }
        Err(e) => {
            row(
                Some(false),
                "ollama",
                &format!("unreachable ({e})"),
                "ollama serve",
            );
            row(None, "model", "skipped, Ollama unreachable", "");
        }
    }

    if socket.exists() {
        let mode = std::fs::metadata(&socket)
            .map(|m| {
                use std::os::unix::fs::MetadataExt;
                m.mode() & 0o777
            })
            .unwrap_or(0);
        row(
            Some(mode == 0o600),
            "socket permissions",
            &format!("{mode:04o}"),
            "restart the daemon (socket must be 0600)",
        );
    } else {
        row(None, "socket permissions", "skipped, daemon down", "");
    }

    let installed = [".zshrc", ".bashrc"].iter().any(|rc| {
        std::fs::read_to_string(config::home().join(rc))
            .map(|s| s.contains("poltergeist init"))
            .unwrap_or(false)
    }) || std::fs::read_to_string(config::home().join(".config/fish/config.fish"))
        .map(|s| s.contains("poltergeist init") || s.contains("init fish"))
        .unwrap_or(false);
    row(
        Some(installed),
        "hook installed",
        if installed {
            "found in shell rc"
        } else {
            "not found in ~/.zshrc, ~/.bashrc or fish config"
        },
        "echo 'eval \"$(poltergeist init zsh)\"' >> ~/.zshrc",
    );

    Ok(())
}

fn row(ok: Option<bool>, name: &str, detail: &str, fix: &str) {
    let tag = match ok {
        Some(true) => "ok",
        Some(false) => "!!",
        None => "--",
    };
    println!("[{tag}] {name}: {detail}");
    if ok == Some(false) && !fix.is_empty() {
        println!("     fix: {fix}");
    }
}

fn command_ok(program: &str, args: &[&str], limit: Duration) -> bool {
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {}
            Err(_) => return false,
        }
        if start.elapsed() >= limit {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn ollama_models(endpoint: &str) -> Result<Vec<String>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .context("building http client")?;
    let url = format!("{}/api/tags", endpoint.trim_end_matches('/'));
    let response = client.get(url).send().context("connecting")?;
    let status = response.status();
    if !status.is_success() {
        bail!("HTTP {status}");
    }
    let value: serde_json::Value = response.json().context("decoding /api/tags")?;
    let models = value
        .get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(models)
}
