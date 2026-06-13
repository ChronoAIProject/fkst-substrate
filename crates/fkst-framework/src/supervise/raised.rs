//! RAISED stdout protocol parser.
//!
//! Framework prints exactly one final line `RAISED: <base64-url-encoded-JSON>` on stdout
//! before exit. The JSON decodes to `[{queue, payload}, ...]`. Multiple RAISED lines →
//! last wins. No RAISED line → empty list, no error. Malformed base64/JSON → log
//! warning, treat as empty (don't crash supervisor).
//!
//! Scanning from end of stdout buffer prevents log lines like
//! `log.info("RAISED: foo")` from being mistaken for the actual protocol.

use base64::Engine;
use fkst_common::Event;
use serde::Deserialize;
use serde_json::Value;
use tracing::warn;

#[derive(Debug, Deserialize)]
struct RaisedEntry {
    queue: String,
    #[serde(default)]
    payload: Value,
}

pub fn parse_raised_line(line: &str) -> Vec<Event> {
    if !line.trim_start().starts_with("RAISED: ") {
        return Vec::new();
    }

    let b64_part = line.trim_start().trim_start_matches("RAISED: ").trim();
    let decoded_bytes = match base64::engine::general_purpose::URL_SAFE.decode(b64_part) {
        Ok(b) => b,
        Err(e) => {
            warn!("RAISED line base64 decode failed: {}", e);
            return Vec::new();
        }
    };
    let entries: Vec<RaisedEntry> = match serde_json::from_slice(&decoded_bytes) {
        Ok(v) => v,
        Err(e) => {
            warn!("RAISED line JSON parse failed: {}", e);
            return Vec::new();
        }
    };
    entries
        .into_iter()
        .map(|e| Event::new(e.queue, e.payload))
        .collect()
}

/// Parse stdout into a list of (queue, Event) tuples. Returns empty vec if no RAISED line.
pub fn parse_raised(stdout: &str) -> Vec<Event> {
    // Find the LAST line starting with "RAISED: " (after any trailing whitespace).
    let last = stdout
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with("RAISED: "));
    let Some(line) = last else {
        return Vec::new();
    };
    parse_raised_line(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn encode(json_str: &str) -> String {
        base64::engine::general_purpose::URL_SAFE.encode(json_str.as_bytes())
    }

    #[test]
    fn empty_stdout_returns_empty() {
        assert!(parse_raised("").is_empty());
    }

    #[test]
    fn stdout_without_raised_returns_empty() {
        let s = "hello\nworld\nlog.info: something\n";
        assert!(parse_raised(s).is_empty());
    }

    #[test]
    fn single_raised_line_parses() {
        let payload = encode(r#"[{"queue":"done","payload":{"n":1}}]"#);
        let stdout = format!("RAISED: {}\n", payload);
        let events = parse_raised(&stdout);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].queue, "done");
        assert_eq!(events[0].payload, serde_json::json!({"n": 1}));
    }

    #[test]
    fn multiple_raised_lines_last_wins() {
        let first = encode(r#"[{"queue":"first","payload":null}]"#);
        let second = encode(r#"[{"queue":"second","payload":null}]"#);
        let stdout = format!("RAISED: {}\nRAISED: {}\n", first, second);
        let events = parse_raised(&stdout);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].queue, "second");
    }

    #[test]
    fn malformed_base64_returns_empty_no_panic() {
        let stdout = "RAISED: !!!not-valid-base64!!!\n";
        assert!(parse_raised(stdout).is_empty());
    }

    #[test]
    fn malformed_json_returns_empty_no_panic() {
        let bad = base64::engine::general_purpose::URL_SAFE.encode(b"not-json");
        let stdout = format!("RAISED: {}\n", bad);
        assert!(parse_raised(&stdout).is_empty());
    }

    #[test]
    fn log_line_containing_raised_does_not_confuse_parser() {
        // A dept that logs "log.info: RAISED: foo" should not trigger.
        // We scan from end + require "RAISED: " at line start (after trim_start).
        // This case has "RAISED" mid-line, so it's NOT picked up.
        let stdout = "log: user said RAISED: not-real\n";
        assert!(parse_raised(stdout).is_empty());
    }

    #[test]
    fn raised_with_no_payload_field_uses_null() {
        let p = encode(r#"[{"queue":"x"}]"#);
        let stdout = format!("RAISED: {}\n", p);
        let events = parse_raised(&stdout);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload, serde_json::Value::Null);
    }
}
