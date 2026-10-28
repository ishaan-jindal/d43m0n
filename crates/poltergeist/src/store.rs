//! SQLite store: schema (SRS 7), inserts, stats, demo seeding.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS dirs (
  cwd_id   TEXT PRIMARY KEY,
  path     TEXT NOT NULL,
  first_ts INTEGER NOT NULL,
  last_ts  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS events (
  id            INTEGER PRIMARY KEY,
  ts            INTEGER NOT NULL,
  cwd_id        TEXT NOT NULL REFERENCES dirs(cwd_id),
  family        TEXT NOT NULL,
  cmd_redacted  TEXT NOT NULL,
  exit_code     INTEGER NOT NULL,
  duration_ms   INTEGER NOT NULL,
  hour          INTEGER NOT NULL,
  dow           INTEGER NOT NULL,
  prev_exit     INTEGER,
  fail_streak   INTEGER NOT NULL,
  secs_since_prev INTEGER,
  git_dirty     INTEGER,
  arg_count     INTEGER NOT NULL,
  has_pipe      INTEGER NOT NULL,
  has_redirect  INTEGER NOT NULL,
  p_fail_pred   REAL,
  predictor     TEXT
);

CREATE INDEX IF NOT EXISTS idx_events_ts ON events(ts);
CREATE INDEX IF NOT EXISTS idx_events_family_cwd ON events(family, cwd_id, ts);
"#;

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;",
    )?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

/// `cwd_id` is the hex SHA-256 of the absolute path.
pub fn cwd_id(path: &str) -> String {
    let digest = Sha256::digest(path.as_bytes());
    hex::encode(digest)
}

pub fn upsert_dir(conn: &Connection, cwd_id: &str, path: &str, ts: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO dirs (cwd_id, path, first_ts, last_ts) VALUES (?1, ?2, ?3, ?3)
         ON CONFLICT(cwd_id) DO UPDATE SET last_ts = excluded.last_ts",
        params![cwd_id, path, ts],
    )?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct EventRow {
    pub ts: i64,
    pub cwd_id: String,
    pub family: String,
    pub cmd_redacted: String,
    pub exit_code: i64,
    pub duration_ms: i64,
    pub hour: i64,
    pub dow: i64,
    pub prev_exit: Option<i64>,
    pub fail_streak: i64,
    pub secs_since_prev: Option<i64>,
    pub git_dirty: Option<i64>,
    pub arg_count: i64,
    pub has_pipe: bool,
    pub has_redirect: bool,
    pub p_fail_pred: Option<f64>,
    pub predictor: Option<String>,
}

pub fn insert_event(conn: &Connection, e: &EventRow) -> Result<i64> {
    conn.execute(
        "INSERT INTO events (ts, cwd_id, family, cmd_redacted, exit_code, duration_ms, hour, dow,
                             prev_exit, fail_streak, secs_since_prev, git_dirty, arg_count,
                             has_pipe, has_redirect, p_fail_pred, predictor)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        params![
            e.ts,
            e.cwd_id,
            e.family,
            e.cmd_redacted,
            e.exit_code,
            e.duration_ms,
            e.hour,
            e.dow,
            e.prev_exit,
            e.fail_streak,
            e.secs_since_prev,
            e.git_dirty,
            e.arg_count,
            e.has_pipe as i64,
            e.has_redirect as i64,
            e.p_fail_pred,
            e.predictor,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

#[derive(Debug, Clone)]
pub struct LastEvent {
    pub exit_code: i64,
    pub ts: i64,
    pub fail_streak: i64,
}

pub fn last_event(conn: &Connection) -> Result<Option<LastEvent>> {
    let mut stmt =
        conn.prepare("SELECT exit_code, ts, fail_streak FROM events ORDER BY id DESC LIMIT 1")?;
    let mut rows = stmt.query([])?;
    Ok(rows.next()?.map(|row| LastEvent {
        exit_code: row.get(0).unwrap_or(0),
        ts: row.get(1).unwrap_or(0),
        fail_streak: row.get(2).unwrap_or(0),
    }))
}

/// Total and failed event counts for a family (exit != 0, M1 approximation).
pub fn family_failure_rate(conn: &Connection, family: &str) -> Result<(i64, i64)> {
    conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(exit_code <> 0), 0) FROM events WHERE family = ?1",
        [family],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .map_err(Into::into)
}

#[derive(Debug, Default)]
pub struct Stats {
    pub total: i64,
    pub failures: i64,
    pub families: Vec<(String, i64)>,
}

pub fn stats(conn: &Connection) -> Result<Stats> {
    let (total, failures) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(exit_code <> 0), 0) FROM events",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let mut stmt = conn.prepare(
        "SELECT family, COUNT(*) AS n FROM events GROUP BY family ORDER BY n DESC, family LIMIT 5",
    )?;
    let families = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Stats {
        total,
        failures,
        families,
    })
}

