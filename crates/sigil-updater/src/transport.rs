//! Transport-layer glue — turn raw gossipsub `data: Vec<u8>` bytes from
//! `/sigil/g0/release` into a verified, fetched, applied binary swap.
//!
//! Caller is sigil-node's tick loop, which already filters by topic. This
//! module does the rest:
//!
//!   1. parse bytes as `ReleaseAnnouncement` (silent skip if not JSON or wrong shape)
//!   2. `verify_announcement` (sig + format precheck)
//!   3. fetch binary from `announcement.binary_url` via a pluggable [`BinaryFetcher`]
//!   4. `verify_binary_bytes` (full BLAKE3)
//!   5. `apply_to_target` (atomic .new→target→.bak swap)
//!   6. report what happened so the caller can log + (optionally) respawn
//!
//! The fetcher trait is sync because flux-p2p's drain_events() is sync —
//! sigil-node's tick loop runs in a tokio context but drains synchronously,
//! and we don't want to push async-everywhere into sigil-updater. For HTTP,
//! the [`CurlFetcher`] shells out to `curl` (no reqwest dep). Callers can
//! also pass a closure for in-memory tests.
//!
//! Skip-vs-fail policy: a release we can't parse, a sig that doesn't verify,
//! or a download that 404s is a *silent skip* — the gossipsub bus carries
//! traffic for many topics over time, including future-schema messages we
//! shouldn't crash on. Only a successful verify+fetch followed by an apply
//! failure raises an error, because by then we'd already trusted the bytes.

use std::path::Path;
use std::process::Command;

use crate::announcement::{ReleaseAnnouncement, UpdaterError};
use crate::apply::{apply_to_target, ApplyOutcome};
use crate::verify::{verify_announcement, verify_binary_bytes, VerifyOk};

/// What handling one release message produced. Granular variants so the
/// caller can log helpfully without re-parsing.
#[derive(Debug)]
pub enum HandledRelease {
    /// Bytes didn't look like a `ReleaseAnnouncement`. Skipped silently.
    NotAnAnnouncement { reason: String },
    /// Parsed but signature/format check failed. Skipped silently.
    VerifyFailed { error: UpdaterError, peer: Option<String> },
    /// Verified, version is not strictly greater than `current_version` — skipped.
    /// (Caller passes their current version; nothing here decides what's "current".)
    NotNewer { announcement_version: String, current_version: String },
    /// Verified + same-or-newer version, but the binary fetch failed. Skipped.
    FetchFailed { url: String, error: String },
    /// Binary fetched but doesn't match the announcement's BLAKE3/size. Skipped.
    BinaryHashMismatch { url: String, error: UpdaterError },
    /// Full success — binary swapped into `target`. Caller arranges respawn.
    Applied { verify: VerifyOk, outcome: ApplyOutcome },
}

impl HandledRelease {
    /// True iff the binary on disk has changed as a result of this call.
    pub fn applied(&self) -> bool {
        matches!(self, HandledRelease::Applied { .. })
    }
}

/// Pluggable fetcher for the announcement's binary URL. Sync by design —
/// flux-p2p's drain_events is sync, and we don't want to push async into
/// sigil-updater. Implementors return the full binary bytes or an error.
pub trait BinaryFetcher {
    fn fetch(&self, url: &str) -> Result<Vec<u8>, String>;
}

/// Default: shell out to `curl`. No reqwest/hyper/etc. dependency. Good
/// enough for Phase 0 — every Linux box has curl, and the binary is fetched
/// at most once per release per node.
pub struct CurlFetcher {
    /// Max seconds curl will spend on a single fetch attempt.
    pub max_secs: u64,
    /// Optional explicit binary path; default uses `curl` from $PATH.
    pub curl_path: Option<String>,
}

impl Default for CurlFetcher {
    fn default() -> Self {
        Self { max_secs: 120, curl_path: None }
    }
}

impl BinaryFetcher for CurlFetcher {
    fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
        let curl = self.curl_path.as_deref().unwrap_or("curl");
        let max_str = self.max_secs.to_string();
        let out = Command::new(curl)
            .args(["-fsSL", "--max-time", &max_str, url])
            .output()
            .map_err(|e| format!("spawn {}: {}", curl, e))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(format!(
                "curl exit {}: {}",
                out.status.code().unwrap_or(-1),
                stderr.chars().take(500).collect::<String>()
            ));
        }
        Ok(out.stdout)
    }
}

