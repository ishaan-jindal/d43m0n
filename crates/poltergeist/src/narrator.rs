//! Narrator client (SRS 5.4): Ollama on localhost, structured facts only,
//! strict output validation, canned-line fallback.

use std::time::Duration;

use serde_json::{json, Value};

use crate::config::Narrator as NarratorCfg;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Pre,
    Post,
    Echo,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Pre => "pre",
            Kind::Post => "post",
            Kind::Echo => "echo",
        }
    }
}

/// The only information the narrator ever receives (FR-30). No command text,
/// arguments, or paths.
#[derive(Clone, Debug)]
pub struct Facts {
    pub family: String,
    pub p_fail: f64,
    pub fail_streak: i64,
    pub hour: u32,
    pub level: u8,
    pub days_since_success: Option<i64>,
    pub kind: Kind,
    pub persona: String,
}

/// FR-35 persona. Terse, dry, eerie; no advice, no command repetition.
pub fn system_prompt() -> &'static str {
    "You are a ghost haunting a user's terminal. Speak one short line, at most 12 words, \
     terse, dry, eerie. Never give advice. Never repeat or describe the command or its arguments. \
     Never insult the user's identity. Only tease the situation. No emoji, no markdown, \
     no quotation marks, no line breaks."
}

pub fn p_fail_bucket(p: f64) -> &'static str {
    if p >= 0.85 {
        "certain"
    } else if p >= 0.60 {
        "high"
    } else if p >= 0.30 {
        "medium"
    } else {
        "low"
    }
}

pub fn hour_bucket(hour: u32) -> &'static str {
    match hour.min(23) {
        0..=4 => "night",
        5..=11 => "morning",
        12..=17 => "afternoon",
        _ => "evening",
    }
}

/// Structured fact sheet. Kept as key: value lines so the small model can use
/// them without free-form context.
pub fn user_prompt(facts: &Facts) -> String {
    let days = facts
        .days_since_success
        .map(|d| d.to_string())
        .unwrap_or_else(|| "unknown".into());
    format!(
        "kind: {}\nfamily: {}\np_fail: {}\nfail_streak: {}\nhour_bucket: {}\nlevel: {}\n\
         days_since_last_success: {}\npersona: {}\n",
        facts.kind.as_str(),
        facts.family,
        p_fail_bucket(facts.p_fail),
        facts.fail_streak,
        hour_bucket(facts.hour),
        facts.level,
        days,
        facts.persona,
    )
}

/// FR-32 validation plus control-character/ANSI stripping. `None` means the
/// caller falls back to a canned line.
pub fn validate(raw: &str, max_words: usize) -> Option<String> {
    let text = crate::redact::validate_whisper(raw, max_words)?;
    if text.chars().any(is_emoji) {
        return None;
    }
    // Markdown emphasis/links would render oddly in a terminal.
    if text.contains('`')
        || text.contains('*')
        || text.contains('_')
        || text.contains('[')
        || text.contains(']')
        || text.contains('~')
    {
        return None;
    }
    Some(text)
}

fn is_emoji(c: char) -> bool {
    matches!(
        c as u32,
        0x1F000..=0x1FAFF
            | 0x2300..=0x23FF
            | 0x25A0..=0x25FF
            | 0x2600..=0x27BF
            | 0x2B00..=0x2BFF
            | 0xFE00..=0xFE0F
            | 0x200D
    )
}

/// FR-61: séance facts are counts and dates only — never paths or commands.
pub fn seance_prompt(days_dormant: i64, events: i64, persona: &str) -> String {
    format!("kind: seance\ndays_dormant: {days_dormant}\nevents: {events}\npersona: {persona}\n")
}

/// Ask Ollama for one validated line. `None` after one retry means canned.
pub fn generate(facts: &Facts, cfg: &NarratorCfg) -> Option<String> {
    ask(&user_prompt(facts), cfg)
}

/// A short remark for a dormant directory (FR-61).
pub fn generate_seance(
    days_dormant: i64,
    events: i64,
    persona: &str,
    cfg: &NarratorCfg,
) -> Option<String> {
    ask(&seance_prompt(days_dormant, events, persona), cfg)
}

