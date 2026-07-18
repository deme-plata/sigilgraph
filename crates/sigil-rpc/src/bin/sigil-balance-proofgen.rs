//! sigil-balance-proofgen — deterministic generator for the proof-carrying
//! `/balance?proof=1` JSON, for demoing + testing `sigil-balance-verify` without
//! binding a node's ports. It builds a SigilState, funds a demo wallet through the
//! real `commit_state_transition` chokepoint, and emits the SAME JSON shape the
//! `sigil-rpcd` `/balance` handler emits. This is the producer half of the
//! end-to-end fresh-node balance-proof demo.
//!
//! Usage:
//!   sigil-balance-proofgen [amount] [--tamper-amount] [--tamper-proof]
//!     amount            balance to fund the demo wallet (default 4200)
//!     --tamper-amount   emit a LYING balance (proof won't match) — verifier must reject
//!     --tamper-proof    corrupt a sibling hash — verifier must reject
//! Prints the balance JSON to stdout.

use sigil_state::{SigilState, StateTransition, StateMutation, NATIVE};

fn to_hex(b: &[u8]) -> String { hex::encode(b) }

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let amount: u128 = args.iter().skip(1)
        .find(|a| !a.starts_with("--"))
        .and_then(|s| s.parse().ok())
        .unwrap_or(4200);
    let tamper_amount = args.iter().any(|a| a == "--tamper-amount");
    let tamper_proof = args.iter().any(|a| a == "--tamper-proof");

    let wallet = [0x11u8; 32];

    // Fund the demo wallet through the real money chokepoint (same path the node
    // uses), so the SMT root + proof are exactly what a live node would produce.
    let mut state = SigilState::new();
    let t = StateTransition {
        at_height: 1,
        mutations: vec![StateMutation::SetBalance { wallet, token: NATIVE, amount }],
    };
    sigil_state::commit_state_transition(&mut state, &t, 1).expect("fund demo wallet");

    let (real_balance, root, mut proof) = state.prove_balance(&wallet, &NATIVE);

    if tamper_proof {
        proof.siblings[0][0] ^= 0x01; // corrupt one sibling
    }
    // The balance we CLAIM in the JSON (a liar sets it != real_balance).
    let claimed = if tamper_amount { real_balance + 1 } else { real_balance };

    let sibs: Vec<String> = proof.siblings.iter().map(|s| format!("\"{}\"", to_hex(s))).collect();
    println!(
        "{{\"ok\":true,\"balance\":{},\"wallet\":\"{}\",\"token\":\"{}\",\"height\":{},\"wallet_smt_root\":\"{}\",\"proof\":{{\"key_hash\":\"{}\",\"leaf\":\"{}\",\"siblings\":[{}]}}}}",
        claimed, to_hex(&wallet), to_hex(&NATIVE), 1u64,
        to_hex(&root), to_hex(&proof.key_hash), to_hex(&proof.leaf), sibs.join(",")
    );
}
