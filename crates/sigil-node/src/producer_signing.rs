//! Opt-in real Ed25519 signing for self-mined blocks — wiring the header's
//! already-built (but dormant) `verify_producer_sig()` / `H1_PRODUCER_SIG_
//! ACTIVATION_HEIGHT` mechanism (`sigil-header`) to an actual keypair.
//!
//! ## Why this is separate from `coinbase::producer_wallet()`
//!
//! `header.producer` is NOT just an address — for solved (externally-mined)
//! blocks it is part of the PoW challenge itself (`sigil-api::mining`'s doc:
//! "the solved header additionally binds the miner's wallet"; the winning
//! nonce is only valid for the exact `(parent_hash, height, producer)` triple
//! the miner hashed against). Repointing that field to a different signing
//! identity after the fact would invalidate the proof-of-work. So this module
//! deliberately does NOT touch external-miner (`solve = Some(..)`) blocks —
//! only the self-mined (`solve = None`) path, where `producer` already comes
//! from `coinbase::producer_wallet()` and nothing else depends on its exact
//! bytes ahead of time.
//!
//! ## Zero behavior change unless explicitly configured
//!
//! Reading `SIGIL_PRODUCER_SIGNING_SEED_HEX` is the ONLY way this module does
//! anything. Unset means every function here returns `None` / is a no-op, and
//! block minting is byte-for-byte identical to before this file existed.
//!
//! ⚠️ **The seed IS set on Epsilon — corrected 2026-09-02.** This paragraph used to
//! assert it was "unset on every live node, Epsilon included — it runs on the
//! `[0xC1;32]` dev-default wallet with no known key". That is FALSE and was
//! actively misleading: `SIGIL_PRODUCER_SIGNING_SEED_HEX` is present in the live
//! producer's environment, and the key it derives is the one the chain records.
//! Verified without ever reading the secret — the `producer` field of live blocks
//! (`GET /v1/dagknight/recent`) is
//! `73b7745271b6be22dd8ca4be17f6fbff2df794d2d0b3c98ae791b219a6bc33d9`, which
//! matches the pubkey derived from that seed.
//!
//! Why the correction matters beyond tidiness: this node has a REAL, externally
//! anchorable identity. Anything the node signs can be verified by a third party
//! against a key they read off a block it minted — no trust in the operator, no
//! side channel. `sigil-api`'s acceptance receipts depend on exactly that. A
//! reader who believed the old comment would conclude no such identity existed
//! and design around a problem that does not exist.
//!
//! Re-check rather than trust either version of this comment:
//!   curl -s http://127.0.0.1:18181/v1/dagknight/recent | python3 -c \
//!     "import sys,json;print(bytes(json.load(sys.stdin)['data']['blocks'][0]['producer']).hex())"

use ed25519_dalek::{Signer, SigningKey};
use sigil_header::{SigScheme, SignatureBytes, SigilBlockHeaderV0};
use sigil_state::WalletId;

const SEED_ENV: &str = "SIGIL_PRODUCER_SIGNING_SEED_HEX";

