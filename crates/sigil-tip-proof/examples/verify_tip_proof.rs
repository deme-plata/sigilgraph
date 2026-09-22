//! Verify a live `/v1/finality/tip-proof` answer from stdin — the light client's check.
//!
//!     curl -s http://127.0.0.1:18181/v1/finality/tip-proof \
//!       | fluxc run --profile release-fast -p sigil-tip-proof --example verify_tip_proof
//!
//! Exit 0 = the SQIsign L5 signature over (version || network_id || height || 4 roots)
//! verifies with the producer public key the node published; non-zero = it does not.
use std::io::Read;

fn main() {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw).expect("stdin");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("json");
    let tp = &v["tip_proof"];
    if tp.is_null() {
        eprintln!("no tip proof yet: {v}");
        std::process::exit(2);
    }
    let pk = hex::decode(tp["producer_pk_sqisign"].as_str().unwrap_or("")).expect("pk hex");
    let proof_bytes = serde_json::to_vec(&tp["proof"]).expect("proof json");
    let proof = sigil_tip_proof::TipProof::decode_json(&proof_bytes).expect("decode tip proof");
    match proof.verify_sqisign(sigil_tip_proof::NETWORK_ID_BYTES, &pk) {
        Ok(()) => {
            println!(
                "✓ SQIsign L5 tip proof VERIFIED: height={} flavor={:?} sig={}B pk={}B wallet_root={}…",
                proof.height, proof.flavor, proof.signature.len(), pk.len(),
                hex::encode(&proof.roots.wallet_state_root[..6])
            );
        }
        Err(e) => {
            println!("✗ tip proof did NOT verify: {e:?}");
            std::process::exit(1);
        }
    }
}
