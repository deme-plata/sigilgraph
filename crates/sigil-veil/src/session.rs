//! Veil Sessions — paying for remote AI with one ordinary shielded payment.
//!
//! zkAPI needed a new Groth16 circuit, a trusted setup and a vault contract. SIGIL
//! already has the hard part: a shielded payment whose spend proof is a STARK
//! (post-quantum, transparent) and whose sealed note carries a 512-byte memo that
//! only the recipient can open. So a session is bought like this:
//!
//! ```text
//!   user                                   provider (a node with a GPU)
//!   ────                                   ────────────────────────────
//!   key  ← 32 random bytes
//!   shielded send, fixed denomination ───► trial-decrypts its notes, finds the memo
//!     memo = VEIL1 ‖ class ‖ key             credits session_id = BLAKE3(key)
//!   request ‖ counter ‖ MAC_key(…)  ──────► checks MAC, counter, credit; serves
//! ```
//!
//! What this buys, and what it does not:
//! * the CHAIN sees one shielded transaction — no amount, no recipient, no sender;
//! * the PROVIDER learns "someone paid one class-N session" and nothing about who:
//!   the spent note's nullifier is unlinkable to where that note came from;
//! * requests WITHIN a session are linkable to each other (one key) — zkAPI's
//!   per-request nullifiers are stronger there; a session here is meant to be one
//!   conversation, which re-links itself through its own context anyway (zkAPI's
//!   own write-up says so);
//! * fixed denominations only — a unique amount would be a fingerprint;
//! * the memo is sealed with X25519 (not post-quantum). A future quantum attacker
//!   who breaks the PROVIDER's published key learns session keys — which the
//!   provider already knows — but still not who paid: the spend proof is the STARK.
//!
//! This module is pure (no chain, no network, no clock, no RNG — the caller
//! supplies randomness), so it can be tested exhaustively and built for wasm.

use std::collections::BTreeMap;

/// Memo magic + version.
pub const MAGIC: &[u8; 5] = b"VEIL1";

/// Fixed session denominations: (class id, price in glyphs, credits granted).
/// 1 SIGIL = 10^10 glyphs. A credit is the provider's unit (≈ 1k tokens).
/// Few classes on purpose: every extra class splits the anonymity set.
pub const CLASSES: &[(u8, u128, u64)] = &[
    (1, 1_000_000_000, 100),     // 0.1 SIGIL → 100 credits
    (2, 10_000_000_000, 1_000),  // 1 SIGIL   → 1 000 credits
];

pub fn class_of(id: u8) -> Option<(u128, u64)> {
    CLASSES.iter().find(|c| c.0 == id).map(|c| (c.1, c.2))
}

/// What the user puts in the memo of the paying note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub class: u8,
    pub key: [u8; 32],
}

impl Grant {
    pub fn session_id(&self) -> [u8; 32] {
        *blake3::hash(&self.key).as_bytes()
    }
}

/// Memo bytes for a grant (38 bytes; the note layer pads every memo to 512).
pub fn encode_memo(g: &Grant) -> Vec<u8> {
    let mut v = Vec::with_capacity(38);
    v.extend_from_slice(MAGIC);
    v.push(g.class);
    v.extend_from_slice(&g.key);
    v
}

/// Parse a memo; `None` for anything that is not a well-formed Veil grant of a
/// known class — an ordinary payment memo is simply not a session.
pub fn decode_memo(m: &[u8]) -> Option<Grant> {
    if m.len() < 38 || &m[..5] != MAGIC {
        return None;
    }
    // the note layer zero-pads; anything non-zero after the grant is not ours
    if m[38..].iter().any(|&b| b != 0) {
        return None;
    }
    let class = m[5];
    class_of(class)?;
    let mut key = [0u8; 32];
    key.copy_from_slice(&m[6..38]);
    Some(Grant { class, key })
}

/// MAC over one request: keyed BLAKE3 of `counter ‖ body`.
pub fn request_mac(key: &[u8; 32], counter: u64, body: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new_keyed(key);
    h.update(&counter.to_le_bytes());
    h.update(body);
    *h.finalize().as_bytes()
}

