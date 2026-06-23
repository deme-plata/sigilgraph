//! sigil-dns-anchor — DNS TXT codec for SIGIL tip-proof checkpoints.
//!
//! Encodes a [`sigil_tip_proof::TipProof`] as a `v=sigil1` DNS TXT string and
//! decodes it back. The TXT record is ~450 bytes — fits in one DNS response
//! without TCP fallback.
//!
//! ## Wire format
//!
//! ```text
//! v=sigil1; t=tip; h=4193822; d=<blake3(roots)>; s=<SQIsign-sig,base64>; k=<key-id>
//! ```
//!
//! - `v` — version tag (`sigil1`)
//! - `t` — record type (`tip` or `genesis`)
//! - `h` — block height
//! - `d` — BLAKE3 digest of the 4 state roots (64 hex chars)
//! - `s` — SQIsign L5 signature, base64-encoded (~388 chars)
//! - `k` — key identifier (producer public key fingerprint, 16 hex chars)
//!
//! The digest anchors the roots; the signature proves the anchor was produced
//! by the key holder. A verifier fetches the full TipProof via DoH/HTTP and
//! checks the signature against the pinned producer key.
//!
//! ## DNS-1: the keystone
//!
//! Everything composes on this codec. Publisher (DNS-2), resolver-verifier
//! (DNS-3), browser WASM (DNS-4), quorum signing (DNS-5) — all depend on
//! the `TipProof ⇄ TXT` round-trip defined here.

/// TXT record fields parsed from a `v=sigil1` string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsAnchor {
    /// Record type: "tip" or "genesis"
    pub record_type: String,
    /// Block height
    pub height: u64,
    /// BLAKE3 digest of the 4 state roots (wallet, dex, event, contract)
    pub digest_hex: String,
    /// SQIsign L5 signature, base64-encoded
    pub sig_base64: String,
    /// Key identifier (producer pk fingerprint)
    pub key_id: String,
    /// #417 (LANE-B): block_hash of the anchored block, 64 hex chars. `None` for legacy
    /// roots-only `v=sigil1` records. REQUIRED for the fold fast-path: it's what
    /// `fast_forward_to_anchored_checkpoint` authenticates the stored anchor block against
    /// (`block.hash() == this`), which the roots digest (`d=`) cannot do.
    pub block_hash_hex: Option<String>,
    /// #417 (LANE-B): monotonic, non-wrapping epoch (e.g. producer Unix-seconds) for the
    /// freshness gate — reject `epoch ≤ last_accepted` AND `now - epoch > MAX_ANCHOR_AGE`.
    /// `None` for legacy records (which therefore can't be freshness-checked → not usable
    /// as a live fold anchor).
    pub epoch: Option<u64>,
}

