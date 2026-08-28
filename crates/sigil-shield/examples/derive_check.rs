// Verification-only scratch binary (NOT part of the shipped product) — prints exact
// reference values so a JS port (for the wallet UI's client-side Shield flow) can be
// checked byte-for-byte against the real Rust implementation. Safe to delete after use.
use sigil_shield::wallet::ShieldedAccount;
use winterfell::math::{fields::f64::BaseElement, StarkField};

fn main() {
    let seed = [0x5Eu8; 32];
    let acct = ShieldedAccount::from_seed(seed);
    println!("seed = 5e*32");
    println!("spend_key = {}", acct.spend_key().as_int());
    println!("blinding(0) = {}", acct.blinding(0).as_int());
    println!("blinding(1) = {}", acct.blinding(1).as_int());
    println!("public_key = {}", acct.public_key().as_int());

    // raw BLAKE3 KAT for the block-chaining/flags logic itself
    println!("blake3(\"abc\") = {}", blake3::hash(b"abc").to_hex());
    println!("blake3(73 zero bytes) = {}", blake3::hash(&[0u8; 73]).to_hex());
    let msg72: Vec<u8> = (0..72u32).map(|i| (i % 256) as u8).collect();
    println!("blake3(0..72 ramp) = {}", blake3::hash(&msg72).to_hex());

    // compress2 reference (Feistel MiMC node hash) for a couple of small inputs
    let a = BaseElement::new(1);
    let b = BaseElement::new(2);
    println!("compress2(1,2) = {}", sigil_shield::mimc::compress2(a, b).as_int());
    println!(
        "compress2(spend_key, blinding0) = {}",
        sigil_shield::mimc::compress2(acct.spend_key(), acct.blinding(0)).as_int()
    );
}
