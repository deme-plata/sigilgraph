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

use serde::{Deserialize, Serialize};

use crate::block::Block;
use crate::chain::ChainTip;
use flux_aether::{rs_reassemble, rs_shard};
use sigil_state::SigilState;

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

// ═══════════════════════════════════════════════════════════════════════════
// v0.36.1 — producer STATE snapshot (the fast-boot path)
//
// The RS-shard code above persists raw block lists; this section persists the
// producer's ACCUMULATED STATE so boot doesn't have to deserialize+apply the
// entire 52 GB / ~21M-block chain.log (~35 min, compute-bound). A StateSnapshot
// is the exact in-RAM contents of [`ChainTip`]:
//   * `state`        — the accumulated `SigilState` at the snapshot tip (wallet/
//                      dex/contract/event accumulators + maps → the 4 roots),
//   * `blocks`       — the bounded recent-block RAM window (WINDOW = 8192), and
//   * `base_height`  — the window's base, so `height()`/`get()` stay correct.
// Those three fields ARE the ChainTip, so restore + `replay_from(snapshot_height
// +1..)` is state-identical to a full replay (release-gated by the
// `snapshot_boot_equals_full_replay` test below).
//
// Codec: rmp-serde (MessagePack) — the same codec store.rs settled on, because
// SIGIL's internally-tagged enums (`SigilEvent`/`SigilTx`, tag = "kind") encode
// under bincode but CANNOT decode (`deserialize_any`), and serde_json can't
// encode `SigilState`'s tuple map keys. Honest limit: rmp encodes raw `u128`
// only up to `u64::MAX` — native SIGIL is capped at 2.1e15 so it always fits;
// an exotic custom-token balance above u64::MAX would make `save_state` error
// (logged, boot falls back to full replay — never corrupts).
//
// File: `<dir>/state-snapshot.bin` = [32-byte BLAKE3(payload)][rmp payload],
// written atomically (tmp → fsync → rename). Corrupt/missing/version-mismatch
// → `load_state` returns None and boot falls back to the full-replay path.
// ═══════════════════════════════════════════════════════════════════════════

/// Bump when the StateSnapshot layout changes — old files then fail loudly
/// into the full-replay fallback instead of mis-decoding.
/// v2 (0.36.1): the on-disk file now carries an SQIsign (PQ, NIST L5) signature
/// over the BLAKE3 sum, so a tampered snapshot is rejected unless re-signed by
/// THIS node's key (v1 BLAKE3-only files version-mismatch → full-replay).
pub const STATE_SNAPSHOT_VERSION: u8 = 2;

const STATE_SNAPSHOT_FILE: &str = "state-snapshot.bin";
const STATE_SNAPSHOT_TMP: &str = "state-snapshot.tmp";

// ── v0.36.1 SQIsign snapshot authentication keys ──────────────────────────
// The node signs each snapshot with a post-quantum SQIsign (Level 5) key so a
// snapshot can't be silently swapped for a re-encoded one (BLAKE3 alone only
// catches accidental corruption — an attacker who rewrites the payload can
// recompute the sum). The keypair is generated on first save next to the
// snapshot; only a snapshot signed by OUR pk is trusted, else boot full-replays.
const SQ_SK_FILE: &str = "snapshot-sqisign.sk";
const SQ_PK_FILE: &str = "snapshot-sqisign.pk";

fn load_or_gen_sq_keys(dir: &Path) -> std::io::Result<(Vec<u8>, Vec<u8>)> {
    let (skp, pkp) = (dir.join(SQ_SK_FILE), dir.join(SQ_PK_FILE));
    if let (Ok(sk), Ok(pk)) = (std::fs::read(&skp), std::fs::read(&pkp)) {
        if !sk.is_empty() && !pk.is_empty() {
            return Ok((sk, pk));
        }
    }
    let (sk, pk) = flux_sqisign::keygen();
    std::fs::create_dir_all(dir)?;
    let sktmp = dir.join("snapshot-sqisign.sk.tmp");
    std::fs::write(&sktmp, &sk)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&sktmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&sktmp, &skp)?;
    std::fs::write(&pkp, &pk)?;
    Ok((sk, pk))
}

/// The node's snapshot verify pubkey (None until the first save generates it).
fn load_sq_pubkey(dir: &Path) -> Option<Vec<u8>> {
    std::fs::read(dir.join(SQ_PK_FILE)).ok().filter(|b| !b.is_empty())
}

