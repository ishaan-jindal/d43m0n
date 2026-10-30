//! Feature builder with leakage-safe prior rates.
//!
//! There are 13 feature rows; `hour` expands to three numeric columns
//! (`hour`, `hour_sin`, `hour_cos`), so one event becomes 15 numbers. The
//! categorical maps are assigned in first-seen order from the event history
//! (stable for appends; unseen keys map to -1).

use std::collections::HashMap;

use anyhow::Result;
use rusqlite::Connection;

/// Numeric width of one feature vector.
pub const N_FEATURES: usize = 15;

pub const FEATURE_NAMES: [&str; N_FEATURES] = [
    "hour",
    "hour_sin",
    "hour_cos",
    "dow",
    "family_id",
    "cwd_id_int",
    "prev_exit_nonzero",
    "fail_streak",
    "log_secs_since_prev",
    "git_dirty",
    "arg_count",
    "has_pipe",
    "has_redirect",
    "family_fail_rate_prior",
    "family_cwd_fail_rate_prior",
];

/// Index of the per-family smoothed failure rate (also the baseline score).
pub const FAMILY_PRIOR: usize = 13;
/// Index of the per-(family, cwd) smoothed failure rate.
pub const FAMILY_CWD_PRIOR: usize = 14;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub n: u64,
    pub fails: u64,
}

impl Counts {
    fn add(&mut self, failed: bool) {
        self.n += 1;
        if failed {
            self.fails += 1;
        }
    }

