//! Atomically swap a running binary for a new one. Borrowed verbatim from the
//! flux-arena-agent v0.1.5 pattern (proven in production on Viktor's Windows
//! box 2026-05-29): write `.new` next to current binary, rename current to
//! `.bak`, rename `.new` into place. On any failure, restore from `.bak`.
//!
//! This module is intentionally portable: Linux + Windows behave the same way
//! when renaming a running executable (Linux unlinks the inode but keeps the
//! file open; Windows allows rename-while-open since long ago). The new bytes
//! take effect on the next exec — the caller is responsible for arranging that
//! (typically: spawn the new binary, exit the old one).
//!
//! NOT done here: the actual `Command::new(target).spawn()` + exit. That stays
//! in `sigil-node`'s main, so this crate doesn't pull a process-management
//! dependency and can be unit-tested without forking processes.

use std::fs;
use std::path::{Path, PathBuf};

use crate::announcement::{ReleaseAnnouncement, UpdaterError};
use crate::verify::verify_binary_bytes;

/// Result of a successful apply. Caller can log the paths + spawn the
/// new binary from `target` to take it live.
#[derive(Debug, Clone)]
pub struct ApplyOutcome {
    pub target: PathBuf,
    pub backup: PathBuf,
    pub previous_existed: bool,
}

/// Write `new_bytes` into a `.new` sibling of `target`, then atomically
/// rename. Original (if present) is preserved at `target` + ".bak".
///
/// Verifies bytes against the announcement first — if the bytes don't match
/// the announced BLAKE3/size, this returns BEFORE touching `target`.
///
/// Idempotent w.r.t. stale `.new` / `.bak`: removes them if present.
pub fn apply_to_target(
    a: &ReleaseAnnouncement,
    new_bytes: &[u8],
    target: &Path,
) -> Result<ApplyOutcome, UpdaterError> {
    verify_binary_bytes(a, new_bytes)?;

    let target = target.to_path_buf();
    let new_path = sibling_with_suffix(&target, ".new");
    let bak_path = sibling_with_suffix(&target, ".bak");

    // Clean up debris from any prior failed run.
    let _ = fs::remove_file(&new_path);

    // 1. Write the .new file next to the target.
    write_atomic(&new_path, new_bytes)?;

    // 2. Snapshot the current target as .bak (if it exists).
    let previous_existed = target.exists();
    if previous_existed {
        // Remove an old .bak from a previous apply so the rename doesn't fail
        // on platforms (Windows) where rename-over-existing isn't atomic.
        let _ = fs::remove_file(&bak_path);
        if let Err(e) = fs::rename(&target, &bak_path) {
            // Roll back our .new file so we don't leave debris.
            let _ = fs::remove_file(&new_path);
            return Err(UpdaterError::Io(e));
        }
    }

    // 3. Move the .new into place. If this fails after we backed up, restore.
    if let Err(e) = fs::rename(&new_path, &target) {
        if previous_existed {
            let _ = fs::rename(&bak_path, &target);
        }
        return Err(UpdaterError::Io(e));
    }

    // 4. Mark the new file executable on Unix. No-op on Windows.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(&target) {
            let mut perms = meta.permissions();
            // 0o755: rwxr-xr-x
            perms.set_mode(perms.mode() | 0o755);
            let _ = fs::set_permissions(&target, perms);
        }
    }

    Ok(ApplyOutcome { target, backup: bak_path, previous_existed })
}

fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut out = path.as_os_str().to_owned();
    out.push(suffix);
    PathBuf::from(out)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), UpdaterError> {
    // Plain write — we already chose a sibling name, so partial writes only
    // pollute the .new file, never the target.
    fs::write(path, bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::announcement::ReleaseAnnouncement;
    use std::io::Read;

    fn fixture(payload: &[u8]) -> (ReleaseAnnouncement, Vec<u8>) {
        let (sk, pk) = flux_sqisign::keygen();
        let bytes = payload.to_vec();
        let mut a = ReleaseAnnouncement::unsigned(
            "sigil-node",
            "0.0.2",
            "https://example.org/sigil-node-v0.0.2",
            &bytes,
            b"{\"fake\": \"proof\"}".to_vec(),
            pk.clone(),
            1,
            1024,
            42,
            "apply-test",
        );
        a.sign(&sk).unwrap();
        (a, bytes)
    }

    #[test]
    fn apply_creates_target_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("sigil-node");
        let (a, bytes) = fixture(b"hello world");
        let outcome = apply_to_target(&a, &bytes, &target).expect("apply");
        assert!(!outcome.previous_existed);

        let mut buf = Vec::new();
        std::fs::File::open(&target).unwrap().read_to_end(&mut buf).unwrap();
        assert_eq!(buf, bytes);
        assert!(!outcome.backup.exists(), "no .bak when no previous binary");
    }

    #[test]
    fn apply_swaps_existing_target_to_bak() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("sigil-node");
        std::fs::write(&target, b"previous bytes").unwrap();
        let (a, bytes) = fixture(b"new bytes");
        let outcome = apply_to_target(&a, &bytes, &target).expect("apply");
        assert!(outcome.previous_existed);

        let mut current = Vec::new();
        std::fs::File::open(&target).unwrap().read_to_end(&mut current).unwrap();
        assert_eq!(current, bytes);

        let mut backup = Vec::new();
        std::fs::File::open(&outcome.backup).unwrap().read_to_end(&mut backup).unwrap();
        assert_eq!(backup, b"previous bytes");
    }

    #[test]
    fn apply_refuses_if_binary_doesnt_match_announcement() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("sigil-node");
        let (a, _bytes) = fixture(b"the bytes the announcement covers");
        let attack = b"these bytes are different";
        let err = apply_to_target(&a, attack, &target).unwrap_err();
        assert!(matches!(err,
            UpdaterError::BinaryHashMismatch { .. } | UpdaterError::BinarySizeMismatch { .. }));
        assert!(!target.exists(), "must not touch target on rejection");
    }

    #[test]
    fn apply_is_idempotent_re_runs_clean() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("sigil-node");
        let (a, bytes) = fixture(b"identical bytes");
        apply_to_target(&a, &bytes, &target).unwrap();
        // Second call: target already has correct bytes. apply rotates .bak
        // again (which is fine) and ends with target == bytes.
        apply_to_target(&a, &bytes, &target).unwrap();
        let mut current = Vec::new();
        std::fs::File::open(&target).unwrap().read_to_end(&mut current).unwrap();
        assert_eq!(current, bytes);
    }
}
