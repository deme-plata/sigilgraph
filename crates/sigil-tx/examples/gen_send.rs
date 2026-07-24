//! Dev helper: emit a deterministic wallet address + a signed Send tx JSON, for
//! end-to-end testing of the braid money API. NOT shipped — a test fixture.
//!
//!   cargo run -p sigil-tx --example gen_send -- <to_hex> <amount_base>
//! prints:  ADDR=<sender 64-hex>   (set this as SIGIL_PRODUCER_WALLET so coinbase funds it)
//!          JSON=<SignedTx json>   (POST to /v1/transactions)
use sigil_state::NATIVE;
use sigil_tx::{ed25519_sign_tx, SigilTx};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let to_hex = args.get(1).cloned().unwrap_or_else(|| "99".repeat(32));
    let amount: u128 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(100_000_000);

    // fixed seed → stable wallet across runs
    let sk = [7u8; 32];
    let signing = ed25519_dalek::SigningKey::from_bytes(&sk);
    let pk = signing.verifying_key().to_bytes();

    // recover the sender address from a throwaway sign (wallet_id derivation is internal)
    let probe = ed25519_sign_tx(
        SigilTx::Send { from: [0u8; 32], to: [0u8; 32], amount: 1, token: NATIVE, fee: 1 },
        &sk, &pk,
    );
    let addr = probe.from_pubkey;

    let mut to = [0u8; 32];
    for i in 0..32 {
        to[i] = u8::from_str_radix(&to_hex[i * 2..i * 2 + 2], 16).unwrap_or(0);
    }
    let tx = SigilTx::Send { from: addr, to, amount, token: NATIVE, fee: 1_000 };
    let signed = ed25519_sign_tx(tx, &sk, &pk);

    println!("ADDR={}", hex_of(&addr));
    println!("TO={}", hex_of(&to));
    println!("JSON={}", serde_json::to_string(&signed).unwrap());
}

fn hex_of(b: &[u8; 32]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
