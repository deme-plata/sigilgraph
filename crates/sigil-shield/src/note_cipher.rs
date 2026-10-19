//! NOTE CIPHERTEXTS — the layer that makes RECEIVING possible (2026-08-23).
//!
//! A note commitment is `compress2(compress2(value, blinding), pk)`. It is *hiding*, so a
//! recipient staring at the chain cannot read `value` or `blinding` out of it. Without a
//! side channel a wallet can only recognise notes it created itself — which is why
//! shielded self-custody worked and shielded *payments* did not.
//!
//! This module is that side channel: the sender seals `(value, blinding)` to the
//! recipient's encryption key and publishes the ciphertext alongside the commitment. The
//! recipient trial-decrypts every ciphertext in every block; the AEAD tag fails for
//! everyone else, so a successful open IS the ownership signal. Nobody learns who the
//! recipient is — only that *someone* could open it, and only that someone knows.
//!
//! # Why this had to come after owner binding
//!
//! Handing a recipient `(value, blinding)` is precisely what this does. Under the earlier
//! circuit, a note's commitment bound no owner, so *both* parties could then spend it —
//! each producing a different nullifier, which the spent-set cannot catch. Shipping
//! ciphertexts first would have shipped a double-spend. Owner binding
//! (`spend_full_v4`) is what makes disclosure safe: the sender learns nothing it can
//! spend, because it lacks the recipient's `sk`.
//!
//! # A shielded address is two keys
//!
//! ```text
//!   pk_shield : field element — what the CIRCUIT binds a note to
//!   pk_enc    : X25519 key    — what CIPHERTEXTS are sealed to
//! ```
//!
//! Two keys because they do different jobs and cannot be the same object: `pk_shield` is
//! a MiMC image chosen so it is cheap to prove *inside* the AIR, and a hash image cannot
//! perform a key exchange. Both descend from one seed, so a user still backs up one
//! secret.
//!
//! # What is deliberately NOT hidden
//!
//! The ciphertext's presence and size are public, so an observer learns how many outputs
//! a transaction had. Amounts, recipients and the link to any prior note stay hidden.
//! Padding output count to a fixed arity (`N_OUTS`) is what keeps that leak constant
//! rather than proportional to real activity.

use serde::{Deserialize, Serialize};
use winterfell::math::fields::f64::BaseElement;
use winterfell::math::StarkField;

use crate::note_v1::{from_wire, to_wire, NoteError};

/// Domain tag so a note ciphertext can never be confused with another sealed payload.
///
/// `SIGILNT1` is the original 24-byte layout (`magic ‖ value ‖ blinding`), still accepted
/// on decode so every ciphertext sealed before memos existed keeps opening.
const NOTE_MAGIC: &[u8; 8] = b"SIGILNT1";

/// The memo-carrying layout (2026-09-02): `magic ‖ value ‖ blinding ‖ memo_len:u16 ‖
/// memo[MEMO_LEN]`. Every sender now seals THIS layout, memo or not.
const NOTE_MAGIC_V2: &[u8; 8] = b"SIGILNT2";

/// Fixed on-the-wire memo capacity, in bytes. The same 512 Zcash chose, and for the same
/// reason: a memo field is only private if every note carries one of the same size. A
/// variable-length field would tell the whole network which payments had a message and
/// roughly how long it was — the ciphertext's SIZE is public even though its bytes are
/// not (see the module docs). So the memo is always padded to `MEMO_LEN` and the real
/// length rides INSIDE the sealed box, where only the recipient can read it.
pub const MEMO_LEN: usize = 512;

/// Byte length of a v2 plaintext: 8 magic + 8 value + 8 blinding + 2 length + memo.
pub const NOTE_PLAINTEXT_V2_LEN: usize = 8 + 8 + 8 + 2 + MEMO_LEN;

/// Byte length of the legacy v1 plaintext.
const NOTE_PLAINTEXT_V1_LEN: usize = 24;

/// Upper bound the API enforces on one published ciphertext string. A v2 envelope is
/// ~1.3 KB of JSON (hex ciphertext of `NOTE_PLAINTEXT_V2_LEN` + 16-byte tag, an ephemeral
/// key, a nonce); this leaves room for a post-quantum-hybrid envelope without letting a
/// sender attach kilobytes of junk to every output. Not a consensus rule — the state
/// stores whatever the mempool admitted — so it lives at the door, in `sigil-api`.
pub const MAX_NOTE_CIPHERTEXT_LEN: usize = 4096;

/// Errors from sealing or opening a note.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CipherError {
    #[error("bad encryption public key: {0}")]
    BadEncKey(String),
    #[error("malformed note plaintext")]
    MalformedPlaintext,
    #[error("ciphertext is not for us")]
    NotForUs,
    #[error("bad address encoding: {0}")]
    BadAddress(&'static str),
    #[error("memo is {got} bytes; the limit is {limit}")]
    MemoTooLong { got: usize, limit: usize },
    #[error(transparent)]
    Note(#[from] NoteError),
}

/// A private message riding inside a sealed note: up to [`MEMO_LEN`] bytes, always
/// transmitted padded to exactly `MEMO_LEN` so its presence and length leak nothing.
///
/// Bytes, not a `String`, on the wire — but every constructor here takes UTF-8 and
/// [`Memo::text`] gives it back, since a memo is for a human to read.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Memo {
    len: u16,
    bytes: [u8; MEMO_LEN],
}

