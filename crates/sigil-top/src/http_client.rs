//! Minimal std-only HTTP status client for the TUI. Extracted from `main.rs`.
//!
//! sigil-top is a std-only binary (no reqwest in the hot path), so it speaks
//! just enough HTTP/1.1 to GET a node's `/status` and pull the tip out of the
//! response. `parse_status` is the tolerant part — it finds the JSON object
//! inside whatever HTTP framing surrounds it — and is unit-tested here.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use super::{NodeStatus, VERSION};

/// One-shot HTTP/1.1 GET over a plain TCP socket. Returns the RAW response
/// (status line + headers + body); callers extract what they need. `None` on
/// any connect/timeout/decode failure — never panics.
pub(crate) fn http_get(url: &str, timeout: Duration) -> Option<String> {
    let rest = url.strip_prefix("http://")?;
    let (hostport, path) = match rest.split_once('/') {
        Some((hp, p)) => (hp.to_string(), format!("/{p}")),
        None => (rest.to_string(), "/".to_string()),
    };
    let addr = if hostport.contains(':') { hostport.clone() } else { format!("{hostport}:80") };
    let sock = addr.to_socket_addrs().ok()?.next()?;
    let mut stream = TcpStream::connect_timeout(&sock, timeout).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {hostport}\r\nConnection: close\r\nUser-Agent: sigil-top/{VERSION}\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).ok()?;
    String::from_utf8(buf).ok()
}

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
