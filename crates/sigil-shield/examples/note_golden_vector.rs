//! Emit a golden vector for the browser-side shielded-note scanner.
//!
//! The wallet must open envelopes produced by THIS Rust code. Re-implementing
//! X25519 + BLAKE3-derive_key + ChaCha20-Poly1305 in JS and assuming it matches
//! is exactly the kind of "looks right" crypto that silently reports a wrong
//! balance — or worse, reports zero forever. So the JS is checked against a real
//! sealed note, not against a spec reading.
//!
//! Run: cargo run --release -p sigil-shield --example note_golden_vector
fn main() {
    let seed = [0x99u8; 32];
    let height: u64 = 2;
    let amount: u128 = 201_881_165;

    let acct = sigil_shield::wallet::ShieldedAccount::from_seed(seed);
    let pk_field = acct.public_key();
    let pk_shield = sigil_shield::note_v1::to_wire(pk_field);
    let enc = sigil_shield::note_cipher::enc_identity_from_seed(&seed);

    let blinding = sigil_shield::note_v1::coinbase_blinding(height, pk_field);
    let pt = sigil_shield::note_cipher::NotePlaintext { value: amount as u64, blinding };
    let addr = sigil_shield::note_cipher::ShieldedAddress::new(pk_field, &enc.public_hex());
    let ct = sigil_shield::note_cipher::seal_note(&pt, &addr).expect("seal");
    let cm = sigil_shield::note_v1::coinbase_commitment_wire(height, &pk_shield, amount).expect("cm");

    println!("{}", serde_json::json!({
        "seed_hex":      hex::encode(seed),
        "pk_enc_hex":    enc.public_hex(),
        "pk_shield_hex": hex::encode(pk_shield),
        "height":        height,
        "expect_value":  amount as u64,
        "expect_blinding_str": blinding.as_int().to_string(),
        "leaf_cm_hex":   hex::encode(cm),
        "ciphertext":    ct.0,
    }));
}
