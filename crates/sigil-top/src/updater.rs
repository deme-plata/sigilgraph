//! Self-update subsystem extracted from main.rs (god-file split, 2026-09-01).
//! Download → verify → pre-flight → swap → relaunch. `use super::*` reaches the
//! shared types (Release, timing) so call sites are unchanged; functions grow into
//! this module one at a time across the split.

use super::*;

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
