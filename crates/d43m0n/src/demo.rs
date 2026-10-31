//! `d43m0n demo --run` (FR-71): scripted replay of a fixed session
//! over the demo store. No daemon is started and the real event database
//! is never opened. Levels and order are deterministic; whisper text comes
//! from the live narrator (silence on a miss).

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use rusqlite::Connection;

use crate::config::Config;
use crate::rate::RateLimiter;
use crate::{level, narrator, store};

#[derive(Clone, Copy)]
struct Step {
    label: &'static str,
    family: &'static str,
    cwd: &'static str,
    hour: u32,
    exit: i64,
}

/// ~20 commands across families, cwds and hours. The prediction cache values
/// chosen here land in L0, L1, L2 and night-time L3.
const SCRIPT: &[Step] = &[
    Step {
        label: "git status",
        family: "git",
        cwd: "/home/dev/work/notes",
        hour: 13,
        exit: 0,
    },
    Step {
        label: "git log --oneline",
        family: "git",
        cwd: "/home/dev/work/notes",
        hour: 13,
        exit: 0,
    },
    Step {
        label: "npm test",
        family: "npm",
        cwd: "/home/dev/work/storefront/packages/ui",
        hour: 13,
        exit: 1,
    },
    Step {
        label: "npm test",
        family: "npm",
        cwd: "/home/dev/work/storefront/packages/ui",
        hour: 13,
        exit: 1,
    },
    Step {
        label: "npm test",
        family: "npm",
        cwd: "/home/dev/work/storefront/packages/ui",
        hour: 13,
        exit: 1,
    },
    Step {
        label: "npm run build",
        family: "npm",
        cwd: "/home/dev/work/storefront",
        hour: 14,
        exit: 1,
    },
    Step {
        label: "npm run build",
        family: "npm",
        cwd: "/home/dev/work/storefront",
        hour: 14,
        exit: 1,
    },
    Step {
        label: "python3 -m pytest",
        family: "pytest",
        cwd: "/home/dev/work/api",
        hour: 2,
        exit: 1,
    },
    Step {
        label: "git status",
        family: "git",
        cwd: "/home/dev/work/notes",
        hour: 2,
        exit: 0,
    },
    Step {
        label: "kubectl get pods",
        family: "kubectl",
        cwd: "/home/dev/work/infra",
        hour: 16,
        exit: 0,
    },
    Step {
        label: "kubectl get pods",
        family: "kubectl",
        cwd: "/home/dev/work/infra",
        hour: 16,
        exit: 1,
    },
    Step {
        label: "kubectl describe pod",
        family: "kubectl",
        cwd: "/home/dev/work/infra",
        hour: 16,
        exit: 1,
    },
    Step {
        label: "cargo test",
        family: "cargo",
        cwd: "/home/dev/work/api",
        hour: 11,
        exit: 0,
    },
    Step {
        label: "make deploy",
        family: "make",
        cwd: "/home/dev/work/analytics",
        hour: 16,
        exit: 0,
    },
    Step {
        label: "python3 -m pytest",
        family: "pytest",
        cwd: "/home/dev/work/api",
        hour: 16,
        exit: 1,
    },
    Step {
        label: "npm run build",
        family: "npm",
        cwd: "/home/dev/work/storefront",
        hour: 1,
        exit: 1,
    },
    Step {
        label: "git diff",
        family: "git",
        cwd: "/home/dev/work/storefront",
        hour: 1,
        exit: 0,
    },
    Step {
        label: "cargo clippy",
        family: "cargo",
        cwd: "/home/dev/work/api",
        hour: 1,
        exit: 0,
    },
];

#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub label: &'static str,
    pub hour: u32,
    pub level: u8,
    pub p_fail: f64,
    pub whisper: Option<String>,
}

/// Cached `(family, cwd, hour_bucket)` failure rates from the demo store.
struct Cache {
    by_key: HashMap<(String, String, u8), f64>,
    by_family: HashMap<String, f64>,
    global: f64,
}

impl Cache {
    fn build(conn: &Connection) -> Result<Self> {
        let mut by_key = HashMap::new();
        let mut stmt = conn.prepare(
            "SELECT e.family, d.path, e.hour / 3, COUNT(*), COALESCE(SUM(e.exit_code <> 0), 0)
             FROM events e JOIN dirs d ON d.cwd_id = e.cwd_id
             GROUP BY e.family, d.path, e.hour / 3",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let family: String = row.get(0)?;
            let path: String = row.get(1)?;
            let bucket: i64 = row.get(2)?;
            let n: i64 = row.get(3)?;
            let fails: i64 = row.get(4)?;
            by_key.insert((family, path, bucket as u8), smoothed(n, fails));
        }

        let mut by_family = HashMap::new();
        let mut stmt = conn.prepare(
            "SELECT family, COUNT(*), COALESCE(SUM(exit_code <> 0), 0) FROM events GROUP BY family",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            by_family.insert(row.get::<_, String>(0)?, smoothed(row.get(1)?, row.get(2)?));
        }
        let (n, fails): (i64, i64) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(exit_code <> 0), 0) FROM events",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(Self {
            by_key,
            by_family,
            global: smoothed(n, fails),
        })
    }

    fn p(&self, family: &str, cwd: &str, hour: u32) -> f64 {
        let bucket = (hour.min(23) / 3) as u8;
        self.by_key
            .get(&(family.to_string(), cwd.to_string(), bucket))
            .or_else(|| self.by_family.get(family))
            .copied()
            .unwrap_or(self.global)
    }
}