/// Client side: a session bought with one grant.
#[derive(Debug, Clone)]
pub struct ClientSession {
    pub grant: Grant,
    counter: u64,
}

impl ClientSession {
    /// `random` must come from a CSPRNG (OS / crypto.getRandomValues).
    pub fn new(class: u8, random: [u8; 32]) -> Option<Self> {
        class_of(class)?;
        Some(ClientSession { grant: Grant { class, key: random }, counter: 0 })
    }
    pub fn memo(&self) -> Vec<u8> {
        encode_memo(&self.grant)
    }
    /// Authenticate the next request: (session_id hex, counter, mac hex).
    pub fn sign(&mut self, body: &[u8]) -> (String, u64, String) {
        self.counter += 1;
        let mac = request_mac(&self.grant.key, self.counter, body);
        (hex(&self.grant.session_id()), self.counter, hex(&mac))
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// Why the provider refused a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reject {
    UnknownSession,
    BadMac,
    /// counter not strictly greater than the last one accepted — a replay
    Replay,
    /// not enough credits left; `left` is what remains
    Exhausted { left: u64 },
    Malformed,
}

#[derive(Debug, Clone)]
struct Entry {
    key: [u8; 32],
    credits: u64,
    last_counter: u64,
}

/// Provider side: credits per session, nothing else. The ledger never learns
/// (and has no field for) who paid.
#[derive(Debug, Clone, Default)]
pub struct ProviderLedger {
    sessions: BTreeMap<[u8; 32], Entry>,
    /// note commitments already credited — the same paying note must not
    /// credit twice if the provider rescans a block
    credited_notes: std::collections::BTreeSet<[u8; 32]>,
}

impl ProviderLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Credit a session from a FINAL incoming note: its commitment, the value
    /// it carried, and its memo. Returns the credits granted, or None when the
    /// memo isn't a grant, the value isn't exactly the class price, or this
    /// note was already credited.
    pub fn credit(&mut self, note_cm: [u8; 32], value_glyphs: u128, memo: &[u8]) -> Option<u64> {
        let g = decode_memo(memo)?;
        let (price, credits) = class_of(g.class)?;
        if value_glyphs != price || !self.credited_notes.insert(note_cm) {
            return None;
        }
        let e = self.sessions.entry(g.session_id()).or_insert(Entry { key: g.key, credits: 0, last_counter: 0 });
        e.credits += credits;
        Some(credits)
    }

    /// Authorise one request costing `cost` credits. On success returns the
    /// credits left; on refusal nothing changes.
    pub fn authorize(&mut self, session_hex: &str, counter: u64, mac_hex: &str, body: &[u8], cost: u64) -> Result<u64, Reject> {
        let (Some(sid), Some(mac)) = (unhex32(session_hex), unhex32(mac_hex)) else { return Err(Reject::Malformed) };
        let e = self.sessions.get_mut(&sid).ok_or(Reject::UnknownSession)?;
        // constant-time compare: a MAC check must not leak how many bytes matched
        let want = request_mac(&e.key, counter, body);
        let diff = want.iter().zip(mac.iter()).fold(0u8, |a, (x, y)| a | (x ^ y));
        if diff != 0 {
            return Err(Reject::BadMac);
        }
        if counter <= e.last_counter {
            return Err(Reject::Replay);
        }
        if e.credits < cost {
            return Err(Reject::Exhausted { left: e.credits });
        }
        e.credits -= cost;
        e.last_counter = counter;
        Ok(e.credits)
    }

