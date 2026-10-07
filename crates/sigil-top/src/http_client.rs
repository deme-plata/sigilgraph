//! Minimal std-only HTTP status client for the TUI. Extracted from `main.rs`.
//!
//! sigil-top is a std-only binary (no reqwest in the hot path), so it speaks
//! just enough HTTP/1.1 to GET a node's `/status` and pull the tip out of the
//! response. `parse_status` is the tolerant part — it finds the JSON object
//! inside whatever HTTP framing surrounds it — and is unit-tested here.

use std::time::Duration;

use super::NodeStatus;
use sigil_top_base::http_get; // moved to sigil-top-base (2026-10-07)

/// Tolerant JSON extraction — grab the outermost object, ignore HTTP framing.
pub(crate) fn parse_status(body: &str) -> Option<NodeStatus> {
    let start = body.find('{')?;
    let end = body.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str(&body[start..=end]).ok()
}

pub(crate) fn fetch(api: &str) -> Result<NodeStatus, ()> {
    // `file:<path>` reads a saved status snapshot — lets you verify the real tip
    // offline / over a transport this std-only binary doesn't speak (e.g. pipe a
    // curl of the https testnet snapshot through a file). Otherwise plain http GET.
    let body = if let Some(path) = api.strip_prefix("file:") {
        std::fs::read_to_string(path).ok()
    } else {
        http_get(api, Duration::from_millis(800))
    };
    body.and_then(|b| parse_status(&b)).ok_or(())
}

#[cfg(test)]
mod http_client_tests {
    use super::parse_status;

    #[test]
    fn parse_status_pulls_json_out_of_http_framing() {
        // Real responses carry a status line + headers before the body — the
        // parser must skip all that and read the object (height via alias).
        let framed = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n\
                      {\"network_id\":\"sigil-g2\",\"block_height\":42}";
        let s = parse_status(framed).expect("extract + parse the JSON object");
        assert_eq!(s.height, 42, "block_height alias maps to height");
    }

    #[test]
    fn parse_status_rejects_bodies_with_no_object_without_panicking() {
        assert!(parse_status("HTTP/1.1 500 Internal Server Error\r\n\r\n").is_none());
        assert!(parse_status("").is_none());
        // Inverted braces (`}` before `{`) must not slice backwards / panic.
        assert!(parse_status("} not json {").is_none());
    }
}
