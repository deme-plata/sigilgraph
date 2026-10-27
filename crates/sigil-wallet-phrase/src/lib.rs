//! Recovery words (+ an optional password) → the 32-byte SIGIL wallet secret.
//!
//! One implementation for every place a SIGIL wallet is opened from words: sigilgraph.org
//! (this crate compiled to wasm, `wasm_abi` below), sigil-top, and the phone. Before this crate
//! each client carried its own copy of the recipe and the copies were kept equal by test
//! vectors; now they share the code and the vectors guard the code.
//!
//! # The schemes
//!
//! | scheme | input | secret |
//! |---|---|---|
//! | raw | 64 hex chars (optionally `0x`) | the bytes themselves (mining rigs, pre-10.4.22 sigil-top) |
//! | v1 | fewer than 12 words, or `v1:` | SHA3-256(phrase) |
//! | v2 | 12+ words, or `v2:` / `mm2:` | Argon2id(phrase, "sigilgraph-wallet-v2", t=3, m=16 MiB, p=1) |
//! | **v3** | BIP39 words + optional password, or `v3:` | see below |
//!
//! v1/v2 hash the phrase exactly as typed (only trimmed), which is why a typo there silently
//! opens a different, empty wallet. v3 is standard BIP39 underneath:
//!
//! ```text
//! words    BIP39 English, checksum-valid (12, 15, 18, 21 or 24 words)
//! mnemonic NFKD(words lowercased, single-spaced)                  — BIP39's canonical form
//! seed64   PBKDF2-HMAC-SHA512(mnemonic, "mnemonic" + NFKD(password), 2048 rounds)  — the BIP39 seed
//! secret   Argon2id(seed64, "sigilgraph-wallet-v3", t=3, m=64 MiB, p=4) → 32 bytes
//! ```
//!
//! `seed64` is the seed every BIP39 tool computes from the same words and password, so the words
//! are a real, checksum-protected BIP39 mnemonic. The Argon2id step (RFC 9106's second
//! recommended setting) is what makes a password worth having: someone who finds the paper must
//! still pay 64 MiB × 3 passes of memory-hard work for every password guess, instead of 2,048
//! cheap HMACs. The password is used exactly as typed (BIP39 never trims it); UIs should refuse
//! to CREATE a wallet whose password starts or ends with whitespace
//! ([`password_has_edge_whitespace`]).
//!
//! # Telling v2 from v3
//!
//! v3 words with no password look like twelve words, and 2.9 % of existing v2 phrases happen to
//! be checksum-valid BIP39 (measured 2026-10-27: 2,026 of the 2,159 v2 list words are BIP39
//! words, and 1 in 16 random phrases passes the 4-bit checksum). [`candidates`] therefore returns
//! BOTH wallets in that case, v3 first, and the caller picks: by the address it expects (the
//! wallet page re-asking before a send), else by which address has history on chain. A password,
//! or a `v3:` prefix, is never ambiguous — v1/v2 have no password.

use hmac::{Hmac, Mac};
use sha2::{Digest as _, Sha256, Sha512};
use sha3::Sha3_256;
use unicode_normalization::UnicodeNormalization;
use zeroize::Zeroize;

/// v2: Argon2id salt and cost. Part of every v2 address — never change.
pub const V2_SALT: &[u8] = b"sigilgraph-wallet-v2";
pub const V2_M_COST_KIB: u32 = 16 * 1024;
pub const V2_T_COST: u32 = 3;
pub const V2_LANES: u32 = 1;

/// v3: Argon2id salt and cost (RFC 9106 §4, second recommended option). Part of every v3
/// address — never change; a retune is a v4.
pub const V3_SALT: &[u8] = b"sigilgraph-wallet-v3";
pub const V3_M_COST_KIB: u32 = 64 * 1024;
pub const V3_T_COST: u32 = 3;
pub const V3_LANES: u32 = 4;

/// BIP39's PBKDF2 round count.
pub const BIP39_ROUNDS: u32 = 2048;

/// The official BIP39 English list. sha256 of the file =
/// 2f5eed53a4727b4bf8880d8f3f199efc90e58503646d9ff8eff3a2ed3b24dbda (the published value;
/// pinned by a test).
const ENGLISH: &str = include_str!("../assets/bip39-english.txt");

