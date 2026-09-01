//! Light-client block-store lifecycle: where it lives, resetting it on a network-id
//! change, healing a wedged store, and the light-boot size guards. Extracted from
//! main.rs. `use super::*` reaches the SIGIL_* env consts + network-id helpers.
use super::*;

/// Cross-platform persistent path for the light client's block store. Windows has no
/// /tmp or /dev/shm (the old hardcoded paths), so the store never persisted there →
/// re-sync from 0 every launch. Prefer a per-user dir; override with SIGIL_TOP_DB.
pub(crate) fn sigil_top_db_path() -> String {
    if let Ok(p) = std::env::var("SIGIL_TOP_DB") {
        if !p.trim().is_empty() { return p; }
    }
    let base = std::env::var("LOCALAPPDATA")
        .or_else(|_| std::env::var("HOME"))
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
    format!("{}/sigil-top-blocks.db", base.trim_end_matches(['/', '\\']))
}

/// PERMANENT self-heal: a store belonging to a DIFFERENT CHAIN is cleared automatically.
///
/// # The failure this ends
///
/// Every header carries `sigil_header::NETWORK_ID`, and `precheck` refuses any header
/// whose id is not the one this binary was built with. When the network was cut over from
/// `sigil-g0` to `sigil-g1`, every existing client woke up holding a database full of
/// perfectly valid `sigil-g0` blocks and started rejecting **its own block 0**:
///
/// ```text
///   ⛓ INTEGRITY BROKEN — h=0: precheck failed:
///      wrong network id: expected [..115,45,103,49..] ("sigil-g1"),
///                             got [..115,45,103,48..] ("sigil-g0")
///   ✗ SPINE BREAK — STUCK   0.0%   rate 0 blk/s   eta —
/// ```
///
/// That is a permanent deadlock, and nothing in the sync loop can escape it: retrying an
/// honest peer harder cannot make a g0 block legal on g1, so the progress bar sits at 0.0%
/// forever. The user's only recourse was to know, unprompted, to delete a database file
/// they were never told about.
///
/// # Why this one is unconditional, unlike [`heal_wedged_store_once`]
///
/// That heal is a ONE-SHOT keyed to a marker string, because "this store might be wedged"
/// is a guess and wiping a healthy archive is expensive. A network-id mismatch is not a
/// guess — it is a proof. Blocks from another chain can never become valid here no matter
/// how long we wait, so clearing them is provably the only recovery, and it should work
/// every time it is needed rather than once per hand-bumped constant. This makes reset
/// work out of the box, permanently, for every future genesis change.
///
/// The chain a store belongs to is recorded in a `.netid` sidecar written on first launch.
/// A store with no sidecar predates this check and cannot be attributed to any chain, so
/// it is cleared once and re-synced — the same cost as the cutover already imposed.
/// The chain id this binary was COMPILED for, as text.
///
/// The TUI used to hard-code `"sigil-g0"` as its placeholder, so after the cutover it
/// cheerfully displayed `sigil-g0` while refusing every g0 block for not being g1 — the
/// header contradicting the error two lines below it. A label that can disagree with the
/// binary is worse than no label.
pub(crate) fn build_network_id() -> String {
    String::from_utf8_lossy(&sigil_header::NETWORK_ID).trim().to_string()
}

pub(crate) fn reset_store_on_network_change(path: &str) {
    let current = String::from_utf8_lossy(&sigil_header::NETWORK_ID).trim().to_string();
    let netid_path = format!("{path}.netid");
    let reason = match std::fs::read_to_string(&netid_path) {
        // Same chain — leave the archive completely alone. This is the normal path and it
        // must stay cheap: one small file read per launch.
        Ok(prev) if prev.trim() == current => return,
        Ok(prev) => format!("network changed: store holds '{}', this build is '{current}'", prev.trim()),
        Err(_) => format!("store predates the network-id marker; cannot prove it is '{current}'"),
    };
    // A store that does not exist yet needs no wipe — just record the chain it will hold.
    let exists = std::path::Path::new(path).exists();
    if exists {
        eprintln!(
            "  ⛓ RESET: {reason} — clearing the local chain store and re-syncing from genesis."
        );
        let _ = std::fs::remove_dir_all(path);
        let _ = std::fs::remove_file(path);
        for sfx in ["-wal", "-shm", ".wal", ".shm"] {
            let _ = std::fs::remove_file(format!("{path}{sfx}"));
        }
        // The one-shot heal marker refers to a store that no longer exists. Dropping it
        // keeps the two mechanisms from disagreeing about what is on disk.
        let _ = std::fs::remove_file(format!("{path}.healver"));
    }
    let _ = std::fs::write(&netid_path, &current);
    boot_trace(&format!("network-id reset: {reason} (store existed: {exists}) — now on '{current}'"));
}

