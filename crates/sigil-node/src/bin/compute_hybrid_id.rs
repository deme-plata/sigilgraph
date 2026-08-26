//! One-off: given an Ed25519 seed (hex) and a SQIsign pubkey (hex), print the
//! resulting hybrid producer id (BLAKE3(sqisign_pk || ed25519_pk)) — MUST
//! match producer_signing::hybrid_producer_id exactly, since this is only
//! used to pre-compute SIGIL_TRUSTED_PRODUCER_ID_HEX for deployment, not
//! read by the live node itself.
use ed25519_dalek::SigningKey;

fn hex_decode(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}
fn hex_encode(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let ed_seed_hex = &args[1];
    let sqisign_pk_hex = &args[2];

    let seed_bytes = hex_decode(ed_seed_hex);
    let seed: [u8; 32] = seed_bytes.try_into().expect("seed must be 32 bytes");
    let ed_pk = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
    let sqisign_pk = hex_decode(sqisign_pk_hex);

    let mut h = blake3::Hasher::new();
    h.update(&sqisign_pk);
    h.update(&ed_pk);
    let id = h.finalize();

    println!("ED25519_PK_HEX={}", hex_encode(&ed_pk));
    println!("SIGIL_TRUSTED_PRODUCER_ID_HEX={}", hex_encode(id.as_bytes()));
}
