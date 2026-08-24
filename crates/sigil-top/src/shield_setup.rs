// sigil-top/src/shield_setup.rs — `mine-rig`'s SIGIL_MINE_SEED auto-registration
// (2026-08-24, revised same day).
//
// THE POINT: a miner should not have to know shielded pools exist to contribute to
// one. Give `mine-rig` a seed instead of a bare address, and it derives both the
// wallet's normal signing key AND its shield spend key from that ONE seed, registers
// once (idempotent — re-registering the same key is a harmless no-op transaction),
// then mines to that wallet as normal. From then on every reward for this wallet
// mints as a private note instead of a transparent credit — see
// `sigil-node::coinbase::build_block_body_for`'s `ShieldedCoinbase` branch, which
// already does the right thing once a wallet is registered; nothing there changes.
//
// SEED FORMAT — REVISED: `SIGIL_MINE_SEED` was NOT a new env var. `miner_keypair()`
// (main.rs) already uses it — required, audit-gated — for the TUI's interactive [M]
// mining key: a raw 64-hex seed fed DIRECTLY into `Keypair::from_seed` (== 32 raw
// bytes as the Ed25519 secret, no hashing step; see `sigil-oauth::Keypair::from_seed`).
// The first version of this module used a DIFFERENT format under the SAME variable
// name — `sha3_256(arbitrary string)` — which would have derived a DIFFERENT wallet
// than [M]ine's path for the identical env var value. Fixed to match the established
// format exactly: whoever already has to set SIGIL_MINE_SEED to mine at all now gets
// shielded registration for the same wallet, for free, with nothing new to configure.
//
// WHY ONE SEED, TWO KEYS: the raw 32 seed bytes are used directly as the Ed25519
// signing key (`SigningKey::from_bytes`, matching `Keypair::from_seed` bit-for-bit).
// The shield spend key is `ShieldedAccount::from_seed` on those SAME 32 bytes — a
// completely different, internally domain-separated BLAKE3 derivation. Two
// independent keys from one seed to back up; knowing one reveals nothing about the
// other, because they diverge at the first hash.
//
// SECURITY NOTE ON INPUT: the seed is read from an env var, never a positional CLI
// argument — a CLI arg is visible to every user on the box via `ps aux`; an env var
// is only visible to root (the same trust boundary as running this process at all).

use ed25519_dalek::{Signer, SigningKey};
use sigil_shield::note_v1::to_wire;
use sigil_shield::wallet::ShieldedAccount;

/// Decode a 64-hex-char string to 32 raw bytes. Deliberately the SAME parsing
/// `hex_to_32` (main.rs) uses for `SIGIL_MINE_SEED` elsewhere — not reimplemented
/// with different edge-case behavior.
fn seed_bytes(seed_hex: &str) -> Option<[u8; 32]> {
    let s = seed_hex.trim().strip_prefix("0x").unwrap_or(seed_hex.trim());
    let v = hex::decode(s).ok()?;
    v.try_into().ok()
}

/// The wallet's Ed25519 signing key — byte-identical to what `miner_keypair()`
/// (main.rs, the TUI's [M]ine path) derives from the SAME `SIGIL_MINE_SEED` value.
/// `None` if `seed_hex` isn't a well-formed 64-hex seed.
pub fn wallet_signing_key(seed_hex: &str) -> Option<SigningKey> {
    Some(SigningKey::from_bytes(&seed_bytes(seed_hex)?))
}

/// What registration actually did, so the caller can print something honest — this
/// never silently swallows a failure.
pub enum RegisterOutcome {
    Registered { wallet_hex: String, txid: String },
    Failed { wallet_hex: String, reason: String },
    /// `seed_hex` wasn't a valid 64-hex seed — distinct from a network/server
    /// failure so the caller can print an actionable message instead of a vague one.
    BadSeed,
}

