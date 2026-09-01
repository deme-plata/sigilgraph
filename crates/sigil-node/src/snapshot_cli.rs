//! `snapshot-create` / `snapshot-info` CLI subcommands. Extracted from main.rs.
//! `use super::*` reaches ChainTip / chain_log / the snapshot module / hex_full.
use super::*;
use anyhow::{anyhow, Context, Result};

pub(crate) fn run_snapshot_create() -> Result<()> {
    let snap_dir = snapshot::snapshot_dir();
    eprintln!("⏳ snapshot-create: full replay of {} (state must be rebuilt by applying blocks — this takes as long as a normal boot)",
        snap_dir.join("chain.log").display());
    let t0 = std::time::Instant::now();
    let mut chain = ChainTip::new();
    let mut applied: u64 = 0;
    let n = chain_log::ChainLog::replay(&snap_dir, |b| {
        if chain.apply(b).is_ok() { applied += 1; }
    })
    .map_err(|e| anyhow!("chain.log replay: {}", e))?;
    if n == 0 {
        return Err(anyhow!("no blocks in {} — nothing to snapshot", snap_dir.join("chain.log").display()));
    }
    if applied != n {
        eprintln!("⚠ replay read {} blocks but applied only {} — snapshot captures state at H={}",
            n, applied, chain.height().saturating_sub(1));
    }
    let replay_secs = t0.elapsed().as_secs_f64();
    let snap = snapshot::StateSnapshot::capture(&chain)
        .ok_or_else(|| anyhow!("chain is empty after replay — nothing to snapshot"))?;
    let t1 = std::time::Instant::now();
    let bytes = snapshot::save_state(&snap, &snap_dir).context("writing state snapshot")?;
    println!("📸 state snapshot written: {}", snapshot::state_snapshot_path(&snap_dir).display());
    println!("   snapshot_height: {}", snap.snapshot_height);
    println!("   window:          [{}..={}] ({} blocks)", snap.base_height, snap.snapshot_height, snap.blocks.len());
    println!("   size:            {} bytes", bytes);
    println!("   replay:          {:.1}s ({} blocks) · write: {:.2}s", replay_secs, n, t1.elapsed().as_secs_f64());
    Ok(())
}

/// `sigil-node snapshot-info` — print height / size / checksum status of the
/// current state-snapshot file. Read-only, instant.
pub(crate) fn run_snapshot_info() -> Result<()> {
    let snap_dir = snapshot::snapshot_dir();
    let path = snapshot::state_snapshot_path(&snap_dir);
    println!("snapshot file: {}", path.display());
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(_) => {
            println!("status:        MISSING (boot will full-replay; create via `sigil-node snapshot-create`)");
            return Ok(());
        }
    };
    println!("size:          {} bytes", meta.len());
    match snapshot::load_state(&snap_dir) {
        Some(s) => {
            println!("checksum:      OK (BLAKE3 verified)");
            println!("version:       {}", s.version);
            println!("height:        {}", s.snapshot_height);
            println!("window:        [{}..={}] ({} blocks)", s.base_height, s.snapshot_height, s.blocks.len());
            if let Some(h) = s.tip_block_hash() {
                println!("tip hash:      {}", hex_full(&h));
            }
        }
        None => {
            println!("checksum:      FAILED (corrupt, torn, or version-mismatched — boot will full-replay)");
        }
    }
    Ok(())
}
