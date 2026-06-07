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

    Ok(DnsAnchor {
        record_type,
        height,
        digest_hex,
        sig_base64,
        key_id,
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
}
