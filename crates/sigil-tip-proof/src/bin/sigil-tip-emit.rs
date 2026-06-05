//! sigil-tip-emit — turn a live `{height, roots}` into a real, verifiable
//! BLAKE3 `TipProof` JSON (the exact artifact the browser lightweight node
//! verifies in ≤10ms).
//!
//! Reads one JSON object on stdin:
//!   {"height": 724354,
//!    "roots": {"wallet_state_root":[..32 u8..], "dex_state_root":[..],
//!              "event_log_root":[..], "contract_state_root":[..]}}
//! (this is exactly the shape the producer logs + the status feed's `.tip`
//! already publish, so a one-line `jq` feeds it).
//!
//! Writes the canonical `TipProof` JSON (version, network_id, height, roots,
//! flavor, signature) on stdout — `new_blake3` fingerprints the canonical
//! signing-bytes, so the emitted proof verifies against the live chain tip
//! and any tamper flips the BLAKE3 check to a hard reject.

use std::io::{Read, Write};

use sigil_state::StateRoots;
use sigil_tip_proof::TipProof;

#[derive(serde::Deserialize)]
struct Input {
    height: u64,
    roots: StateRoots,
}

fn main() {
    let mut buf = String::new();
    if std::io::stdin().read_to_string(&mut buf).is_err() || buf.trim().is_empty() {
        eprintln!("sigil-tip-emit: empty stdin (expected {{height, roots}})");
        std::process::exit(2);
    }
    let input: Input = match serde_json::from_str(&buf) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("sigil-tip-emit: bad input json: {e}");
            std::process::exit(2);
        }
    };
    let proof = TipProof::new_blake3(input.height, input.roots);
    let out = proof.encode_json();
    std::io::stdout().write_all(&out).expect("write proof");
}