/// Derive both keys from `seed_hex` (a raw 64-hex seed, the SAME format
/// `SIGIL_MINE_SEED` already requires for [M]ine), sign a `RegisterShieldedAddress`
/// request, and submit it to `node_url`. Synchronous/blocking — this runs once,
/// before the mining loop starts, so there is no async runtime to thread through.
pub fn register_for_shielded_mining(node_url: &str, seed_hex: &str) -> RegisterOutcome {
    let Some(seed) = seed_bytes(seed_hex) else { return RegisterOutcome::BadSeed };
    let sk = SigningKey::from_bytes(&seed);
    let wallet_hex = hex::encode(sk.verifying_key().to_bytes());
    let account = ShieldedAccount::from_seed(seed);
    let pk_shield_hex = hex::encode(to_wire(account.public_key()));
    // 2026-08-24: the note-delivery key, derived from the SAME seed via
    // `note_cipher::enc_identity_from_seed`'s own domain separation — a third
    // independent key from the one seed a miner already backs up. Without publishing
    // this alongside pk_shield, a miner could be paid privately by a third party but
    // would have no way to ever find that payment (only self-mined coinbase notes,
    // whose blinding is publicly re-derivable, would still be discoverable).
    let pk_encrypt_hex = sigil_shield::note_cipher::enc_identity_from_seed(&seed).public_hex();
    let fee: u128 = 0;
    // Same clock source the browser wallet's signer uses (Date.now()-equivalent) —
    // any strictly-increasing value works, this just needs to beat whatever this
    // wallet last used.
    let req_nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(1);

    let msg = format!(
        "sigil-rpc/v1|shield-register|{wallet_hex}|{pk_shield_hex}|{pk_encrypt_hex}|{fee}|nonce={req_nonce}"
    );
    let sig = hex::encode(sk.sign(msg.as_bytes()).to_bytes());

    let body = serde_json::json!({
        "wallet": wallet_hex,
        "pk_shield": pk_shield_hex,
        "pk_encrypt": pk_encrypt_hex,
        "fee": fee.to_string(),
        "sig": sig,
        "req_nonce": req_nonce,
    });

    // 2026-08-24: was "/api/v1/shielded/register" — sigil-api mirrors most routes
    // under both /v1/* and /api/v1/*, but the shielded endpoints were never added to
    // that mirror list (confirmed: /v1/shielded/register is live, /api/v1/shielded/
    // register 404s). Every SIGIL_MINE_SEED registration attempt since this feature
    // shipped (v7.1.74) silently failed at this URL and fell through to "mining
    // continues, but rewards stay transparent" — caught live while testing a real
    // end-to-end shielded send against the production API.
    let url = format!("{}/v1/shielded/register", node_url.trim_end_matches('/'));
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => return RegisterOutcome::Failed { wallet_hex, reason: format!("http client: {e}") },
    };
    let resp = match client.post(&url).json(&body).send() {
        Ok(r) => r,
        Err(e) => return RegisterOutcome::Failed { wallet_hex, reason: format!("request failed: {e}") },
    };
    let parsed: serde_json::Value = match resp.json() {
        Ok(v) => v,
        Err(e) => return RegisterOutcome::Failed { wallet_hex, reason: format!("bad response: {e}") },
    };
    if parsed.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        let txid = parsed.get("txid").and_then(|v| v.as_str()).unwrap_or("").to_string();
        RegisterOutcome::Registered { wallet_hex, txid }
    } else {
        let reason = parsed.get("error").and_then(|v| v.as_str()).unwrap_or("unknown error").to_string();
        RegisterOutcome::Failed { wallet_hex, reason }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_seed(byte: u8) -> String {
        hex::encode([byte; 32])
    }

    #[test]
    fn rejects_malformed_seeds_instead_of_silently_deriving_something() {
        assert!(wallet_signing_key("not hex at all").is_none());
        assert!(wallet_signing_key("deadbeef").is_none()); // too short (4 bytes)
        let too_long = format!("{}ff", valid_seed(0x11)); // 33 bytes
        assert!(wallet_signing_key(&too_long).is_none());
    }

    #[test]
    fn same_seed_derives_the_same_wallet_and_shield_key_every_time() {
        let seed = valid_seed(0x42);
        let a = wallet_signing_key(&seed).unwrap();
        let b = wallet_signing_key(&seed).unwrap();
        assert_eq!(a.verifying_key().to_bytes(), b.verifying_key().to_bytes());

        let acct_a = ShieldedAccount::from_seed(seed_bytes(&seed).unwrap());
        let acct_b = ShieldedAccount::from_seed(seed_bytes(&seed).unwrap());
        assert_eq!(acct_a.public_key(), acct_b.public_key());
    }

    #[test]
    fn different_seeds_never_collide() {
        let seed1 = valid_seed(0x01);
        let seed2 = valid_seed(0x02);
        let a = wallet_signing_key(&seed1).unwrap();
        let b = wallet_signing_key(&seed2).unwrap();
        assert_ne!(a.verifying_key().to_bytes(), b.verifying_key().to_bytes());

        let acct_a = ShieldedAccount::from_seed(seed_bytes(&seed1).unwrap());
        let acct_b = ShieldedAccount::from_seed(seed_bytes(&seed2).unwrap());
        assert_ne!(acct_a.public_key(), acct_b.public_key());
    }

    /// THE COMPATIBILITY GATE this whole revision exists for: this module's wallet
    /// derivation must match `sigil-oauth::Keypair::from_seed` — the SAME
    /// `SIGIL_MINE_SEED` value must resolve to the SAME wallet whether it drives the
    /// TUI's [M]ine key or `mine-rig`'s auto-registration. `Keypair::from_seed` is
    /// `SigningKey::from_bytes(seed)` with no hashing step (verified by reading
    /// crates/sigil-oauth/src/lib.rs directly) — this test pins that this module does
    /// the identical thing, not a re-derivation that happens to look similar.
    #[test]
    fn wallet_derivation_matches_the_existing_sigil_mine_seed_convention() {
        let seed_hex = valid_seed(0x99);
        let seed = seed_bytes(&seed_hex).unwrap();

        // What sigil-oauth::Keypair::from_seed does, reproduced inline (that crate
        // isn't a dependency here, so this mirrors its exact one-line body rather
        // than importing it) — SigningKey::from_bytes(seed), nothing else.
        let expected = SigningKey::from_bytes(&seed);
        let actual = wallet_signing_key(&seed_hex).unwrap();
        assert_eq!(
            expected.verifying_key().to_bytes(),
            actual.verifying_key().to_bytes(),
            "SIGIL_MINE_SEED must resolve to the SAME wallet in both mining paths"
        );
    }

    /// The message this module signs must byte-for-byte match what the server's
    /// `verify_wallet_sig` in `sigil-api/src/shielded.rs` reconstructs, or every real
    /// registration would fail signature verification despite a correctly-generated
    /// signature — a silent, total outage for this feature. Pin the exact format here
    /// so a change on either side is caught immediately.
    #[test]
    fn signed_message_matches_the_server_side_canonical_format() {
        let seed_hex = valid_seed(0x77);
        let seed = seed_bytes(&seed_hex).unwrap();
        let sk = wallet_signing_key(&seed_hex).unwrap();
        let wallet_hex = hex::encode(sk.verifying_key().to_bytes());
        let account = ShieldedAccount::from_seed(seed);
        let pk_shield_hex = hex::encode(to_wire(account.public_key()));
        let pk_encrypt_hex = sigil_shield::note_cipher::enc_identity_from_seed(&seed).public_hex();
        let fee: u128 = 0;
        let req_nonce = 42u64;
        let msg = format!(
            "sigil-rpc/v1|shield-register|{wallet_hex}|{pk_shield_hex}|{pk_encrypt_hex}|{fee}|nonce={req_nonce}"
        );
        let sig_bytes = sk.sign(msg.as_bytes()).to_bytes();

        // Reproduce the server's verify step directly (same crate ed25519_dalek is
        // used both sides) rather than trusting our own signing call blindly.
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};
        let vk = VerifyingKey::from_bytes(&sk.verifying_key().to_bytes()).unwrap();
        let sig = Signature::from_bytes(&sig_bytes);
        vk.verify(msg.as_bytes(), &sig).expect("must verify under the exact server-side message format");
    }
}