/// v7.0.7 ONE-TIME store heal, RE-ARMED 2026-08-24 (see below). Stores built by the
/// v7.0.3–7.0.5 sync FRONTIER-STALL bug wedge at a chunk boundary (a Fatal parent-linkage
/// break — the operator's h≈393,265 "SPINE BREAK — STUCK") and do NOT self-heal in place:
/// a later fetch-ordering fix only prevents NEW wedges, it can't repair an already-corrupt
/// on-disk chain. So on the FIRST launch of a build carrying a given `HEAL_MARKER`, delete
/// the store once; the sync then rebuilds it clean. A tiny `.healver` marker next to the
/// store records that THIS marker's heal ran, so later launches skip it (never a repeat
/// wipe for the same marker). A store that was already clean simply re-syncs once — a
/// testnet-acceptable one-time cost, and the ONLY way to make "press U and it just works"
/// true for every node already carrying a wedged store.
///
/// **RE-ARMED 2026-08-24** (bumped `v7.1.49` → `v7.1.75`): the marker is a ONE-SHOT per
/// exact string, not a permanent "this class of bug can never recur" guarantee — any store
/// that already consumed the v7.1.49 heal (i.e. every node that's launched at least once
/// since that release) is now IMMUNE to a repeat wipe even if it picks up a NEW wedge from
/// a DIFFERENT bug, because `heal_wedged_store_once` only compares the marker file's
/// content to the CURRENT constant. Confirmed this actually happened: live-reproduced on
/// Epsilon (2026-08-24) against a shared root-owned store that HAD already been healed
/// under the old marker — a background reconciliation pass hit hundreds of consecutive
/// `[store] rejected height-index fork overwrite` entries in the h≈1.6–2.0M range (two
/// completely different, uncorrelated hashes for the same height, `existing=` vs
/// `incoming=`), with the sync frontier frozen and the live fetch loop timing out on 71%
/// of requests — the exact "SPINE BREAK — STUCK" signature, just at a different height and
/// from whichever of the many uncommitted skeleton/backfill passes (see
/// `project_sigil_sync_index_vs_body_gap_2026_08_18` memory — VCATCH v1/v2, the
/// height-1-index-conflict finding, several NOT-yet-fully-landed attempts) wrote a
/// conflicting entry into that store at some point after v7.1.49 shipped. The store's own
/// conflict-rejection logic (`block_store.rs::put_blocks_batch` — correctly refusing to
/// let ANY later response silently overwrite an already-indexed height, which is real
/// anti-fork protection) cannot distinguish "a malicious/forked peer" from "our own local
/// store wrote something wrong, once, from an in-development code path" — so once a bad
/// entry lands, by ANY mechanism, ever, the spine wedges at that exact height PERMANENTLY
/// and no amount of retrying a healthy, honest peer can ever recover it. This is NOT a fix
/// for whatever wrote the bad entry in the first place (that remains open — the skeleton/
/// backfill code has had multiple actively-evolving, sometimes-uncommitted-and-broken
/// attempts across the v7.1.30–v7.1.74 range; see the memory file above for the specifics
/// not yet fully closed) — it is the same "clear the landmine" recovery this project has
/// already shipped once, re-armed so it fires again for stores that picked up NEW damage
/// since the last time.
pub(crate) fn heal_wedged_store_once(path: &str) {
    // RE-ARMED 2026-08-27 (v7.1.75 -> v7.2.5). The marker is a ONE-SHOT per exact string,
    // so every store that has launched since v7.1.75 is immune to a repeat wipe even when
    // it picks up NEW damage — which is exactly what happened. Live: an operator's client
    // sat with `verified 30,250` while `fetched-to` ran to 120,000, across several restarts
    // and four releases. Honest headers for `30,250..40,250` arrived and completed in ~2 s
    // every time, and the store refused to splice them: a poisoned local seam that no
    // amount of refetching can repair, because the bad block is OURS.
    //
    // The bounded in-run self-heal (`rollback_frontier(4096)`, 3 attempts behind a 45 s
    // watchdog) only covers damage within ~12k blocks of the frontier. This covers the case
    // where it is deeper, or where the wedge survives restarts. The store re-syncs clean —
    // at the rates these clients reach that is a couple of minutes.
    const HEAL_MARKER: &str = "frontier-stall-heal-v7.2.5";
    let marker = format!("{path}.healver");
    if std::fs::read_to_string(&marker).map(|s| s.trim() == HEAL_MARKER).unwrap_or(false) {
        return; // already healed under THIS marker on a prior launch — leave the store alone
    }
    // The store may be a flux-db DIRECTORY or a file, plus WAL/SHM sidecars — clear them all.
    let _ = std::fs::remove_dir_all(path);
    let _ = std::fs::remove_file(path);
    for sfx in ["-wal", "-shm", ".wal", ".shm"] {
        let _ = std::fs::remove_file(format!("{path}{sfx}"));
    }
    let _ = std::fs::write(&marker, HEAL_MARKER);
    boot_trace(&format!(
        "v7.1.75 one-time heal: cleared possibly-wedged store {path} (re-armed frontier-stall heal) — re-syncing from genesis"
    ));
}

