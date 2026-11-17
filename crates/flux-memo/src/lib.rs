//! flux-memo — what a payment memo IS, defined once (2026-11-17, lane sigil-s19-1117).
//!
//! A memo is a short message a sender attaches to a payment. On SIGIL it rides inside the sealed
//! note (`sigil-shield::note_cipher`, 512 bytes, always padded so its length leaks nothing). Until
//! now it was raw bytes copied through six code paths (web, Android, sigil-top, MCP, wasm, Kotlin)
//! with no type, no display rule and no fingerprint. This crate is the one definition.
//!
//! # Where the design comes from
//!
//! * **Quillon Graph** (`q-types` `Transaction::memo: Option<String>`, read 2026-11-17): the memo is
//!   covered by the sender's signature — good — but it is appended UNFRAMED as the last signed field,
//!   it is NOT in the transaction id (two transfers differing only in memo share an id), and no
//!   length cap exists anywhere. Its `temporal_memo` module adds the useful idea: a leading
//!   VERSION BYTE (0x00 plain, 0x01 AES, 0x02 TemporalShield) so the format can grow.
//! * **Zcash ZIP-302**: the same idea without breaking text. A UTF-8 string can never START with a
//!   byte above 0xF4, so a first byte `<= 0xF4` means "this is text" and `0xF5..=0xFF` are free
//!   type tags. That is the rule here, and it is why a text memo stays byte-for-byte what every
//!   SIGIL wallet already seals: old wallets read new text memos, new wallets read old ones.
//!
//! # The encoding (the "body"; the note cipher adds its own length + padding around it)
//!
//! ```text
//!   empty   : 0 bytes                       (a lone 0xF6, Zcash's "no memo", also decodes to Empty)
//!   text    : UTF-8 bytes, first byte <= 0xF4
//!   data    : 0xF5 ‖ kind:u8 ‖ bytes        (application data: a digest, an invoice id, …)
//!   reserved: 0xF7..=0xFF ‖ bytes           (kept and shown as unsupported, never invented)
//! ```
//!
//! # A memo is written by the PAYER
//!
//! Whoever pays you chooses its bytes. So: [`Memo::text`] (what honest wallets call) refuses
//! characters that only serve to mislead — control codes, direction overrides, invisible
//! characters, Unicode tag characters (an invisible channel to AI models) — and
//! [`Memo::display`] turns any such character that DOES arrive (from software that skipped the
//! check) into a visible `⟦U+202E⟧`. Treat memo text as data, never as instructions.

#![forbid(unsafe_code)]

use std::fmt;

/// Hard cap on a memo body, in bytes — the same 512 SIGIL's note cipher reserves (and Zcash chose).
pub const MEMO_CAP: usize = 512;

/// First byte of an application-data memo.
pub const TAG_DATA: u8 = 0xF5;
/// First byte of Zcash's explicit "no memo"; decodes to [`Memo::Empty`].
pub const TAG_NONE: u8 = 0xF6;
/// Highest first byte a UTF-8 string can have; everything above is a type tag.
pub const MAX_TEXT_LEAD: u8 = 0xF4;

/// Well-known `kind` values for [`Memo::Data`]. Free-form otherwise; these are just names.
pub mod kind {
    /// A 32-byte digest anchored by a payment (e.g. `flux_sigil_forge_anchor`).
    pub const DIGEST32: u8 = 0x01;
    /// An invoice / order reference chosen by the payee.
    pub const INVOICE_REF: u8 = 0x02;
}

const DIGEST_CONTEXT: &str = "flux-memo v1 2026-11-17 memo digest";

/// One payment memo.
#[derive(Clone, PartialEq, Eq, Default)]
pub enum Memo {
    /// No message.
    #[default]
    Empty,
    /// Human text (valid UTF-8, non-empty).
    Text(String),
    /// Application data: `kind` says what the bytes mean (see [`kind`]).
    Data { kind: u8, bytes: Vec<u8> },
    /// A first byte that says "text" but bytes that are not UTF-8. Kept exactly, shown lossily.
    BadText(Vec<u8>),
    /// A type tag this version does not know (0xF7..=0xFF). Kept exactly, never interpreted.
    Reserved { tag: u8, bytes: Vec<u8> },
}

/// Why a memo was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoError {
    /// Longer than [`MEMO_CAP`] once encoded. Refused, never truncated: cutting what the sender
    /// wrote is worse than telling them it does not fit.
    TooLong { got: usize, limit: usize },
    /// The text contains a character honest payment software has no reason to send.
    Refused { ch: char, at: usize, why: Hazard },
}

impl fmt::Display for MemoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemoError::TooLong { got, limit } => write!(f, "memo is {got} bytes; the limit is {limit}"),
            MemoError::Refused { ch, at, why } => {
                write!(f, "memo character U+{:04X} at byte {at} is not allowed ({why})", *ch as u32)
            }
        }
    }
}