fn english() -> &'static [&'static str] {
    static W: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    W.get_or_init(|| ENGLISH.lines().collect())
}

fn word_index(w: &str) -> Option<u16> {
    english().binary_search(&w).ok().map(|i| i as u16)
}

/// Why words did not open a wallet. Messages are written for the person typing them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhraseError {
    Empty,
    /// BIP39 has 12, 15, 18, 21 or 24 words.
    WordCount(usize),
    /// `position` is 1-based, as a person counts.
    UnknownWord { position: usize, word: String },
    /// Every word is real but they do not fit together — almost always one mistyped or swapped word.
    Checksum,
    /// A password was given, but the words are not BIP39 words (only v3 wallets have a password).
    PasswordWithoutBip39Words(Box<PhraseError>),
    BadHexSeed,
    Kdf(String),
}

impl std::fmt::Display for PhraseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "the recovery words are empty"),
            Self::WordCount(n) => write!(f, "{n} words — a recovery phrase has 12 (or 15, 18, 21, 24) words"),
            Self::UnknownWord { position, word } => write!(f, "word {position} (\"{word}\") is not a recovery word — check its spelling"),
            Self::Checksum => write!(f, "the words do not fit together — one word is probably mistyped or two are swapped"),
            Self::PasswordWithoutBip39Words(e) => write!(f, "only new (v3) wallets have a password, and these words are not v3 words: {e}"),
            Self::BadHexSeed => write!(f, "that is not a valid 64-character hex key"),
            Self::Kdf(e) => write!(f, "key stretching failed: {e}"),
        }
    }
}

impl std::error::Error for PhraseError {}

/// Which recipe produced a secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    RawSeed,
    V1,
    V2,
    V3,
}

impl Scheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RawSeed => "raw",
            Self::V1 => "v1",
            Self::V2 => "v2",
            Self::V3 => "v3",
        }
    }
}

/// One wallet the words could open. The secret is wiped when this is dropped.
#[derive(Clone, PartialEq, Eq)]
pub struct Candidate {
    pub scheme: Scheme,
    pub secret: [u8; 32],
}

impl std::fmt::Debug for Candidate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Candidate").field("scheme", &self.scheme).field("secret", &"<hidden>").finish()
    }
}

impl Drop for Candidate {
    fn drop(&mut self) {
        self.secret.zeroize();
    }
}

/// BIP39's canonical word form: lowercased, single spaces, no edge whitespace.
fn canonical_words(phrase: &str) -> Vec<String> {
    phrase.split_whitespace().map(str::to_lowercase).collect()
}

/// Checks BIP39 words (spelling, count, checksum) and returns the entropy they encode.
pub fn bip39_entropy(phrase: &str) -> Result<Vec<u8>, PhraseError> {
    let words = canonical_words(phrase);
    let n = words.len();
    if n == 0 {
        return Err(PhraseError::Empty);
    }
    if !matches!(n, 12 | 15 | 18 | 21 | 24) {
        return Err(PhraseError::WordCount(n));
    }
    let mut bits: Vec<bool> = Vec::with_capacity(n * 11);
    for (i, w) in words.iter().enumerate() {
        let k = word_index(w).ok_or_else(|| PhraseError::UnknownWord { position: i + 1, word: w.clone() })?;
        bits.extend((0..11).rev().map(|b| (k >> b) & 1 == 1));
    }
    let ent_bits = n * 11 * 32 / 33;
    let entropy: Vec<u8> = bits[..ent_bits]
        .chunks(8)
        .map(|c| c.iter().fold(0u8, |a, &b| (a << 1) | b as u8))
        .collect();
    let h = Sha256::digest(&entropy);
    let cs_ok = bits[ent_bits..].iter().enumerate().all(|(i, &b)| ((h[0] >> (7 - i)) & 1 == 1) == b);
    if !cs_ok {
        return Err(PhraseError::Checksum);
    }
    Ok(entropy)
}