impl Default for Memo {
    fn default() -> Self {
        Self::empty()
    }
}

impl std::fmt::Debug for Memo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Memo").field("len", &self.len).field("text", &self.text()).finish()
    }
}

impl Memo {
    /// No message. Still occupies `MEMO_LEN` bytes on the wire, by design.
    pub const fn empty() -> Self {
        Self { len: 0, bytes: [0u8; MEMO_LEN] }
    }

    /// A memo from UTF-8 text. Refused, not truncated, past `MEMO_LEN` bytes: silently
    /// cutting a message the sender wrote is worse than telling them it does not fit.
    pub fn from_text(text: &str) -> Result<Self, CipherError> {
        Self::from_bytes(text.as_bytes())
    }

    pub fn from_bytes(b: &[u8]) -> Result<Self, CipherError> {
        if b.len() > MEMO_LEN {
            return Err(CipherError::MemoTooLong { got: b.len(), limit: MEMO_LEN });
        }
        let mut bytes = [0u8; MEMO_LEN];
        bytes[..b.len()].copy_from_slice(b);
        Ok(Self { len: b.len() as u16, bytes })
    }

    /// The unpadded message bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    /// The message as text (lossy for non-UTF-8 bytes, which no client of ours writes).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(self.as_bytes()).into_owned()
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// What a sender transmits to a recipient about one output note.
///
/// `(value, blinding)` — the recipient supplies its own `pk_shield` when reconstructing
/// the commitment, so a ciphertext cannot be used to make the recipient accept a note
/// bound to somebody else's key — plus a [`Memo`] only the recipient can read.
///
/// The memo is NOT bound by the STARK: the circuit proves value conservation and
/// ownership, and a memo is neither. It is authenticated by the AEAD tag, so nobody can
/// alter it in flight, but it is not part of the commitment and changing it changes
/// nothing the chain checks. Treat it exactly as a sealed letter: private, tamper-evident,
/// and outside consensus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotePlaintext {
    pub value: u64,
    pub blinding: BaseElement,
    pub memo: Memo,
}

impl NotePlaintext {
    /// A note with no memo.
    pub const fn new(value: u64, blinding: BaseElement) -> Self {
        Self { value, blinding, memo: Memo::empty() }
    }

    /// Attach a memo. Errors past [`MEMO_LEN`] bytes.
    pub fn with_memo(mut self, text: &str) -> Result<Self, CipherError> {
        self.memo = Memo::from_text(text)?;
        Ok(self)
    }

    /// Always the v2 layout: `NOTE_PLAINTEXT_V2_LEN` bytes, memo or not. Sealing an empty
    /// memo at full width is the point, not waste — see [`MEMO_LEN`].
    fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(NOTE_PLAINTEXT_V2_LEN);
        v.extend_from_slice(NOTE_MAGIC_V2);
        v.extend_from_slice(&self.value.to_le_bytes());
        v.extend_from_slice(&self.blinding.as_int().to_le_bytes());
        v.extend_from_slice(&self.memo.len.to_le_bytes());
        v.extend_from_slice(&self.memo.bytes);
        debug_assert_eq!(v.len(), NOTE_PLAINTEXT_V2_LEN);
        v
    }

    /// The legacy layout, memo-less. Only [`seal_note_compact`] uses it.
    fn encode_v1(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(NOTE_PLAINTEXT_V1_LEN);
        v.extend_from_slice(NOTE_MAGIC);
        v.extend_from_slice(&self.value.to_le_bytes());
        v.extend_from_slice(&self.blinding.as_int().to_le_bytes());
        v
    }

    fn decode(b: &[u8]) -> Result<Self, CipherError> {
        let memo = match (b.len(), b.get(..8)) {
            (NOTE_PLAINTEXT_V1_LEN, Some(m)) if m == NOTE_MAGIC => Memo::empty(),
            (NOTE_PLAINTEXT_V2_LEN, Some(m)) if m == NOTE_MAGIC_V2 => {
                let len = u16::from_le_bytes([b[24], b[25]]) as usize;
                if len > MEMO_LEN {
                    return Err(CipherError::MalformedPlaintext);
                }
                let mut bytes = [0u8; MEMO_LEN];
                bytes.copy_from_slice(&b[26..26 + MEMO_LEN]);
                Memo { len: len as u16, bytes }
            }
            _ => return Err(CipherError::MalformedPlaintext),
        };
        let mut v = [0u8; 8];
        v.copy_from_slice(&b[8..16]);
        let mut r = [0u8; 8];
        r.copy_from_slice(&b[16..24]);
        let raw = u64::from_le_bytes(r);
        // Reject a non-canonical blinding: two encodings of one field element would give
        // the same note two commitments, and a note with two spellings is a headache the
        // nullifier set should never have to reason about.
        let mut wire = [0u8; 32];
        wire[..8].copy_from_slice(&raw.to_le_bytes());
        let blinding = from_wire(&wire)?;
        Ok(Self { value: u64::from_le_bytes(v), blinding, memo })
    }
}