pub fn prune(conn: &Connection, retention_days: u32) -> Result<usize> {
    let cutoff = crate::level::now_ts() - retention_days as i64 * 86_400;
    Ok(conn.execute("DELETE FROM events WHERE ts < ?1", [cutoff])?)
}

pub fn purge(conn: &Connection) -> Result<()> {
    conn.execute_batch("DELETE FROM events; DELETE FROM dirs;")?;
    Ok(())
}

/// Insert `count` synthetic events for `poltergeist demo --seed` (FR-70).
pub fn seed_demo(conn: &Connection, count: usize) -> Result<usize> {
    // ponytail: toy LCG instead of a rand dependency; determinism is all M1 needs.
    let mut rng: u64 = 0x5EED_1234_ABCD_0001;
    let mut next = move || {
        rng = rng
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (rng >> 33) as u32
    };

    let families: [(&str, &str, f64); 8] = [
        ("cargo", "cargo build <arg>", 0.25),
        ("git", "git status", 0.10),
        ("ls", "ls <arg>", 0.02),
        ("cd", "cd <path>", 0.03),
        ("docker", "docker compose <arg>", 0.30),
        ("npm", "npm run <arg>", 0.20),
        ("pytest", "pytest <arg>", 0.35),
        ("make", "make <arg>", 0.18),
    ];
    let dirs = [
        "/home/u/proj",
        "/home/u/proj/crates",
        "/home/u/work/api",
        "/home/u/work/web",
        "/home/u/scratch",
        "/home/u/dotfiles",
    ];

    let now = crate::level::now_ts();
    let start = now - 180 * 86_400;
    for dir in dirs {
        upsert_dir(conn, &cwd_id(dir), dir, start)?;
    }

    let mut ts = start;
    let mut prev_exit: Option<i64> = None;
    let mut streak: i64 = 0;
    for _ in 0..count {
        ts += 60 + (next() % 7_200) as i64;
        let (family, template, fail_rate) = families[(next() as usize) % families.len()];
        let dir = dirs[(next() as usize) % dirs.len()];
        let failed = (next() as f64 / u32::MAX as f64) < fail_rate;
        let exit_code = if failed { 1 + (next() % 2) as i64 } else { 0 };
        streak = if exit_code == 0 { 0 } else { streak + 1 };
        let (hour, dow) = crate::level::local_hour_dow(ts);
        let row = EventRow {
            ts,
            cwd_id: cwd_id(dir),
            family: family.into(),
            cmd_redacted: template.into(),
            exit_code,
            duration_ms: (next() % 5_000) as i64,
            hour: hour as i64,
            dow: dow as i64,
            prev_exit,
            fail_streak: streak,
            secs_since_prev: prev_exit.map(|_| 60 + (next() % 7_000) as i64),
            git_dirty: Some((next() % 2) as i64),
            arg_count: if template.contains('<') { 1 } else { 0 },
            has_pipe: next() % 10 == 0,
            has_redirect: next() % 10 == 0,
            p_fail_pred: Some(fail_rate),
            predictor: Some("baseline".into()),
        };
        upsert_dir(conn, &row.cwd_id, dir, ts)?;
        insert_event(conn, &row)?;
        prev_exit = Some(exit_code);
    }
    Ok(count)
}

/// Directories the bundled demo history was generated from (scripts/gen_demo_history.py).
const DEMO_DIRS: [&str; 7] = [
    "/home/dev/work/storefront",
    "/home/dev/work/storefront/packages/ui",
    "/home/dev/work/api",
    "/home/dev/work/analytics",
    "/home/dev/work/infra",
    "/home/dev/work/notes",
    "/home/dev/scratch",
];

fn demo_path_for(cwd: &str) -> Option<&'static str> {
    DEMO_DIRS
        .iter()
        .copied()
        .find(|path| cwd_id(path).starts_with(cwd))
}

