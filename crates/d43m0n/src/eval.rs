//! `d43m0n eval`: time-ordered holdout, TabPFN vs baseline,
//! ROC AUC / log loss / accuracy at 0.5, plus the latency benchmark.

use std::cmp::Ordering;
use std::path::Path;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use serde_json::json;

use crate::config::Config;
use crate::features::{self, Sample, FAMILY_PRIOR, N_FEATURES};
use crate::{level, narrator, predictor, proto, store};

#[derive(Clone, Debug)]
pub struct Metrics {
    pub n: usize,
    pub auc: Option<f64>,
    pub logloss: f64,
    pub acc: f64,
}

impl Metrics {
    pub fn compute(labels: &[i64], preds: &[f64]) -> Self {
        let n = labels.len().min(preds.len());
        let mut logloss = 0.0;
        let mut correct = 0usize;
        for i in 0..n {
            let p = preds[i].clamp(1e-15, 1.0 - 1e-15);
            let y = labels[i] as f64;
            logloss -= y * p.ln() + (1.0 - y) * (1.0 - p).ln();
            if (preds[i] >= 0.5) == (labels[i] == 1) {
                correct += 1;
            }
        }
        Self {
            n,
            auc: auc(&labels[..n], &preds[..n]),
            logloss: if n == 0 { 0.0 } else { logloss / n as f64 },
            acc: if n == 0 {
                0.0
            } else {
                correct as f64 / n as f64
            },
        }
    }

    fn to_json(&self) -> serde_json::Value {
        json!({
            "n": self.n,
            "auc": self.auc,
            "logloss": self.logloss,
            "acc": self.acc,
        })
    }
}

/// Rank-based ROC AUC with average ranks for ties. `None` if one class is
/// missing from the holdout.
pub fn auc(labels: &[i64], preds: &[f64]) -> Option<f64> {
    let n = labels.len().min(preds.len());
    let positives = labels[..n].iter().filter(|l| **l == 1).count();
    let negatives = n - positives;
    if positives == 0 || negatives == 0 {
        return None;
    }
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|a, b| preds[*a].partial_cmp(&preds[*b]).unwrap_or(Ordering::Equal));
    let mut rank_sum_positive = 0.0;
    let mut i = 0;
    while i < n {
        let mut j = i;
        while j + 1 < n && preds[order[j + 1]] == preds[order[i]] {
            j += 1;
        }
        // Ranks are 1-based; tied values share the average rank.
        let avg_rank = ((i + 1) + (j + 1)) as f64 / 2.0;
        for idx in &order[i..=j] {
            if labels[*idx] == 1 {
                rank_sum_positive += avg_rank;
            }
        }
        i = j + 1;
    }
    let pos = positives as f64;
    let neg = negatives as f64;
    Some((rank_sum_positive - pos * (pos + 1.0) / 2.0) / (pos * neg))
}

/// Time-ordered split (8.2): the first `ratio` of rows train, the rest hold
/// out. No shuffling.
pub fn split(samples: &[Sample], ratio: f64) -> (&[Sample], &[Sample]) {
    let cut = ((samples.len() as f64) * ratio).floor() as usize;
    let cut = cut.clamp(1.min(samples.len()), samples.len());
    samples.split_at(cut)
}

struct Report {
    events: usize,
    train: usize,
    holdout: usize,
    baseline: Metrics,
    tabpfn: std::result::Result<Metrics, String>,
}