fn ask(prompt: &str, cfg: &NarratorCfg) -> Option<String> {
    if cfg.backend != "ollama" {
        return None;
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(cfg.timeout_ms.max(1)))
        .build()
        .ok()?;
    let url = format!("{}/api/generate", cfg.endpoint.trim_end_matches('/'));
    let body = json!({
        "model": cfg.model,
        "system": system_prompt(),
        "prompt": prompt,
        "stream": false,
        // Thinking models (e.g. gemma4) burn the token budget on
        // chain-of-thought and return an empty `response` otherwise.
        "think": false,
        "keep_alive": "30m",
        "options": {"temperature": 0.9, "num_predict": 32},
    });

    for attempt in 0..2 {
        let reply = client.post(&url).json(&body).send();
        let reply = match reply {
            Ok(reply) => reply,
            Err(e) => {
                tracing::warn!(error = %e, attempt, "narrator: request failed");
                continue;
            }
        };
        let Ok(value) = reply.json::<Value>() else {
            tracing::warn!("narrator: bad JSON (attempt {attempt})");
            continue;
        };
        let Some(text) = value.get("response").and_then(Value::as_str) else {
            tracing::warn!("narrator: no response field (attempt {attempt})");
            continue;
        };
        if let Some(line) = validate(text, cfg.max_words) {
            return Some(line);
        }
        tracing::warn!("narrator: rejected {:?}", text);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validator_accepts_plain_one_liner_and_strips_ansi() {
        assert_eq!(
            validate("\x1b[2mThe house remembers.\x1b[0m", 12),
            Some("The house remembers.".into())
        );
        assert_eq!(validate("  Too quiet.  ", 12), Some("Too quiet.".into()));
    }

    #[test]
    fn validator_rejects_long_newline_markdown_emoji() {
        assert_eq!(
            validate(
                "one two three four five six seven eight nine ten eleven twelve thirteen",
                12
            ),
            None
        );
        assert_eq!(validate("two\nlines", 12), None);
        assert_eq!(validate("**bold**", 12), None);
        assert_eq!(validate("`code`", 12), None);
        assert_eq!(validate("The house 👻 remembers", 12), None);
        assert_eq!(validate("The house remembers ⌛", 12), None);
        assert_eq!(validate("", 12), None);
    }

    #[test]
    fn prompt_contains_only_structured_facts() {
        let facts = Facts {
            family: "cargo".into(),
            p_fail: 0.71,
            fail_streak: 2,
            hour: 2,
            level: 3,
            days_since_success: Some(4),
            kind: Kind::Pre,
            persona: "poltergeist".into(),
        };
        let prompt = user_prompt(&facts);
        assert!(prompt.contains("family: cargo"));
        assert!(prompt.contains("p_fail: high"));
        assert!(prompt.contains("hour_bucket: night"));
        assert!(prompt.contains("days_since_last_success: 4"));
        // No paths, arguments, or raw command text can reach the prompt.
        assert!(!prompt.contains('/'));
        assert!(!prompt.contains("--release"));
        assert!(!prompt.contains("<arg>"));
    }

    #[test]
    fn seance_prompt_carries_only_counts_and_dates() {
        let prompt = seance_prompt(94, 42, "poltergeist");
        assert!(prompt.contains("days_dormant: 94"));
        assert!(prompt.contains("events: 42"));
        assert!(!prompt.contains('/'));
        assert!(!prompt.contains("cargo"));
        assert!(!prompt.contains("<arg>"));
    }

    #[test]
    fn buckets_match_thresholds() {
        assert_eq!(p_fail_bucket(0.29), "low");
        assert_eq!(p_fail_bucket(0.30), "medium");
        assert_eq!(p_fail_bucket(0.60), "high");
        assert_eq!(p_fail_bucket(0.85), "certain");
        assert_eq!(hour_bucket(0), "night");
        assert_eq!(hour_bucket(4), "night");
        assert_eq!(hour_bucket(5), "morning");
        assert_eq!(hour_bucket(13), "afternoon");
        assert_eq!(hour_bucket(22), "evening");
    }
}