    /// Beta(1, 1) posterior mean; 0.5 with no observations.
    pub fn smoothed(self) -> f64 {
        (self.fails as f64 + 1.0) / (self.n as f64 + 2.0)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CountsPair {
    pub family: Counts,
    pub family_cwd: Counts,
    pub global: Counts,
}

/// One event's non-derived inputs. Deliberately has no command text.
#[derive(Clone, Debug)]
pub struct RowInput {
    pub family: String,
    pub cwd_id: String,
    pub hour: i64,
    pub dow: i64,
    pub prev_exit: Option<i64>,
    pub fail_streak: i64,
    pub secs_since_prev: Option<i64>,
    pub git_dirty: Option<i64>,
    pub arg_count: i64,
    pub has_pipe: bool,
    pub has_redirect: bool,
}

#[derive(Clone, Debug)]
pub struct Sample {
    pub ts: i64,
    pub family: String,
    pub label: i64,
    pub features: [f64; N_FEATURES],
}

/// First-seen categorical ids. Assigned by scanning history in ts order, so
/// ids never change when new families/dirs appear later.
#[derive(Clone, Debug, Default)]
pub struct IdMaps {
    families: HashMap<String, i64>,
    cwds: HashMap<String, i64>,
}

impl IdMaps {
    pub fn from_conn(conn: &Connection) -> Result<Self> {
        let mut maps = Self::default();
        {
            let mut stmt = conn.prepare(
                "SELECT family FROM events GROUP BY family ORDER BY MIN(ts) ASC, family ASC",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let family: String = row.get(0)?;
                let next = maps.families.len() as i64;
                maps.families.entry(family).or_insert(next);
            }
        }
        {
            let mut stmt = conn.prepare(
                "SELECT cwd_id FROM events GROUP BY cwd_id ORDER BY MIN(ts) ASC, cwd_id ASC",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let cwd: String = row.get(0)?;
                let next = maps.cwds.len() as i64;
                maps.cwds.entry(cwd).or_insert(next);
            }
        }
        Ok(maps)
    }

    pub fn family_id(&self, family: &str) -> f64 {
        self.families.get(family).map(|i| *i as f64).unwrap_or(-1.0)
    }

    pub fn cwd_id_int(&self, cwd_id: &str) -> f64 {
        self.cwds.get(cwd_id).map(|i| *i as f64).unwrap_or(-1.0)
    }
}

/// Per-family and per-(family, cwd) labeled counts, advanced in ts order.
///
/// Rows sharing a timestamp stay buffered until a later timestamp arrives, so
/// [`Self::counts`] only ever reflects strictly earlier timestamps (8.2).
#[derive(Clone, Debug, Default)]
pub struct PriorTracker {
    family: HashMap<String, Counts>,
    family_cwd: HashMap<String, HashMap<String, Counts>>,
    global: Counts,
    pending: Vec<(String, String, bool)>,
    pending_ts: Option<i64>,
}

impl PriorTracker {
    /// Seed from every labeled event already in the database.
    pub fn from_conn(conn: &Connection, exclude: &[i64]) -> Result<Self> {
        let mut tracker = Self::default();
        tracker.seed(conn, &labeled_sql(exclude))?;
        tracker.finish();
        Ok(tracker)
    }

    fn seed(&mut self, conn: &Connection, filter: &str) -> Result<()> {
        let sql = format!(
            "SELECT family, cwd_id, COUNT(*), COALESCE(SUM(exit_code <> 0), 0) \
             FROM events WHERE {filter} GROUP BY family, cwd_id"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let family: String = row.get(0)?;
            let cwd: String = row.get(1)?;
            let counts = Counts {
                n: row.get::<_, i64>(2)?.max(0) as u64,
                fails: row.get::<_, i64>(3)?.max(0) as u64,
            };
            self.apply(&family, &cwd, counts.n, counts.fails);
        }
        Ok(())
    }

    fn apply(&mut self, family: &str, cwd: &str, n: u64, fails: u64) {
        let fam = self.family.entry(family.to_string()).or_default();
        fam.n += n;
        fam.fails += fails;
        let fc = self
            .family_cwd
            .entry(family.to_string())
            .or_default()
            .entry(cwd.to_string())
            .or_default();
        fc.n += n;
        fc.fails += fails;
        self.global.n += n;
        self.global.fails += fails;
    }

    pub fn counts(&self, family: &str, cwd: &str) -> CountsPair {
        let fam = self.family.get(family).copied().unwrap_or_default();
        let fc = self
            .family_cwd
            .get(family)
            .and_then(|m| m.get(cwd))
            .copied()
            .unwrap_or_default();
        CountsPair {
            family: fam,
            family_cwd: fc,
            global: self.global,
        }
    }

    /// Make buffered rows from earlier timestamps visible. Call before
    /// [`Self::counts`] when moving to a new timestamp.
    pub fn begin(&mut self, ts: i64) {
        if self.pending_ts != Some(ts) {
            self.flush();
            self.pending_ts = Some(ts);
        }
    }

    /// Record a labeled event. If `ts` moved forward, buffered rows become
    /// visible to [`Self::counts`].
    pub fn observe(&mut self, ts: i64, family: &str, cwd: &str, failed: bool) {
        self.begin(ts);
        self.pending
            .push((family.to_string(), cwd.to_string(), failed));
    }

    fn flush(&mut self) {
        for (family, cwd, failed) in self.pending.drain(..) {
            let fam = self.family.entry(family.clone()).or_default();
            fam.add(failed);
            let fc = self
                .family_cwd
                .entry(family)
                .or_default()
                .entry(cwd)
                .or_default();
            fc.add(failed);
            self.global.add(failed);
        }
    }

    pub fn finish(&mut self) {
        self.flush();
        self.pending_ts = None;
    }
}

/// `exit_code NOT IN (...)` for the configured excluded codes.
pub fn labeled_sql(exclude: &[i64]) -> String {
    if exclude.is_empty() {
        "1=1".to_string()
    } else {
        let codes: Vec<String> = exclude.iter().map(i64::to_string).collect();
        format!("exit_code NOT IN ({})", codes.join(","))
    }
}

pub fn count_labeled(conn: &Connection, exclude: &[i64]) -> Result<u64> {
    let n: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM events WHERE {}", labeled_sql(exclude)),
        [],
        |row| row.get(0),
    )?;
    Ok(n.max(0) as u64)
}

pub fn vector(input: &RowInput, ids: &IdMaps, counts: &CountsPair) -> [f64; N_FEATURES] {
    let hour = input.hour.clamp(0, 23) as f64;
    let angle = 2.0 * std::f64::consts::PI * hour / 24.0;
    let secs = input.secs_since_prev.unwrap_or(0).max(0) as f64;
    let family_prior = if counts.family.n > 0 {
        counts.family.smoothed()
    } else {
        counts.global.smoothed()
    };
    let family_cwd_prior = if counts.family_cwd.n > 0 {
        counts.family_cwd.smoothed()
    } else {
        family_prior
    };
    [
        hour,
        angle.sin(),
        angle.cos(),
        input.dow as f64,
        ids.family_id(&input.family),
        ids.cwd_id_int(&input.cwd_id),
        i64::from(input.prev_exit.map(|e| e != 0).unwrap_or(false)) as f64,
        input.fail_streak.clamp(0, 10) as f64,
        (1.0 + secs).ln().min(13.3),
        match input.git_dirty {
            Some(0) => 0.0,
            Some(_) => 1.0,
            None => -1.0,
        },
        input.arg_count.max(0) as f64,
        i64::from(input.has_pipe) as f64,
        i64::from(input.has_redirect) as f64,
        family_prior,
        family_cwd_prior,
    ]
}

/// Most recent `limit` labeled samples, featurized in ts order.
///
/// Prior counts for the first sample come from events strictly before its
/// timestamp; no row ever sees its own label or a later one.
pub fn training_samples(conn: &Connection, limit: usize, exclude: &[i64]) -> Result<Vec<Sample>> {
    let limit = limit.max(1);
    let clause = labeled_sql(exclude);
    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM events WHERE {clause}"),
        [],
        |row| row.get(0),
    )?;
    let ids = IdMaps::from_conn(conn)?;