/// Locate `data/demo_history.csv` at runtime (env override, cwd, or next to
/// the executable).
pub fn demo_csv_path() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("POLTERGEIST_DEMO_CSV") {
        let p = std::path::PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let mut roots = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            roots.push(dir.to_path_buf());
        }
    }
    for root in roots {
        for ancestor in root.ancestors().take(6) {
            let candidate = ancestor.join("data").join("demo_history.csv");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Load the bundled CSV (FR-70). Returns the number of events inserted.
pub fn seed_demo_csv(conn: &Connection, text: &str) -> Result<usize> {
    let mut count = 0usize;
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim().trim_end_matches('\r');
        if line.is_empty() || (index == 0 && line.starts_with("ts,")) {
            continue;
        }
        let cols: Vec<&str> = line.split(',').collect();
        if cols.len() < 17 {
            continue;
        }
        let int = |i: usize| cols[i].trim().parse::<i64>().ok();
        let (Some(ts), Some(exit_code), Some(duration_ms)) = (int(0), int(4), int(5)) else {
            continue;
        };
        let cwd = cols[1].trim();
        if cwd.is_empty() || cols[2].trim().is_empty() {
            continue;
        }
        let dir = demo_path_for(cwd).unwrap_or("(demo)");
        upsert_dir(conn, cwd, dir, ts)?;
        let row = EventRow {
            ts,
            cwd_id: cwd.to_string(),
            family: cols[2].trim().to_string(),
            cmd_redacted: cols[3].trim().to_string(),
            exit_code,
            duration_ms,
            hour: int(6).unwrap_or(0),
            dow: int(7).unwrap_or(0),
            prev_exit: int(8),
            fail_streak: int(9).unwrap_or(0),
            secs_since_prev: int(10),
            git_dirty: int(11),
            arg_count: int(12).unwrap_or(0),
            has_pipe: int(13).unwrap_or(0) != 0,
            has_redirect: int(14).unwrap_or(0) != 0,
            p_fail_pred: cols[15].trim().parse::<f64>().ok(),
            predictor: (!cols[16].trim().is_empty()).then(|| cols[16].trim().to_string()),
        };
        insert_event(conn, &row)?;
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "poltergeist-test-{}-{}-{}.db",
            tag,
            std::process::id(),
            nanos
        ))
    }

    fn cleanup(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let mut p = path.as_os_str().to_owned();
            p.push(suffix);
            let _ = std::fs::remove_file(std::path::PathBuf::from(p));
        }
    }

    #[test]
    fn schema_and_round_trip() {
        let path = temp_db("store");
        let conn = open(&path).unwrap();

        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");

        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(events)")
            .unwrap()
            .query_map([], |r| r.get(1))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(cols.len(), 18);
        for name in [
            "id",
            "ts",
            "cwd_id",
            "family",
            "cmd_redacted",
            "exit_code",
            "duration_ms",
            "hour",
            "dow",
            "prev_exit",
            "fail_streak",
            "secs_since_prev",
            "git_dirty",
            "arg_count",
            "has_pipe",
            "has_redirect",
            "p_fail_pred",
            "predictor",
        ] {
            assert!(cols.iter().any(|c| c == name), "missing column {name}");
        }

        let dir = "/home/u/proj";
        let id = cwd_id(dir);
        upsert_dir(&conn, &id, dir, 100).unwrap();
        upsert_dir(&conn, &id, dir, 200).unwrap();
        let row = EventRow {
            ts: 200,
            cwd_id: id.clone(),
            family: "cargo".into(),
            cmd_redacted: "cargo build <arg>".into(),
            exit_code: 101,
            duration_ms: 42,
            hour: 12,
            dow: 3,
            prev_exit: Some(0),
            fail_streak: 1,
            secs_since_prev: Some(60),
            git_dirty: Some(1),
            arg_count: 1,
            has_pipe: false,
            has_redirect: false,
            p_fail_pred: Some(0.4),
            predictor: Some("baseline".into()),
        };
        insert_event(&conn, &row).unwrap();

        let stats = stats(&conn).unwrap();
        assert_eq!(stats.total, 1);
        assert_eq!(stats.failures, 1);
        assert_eq!(stats.families[0], ("cargo".to_string(), 1));

        let last = last_event(&conn).unwrap().unwrap();
        assert_eq!(last.exit_code, 101);
        assert_eq!(last.fail_streak, 1);

        let (n, fails) = family_failure_rate(&conn, "cargo").unwrap();
        assert_eq!((n, fails), (1, 1));

        drop(conn);
        cleanup(&path);
    }

    #[test]
    fn cwd_id_is_stable_and_distinct() {
        assert_eq!(cwd_id("/a/b"), cwd_id("/a/b"));
        assert_ne!(cwd_id("/a/b"), cwd_id("/a/c"));
        assert_eq!(cwd_id("/a/b").len(), 64);
    }

    #[test]
    fn demo_seed_is_deterministic_shape() {
        let path = temp_db("demo");
        let conn = open(&path).unwrap();
        seed_demo(&conn, 150).unwrap();
        let stats = stats(&conn).unwrap();
        assert_eq!(stats.total, 150);
        assert!(stats.failures > 0);
        assert!(!stats.families.is_empty());
        drop(conn);
        cleanup(&path);
    }

    #[test]
    fn demo_csv_seeds_rows_and_dirs() {
        let path = temp_db("democsv");
        let conn = open(&path).unwrap();
        let csv = "ts,cwd_id,family,cmd_redacted,exit_code,duration_ms,hour,dow,prev_exit,fail_streak,secs_since_prev,git_dirty,arg_count,has_pipe,has_redirect,p_fail_pred,predictor\r\n\
                   100,4aa9acb1199c7ec1,cargo,cargo build <arg>,0,10,12,1,,0,,1,1,0,0,0.2,baseline\r\n\
                   200,4aa9acb1199c7ec1,cargo,cargo test,101,20,12,1,0,1,100,0,0,0,0,,\r\n";
        assert_eq!(seed_demo_csv(&conn, csv).unwrap(), 2);
        let stats = stats(&conn).unwrap();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.failures, 1);
        let dir: String = conn
            .query_row("SELECT path FROM dirs LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(dir, "/home/dev/work/storefront");
        drop(conn);
        cleanup(&path);
    }
}
