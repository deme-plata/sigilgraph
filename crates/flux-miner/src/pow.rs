//! BLAKE4 — the flux-miner proof-of-work hash, **parameterized by round count**.
//!
//! `flux-miner`'s original `blake4()` was literally full BLAKE3 (a placeholder —
//! see `docs/BLAKE4.md` §8 "what's still pretend"). This module makes BLAKE4 a
//! *real, distinct* primitive: the BLAKE3 compression over a single ≤64-byte
//! block (`header‖nonce`) with the **round count as a tunable knob**.
//!
//!   * `R = 7` (`FULL_ROUNDS`) ⇒ **byte-identical to BLAKE3** — the SOUND anchor,
//!     proven by a known-answer test against the `blake3` crate (`tests` below).
//!   * `R < 7` ⇒ fewer mixing rounds = faster = the lever into the measured **83×
//!     headroom** between BLAKE4-sound (155 MH/s) and the invertible ceiling
//!     (12.9 GH/s). Reduced rounds trade *security margin* for hashrate; for a
//!     PoW difficulty search over a 64-bit window you need preimage-hardness +
//!     grindability, NOT 256-bit collision resistance — so a sub-7-round core can
//!     be sound *enough* for mining while being materially faster. Which `R` is
//!     safe is an empirical question (diffusion / preimage margin) the
//!     flux-development bench loop answers; promoting a reduced `R` as the
//!     deployed PoW is a deliberate consensus change, gated behind crypto-agility.
//!
//! Why a single ≤64-byte block: the miner header (`client::build_header`) is a
//! 32-byte BLAKE3 digest, so `header‖nonce` is 40 bytes → exactly one BLAKE3
//! compression. That keeps BLAKE4 a single, branch-free, SIMD-friendly call —
//! "the Flux way."

// ── BLAKE3 constants (so R=7 is byte-exact) ──────────────────────────────────
const IV: [u32; 8] = [
    0x6A09_E667, 0xBB67_AE85, 0x3C6E_F372, 0xA54F_F53A,
    0x510E_527F, 0x9B05_688C, 0x1F83_D9AB, 0x5BE0_CD19,
];
const MSG_PERMUTATION: [usize; 16] = [2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8];
const CHUNK_START: u32 = 1 << 0;
const CHUNK_END: u32 = 1 << 1;
const ROOT: u32 = 1 << 3;

/// BLAKE3's full round count — the sound baseline (R=7 ≡ BLAKE3).
pub const FULL_ROUNDS: u32 = 7;
/// Deployed PoW round count. Stays at `FULL_ROUNDS` (= BLAKE3, no consensus
/// change) until a reduced count is validated + promoted.
pub const BLAKE4_ROUNDS: u32 = FULL_ROUNDS;

