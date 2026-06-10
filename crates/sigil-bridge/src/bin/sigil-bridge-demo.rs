//! sigil-bridge-demo — the "one better than Quillon" bridge end to end:
//! a PoW-verified on-chain deposit, the instant Lightning rail, the committed
//! supply root, and rejected over-mint / weak-PoW attacks.

use sha2::{Digest, Sha256};
use sigil_bridge::proof::{dsha256, header_meets_target, DEPOSIT_MAGIC};
use sigil_bridge::{process_deposit, process_ln_deposit, BridgeAsset, BridgeLedger, LnProof, SpvProof};

const EASY_NBITS: u32 = 0x207f_ffff;

/// Build a tx carrying the SIGIL deposit memo (amount + recipient are bound to
/// the proven bytes — see audit C9).
fn deposit_tx(amount: u128, recipient: [u8; 32]) -> Vec<u8> {
    let mut v = b"deposit:".to_vec();
    v.extend_from_slice(DEPOSIT_MAGIC);
    v.extend_from_slice(&amount.to_le_bytes());
    v.extend_from_slice(&recipient);
    v
}

fn mine(prev: [u8; 32], merkle_root: [u8; 32]) -> [u8; 80] {
    let mut h = [0u8; 80];
    h[0] = 1;
    h[4..36].copy_from_slice(&prev);
    h[36..68].copy_from_slice(&merkle_root);
    h[72..76].copy_from_slice(&EASY_NBITS.to_le_bytes());
    let mut n = 0u32;
    loop {
        h[76..80].copy_from_slice(&n.to_le_bytes());
        if header_meets_target(&h) {
            return h;
        }
        n += 1;
    }
}

fn spv_proof(tx: &[u8], confirmations: u32) -> SpvProof {
    let leaf = dsha256(tx);
    let mut headers = vec![mine([0u8; 32], leaf)];
    for _ in 1..confirmations {
        let prev = dsha256(headers.last().unwrap());
        headers.push(mine(prev, [7u8; 32]));
    }
    SpvProof { tx_bytes: tx.to_vec(), tx_hash: leaf, branch: vec![], tx_index: 0, headers }
}

fn sha256(d: &[u8]) -> [u8; 32] {
    let h = Sha256::digest(d);
    let mut o = [0u8; 32];
    o.copy_from_slice(&h);
    o
}

fn main() {
    println!("\n  sigil-bridge — proof-carrying, supply-committed (ONE BETTER than Quillon)\n");
    println!("  Quillon: 7-of-11 committee SIGNS \"we saw it\" → mint (trust signers, custodial)");
    println!("  SIGIL:   node VERIFIES proof-of-work / a paid invoice → mint, peg in a committed root\n");

    let mut ledger = BridgeLedger::new();

    // ── on-chain rail: a BTC deposit buried under 6 real-PoW headers ──
    let proof = spv_proof(&deposit_tx(100_000, [0x11; 32]), 6);
    match process_deposit(&mut ledger, BridgeAsset::Btc, &proof, None) {
        Ok(r) => println!("  ✓ on-chain: 6 PoW headers verified → minted {} {} · root {}",
            r.amount, r.asset.wrapped_symbol(), hex::encode(&r.supply_root[..8])),
        Err(e) => println!("  ✗ on-chain: {e}"),
    }

    // ── lightning rail: instant, gated on a SIGNED BOLT11 + the preimage ──
    // (C9: the LN deposit amount is now bound to the payee-signed invoice, not a
    // caller field. Constructing a signed invoice needs a secp key, so the rail
    // is exercised in the unit tests — `cargo test -p sigil-bridge ln::tests`.)
    println!("  ⚡ lightning (LNbits): LnProof now carries the SIGNED BOLT11 — its");
    println!("     secp256k1 signature is verified and the minted amount + payment_hash");
    println!("     are bound to the invoice (see ln::tests::signed_invoice_binds_amount).");

    // ── attacks the model structurally refuses ──
    print!("  over-mint wBTC with no backing:        ");
    match ledger.mint(BridgeAsset::Btc, 1) {
        Ok(_) => println!("MINTED — peg broken (BUG)"),
        Err(e) => println!("REJECTED ✓ — {e}"),
    }
    print!("  BTC deposit with only 2 PoW headers:   ");
    let weak = spv_proof(&deposit_tx(9_999, [0x22; 32]), 2); // need 6
    match process_deposit(&mut ledger, BridgeAsset::Btc, &weak, None) {
        Ok(_) => println!("minted (BUG)"),
        Err(e) => println!("REJECTED ✓ — {e}"),
    }

    println!("\n  peg sound across all assets: {}", if ledger.peg_ok() { "✓ minted ≤ locked everywhere" } else { "✗ FAULT" });
    println!("  → on-chain mints need REAL proof-of-work; lightning needs a paid-invoice preimage;");
    println!("    the peg lives in a block-committed root. Proof, not trust.\n");
}
