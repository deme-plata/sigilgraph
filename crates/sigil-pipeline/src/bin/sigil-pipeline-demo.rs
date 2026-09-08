//! sigil-pipeline-demo — public Bitcoin → private SIGIL → public Ethereum, offline, end to end.
//!
//! Prints the attestation chain as JSON. Every step runs the production code path:
//! the SPV verifier, the v5 hiding STARK (prover + consensus verifier), the note cipher,
//! and the byte-exact wSIGIL3 mint calldata. Nothing is sent anywhere.

use sigil_bridge::BridgeLedger;
use sigil_pipeline::btc_in::{decode_header_hex, display_hash, mainnet_pow_limit, synthetic, BtcPolicy, BITCOIN_GENESIS_HASH_DISPLAY, BITCOIN_GENESIS_HEADER_HEX};
use sigil_pipeline::eth_out::{parse_eth_address, POLYGON_CHAIN_ID, WSIGIL3_POLYGON};
use sigil_pipeline::private_mid::{shield_pk_wire, ExitIntent, Rate};
use sigil_pipeline::run_offline;
use sigil_shield::note_cipher::enc_identity_from_seed;
use sigil_shield::wallet::ShieldedAccount;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let eth_dest = args.get(1).map(String::as_str).unwrap_or("0xd7cab8075188df9a50dc494e9bb827f96df93936");
    let sats: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(100_000);
    let exit: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(sats * 6 / 10);
    let eth_dest = parse_eth_address(eth_dest).expect("arg 1: Ethereum address");

    // A real-world anchor first: the mainnet genesis header is genuine PoW under the floor.
    let g = decode_header_hex(BITCOIN_GENESIS_HEADER_HEX).unwrap();
    assert_eq!(display_hash(sigil_bridge::dsha256(&g)), BITCOIN_GENESIS_HASH_DISPLAY);
    let _ = mainnet_pow_limit();
    eprintln!("✓ bitcoin genesis header re-hashes to {BITCOIN_GENESIS_HASH_DISPLAY}");

    let depositor = ShieldedAccount::from_seed([11u8; 32]);
    let vault = ShieldedAccount::from_seed([99u8; 32]);
    let vault_enc = enc_identity_from_seed(&[99u8; 32]);

    eprintln!("⛏ mining a synthetic 6-block Bitcoin chain around a {sats}-sat deposit naming shield pk {}…", hex::encode(shield_pk_wire(&depositor)));
    let proof = synthetic::deposit_proof(sats, shield_pk_wire(&depositor), 6);

    eprintln!("🔒 proving the private exit ({exit} glyphs to the vault, fee 1) — real v5 STARK, ~seconds…");
    let t = std::time::Instant::now();
    let att = run_offline(
        &mut BridgeLedger::new(),
        &proof,
        &BtcPolicy::synthetic(),
        Rate::UNIT,
        &depositor,
        &vault,
        &vault_enc,
        exit,
        1,
        ExitIntent { chain_id: POLYGON_CHAIN_ID, eth_dest },
        WSIGIL3_POLYGON,
    )
    .expect("pipeline");
    eprintln!("✓ verified through note_v1::verify_spend_wire in {:.2?}; attestation chain ok = {}", t.elapsed(), att.verify_chain());
    println!("{}", att.to_json_pretty());
}