/// The BLAKE2/BLAKE3 quarter-round mixing function `G`.
#[inline(always)]
fn g(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize, mx: u32, my: u32) {
    s[a] = s[a].wrapping_add(s[b]).wrapping_add(mx);
    s[d] = (s[d] ^ s[a]).rotate_right(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_right(12);
    s[a] = s[a].wrapping_add(s[b]).wrapping_add(my);
    s[d] = (s[d] ^ s[a]).rotate_right(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_right(7);
}

/// One BLAKE3 round: 4 column mixes + 4 diagonal mixes over the 16-word state.
#[inline(always)]
fn round(s: &mut [u32; 16], m: &[u32; 16]) {
    g(s, 0, 4, 8, 12, m[0], m[1]);
    g(s, 1, 5, 9, 13, m[2], m[3]);
    g(s, 2, 6, 10, 14, m[4], m[5]);
    g(s, 3, 7, 11, 15, m[6], m[7]);
    g(s, 0, 5, 10, 15, m[8], m[9]);
    g(s, 1, 6, 11, 12, m[10], m[11]);
    g(s, 2, 7, 8, 13, m[12], m[13]);
    g(s, 3, 4, 9, 14, m[14], m[15]);
}

#[inline(always)]
fn permute(m: &mut [u32; 16]) {
    let old = *m;
    for i in 0..16 {
        m[i] = old[MSG_PERMUTATION[i]];
    }
}

/// Compress one ≤64-byte block at `rounds` rounds → the 8-word root CV.
/// `rounds == 7` is byte-identical to BLAKE3's single-block root output.
#[inline]
fn compress8(block: &[u8], rounds: u32) -> [u32; 8] {
    debug_assert!(block.len() <= 64, "BLAKE4 is single-block (≤64 bytes)");
    let mut buf = [0u8; 64];
    buf[..block.len()].copy_from_slice(block);
    let mut m = [0u32; 16];
    for i in 0..16 {
        m[i] = u32::from_le_bytes(buf[i * 4..i * 4 + 4].try_into().unwrap());
    }
    let flags = CHUNK_START | CHUNK_END | ROOT;
    let mut v: [u32; 16] = [
        IV[0], IV[1], IV[2], IV[3], IV[4], IV[5], IV[6], IV[7],
        IV[0], IV[1], IV[2], IV[3],
        0, 0, block.len() as u32, flags, // counter_lo, counter_hi, block_len, flags
    ];
    for _ in 0..rounds {
        round(&mut v, &m);
        permute(&mut m);
    }
    let mut out = [0u32; 8];
    for i in 0..8 {
        out[i] = v[i] ^ v[i + 8];
    }
    out
}

// ── SOUND consensus API (ungated): the ONLY hashing reachable in a default build,
// and it is ALWAYS FULL_ROUNDS (≡ BLAKE3). The reduced-round `rounds`-parameterized
// variants below are QUARANTINED behind `test`/`bench`/`gpu` so no consensus or
// fork-flag path can ever produce a weakened hash (red-team fix; DeepSeek flagged the
// mere existence of a reduced-round option as a soundness smell). `compress8` is
// private, so the `rounds` parameter is unreachable outside those gated callers.

/// Sound BLAKE4 digest (32 bytes) of a ≤64-byte input at FULL_ROUNDS (≡ BLAKE3).
pub fn blake4_digest(input: &[u8]) -> [u8; 32] {
    let w = compress8(input, FULL_ROUNDS);
    let mut out = [0u8; 32];
    for i in 0..8 {
        out[i * 4..i * 4 + 4].copy_from_slice(&w[i].to_le_bytes());
    }
    out
}

/// Sound miner target word at FULL_ROUNDS: first 8 bytes of `BLAKE4(header‖nonce)`
/// as a little-endian `u64`. `header` must be ≤56 bytes; `header‖nonce` is one
/// ≤64-byte block. This is what a consensus-grade miner uses.
#[inline]
pub fn blake4_word_sound(header: &[u8], nonce: u64) -> u64 {
    let hlen = header.len().min(56);
    let mut buf = [0u8; 64];
    buf[..hlen].copy_from_slice(&header[..hlen]);
    buf[hlen..hlen + 8].copy_from_slice(&nonce.to_le_bytes());
    let w = compress8(&buf[..hlen + 8], FULL_ROUNDS);
    (w[0] as u64) | ((w[1] as u64) << 32)
}

// ── QUARANTINED reduced-round API — bench/test/gpu builds ONLY ───────────────
// Not compiled into a default (consensus) build, so a future careless change or a
// fork flag cannot route consensus through R<7. The `gpu` feature is off by default
// and reduced-round output can't pass the node's verify (it re-hashes at FULL_ROUNDS).

/// BLAKE4 digest (32 bytes) of a ≤64-byte input at `rounds` rounds. **bench/test/gpu only.**
#[cfg(any(test, feature = "bench", feature = "gpu"))]
pub fn blake4_rounds(input: &[u8], rounds: u32) -> [u8; 32] {
    let w = compress8(input, rounds);
    let mut out = [0u8; 32];
    for i in 0..8 {
        out[i * 4..i * 4 + 4].copy_from_slice(&w[i].to_le_bytes());
    }
    out
}

/// The miner target word at `rounds`. **bench/test/gpu only** — reduced rounds are a
/// speed-research lever, never a consensus setting (use [`blake4_word_sound`]).
#[cfg(any(test, feature = "bench", feature = "gpu"))]
#[inline]
pub fn blake4_word(header: &[u8], nonce: u64, rounds: u32) -> u64 {
    let hlen = header.len().min(56);
    let mut buf = [0u8; 64];
    buf[..hlen].copy_from_slice(&header[..hlen]);
    buf[hlen..hlen + 8].copy_from_slice(&nonce.to_le_bytes());
    let w = compress8(&buf[..hlen + 8], rounds);
    (w[0] as u64) | ((w[1] as u64) << 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// KAT: at R=7, BLAKE4 MUST equal BLAKE3 for any single-block (≤64B) input.
    /// This proves the from-scratch compression (G, message schedule, IV, flags)
    /// is correct — so reduced-round variants are "real BLAKE3 with fewer rounds,"
    /// not an unrelated function.
    #[test]
    fn r7_is_byte_identical_to_blake3() {
        let cases: [&[u8]; 6] = [
            b"",
            b"abc",
            b"sigil-g0",
            &[0u8; 40],
            &[0xABu8; 64],
            b"the quick brown fox jumps over the lazy dog!!", // 45 bytes
        ];
        for input in cases {
            let mine = blake4_rounds(input, FULL_ROUNDS);
            let reference = blake3::hash(input);
            assert_eq!(
                &mine,
                reference.as_bytes(),
                "R=7 must equal BLAKE3 for a {}-byte input",
                input.len()
            );
        }
    }

    /// The word extractor must agree with the full digest's first 8 bytes, and
    /// match flux-miner's original `blake4()` semantics at R=7.
    #[test]
    fn word_matches_digest_prefix_and_legacy() {
        let header = [0x11u8; 32];
        let nonce = 0xDEAD_BEEF_u64;
        let w = blake4_word(&header, nonce, FULL_ROUNDS);

        // first 8 bytes of the full digest of header‖nonce
        let mut buf = Vec::new();
        buf.extend_from_slice(&header);
        buf.extend_from_slice(&nonce.to_le_bytes());
        let digest = blake4_rounds(&buf, FULL_ROUNDS);
        let prefix = u64::from_le_bytes(digest[0..8].try_into().unwrap());
        assert_eq!(w, prefix);

        // legacy blake4() = blake3::Hasher over header‖nonce, first 8 bytes
        let legacy = {
            let mut h = blake3::Hasher::new();
            h.update(&header);
            h.update(&nonce.to_le_bytes());
            u64::from_le_bytes(h.finalize().as_bytes()[0..8].try_into().unwrap())
        };
        assert_eq!(w, legacy, "BLAKE4 R=7 must match the legacy blake4() word");
    }

    /// Reduced rounds are deterministic and genuinely different from full rounds.
    #[test]
    fn reduced_rounds_are_distinct_and_stable() {
        let i = b"sigil-g0-block";
        assert_eq!(blake4_rounds(i, 3), blake4_rounds(i, 3));
        assert_ne!(blake4_rounds(i, 3), blake4_rounds(i, FULL_ROUNDS));
        assert_ne!(blake4_rounds(i, 1), blake4_rounds(i, 2));
    }
}