    pub fn credits(&self, session_hex: &str) -> Option<u64> {
        unhex32(session_hex).and_then(|s| self.sessions.get(&s)).map(|e| e.credits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pad(m: Vec<u8>) -> Vec<u8> {
        let mut v = m;
        v.resize(512, 0); // what the note layer does to every memo
        v
    }

    #[test]
    fn memo_roundtrip_through_note_padding() {
        let s = ClientSession::new(2, [7u8; 32]).unwrap();
        let g = decode_memo(&pad(s.memo())).expect("padded memo must decode");
        assert_eq!(g, s.grant);
        assert_eq!(s.memo().len(), 38);
    }

    #[test]
    fn ordinary_memos_and_unknown_classes_are_not_sessions() {
        assert!(decode_memo(&pad(b"thanks for the coffee".to_vec())).is_none());
        let mut m = encode_memo(&Grant { class: 9, key: [1; 32] });
        assert!(decode_memo(&m).is_none(), "unknown class");
        m = encode_memo(&Grant { class: 1, key: [1; 32] });
        m.push(0xFF);
        assert!(decode_memo(&pad(m)).is_none(), "trailing garbage after the grant");
        assert!(ClientSession::new(9, [0; 32]).is_none());
    }

    #[test]
    fn full_flow_credit_then_spend_down() {
        let mut user = ClientSession::new(1, [42u8; 32]).unwrap();
        let mut p = ProviderLedger::new();
        assert_eq!(p.credit([1; 32], 1_000_000_000, &pad(user.memo())), Some(100));
        let (sid, c, mac) = user.sign(b"{\"prompt\":\"[QNK_1] balance?\"}");
        assert_eq!(p.authorize(&sid, c, &mac, b"{\"prompt\":\"[QNK_1] balance?\"}", 30), Ok(70));
        let (sid, c, mac) = user.sign(b"second");
        assert_eq!(p.authorize(&sid, c, &mac, b"second", 80), Err(Reject::Exhausted { left: 70 }));
        assert_eq!(p.credits(&sid), Some(70), "a refusal changes nothing");
    }

    #[test]
    fn wrong_amount_and_double_credit_are_refused() {
        let user = ClientSession::new(1, [3u8; 32]).unwrap();
        let mut p = ProviderLedger::new();
        assert_eq!(p.credit([1; 32], 999, &pad(user.memo())), None, "underpaid");
        assert_eq!(p.credit([1; 32], 1_000_000_000, &pad(user.memo())), Some(100));
        assert_eq!(p.credit([1; 32], 1_000_000_000, &pad(user.memo())), None, "same note rescanned");
        assert_eq!(p.credit([2; 32], 1_000_000_000, &pad(user.memo())), Some(100), "a second note tops up");
    }

    #[test]
    fn replay_tamper_and_forgery_are_refused() {
        let mut user = ClientSession::new(2, [9u8; 32]).unwrap();
        let mut p = ProviderLedger::new();
        p.credit([5; 32], 10_000_000_000, &pad(user.memo())).unwrap();
        let (sid, c, mac) = user.sign(b"hello");
        assert!(p.authorize(&sid, c, &mac, b"hello", 1).is_ok());
        assert_eq!(p.authorize(&sid, c, &mac, b"hello", 1), Err(Reject::Replay));
        let (sid2, c2, mac2) = user.sign(b"hello");
        assert_eq!(p.authorize(&sid2, c2, &mac2, b"HELLO", 1), Err(Reject::BadMac), "body tampered");
        let forged = request_mac(&[0u8; 32], c2, b"hello");
        assert_eq!(p.authorize(&sid2, c2, &hex(&forged), b"hello", 1), Err(Reject::BadMac));
        assert_eq!(p.authorize(&"00".repeat(32), 1, &mac2, b"x", 1), Err(Reject::UnknownSession));
        assert_eq!(p.authorize("zz", 1, &mac2, b"x", 1), Err(Reject::Malformed));
    }

    #[test]
    fn the_ledger_has_nothing_that_identifies_a_payer() {
        // A structural promise, pinned: credit() takes only (commitment, value,
        // memo). There is no sender, address, IP or timestamp parameter to store.
        let user = ClientSession::new(1, [8u8; 32]).unwrap();
        let mut p = ProviderLedger::new();
        p.credit([9; 32], 1_000_000_000, &pad(user.memo()));
        let dump = format!("{p:?}");
        assert!(!dump.contains("qnk") && !dump.contains("addr") && !dump.contains("ip"));
    }
}