/// Closure-based fetcher — handy for tests + special transports (file://,
/// IPFS, etc.) without writing a fresh trait impl.
pub struct ClosureFetcher<F>(pub F)
where
    F: Fn(&str) -> Result<Vec<u8>, String>;

impl<F> BinaryFetcher for ClosureFetcher<F>
where
    F: Fn(&str) -> Result<Vec<u8>, String>,
{
    fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
        (self.0)(url)
    }
}

/// One-shot: parse bytes, verify announcement, gate on version, fetch
/// binary, verify hash, apply to `target`. Returns [`HandledRelease`] in
/// every case — does NOT return `Err` for ordinary skip cases (wrong shape,
/// invalid sig, no-newer, fetch failed). Errors are reserved for cases where
/// we already trusted the bytes but couldn't apply (filesystem failure).
///
/// `current_version` is the version sigil-node is running now — anything <=
/// that is skipped. Use `""` to apply everything (e.g. for tests / forced
/// downgrades — but in production never do this).
pub fn handle_release_message<F: BinaryFetcher>(
    data: &[u8],
    peer: Option<&str>,
    current_version: &str,
    target: &Path,
    fetcher: &F,
) -> Result<HandledRelease, UpdaterError> {
    // 1. parse
    let announcement: ReleaseAnnouncement = match serde_json::from_slice(data) {
        Ok(a) => a,
        Err(e) => {
            return Ok(HandledRelease::NotAnAnnouncement {
                reason: format!("json: {}", e),
            });
        }
    };

    // 2. verify announcement (sig + format)
    let verify = match verify_announcement(&announcement) {
        Ok(v) => v,
        Err(e) => {
            return Ok(HandledRelease::VerifyFailed {
                error: e,
                peer: peer.map(String::from),
            });
        }
    };

    // 3. version gate — only apply strictly newer
    if !is_strictly_newer(&announcement.version, current_version) {
        return Ok(HandledRelease::NotNewer {
            announcement_version: announcement.version.clone(),
            current_version: current_version.into(),
        });
    }

    // 4. fetch binary
    let bytes = match fetcher.fetch(&announcement.binary_url) {
        Ok(b) => b,
        Err(e) => {
            return Ok(HandledRelease::FetchFailed {
                url: announcement.binary_url.clone(),
                error: e,
            });
        }
    };

    // 5. verify hash
    if let Err(e) = verify_binary_bytes(&announcement, &bytes) {
        return Ok(HandledRelease::BinaryHashMismatch {
            url: announcement.binary_url.clone(),
            error: e,
        });
    }

    // 6. apply — propagate filesystem errors as Err, not as a HandledRelease
    let outcome = apply_to_target(&announcement, &bytes, target)?;
    Ok(HandledRelease::Applied { verify, outcome })
}

/// Strict semver `>` comparison (ignoring pre-release tags). Same parser as
/// flux-arena-agent v0.1.5: take the leading ASCII-digit run of each dot
/// segment, ignore the rest. Robust to "0.0.2-rc1" vs "0.0.2".
pub fn is_strictly_newer(announcement: &str, current: &str) -> bool {
    semver_tuple(announcement) > semver_tuple(current)
}

