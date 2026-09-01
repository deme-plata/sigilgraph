//! WireGuard peer-manifest persistence: the list of `WgPeer`s for an
//! interface, stored at `<db>/wg-peers/<iface>.json`.
//!
//! Extracted from `main.rs`. Writes are atomic (tmp + rename) so a crash
//! mid-write never leaves a truncated manifest that would fail to parse on
//! the next boot. A missing manifest is not an error — it means "no peers yet".

use anyhow::{Context, Result};

pub(crate) fn peers_manifest_path(db_path: &std::path::Path, iface: &str) -> std::path::PathBuf {
    db_path.join("wg-peers").join(format!("{iface}.json"))
}

/// Read the peer manifest for `iface`. Missing file → empty list (no error).
pub(crate) fn load_peers_manifest(
    db_path: &std::path::Path,
    iface: &str,
) -> Result<Vec<sigil_net_wg::WgPeer>> {
    let p = peers_manifest_path(db_path, iface);
    if !p.exists() {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
    let peers: Vec<sigil_net_wg::WgPeer> = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing JSON manifest at {}", p.display()))?;
    Ok(peers)
}

/// Persist a peer manifest atomically — write to `<file>.tmp`, then rename.
pub(crate) fn save_peers_manifest(
    db_path: &std::path::Path,
    iface: &str,
    peers: &[sigil_net_wg::WgPeer],
) -> Result<()> {
    let p = peers_manifest_path(db_path, iface);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let tmp = p.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(peers).context("serializing peer manifest")?;
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &p)
        .with_context(|| format!("rename {} -> {}", tmp.display(), p.display()))?;
    Ok(())
}

#[cfg(test)]
mod wg_manifest_tests {
    use super::{load_peers_manifest, peers_manifest_path, save_peers_manifest};

    fn scratch() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "sigil-wgman-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn path_layout_is_stable() {
        let p = peers_manifest_path(std::path::Path::new("/db"), "sigilwg0");
        assert!(p.ends_with("wg-peers/sigilwg0.json"));
    }

    #[test]
    fn missing_manifest_is_empty_not_error() {
        let d = scratch();
        // No file written → empty list, no error.
        let peers = load_peers_manifest(&d, "sigilwg0").expect("missing → Ok(empty)");
        assert!(peers.is_empty());
    }

    #[test]
    fn save_then_load_roundtrips_and_creates_file() {
        let d = scratch();
        save_peers_manifest(&d, "sigilwg0", &[]).expect("save empty");
        assert!(peers_manifest_path(&d, "sigilwg0").exists(), "file created");
        let back = load_peers_manifest(&d, "sigilwg0").expect("load back");
        assert!(back.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn corrupt_manifest_is_a_loud_error_not_a_silent_empty() {
        let d = scratch();
        let p = peers_manifest_path(&d, "sigilwg0");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"{ this is not valid json").unwrap();
        // A truncated/garbage manifest must error, never be read as "no peers".
        assert!(load_peers_manifest(&d, "sigilwg0").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
