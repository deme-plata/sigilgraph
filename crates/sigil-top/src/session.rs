//! Login-session persistence: remembers the `wallet_id` of the last
//! logged-in wallet at `$HOME/.flux/sigil-session.json`.
//!
//! Extracted from `main.rs`. The JSON read/write is split into a pure core
//! (`format_session` / `parse_session`) so the serialization contract is
//! unit-tested without touching `$HOME` or the filesystem.

use super::flux_home;

pub(crate) fn session_path() -> String {
    format!("{}/.flux/sigil-session.json", flux_home())
}

/// Serialize the session record. Pure — no I/O.
pub(crate) fn format_session(id: &str, ts: u64) -> String {
    format!("{{\"wallet_id\":\"{id}\",\"ts\":{ts}}}")
}

/// Extract the `wallet_id` from a session JSON body. Pure — no I/O.
/// Returns `None` for malformed JSON or a missing/non-string `wallet_id`.
pub(crate) fn parse_session(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get("wallet_id")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
}

pub(crate) fn read_session() -> Option<String> {
    parse_session(&std::fs::read_to_string(session_path()).ok()?)
}

pub(crate) fn write_session(id: &str) {
    let _ = std::fs::create_dir_all(format!("{}/.flux", flux_home()));
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = std::fs::write(session_path(), format_session(id, ts));
}

pub(crate) fn clear_session() {
    let _ = std::fs::remove_file(session_path());
}

#[cfg(test)]
mod session_tests {
    use super::{format_session, parse_session};

    #[test]
    fn format_parse_roundtrips() {
        let body = format_session("wallet-abc123", 1_780_000_000);
        assert_eq!(parse_session(&body).as_deref(), Some("wallet-abc123"));
    }

    #[test]
    fn parse_rejects_garbage_and_missing_field() {
        assert!(parse_session("not json at all").is_none());
        assert!(parse_session("{}").is_none());
        assert!(parse_session(r#"{"wallet_id":42}"#).is_none()); // non-string
        assert!(parse_session(r#"{"other":"x"}"#).is_none());
    }
}