/// The BIP39 words for 16, 20, 24, 28 or 32 bytes of entropy (12 … 24 words).
pub fn bip39_words(entropy: &[u8]) -> Result<String, PhraseError> {
    if !(16..=32).contains(&entropy.len()) || entropy.len() % 4 != 0 {
        return Err(PhraseError::Kdf(format!("entropy must be 16..=32 bytes in steps of 4, got {}", entropy.len())));
    }
    let h = Sha256::digest(entropy);
    let cs = entropy.len() / 4;
    let mut bits: Vec<bool> = entropy.iter().flat_map(|b| (0..8).rev().map(move |i| (b >> i) & 1 == 1)).collect();
    bits.extend((0..cs).map(|i| (h[0] >> (7 - i)) & 1 == 1));
    let wl = english();
    Ok(bits
        .chunks(11)
        .map(|c| wl[c.iter().fold(0usize, |a, &b| (a << 1) | b as usize)])
        .collect::<Vec<_>>()
        .join(" "))
}

/// Twelve fresh v3 words from 16 random bytes (the caller supplies the randomness, so the same
/// code runs in a browser, on a phone and in sigil-top).
pub fn generate_v3_words(entropy: &[u8; 16]) -> String {
    bip39_words(entropy).expect("16 bytes is a valid BIP39 entropy length")
}

fn pbkdf2_sha512_64(pass: &[u8], salt: &[u8], rounds: u32) -> [u8; 64] {
    let mac = Hmac::<Sha512>::new_from_slice(pass).expect("HMAC takes a key of any length");
    let mut u: [u8; 64] = mac.clone().chain_update(salt).chain_update(1u32.to_be_bytes()).finalize().into_bytes().into();
    let mut t = u;
    for _ in 1..rounds {
        u = mac.clone().chain_update(u).finalize().into_bytes().into();
        t.iter_mut().zip(u.iter()).for_each(|(a, b)| *a ^= b);
    }
    u.zeroize();
    t
}

/// The standard BIP39 seed of checksum-valid words and a password (empty = no password).
pub fn bip39_seed(phrase: &str, password: &str) -> Result<[u8; 64], PhraseError> {
    bip39_entropy(phrase)?;
    let mut mnemonic: String = canonical_words(phrase).join(" ").nfkd().collect();
    let mut salt = String::from("mnemonic");
    salt.extend(password.nfkd());
    let seed = pbkdf2_sha512_64(mnemonic.as_bytes(), salt.as_bytes(), BIP39_ROUNDS);
    mnemonic.zeroize();
    salt.zeroize();
    Ok(seed)
}

fn argon2id(input: &[u8], salt: &[u8], m_kib: u32, t: u32, lanes: u32) -> Result<[u8; 32], PhraseError> {
    let params = argon2::Params::new(m_kib, t, lanes, Some(32)).map_err(|e| PhraseError::Kdf(e.to_string()))?;
    let mut out = [0u8; 32];
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
        .hash_password_into(input, salt, &mut out)
        .map_err(|e| PhraseError::Kdf(e.to_string()))?;
    Ok(out)
}

/// v3: BIP39 words + password → secret.
pub fn derive_v3(phrase: &str, password: &str) -> Result<[u8; 32], PhraseError> {
    let mut seed = bip39_seed(phrase, password)?;
    let out = argon2id(&seed, V3_SALT, V3_M_COST_KIB, V3_T_COST, V3_LANES);
    seed.zeroize();
    out
}

/// v2: the phrase bytes exactly as given (callers pass it trimmed, never re-spaced).
pub fn derive_v2(material: &str) -> Result<[u8; 32], PhraseError> {
    argon2id(material.as_bytes(), V2_SALT, V2_M_COST_KIB, V2_T_COST, V2_LANES)
}

/// v1: SHA3-256 of the phrase bytes exactly as given.
pub fn derive_v1(material: &str) -> [u8; 32] {
    Sha3_256::digest(material.as_bytes()).into()
}

fn hex64(s: &str) -> Option<&str> {
    let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    (s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())).then_some(s)
}

fn strip_prefix_ci<'a>(s: &'a str, p: &str) -> Option<&'a str> {
    (s.len() >= p.len() && s.is_char_boundary(p.len()) && s[..p.len()].eq_ignore_ascii_case(p)).then(|| s[p.len()..].trim())
}

