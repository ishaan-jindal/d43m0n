//! Echoes: "this worked here before" for a `(family, cwd)` whose
//! most recent event failed after at least one earlier success.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

use crate::level::now_ts;

/// Days since the last success for `(family, cwd)`, if any.
pub fn days_since_success(conn: &Connection, family: &str, cwd_id: &str) -> Result<Option<i64>> {
    let last: Option<i64> = conn
        .query_row(
            "SELECT MAX(ts) FROM events WHERE family = ?1 AND cwd_id = ?2 AND exit_code = 0",
            params![family, cwd_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    Ok(last.map(|ts| (now_ts() - ts).max(0) / 86_400))
}

/// FR-50: return days since last success when the pair succeeded before and
/// its most recent event failed.
pub fn should_echo(conn: &Connection, family: &str, cwd_id: &str) -> Result<Option<i64>> {
    let last_exit: Option<i64> = conn
        .query_row(
            "SELECT exit_code FROM events WHERE family = ?1 AND cwd_id = ?2 \
             ORDER BY ts DESC, id DESC LIMIT 1",
            params![family, cwd_id],
            |row| row.get(0),
        )
        .optional()?;
    match last_exit {
        Some(code) if code != 0 => days_since_success(conn, family, cwd_id),
        _ => Ok(None),
    }
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
            "d43m0n-echo-{tag}-{}-{nanos}.db",
            std::process::id()
        ))
    }

    fn cleanup(path: &std::path::Path) {
        for suffix in ["", "-wal", "-shm"] {
            let mut p = path.as_os_str().to_owned();
            p.push(suffix);
            let _ = std::fs::remove_file(std::path::PathBuf::from(p));
        }
    }

    fn insert(conn: &Connection, ts: i64, family: &str, cwd: &str, exit: i64) {
        let row = EventRow {
            ts,
            cwd_id: store::cwd_id(cwd),
            family: family.into(),
            cmd_redacted: format!("{family} <arg>"),
            exit_code: exit,
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
    fn echo_needs_success_then_failure() {
        let path = temp_db("flow");
        let conn = store::open(&path).unwrap();
        let cwd = "/tmp/proj";
        let five_days_ago = now_ts() - 5 * 86_400;

        insert(&conn, five_days_ago, "cargo", cwd, 0);
        // Most recent event is a success: no echo yet.
        assert_eq!(
            should_echo(&conn, "cargo", &store::cwd_id(cwd)).unwrap(),
            None
        );

        // Failure after an earlier success arms the echo, with the success age.
        insert(&conn, five_days_ago + 10, "cargo", cwd, 101);
        assert_eq!(
            should_echo(&conn, "cargo", &store::cwd_id(cwd)).unwrap(),
            Some(5)
        );

        // A later success clears the echo.
        insert(&conn, five_days_ago + 10, "cargo", cwd, 0);
        assert_eq!(
            should_echo(&conn, "cargo", &store::cwd_id(cwd)).unwrap(),
            None
        );

        // A later failure after a success re-arms it.
        insert(&conn, five_days_ago + 20, "cargo", cwd, 101);
        assert!(should_echo(&conn, "cargo", &store::cwd_id(cwd))
            .unwrap()
            .is_some());

        // Never-succeeded pair: no echo.
        insert(&conn, five_days_ago + 30, "pytest", cwd, 1);
        assert_eq!(
            should_echo(&conn, "pytest", &store::cwd_id(cwd)).unwrap(),
            None
        );

        drop(conn);
        cleanup(&path);
    }
}