/// Beta(1, 1) smoothed failure rate, matching the Rust baseline predictor.
fn smoothed(n: i64, fails: i64) -> f64 {
    (fails.max(0) as f64 + 1.0) / (n.max(0) as f64 + 2.0)
}

pub fn replay(conn: &Connection, cfg: &Config) -> Result<Vec<Line>> {
    let cache = Cache::build(conn)?;
    let mut limiter = RateLimiter::new(Duration::ZERO);
    let mut clock = Instant::now();
    let mut streak = 0u32;
    let mut out = Vec::with_capacity(SCRIPT.len());

    for step in SCRIPT {
        let p_fail = cache.p(step.family, step.cwd, step.hour);
        let lvl = level::level(p_fail, streak, step.hour, &cfg.levels);
        // Synthetic spacing so the 5 s cooldown never depends on wall time.
        clock += Duration::from_secs(6);
        // Pure Gemma: live narration per qualifying line, silence on a miss.
        // (Slow by design — one model round trip per whisper.)
        let whisper = if lvl > 0 && limiter.allow_at(lvl, clock) {
            narrator::generate(
                &narrator::Facts {
                    family: step.family.to_string(),
                    p_fail,
                    fail_streak: streak as i64,
                    hour: step.hour,
                    level: lvl,
                    days_since_success: None,
                    kind: narrator::Kind::Post,
                    persona: cfg.general.persona.clone(),
                },
                &cfg.narrator,
            )
        } else {
            None
        };
        streak = if step.exit == 0 { 0 } else { streak + 1 };
        out.push(Line {
            label: step.label,
            hour: step.hour,
            level: lvl,
            p_fail,
            whisper,
        });
    }
    Ok(out)
}

pub fn run(db: &Path) -> Result<()> {
    if !db.exists() {
        bail!(
            "no demo database at {}; run `d43m0n demo --seed` first",
            db.display()
        );
    }
    let conn = store::open(db)?;
    let cfg = Config::load_or_default();
    let lines = replay(&conn, &cfg)?;

    println!(
        "demo replay: {} commands · live narrator · {}",
        lines.len(),
        db.display()
    );
    println!("(no events written; your real database is untouched)\n");
    for line in &lines {
        let verdict = match &line.whisper {
            Some(whisper) => format!("{} L{} {}", cfg.general.glyph, line.level, whisper),
            None if line.level == 0 => "· L0 (silent)".to_string(),
            None => format!("· L{} (rate-limited)", line.level),
        };
        println!(
            "  [{:02}:00] {:<22} p={:.2}  {verdict}",
            line.hour, line.label, line.p_fail
        );
    }

    let levels: BTreeSet<u8> = lines.iter().map(|l| l.level).collect();
    let whispers = lines.iter().filter(|l| l.whisper.is_some()).count();
    println!("\nlevels reached: {levels:?} · whispers: {whispers}");
    for needed in [0, 1, 2, 3] {
        if !levels.contains(&needed) {
            bail!("demo replay missed L{needed}; the demo store may be stale");
        }
    }
    Ok(())
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
            "d43m0n-demo-{tag}-{}-{nanos}.db",
            std::process::id()
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
    fn replay_reaches_every_level() {
        let path = temp_db("run");
        let conn = store::open(&path).unwrap();
        let csv = std::fs::read_to_string(
            store::demo_csv_path().expect("bundled demo history is discoverable"),
        )
        .unwrap();
        assert_eq!(store::seed_demo_csv(&conn, &csv).unwrap(), 1600);

        let cfg = Config::default();
        // Levels are deterministic (cached rates + fixed script); whisper
        // text comes from the live narrator and varies, so only the shape
        // is asserted here.
        let lines = replay(&conn, &cfg).unwrap();

        let levels: BTreeSet<u8> = lines.iter().map(|l| l.level).collect();
        assert_eq!(levels, BTreeSet::from([0, 1, 2, 3]));
        assert!(lines.iter().any(|l| l.level == 0 && l.whisper.is_none()));

        drop(conn);
        cleanup(&path);
    }
}