/// Errors from encoding or decoding a DNS anchor TXT record.
#[derive(Debug, thiserror::Error)]
pub enum AnchorError {
    #[error("missing required field: {0}")]
    MissingField(&'static str),
    #[error("unknown version: {0}")]
    UnknownVersion(String),
    #[error("unknown record type: {0}")]
    UnknownRecordType(String),
    #[error("invalid height: {0}")]
    InvalidHeight(String),
    #[error("digest must be 64 hex chars, got {0}")]
    InvalidDigest(usize),
    #[error("signature too short: {0} bytes (need ≥200 for SQIsign L5 base64)")]
    SignatureTooShort(usize),
    #[error("key ID must be 16 hex chars, got {0}")]
    InvalidKeyId(usize),
    #[error("block_hash must be 64 hex chars, got {0}")]
    InvalidBlockHash(usize),
    #[error("invalid epoch: {0}")]
    InvalidEpoch(String),
    #[error("not a signed anchor: missing block_hash and/or epoch (legacy roots-only record)")]
    NotSignedAnchor,
    #[error("signature is not valid base64")]
    InvalidSignatureEncoding,
    #[error("SQIsign verify error: {0}")]
    VerifyError(String),
}

/// Encode a tip-proof into a `v=sigil1` TXT string.
///
/// The digest (`d=`) is BLAKE3 over the canonical signing bytes of the tip-proof
/// (height + roots + network_id). The signature (`s=`) and key-id (`k=`) are
/// provided by the caller — this codec does not sign; it formats.
pub fn encode_tip(height: u64, roots_digest: &[u8; 32], sig_base64: &str, key_id: &str) -> String {
    let digest_hex = hex::encode(roots_digest);
    format!(
        "v=sigil1; t=tip; h={height}; d={digest_hex}; s={sig_base64}; k={key_id}"
    )
}

/// Encode a genesis anchor TXT string.
pub fn encode_genesis(genesis_hash: &[u8; 32], sig_base64: &str, key_id: &str) -> String {
    let digest_hex = hex::encode(genesis_hash);
    format!(
        "v=sigil1; t=genesis; h=0; d={digest_hex}; s={sig_base64}; k={key_id}"
    )
}

/// #417 (LANE-B) — THE canonical bytes the producer SQIsigns for a signed fold anchor,
/// and that the client (A's `dns_anchor_tip`) reconstructs + verifies against the pinned
/// producer pubkey. Layout (fixed 80 B, little-endian scalars):
///
/// ```text
///   block_hash[32] ‖ roots_digest[32] ‖ height_le[8] ‖ epoch_le[8]
/// ```
///
/// Why each field: `block_hash` is what `fast_forward_to_anchored_checkpoint`
/// authenticates the stored anchor block against (the roots digest can't); `roots_digest`
/// keeps the existing roots commitment bound; `height` pins the anchor height; `epoch` is
/// the monotonic freshness gate (defeats a stale-anchor replay by a still-valid key). Both
/// signer and verifier MUST build the bytes here — never hand-roll the concatenation.
pub fn anchor_signing_bytes(
    block_hash: &[u8; 32],
    roots_digest: &[u8; 32],
    height: u64,
    epoch: u64,
) -> Vec<u8> {
    let mut v = Vec::with_capacity(32 + 32 + 8 + 8);
    v.extend_from_slice(block_hash);
    v.extend_from_slice(roots_digest);
    v.extend_from_slice(&height.to_le_bytes());
    v.extend_from_slice(&epoch.to_le_bytes());
    v
}

/// #417 (LANE-B) — encode a SIGNED tip anchor: the extended `v=sigil1` line carrying the
/// `b=` block_hash + `e=` epoch the fold fast-path + freshness need, alongside the legacy
/// `d=` roots digest. The producer signs [`anchor_signing_bytes`]`(block_hash, roots_digest,
/// height, epoch)` and passes the base64 sig. Backward compatible: legacy decoders ignore
/// `b=`/`e=`; [`decode`] returns them as `Some(..)`. (Old [`encode_tip`] stays for the
/// roots-only path.)
pub fn encode_tip_signed(
    height: u64,
    block_hash: &[u8; 32],
    roots_digest: &[u8; 32],
    epoch: u64,
    sig_base64: &str,
    key_id: &str,
) -> String {
    let bh = hex::encode(block_hash);
    let dd = hex::encode(roots_digest);
    format!("v=sigil1; t=tip; h={height}; b={bh}; d={dd}; e={epoch}; s={sig_base64}; k={key_id}")
}

/// #417 (LANE-B) — verify a SIGNED fold anchor's SQIsign signature. The crypto half of
/// `dns_anchor_tip()`: A's client calls this, then applies freshness (epoch ≤ last / age >
/// MAX_ANCHOR_AGE) + the key_id match itself. Lives in THIS crate so `sigil-top` stays
/// dep-clean (it gets the verify transitively via `sigil-dns-anchor`, no `flux-sqisign` dep
/// + no Cargo bump there — per lead #448).
///
/// Returns `Ok(true)` iff `a` is the signed format (has `block_hash` + `epoch`) AND the
/// producer's SQIsign over [`anchor_signing_bytes`]`(block_hash, roots_digest, height,
/// epoch)` verifies against `producer_pk`. `Ok(false)` = well-formed but the signature
/// doesn't verify (benchable peer). `Err(NotSignedAnchor)` = a legacy roots-only record
/// (can't be a live fold anchor). Does NOT check freshness or key_id — that's the caller's.
pub fn verify_signed_anchor(a: &DnsAnchor, producer_pk: &[u8]) -> Result<bool, AnchorError> {
    use base64::Engine as _; // brings the .decode() method into scope (base64 0.22)
    let bh_hex = a.block_hash_hex.as_deref().ok_or(AnchorError::NotSignedAnchor)?;
    let epoch = a.epoch.ok_or(AnchorError::NotSignedAnchor)?;
    // block_hash + roots digest: decode() already length-checked both to 64 hex, but hex
    // VALIDITY (not just length) is checked here — a non-hex char is a malformed anchor.
    let block_hash: [u8; 32] = hex::decode(bh_hex)
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or(AnchorError::InvalidBlockHash(bh_hex.len()))?;
    let roots_digest: [u8; 32] = hex::decode(&a.digest_hex)
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or(AnchorError::InvalidDigest(a.digest_hex.len()))?;
    let sig = base64::prelude::BASE64_STANDARD
        .decode(a.sig_base64.as_bytes())
        .map_err(|_| AnchorError::InvalidSignatureEncoding)?;
    let msg = anchor_signing_bytes(&block_hash, &roots_digest, a.height, epoch);
    flux_sqisign::verify(&msg, &sig, producer_pk).map_err(AnchorError::VerifyError)
}

/// Parse a `v=sigil1` TXT string into its fields. Performs structural
/// validation only — does NOT verify the SQIsign signature (that's DNS-3).
pub fn decode(txt: &str) -> Result<DnsAnchor, AnchorError> {
    // Parse semicolon-delimited key=value pairs
    let mut fields: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for part in txt.split(';') {
        let part = part.trim();
        if let Some((k, v)) = part.split_once('=') {
            fields.insert(k.trim(), v.trim());
        }
    }

    // Version check
    let version = fields.get("v").ok_or(AnchorError::MissingField("v"))?;
    if *version != "sigil1" {
        return Err(AnchorError::UnknownVersion(version.to_string()));
    }

    // Record type
    let record_type = fields
        .get("t")
        .ok_or(AnchorError::MissingField("t"))?
        .to_string();
    if record_type != "tip" && record_type != "genesis" {
        return Err(AnchorError::UnknownRecordType(record_type));
    }

    // Height
    let height_str = fields.get("h").ok_or(AnchorError::MissingField("h"))?;
    let height: u64 = height_str
        .parse()
        .map_err(|_| AnchorError::InvalidHeight(height_str.to_string()))?;

    // Digest (64 hex chars = 32 bytes BLAKE3)
    let digest_hex = fields.get("d").ok_or(AnchorError::MissingField("d"))?.to_string();
    if digest_hex.len() != 64 {
        return Err(AnchorError::InvalidDigest(digest_hex.len()));
    }

    // Signature (base64, ≥200 chars for SQIsign L5 292B)
    let sig_base64 = fields.get("s").ok_or(AnchorError::MissingField("s"))?.to_string();
    if sig_base64.len() < 200 {
        return Err(AnchorError::SignatureTooShort(sig_base64.len()));
    }

    // Key ID (16 hex chars = 8 bytes fingerprint)
    let key_id = fields.get("k").ok_or(AnchorError::MissingField("k"))?.to_string();
    if key_id.len() != 16 {
        return Err(AnchorError::InvalidKeyId(key_id.len()));
    }

    // #417 (LANE-B): OPTIONAL block_hash (b=) + epoch (e=) for the signed fold-anchor.
    // Legacy roots-only `v=sigil1` records omit them → None (still decode cleanly; the
    // HashMap parser already ignores unknown keys, so old verifiers are unaffected).
    let block_hash_hex = match fields.get("b") {
        Some(b) => {
            if b.len() != 64 {
                return Err(AnchorError::InvalidBlockHash(b.len()));
            }
            Some(b.to_string())
        }
        None => None,
    };
    let epoch = match fields.get("e") {
        Some(e) => Some(e.parse::<u64>().map_err(|_| AnchorError::InvalidEpoch(e.to_string()))?),
        None => None,
    };

    Ok(DnsAnchor {
        record_type,
        height,
        digest_hex,
        sig_base64,
        key_id,
        block_hash_hex,
        epoch,
    })
}

/// Compute the roots digest: BLAKE3(wallet_root || dex_root || event_root || contract_root).
pub fn roots_digest(
    wallet_state_root: &[u8; 32],
    dex_state_root: &[u8; 32],
    event_log_root: &[u8; 32],
    contract_state_root: &[u8; 32],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(wallet_state_root);
    hasher.update(dex_state_root);
    hasher.update(event_log_root);
    hasher.update(contract_state_root);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_tip() {
        let digest = [0x42u8; 32];
        let sig = "a".repeat(390); // base64 of 292B SQIsign sig (~388 chars)
        let txt = encode_tip(4193822, &digest, &sig, "abcd1234abcd1234");
        let anchor = decode(&txt).unwrap();
        assert_eq!(anchor.record_type, "tip");
        assert_eq!(anchor.height, 4193822);
        assert_eq!(anchor.digest_hex, hex::encode(&digest));
        assert_eq!(anchor.sig_base64, sig);
        assert_eq!(anchor.key_id, "abcd1234abcd1234");
    }

    #[test]
    fn roundtrip_genesis() {
        let hash = [0x13u8; 32];
        let sig = "b".repeat(390);
        let txt = encode_genesis(&hash, &sig, "deadbeefdeadbeef");
        let anchor = decode(&txt).unwrap();
        assert_eq!(anchor.record_type, "genesis");
        assert_eq!(anchor.height, 0);
    }

    #[test]
    fn reject_wrong_version() {
        let txt = "v=sigil2; t=tip; h=1; d=4242424242424242424242424242424242424242424242424242424242424242; s=AAAA; k=abcd1234abcd1234";
        assert!(decode(txt).is_err());
    }

    #[test]
    fn reject_short_digest() {
        let sig = "a".repeat(390);
        let txt = "v=sigil1; t=tip; h=1; d=too_short; s=".to_string() + &sig + "; k=abcd1234abcd1234";
        assert!(decode(&txt).is_err());
    }

    #[test]
    fn reject_short_signature() {
        let digest = "42".repeat(32);
        let txt = format!("v=sigil1; t=tip; h=1; d={digest}; s=short; k=abcd1234abcd1234");
        assert!(decode(&txt).is_err());
    }

    // ── #417 signed-anchor format (block_hash + epoch) ──────────────────────────

    #[test]
    fn legacy_tip_has_no_block_hash_or_epoch() {
        // Old roots-only encode → decode must yield None for the new fields (back-compat).
        let digest = [0x42u8; 32];
        let sig = "a".repeat(390);
        let anchor = decode(&encode_tip(7, &digest, &sig, "abcd1234abcd1234")).unwrap();
        assert_eq!(anchor.block_hash_hex, None);
        assert_eq!(anchor.epoch, None);
    }

    #[test]
    fn roundtrip_signed_tip_carries_block_hash_and_epoch() {
        let block_hash = [0xABu8; 32];
        let roots = [0x42u8; 32];
        let sig = "c".repeat(390);
        let txt = encode_tip_signed(4193822, &block_hash, &roots, 1_781_838_000, &sig, "d6214c8ddc0fca2b");
        let a = decode(&txt).unwrap();
        assert_eq!(a.record_type, "tip");
        assert_eq!(a.height, 4193822);
        assert_eq!(a.digest_hex, hex::encode(&roots));
        assert_eq!(a.block_hash_hex.as_deref(), Some(hex::encode(block_hash).as_str()));
        assert_eq!(a.epoch, Some(1_781_838_000));
        assert_eq!(a.sig_base64, sig);
        assert_eq!(a.key_id, "d6214c8ddc0fca2b");
    }

    #[test]
    fn signing_bytes_are_deterministic_80b_and_order_sensitive() {
        let bh = [1u8; 32];
        let roots = [2u8; 32];
        let a = anchor_signing_bytes(&bh, &roots, 100, 200);
        assert_eq!(a.len(), 80, "32+32+8+8");
        assert_eq!(a, anchor_signing_bytes(&bh, &roots, 100, 200), "deterministic");
        // a different block_hash, roots, height, or epoch MUST change the signed bytes.
        assert_ne!(a, anchor_signing_bytes(&[9u8; 32], &roots, 100, 200));
        assert_ne!(a, anchor_signing_bytes(&bh, &[9u8; 32], 100, 200));
        assert_ne!(a, anchor_signing_bytes(&bh, &roots, 101, 200));
        assert_ne!(a, anchor_signing_bytes(&bh, &roots, 100, 201));
    }

    #[test]
    fn reject_malformed_block_hash() {
        let digest = "42".repeat(32);
        let sig = "a".repeat(390);
        let txt = format!("v=sigil1; t=tip; h=1; b=too_short; d={digest}; e=5; s={sig}; k=abcd1234abcd1234");
        assert!(matches!(decode(&txt), Err(AnchorError::InvalidBlockHash(_))));
    }

    /// #417 — the full producer→verifier crypto round-trip: sign anchor_signing_bytes with a
    /// real SQIsign keypair, encode the signed TXT, decode, verify. Honest → true; foreign
    /// key → false; legacy roots-only record → NotSignedAnchor (can't be a live anchor).
    #[test]
    fn verify_signed_anchor_roundtrip_tamper_and_legacy() {
        use base64::Engine as _;
        let (sk, pk) = flux_sqisign::keygen();
        let bh = [0xABu8; 32];
        let roots = [0x42u8; 32];
        let (height, epoch) = (32_022u64, 1_781_840_000u64);
        let msg = anchor_signing_bytes(&bh, &roots, height, epoch);
        let sig = flux_sqisign::sign(&msg, &sk, &pk).expect("sign");
        let sig_b64 = base64::prelude::BASE64_STANDARD.encode(&sig);
        let txt = encode_tip_signed(height, &bh, &roots, epoch, &sig_b64, "d6214c8ddc0fca2b");

        let a = decode(&txt).expect("signed anchor decodes");
        assert_eq!(verify_signed_anchor(&a, &pk).unwrap(), true, "honest anchor verifies");

        // foreign producer key → does NOT verify (false, not a crash).
        let (_, foreign_pk) = flux_sqisign::keygen();
        assert_eq!(verify_signed_anchor(&a, &foreign_pk).unwrap_or(false), false, "foreign key rejected");

        // legacy roots-only record → NotSignedAnchor (no block_hash/epoch to anchor on).
        let legacy = decode(&encode_tip(1, &roots, &"a".repeat(390), "abcd1234abcd1234")).unwrap();
        assert!(matches!(verify_signed_anchor(&legacy, &pk), Err(AnchorError::NotSignedAnchor)));
    }
}
