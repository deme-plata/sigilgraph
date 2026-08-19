//! v7.1.35+ (grogu-sync-perf, 2026-08-19, operator-approved): one-shot tool to mint a FRESH
//! signed anchor and a NEW trust-root keypair, replacing the lost/stale one (old key_id
//! d6214c8ddc0fca2b — private half confirmed missing everywhere on Epsilon; only the pubkey
//! survives at dist-fluxapp/sigil-anchor-key.json). Prints everything needed to publish:
//! the new key_id + pubkey (for sigil-anchor-key.json + SIGIL_ANCHOR_PK_HEX), the private
//! key (operator custody ONLY — never published), and the ready-to-publish anchor TXT line.
//!
//! Usage: mint_anchor <height> <hash_hex64> <wallet_root_hex64> <dex_root_hex64> \
//!                     <event_root_hex64> <contract_root_hex64>
//! (all from a live tip snapshot, e.g. dist-fluxapp/sigil-status.json's "tip" object)

use base64::Engine as _;

fn parse_hex32(s: &str, label: &str) -> [u8; 32] {
    let bytes = hex::decode(s).unwrap_or_else(|e| panic!("{label}: bad hex: {e}"));
    <[u8; 32]>::try_from(bytes.as_slice()).unwrap_or_else(|_| panic!("{label}: need exactly 32 bytes"))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 7 {
        eprintln!("usage: mint_anchor <height> <hash_hex64> <wallet_root> <dex_root> <event_root> <contract_root>");
        std::process::exit(2);
    }
    let height: u64 = args[1].parse().expect("height must be u64");
    let block_hash = parse_hex32(&args[2], "block_hash");
    let wallet_root = parse_hex32(&args[3], "wallet_state_root");
    let dex_root = parse_hex32(&args[4], "dex_state_root");
    let event_root = parse_hex32(&args[5], "event_log_root");
    let contract_root = parse_hex32(&args[6], "contract_state_root");

    let roots = sigil_dns_anchor::roots_digest(&wallet_root, &dex_root, &event_root, &contract_root);

    let epoch = std::env::var("MINT_EPOCH")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock before epoch")
                .as_secs()
        });

    // Fresh SQIsign L5 trust-root keypair — the old one's private half is confirmed lost.
    let (sk, pk) = flux_sqisign::keygen();
    // key_id is an opaque 16-hex-char label (verified structurally only, len==16) — derive
    // it deterministically from the new pubkey so it's reproducible + collision-resistant,
    // matching the convention of the old key_id (also 16 hex chars).
    let key_id = hex::encode(blake3::hash(&pk).as_bytes())[..16].to_string();

    let msg = sigil_dns_anchor::anchor_signing_bytes(&block_hash, &roots, height, epoch);
    let sig = flux_sqisign::sign(&msg, &sk, &pk).expect("sign");
    let sig_b64 = base64::prelude::BASE64_STANDARD.encode(&sig);

    let txt = sigil_dns_anchor::encode_tip_signed(height, &block_hash, &roots, epoch, &sig_b64, &key_id);

    // Self-check before printing anything — never hand over an anchor that doesn't verify.
    let decoded = sigil_dns_anchor::decode(&txt).expect("self-encoded anchor must decode");
    let ok = sigil_dns_anchor::verify_signed_anchor(&decoded, &pk).expect("verify_signed_anchor");
    assert!(ok, "SELF-CHECK FAILED: freshly signed anchor does not verify against its own pubkey");

    // The private key NEVER goes to stdout/println — it's written straight to a file
    // (0600) so it never lands in a terminal scrollback, log capture, or agent transcript.
    // Only public material (pubkey, key_id, the signed anchor) is safe to print/publish.
    let sk_path = std::env::var("MINT_SK_OUT").unwrap_or_else(|_| "anchor-sk.hex".to_string());
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true).create(true).truncate(true).mode(0o600)
            .open(&sk_path).expect("open sk output file");
        writeln!(f, "{}", hex::encode(&sk)).expect("write sk");
    }

    println!("=== SELF-CHECK: PASSED (this anchor verifies against the new pubkey) ===");
    println!();
    println!("NEW key_id:        {key_id}");
    println!("NEW pubkey (hex):  {}", hex::encode(&pk));
    println!("NEW privkey: written to {sk_path} (mode 0600) — operator custody ONLY, never publish, never printed");
    println!();
    println!("Anchor TXT (publish to sigil-tip-anchor.txt, both dist-fluxapp and legacy roots):");
    println!("{txt}");
    println!();
    println!("sigil-anchor-key.json (public only, safe to publish):");
    println!(
        "{{\n  \"producer_pk_hex\": \"{}\",\n  \"key_id\": \"{key_id}\",\n  \"sig_scheme\": \"SqiSign5\",\n  \"use\": \"_sigil-tip DNS anchor verification\"\n}}",
        hex::encode(&pk)
    );
    println!();
    println!("Set on any client wanting the fast path: SIGIL_ANCHOR_PK_HEX={}", hex::encode(&pk));
    println!("fetch.rs's EXPECTED_KEY_ID constant must be updated to: {key_id}");
}