/// Everything needed to reconstruct a [`ChainTip`] without replaying history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateSnapshot {
    /// Layout version byte — must equal [`STATE_SNAPSHOT_VERSION`].
    pub version: u8,
    /// Header height of the TIP block at capture (== `chain.height() - 1`).
    /// Boot resumes with `replay_from(dir, snapshot_height + 1, …)`.
    pub snapshot_height: u64,
    /// Height of the oldest block in `blocks` (the RAM window base).
    pub base_height: u64,
    /// The recent-block RAM window, heights `base_height ..= snapshot_height`.
    pub blocks: Vec<Block>,
    /// The accumulated state at `snapshot_height` (wallets, pools, contracts,
    /// the four root accumulators, master wallet, native supply).
    pub state: SigilState,
}

impl StateSnapshot {
    /// Capture the live chain's snapshot parts (clones — the write can then
    /// happen off the hot path). `None` on an empty chain (nothing to save).
    pub fn capture(chain: &ChainTip) -> Option<Self> {
        let (state, blocks, base_height) = chain.snapshot_parts();
        let tip = blocks.back()?;
        Some(Self {
            version: STATE_SNAPSHOT_VERSION,
            snapshot_height: tip.header.height,
            base_height,
            blocks: blocks.iter().cloned().collect(),
            state: state.clone(),
        })
    }

    /// 32-byte hash of the snapshot's tip block header — boot compares this
    /// against `chain_log.get(snapshot_height).hash()` so a snapshot from a
    /// different chain (or a truncated/rewritten log) is rejected pre-restore.
    pub fn tip_block_hash(&self) -> Option<[u8; 32]> {
        self.blocks.last().map(|b| b.hash())
    }

    /// Consume into a live [`ChainTip`]. Integrity MUST already be verified
    /// (BLAKE3 checksum in [`load_state`] + the caller's tip-hash-vs-log check).
    pub fn restore(self) -> ChainTip {
        ChainTip::from_parts(self.state, self.blocks.into_iter().collect(), self.base_height)
    }
}

/// Path of the state-snapshot file under the node's snapshot/chain-log dir.
pub fn state_snapshot_path(dir: &Path) -> PathBuf {
    dir.join(STATE_SNAPSHOT_FILE)
}

/// Atomically persist a [`StateSnapshot`]: serialize (rmp), prefix a BLAKE3
/// checksum, write `<dir>/state-snapshot.tmp`, fsync, rename to
/// `state-snapshot.bin`. Returns total bytes written.
pub fn save_state(snap: &StateSnapshot, dir: &Path) -> std::io::Result<u64> {
    use std::io::Write;
    let payload = rmp_serde::to_vec(snap)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("rmp encode: {e}")))?;
    let sum = blake3::hash(&payload);
    // v0.36.1: SQIsign (PQ L5) over the BLAKE3 sum — authenticates the snapshot.
    // Slow (~isogeny), but save runs OFF the producer hot path so it's fine.
    let (sk, pk) = load_or_gen_sq_keys(dir)?;
    let sig = flux_sqisign::sign(sum.as_bytes(), &sk, &pk)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("sqisign sign: {e}")))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(STATE_SNAPSHOT_TMP);
    let mut f = std::fs::File::create(&tmp)?;
    // file v2 = [32 BLAKE3 sum][u16 siglen][sig][u16 pklen][pk][rmp payload]
    f.write_all(sum.as_bytes())?;
    f.write_all(&(sig.len() as u16).to_be_bytes())?;
    f.write_all(&sig)?;
    f.write_all(&(pk.len() as u16).to_be_bytes())?;
    f.write_all(&pk)?;
    f.write_all(&payload)?;
    f.sync_all()?; // fsync BEFORE rename — the rename must never expose a torn file
    drop(f);
    std::fs::rename(&tmp, state_snapshot_path(dir))?;
    Ok((32 + 2 + sig.len() + 2 + pk.len() + payload.len()) as u64)
}

