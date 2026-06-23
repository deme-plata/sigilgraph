//! sigil-sign — the reference signer for sigil-rpcd's auth-gated flat routes.
//!
//! The v0.35 auth gate (`sigil_rpc::auth`) requires every money-mutating request
//! to carry an Ed25519 `sig` over a canonical, domain-separated, `req_nonce`-
//! guarded message, signed by the wallet whose funds move. This tool IS that
//! contract, executable: derive the wallet from a 32-byte seed (exactly as the
//! node does, `sigil_oauth::Keypair::from_seed`), sign the canonical message for
//! an `action` + ordered `fields` + `req_nonce`, and print the wallet + sig so
//! any client (shell scripts, the wallet, CI) can attach `from`/`sig`/`req_nonce`.
//!
//! Usage:
//!   sigil-sign <seed_hex32> <req_nonce> <action> [field ...]
//!
//! Example (a /swap by the seed-derived wallet):
//!   N=$(date +%s%3N)
//!   sigil-sign <seed> $N swap <from_hex> <pool_hex> AtoB 1000 1
//!   → {"wallet":"<hex>","req_nonce":<N>,"sig":"<128-hex>"}
//!
//! The field order MUST match the route handler's `authorize(..)` call:
//!   swap          : from_hex pool_hex dir(AtoB|BtoA) amount_in min_out
//!   add_liquidity : from_hex pool_hex amount_a amount_b
//!   deploy_token  : symbol supply to_hex
//!   credit        : operator_pool_hex pool_amount verifiers_csv
//!   mine          : miner_hex header pow_nonce

use sigil_oauth::Keypair;
use sigil_rpc::auth::auth_message;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        eprintln!("usage: sigil-sign <seed_hex32> <req_nonce> <action> [field ...]");
        std::process::exit(2);
    }
    let seed_bytes = match hex::decode(args[0].trim()) {
        Ok(b) if b.len() == 32 => b,
        _ => {
            eprintln!("error: seed must be exactly 32 bytes of hex (64 chars)");
            std::process::exit(2);
        }
    };
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&seed_bytes);

    let req_nonce: u64 = match args[1].parse() {
        Ok(n) => n,
        Err(_) => {
            eprintln!("error: req_nonce must be a u64 (use a ms timestamp, e.g. `date +%s%3N`)");
            std::process::exit(2);
        }
    };
    let action = args[2].as_str();
    let fields: Vec<&str> = args[3..].iter().map(|s| s.as_str()).collect();

    let kp = Keypair::from_seed(&seed);
    let sig = kp.sign(&auth_message(action, &fields, req_nonce));

    // JSON line so callers can pluck wallet/sig with any parser.
    println!(
        "{{\"wallet\":\"{}\",\"req_nonce\":{},\"sig\":\"{}\"}}",
        hex::encode(kp.pubkey()),
        req_nonce,
        hex::encode(sig)
    );
}