/// v7 (2026-10-12): the 256-bit note's envelope — `SIGILNT7 ‖ value(8) ‖ blinding(32, four
/// LE limbs) ‖ memo_len:u16 ‖ memo[512]`, 562 bytes, always full width like v2. Same X25519
/// sealing as v1/v2, so one delivery key serves both generations; the magic tells a
/// wallet trial-decrypting a mixed pool which opener to run.
const NOTE_MAGIC_V7: &[u8; 8] = b"SIGILNT7";
pub const NOTE_PLAINTEXT_V7_LEN: usize = 8 + 8 + 32 + 2 + MEMO_LEN;

#[derive(Clone)]
pub struct NotePlaintextV7 {
    pub value: u64,
    pub blinding: crate::note_v7::Digest4,
    pub memo: Memo,
}

impl NotePlaintextV7 {
    pub const fn new(value: u64, blinding: crate::note_v7::Digest4) -> Self {
        Self { value, blinding, memo: Memo::empty() }
    }
    pub fn with_memo(mut self, text: &str) -> Result<Self, CipherError> {
        self.memo = Memo::from_text(text)?;
        Ok(self)
    }
    fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(NOTE_PLAINTEXT_V7_LEN);
        v.extend_from_slice(NOTE_MAGIC_V7);
        v.extend_from_slice(&self.value.to_le_bytes());
        v.extend_from_slice(&self.blinding.to_wire());
        v.extend_from_slice(&self.memo.len.to_le_bytes());
        v.extend_from_slice(&self.memo.bytes);
        debug_assert_eq!(v.len(), NOTE_PLAINTEXT_V7_LEN);
        v
    }
    fn decode(b: &[u8]) -> Result<Self, CipherError> {
        if b.len() != NOTE_PLAINTEXT_V7_LEN || &b[..8] != NOTE_MAGIC_V7 {
            return Err(CipherError::MalformedPlaintext);
        }
        let mut v = [0u8; 8];
        v.copy_from_slice(&b[8..16]);
        let mut w = [0u8; 32];
        w.copy_from_slice(&b[16..48]);
        // strict: a non-canonical limb would be a second spelling of one commitment
        let blinding = crate::note_v7::Digest4::from_wire(&w).map_err(|_| CipherError::MalformedPlaintext)?;
        let len = u16::from_le_bytes([b[48], b[49]]) as usize;
        if len > MEMO_LEN {
            return Err(CipherError::MalformedPlaintext);
        }
        let mut bytes = [0u8; MEMO_LEN];
        bytes.copy_from_slice(&b[50..50 + MEMO_LEN]);
        Ok(Self { value: u64::from_le_bytes(v), blinding, memo: Memo { len: len as u16, bytes } })
    }
}

/// Seal a v7 note to a delivery key (`pk_enc`, hex X25519 — the same key a v1 address carries).
///
/// 2026-10-19 (sigil-g3): sealed WITH a view tag, like every v1 note since 10-16. Before this
/// no v7 envelope carried one on any branch, so a v7 wallet had to trial-decrypt every note on
/// the chain; on g3 an untagged v7 envelope is refused by the chain ([`is_tagged_envelope`]).
pub fn seal_note_v7(plaintext: &NotePlaintextV7, pk_enc_hex: &str) -> Result<NoteCiphertext, CipherError> {
    let pk = flux_swarm_secret::parse_pubkey_hex(pk_enc_hex).map_err(|e| CipherError::BadEncKey(e.to_string()))?;
    seal_tagged(&plaintext.encode(), &pk)
}

/// Try to open a v7 ciphertext with our identity. `NotForUs` for anything not ours, and
/// `MalformedPlaintext` for a v1/v2 envelope that IS ours (so a scanner can fall back).
pub fn try_open_note_v7(ct: &NoteCiphertext, id: &flux_swarm_secret::SecretIdentity) -> Result<NotePlaintextV7, CipherError> {
    // A tagged note that is not ours is ruled out before the AEAD, as in `try_open_note`.
    if let Some(view) = tag_view(&ct.0) {
        if !may_be_ours(&view, id) {
            return Err(CipherError::NotForUs);
        }
    }
    let env: flux_swarm_secret::SealedEnvelope = serde_json::from_str(&ct.0).map_err(|_| CipherError::NotForUs)?;
    let pt = flux_swarm_secret::open(&env, id).map_err(|_| CipherError::NotForUs)?;
    NotePlaintextV7::decode(&pt)
}

/// A published, opaque note ciphertext. Serialized as the sealed envelope's JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteCiphertext(pub String);

