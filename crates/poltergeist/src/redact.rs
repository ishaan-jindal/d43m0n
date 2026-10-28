//! Redaction (SRS 9.2), secret guard (9.3) and narrator output validation.

use std::sync::OnceLock;

use regex::{Regex, RegexSet};

use crate::family;

fn flag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^--?[A-Za-z][A-Za-z0-9_-]*$").expect("valid regex"))
}

fn ansi_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").expect("valid regex"))
}

/// `family` + flags + placeholders. No raw argument text survives.
pub fn redact(cmd: &str) -> String {
    let (fam, args) = family::split_family(cmd);
    let mut out: Vec<String> = Vec::new();
    if !fam.is_empty() {
        out.push(fam);
    }
    for tok in args {
        if tok.is_empty() {
            continue;
        }
        if let Some(flag) = flag_form(&tok) {
            out.push(flag);
        } else if is_url(&tok) {
            out.push("<url>".into());
        } else if is_path(&tok) {
            out.push("<path>".into());
        } else {
            out.push("<arg>".into());
        }
    }
    out.join(" ")
}

fn flag_form(tok: &str) -> Option<String> {
    let base = tok.split('=').next().unwrap_or(tok);
    flag_re().is_match(base).then(|| base.to_string())
}

fn is_url(tok: &str) -> bool {
    let Some(pos) = tok.find("://") else {
        return false;
    };
    let scheme = &tok[..pos];
    !scheme.is_empty()
        && scheme
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

fn is_path(tok: &str) -> bool {
    tok.starts_with('/')
        || tok.starts_with('~')
        || tok.starts_with("./")
        || tok.starts_with("../")
        || tok.contains('/')
}

fn secret_set() -> &'static RegexSet {
    static SET: OnceLock<RegexSet> = OnceLock::new();
    SET.get_or_init(|| {
        RegexSet::new([
            r"AKIA[0-9A-Z]{16}",
            r"gh[pousr]_[A-Za-z0-9]{20,}",
            r"sk-[A-Za-z0-9]{20,}",
            r"xox[abp]-",
            r"(?i)(password|passwd|token|secret|api[_-]?key)\s*[=:]",
            r"(?i)authorization:",
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----",
        ])
        .expect("valid secret regex set")
    })
}

/// True when the raw command must be dropped before it touches anything.
pub fn secret_guard(cmd: &str) -> bool {
    secret_set().is_match(cmd)
}

/// Same as [`secret_guard`] plus user-configured patterns.
pub fn secret_guard_with(cmd: &str, extra: &[String]) -> bool {
    if secret_guard(cmd) {
        return true;
    }
    extra
        .iter()
        .filter_map(|p| Regex::new(p).ok())
        .any(|re| re.is_match(cmd))
}

/// Strip ANSI, then enforce FR-32: one line, no control chars, no markdown,
/// at most `max_words` words. `None` means use the canned fallback.
pub fn validate_whisper(raw: &str, max_words: usize) -> Option<String> {
    let clean = ansi_re().replace_all(raw, "");
    let line = clean.trim();
    if line.is_empty()
        || line.contains('\n')
        || line.contains('\r')
        || line.chars().any(char::is_control)
        || line.contains("**")
        || line.contains("```")
        || line.starts_with('#')
    {
        return None;
    }
    if line.split_whitespace().count() > max_words {
        return None;
    }
    Some(line.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_paths_urls_flags_args() {
        assert_eq!(
            redact(r#"curl -H "Authorization: Bearer abc" https://x.io/a"#),
            "curl -H <arg> <url>"
        );
        assert_eq!(redact("cargo build --release"), "cargo <arg> --release");
        assert_eq!(redact(r#"git commit -m "fix bug""#), "git <arg> -m <arg>");
        assert_eq!(redact("cat /etc/passwd"), "cat <path>");
        assert_eq!(redact("cat ~/.ssh/id_rsa"), "cat <path>");
        assert_eq!(redact("ls src/main.rs"), "ls <path>");
        assert_eq!(redact("cat ../x"), "cat <path>");
        assert_eq!(
            redact("cargo test -- --nocapture"),
            "cargo <arg> <arg> --nocapture"
        );
        assert_eq!(redact("cargo test --foo=bar"), "cargo <arg> --foo");
        assert_eq!(redact("FOO=1 cargo test"), "cargo <arg>");
        assert_eq!(
            redact("sudo -E /usr/bin/apt-get install -y"),
            "apt-get <arg> -y"
        );
        assert_eq!(redact("echo hi > out.txt"), "echo <arg> <arg> <arg>");
    }

    #[test]
    fn secret_guard_positive_samples() {
        for sample in [
            "export AWS=AKIAIOSFODNN7EXAMPLE",
            "curl -H 'Authorization: Bearer abc' https://x",
            "git clone https://ghp_1234567890abcdefghijklmnop@github.com/x/y",
            "use gho_abcdefghijklmnopqrstuvwx",
            "OPENAI=sk-abcdefghijklmnopqrstuvwxyz",
            "slack xoxb-1234",
            "password = hunter2",
            "POSTGRES_PASSWORD=secret",
            "api_key: abc",
            "-----BEGIN RSA PRIVATE KEY-----",
            "-----BEGIN OPENSSH PRIVATE KEY-----",
        ] {
            assert!(secret_guard(sample), "should match: {sample}");
        }
    }

    #[test]
    fn secret_guard_negative_samples() {
        for sample in [
            "cargo build --release",
            "git commit -m 'fix tokenizer'",
            "ls -la /tmp",
            "echo hi",
            "grep -r password .",
        ] {
            assert!(!secret_guard(sample), "should not match: {sample}");
        }
        assert!(secret_guard_with("x CUSTOM-42", &["CUSTOM-\\d+".into()]));
    }

    #[test]
    fn validator_rules() {
        assert_eq!(
            validate_whisper("\x1b[2mThe house remembers.\x1b[0m", 12),
            Some("The house remembers.".into())
        );
        assert_eq!(validate_whisper("two\nlines", 12), None);
        assert_eq!(
            validate_whisper(
                "one two three four five six seven eight nine ten eleven twelve thirteen",
                12
            ),
            None
        );
        assert_eq!(validate_whisper("**bold**", 12), None);
        assert_eq!(validate_whisper("   ", 12), None);
    }
}