/// The one wallet the pre-v3 rules give for a phrase — byte-for-byte what sigilgraph.org's
/// `sigilSchemeFor` + `sigilDeriveAuto`, sigil-top's `wallet_phrase` and the phone's core did
/// before v3 existed.
pub fn legacy(phrase: &str) -> Result<Candidate, PhraseError> {
    let t = phrase.trim();
    if let Some(h) = hex64(t) {
        let mut secret = [0u8; 32];
        for (i, b) in secret.iter_mut().enumerate() {
            *b = u8::from_str_radix(&h[2 * i..2 * i + 2], 16).map_err(|_| PhraseError::BadHexSeed)?;
        }
        return Ok(Candidate { scheme: Scheme::RawSeed, secret });
    }
    let (v2, material) = if let Some(m) = strip_prefix_ci(t, "v1:") {
        (false, m)
    } else if let Some(m) = strip_prefix_ci(t, "v2:") {
        (true, m)
    } else if strip_prefix_ci(t, "mm2:").is_some() {
        (true, t) // a MetaMask-derived phrase keeps its prefix in the hashed material
    } else {
        (t.split_whitespace().count() >= 12, t)
    };
    if material.is_empty() {
        return Err(PhraseError::Empty);
    }
    Ok(if v2 {
        Candidate { scheme: Scheme::V2, secret: derive_v2(material)? }
    } else {
        Candidate { scheme: Scheme::V1, secret: derive_v1(material) }
    })
}

/// Every wallet these words (and password) could open, most likely first.
///
/// * a password, or a `v3:` prefix → exactly one: v3 (an error if the words are not BIP39);
/// * a raw hex key, or a `v1:` / `v2:` / `mm2:` prefix → exactly one: the legacy wallet;
/// * checksum-valid BIP39 words without a password → TWO: v3, then v2. Pick the one whose
///   address you expect, or the one with history on chain;
/// * anything else → exactly one: the legacy wallet (v2 for 12+ words, v1 below).
pub fn candidates(phrase: &str, password: &str) -> Result<Vec<Candidate>, PhraseError> {
    let t = phrase.trim();
    if t.is_empty() {
        return Err(PhraseError::Empty);
    }
    let forced = strip_prefix_ci(t, "v3:");
    if forced.is_some() || !password.is_empty() {
        let words = forced.unwrap_or(t);
        return match derive_v3(words, password) {
            Ok(secret) => Ok(vec![Candidate { scheme: Scheme::V3, secret }]),
            Err(e @ PhraseError::Kdf(_)) => Err(e),
            Err(e) if forced.is_none() => Err(PhraseError::PasswordWithoutBip39Words(Box::new(e))),
            Err(e) => Err(e),
        };
    }
    let prefixed = ["v1:", "v2:", "mm2:"].iter().any(|p| strip_prefix_ci(t, p).is_some());
    if hex64(t).is_some() || prefixed || bip39_entropy(t).is_err() {
        return Ok(vec![legacy(t)?]);
    }
    Ok(vec![Candidate { scheme: Scheme::V3, secret: derive_v3(t, "")? }, legacy(t)?])
}

/// True when a password begins or ends with whitespace. BIP39 keeps that whitespace, so a
/// wallet created with it can only be reopened by typing it again — creation screens refuse it.
pub fn password_has_edge_whitespace(password: &str) -> bool {
    password != password.trim()
}

/// The browser's door into this crate: a plain C ABI over wasm linear memory, so the page needs
/// no generated glue (`sigil-wallet-phrase.mjs` on sigilgraph.org is the whole loader).
///
/// Error codes (negative returns): 1 empty · 2 word count · 3 unknown word (`swp_err_pos` gives
/// the 1-based position) · 4 checksum · 5 bad hex key · 6 key stretching · 9 not UTF-8 ·
/// 10 + inner code = a password was given but the words are not BIP39.
#[cfg(target_arch = "wasm32")]
pub mod wasm_abi {
    use super::*;
    use std::sync::atomic::{AtomicI32, Ordering};

    static ERR_POS: AtomicI32 = AtomicI32::new(0);