/// A shielded address: the circuit key plus the encryption key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShieldedAddress {
    /// Hex of the wire-encoded `pk_shield` field element.
    pub pk_shield: String,
    /// Hex X25519 public key.
    pub pk_enc: String,
}

impl ShieldedAddress {
    pub fn new(pk_shield: BaseElement, pk_enc_hex: &str) -> Self {
        Self { pk_shield: hex::encode(to_wire(pk_shield)), pk_enc: pk_enc_hex.to_string() }
    }

    /// The circuit key a payer must bind an output note to.
    pub fn shield_key(&self) -> Result<BaseElement, CipherError> {
        let v = hex::decode(&self.pk_shield)
            .map_err(|_| CipherError::BadAddress("pk_shield must be hex"))?;
        if v.len() != 32 {
            return Err(CipherError::BadAddress("pk_shield must be 32 bytes"));
        }
        let mut w = [0u8; 32];
        w.copy_from_slice(&v);
        Ok(from_wire(&w)?)
    }

    /// A single copy-pasteable string, `sigil1s:<pk_shield>:<pk_enc>`.
    pub fn encode(&self) -> String {
        format!("sigil1s:{}:{}", self.pk_shield, self.pk_enc)
    }

    pub fn decode(s: &str) -> Result<Self, CipherError> {
        let mut parts = s.split(':');
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some("sigil1s"), Some(a), Some(b), None) if !a.is_empty() && !b.is_empty() => {
                Ok(Self { pk_shield: a.to_string(), pk_enc: b.to_string() })
            }
            _ => Err(CipherError::BadAddress("expected sigil1s:<pk_shield>:<pk_enc>")),
        }
    }
}

/// Seal a note's `(value, blinding)` to a recipient address.
///
/// Sealed-box style: anyone may seal to an address, and only the holder of the matching
/// secret can open it. The sender cannot open its own ciphertext afterwards — the
/// ephemeral key is consumed — which is a property worth having, not a limitation: a
/// sender who kept it could later prove to a third party what it paid.
pub fn seal_note(
    plaintext: &NotePlaintext,
    recipient: &ShieldedAddress,
) -> Result<NoteCiphertext, CipherError> {
    let pk = flux_swarm_secret::parse_pubkey_hex(&recipient.pk_enc)
        .map_err(|e| CipherError::BadEncKey(e.to_string()))?;
    seal_tagged(&plaintext.encode(), &pk)
}

/// Seal a note in the legacy 24-byte layout — for outputs that are NOT messages.
///
/// Shielded coinbase seals one ciphertext per registered payee per BLOCK, and its preimage
/// is publicly derivable anyway (`coinbase_blinding`). Padding those to `MEMO_LEN` would
/// grow chain state by ~1 KB per block per miner for a field nobody could ever put a
/// message in. Payments (`seal_note`) always pay the padding; this path refuses a memo so
/// it cannot be misused to send an unpadded — and therefore size-fingerprinted — message.
pub fn seal_note_compact(
    plaintext: &NotePlaintext,
    recipient: &ShieldedAddress,
) -> Result<NoteCiphertext, CipherError> {
    if !plaintext.memo.is_empty() {
        return Err(CipherError::MemoTooLong { got: plaintext.memo.len(), limit: 0 });
    }
    let pk = flux_swarm_secret::parse_pubkey_hex(&recipient.pk_enc)
        .map_err(|e| CipherError::BadEncKey(e.to_string()))?;
    seal_tagged(&plaintext.encode_v1(), &pk)
}

/// Seal, and carry a one-byte VIEW TAG as `"vt"` beside the envelope's own fields.
///
/// 1M-users step 2 (2026-10-16). Finding your payments used to mean downloading every
/// ciphertext (~1.2 KB each) and trial-decrypting it. With the tag a wallet downloads
/// only `(eph_pk, vt)` per note — 33 bytes, [`tag_view`] — and fetches a full ciphertext
/// only when the tag matches, which a stranger's note does 1 time in 256. The tag is a
/// separate KDF output of the same X25519 secret (`flux_swarm_secret::seal_with_view_tag`),
/// so it says nothing about the AEAD key, and to anyone without the recipient's key it is
/// a uniform byte. An older reader ignores the unknown field and opens the note as before;
/// the ciphertext is opaque to consensus (any string the door admits is stored as is).
fn seal_tagged(pt: &[u8], pk: &flux_swarm_secret::X25519PublicKey) -> Result<NoteCiphertext, CipherError> {
    let (env, tag) = flux_swarm_secret::seal_with_view_tag(pt, pk);
    let json = serde_json::to_string(&env).map_err(|_| CipherError::MalformedPlaintext)?;
    // `{"v":1,…,"ct":"…"}` → `{"v":1,…,"ct":"…","vt":"ab"}`: the envelope's own bytes and
    // field order untouched, one field appended.
    let body = json.strip_suffix('}').ok_or(CipherError::MalformedPlaintext)?;
    Ok(NoteCiphertext(format!("{body},\"vt\":\"{tag:02x}\"}}")))
}

