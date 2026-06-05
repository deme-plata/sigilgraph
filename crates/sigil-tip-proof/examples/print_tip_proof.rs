//! Print a canonical Blake3Fingerprint tip-proof to stdout — the fixture
//! the in-browser verifier (`verify-tip.html`, P4-E) and downstream
//! integration tests consume.
//!
//! Run with:
//! ```
//! fluxc run --package sigil-tip-proof --example print_tip_proof > tip-proof-sample.json
//! ```
//! Or via raw cargo:
//! ```
//! cargo run -p sigil-tip-proof --example print_tip_proof
//! ```
//!
//! The roots are deterministic placeholders so the JSON byte-for-byte
//! matches across runs — useful when committing the file into a static
//! dist-final/ deploy. Real producer tip-proofs (run_produce_block) compute
//! their roots from actual block state.

use sigil_state::StateRoots;
use sigil_tip_proof::TipProof;

fn main() {
    // Deterministic placeholder roots — visually distinguishable byte
    // patterns so an operator eyeballing the JSON can tell which field is
    // which without consulting the spec.
    let roots = StateRoots {
        wallet_state_root:   [0x11; 32],
        dex_state_root:      [0x22; 32],
        event_log_root:      [0x33; 32],
        contract_state_root: [0x44; 32],
    };
    let proof = TipProof::new_blake3(1, roots);

    // Sanity round-trip on stdout — verify must pass before we print.
    proof
        .verify(sigil_net::NETWORK_ID)
        .expect("self-produced fixture must verify before emit");

    let json = serde_json::to_string_pretty(&proof).expect("serialize");
    println!("{json}");
}