    fn code(e: &PhraseError) -> i32 {
        match e {
            PhraseError::Empty => 1,
            PhraseError::WordCount(_) => 2,
            PhraseError::UnknownWord { position, .. } => {
                ERR_POS.store(*position as i32, Ordering::Relaxed);
                3
            }
            PhraseError::Checksum => 4,
            PhraseError::BadHexSeed => 5,
            PhraseError::Kdf(_) => 6,
            PhraseError::PasswordWithoutBip39Words(inner) => 10 + code(inner),
        }
    }

    unsafe fn text<'a>(p: *const u8, n: usize) -> Result<&'a str, i32> {
        if n == 0 {
            return Ok("");
        }
        std::str::from_utf8(std::slice::from_raw_parts(p, n)).map_err(|_| 9)
    }

    #[no_mangle]
    pub extern "C" fn swp_alloc(n: usize) -> *mut u8 {
        let mut v = Vec::<u8>::with_capacity(n.max(1));
        let p = v.as_mut_ptr();
        std::mem::forget(v);
        p
    }

    /// Wipes, then frees, a buffer from `swp_alloc` (secrets pass through these buffers).
    #[no_mangle]
    pub unsafe extern "C" fn swp_free(p: *mut u8, n: usize) {
        let mut v = Vec::from_raw_parts(p, n.max(1), n.max(1));
        v.zeroize();
    }

    #[no_mangle]
    pub extern "C" fn swp_err_pos() -> i32 {
        ERR_POS.load(Ordering::Relaxed)
    }

    /// 0 = checksum-valid BIP39 words, else the negative error code.
    #[no_mangle]
    pub unsafe extern "C" fn swp_check(p: *const u8, n: usize) -> i32 {
        match text(p, n) {
            Err(c) => -c,
            Ok(t) => bip39_entropy(t).map(|_| 0).unwrap_or_else(|e| -code(&e)),
        }
    }

    /// Writes up to two candidates, 33 bytes each ([scheme 0 raw/1 v1/2 v2/3 v3][secret 32]),
    /// into `out` (66 bytes). Returns how many, or a negative error code.
    #[no_mangle]
    pub unsafe extern "C" fn swp_candidates(p: *const u8, n: usize, pw: *const u8, pwn: usize, out: *mut u8) -> i32 {
        let (t, w) = match (text(p, n), text(pw, pwn)) {
            (Ok(t), Ok(w)) => (t, w),
            _ => return -9,
        };
        match candidates(t, w) {
            Err(e) => -code(&e),
            Ok(cs) => {
                let o = std::slice::from_raw_parts_mut(out, 66);
                for (i, c) in cs.iter().take(2).enumerate() {
                    o[i * 33] = match c.scheme {
                        Scheme::RawSeed => 0,
                        Scheme::V1 => 1,
                        Scheme::V2 => 2,
                        Scheme::V3 => 3,
                    };
                    o[i * 33 + 1..i * 33 + 33].copy_from_slice(&c.secret);
                }
                cs.len().min(2) as i32
            }
        }
    }

    /// v2 alone (the page's existing synchronous path, ~18× faster than the JS Argon2id).
    #[no_mangle]
    pub unsafe extern "C" fn swp_derive_v2(p: *const u8, n: usize, out: *mut u8) -> i32 {
        match text(p, n) {
            Err(c) => -c,
            Ok(t) => match derive_v2(t) {
                Ok(s) => {
                    std::slice::from_raw_parts_mut(out, 32).copy_from_slice(&s);
                    0
                }
                Err(e) => -code(&e),
            },
        }
    }

    /// The BIP39 words for `entn` bytes of entropy, written as UTF-8 into `out` (capacity `cap`).
    /// Returns the byte length, or a negative error code.
    #[no_mangle]
    pub unsafe extern "C" fn swp_words(ent: *const u8, entn: usize, out: *mut u8, cap: usize) -> i32 {
        match bip39_words(std::slice::from_raw_parts(ent, entn)) {
            Err(e) => -code(&e),
            Ok(s) if s.len() > cap => -2,
            Ok(s) => {
                std::slice::from_raw_parts_mut(out, s.len()).copy_from_slice(s.as_bytes());
                s.len() as i32
            }
        }
    }
}