fn report(db: &Path) -> Result<Report> {
    if !db.exists() {
        bail!(
            "no event database at {} (start the daemon or run `d43m0n demo --seed`)",
            db.display()
        );
    }
    let cfg = Config::load_or_default();
    let conn = store::open(db)?;
    let samples = features::training_samples(&conn, usize::MAX, &cfg.predictor.fail_exclude_codes)?;
    if samples.len() < 10 {
        bail!(
            "need at least 10 labeled events (have {}); run `d43m0n demo --seed` for a demo set",
            samples.len()
        );
    }
    let (train, holdout) = split(&samples, 0.8);
    let labels: Vec<i64> = holdout.iter().map(|s| s.label).collect();
    let base_preds: Vec<f64> = holdout.iter().map(|s| s.features[FAMILY_PRIOR]).collect();
    let baseline = Metrics::compute(&labels, &base_preds);

    let train_rows: Vec<[f64; N_FEATURES]> = train.iter().map(|s| s.features).collect();
    let train_labels: Vec<i64> = train.iter().map(|s| s.label).collect();
    let holdout_rows: Vec<[f64; N_FEATURES]> = holdout.iter().map(|s| s.features).collect();
    let tabpfn = predictor::eval_tabpfn(&train_rows, &train_labels, &holdout_rows)
        .map(|preds| Metrics::compute(&labels, &preds))
        .map_err(|e| format!("{e:#}"));

    Ok(Report {
        events: samples.len(),
        train: train.len(),
        holdout: holdout.len(),
        baseline,
        tabpfn,
    })
}

pub fn run(db: &Path, as_json: bool, latency: bool) -> Result<()> {
    let latency = if latency {
        Some(Latency::measure()?)
    } else {
        None
    };
    let report = report(db);

    if as_json {
        let mut value = match &report {
            Ok(r) => json!({
                "db": db.display().to_string(),
                "events": r.events,
                "train": r.train,
                "holdout": r.holdout,
                "split": "time-ordered 80/20",
                "baseline": r.baseline.to_json(),
                "tabpfn": match &r.tabpfn {
                    Ok(metrics) => json!({"available": true, "metrics": metrics.to_json()}),
                    Err(reason) => json!({"available": false, "reason": reason}),
                },
            }),
            Err(e) => json!({
                "db": db.display().to_string(),
                "error": format!("{e:#}"),
            }),
        };
        if let Some(lat) = &latency {
            value["latency"] = lat.to_json();
        }
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    match report {
        Ok(r) => {
            eprintln!(
                "eval: {} labeled events, time-ordered 80/20 ({} train / {} holdout)",
                r.events, r.train, r.holdout
            );
            println!(
                "events: {}  train: {}  holdout: {}  split: time-ordered 80/20",
                r.events, r.train, r.holdout
            );
            print_metrics("baseline", &r.baseline);
            match &r.tabpfn {
                Ok(metrics) => print_metrics("tabpfn", metrics),
                Err(reason) => println!("tabpfn    unavailable: {reason}"),
            }
        }
        Err(e) if latency.is_some() => eprintln!("eval: {e:#} (latency only)"),
        Err(e) => return Err(e),
    }
    if let Some(lat) = &latency {
        lat.print();
    }
    Ok(())
}

fn print_metrics(name: &str, m: &Metrics) {
    let auc = match m.auc {
        Some(v) => format!("{v:.3}"),
        None => "n/a".to_string(),
    };
    println!(
        "{name:<9} auc {auc}  logloss {:.4}  acc {:.3}  n {}",
        m.logloss, m.acc, m.n
    );
}

/* ----------------------------- latency (FR-82) ----------------------------- */

/// In-process timings, milliseconds. No shell and no daemon socket: the same
/// functions the daemon runs, called back to back over a synthetic history.
pub struct Latency {
    n: usize,
    pre: Vec<f64>,
    post: Vec<f64>,
    predict: Vec<f64>,
    narrate: Vec<f64>,
    narrate_live: usize,
}

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1_000.0
}

fn pick(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((q * sorted.len() as f64).ceil() as usize)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    sorted[idx]
}

/// Nearest-rank percentile (sorts a copy).
pub fn percentile(values: &[f64], q: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    pick(&sorted, q)
}

fn p50_p95(values: &[f64]) -> (f64, f64) {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    (pick(&sorted, 0.50), pick(&sorted, 0.95))
}