    let cutoff = if total as usize > limit {
        conn.query_row(
            &format!(
                "SELECT ts FROM events WHERE {clause} ORDER BY ts DESC, id DESC LIMIT 1 OFFSET ?1"
            ),
            [limit as i64 - 1],
            |row| row.get::<_, i64>(0),
        )?
    } else {
        i64::MIN
    };

    let mut tracker = PriorTracker::default();
    if cutoff > i64::MIN {
        tracker.seed(conn, &format!("{clause} AND ts < {cutoff}"))?;
        tracker.finish();
    }

    let mut out = Vec::new();
    let mut stmt = conn.prepare(&format!(
        "SELECT ts, cwd_id, family, exit_code, hour, dow, prev_exit, fail_streak, \
                secs_since_prev, git_dirty, arg_count, has_pipe, has_redirect \
         FROM events WHERE {clause} AND ts >= ?1 ORDER BY ts ASC, id ASC"
    ))?;
    let mut rows = stmt.query([cutoff])?;
    while let Some(row) = rows.next()? {
        let ts: i64 = row.get(0)?;
        let cwd_id: String = row.get(1)?;
        let family: String = row.get(2)?;
        let exit: i64 = row.get(3)?;
        let input = RowInput {
            family: family.clone(),
            cwd_id: cwd_id.clone(),
            hour: row.get(4)?,
            dow: row.get(5)?,
            prev_exit: row.get(6)?,
            fail_streak: row.get(7)?,
            secs_since_prev: row.get(8)?,
            git_dirty: row.get(9)?,
            arg_count: row.get(10)?,
            has_pipe: row.get::<_, i64>(11)? != 0,
            has_redirect: row.get::<_, i64>(12)? != 0,
        };
        tracker.begin(ts);
        let counts = tracker.counts(&family, &cwd_id);
        let features = vector(&input, &ids, &counts);
        let failed = exit != 0;
        out.push(Sample {
            ts,
            family,
            label: i64::from(failed),
            features,
        });
        tracker.observe(ts, &input.family, &cwd_id, failed);
    }
    tracker.finish();
    Ok(out)
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
            "d43m0n-feat-{tag}-{}-{nanos}.db",
            std::process::id()
        ))
    }

    fn row(ts: i64, family: &str, cwd: &str, exit: i64) -> EventRow {
        EventRow {
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
        }
    }

    fn cleanup(path: &std::path::Path) {
        for suffix in ["", "-wal", "-shm"] {
            let mut p = path.as_os_str().to_owned();
            p.push(suffix);
            let _ = std::fs::remove_file(std::path::PathBuf::from(p));
        }
    }

    #[test]
    fn vector_shape_and_names() {
        let ids = IdMaps::default();
        let input = RowInput {
            family: "cargo".into(),
            cwd_id: "abc".into(),
            hour: 23,
            dow: 6,
            prev_exit: Some(101),
            fail_streak: 42,
            secs_since_prev: Some(3_600),
            git_dirty: Some(1),
            arg_count: 2,
            has_pipe: true,
            has_redirect: false,
        };
        let v = vector(&input, &ids, &CountsPair::default());
        assert_eq!(v.len(), N_FEATURES);
        assert_eq!(v[0], 23.0);
        assert!((v[1].powi(2) + v[2].powi(2) - 1.0).abs() < 1e-9);
        assert_eq!(v[4], -1.0); // unseen family
        assert_eq!(v[5], -1.0); // unseen cwd
        assert_eq!(v[7], 10.0); // streak capped
        assert_eq!(v[9], 1.0);
        assert_eq!(v[13], 0.5); // Beta(1,1) with no data
        assert_eq!(v[14], 0.5);
    }

    #[test]
    fn priors_exclude_same_and_future_ts() {
        let path = temp_db("leak");
        let conn = store::open(&path).unwrap();
        let cwd = "/tmp/proj";
        store::upsert_dir(&conn, &store::cwd_id(cwd), cwd, 1).unwrap();
        // Two events at the same timestamp must not see each other.
        store::insert_event(&conn, &row(1_000, "cargo", cwd, 0)).unwrap();
        store::insert_event(&conn, &row(1_000, "cargo", cwd, 0)).unwrap();
        store::insert_event(&conn, &row(2_000, "cargo", cwd, 1)).unwrap();

        let samples = training_samples(&conn, usize::MAX, &[130, 148]).unwrap();
        assert_eq!(samples.len(), 3);
        // First row: no earlier events.
        assert!((samples[0].features[FAMILY_PRIOR] - 0.5).abs() < 1e-12);
        // Second row shares ts=1000: still no strictly earlier labels.
        assert!((samples[1].features[FAMILY_PRIOR] - 0.5).abs() < 1e-12);
        // Third row sees both earlier successes only: (0+1)/(2+2) = 0.25.
        assert!((samples[2].features[FAMILY_PRIOR] - 0.25).abs() < 1e-12);
        // And the family+cwd prior matches it.
        assert!((samples[2].features[FAMILY_CWD_PRIOR] - 0.25).abs() < 1e-12);

        // Excluded codes never feed the priors.
        store::insert_event(&conn, &row(3_000, "cargo", cwd, 130)).unwrap();
        let samples = training_samples(&conn, usize::MAX, &[130, 148]).unwrap();
        assert_eq!(samples.len(), 3);
        drop(conn);
        cleanup(&path);
    }

    #[test]
    fn window_limit_keeps_recent_rows_with_history_priors() {
        let path = temp_db("window");
        let conn = store::open(&path).unwrap();
        let cwd = "/tmp/proj";
        store::upsert_dir(&conn, &store::cwd_id(cwd), cwd, 1).unwrap();
        for i in 0..5 {
            store::insert_event(&conn, &row(1_000 + i, "cargo", cwd, 0)).unwrap();
        }
        let samples = training_samples(&conn, 2, &[]).unwrap();
        assert_eq!(samples.len(), 2);
        // Both retained rows still see the 3 older successes: (0+1)/(3+2).
        assert!((samples[0].features[FAMILY_PRIOR] - 0.2).abs() < 1e-12);
        drop(conn);
        cleanup(&path);
    }

    #[test]
    fn ids_are_first_seen_and_stable() {
        let path = temp_db("ids");
        let conn = store::open(&path).unwrap();
        let a = store::cwd_id("/a");
        store::upsert_dir(&conn, &a, "/a", 1).unwrap();
        store::insert_event(&conn, &row(1, "git", "/a", 0)).unwrap();
        store::insert_event(&conn, &row(2, "cargo", "/a", 0)).unwrap();
        let maps = IdMaps::from_conn(&conn).unwrap();
        assert_eq!(maps.family_id("git"), 0.0);
        assert_eq!(maps.family_id("cargo"), 1.0);
        assert_eq!(maps.family_id("nope"), -1.0);
        assert_eq!(maps.cwd_id_int(&a), 0.0);
        drop(conn);
        cleanup(&path);
    }
}