fn semver_tuple(s: &str) -> (u32, u32, u32) {
    let s = s.trim_start_matches('v');
    let mut it = s.splitn(3, '.').map(|p| {
        p.chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse::<u32>()
            .unwrap_or(0)
    });
    (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::announcement::ReleaseAnnouncement;

    fn signed_announcement(version: &str, binary: &[u8]) -> (ReleaseAnnouncement, Vec<u8>) {
        let (sk, pk) = flux_sqisign::keygen();
        let mut a = ReleaseAnnouncement::unsigned(
            "sigil-node",
            version,
            "https://example.org/binary",
            binary,
            b"{}".to_vec(),
            pk,
            0, 1024, 0, "transport-test",
        );
        a.sign(&sk).expect("sign");
        let bytes = serde_json::to_vec(&a).expect("ser");
        (a, bytes)
    }

    #[test]
    fn semver_compare_basic() {
        assert!(is_strictly_newer("0.0.2", "0.0.1"));
        assert!(is_strictly_newer("0.1.0", "0.0.99"));
        assert!(is_strictly_newer("1.0.0", "0.99.99"));
        assert!(!is_strictly_newer("0.0.1", "0.0.1"));
        assert!(!is_strictly_newer("0.0.1", "0.0.2"));
        assert!(is_strictly_newer("0.0.2-rc1", "0.0.1"));
        assert!(is_strictly_newer("v0.0.2", "v0.0.1"));
    }

    #[test]
    fn empty_current_means_apply_any_versioned_release() {
        // Empty string parses to (0,0,0). Anything > (0,0,0) is newer.
        assert!(is_strictly_newer("0.0.1", ""));
        assert!(is_strictly_newer("1.0.0", ""));
        // (0,0,0) > (0,0,0) is false — empty == "0.0.0" both parse identically.
        assert!(!is_strictly_newer("0.0.0", ""));
        assert!(!is_strictly_newer("", ""));
        // For force-apply, set current="" or "0.0.0" and ship anything strictly
        // greater (e.g., "0.0.1"). To downgrade, use the apply API directly,
        // not the transport-layer gate.
    }

    #[test]
    fn handle_release_skips_garbage_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("sigil-node");
        let fetch = ClosureFetcher(|_| Err("not used".into()));
        let out = handle_release_message(b"definitely not json", None, "0.0.1", &target, &fetch).unwrap();
        assert!(matches!(out, HandledRelease::NotAnAnnouncement { .. }));
        assert!(!target.exists());
    }

    #[test]
    fn handle_release_skips_invalid_signature() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("sigil-node");
        let (mut a, _bytes) = signed_announcement("0.0.2", b"some bytes");
        // tamper: bump version, leaving sig stale
        a.version = "9.9.9".into();
        let bytes = serde_json::to_vec(&a).unwrap();
        let fetch = ClosureFetcher(|_| Err("not used".into()));
        let out = handle_release_message(&bytes, None, "0.0.1", &target, &fetch).unwrap();
        assert!(matches!(out, HandledRelease::VerifyFailed { .. }));
        assert!(!target.exists());
    }

    #[test]
    fn handle_release_skips_not_newer() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("sigil-node");
        let (_a, bytes) = signed_announcement("0.0.1", b"some bytes");
        let fetch = ClosureFetcher(|_| Err("not used".into()));
        let out = handle_release_message(&bytes, None, "0.0.1", &target, &fetch).unwrap();
        assert!(matches!(out, HandledRelease::NotNewer { .. }));
        assert!(!target.exists());
    }

    #[test]
    fn handle_release_reports_fetch_failure() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("sigil-node");
        let (_a, bytes) = signed_announcement("0.0.2", b"some bytes");
        let fetch = ClosureFetcher(|_| Err("simulated DNS fail".into()));
        let out = handle_release_message(&bytes, None, "0.0.1", &target, &fetch).unwrap();
        match out {
            HandledRelease::FetchFailed { url, error } => {
                assert_eq!(url, "https://example.org/binary");
                assert!(error.contains("DNS"));
            }
            other => panic!("expected FetchFailed, got {:?}", other),
        }
        assert!(!target.exists());
    }

    #[test]
    fn handle_release_reports_hash_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("sigil-node");
        let real_binary = b"the bytes the producer signed".to_vec();
        let (_a, bytes) = signed_announcement("0.0.2", &real_binary);
        // Fetcher returns DIFFERENT bytes than what was announced.
        let fetch = ClosureFetcher(|_| Ok(b"different bytes from the network!".to_vec()));
        let out = handle_release_message(&bytes, None, "0.0.1", &target, &fetch).unwrap();
        assert!(matches!(out, HandledRelease::BinaryHashMismatch { .. }));
        assert!(!target.exists(), "must not touch target on hash mismatch");
    }

    #[test]
    fn handle_release_happy_path_applies() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("sigil-node");
        let binary = b"the real bytes".to_vec();
        let (_a, ann_bytes) = signed_announcement("0.0.2", &binary);
        let binary_clone = binary.clone();
        let fetch = ClosureFetcher(move |_| Ok(binary_clone.clone()));
        let out = handle_release_message(&ann_bytes, Some("test-peer"), "0.0.1", &target, &fetch).unwrap();
        assert!(out.applied(), "expected Applied, got {:?}", out);
        assert!(target.exists());
        let installed = std::fs::read(&target).unwrap();
        assert_eq!(installed, binary);
    }
}