impl Latency {
    pub fn measure() -> Result<Self> {
        const N: usize = 200;
        let conn = Connection::open_in_memory().context("opening the in-memory store")?;
        conn.execute_batch(store::SCHEMA)?;
        store::seed_demo(&conn, 300)?;

        let cfg = Config::load_or_default();
        let ids = features::IdMaps::from_conn(&conn)?;
        let mut priors =
            features::PriorTracker::from_conn(&conn, &cfg.predictor.fail_exclude_codes)?;
        let cwd = "/tmp/d43m0n/latency-bench";
        let cwd_id = store::cwd_id(cwd);
        let start_ts = level::now_ts() - N as i64 * 60;

        let mut out = Self {
            n: N,
            pre: Vec::with_capacity(N),
            post: Vec::with_capacity(N),
            predict: Vec::with_capacity(N),
            narrate: Vec::with_capacity(N),
            narrate_live: 0,
        };

        for i in 0..N {
            let ts = start_ts + i as i64 * 60;
            let exit = if i % 5 == 0 { 1 } else { 0 };
            let request = format!(
                r#"{{"v":1,"type":"pre","cmd":"cargo test --release","cwd":"{cwd}","ts":{ts},"git_dirty":true}}"#
            );
            let start = Instant::now();
            let req: proto::Request = serde_json::from_str(&request).expect("bench request");
            let cmd = req.cmd.as_deref().expect("bench cmd");
            store::upsert_dir(&conn, &cwd_id, cwd, ts)?;
            let last = store::last_event(&conn)?;
            let scored = features::score_pre(
                cmd,
                &cwd_id,
                ts,
                req.git_dirty.map(|d| d as i64),
                last.as_ref(),
                &[],
                &ids,
                &mut priors,
            )
            .expect("bench command must score");
            let p_fail = scored.p_fail;
            let lvl = scored.level(p_fail, &cfg.levels);
            // Pure Gemma: the pre path never has a line to show.
            let whisper: Option<String> = None;
            let _ = proto::encode(&proto::Reply::pre(whisper, lvl, p_fail));
            out.pre.push(elapsed_ms(start));

            let start = Instant::now();
            let counts = priors.counts(&scored.family, &cwd_id);
            let v = features::vector(&scored.input, &ids, &counts);
            std::hint::black_box(v[FAMILY_PRIOR]);
            out.predict.push(elapsed_ms(start));

            let start = Instant::now();
            let prev = store::last_event(&conn)?;
            let fail_streak = if exit == 0 {
                0
            } else {
                prev.as_ref().map(|e| e.fail_streak + 1).unwrap_or(1)
            };
            let row = store::EventRow {
                ts,
                cwd_id: cwd_id.clone(),
                family: scored.family,
                cmd_redacted: scored.redacted,
                exit_code: exit,
                duration_ms: 42,
                hour: scored.input.hour,
                dow: scored.input.dow,
                prev_exit: prev.as_ref().map(|e| e.exit_code),
                fail_streak,
                secs_since_prev: prev.as_ref().map(|e| (ts - e.ts).max(0)),
                git_dirty: scored.input.git_dirty,
                arg_count: scored.input.arg_count,
                has_pipe: scored.input.has_pipe,
                has_redirect: scored.input.has_redirect,
                p_fail_pred: Some(p_fail),
                predictor: Some("baseline".into()),
            };
            store::insert_event(&conn, &row)?;
            out.post.push(elapsed_ms(start));
        }

        let facts = narrator::Facts {
            family: "cargo".into(),
            p_fail: 0.71,
            fail_streak: 2,
            hour: 2,
            level: 3,
            days_since_success: Some(4),
            kind: narrator::Kind::Post,
            persona: cfg.general.persona.clone(),
        };
        for _ in 0..N {
            let start = Instant::now();
            // Pure Gemma: a miss is silence, not a fallback line.
            if narrator::generate(&facts, &cfg.narrator).is_some() {
                out.narrate_live += 1;
            }
            out.narrate.push(elapsed_ms(start));
        }
        Ok(out)
    }

