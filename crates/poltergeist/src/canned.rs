//! Canned whisper bank (FR-33), loaded from `data/canned.toml`.
//!
//! Runtime order: `$XDG_DATA_HOME/poltergeist/canned.toml`, then the copy
//! embedded at build time. Selection is random without immediate repeats per
//! `(kind, level)`.

use std::collections::HashMap;

use serde::Deserialize;

use crate::config;

const EMBEDDED: &str = include_str!("../../../data/canned.toml");

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Pre,
    Post,
    Echo,
}

#[derive(Deserialize, Default)]
struct Lines {
    #[serde(default)]
    lines: Vec<String>,
}

#[derive(Deserialize, Default)]
struct LevelBank {
    #[serde(default)]
    pre: Lines,
    #[serde(default)]
    post: Lines,
}

#[derive(Deserialize, Default)]
struct BankFile {
    #[serde(default)]
    level1: LevelBank,
    #[serde(default)]
    level2: LevelBank,
    #[serde(default)]
    level3: LevelBank,
    #[serde(default)]
    echo: Lines,
}

pub struct Bank {
    pre: [Vec<String>; 4],
    post: [Vec<String>; 4],
    echo: Vec<String>,
    last: HashMap<(Kind, u8), usize>,
    state: u64,
}

impl Default for Bank {
    fn default() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x5EED)
            ^ ((std::process::id() as u64) << 17);
        Self {
            pre: Default::default(),
            post: Default::default(),
            echo: Vec::new(),
            last: HashMap::new(),
            state: seed | 1,
        }
    }
}

impl Bank {
    /// Load the user copy if present, else the embedded bank.
    pub fn load() -> Self {
        let runtime = config::data_dir().join("canned.toml");
        std::fs::read_to_string(runtime)
            .ok()
            .and_then(|text| Self::parse(&text))
            .or_else(|| Self::parse(EMBEDDED))
            .unwrap_or_default()
    }

    /// Embedded/user bank with a fixed selection seed (`demo --run`).
    pub fn load_seeded(seed: u64) -> Self {
        let mut bank = Self::load();
        bank.state = seed | 1;
        bank
    }

    fn parse(text: &str) -> Option<Self> {
        let file: BankFile = toml::from_str(text).ok()?;
        let mut bank = Self::default();
        let levels = [file.level1, file.level2, file.level3];
        for (i, level) in levels.into_iter().enumerate() {
            bank.pre[i + 1] = level.pre.lines;
            bank.post[i + 1] = level.post.lines;
        }
        bank.echo = file.echo.lines;
        Some(bank)
    }

    /// Random line without an immediate repeat. `None` for an empty bank.
    pub fn line(&mut self, level: u8, kind: Kind) -> Option<String> {
        let len = match kind {
            Kind::Pre => self.pre.get(level as usize)?.len(),
            Kind::Post => self.post.get(level as usize)?.len(),
            Kind::Echo => self.echo.len(),
        };
        if len == 0 {
            return None;
        }
        let idx = self.pick(kind, level, len);
        let bank = match kind {
            Kind::Pre => &self.pre[level as usize],
            Kind::Post => &self.post[level as usize],
            Kind::Echo => &self.echo,
        };
        Some(bank[idx].clone())
    }

    fn pick(&mut self, kind: Kind, level: u8, len: usize) -> usize {
        // ponytail: tiny LCG; selection only needs to look random to a human.
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let mut idx = ((self.state >> 33) as usize) % len;
        if len > 1 {
            if self.last.get(&(kind, level)) == Some(&idx) {
                idx = (idx + 1) % len;
            }
        }
        self.last.insert((kind, level), idx);
        idx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn embedded() -> Bank {
        Bank::parse(EMBEDDED).expect("embedded canned.toml parses")
    }

    #[test]
    fn every_level_and_echo_has_at_least_eight_lines() {
        let bank = embedded();
        for level in 1..=3 {
            assert_eq!(bank.pre[level].len(), 8, "pre level {level}");
            assert_eq!(bank.post[level].len(), 8, "post level {level}");
        }
        assert_eq!(bank.echo.len(), 8);
        assert!(bank.pre[0].is_empty());
    }

    #[test]
    fn selection_never_repeats_immediately() {
        let mut bank = embedded();
        let mut prev: Option<String> = None;
        for _ in 0..64 {
            let line = bank.line(2, Kind::Pre).unwrap();
            if let Some(prev) = &prev {
                assert_ne!(prev, &line);
            }
            assert!(bank.pre[2].contains(&line));
            prev = Some(line);
        }
        // Echo and post banks are independent.
        for _ in 0..16 {
            let echo = bank.line(0, Kind::Echo).unwrap();
            assert!(bank.echo.contains(&echo));
            let post = bank.line(3, Kind::Post).unwrap();
            assert!(bank.post[3].contains(&post));
        }
    }

    #[test]
    fn l0_and_unknown_levels_have_no_line() {
        let mut bank = embedded();
        assert!(bank.line(0, Kind::Pre).is_none());
        assert!(bank.line(9, Kind::Post).is_none());
    }
}