/// What a scanner needs from one published ciphertext: its one-time key and, when the
/// sender attached one, its view tag. 33 bytes instead of the whole envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TagView {
    pub eph_pk: [u8; 32],
    pub vt: Option<u8>,
}

#[derive(Deserialize)]
struct TagFields {
    eph_pk: String,
    #[serde(default)]
    vt: Option<String>,
}

/// Read `(eph_pk, vt)` out of a published ciphertext. `None` for anything that is not an
/// X25519 sealed envelope (junk, or a future suite) — a scanner then falls back to a full
/// trial-decrypt, exactly as it does for an untagged note.
pub fn tag_view(ct: &str) -> Option<TagView> {
    let f: TagFields = serde_json::from_str(ct).ok()?;
    let k = hex::decode(f.eph_pk).ok()?;
    let eph_pk: [u8; 32] = k.as_slice().try_into().ok()?;
    let vt = match f.vt {
        None => None,
        Some(h) => {
            let b = hex::decode(h).ok()?;
            if b.len() != 1 {
                return None;
            }
            Some(b[0])
        }
    };
    Some(TagView { eph_pk, vt })
}

/// The sigil-g3 chain rule (2026-10-19, Viktor: "chain rule"): is `ct` a sealed X25519
/// envelope that carries a view tag? The chain cannot check the tag's VALUE — only the
/// recipient can derive it — so it checks that one is there, is one byte, and that the rest
/// parses as an envelope. A sender who writes a wrong tag only hides the note from its own
/// recipient, which it could do by writing garbage anyway.
pub fn is_tagged_envelope(ct: &str) -> bool {
    matches!(tag_view(ct), Some(TagView { vt: Some(_), .. }))
        && serde_json::from_str::<flux_swarm_secret::SealedEnvelope>(ct).is_ok()
}

/// Could this note be ours? `false` only for a TAGGED note whose tag is not the one our
/// key derives — then it is certainly not ours. Untagged notes (sealed before view tags)
/// always answer `true`: only the full trial-decrypt can tell.
pub fn may_be_ours(view: &TagView, id: &flux_swarm_secret::SecretIdentity) -> bool {
    match view.vt {
        None => true,
        Some(t) => flux_swarm_secret::view_tag(id, &flux_swarm_secret::X25519PublicKey::from(view.eph_pk)) == t,
    }
}

/// Try to open a ciphertext with our encryption identity.
///
/// Returns `NotForUs` for anything not addressed to us — which is the common case and
/// must stay cheap, since a wallet runs this over every ciphertext in every block. The
/// AEAD tag does the work: a wrong key cannot produce a valid tag, so a successful open
/// is proof the note is ours. There is no separate "is this mine?" marker to leak.
pub fn try_open_note(
    ct: &NoteCiphertext,
    id: &flux_swarm_secret::SecretIdentity,
) -> Result<NotePlaintext, CipherError> {
    // A tagged note that is not ours is ruled out before the AEAD (one X25519 either way).
    if let Some(view) = tag_view(&ct.0) {
        if !may_be_ours(&view, id) {
            return Err(CipherError::NotForUs);
        }
    }
    let env: flux_swarm_secret::SealedEnvelope =
        serde_json::from_str(&ct.0).map_err(|_| CipherError::NotForUs)?;
    let pt = flux_swarm_secret::open(&env, id).map_err(|_| CipherError::NotForUs)?;
    NotePlaintext::decode(&pt)
}