pub(crate) fn light_boot_store_limit_bytes() -> u64 {
    if let Ok(raw) = std::env::var("SIGIL_TOP_BOOT_STORE_LIMIT_MB") {
        if let Ok(mb) = raw.trim().parse::<u64>() {
            return if mb == 0 { u64::MAX } else { mb.saturating_mul(1024 * 1024) };
        }
    }
    // v7.0.12: was 512 MiB (Windows) / 1.5 GiB (Linux) — FAR too low. A full-archive store is
    // several GB, so it ALWAYS exceeded the cap → the dashboard booted on a throwaway VOLATILE
    // store every launch → the sync NEVER persisted and re-synced from 0 on every update ("doesn't
    // resume"). 64 GiB opens the REAL persistent store for any realistic archive; the 20s
    // open-timeout still catches a genuinely-stuck open, and SIGIL_TOP_BOOT_STORE_LIMIT_MB=0
    // disables the cap entirely.
    64u64 * 1024 * 1024 * 1024
}

pub(crate) fn dir_size_capped(path: &str, cap: u64) -> std::io::Result<u64> {
    let root = std::path::Path::new(path);
    if !root.exists() {
        return Ok(0);
    }
    let meta = fs::metadata(root)?;
    if meta.is_file() {
        return Ok(meta.len());
    }
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total = total.saturating_add(meta.len());
                if total > cap {
                    return Ok(total);
                }
            }
        }
    }
    Ok(total)
}

pub(crate) fn oversized_store_for_light_boot(path: &str, want_sync: bool) -> Option<u64> {
    if want_sync || std::env::var("SIGIL_TOP_FORCE_STORE").is_ok() {
        return None;
    }
    let cap = light_boot_store_limit_bytes();
    if cap == u64::MAX {
        return None;
    }
    match dir_size_capped(path, cap) {
        Ok(bytes) if bytes > cap => Some(bytes),
        Ok(_) => None,
        Err(e) => {
            boot_trace(&format!("store size preflight failed for {path}: {e}"));
            None
        }
    }
}

pub(crate) fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut n = bytes as f64;
    let mut unit = 0usize;
    while n >= 1024.0 && unit + 1 < UNITS.len() {
        n /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{n:.1} {}", UNITS[unit])
    }
}