    pub fn to_json(&self) -> serde_json::Value {
        let (pre_p50, pre_p95) = p50_p95(&self.pre);
        let (post_p50, post_p95) = p50_p95(&self.post);
        let (predict_p50, predict_p95) = p50_p95(&self.predict);
        let (narrate_p50, narrate_p95) = p50_p95(&self.narrate);
        json!({
            "N": self.n,
            "pre_p50": pre_p50,
            "pre_p95": pre_p95,
            "post_p50": post_p50,
            "post_p95": post_p95,
            "predict_p50": predict_p50,
            "predict_p95": predict_p95,
            "narrate_p50": narrate_p50,
            "narrate_p95": narrate_p95,
        })
    }

    pub fn print(&self) {
        let (pre_p50, pre_p95) = p50_p95(&self.pre);
        let (post_p50, post_p95) = p50_p95(&self.post);
        let (predict_p50, predict_p95) = p50_p95(&self.predict);
        let (narrate_p50, narrate_p95) = p50_p95(&self.narrate);
        println!(
            "latency: N={} in-process (no shell, no daemon socket)",
            self.n
        );
        println!("  {:<8} p50 {pre_p50:.3} ms  p95 {pre_p95:.3} ms", "pre");
        println!("  {:<8} p50 {post_p50:.3} ms  p95 {post_p95:.3} ms", "post");
        println!(
            "  {:<8} p50 {predict_p50:.3} ms  p95 {predict_p95:.3} ms  (Rust baseline)",
            "predict"
        );
        let mode = if self.narrate_live == 0 {
            "silent"
        } else {
            "live"
        };
        println!(
            "  {:<8} p50 {narrate_p50:.3} ms  p95 {narrate_p95:.3} ms  ({mode})",
            "narrate"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ts: i64) -> Sample {
        Sample {
            ts,
            family: "cargo".into(),
            label: 0,
            features: [0.0; N_FEATURES],
        }
    }

    #[test]
    fn split_is_time_ordered() {
        let samples: Vec<Sample> = (0..100).map(sample).collect();
        let (train, holdout) = split(&samples, 0.8);
        assert_eq!(train.len(), 80);
        assert_eq!(holdout.len(), 20);
        assert!(train.last().unwrap().ts < holdout.first().unwrap().ts);
        // Small sets keep at least one row on each side.
        let (train, holdout) = split(&samples[..3], 0.8);
        assert_eq!((train.len(), holdout.len()), (2, 1));
    }

    #[test]
    fn auc_handles_separation_and_ties() {
        assert_eq!(auc(&[0, 0, 1, 1], &[0.1, 0.2, 0.8, 0.9]), Some(1.0));
        assert_eq!(auc(&[1, 0, 1, 0], &[0.5, 0.5, 0.5, 0.5]), Some(0.5));
        assert_eq!(auc(&[0, 0], &[0.1, 0.2]), None);
    }

    #[test]
    fn logloss_and_accuracy() {
        let m = Metrics::compute(&[1, 0, 1, 0], &[0.9, 0.1, 0.8, 0.2]);
        assert!((m.acc - 1.0).abs() < 1e-12);
        assert!(m.logloss > 0.0 && m.logloss < 0.2);
        let m = Metrics::compute(&[1, 0], &[0.4, 0.6]);
        assert!((m.acc - 0.0).abs() < 1e-12);
    }

    #[test]
    fn latency_percentiles_use_nearest_rank() {
        let values: Vec<f64> = (1..=100).map(|v| v as f64).collect();
        assert_eq!(percentile(&values, 0.50), 50.0);
        assert_eq!(percentile(&values, 0.95), 95.0);
        assert_eq!(percentile(&values, 0.0), 1.0);
        assert_eq!(percentile(&[], 0.5), 0.0);
        assert_eq!(percentile(&[1.0, 2.0, 3.0], 0.5), 2.0);
    }
}