/// Load + verify the state snapshot. `None` ⇒ missing, torn, checksum-corrupt,
/// version-mismatched, or internally inconsistent — boot then falls back to
/// the full-replay path (never trusts a bad snapshot).
pub fn load_state(dir: &Path) -> Option<StateSnapshot> {
    let bytes = std::fs::read(state_snapshot_path(dir)).ok()?;
    // v2 layout: [32 sum][u16 siglen][sig][u16 pklen][pk][payload]
    if bytes.len() < 36 {
        return None;
    }
    let sum = &bytes[0..32];
    let siglen = u16::from_be_bytes([bytes[32], bytes[33]]) as usize;
    let sig_start = 34;
    let pklen_at = sig_start + siglen;
    if bytes.len() < pklen_at + 2 {
        return None;
    }
    let sig = &bytes[sig_start..pklen_at];
    let pklen = u16::from_be_bytes([bytes[pklen_at], bytes[pklen_at + 1]]) as usize;
    let pk_start = pklen_at + 2;
    if bytes.len() < pk_start + pklen {
        return None;
    }
    let pk = &bytes[pk_start..pk_start + pklen];
    let payload = &bytes[pk_start + pklen..];
    // 1) corruption: BLAKE3 sum over the payload
    if blake3::hash(payload).as_bytes() != sum {
        return None;
    }
    // 2) authentication (v0.36.1): the embedded signer pk MUST be THIS node's key
    //    (else an attacker re-signs a tampered payload with their own key), AND
    //    the SQIsign signature over the sum must verify. Any failure → None →
    //    boot falls back to the (source-of-truth) full chain-log replay.
    match load_sq_pubkey(dir) {
        Some(expected_pk) if expected_pk.as_slice() == pk => {}
        _ => return None,
    }
    match flux_sqisign::verify(sum, sig, pk) {
        Ok(true) => {}
        _ => return None,
    }
    let snap: StateSnapshot = rmp_serde::from_slice(payload).ok()?;
    if snap.version != STATE_SNAPSHOT_VERSION {
        return None;
    }
    // Internal-consistency gate: window must be non-empty + height-contiguous
    // with the declared snapshot/base heights.
    if snap.blocks.is_empty()
        || snap.blocks.last().map(|b| b.header.height) != Some(snap.snapshot_height)
        || snap.blocks.first().map(|b| b.header.height) != Some(snap.base_height)
        || snap.snapshot_height + 1 - snap.base_height != snap.blocks.len() as u64
    {
        return None;
    }
    Some(snap)
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

    /// v0.36.1 RELEASE GATE — snapshot boot must be STATE-IDENTICAL to a full
    /// replay. Builds a real 500-block chain (genesis + 499 producer blocks
    /// through the live `mint_next_block` path), snapshots at H=400, then
    /// boots a FRESH ChainTip via restore + `ChainLog::replay_from(401..)`
    /// and asserts the final tip hash, height, window base, and ALL FOUR
    /// state roots exactly equal the full-replay chain's.
    #[test]
    fn snapshot_boot_equals_full_replay() {
        use crate::chain_log::ChainLog;

        let dir = std::env::temp_dir()
            .join(format!("sigil-state-snap-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // ── build + log a real 500-block chain (heights 0..=499) ──
        let mut log = ChainLog::open(&dir).unwrap();
        let mut chain = ChainTip::new();
        let genesis = crate::build_genesis().expect("genesis builds");
        log.append(&genesis).unwrap();
        chain.apply(genesis).expect("genesis applies");
        let mut snap_height_saved: Option<u64> = None;
        let mut snap_bytes: u64 = 0;
        let mut snap_write_secs: f64 = 0.0;
        for _ in 1..500u64 {
            let b = crate::mint_next_block(&chain, vec![], &[]).expect("mint");
            log.append(&b).unwrap();
            chain.apply(b).expect("block applies");
            // snapshot at H=400 (tip header height 400, 401 blocks applied)
            if chain.height() == 401 {
                let snap = StateSnapshot::capture(&chain).expect("capture non-empty chain");
                assert_eq!(snap.snapshot_height, 400);
                let t0 = std::time::Instant::now();
                snap_bytes = save_state(&snap, &dir).expect("snapshot save");
                snap_write_secs = t0.elapsed().as_secs_f64();
                snap_height_saved = Some(snap.snapshot_height);
            }
        }
        assert_eq!(chain.height(), 500);
        assert_eq!(log.height(), 500);
        let live_tip = chain.parent_hash();
        let live_roots = chain.roots();

        // ── reference: full replay from block 0 (the old boot path) ──
        let mut full = ChainTip::new();
        let n_full = ChainLog::replay(&dir, |b| {
            full.apply(b).expect("full replay applies every block");
        })
        .unwrap();
        assert_eq!(n_full, 500);

        // ── snapshot boot: load → verify → restore → replay_from(401..) ──
        let snap = load_state(&dir).expect("snapshot loads + checksum verifies");
        assert_eq!(Some(snap.snapshot_height), snap_height_saved);
        // tip-hash continuity check, exactly as run_start performs it
        let log_tip_at_snap = log.get(snap.snapshot_height).expect("log has snapshot tip");
        assert_eq!(
            snap.tip_block_hash(),
            Some(log_tip_at_snap.hash()),
            "snapshot tip must match chain.log at the same height"
        );
        let from = snap.snapshot_height + 1;
        let mut restored = snap.restore();
        assert_eq!(restored.height(), 401);
        let n_tail = ChainLog::replay_from(&dir, from, |b| {
            restored.apply(b).expect("tail replay applies every block");
        })
        .map_err(|e| format!("{e}"))
        .unwrap();
        assert_eq!(n_tail, 99, "tail = blocks 401..=499");

        // ── THE GATE: snapshot boot ≡ full replay ≡ live chain ──
        for (name, c) in [("full-replay", &full), ("snapshot-boot", &restored)] {
            assert_eq!(c.height(), 500, "{name} height");
            assert_eq!(c.parent_hash(), live_tip, "{name} tip hash");
            assert_eq!(c.window_base(), chain.window_base(), "{name} window base");
            let r = c.roots();
            assert_eq!(r.wallet_state_root, live_roots.wallet_state_root, "{name} wallet root");
            assert_eq!(r.dex_state_root, live_roots.dex_state_root, "{name} dex root");
            assert_eq!(r.event_log_root, live_roots.event_log_root, "{name} event root");
            assert_eq!(r.contract_state_root, live_roots.contract_state_root, "{name} contract root");
        }
        // state-level spot check beyond the roots: demo wallet balance survives
        assert_eq!(
            restored.state_snapshot().balance_of(&crate::DEMO_WALLET, &[0u8; 32]),
            full.state_snapshot().balance_of(&crate::DEMO_WALLET, &[0u8; 32]),
        );

        let metrics = format!(
            "📸 snapshot file: {} bytes, write {:.3}s (401-block window @ H=400)",
            snap_bytes, snap_write_secs
        );
        eprintln!("{metrics}");
        // also persist the measurement so it's readable even when the libtest
        // harness captures passing-test output (CI/agent runs without --nocapture)
        let _ = std::fs::write(
            std::env::temp_dir().join("sigil-snapshot-test-metrics.txt"),
            &metrics,
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Corrupt snapshot file ⇒ `load_state` returns None (boot falls back to
    /// full replay instead of restoring garbage).
    #[test]
    fn corrupt_state_snapshot_is_rejected() {
        let dir = std::env::temp_dir()
            .join(format!("sigil-state-snap-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut chain = ChainTip::new();
        chain.apply(crate::build_genesis().unwrap()).unwrap();
        let snap = StateSnapshot::capture(&chain).unwrap();
        save_state(&snap, &dir).unwrap();
        assert!(load_state(&dir).is_some(), "pristine snapshot loads");

        // flip one payload byte → BLAKE3 mismatch → None
        let path = state_snapshot_path(&dir);
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();
        assert!(load_state(&dir).is_none(), "corrupt snapshot must be rejected");

        // missing file → None
        std::fs::remove_file(&path).unwrap();
        assert!(load_state(&dir).is_none(), "missing snapshot must be rejected");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v0.36.1 SQIsign: a snapshot whose signature is tampered, or whose signer
    /// pubkey is swapped for a foreign key, is REJECTED (boot → full replay).
    #[test]
    fn sqisign_tampered_or_foreign_snapshot_is_rejected() {
        let dir = std::env::temp_dir()
            .join(format!("sigil-snap-sqisign-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut chain = ChainTip::new();
        chain.apply(crate::build_genesis().unwrap()).unwrap();
        let snap = StateSnapshot::capture(&chain).unwrap();
        save_state(&snap, &dir).unwrap();
        assert!(load_state(&dir).is_some(), "pristine signed snapshot loads");

        // (a) flip a byte INSIDE the SQIsign signature → verify fails → None
        let path = state_snapshot_path(&dir);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[40] ^= 0xFF; // inside the sig region (32 sum + 2 len + sig[6])
        std::fs::write(&path, &bytes).unwrap();
        assert!(load_state(&dir).is_none(), "tampered signature must be rejected");

        // (b) re-save pristine, then swap the signer pubkey for a FOREIGN key →
        //     signer-mismatch → None (an attacker can't re-sign with their key)
        save_state(&snap, &dir).unwrap();
        assert!(load_state(&dir).is_some(), "re-saved snapshot loads");
        let (_, foreign_pk) = flux_sqisign::keygen();
        std::fs::write(dir.join(SQ_PK_FILE), &foreign_pk).unwrap();
        assert!(load_state(&dir).is_none(), "foreign signer pubkey must be rejected");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
