//! backfill_shielded_notes — recover the shielded coinbase notes minted before
//! delivery ciphertexts existed.
//!
//! THE PROBLEM (measured live, 2026-08-26): every shielded coinbase note minted
//! before `c6de572` carries no ciphertext — 15,047 of 15,047 on the live pool.
//! A wallet finds its notes by trial-decryption, so those notes are invisible to
//! it. The value is real and locked in the pool; it simply cannot be identified,
//! and therefore cannot be spent.
//!
//! WHY A SCAN IS THE ONLY WAY BACK. The commitment binds `(height, pk_shield,
//! amount)`. A wallet knows its own `pk_shield` — but not which heights paid it,
//! nor how much, and `amount` spans 2^58 so it cannot be searched. The one place
//! that pair was ever recorded is the `ShieldedCoinbase` mutation inside each
//! block body, where `pk_shield` and `amount` sit in the clear. So the facts are
//! recoverable; they just have to be read back out of the log.
//!
//! WHAT THIS EMITS. One row per legacy note: `(height, pk_shield, amount, cm)`.
//! A wallet filters to its own `pk_shield`, re-derives
//! `blinding = coinbase_blinding(height, pk_shield)`, and now holds exactly what
//! a ciphertext would have told it — the note is spendable again.
//!
//! PRIVACY: this publishes nothing new. Every field here is already in the clear
//! in a public block body, and `coinbase_blinding`'s own doc records that a
//! coinbase note is attributable at MINT time by design and private only at
//! SPEND time. It republishes what the chain already says, in a form a wallet
//! can actually use.
//!
//! READ-ONLY: opens the log with `ChainLog::replay_range`, which takes no writer
//! handle and rebuilds no index, so it is safe to run against a log the producer
//! is actively appending to.
//!
//! Usage: backfill_shielded_notes <chain_log_dir> [--pk <64-hex>] [--out <file>]
//!   --pk   only rows for this shield key (what one wallet needs)
//!   --out  write JSON here instead of stdout

use sigil_node::chain_log::ChainLog;
use sigil_state::StateMutation;
use std::path::PathBuf;

/// Blocks per bounded replay window. The log is read in slices so memory stays
/// flat regardless of chain length — the same reason `replay_range` exists.
const WINDOW: u64 = 25_000;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: backfill_shielded_notes <chain_log_dir> [--pk <64-hex>] [--out <file>]");
        std::process::exit(2);
    }
    let dir = PathBuf::from(&args[1]);
    let only_pk: Option<String> = args
        .iter()
        .position(|a| a == "--pk")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.trim().to_lowercase());
    let out_path: Option<PathBuf> = args
        .iter()
        .position(|a| a == "--out")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);

    eprintln!("scanning {} for ShieldedCoinbase mutations…", dir.display());
    if let Some(pk) = &only_pk {
        eprintln!("  filtering to pk_shield {pk}");
    }

    let mut rows: Vec<serde_json::Value> = Vec::new();
    let mut scanned = 0u64;
    let mut with_ct = 0u64;
    let mut from = 0u64;

    loop {
        let to = from.saturating_add(WINDOW - 1);
        let n = ChainLog::replay_range(&dir, from, to, |b| {
            scanned += 1;
            let height = b.header.height;
            for m in &b.transition.mutations {
                if let StateMutation::ShieldedCoinbase { pk_shield, amount, cm, ct } = m {
                    // A note that already carries a ciphertext needs no recovery —
                    // the wallet finds it the normal way. Counted, not emitted.
                    if ct.is_some() {
                        with_ct += 1;
                        continue;
                    }
                    let pk_hex = hex::encode(pk_shield);
                    if let Some(want) = &only_pk {
                        if &pk_hex != want {
                            continue;
                        }
                    }
                    rows.push(serde_json::json!({
                        "height": height,
                        "pk_shield": pk_hex,
                        "amount": amount.to_string(),
                        "cm": hex::encode(cm),
                    }));
                }
            }
        })
        .unwrap_or_else(|e| {
            eprintln!("replay_range({from}..={to}) failed: {e}");
            0
        });
        if n == 0 {
            break; // past the tip
        }
        from = to.saturating_add(1);
        if scanned % 250_000 < n {
            eprintln!("  … {scanned} blocks, {} legacy notes so far", rows.len());
        }
    }

    eprintln!(
        "\nscanned {scanned} blocks\n  legacy notes (no ciphertext): {}\n  already deliverable: {with_ct}",
        rows.len()
    );
    if rows.is_empty() {
        eprintln!("  nothing to recover — every shielded coinbase already carries a ciphertext.");
    }

    let doc = serde_json::json!({
        "ok": true,
        "note": "legacy shielded coinbase notes: blinding = coinbase_blinding(height, pk_shield)",
        "count": rows.len(),
        "notes": rows,
    });
    let body = serde_json::to_string(&doc).unwrap_or_else(|_| "{}".into());
    match out_path {
        Some(p) => {
            if let Err(e) = std::fs::write(&p, body) {
                eprintln!("write {}: {e}", p.display());
                std::process::exit(1);
            }
            eprintln!("wrote {}", p.display());
        }
        None => println!("{body}"),
    }
}
