//! Hook <-> daemon protocol, version 1 (SRS 6.2). Newline-delimited JSON.

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub v: u32,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub cmd: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub ts: Option<i64>,
    #[serde(default)]
    pub git_dirty: Option<bool>,
    #[serde(rename = "exit", default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub duration_ms: Option<i64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Reply {
    #[serde(default)]
    pub v: u32,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub whisper: Option<String>,
    #[serde(default)]
    pub level: Option<u8>,
    #[serde(default)]
    pub p_fail: Option<f64>,
    #[serde(default)]
    pub code: Option<String>,
}

impl Reply {
    pub fn pre(whisper: Option<String>, level: u8, p_fail: f64) -> Self {
        Self {
            v: 1,
            kind: "pre_reply".into(),
            whisper,
            level: Some(level),
            p_fail: Some(p_fail),
            code: None,
        }
    }

    pub fn post(whisper: Option<String>, level: u8) -> Self {
        Self {
            v: 1,
            kind: "post_reply".into(),
            whisper,
            level: Some(level),
            p_fail: None,
            code: None,
        }
    }

    pub fn error(code: &str) -> Self {
        Self {
            v: 1,
            kind: "error".into(),
            whisper: None,
            code: Some(code.into()),
            ..Default::default()
        }
    }
}

pub fn encode(reply: &Reply) -> String {
    let mut line = serde_json::to_string(reply)
        .unwrap_or_else(|_| r#"{"v":1,"type":"error","code":"encode"}"#.to_string());
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pre_and_ignores_unknown_fields() {
        let req: Request = serde_json::from_str(
            r#"{"v":1,"type":"pre","cmd":"cargo build","cwd":"/tmp","ts":1,"git_dirty":true,"extra":42}"#,
        )
        .unwrap();
        assert_eq!(req.v, 1);
        assert_eq!(req.kind, "pre");
        assert_eq!(req.cmd.as_deref(), Some("cargo build"));
        assert_eq!(req.git_dirty, Some(true));
    }

    #[test]
    fn parses_post() {
        let req: Request =
            serde_json::from_str(r#"{"v":1,"type":"post","exit":101,"duration_ms":42,"ts":2}"#)
                .unwrap();
        assert_eq!(req.exit_code, Some(101));
        assert_eq!(req.duration_ms, Some(42));
    }

    #[test]
    fn reply_round_trip() {
        let encoded = encode(&Reply::pre(Some("Again.".into()), 2, 0.71));
        assert!(encoded.ends_with('\n'));
        let reply: Reply = serde_json::from_str(&encoded).unwrap();
        assert_eq!(reply.kind, "pre_reply");
        assert_eq!(reply.whisper.as_deref(), Some("Again."));
        assert_eq!(reply.level, Some(2));
    }
}
