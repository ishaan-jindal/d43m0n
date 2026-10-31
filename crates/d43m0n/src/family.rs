//! Command normalization: strip wrappers and env assignments, keep the family.

use std::path::Path;

const WRAPPERS: &[&str] = &["sudo", "env", "time", "nohup", "command"];

/// Split a command into its normalized family and its remaining argument
/// tokens. Tokens preserve shell quoting (quotes removed, quoted spans kept
/// together). Operators (`|`, `>`, `;`, ...) become their own tokens.
pub fn split_family(cmd: &str) -> (String, Vec<String>) {
    let tokens = tokenize(cmd);
    let mut i = 0usize;

    'outer: loop {
        while i < tokens.len() && is_env_assignment(&tokens[i]) {
            i += 1;
        }
        let Some(tok) = tokens.get(i).map(String::as_str) else {
            break;
        };
        if !WRAPPERS.contains(&tok) {
            break;
        }
        i += 1;
        match tok {
            "sudo" => skip_flags(
                &tokens,
                &mut i,
                &[
                    "-u",
                    "-g",
                    "-p",
                    "-C",
                    "-h",
                    "-r",
                    "-t",
                    "-U",
                    "-D",
                    "--user",
                    "--group",
                    "--prompt",
                    "--close-from",
                    "--host",
                    "--role",
                    "--type",
                    "--other-user",
                ],
            ),
            "env" => skip_flags(
                &tokens,
                &mut i,
                &["-u", "--unset", "-C", "--chdir", "-S", "--split-string"],
            ),
            _ => skip_flags(&tokens, &mut i, &[]),
        }
        continue 'outer;
    }

    let Some(exe) = tokens.get(i).cloned() else {
        return (String::new(), Vec::new());
    };
    let family = Path::new(&exe)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or(exe);
    (family, tokens[i + 1..].to_vec())
}

pub fn family(cmd: &str) -> String {
    split_family(cmd).0
}

fn skip_flags(tokens: &[String], i: &mut usize, value_flags: &[&str]) {
    while *i < tokens.len() && tokens[*i].starts_with('-') {
        let takes_value = value_flags.contains(&tokens[*i].as_str());
        *i += 1;
        if takes_value && *i < tokens.len() {
            *i += 1;
        }
    }
}

fn is_env_assignment(tok: &str) -> bool {
    let Some(eq) = tok.find('=') else {
        return false;
    };
    let name = &tok[..eq];
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Minimal shell-like tokenizer: whitespace separates, quotes group, unquoted
/// operators split. Not a full shell parser; commands are only ever tokenized
/// for normalization and redaction, never re-executed.
pub fn tokenize(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    let mut chars = cmd.chars().peekable();

    let flush = |cur: &mut String, started: &mut bool, out: &mut Vec<String>| {
        if *started {
            out.push(std::mem::take(cur));
            *started = false;
        }
    };

    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if c == '\\' && q == '"' {
                    if let Some(next) = chars.next() {
                        cur.push(next);
                    }
                } else {
                    cur.push(c);
                }
                started = true;
            }
            None => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                    started = true;
                } else if c.is_whitespace() {
                    flush(&mut cur, &mut started, &mut out);
                } else if matches!(c, '|' | '&' | ';' | '<' | '>') {
                    flush(&mut cur, &mut started, &mut out);
                    out.push(c.to_string());
                } else {
                    cur.push(c);
                    started = true;
                }
            }
        }
    }
    flush(&mut cur, &mut started, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_wrappers_and_env() {
        assert_eq!(family("sudo apt install x"), "apt");
        assert_eq!(family("sudo -u root /usr/bin/apt update"), "apt");
        assert_eq!(family("env FOO=1 BAR=2 cargo test"), "cargo");
        assert_eq!(family("FOO=1 time nohup ./target/debug/foo --x"), "foo");
        assert_eq!(family("command ls -la"), "ls");
        assert_eq!(family("/usr/bin/git status"), "git");
        assert_eq!(family("time -p make"), "make");
        assert_eq!(family("  "), "");
    }

    #[test]
    fn args_after_wrappers() {
        let (fam, args) = split_family("sudo -E cargo build --release");
        assert_eq!(fam, "cargo");
        assert_eq!(args, vec!["build", "--release"]);
    }

    #[test]
    fn tokenizer_keeps_quoted_spans() {
        assert_eq!(
            tokenize(r#"curl -H "Authorization: Bearer abc" https://x.io/a"#),
            vec!["curl", "-H", "Authorization: Bearer abc", "https://x.io/a"]
        );
        assert_eq!(
            tokenize("echo hi | tee out"),
            vec!["echo", "hi", "|", "tee", "out"]
        );
    }
}