impl std::error::Error for MemoError {}

/// The kinds of misleading character a memo can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hazard {
    /// C0/C1 control codes, DEL, CR: can rewrite a terminal line or inject escape sequences.
    Control,
    /// Bidi embeddings / overrides / isolates: make text read in a different order than it is.
    Direction,
    /// Zero-width / invisible characters (not ZWJ/ZWNJ, which scripts and emoji need).
    Invisible,
    /// Unicode tag characters U+E0000–E007F: invisible text that AI models still read.
    Tag,
    /// U+2028 / U+2029: line breaks that some renderers honour and others do not.
    Separator,
}

impl fmt::Display for Hazard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Hazard::Control => "control character",
            Hazard::Direction => "text-direction override",
            Hazard::Invisible => "invisible character",
            Hazard::Tag => "invisible tag character",
            Hazard::Separator => "line/paragraph separator",
        })
    }
}

/// The hazard class of one character, or `None` when it is fine to write and show as is.
/// `\n` and `\t` are allowed; ZWJ (U+200D) and ZWNJ (U+200C) are allowed (emoji, Indic and Persian
/// scripts need them); LRM/RLM marks are allowed (right-to-left writing needs them, and they do
/// not reorder anything by themselves).
pub fn hazard(c: char) -> Option<Hazard> {
    let u = c as u32;
    match u {
        0x09 | 0x0A => None,
        0x00..=0x1F | 0x7F..=0x9F => Some(Hazard::Control),
        0x061C | 0x202A..=0x202E | 0x2066..=0x2069 => Some(Hazard::Direction),
        0x200B | 0x2060..=0x2064 | 0xFEFF | 0x180E | 0x115F | 0x1160 | 0x3164 | 0xFFA0 => Some(Hazard::Invisible),
        0xE0000..=0xE007F => Some(Hazard::Tag),
        0x2028 | 0x2029 => Some(Hazard::Separator),
        _ => None,
    }
}

impl Memo {
    /// A text memo, as an honest wallet writes it. Empty text is [`Memo::Empty`]. Refuses text
    /// over [`MEMO_CAP`] bytes and any character [`hazard`] names.
    pub fn text(s: &str) -> Result<Self, MemoError> {
        if s.is_empty() {
            return Ok(Memo::Empty);
        }
        if s.len() > MEMO_CAP {
            return Err(MemoError::TooLong { got: s.len(), limit: MEMO_CAP });
        }
        if let Some((at, ch, why)) = s.char_indices().find_map(|(i, c)| hazard(c).map(|h| (i, c, h))) {
            return Err(MemoError::Refused { ch, at, why });
        }
        Ok(Memo::Text(s.to_owned()))
    }

    /// An application-data memo: `0xF5 ‖ kind ‖ bytes`, at most `MEMO_CAP - 2` bytes of payload.
    pub fn data(kind: u8, bytes: &[u8]) -> Result<Self, MemoError> {
        if bytes.len() + 2 > MEMO_CAP {
            return Err(MemoError::TooLong { got: bytes.len() + 2, limit: MEMO_CAP });
        }
        Ok(Memo::Data { kind, bytes: bytes.to_vec() })
    }

