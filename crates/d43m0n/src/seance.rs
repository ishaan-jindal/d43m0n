//! Séance: dormant directories, redacted last commands,
//! optional narration from counts and dates only.

use std::path::Path;

use anyhow::Result;
use rusqlite::Connection;

use crate::config::Config;
use crate::{level, narrator, store};

/// One directory whose most recent event is older than the cutoff.
#[derive(Clone, Debug, PartialEq)]
pub struct Dormant {
    pub path: String,
    pub last_ts: i64,
    pub events: i64,
    /// Up to 5 most recent redacted forms, oldest first.
    pub recent: Vec<String>,
}

pub fn dormant(conn: &Connection, now: i64, days: u32) -> Result<Vec<Dormant>> {
    let cutoff = now - days as i64 * 86_400;
    let mut stmt = conn.prepare(
        "SELECT d.cwd_id, d.path, MAX(e.ts) AS last_ts, COUNT(e.id)
         FROM dirs d JOIN events e ON e.cwd_id = d.cwd_id
         GROUP BY d.cwd_id
         HAVING last_ts < ?1
         ORDER BY last_ts ASC, d.path ASC",
    )?;
    let rows = stmt
        .query_map([cutoff], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut out = Vec::with_capacity(rows.len());
    for (cwd_id, path, last_ts, events) in rows {
        let mut recent_stmt = conn.prepare(
            "SELECT cmd_redacted FROM events WHERE cwd_id = ?1
             ORDER BY ts DESC, id DESC LIMIT 5",
        )?;
        let mut recent = recent_stmt
            .query_map([&cwd_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        recent.reverse();
        out.push(Dormant {
            path,
            last_ts,
            events,
            recent,
        });
    }
    Ok(out)
}

/// Local `YYYY-MM-DD` for a unix timestamp.
pub fn date(ts: i64) -> String {
    // SAFETY: localtime_r writes into a zeroed, properly sized tm and keeps no
    // pointer. A null return means the timestamp was out of range.
    unsafe {
        let t = ts as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return "unknown".into();
        }
        format!(
            "{:04}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday
        )
    }
}

pub fn run(db: &Path, days: u32, narrate: bool) -> Result<()> {
    if !db.exists() {
        println!("séance: no events recorded yet (start the daemon first)");
        return Ok(());
    }
    let conn = store::open(db)?;
    let now = level::now_ts();
    let rows = dormant(&conn, now, days)?;
    if rows.is_empty() {
        println!("séance: no directories dormant for {days}+ days");
        return Ok(());
    }

    let cfg = Config::load_or_default();
    println!(
        "séance: {} directories dormant for {days}+ days (oldest first)\n",
        rows.len()
    );
    for (index, d) in rows.iter().enumerate() {
        let departed = !Path::new(&d.path).exists();
        println!("{}{}", d.path, if departed { "  (departed)" } else { "" });
        println!("  last active {} · {} events", date(d.last_ts), d.events);
        if !d.recent.is_empty() {
            println!("  last commands: {}", d.recent.join(", "));
        }
        if narrate && index < 3 {
            let days_dormant = (now - d.last_ts).max(0) / 86_400;
            // Pure Gemma: no fallback lines. Silence when the model misses.
            if let Some(line) = narrator::generate_seance(
                days_dormant,
                d.events,
                &cfg.general.persona,
                &cfg.narrator,
            ) {
                println!("  {} {line}", cfg.general.glyph);
            }
        }
        println!();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{self, EventRow};

    fn temp_db(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "d43m0n-seance-{tag}-{}-{nanos}.db",
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

    fn insert(conn: &Connection, ts: i64, cwd: &str, label: &str) {
        let row = EventRow {
            ts,
            cwd_id: store::cwd_id(cwd),
            family: "cargo".into(),
            cmd_redacted: label.into(),
            exit_code: 0,
            duration_ms: 1,
            hour: 12,
            dow: 1,
            prev_exit: None,
            fail_streak: 0,
            secs_since_prev: None,
            git_dirty: None,
            arg_count: 0,
            has_pipe: false,
            has_redirect: false,
            p_fail_pred: None,
            predictor: None,
        };
        store::upsert_dir(conn, &row.cwd_id, cwd, ts).unwrap();
        store::insert_event(conn, &row).unwrap();
    }

    #[test]
    fn lists_only_old_dirs_oldest_first_with_five_redacted_forms() {
        let path = temp_db("dormant");
        let conn = store::open(&path).unwrap();
        let now = 2_000_000_000;
        let old = "/tmp/d43m0n-test/old-project";
        let older = "/tmp/d43m0n-test/older-project";
        let fresh = "/tmp/d43m0n-test/fresh-project";

        for i in 0..7 {
            insert(
                &conn,
                now - 120 * 86_400 + i,
                old,
                &format!("cargo <arg> {i}"),
            );
        }
        insert(&conn, now - 200 * 86_400, older, "cargo build");
        insert(&conn, now - 5 * 86_400, fresh, "cargo test");

        let rows = dormant(&conn, now, 90).unwrap();
        let paths: Vec<&str> = rows.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec![older, old]);
        assert_eq!(rows[1].events, 7);
        assert_eq!(rows[1].recent.len(), 5);
        // Oldest of the last five first; the two newest are dropped.
        assert_eq!(rows[1].recent.first().unwrap(), "cargo <arg> 2");
        assert_eq!(rows[1].recent.last().unwrap(), "cargo <arg> 6");
        // Test paths do not exist: the séance marks them departed.
        assert!(!Path::new(old).exists());

        // A lower threshold re-admits the fresh directory.
        assert_eq!(dormant(&conn, now, 1).unwrap().len(), 3);
        drop(conn);
        cleanup(&path);
    }

    #[test]
    fn date_is_iso_shaped() {
        assert_eq!(date(0).len(), 10);
        assert_eq!(date(0).matches('-').count(), 2);
    }
}
