//! Whisper rate limiting (FR-42).

use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct RateLimiter {
    /// Commands since the last emitted whisper.
    since_whisper: u32,
    last_whisper: Option<Instant>,
    cooldown: Duration,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(Duration::from_secs(5))
    }
}

impl RateLimiter {
    pub fn new(cooldown: Duration) -> Self {
        Self {
            since_whisper: 0,
            last_whisper: None,
            cooldown,
        }
    }

    /// Decide whether the current qualifying command may whisper. L1 allows
    /// every 3rd command, L2 every 2nd, L3 every command, all subject to the
    /// absolute cooldown. L0 never whispers.
    pub fn allow(&mut self, level: u8) -> bool {
        self.allow_at(level, Instant::now())
    }

    pub fn allow_at(&mut self, level: u8, now: Instant) -> bool {
        if level == 0 {
            return false;
        }
        self.since_whisper += 1;
        if let Some(last) = self.last_whisper {
            if now.saturating_duration_since(last) < self.cooldown {
                return false;
            }
        }
        let interval = match level {
            1 => 3,
            2 => 2,
            _ => 1,
        };
        if self.since_whisper >= interval {
            self.since_whisper = 0;
            self.last_whisper = Some(now);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l0_never_whispers() {
        let mut r = RateLimiter::new(Duration::ZERO);
        assert!(!r.allow(0));
    }

    #[test]
    fn l1_every_third_command() {
        let mut r = RateLimiter::new(Duration::ZERO);
        assert!(!r.allow(1));
        assert!(!r.allow(1));
        assert!(r.allow(1));
        assert!(!r.allow(1));
        assert!(!r.allow(1));
        assert!(r.allow(1));
    }

    #[test]
    fn l2_every_second_and_l3_every() {
        let mut r = RateLimiter::new(Duration::ZERO);
        assert!(!r.allow(2));
        assert!(r.allow(2));
        assert!(r.allow(3));
        assert!(r.allow(3));
    }

    #[test]
    fn cooldown_blocks() {
        let mut r = RateLimiter::new(Duration::from_secs(5));
        let t0 = Instant::now();
        assert!(r.allow_at(3, t0));
        assert!(!r.allow_at(3, t0 + Duration::from_secs(4)));
        assert!(r.allow_at(3, t0 + Duration::from_secs(6)));
    }
}
