//! snapshot.rs — durable chain persistence via flux-aether Reed-Solomon shards.
//!
//! Phase-0 `ChainTip` is in-memory (the flux-db substitute), so a restart used
//! to lose the chain. This makes it **can't-lose**: periodically snapshot the
//! blocks to RS-aether shards on disk; on boot, RS-reassemble + replay. You can
//! lose up to N-K shards (hosts) and still recover byte-identical — the exact
//! property proven by `flux-aether/bin/durability-proof`.
//!
//! Snapshot dir: `$SIGIL_DB_PATH/aether` (or `<data>/aether`). Layout:
//!   manifest        — "orig_len k parity n height"
//!   shard.0 .. n-1  — the erasure shards

use std::path::{Path, PathBuf};

use crate::block::Block;
use flux_aether::{rs_reassemble, rs_shard};

const K: usize = 16;
const PARITY: usize = 8; // tolerate losing any 8 of 24 shards

/// Where snapshots live. Uses a DEDICATED env — NOT `SIGIL_DB_PATH`, which the
/// node's net layer also consumes (setting it changes node startup behavior).
pub fn snapshot_dir() -> PathBuf {
    let base = std::env::var("SIGIL_SNAPSHOT_DIR").unwrap_or_else(|_| "/home/orobit/sigil-data/snap".into());
    Path::new(&base).join("aether")
}

/// Snapshot the whole chain to RS-aether shards (atomic-ish: tmp then rename).
pub fn save(blocks: &[Block], dir: &Path) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(blocks).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let (orig_len, shards) = rs_shard(&bytes, K, PARITY);
    std::fs::create_dir_all(dir)?;
    let height = blocks.last().map(|b| b.header.height + 1).unwrap_or(0);
    for (i, s) in shards.iter().enumerate() {
        let tmp = dir.join(format!("shard.{i}.tmp"));
        std::fs::write(&tmp, s)?;
        std::fs::rename(&tmp, dir.join(format!("shard.{i}")))?;
    }
    // manifest last → its presence means the snapshot is complete
    let manifest = format!("{orig_len} {K} {PARITY} {} {height}", shards.len());
    let tmp = dir.join("manifest.tmp");
    std::fs::write(&tmp, &manifest)?;
    std::fs::rename(&tmp, dir.join("manifest"))?;
    Ok(())
}

/// Load + RS-reassemble the chain from shards under `dir`. Tolerates missing
/// shards (lost hosts) as long as ≥ K survive. None ⇒ no/!complete snapshot.
pub fn load(dir: &Path) -> Option<Vec<Block>> {
    let manifest = std::fs::read_to_string(dir.join("manifest")).ok()?;
    let f: Vec<usize> = manifest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
    if f.len() < 4 {
        return None;
    }
    let (orig_len, k, parity, n) = (f[0], f[1], f[2], f[3]);
    let shards: Vec<Option<Vec<u8>>> =
        (0..n).map(|i| std::fs::read(dir.join(format!("shard.{i}"))).ok()).collect();
    let bytes = rs_reassemble(orig_len, k, parity, shards)?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load_roundtrip_losing_shards() {
        // a tiny fake chain (just headers matter for serde round-trip)
        let blocks = crate::block::__test_chain(40);
        let dir = std::env::temp_dir().join(format!("sigil-snap-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        save(&blocks, &dir).unwrap();
        // simulate losing 8 shards (the max) by deleting them
        for i in 0..PARITY {
            let _ = std::fs::remove_file(dir.join(format!("shard.{i}")));
        }
        let recovered = load(&dir).expect("recover from survivors");
        assert_eq!(recovered, blocks);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