    /// The body bytes (what goes between the note cipher's length prefix and its padding).
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Memo::Empty => Vec::new(),
            Memo::Text(s) => s.as_bytes().to_vec(),
            Memo::BadText(b) => b.clone(),
            Memo::Data { kind, bytes } => {
                let mut v = Vec::with_capacity(2 + bytes.len());
                v.push(TAG_DATA);
                v.push(*kind);
                v.extend_from_slice(bytes);
                v
            }
            Memo::Reserved { tag, bytes } => {
                let mut v = Vec::with_capacity(1 + bytes.len());
                v.push(*tag);
                v.extend_from_slice(bytes);
                v
            }
        }
    }

    /// Read a body. Never fails on content (a memo is shown, not validated, on the way IN — the
    /// sender's software already had its chance); only a body over [`MEMO_CAP`] is an error.
    /// `decode(encode(m)) == m` for every memo this crate can build.
    pub fn decode(body: &[u8]) -> Result<Self, MemoError> {
        if body.len() > MEMO_CAP {
            return Err(MemoError::TooLong { got: body.len(), limit: MEMO_CAP });
        }
        Ok(match body.first() {
            None => Memo::Empty,
            Some(&b) if b <= MAX_TEXT_LEAD => match std::str::from_utf8(body) {
                Ok(s) => Memo::Text(s.to_owned()),
                Err(_) => Memo::BadText(body.to_vec()),
            },
            Some(&TAG_DATA) if body.len() >= 2 => Memo::Data { kind: body[1], bytes: body[2..].to_vec() },
            Some(&TAG_DATA) => Memo::Reserved { tag: TAG_DATA, bytes: Vec::new() },
            Some(&TAG_NONE) if body[1..].iter().all(|&x| x == 0) => Memo::Empty,
            Some(&tag) => Memo::Reserved { tag, bytes: body[1..].to_vec() },
        })
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, Memo::Empty)
    }

    /// `len:u16 LE ‖ body` — the form to put inside anything a sender SIGNS or HASHES. Framing is
    /// what Quillon's browser signature lacks (it appends the memo raw as the last field).
    pub fn framed(&self) -> Vec<u8> {
        let body = self.encode();
        let mut v = Vec::with_capacity(2 + body.len());
        v.extend_from_slice(&(body.len() as u16).to_le_bytes());
        v.extend_from_slice(&body);
        v
    }

    /// A 32-byte fingerprint of the memo (BLAKE3 derive-key over [`Memo::framed`]). A sender can
    /// keep it as a receipt — "I sent THIS memo" — without keeping or revealing the text.
    pub fn digest(&self) -> [u8; 32] {
        blake3::derive_key(DIGEST_CONTEXT, &self.framed())
    }

    /// Every misleading character in a text memo, in order (empty for other kinds).
    pub fn hazards(&self) -> Vec<(usize, char, Hazard)> {
        match self {
            Memo::Text(s) => s.char_indices().filter_map(|(i, c)| hazard(c).map(|h| (i, c, h))).collect(),
            _ => Vec::new(),
        }
    }

    /// What a person (or an agent) should be SHOWN. Text keeps its letters, `\n` and `\t`; every
    /// [`hazard`] character becomes a visible `⟦U+XXXX⟧`, so a reversed or hidden phrase cannot
    /// pass as normal text. Non-text kinds are described, never rendered as text.
    pub fn display(&self) -> String {
        match self {
            Memo::Empty => String::new(),
            Memo::Text(s) => neutralise(s),
            Memo::BadText(b) => format!("⟦not valid UTF-8⟧ {}", neutralise(&String::from_utf8_lossy(b))),
            Memo::Data { kind, bytes } => {
                let shown: String = bytes.iter().take(16).map(|b| format!("{b:02x}")).collect();
                let more = if bytes.len() > 16 { "…" } else { "" };
                format!("⟦data · kind 0x{kind:02x} · {} bytes · {shown}{more}⟧", bytes.len())
            }
            Memo::Reserved { tag, bytes } => format!("⟦unsupported memo type 0x{tag:02x} · {} bytes⟧", bytes.len()),
        }
    }
}