/// Derive a deterministic X25519 identity from a wallet seed.
///
/// Domain-separated from the spend key so the two secrets are independent: compromising
/// the viewing side must not hand over the ability to spend.
pub fn enc_identity_from_seed(seed: &[u8; 32]) -> flux_swarm_secret::SecretIdentity {
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-shielded-enc-key-v1");
    h.update(seed);
    flux_swarm_secret::SecretIdentity::from_sk_bytes(*h.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(id: &flux_swarm_secret::SecretIdentity, pk_shield: u64) -> ShieldedAddress {
        ShieldedAddress::new(BaseElement::new(pk_shield), &id.public_hex())
    }

    /// THE RECEIVING GATE: a sealed note opens for its recipient and yields the exact
    /// preimage needed to reconstruct — and later spend — the note.
    #[test]
    fn a_sealed_note_opens_for_its_recipient() {
        let bob = enc_identity_from_seed(&[0xB0; 32]);
        let bob_addr = addr(&bob, 0xBEEF);
        let pt = NotePlaintext::new(4_242, BaseElement::new(0x1234_5678));

        let ct = seal_note(&pt, &bob_addr).expect("seal");
        let got = try_open_note(&ct, &bob).expect("bob must be able to open his own note");
        assert_eq!(got, pt, "the recovered preimage must be exact");
    }

    /// THE PRIVACY PROPERTY: nobody else can open it. This is what lets a wallet trial-
    /// decrypt every ciphertext on the chain without learning anything about other people's.
    #[test]
    fn nobody_else_can_open_it() {
        let bob = enc_identity_from_seed(&[0xB0; 32]);
        let eve = enc_identity_from_seed(&[0xE7; 32]);
        let pt = NotePlaintext::new(999, BaseElement::new(7));
        let ct = seal_note(&pt, &addr(&bob, 1)).expect("seal");

        assert_eq!(
            try_open_note(&ct, &eve),
            Err(CipherError::NotForUs),
            "SECURITY: a non-recipient must not recover the note preimage"
        );
    }

    /// A tampered ciphertext must not open — the AEAD tag is what makes a successful open
    /// mean something.
    #[test]
    fn tampering_breaks_the_open() {
        let bob = enc_identity_from_seed(&[0xB0; 32]);
        let pt = NotePlaintext::new(5, BaseElement::new(6));
        let ct = seal_note(&pt, &addr(&bob, 1)).expect("seal");

        let mut env: serde_json::Value = serde_json::from_str(&ct.0).unwrap();
        let mut hexct = env["ct"].as_str().unwrap().to_string();
        // flip one hex nibble of the ciphertext body
        let c = hexct.remove(0);
        hexct.insert(0, if c == 'a' { 'b' } else { 'a' });
        env["ct"] = serde_json::Value::String(hexct);
        let tampered = NoteCiphertext(env.to_string());

        assert_eq!(
            try_open_note(&tampered, &bob),
            Err(CipherError::NotForUs),
            "SECURITY: a tampered ciphertext must not open"
        );
    }

    /// Sealing twice must produce different ciphertexts, or equal payments would be
    /// linkable by comparing bytes.
    #[test]
    fn sealing_is_randomized() {
        let bob = enc_identity_from_seed(&[0xB0; 32]);
        let a = addr(&bob, 1);
        let pt = NotePlaintext::new(100, BaseElement::new(1));
        let c1 = seal_note(&pt, &a).expect("seal");
        let c2 = seal_note(&pt, &a).expect("seal");
        assert_ne!(c1, c2, "PRIVACY: identical notes must not produce identical ciphertexts");
        // ...and both still open to the same plaintext.
        assert_eq!(try_open_note(&c1, &bob).unwrap(), pt);
        assert_eq!(try_open_note(&c2, &bob).unwrap(), pt);
    }

    #[test]
    fn address_round_trips_and_rejects_junk() {
        let bob = enc_identity_from_seed(&[0xB0; 32]);
        let a = addr(&bob, 0xABCD);
        let s = a.encode();
        assert!(s.starts_with("sigil1s:"));
        let back = ShieldedAddress::decode(&s).expect("round-trip");
        assert_eq!(back, a);
        assert_eq!(back.shield_key().unwrap(), BaseElement::new(0xABCD));

        for junk in ["", "sigil1s:", "nope:a:b", "sigil1s:a:b:c", "sigil1s::b"] {
            assert!(ShieldedAddress::decode(junk).is_err(), "must reject {junk:?}");
        }
    }

    /// A malformed plaintext must not be accepted as a note — an attacker who could make
    /// a wallet accept junk could make it track a note that does not exist.
    #[test]
    fn malformed_plaintext_is_rejected() {
        assert_eq!(NotePlaintext::decode(&[]).unwrap_err(), CipherError::MalformedPlaintext);
        assert_eq!(NotePlaintext::decode(&[0u8; 24]).unwrap_err(), CipherError::MalformedPlaintext);
        assert_eq!(
            NotePlaintext::decode(&[0u8; NOTE_PLAINTEXT_V2_LEN]).unwrap_err(),
            CipherError::MalformedPlaintext
        );
        let mut wrong_magic = NotePlaintext::new(1, BaseElement::new(1)).encode();
        wrong_magic[0] = b'X';
        assert_eq!(
            NotePlaintext::decode(&wrong_magic).unwrap_err(),
            CipherError::MalformedPlaintext
        );
    }

    /// The encryption key must be domain-separated from the seed itself, and deterministic.
    #[test]
    fn enc_identity_is_deterministic_and_separated() {
        let a = enc_identity_from_seed(&[9u8; 32]);
        let b = enc_identity_from_seed(&[9u8; 32]);
        let c = enc_identity_from_seed(&[8u8; 32]);
        assert_eq!(a.public_hex(), b.public_hex(), "same seed ⇒ same identity");
        assert_ne!(a.public_hex(), c.public_hex(), "different seeds ⇒ different identities");
        assert_ne!(
            a.to_sk_bytes(),
            [9u8; 32],
            "the encryption secret must not BE the wallet seed"
        );
    }
    /// THE MEMO GATE: a message sealed with the note comes back byte-exact to the recipient,
    /// and to nobody else.
    #[test]
    fn a_memo_rides_inside_the_sealed_note() {
        let bob = enc_identity_from_seed(&[0xB0; 32]);
        let eve = enc_identity_from_seed(&[0xE7; 32]);
        let pt = NotePlaintext::new(4_242, BaseElement::new(0x1234_5678))
            .with_memo("Lit one for you, Viktor. — Rocky ☕")
            .expect("fits");
        let ct = seal_note(&pt, &addr(&bob, 0xBEEF)).expect("seal");
        let got = try_open_note(&ct, &bob).expect("bob opens");
        assert_eq!(got, pt);
        assert_eq!(got.memo.text(), "Lit one for you, Viktor. — Rocky ☕");
        assert_eq!(try_open_note(&ct, &eve), Err(CipherError::NotForUs));
    }

    /// PRIVACY: the ciphertext is the same size whether the memo is empty, short, or full.
    /// If this ever fails, the network can see who is talking and how much.
    #[test]
    fn memo_length_does_not_change_ciphertext_size() {
        let bob = enc_identity_from_seed(&[0xB0; 32]);
        let a = addr(&bob, 1);
        let base = NotePlaintext::new(1, BaseElement::new(1));
        let none = seal_note(&base, &a).unwrap().0.len();
        let short = seal_note(&base.with_memo("hi").unwrap(), &a).unwrap().0.len();
        let full = seal_note(&base.with_memo(&"x".repeat(MEMO_LEN)).unwrap(), &a).unwrap().0.len();
        assert_eq!(none, short, "an empty memo must not be distinguishable by size");
        assert_eq!(short, full, "a full memo must not be distinguishable by size");
        assert!(none < MAX_NOTE_CIPHERTEXT_LEN, "the API bound must admit every honest ciphertext ({none} >= {MAX_NOTE_CIPHERTEXT_LEN})");
    }

    /// A memo past the cap is refused, never truncated.
    #[test]
    fn oversized_memo_is_refused_not_truncated() {
        let err = NotePlaintext::new(1, BaseElement::new(1))
            .with_memo(&"y".repeat(MEMO_LEN + 1))
            .unwrap_err();
        assert_eq!(err, CipherError::MemoTooLong { got: MEMO_LEN + 1, limit: MEMO_LEN });
        // exactly at the cap is fine
        assert!(NotePlaintext::new(1, BaseElement::new(1)).with_memo(&"y".repeat(MEMO_LEN)).is_ok());
    }

    /// COMPATIBILITY: a ciphertext sealed by a pre-memo client (24-byte `SIGILNT1`
    /// plaintext) still opens, with an empty memo. Every note delivered before today is
    /// one of these.
    #[test]
    fn legacy_v1_plaintext_still_decodes() {
        let mut v1 = Vec::with_capacity(24);
        v1.extend_from_slice(NOTE_MAGIC);
        v1.extend_from_slice(&777u64.to_le_bytes());
        v1.extend_from_slice(&BaseElement::new(0xAB).as_int().to_le_bytes());
        let pt = NotePlaintext::decode(&v1).expect("v1 layout must still decode");
        assert_eq!(pt, NotePlaintext::new(777, BaseElement::new(0xAB)));
        assert!(pt.memo.is_empty());
    }

    /// The compact (coinbase) seal opens like any other, is measurably smaller, and refuses
    /// to carry a memo.
    #[test]
    fn compact_seal_is_smaller_and_memo_free() {
        let bob = enc_identity_from_seed(&[0xB0; 32]);
        let a = addr(&bob, 1);
        let pt = NotePlaintext::new(31, BaseElement::new(9));
        let compact = seal_note_compact(&pt, &a).expect("seal");
        let padded = seal_note(&pt, &a).expect("seal");
        assert!(compact.0.len() < padded.0.len());
        assert_eq!(try_open_note(&compact, &bob).unwrap(), pt);
        let with_memo = pt.with_memo("x").unwrap();
        assert_eq!(
            seal_note_compact(&with_memo, &a).unwrap_err(),
            CipherError::MemoTooLong { got: 1, limit: 0 }
        );
    }

    /// A v2 plaintext claiming more memo than fits is malformed, not silently clamped.
    #[test]
    fn v2_with_lying_length_is_rejected() {
        let mut b = NotePlaintext::new(1, BaseElement::new(1)).encode();
        b[24..26].copy_from_slice(&((MEMO_LEN as u16) + 1).to_le_bytes());
        assert_eq!(NotePlaintext::decode(&b).unwrap_err(), CipherError::MalformedPlaintext);
    }

    #[test]
    fn a_sealed_note_carries_a_view_tag_its_recipient_matches() {
        let alice = enc_identity_from_seed(&[1; 32]);
        let bob = enc_identity_from_seed(&[2; 32]);
        let ct = seal_note(&NotePlaintext::new(42, BaseElement::new(7)), &addr(&alice, 1)).unwrap();
        let v = tag_view(&ct.0).expect("tagged envelope");
        assert!(v.vt.is_some());
        assert!(may_be_ours(&v, &alice));
        assert_eq!(try_open_note(&ct, &alice).unwrap().value, 42);
        // Bob: either the tag rules it out or the AEAD does — never opened.
        assert_eq!(try_open_note(&ct, &bob), Err(CipherError::NotForUs));
    }

    #[test]
    fn an_old_reader_opens_a_tagged_note_and_a_new_reader_opens_an_untagged_one() {
        let alice = enc_identity_from_seed(&[3; 32]);
        let pt = NotePlaintext::new(9, BaseElement::new(11));
        // tagged → the envelope parser every released wallet uses (unknown field ignored)
        let tagged = seal_note(&pt, &addr(&alice, 1)).unwrap();
        let env: flux_swarm_secret::SealedEnvelope = serde_json::from_str(&tagged.0).unwrap();
        assert!(flux_swarm_secret::open(&env, &alice).is_ok());
        // untagged (sealed before 2026-10-16) → still opens, and the scanner must fetch it
        let pk = flux_swarm_secret::parse_pubkey_hex(&alice.public_hex()).unwrap();
        let old = NoteCiphertext(serde_json::to_string(&flux_swarm_secret::seal(&pt.encode(), &pk)).unwrap());
        let v = tag_view(&old.0).unwrap();
        assert_eq!(v.vt, None);
        assert!(may_be_ours(&v, &enc_identity_from_seed(&[4; 32])), "untagged = cannot rule out");
        assert_eq!(try_open_note(&old, &alice).unwrap().value, 9);
    }

    #[test]
    fn the_tag_adds_ten_bytes_to_the_envelope() {
        let alice = enc_identity_from_seed(&[5; 32]);
        let pt = NotePlaintext::new(1, BaseElement::new(1));
        let pk = flux_swarm_secret::parse_pubkey_hex(&alice.public_hex()).unwrap();
        let untagged = serde_json::to_string(&flux_swarm_secret::seal(&pt.encode(), &pk)).unwrap();
        let tagged = seal_note(&pt, &addr(&alice, 1)).unwrap().0;
        assert_eq!(tagged.len(), untagged.len() + 10, r#"exactly ,"vt":"xx""#);
        assert!(tagged.len() <= MAX_NOTE_CIPHERTEXT_LEN);
        assert!(tag_view("not json").is_none());
        assert!(tag_view(r#"{"eph_pk":"00","ct":""}"#).is_none(), "short key refused");
        assert!(tag_view(&format!(r#"{{"eph_pk":"{}","vt":"abcd"}}"#, "00".repeat(32))).is_none(), "2-byte tag refused");
    }
}

/// sigil-g3 (2026-10-19): every v7 envelope carries a view tag, and the chain rule's check.
#[cfg(test)]
mod v7_view_tag_tests {
    use super::*;

    fn blinding() -> crate::note_v7::Digest4 {
        let mut w = [0u8; 32];
        w[0] = 7;
        w[8] = 9;
        w[16] = 11;
        w[24] = 13;
        crate::note_v7::Digest4::from_wire(&w).expect("canonical limbs")
    }

    #[test]
    fn a_v7_note_is_sealed_with_a_view_tag_and_passes_the_chain_rule() {
        let bob = enc_identity_from_seed(&[0xB0; 32]);
        let pt = NotePlaintextV7::new(4_242, blinding());
        let ct = seal_note_v7(&pt, &bob.public_hex()).expect("seal");
        let view = tag_view(&ct.0).expect("an X25519 envelope");
        assert!(view.vt.is_some(), "every v7 envelope must carry a view tag");
        assert!(may_be_ours(&view, &bob), "the tag must match the recipient's key");
        assert!(is_tagged_envelope(&ct.0), "a tagged v7 envelope passes the g3 chain rule");
        let got = try_open_note_v7(&ct, &bob).expect("bob opens his note");
        assert_eq!(got.value, pt.value);
        assert_eq!(got.blinding.to_wire(), pt.blinding.to_wire());
    }

    #[test]
    fn a_stranger_is_ruled_out_and_cannot_open_it() {
        let bob = enc_identity_from_seed(&[0xB0; 32]);
        let eve = enc_identity_from_seed(&[0xE7; 32]);
        let ct = seal_note_v7(&NotePlaintextV7::new(1, blinding()), &bob.public_hex()).expect("seal");
        assert!(matches!(try_open_note_v7(&ct, &eve), Err(CipherError::NotForUs)));
    }

    #[test]
    fn the_chain_rule_refuses_untagged_envelopes_and_junk() {
        let bob = enc_identity_from_seed(&[0xB0; 32]);
        let pk = flux_swarm_secret::parse_pubkey_hex(&bob.public_hex()).unwrap();
        let untagged = serde_json::to_string(&flux_swarm_secret::seal(b"note", &pk)).unwrap();
        assert!(!is_tagged_envelope(&untagged), "an envelope without a tag is refused on g3");
        assert!(!is_tagged_envelope("not json"));
        assert!(!is_tagged_envelope(r#"{"eph_pk":"00","vt":"ab"}"#), "a tag on a non-envelope is refused");
        let tagged = seal_note_v7(&NotePlaintextV7::new(5, blinding()), &bob.public_hex()).unwrap().0;
        let two_byte_tag = tagged.replacen("\"vt\":\"", "\"vt\":\"00", 1);
        assert!(!is_tagged_envelope(&two_byte_tag), "a tag must be exactly one byte");
    }
}
