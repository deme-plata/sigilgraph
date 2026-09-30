//! Self-update subsystem extracted from main.rs (god-file split, 2026-09-01).
//! Download → verify → pre-flight → swap → relaunch. `use super::*` reaches the
//! shared types (Release, timing) so call sites are unchanged; functions grow into
//! this module one at a time across the split.

use super::*;

/// Download an update binary over HTTPS (TLS ≥1.2, versioned UA, 120s cap).
pub(crate) fn fetch_binary_reqwest(url: &str) -> Result<Vec<u8>, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .user_agent(concat!("sigil-top/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("client init: {e}"))?;
    let resp = client.get(url).send().map_err(|e| format!("request failed: {e}"))?;
    let resp = resp.error_for_status().map_err(|e| format!("HTTP status: {e}"))?;
    if resp.content_length().map_or(false, |n| n > MAX_UPDATE_BYTES) {
        return Err(format!("update body advertises more than {} MiB — refusing", MAX_UPDATE_BYTES >> 20));
    }
    read_capped(resp, MAX_UPDATE_BYTES)
}

/// A release binary is ~36 MB. A download server that streams gigabytes must not be able to exhaust memory
/// before the hash gate ever runs (`resp.bytes()` buffered the whole body with only a 120 s timeout).
pub(crate) const MAX_UPDATE_BYTES: u64 = 512 << 20;

/// Read at most `cap` bytes; a body longer than that is an error, not a truncation.
pub(crate) fn read_capped<R: std::io::Read>(r: R, cap: u64) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    std::io::Read::take(r, cap + 1).read_to_end(&mut buf).map_err(|e| format!("read body: {e}"))?;
    if buf.len() as u64 > cap {
        return Err(format!("update body exceeds {} MiB — refusing", cap >> 20));
    }
    Ok(buf)
}

#[cfg(test)]
mod cap_tests {
    use super::*;
    #[test]
    fn a_body_at_the_cap_is_read_and_one_byte_over_is_refused() {
        assert_eq!(read_capped(&[7u8; 10][..], 10).unwrap().len(), 10);
        assert!(read_capped(&[7u8; 11][..], 10).is_err());
    }
}

/// Pre-flight a freshly-swapped binary with `--selfcheck` BEFORE any handoff.
/// v0.25 FAIL-SAFE: `exec` destroys this process, so we only ever hand off to a
/// binary we've CONFIRMED starts and reports a sane version.
///
/// v0.26 (DeepSeek-hardened): poll on THIS thread so we keep the Child handle and can KILL
/// it on timeout — a binary that HANGS on start must not leak a thread + zombie child (the
/// old wait_with_output-on-a-thread design couldn't reach the child to kill it). `--selfcheck`
/// prints ~7 bytes then exits immediately, so the stdout pipe never fills → no try_wait
/// deadlock. A hung child is itself a strong "don't hand off" signal.
pub(crate) fn preflight_binary(target: &std::path::Path) -> Result<String, String> {
    use std::process::{Command, Stdio};
    use std::io::Read;
    let mut child = Command::new(target)
        .arg("--selfcheck")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn failed: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(6);
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait(); // reap the zombie
                    return Err("--selfcheck timed out (binary hangs on start)".into());
                }
                std::thread::sleep(Duration::from_millis(30));
            }
            Err(e) => { let _ = child.kill(); let _ = child.wait(); return Err(format!("wait failed: {e}")); }
        }
    };
    let mut buf = String::new();
    if let Some(mut out) = child.stdout.take() { let _ = out.read_to_string(&mut buf); }
    if !status.success() {
        return Err(format!("--selfcheck exited {:?}", status.code()));
    }
    let v = buf.trim().to_string();
    if v.is_empty() { Err("empty --selfcheck output".into()) } else { Ok(v) }
}
