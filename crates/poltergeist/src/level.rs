//! Pure haunting-level function (FR-40/41) and small clock helpers.

use crate::config::Levels;

/// Computed level, 0..=3. L0 never whispers (FR-41a).
pub fn level(p_fail: f64, fail_streak: u32, hour: u32, cfg: &Levels) -> u8 {
    let base = if p_fail >= cfg.l2 || fail_streak >= cfg.streak_l2 {
        2
    } else if p_fail >= cfg.l1 {
        1
    } else {
        0
    };

    let night = hour >= cfg.l3_night_start && hour < cfg.l3_night_end;
    if p_fail >= cfg.l3_p || (night && base >= 1) {
        3
    } else {
        base
    }
}

pub fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Local hour (0..23) and day of week (0..6, Sunday = 0) for a unix timestamp.
pub fn local_hour_dow(ts: i64) -> (u32, u32) {
    let t = ts as libc::time_t;
    // SAFETY: localtime_r writes into a zeroed, properly sized tm and does not
    // retain the pointer. Null return means the timestamp was out of range.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return (0, 0);
        }
        (tm.tm_hour as u32, tm.tm_wday as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Levels {
        Levels::default()
    }

    #[test]
    fn thresholds() {
        let c = cfg();
        assert_eq!(level(0.0, 0, 12, &c), 0);
        assert_eq!(level(0.29, 1, 12, &c), 0);
        assert_eq!(level(0.30, 0, 12, &c), 1);
        assert_eq!(level(0.59, 0, 12, &c), 1);
        assert_eq!(level(0.60, 0, 12, &c), 2);
        assert_eq!(level(0.10, 3, 12, &c), 2);
        assert_eq!(level(0.85, 0, 12, &c), 3);
    }

    #[test]
    fn night_escalates_only_above_l0() {
        let c = cfg();
        assert_eq!(level(0.40, 0, 0, &c), 3);
        assert_eq!(level(0.40, 0, 4, &c), 3);
        assert_eq!(level(0.40, 0, 5, &c), 1);
        assert_eq!(level(0.10, 0, 2, &c), 0);
        assert_eq!(level(0.90, 0, 12, &c), 3);
    }

    #[test]
    fn local_hour_is_in_range() {
        let (h, d) = local_hour_dow(now_ts());
        assert!(h < 24);
        assert!(d < 7);
    }
}