/// The operator-configured signing key, if any. `SIGIL_PRODUCER_SIGNING_SEED_HEX`
/// must be exactly 64 hex chars (32 raw bytes) — the ed25519-dalek `SigningKey`
/// seed. Malformed input is treated as "not configured" (fails safe to the
/// legacy unsigned path) rather than a hard panic — an operator typo should
/// degrade to "not signed yet," not take the producer down.
pub fn configured_signing_key() -> Option<SigningKey> {
    let hex_seed = std::env::var(SEED_ENV).ok()?;
    let hex_seed = hex_seed.trim();
    if hex_seed.len() != 64 || !hex_seed.is_ascii() {
        return None; // is_ascii: a 64-BYTE multibyte seed would split a UTF-8 boundary below
    }
    let mut seed = [0u8; 32];
    for i in 0..32 {
        seed[i] = u8::from_str_radix(&hex_seed[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(SigningKey::from_bytes(&seed))
}

/// The wallet address implied by the configured signing key (its raw 32-byte
/// public key — same shape as a `WalletId`/`ValidatorId`, so it can be used
/// directly as both). `None` if no signing key is configured.
pub fn configured_signing_wallet() -> Option<WalletId> {
    configured_signing_key().map(|k| k.verifying_key().to_bytes())
}

/// Resolve the effective producer wallet, reconciling `SIGIL_PRODUCER_WALLET`
/// (the existing explicit-address override) with a configured signing key.
///
/// - Neither set: unchanged legacy behavior — caller falls through to the
///   `[0xC1;32]` dev default (this function only handles the signing-key
///   interaction; `coinbase::producer_wallet()` keeps its own fallback).
/// - Only `SIGIL_PRODUCER_WALLET` set: unchanged legacy behavior, `None` here.
/// - Only the signing key set: the derived wallet becomes the producer
///   wallet, so PoW-binding + reward + signature verification all agree.
/// - Both set and they MATCH: same as "only signing key set."
/// - Both set and they DISAGREE: `Err` — this is very likely an operator
///   mistake (rewards would silently go to an address that can never sign
///   for itself, or a signed identity would never receive its own reward).
///   Fail loud at startup rather than silently pick one.
pub fn reconcile_producer_wallet(
    explicit_wallet_hex: Option<&str>,
) -> Result<Option<WalletId>, String> {
    let Some(derived) = configured_signing_wallet() else {
        return Ok(None);
    };
    if let Some(hex_addr) = explicit_wallet_hex {
        let explicit = parse_hex64(hex_addr)
            .ok_or_else(|| format!("SIGIL_PRODUCER_WALLET is not valid 64-hex: {hex_addr:?}"))?;
        if explicit != derived {
            return Err(format!(
                "SIGIL_PRODUCER_WALLET ({}) does not match the wallet derived from \
                 {SEED_ENV} ({}) — configure them to agree, or unset one.",
                hex::encode(explicit),
                hex::encode(derived),
            ));
        }
    }
    Ok(Some(derived))
}

fn parse_hex64(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 || !s.is_ascii() {
        return None; // is_ascii: a 64-BYTE multibyte string would split a UTF-8 boundary below
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Sign `header` in place if `header.producer` matches the configured signing
/// key's wallet — no-op otherwise (legacy `SqiSign5`/zeroed-sig header passed
/// through unchanged). Must be called with `header.producer_sig` still at
/// whatever placeholder the caller built (it gets overwritten here) and
/// BEFORE the header is hashed/stored anywhere, since `sig_scheme` and
/// `producer_sig` are both part of the block's identity once sealed.
///
/// Real Ed25519 sign-then-verify round trip, not a stub: the exact same
/// `signing_bytes()` canonicalization `verify_producer_sig()` checks against.
pub fn maybe_sign(header: &mut SigilBlockHeaderV0) {
    let Some(key) = configured_signing_key() else { return };
    if key.verifying_key().to_bytes() != header.producer {
        // Configured key doesn't match this header's producer (e.g. an
        // externally-solved block credited a different wallet) — nothing to
        // sign with here. Leave the header exactly as the caller built it.
        return;
    }
    header.sig_scheme = SigScheme::Ed25519Hot;
    header.producer_sig = SignatureBytes(Vec::new()); // zeroed for signing_bytes()
    let sig = key.sign(&header.signing_bytes());
    header.producer_sig = SignatureBytes(sig.to_bytes().to_vec());
}

// ─────────────────────────────────────────────────────────────────────────
// Post-quantum: SQIsign5 + Ed25519, REQUIRE-BOTH (SigScheme::HybridSqiEd25519)
// ─────────────────────────────────────────────────────────────────────────
//
// Ed25519 alone is NOT quantum-resistant — `maybe_sign` above is the
// classical-only leg. This section adds SQIsign5 (isogeny-based, NIST PQC
// Level 5) as a REQUIRED second leg, reusing `flux_sqisign::hybrid` — the
// same require-all acceptance model, domain-separated binding, already used
// for release/artifact provenance signing in this codebase — applied here
// to a block header's `signing_bytes()` instead of a release artifact. A
// break in EITHER family alone does not forge a block.
//
// `header.producer` (32 bytes) can't hold either pubkey directly (SQIsign5's
// alone is 129 bytes) — per `ValidatorId`'s own doc ("content-addressed"),
// it's set to `BLAKE3(sqisign_pk || ed25519_pk)` instead. The full pubkeys
// travel INSIDE the signature bundle (`hybrid::serialize_hybrid` already
// includes each leg's pubkey), so a verifier can extract them from the
// block alone. The actual trust anchor is a single pinned 32-byte value
// (`SIGIL_TRUSTED_PRODUCER_ID_HEX`): every verifying node (including the
// signer itself, which also applies its own blocks) must agree in advance
// on the EXPECTED `producer` value for the real producer — otherwise anyone
// could mint a fresh keypair, embed it, and pass verification against
// itself. Same "pinned trusted pubkey" pattern already used elsewhere in
// this codebase (e.g. the snapshot-anchor `SIGIL_ANCHOR_PK_HEX`).

use flux_sqisign::hybrid::{self, SchemeId};

const SQISIGN_SK_ENV: &str = "SIGIL_PRODUCER_SQISIGN_SK_HEX";
const SQISIGN_PK_ENV: &str = "SIGIL_PRODUCER_SQISIGN_PK_HEX";
const TRUSTED_PRODUCER_ID_ENV: &str = "SIGIL_TRUSTED_PRODUCER_ID_HEX";

/// This node's own SQIsign5 keypair, if configured (only set on an actual
/// signing/producer node — a pure verifier never needs it). `None` on any
/// malformed hex, failing safe to "can't sign" rather than a panic.
fn configured_sqisign_keypair() -> Option<(Vec<u8>, Vec<u8>)> {
    let sk = hex_decode_var(SQISIGN_SK_ENV)?;
    let pk = hex_decode_var(SQISIGN_PK_ENV)?;
    if pk.len() != flux_sqisign::public_key_size() {
        return None;
    }
    Some((sk, pk))
}

fn hex_decode_var(var: &str) -> Option<Vec<u8>> {
    let s = std::env::var(var).ok()?;
    let s = s.trim();
    if s.len() % 2 != 0 || !s.is_ascii() {
        return None; // is_ascii: an even-BYTE multibyte string could straddle a slice below
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// The pinned, independently-agreed "this is the real producer" identity —
/// the EXPECTED value of `header.producer` for a trusted hybrid-signed
/// block. Every node (signer and verifiers alike) must set this to the SAME
/// value out-of-band for hybrid verification to mean anything; unset means
/// no hybrid block can ever be trusted (fails closed, not open).
fn trusted_producer_id() -> Option<[u8; 32]> {
    parse_hex64(std::env::var(TRUSTED_PRODUCER_ID_ENV).ok()?.trim())
}

/// Content-address of a (SQIsign pk, Ed25519 pk) pair — what `header.
/// producer` holds for a `HybridSqiEd25519` block.
fn hybrid_producer_id(sqisign_pk: &[u8], ed25519_pk: &[u8; 32]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(sqisign_pk);
    h.update(ed25519_pk);
    *h.finalize().as_bytes()
}

/// The wallet this node would sign hybrid blocks as, if both an Ed25519
/// signing key AND a SQIsign5 keypair are configured. `None` if either half
/// is missing — hybrid signing requires BOTH legs, no partial credit, same
/// as verification.
pub fn configured_hybrid_producer_wallet() -> Option<WalletId> {
    let ed = configured_signing_key()?;
    let (_, sqisign_pk) = configured_sqisign_keypair()?;
    Some(hybrid_producer_id(&sqisign_pk, &ed.verifying_key().to_bytes()))
}

/// How often (in blocks) a self-mined block gets the full post-quantum
/// hybrid signature, rather than the fast Ed25519-only one.
///
/// MEASURED, not guessed: real SQIsign5 `sign()` costs ~1.16s on this
/// hardware (release build) — roughly 40× Epsilon's current per-block tick
/// budget. Signing every block would throttle production to under 1 blk/s.
/// Operator-directed 2026-08-20: periodic checkpoints instead — matches this
/// codebase's own existing design split (`SigScheme::Ed25519Hot`'s doc:
/// "the CLASSICAL hot-path scheme... SqiSign5 stays the post-quantum
/// SETTLEMENT scheme"). 128 is a round number giving comfortable wall-clock
/// headroom above the ~1.16s sign cost even at a high production rate
/// (≥40 blk/s → ≥3.2s between checkpoints); at slower observed rates the
/// headroom is far larger. Not env-configurable (yet) — a deliberate,
/// reviewable constant, same posture as the activation height.
pub const HYBRID_CHECKPOINT_INTERVAL: u64 = 128;

/// True if `height` is a hybrid-checkpoint height (`height % INTERVAL == 0`
/// — includes height 0, which is fine, genesis isn't self-mined through this
/// path). Callers use this to decide whether to even ATTEMPT the ~1.16s
/// hybrid signing path for this block, before touching `header.producer`.
pub fn is_hybrid_checkpoint(height: u64) -> bool {
    height % HYBRID_CHECKPOINT_INTERVAL == 0
}

/// Sign `header` with the REAL post-quantum-safe hybrid scheme in place, if
/// (and only if) both legs are configured AND `header.producer` already
/// matches the resulting wallet — same no-op-unless-matching contract as
/// `maybe_sign`. Callers should try this FIRST (see `mint_next_block`) and
/// fall back to `maybe_sign` (Ed25519-only) only if this doesn't apply.
pub fn maybe_sign_hybrid(header: &mut SigilBlockHeaderV0) {
    let Some(ed) = configured_signing_key() else { return };
    let Some((sqisign_sk, sqisign_pk)) = configured_sqisign_keypair() else { return };
    let ed_pk = ed.verifying_key().to_bytes();
    if hybrid_producer_id(&sqisign_pk, &ed_pk) != header.producer {
        return;
    }

    let mut keys = std::collections::HashMap::new();
    keys.insert(SchemeId::SQIsign, (sqisign_sk, sqisign_pk));
    keys.insert(SchemeId::Ed25519, (ed.to_bytes().to_vec(), ed_pk.to_vec()));

    header.sig_scheme = SigScheme::HybridSqiEd25519;
    header.producer_sig = SignatureBytes(Vec::new()); // zeroed for signing_bytes()
    let record = header.signing_bytes();
    let schemes = [SchemeId::SQIsign, SchemeId::Ed25519];
    match hybrid::hybrid_sign(&record, &keys, &schemes) {
        Ok(bundle) => {
            header.producer_sig = SignatureBytes(hybrid::serialize_hybrid(&bundle));
        }
        Err(e) => {
            // Signing genuinely failed (shouldn't happen with well-formed
            // keys) — fail SAFE by resetting to the legacy unsigned shape
            // rather than shipping a half-built header.
            eprintln!("⚠ hybrid producer-signing failed, falling back to unsigned: {e}");
            header.sig_scheme = SigScheme::SqiSign5;
            header.producer_sig = SignatureBytes(vec![0u8; sigil_header::SQISIGN_L5_LEN]);
        }
    }
}

/// Real verification for a `HybridSqiEd25519` block — the check `sigil-
/// header::verify_producer_sig` deliberately can't do itself (would need to
/// link `flux-sqisign`/`sqisign_rs`, breaking that crate's dependency-light
/// design for light clients). Called from `ChainTip::apply` alongside (not
/// instead of) `header.verify_at_height`.
///
/// Fails closed on anything unconfigured or mismatched:
///   - `SIGIL_TRUSTED_PRODUCER_ID_HEX` unset → no hybrid block ever trusted.
///   - `header.producer` != the pinned trusted id → reject (wrong signer).
///   - the embedded pubkeys don't hash to `header.producer` → reject
///     (bundle doesn't match the identity it claims — belt and suspenders
///     against a crafted bundle with different keys than the id implies).
///   - `hybrid_verify` doesn't return `all_valid` (either leg fails, or the
///     bundle's scheme-set isn't exactly {SQIsign, Ed25519}) → reject.
pub fn verify_self_mined_hybrid(header: &SigilBlockHeaderV0) -> Result<(), String> {
    let trusted = trusted_producer_id().ok_or_else(|| {
        format!("{TRUSTED_PRODUCER_ID_ENV} not configured — no hybrid block can be trusted")
    })?;
    if header.producer != trusted {
        return Err("header.producer does not match the pinned trusted producer id".into());
    }
    let bundle = hybrid::deserialize_hybrid(&header.producer_sig.0)
        .map_err(|e| format!("malformed hybrid signature bundle: {e}"))?;

    let sqisign_pk = bundle
        .signatures
        .iter()
        .find(|s| s.scheme == SchemeId::SQIsign)
        .map(|s| s.public_key.clone())
        .ok_or("hybrid bundle missing SQIsign leg")?;
    let ed_pk_vec = bundle
        .signatures
        .iter()
        .find(|s| s.scheme == SchemeId::Ed25519)
        .map(|s| s.public_key.clone())
        .ok_or("hybrid bundle missing Ed25519 leg")?;
    let ed_pk: [u8; 32] = ed_pk_vec
        .as_slice()
        .try_into()
        .map_err(|_| "malformed Ed25519 pubkey in bundle")?;
    if hybrid_producer_id(&sqisign_pk, &ed_pk) != header.producer {
        return Err("bundle's embedded pubkeys do not hash to header.producer".into());
    }

    let result = hybrid::hybrid_verify(
        &header.signing_bytes(),
        &bundle,
        &[SchemeId::SQIsign, SchemeId::Ed25519],
    );
    if !result.all_valid {
        return Err(format!(
            "hybrid verification failed: {} (passed={:?} failed={:?})",
            result.reason, result.passed_schemes, result.failed_schemes
        ));
    }
    Ok(())
}

// Rust runs tests in parallel within one process, and any test anywhere in
// this crate that touches `SIGIL_PRODUCER_SIGNING_SEED_HEX` mutates the SAME
// process-wide env var — without serializing, they race. `pub(crate)` (not
// just `mod tests`-private) so `main.rs`'s own integration tests, which mint
// a real block through this same env var, can share the SAME lock rather
// than risk a race against a second, independent lock. Test-only —
// production code (`configured_signing_key` etc.) never touches this.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
#[cfg(test)]
pub(crate) fn locked() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_header::{
        ProofBundle, SqiSignature, StarkProof, WesolowskiProof, HEADER_VERSION, NETWORK_ID,
        SQISIGN_L5_LEN,
    };

    fn fake_header(producer: [u8; 32]) -> SigilBlockHeaderV0 {
        SigilBlockHeaderV0 {
            version: HEADER_VERSION,
            network_id: NETWORK_ID,
            height: 42,
            parent_hash: [9u8; 32],
            merge_parents: vec![],
            timestamp_ms: 0,
            nonce_sqisign: SqiSignature::from_array([0u8; SQISIGN_L5_LEN]),
            vdf_input: {
                let mut h = blake3::Hasher::new();
                h.update(&[9u8; 32]);
                h.update(&[0u8; SQISIGN_L5_LEN]);
                *h.finalize().as_bytes()
            },
            vdf_proof: WesolowskiProof { y: vec![], pi: vec![], t: 0 },
            difficulty: 0,
            wallet_state_root: [1u8; 32],
            dex_state_root: [0u8; 32],
            event_log_root: [0u8; 32],
            contract_state_root: [0u8; 32],
            state_transition_proof: StarkProof { bytes: vec![], public_inputs_hash: [0u8; 32] },
            txs_merkle_root: [0u8; 32],
            tx_count: 0,
            fluxc_artifact_proof: ProofBundle {
                artifact_blake3: [0u8; 32],
                sqisign_sig: vec![],
                sqisign_pubkey: vec![],
                settle_tx: None,
            },
            sig_scheme: SigScheme::SqiSign5,
            producer,
            producer_sig: SignatureBytes(vec![0u8; SQISIGN_L5_LEN]),
            topology_commitment: None,
        }
    }

    #[test]
    fn unconfigured_env_is_a_complete_no_op() {
        let _guard = locked();
        std::env::remove_var(SEED_ENV);
        assert!(configured_signing_key().is_none());
        assert!(configured_signing_wallet().is_none());
        assert_eq!(reconcile_producer_wallet(None), Ok(None));
        assert_eq!(reconcile_producer_wallet(Some("aa".repeat(32).as_str())), Ok(None));

        let mut header = fake_header([0xC1; 32]);
        let before = header.clone();
        maybe_sign(&mut header);
        assert_eq!(header, before, "no signing key configured => header untouched");
    }

    #[test]
    fn configured_key_signs_and_verifies_real_round_trip() {
        let _guard = locked();
        let seed = [7u8; 32];
        std::env::set_var(SEED_ENV, hex::encode(seed));
        let key = configured_signing_key().expect("seed parses");
        let wallet = key.verifying_key().to_bytes();
        assert_eq!(configured_signing_wallet(), Some(wallet));

        let mut header = fake_header(wallet);
        maybe_sign(&mut header);
        assert_eq!(header.sig_scheme, SigScheme::Ed25519Hot);
        assert_ne!(header.producer_sig.0, vec![0u8; 64], "must be a real signature, not zeroed");
        header.verify_producer_sig().expect("real signature must verify");

        std::env::remove_var(SEED_ENV);
    }

    #[test]
    fn tampering_after_signing_breaks_verification() {
        let _guard = locked();
        let seed = [11u8; 32];
        std::env::set_var(SEED_ENV, hex::encode(seed));
        let key = configured_signing_key().expect("seed parses");
        let wallet = key.verifying_key().to_bytes();

        let mut header = fake_header(wallet);
        maybe_sign(&mut header);
        header.verify_producer_sig().expect("unmodified signed header verifies");

        // Flip one byte of a state root AFTER signing — the signature must
        // no longer verify (this is the entire point: a forged/tampered
        // header must be loudly rejected, not silently accepted).
        header.wallet_state_root[0] ^= 0xFF;
        assert!(header.verify_producer_sig().is_err(), "tampered header must fail verification");

        std::env::remove_var(SEED_ENV);
    }

    #[test]
    fn mismatched_producer_is_left_unsigned() {
        let _guard = locked();
        let seed = [3u8; 32];
        std::env::set_var(SEED_ENV, hex::encode(seed));
        // Header's producer is some OTHER wallet (e.g. an externally-solved
        // block) — must not be signed with a key that doesn't match it.
        let mut header = fake_header([0xAA; 32]);
        let before = header.clone();
        maybe_sign(&mut header);
        assert_eq!(header, before, "producer mismatch => left exactly as built");

        std::env::remove_var(SEED_ENV);
    }

    #[test]
    fn reconcile_rejects_mismatched_explicit_wallet() {
        let _guard = locked();
        let seed = [5u8; 32];
        std::env::set_var(SEED_ENV, hex::encode(seed));
        let derived = configured_signing_wallet().unwrap();

        // Matching explicit wallet: fine.
        assert_eq!(
            reconcile_producer_wallet(Some(hex::encode(derived).as_str())),
            Ok(Some(derived))
        );
        // Mismatched explicit wallet: loud error, not a silent pick.
        assert!(reconcile_producer_wallet(Some(&"bb".repeat(32))).is_err());

        std::env::remove_var(SEED_ENV);
    }

    // ── Hybrid (SQIsign5 + Ed25519, post-quantum) ──
    //
    // Real `flux_sqisign::keygen()`/`sign()` cost real wall-clock time
    // (~0.5s / ~1.2s measured, release build) — these tests use REAL crypto,
    // not mocks, so they're deliberately few and each does real work once.

    fn set_hybrid_env(sqisign_sk: &[u8], sqisign_pk: &[u8], ed_seed: [u8; 32]) {
        std::env::set_var(SQISIGN_SK_ENV, hex::encode(sqisign_sk));
        std::env::set_var(SQISIGN_PK_ENV, hex::encode(sqisign_pk));
        std::env::set_var(SEED_ENV, hex::encode(ed_seed));
    }

    fn clear_hybrid_env() {
        std::env::remove_var(SQISIGN_SK_ENV);
        std::env::remove_var(SQISIGN_PK_ENV);
        std::env::remove_var(SEED_ENV);
        std::env::remove_var(TRUSTED_PRODUCER_ID_ENV);
    }

    #[test]
    fn hybrid_unconfigured_is_a_complete_no_op() {
        let _guard = locked();
        clear_hybrid_env();
        assert!(configured_hybrid_producer_wallet().is_none());
        let mut header = fake_header([0u8; 32]);
        let before = header.clone();
        maybe_sign_hybrid(&mut header);
        assert_eq!(header, before, "no hybrid keys configured => header untouched");
        assert!(
            verify_self_mined_hybrid(&header).is_err(),
            "no trusted id configured => fails closed, not open"
        );
    }

    #[test]
    fn hybrid_real_sign_and_verify_round_trip() {
        let _guard = locked();
        let (sqisign_sk, sqisign_pk) = flux_sqisign::keygen();
        let ed_seed = [77u8; 32];
        set_hybrid_env(&sqisign_sk, &sqisign_pk, ed_seed);

        let wallet = configured_hybrid_producer_wallet().expect("both legs configured");
        std::env::set_var(TRUSTED_PRODUCER_ID_ENV, hex::encode(wallet));

        let mut header = fake_header(wallet);
        maybe_sign_hybrid(&mut header);
        assert_eq!(header.sig_scheme, SigScheme::HybridSqiEd25519);
        assert_eq!(header.producer_sig.0.len(), SigScheme::HybridSqiEd25519.expected_sig_len());

        verify_self_mined_hybrid(&header).expect("real hybrid signature must verify");

        clear_hybrid_env();
    }

    #[test]
    fn hybrid_tampered_header_rejected() {
        let _guard = locked();
        let (sqisign_sk, sqisign_pk) = flux_sqisign::keygen();
        let ed_seed = [88u8; 32];
        set_hybrid_env(&sqisign_sk, &sqisign_pk, ed_seed);
        let wallet = configured_hybrid_producer_wallet().unwrap();
        std::env::set_var(TRUSTED_PRODUCER_ID_ENV, hex::encode(wallet));

        let mut header = fake_header(wallet);
        maybe_sign_hybrid(&mut header);
        verify_self_mined_hybrid(&header).expect("unmodified signed header verifies");

        header.wallet_state_root[0] ^= 0xFF; // tamper AFTER signing
        assert!(
            verify_self_mined_hybrid(&header).is_err(),
            "tampered header must fail hybrid verification"
        );

        clear_hybrid_env();
    }

    #[test]
    fn hybrid_untrusted_producer_id_rejected_even_with_valid_crypto() {
        let _guard = locked();
        let (sqisign_sk, sqisign_pk) = flux_sqisign::keygen();
        let ed_seed = [99u8; 32];
        set_hybrid_env(&sqisign_sk, &sqisign_pk, ed_seed);
        let wallet = configured_hybrid_producer_wallet().unwrap();
        // Deliberately do NOT set TRUSTED_PRODUCER_ID_ENV to match `wallet` —
        // pin it to something else, simulating a verifier that doesn't trust
        // THIS particular (validly-signing) identity.
        std::env::set_var(TRUSTED_PRODUCER_ID_ENV, hex::encode([0xEE; 32]));

        let mut header = fake_header(wallet);
        maybe_sign_hybrid(&mut header); // signs correctly — crypto is fine
        assert_eq!(header.sig_scheme, SigScheme::HybridSqiEd25519, "signing itself succeeds");
        assert!(
            verify_self_mined_hybrid(&header).is_err(),
            "valid crypto from an UNTRUSTED identity must still be rejected"
        );

        clear_hybrid_env();
    }

    #[test]
    fn hybrid_checkpoint_gating_is_periodic() {
        assert!(is_hybrid_checkpoint(0));
        assert!(is_hybrid_checkpoint(HYBRID_CHECKPOINT_INTERVAL));
        assert!(is_hybrid_checkpoint(HYBRID_CHECKPOINT_INTERVAL * 7));
        assert!(!is_hybrid_checkpoint(1));
        assert!(!is_hybrid_checkpoint(HYBRID_CHECKPOINT_INTERVAL - 1));
        assert!(!is_hybrid_checkpoint(HYBRID_CHECKPOINT_INTERVAL + 1));
    }
}