fn neutralise(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if hazard(c).is_some() {
            out.push_str(&format!("⟦U+{:04X}⟧", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

impl fmt::Debug for Memo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Memo({:?})", self.display())
    }
}

impl fmt::Display for Memo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_byte_identical_to_what_sigil_wallets_already_seal() {
        // The note cipher's existing memo = raw UTF-8 bytes. A new text memo must encode to exactly
        // those bytes, and an old body must read back as the same text.
        let s = "Lit one for you, Viktor. — Rocky ☕";
        assert_eq!(Memo::text(s).unwrap().encode(), s.as_bytes());
        assert_eq!(Memo::decode(s.as_bytes()).unwrap(), Memo::Text(s.into()));
        assert_eq!(Memo::text("").unwrap(), Memo::Empty);
        assert_eq!(Memo::decode(&[]).unwrap(), Memo::Empty);
    }

    #[test]
    fn every_kind_round_trips_exactly() {
        let all = [
            Memo::Empty,
            Memo::text("for the coffee\nthanks\t!").unwrap(),
            Memo::text("👨‍👩‍👧 ‌ مرحبا\u{200F}").unwrap(), // ZWJ, ZWNJ, an RLM: allowed
            Memo::data(kind::DIGEST32, &[7u8; 32]).unwrap(),
            Memo::data(kind::INVOICE_REF, b"INV-2026-0042").unwrap(),
            Memo::data(0x00, &[]).unwrap(),
            Memo::BadText(vec![0x41, 0xC3, 0x28]),
            Memo::Reserved { tag: 0xFF, bytes: vec![1, 2, 3] },
        ];
        for m in all {
            assert_eq!(Memo::decode(&m.encode()).unwrap(), m, "{m:?}");
        }
    }

    #[test]
    fn the_first_byte_decides_the_type_and_text_can_never_collide_with_a_tag() {
        // Every valid UTF-8 lead byte is <= 0xF4, so no text memo can be misread as data.
        for c in ['\u{0}', 'A', 'é', '€', '😀', '\u{10FFFF}'] {
            let mut b = [0u8; 4];
            assert!(c.encode_utf8(&mut b).as_bytes()[0] <= MAX_TEXT_LEAD);
        }
        assert_eq!(Memo::decode(&[TAG_NONE]).unwrap(), Memo::Empty);
        assert_eq!(Memo::decode(&[TAG_NONE, 0, 0, 0]).unwrap(), Memo::Empty);
        assert!(matches!(Memo::decode(&[TAG_NONE, 9]).unwrap(), Memo::Reserved { tag: TAG_NONE, .. }));
        assert!(matches!(Memo::decode(&[0xF7, 1]).unwrap(), Memo::Reserved { tag: 0xF7, .. }));
        assert!(matches!(Memo::decode(&[TAG_DATA]).unwrap(), Memo::Reserved { tag: TAG_DATA, .. }));
    }

    #[test]
    fn the_cap_is_refused_not_truncated() {
        let ok = "x".repeat(MEMO_CAP);
        assert!(Memo::text(&ok).is_ok());
        assert_eq!(Memo::text(&(ok.clone() + "y")), Err(MemoError::TooLong { got: MEMO_CAP + 1, limit: MEMO_CAP }));
        assert!(Memo::data(1, &vec![0u8; MEMO_CAP - 2]).is_ok());
        assert!(Memo::data(1, &vec![0u8; MEMO_CAP - 1]).is_err());
        assert!(Memo::decode(&vec![b'a'; MEMO_CAP + 1]).is_err());
        // a multi-byte character at the edge counts in BYTES, like the wire
        assert!(Memo::text(&("x".repeat(MEMO_CAP - 2) + "€")).is_err());
    }

    #[test]
    fn honest_wallets_cannot_write_misleading_characters() {
        let cases = [
            ("pay\u{1B}[2Jnow", Hazard::Control),             // terminal escape
            ("Paid 1 SIGIL\rPaid 1000 SIGIL", Hazard::Control), // carriage return overwrite
            ("invoice \u{202E}fdp.exe", Hazard::Direction),     // right-to-left override
            ("a\u{2067}b\u{2069}", Hazard::Direction),          // isolate
            ("qnk22ac\u{200B}7dd", Hazard::Invisible),          // zero-width space inside an address
            ("thanks\u{E0049}\u{E0067}", Hazard::Tag),          // invisible tag text
            ("line\u{2028}break", Hazard::Separator),
        ];
        for (s, h) in cases {
            match Memo::text(s) {
                Err(MemoError::Refused { why, .. }) => assert_eq!(why, h, "{s:?}"),
                other => panic!("{s:?} should be refused as {h:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn display_makes_anything_that_arrives_visible() {
        // A memo built by software that skipped the check still decodes (a payment is never
        // refused on the way in) — but it is shown with its tricks spelled out.
        let evil = "Refund to qnk\u{202E}cba\u{202C}\u{200B} \u{1B}[31mURGENT";
        let m = Memo::decode(evil.as_bytes()).unwrap();
        assert_eq!(m.hazards().len(), 4);
        let shown = m.display();
        assert_eq!(shown, "Refund to qnk⟦U+202E⟧cba⟦U+202C⟧⟦U+200B⟧ ⟦U+001B⟧[31mURGENT");
        assert!(shown.chars().all(|c| hazard(c).is_none()), "display output carries no hazard");
        assert_eq!(Memo::text("a\nb\tc").unwrap().display(), "a\nb\tc");
        assert!(Memo::data(kind::DIGEST32, &[0xAB; 32]).unwrap().display().starts_with("⟦data · kind 0x01 · 32 bytes · abab"));
        assert_eq!(Memo::Empty.display(), "");
    }

    #[test]
    fn the_digest_is_stable_and_separates_what_quillon_s_signature_conflates() {
        let a = Memo::text("for the coffee").unwrap();
        assert_eq!(a.digest(), Memo::text("for the coffee").unwrap().digest());
        assert_ne!(a.digest(), Memo::text("for the coffee.").unwrap().digest());
        // Text and data with the same bytes are different memos.
        assert_ne!(Memo::Text("\u{1}a".into()).digest(), Memo::data(0x01, b"a").unwrap().digest());
        // Framing: the length is inside, so "memo then more bytes" cannot equal a longer memo.
        assert_eq!(Memo::Empty.framed(), vec![0, 0]);
        assert_eq!(a.framed()[..2], (14u16).to_le_bytes());
        // Pinned, so a change of context string or framing is a deliberate, visible decision.
        let hex: String = Memo::Empty.digest().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex.len(), 64);
        assert_eq!(hex, Memo::decode(&[TAG_NONE]).unwrap().digest().iter().map(|b| format!("{b:02x}")).collect::<String>());
    }
}
