//! # sigil-top-base — what every sigil-top crate shares
//!
//! Split out of `sigil-top`'s `main.rs` (2026-10-07). rustc's front end runs on one
//! thread per crate and sigil-top was 1.8 MB in one crate, so its subsystems are
//! becoming separate crates; these helpers are the part they all need. Moved
//! verbatim except `http_get`'s user-agent version, now set once by the binary
//! ([`set_version`]) so it still says the binary's version, not this crate's.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

static VERSION: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

/// Set the version `http_get` reports in its user-agent. Call once, first thing in `main`.
pub fn set_version(v: &'static str) {
    let _ = VERSION.set(v);
}

/// The version set by [`set_version`] (`"0"` if it never was, e.g. in tests).
pub fn version() -> &'static str {
    VERSION.get().copied().unwrap_or("0")
}

/// v0.7.21: Windows-safe "Instant N seconds in the past".
/// `Instant::now() - Duration` panics with "overflow when subtracting duration
/// from instant" when the monotonic clock is younger than the duration — which
/// happens at process start on Windows (QPC epoch is near process/boot start),
/// crashing sigil-top before the TUI even draws. `checked_sub` returns None in
/// that case; we fall back to `now`. The intent of these call sites is "make the
/// first periodic check overdue"; on the rare clamp the first tick is merely
/// delayed by one interval instead of firing immediately — never a crash.
pub fn instant_ago(secs: u64) -> Instant {
    let now = Instant::now();
    now.checked_sub(Duration::from_secs(secs)).unwrap_or(now)
}

/// When the ratatui TUI owns the screen, raw `eprintln!` from the background P2P
/// thread (and elsewhere) smears the frame. Once the TUI starts we flip this and
/// route those lines to a logfile instead, so the dashboard stays clean.
pub static IN_TUI: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn log_line(s: String) {
    // v7.1.39 (grogu-sync-perf, 2026-08-19): `full-sync` and every other headless
    // subcommand NEVER call run_tui(), so IN_TUI stays false for their entire
    // lifetime — which means EVERY crate::tlog!() call anywhere in the sync/fetch
    // code ([snap-diag], [mesh] DIAL-FAIL, [anchor], [sync] snapshot-pull, VCATCH,
    // etc.) was a silent no-op in the one mode most suited to reproducible testing
    // (headless full-sync, CI gates). Root-caused live: a fresh full-sync repro run
    // showed 0 peers for 85s straight with ZERO diagnostic output anywhere, even
    // though extensive tlog! diagnostics were added earlier this session specifically
    // to explain exactly this kind of stall. Fix: outside the TUI, tlog! lines are
    // harmless on stderr (nothing owns the terminal) — print them directly, same as
    // the pre-TUI eprintln convention documented at run_tui's top ("pre-TUI prints go
    // to stderr — exactly where pre-UI diagnostics belong"). Zero behavior change
    // inside the TUI (still routes to the logfile only, unsmeared).
    if !IN_TUI.load(std::sync::atomic::Ordering::Relaxed) {
        eprintln!("{s}");
        return;
    }
    if IN_TUI.load(std::sync::atomic::Ordering::Relaxed) {
        let p = std::env::var("HOME")
            .map(|h| format!("{h}/.sigil-top.log"))
            .or_else(|_| std::env::var("TEMP").map(|t| format!("{t}\\sigil-top.log")))
            .unwrap_or_else(|_| "sigil-top.log".into());
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
            use std::io::Write;
            let _ = writeln!(f, "{s}");
        }
        // v0.26: cap the logfile so a 24/7 run can't fill the disk. Checked cheaply once
        // every LOG_CAP_EVERY writes; when it exceeds 4 MB, keep only the last ~1 MB.
        static LOG_WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        const LOG_CAP_EVERY: u64 = 512;
        const LOG_MAX: u64 = 4 * 1024 * 1024;
        const LOG_KEEP: u64 = 1024 * 1024;
        if LOG_WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % LOG_CAP_EVERY == 0 {
            if std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0) > LOG_MAX {
                let tail = {
                    use std::io::{Read, Seek, SeekFrom};
                    std::fs::File::open(&p).ok().and_then(|mut fh| {
                        let len = fh.metadata().map(|m| m.len()).unwrap_or(0);
                        fh.seek(SeekFrom::Start(len.saturating_sub(LOG_KEEP))).ok()?;
                        let mut b = Vec::new(); fh.take(LOG_KEEP).read_to_end(&mut b).ok()?; Some(b)
                    })
                };
                if let Some(b) = tail { let _ = std::fs::write(&p, &b); }
            }
        }
    } else {
        eprintln!("{s}");
    }
}

/// `tlog!(...)` — like `eprintln!` but TUI-safe (goes to the logfile while the
/// dashboard is up). Use for all background/diagnostic output.
#[macro_export]
macro_rules! tlog {
    ($($a:tt)*) => {{ $crate::log_line(format!($($a)*)); }};
}

/// One-shot HTTP/1.1 GET over a plain TCP socket. Returns the RAW response
/// (status line + headers + body); callers extract what they need. `None` on
/// any connect/timeout/decode failure — never panics.
pub fn http_get(url: &str, timeout: Duration) -> Option<String> {
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
        "GET {path} HTTP/1.1\r\nHost: {hostport}\r\nConnection: close\r\nUser-Agent: sigil-top/{}\r\n\r\n",
        version()
    );
    stream.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).ok()?;
    String::from_utf8(buf).ok()
}

/// v0.64: append a startup breadcrumb to %TEMP%/sigil-top-startup.log (best-effort).
/// The Windows-startup boot trace flushes per line (each call opens+appends+closes),
/// so the LAST written line is a reliable "died here" marker under wine-verify.
pub fn boot_trace(msg: &str) {
    use std::io::Write;
    let p = std::env::temp_dir().join("sigil-top-startup.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{}", msg);
    }
}
