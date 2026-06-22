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

// ── BLAKE4 diffusion instrument (the empirical "which R is safe" answer) ─────
// pow.rs §top and docs/BLAKE4.md both state that which reduced round count is
// "sound enough for PoW" is an EMPIRICAL question the flux-dev bench loop answers —
// but until now the tree only proved R=7≡BLAKE3 (KAT) and measured grind speed, with
// NO measurement of the security MARGIN per round. This is that measurement: a strict
// avalanche-criterion (SAC) probe. bench/test only — a research instrument, never on a
// consensus path (it calls the quarantined reduced-round `blake4_rounds`).

/// Diffusion statistics for BLAKE4 at a fixed round count.
#[cfg(any(test, feature = "bench"))]
#[derive(Debug, Clone, Copy)]
pub struct Avalanche {
    /// Mean fraction of the 256 output bits that flip when ONE input bit flips,
    /// averaged over every input-bit position and sample. Ideal diffusion ≈ 0.5.
    pub mean: f64,
    /// Worst-case `|P(output bit i flips) − 0.5|` over all 256 output bits. Ideal → 0.
    /// This is the SAFETY discriminator: a single under/over-coupled output bit (high
    /// bias) is a grind handle even when `mean` already looks ideal — so a reduced R
    /// is only "sound enough" if BOTH mean ≈ 0.5 AND max_bias stays small.
    pub max_bias: f64,
}

/// Measure BLAKE4's strict-avalanche behaviour at `rounds` rounds over `samples`
/// pseudo-random 40-byte (`header‖nonce`-shaped) inputs: for each input we flip every
/// one of its 320 bits in turn and count which of the 256 output bits change. Returns
/// the mean avalanche and the worst per-output-bit bias.
///
/// Deterministic + dependency-free: each sample's input is derived from `blake4_digest`
/// of the sample index (no `rand` crate), so the curve is reproducible run-to-run.
/// **bench/test only.**
#[cfg(any(test, feature = "bench"))]
pub fn blake4_avalanche(rounds: u32, samples: u32) -> Avalanche {
    const INPUT_LEN: usize = 40; // header(32)‖nonce(8) shape — one ≤64-byte block
    let mut bit_flips = [0u64; 256];
    let mut trials: u64 = 0;
    for s in 0..samples {
        // deterministic pseudo-random input from the sample index (two 32-B digests)
        let mut input = [0u8; INPUT_LEN];
        let d0 = blake4_digest(&s.to_le_bytes());
        let d1 = blake4_digest(&(s ^ 0xA5A5_A5A5u32).to_le_bytes());
        input[..32].copy_from_slice(&d0);
        input[32..40].copy_from_slice(&d1[..8]);

        let base = blake4_rounds(&input, rounds);
        for bit in 0..(INPUT_LEN * 8) {
            let mut flipped = input;
            flipped[bit / 8] ^= 1u8 << (bit % 8);
            let other = blake4_rounds(&flipped, rounds);
            for ob in 0..256usize {
                if (base[ob / 8] ^ other[ob / 8]) & (1u8 << (ob % 8)) != 0 {
                    bit_flips[ob] += 1;
                }
            }
            trials += 1;
        }
    }
    let t = trials as f64;
    let mean = bit_flips.iter().map(|&c| c as f64).sum::<f64>() / (t * 256.0);
    let max_bias = bit_flips
        .iter()
        .map(|&c| ((c as f64 / t) - 0.5).abs())
        .fold(0.0f64, f64::max);
    Avalanche { mean, max_bias }
}

// ── PoW grind-bias survey (the difficulty-soundness instrument) ──────────────
// SAC above measures the full 256-bit digest under arbitrary input flips. But a PoW
// miner only ever looks at the 64-bit TARGET WORD (`blake4_word`, compared to
// difficulty) and only ever moves by NONCE INCREMENT. So the soundness question
// "can a reduced-R miner realize wins faster than the nominal difficulty?" lives on
// those two surfaces, which generic SAC does not isolate. This probe does:
//   • target-word monobit bias — is the difficulty surface uniform?
//   • consecutive-nonce avalanche — are grind attempts independent, or do near-target
//     words cluster (letting a near-win be cheaply extended)?
// bench/test only.

/// PoW grind-soundness statistics at a fixed round count, on the 64-bit target word.
#[cfg(any(test, feature = "bench"))]
#[derive(Debug, Clone, Copy)]
pub struct GrindBias {
    /// Worst monobit bias over the 64 target-word bits: `max |P(bit i set) − 0.5|` across a
    /// nonce sweep. Ideal → 0. A biased target word = a skewed difficulty surface, so realized
    /// work per win diverges from nominal difficulty (the miner grinds "easy" wins).
    pub word_monobit_bias: f64,
    /// Mean fraction of the 64 target-word bits that differ between consecutive nonces `n,n+1`
    /// (the real grind step). Ideal ≈ 0.5 = each attempt independent. Markedly below 0.5 =
    /// consecutive attempts correlate → near-target words cluster → a near-win is cheap to
    /// extend (a difficulty shortcut).
    pub nonce_step_avalanche: f64,
}

/// Sweep `nonces` consecutive nonces over a fixed header and measure the two PoW-relevant
/// distributions of the 64-bit target word at `rounds` rounds. Deterministic, dep-free.
/// **bench/test only.**
#[cfg(any(test, feature = "bench"))]
pub fn blake4_grind_bias(rounds: u32, nonces: u64) -> GrindBias {
    debug_assert!(nonces >= 2, "need ≥2 nonces for a grind step");
    let header = blake4_digest(b"sigil-g0-grind-survey"); // fixed 32-byte header
    let mut set_count = [0u64; 64];
    let mut step_diff_bits: u64 = 0;
    let mut prev: Option<u64> = None;
    for n in 0..nonces {
        let w = blake4_word(&header, n, rounds);
        for b in 0..64 {
            set_count[b] += (w >> b) & 1;
        }
        if let Some(p) = prev {
            step_diff_bits += (w ^ p).count_ones() as u64;
        }
        prev = Some(w);
    }
    let nf = nonces as f64;
    let word_monobit_bias = set_count
        .iter()
        .map(|&c| (c as f64 / nf - 0.5).abs())
        .fold(0.0f64, f64::max);
    let nonce_step_avalanche = step_diff_bits as f64 / ((nonces - 1) as f64 * 64.0);
    GrindBias { word_monobit_bias, nonce_step_avalanche }
}

// ── Differential survey (the trail instrument SAC + grind-bias miss) ─────────
// SAC measures single-bit input differences (averaged); grind-bias measures the
// nonce-increment difference. NEITHER reports a LOW-WEIGHT MULTI-BIT input difference
// that propagates "quietly" — a differential TRAIL, the classic ARX/BLAKE attack class.
// This probe sweeps a Δ set (nonce single-bit + pseudo-random 2-bit + rotation-aligned
// 2-bit) and reports the WORST-CASE (minimum) output-differential avalanche over that
// set: ideal ≈ 0.5 for every Δ; a Δ whose avalanche collapses is a trail. Statistically
// clean (mean over 256 output bits, then min over Δ — tight, unlike a max-over-bits).
// bench/test only.

/// Worst-case differential diffusion at a fixed round count.
#[cfg(any(test, feature = "bench"))]
#[derive(Debug, Clone, Copy)]
pub struct DiffBias {
    /// `min` over the tested input differences Δ of the mean output-differential avalanche
    /// (fraction of the 256 output bits that flip under Δ). Ideal ≈ 0.5. Markedly below 0.5
    /// = a low-weight differential trail: some fixed Δ propagates with low output weight.
    pub min_avalanche: f64,
    /// Hamming weight of the Δ that achieved `min_avalanche` (1 = a single-bit Δ SAC averages
    /// away; ≥2 = a multi-bit trail no other instrument here covers).
    pub worst_delta_weight: u32,
}

/// Differential survey: sweep a deterministic set of input differences Δ and, per Δ, measure
/// the mean output-differential avalanche over `samples` random inputs. Returns the worst
/// (minimum) avalanche found and the weight of the Δ that caused it. **bench/test only.**
#[cfg(any(test, feature = "bench"))]
pub fn blake4_diff_bias(rounds: u32, samples: u32) -> DiffBias {
    const INPUT_LEN: usize = 40;
    const BITS: usize = INPUT_LEN * 8; // 320
    let set_bit = |d: &mut [u8; INPUT_LEN], bit: usize| d[bit / 8] ^= 1u8 << (bit % 8);

    // Δ set (40-byte masks): (1) nonce single-bit — the grind surface; (2) pseudo-random
    // 2-bit; (3) rotation-aligned 2-bit i,(i+32) — mimics ARX carry/rotate alignment.
    let mut deltas: Vec<[u8; INPUT_LEN]> = Vec::new();
    for bit in (32 * 8)..(40 * 8) {
        let mut d = [0u8; INPUT_LEN];
        set_bit(&mut d, bit);
        deltas.push(d);
    }
    for k in 0u32..64 {
        let h = blake4_digest(&k.to_le_bytes());
        let a = u16::from_le_bytes([h[0], h[1]]) as usize % BITS;
        let b = u16::from_le_bytes([h[2], h[3]]) as usize % BITS;
        if a != b {
            let mut d = [0u8; INPUT_LEN];
            set_bit(&mut d, a);
            set_bit(&mut d, b);
            deltas.push(d);
        }
    }
    for i in (0..BITS).step_by(10) {
        let mut d = [0u8; INPUT_LEN];
        set_bit(&mut d, i);
        set_bit(&mut d, (i + 32) % BITS);
        deltas.push(d);
    }

    let mut min_avalanche = 1.0f64;
    let mut worst_delta_weight = 0u32;
    for d in &deltas {
        let w = d.iter().map(|b| b.count_ones()).sum::<u32>();
        let mut diff_bits: u64 = 0;
        for s in 0..samples {
            let mut x = [0u8; INPUT_LEN];
            let g0 = blake4_digest(&s.to_le_bytes());
            let g1 = blake4_digest(&(s ^ 0x5A5A_5A5Au32).to_le_bytes());
            x[..32].copy_from_slice(&g0);
            x[32..40].copy_from_slice(&g1[..8]);
            let mut xd = x;
            for i in 0..INPUT_LEN {
                xd[i] ^= d[i];
            }
            let y0 = blake4_rounds(&x, rounds);
            let y1 = blake4_rounds(&xd, rounds);
            for i in 0..32 {
                diff_bits += (y0[i] ^ y1[i]).count_ones() as u64;
            }
        }
        let av = diff_bits as f64 / (samples as f64 * 256.0);
        if av < min_avalanche {
            min_avalanche = av;
            worst_delta_weight = w;
        }
    }
    DiffBias { min_avalanche, worst_delta_weight }
}

// ── AVX2 8-way grind (FULL_ROUNDS only) ──────────────────────────────────────
// The hot loop Cortex keeps flagging: an AVX2 intrinsic on the BLAKE3 compression
// rounds. Hashes 8 consecutive nonces in parallel for the MINER GRIND. It is
// consensus-safe: each lane computes the EXACT FULL_ROUNDS word that scalar
// `blake4_word_sound` does (proven lane == scalar in tests), 8 at a time. The
// node's verify path is unchanged (still scalar, still re-hashes at FULL_ROUNDS).

/// 16 little-endian message words for `header‖nonce` (one ≤64-byte block).
#[inline(always)]
fn message_words(header: &[u8], nonce: u64) -> ([u32; 16], u32) {
    let hlen = header.len().min(56);
    let mut buf = [0u8; 64];
    buf[..hlen].copy_from_slice(&header[..hlen]);
    buf[hlen..hlen + 8].copy_from_slice(&nonce.to_le_bytes());
    let mut m = [0u32; 16];
    for i in 0..16 {
        m[i] = u32::from_le_bytes(buf[i * 4..i * 4 + 4].try_into().unwrap());
    }
    (m, (hlen + 8) as u32)
}

/// 8 consecutive nonces → 8 FULL_ROUNDS target words. Uses AVX2 when present,
/// else the scalar sound path. `out[i] == blake4_word_sound(header, base+i)`.
#[inline]
pub fn blake4_words_x8(header: &[u8], base: u64) -> [u64; 8] {
    #[cfg(target_arch = "x86_64")]
    {
        if std::is_x86_feature_detected!("avx2") {
            // SAFETY: guarded by the runtime avx2 feature check.
            return unsafe { blake4_words_x8_avx2(header, base) };
        }
    }
    let mut out = [0u64; 8];
    for i in 0..8u64 {
        out[i as usize] = blake4_word_sound(header, base.wrapping_add(i));
    }
    out
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn blake4_words_x8_avx2(header: &[u8], base: u64) -> [u64; 8] {
    use core::arch::x86_64::*;
    macro_rules! rotr {
        ($x:expr, $n:expr) => {
            _mm256_or_si256(_mm256_srli_epi32($x, $n), _mm256_slli_epi32($x, 32 - $n))
        };
    }

    // Per-lane message words (scalar prep handles any header length / nonce straddle).
    let mut block_len = 0u32;
    let mut msgs = [[0u32; 16]; 8];
    for lane in 0..8u64 {
        let (m, bl) = message_words(header, base.wrapping_add(lane));
        msgs[lane as usize] = m;
        block_len = bl;
    }
    // Transpose → 16 message vectors (nonce `i` lives in lane `i`).
    let mut mv = [_mm256_setzero_si256(); 16];
    for w in 0..16 {
        mv[w] = _mm256_setr_epi32(
            msgs[0][w] as i32, msgs[1][w] as i32, msgs[2][w] as i32, msgs[3][w] as i32,
            msgs[4][w] as i32, msgs[5][w] as i32, msgs[6][w] as i32, msgs[7][w] as i32,
        );
    }

    let flags = (CHUNK_START | CHUNK_END | ROOT) as i32;
    let mut v = [
        _mm256_set1_epi32(IV[0] as i32), _mm256_set1_epi32(IV[1] as i32),
        _mm256_set1_epi32(IV[2] as i32), _mm256_set1_epi32(IV[3] as i32),
        _mm256_set1_epi32(IV[4] as i32), _mm256_set1_epi32(IV[5] as i32),
        _mm256_set1_epi32(IV[6] as i32), _mm256_set1_epi32(IV[7] as i32),
        _mm256_set1_epi32(IV[0] as i32), _mm256_set1_epi32(IV[1] as i32),
        _mm256_set1_epi32(IV[2] as i32), _mm256_set1_epi32(IV[3] as i32),
        _mm256_set1_epi32(0), _mm256_set1_epi32(0),
        _mm256_set1_epi32(block_len as i32), _mm256_set1_epi32(flags),
    ];

    macro_rules! g {
        ($a:expr,$b:expr,$c:expr,$d:expr,$mx:expr,$my:expr) => {{
            v[$a] = _mm256_add_epi32(_mm256_add_epi32(v[$a], v[$b]), $mx);
            v[$d] = rotr!(_mm256_xor_si256(v[$d], v[$a]), 16);
            v[$c] = _mm256_add_epi32(v[$c], v[$d]);
            v[$b] = rotr!(_mm256_xor_si256(v[$b], v[$c]), 12);
            v[$a] = _mm256_add_epi32(_mm256_add_epi32(v[$a], v[$b]), $my);
            v[$d] = rotr!(_mm256_xor_si256(v[$d], v[$a]), 8);
            v[$c] = _mm256_add_epi32(v[$c], v[$d]);
            v[$b] = rotr!(_mm256_xor_si256(v[$b], v[$c]), 7);
        }};
    }

    for _ in 0..FULL_ROUNDS {
        g!(0, 4, 8, 12, mv[0], mv[1]);
        g!(1, 5, 9, 13, mv[2], mv[3]);
        g!(2, 6, 10, 14, mv[4], mv[5]);
        g!(3, 7, 11, 15, mv[6], mv[7]);
        g!(0, 5, 10, 15, mv[8], mv[9]);
        g!(1, 6, 11, 12, mv[10], mv[11]);
        g!(2, 7, 8, 13, mv[12], mv[13]);
        g!(3, 4, 9, 14, mv[14], mv[15]);
        let old = mv;
        for i in 0..16 {
            mv[i] = old[MSG_PERMUTATION[i]];
        }
    }

    let lo = _mm256_xor_si256(v[0], v[8]); // w[0] per lane
    let hi = _mm256_xor_si256(v[1], v[9]); // w[1] per lane
    let mut lo_a = [0i32; 8];
    let mut hi_a = [0i32; 8];
    _mm256_storeu_si256(lo_a.as_mut_ptr() as *mut __m256i, lo);
    _mm256_storeu_si256(hi_a.as_mut_ptr() as *mut __m256i, hi);
    let mut out = [0u64; 8];
    for i in 0..8 {
        out[i] = (lo_a[i] as u32 as u64) | ((hi_a[i] as u32 as u64) << 32);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The AVX2 8-way grind MUST produce the EXACT same FULL_ROUNDS words as the
    /// scalar sound path, lane-for-lane — or the miner would find different nonces.
    /// This is the correctness gate for the Cortex AVX2 experiment.
    #[test]
    fn x8_lanes_match_scalar_sound() {
        for hlen in [32usize, 40, 48, 56, 37 /* unaligned */] {
            let header = vec![0x11u8 ^ hlen as u8; hlen];
            for base in [0u64, 1, 7, 1000, 0xDEAD_BEEF, u64::MAX - 7] {
                let x8 = blake4_words_x8(&header, base);
                for i in 0..8u64 {
                    assert_eq!(
                        x8[i as usize],
                        blake4_word_sound(&header, base.wrapping_add(i)),
                        "lane {i} (hlen {hlen}, base {base}) must equal scalar sound word"
                    );
                }
            }
        }
    }

    /// Honest measurement gate: scalar single-nonce grind vs AVX2 8-way grind.
    /// Run with: cargo test -p flux-miner --release bench_grind -- --ignored --nocapture
    #[test]
    #[ignore]
    fn bench_grind_scalar_vs_x8() {
        use std::time::Instant;
        let header = [0x11u8; 32];
        let iters: u64 = 4_000_000;

        let t = Instant::now();
        let mut acc = 0u64;
        for n in 0..iters {
            acc ^= blake4_word_sound(&header, n);
        }
        let s_scalar = t.elapsed().as_secs_f64();

        let t = Instant::now();
        let mut acc2 = 0u64;
        let mut n = 0u64;
        while n < iters {
            for w in blake4_words_x8(&header, n) {
                acc2 ^= w;
            }
            n += 8;
        }
        let s_x8 = t.elapsed().as_secs_f64();

        let mhs_scalar = iters as f64 / s_scalar / 1e6;
        let mhs_x8 = iters as f64 / s_x8 / 1e6;
        println!(
            "BLAKE4 grind  scalar {:.1} MH/s   avx2-x8 {:.1} MH/s   speedup {:.2}x   (acc {} {})",
            mhs_scalar, mhs_x8, mhs_x8 / mhs_scalar, acc, acc2
        );
        assert_eq!(acc, acc2, "scalar and x8 must grind identical words");
    }

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

    /// Diffusion curve R=1..7 — the empirical security-margin data behind any future
    /// decision to promote a reduced R as the deployed PoW (a consensus change). The
    /// deployed round count (R=7 ≡ BLAKE3) MUST show ideal ~50% avalanche with a small
    /// worst-bit bias; the curve down to R=1 shows how fast the margin erodes per round.
    #[test]
    fn avalanche_curve_quantifies_round_security_margin() {
        // MEASURED 2026-06-21 (this instrument, 48 samples = 15,360 single-bit trials/round):
        //   R=1 mean=0.366 bias=0.200   ← structurally broken (one output bit 0.20 off ideal)
        //   R=2 mean=0.500 bias=0.012   ← strict-avalanche ALREADY saturated
        //   R=3..7 mean≈0.500 bias≈0.011–0.014 (statistical noise floor @48 samples)
        // FINDING: BLAKE3's G + message permutation reaches full SAC in just 2 rounds, so the
        // diffusion FLOOR is R=2. (SAC is necessary, not sufficient — it doesn't rule out
        // higher-order/algebraic weaknesses of reduced-round BLAKE — but it definitively kills
        // the R=1 lever and points any safe speed-lever at R≥3.) These asserts lock the curve
        // as a regression guard: a bug in `compress8`/`g`/`permute` would move these numbers.
        let mut curve = [Avalanche { mean: 0.0, max_bias: 0.0 }; (FULL_ROUNDS + 1) as usize];
        for r in 1..=FULL_ROUNDS {
            curve[r as usize] = blake4_avalanche(r, 48);
            println!(
                "BLAKE4 avalanche  R={r}  mean={:.4}  max_bias={:.4}  (ideal mean=0.5000 bias=0)",
                curve[r as usize].mean, curve[r as usize].max_bias
            );
        }
        // R=1 is provably broken — must NEVER be a candidate PoW round count.
        assert!(
            curve[1].mean < 0.45 && curve[1].max_bias > 0.10,
            "R=1 must show broken diffusion (mean<0.45, bias>0.10), got mean={:.4} bias={:.4}",
            curve[1].mean, curve[1].max_bias
        );
        // R≥2 must be at ideal strict-avalanche (the saturation floor).
        for r in 2..=FULL_ROUNDS as usize {
            assert!(
                (curve[r].mean - 0.5).abs() < 0.02 && curve[r].max_bias < 0.05,
                "R={r} must show ideal SAC (mean≈0.5±0.02, bias<0.05), got mean={:.4} bias={:.4}",
                curve[r].mean, curve[r].max_bias
            );
        }
        // Tight sample on the DEPLOYED round count: it IS BLAKE3, must be near-perfect.
        let full = blake4_avalanche(FULL_ROUNDS, 192);
        println!(
            "BLAKE4 avalanche  R=7 (tight, 192 samples)  mean={:.4}  max_bias={:.4}",
            full.mean, full.max_bias
        );
        assert!(
            (full.mean - 0.5).abs() < 0.01 && full.max_bias < 0.03,
            "R=7 (deployed ≡ BLAKE3) must diffuse ideally, got mean={:.4} bias={:.4}",
            full.mean, full.max_bias
        );
    }

    /// PoW grind-bias survey R=1..7 — measures soundness on the surfaces a miner actually
    /// touches (the 64-bit target word + the nonce-increment grind step), which generic SAC
    /// does not isolate. The deployed R=7 MUST show a uniform difficulty surface (small word
    /// monobit bias) and independent grind attempts (consecutive-nonce avalanche ≈ 0.5).
    #[test]
    fn grind_bias_survey_targets_the_pow_word() {
        let mut curve = [GrindBias { word_monobit_bias: 0.0, nonce_step_avalanche: 0.0 };
            (FULL_ROUNDS + 1) as usize];
        for r in 1..=FULL_ROUNDS {
            curve[r as usize] = blake4_grind_bias(r, 16_384);
            println!(
                "BLAKE4 grind  R={r}  word_monobit_bias={:.4}  nonce_step_avalanche={:.4}  (ideal bias=0 step=0.5)",
                curve[r as usize].word_monobit_bias, curve[r as usize].nonce_step_avalanche
            );
        }
        // MEASURED 2026-06-22 (16,384-nonce sweep/round): R=1 is DEGENERATE on the PoW
        // surfaces — word_monobit_bias=0.500 (some target-word bit is CONSTANT across all
        // nonces) and nonce_step_avalanche=0.075 (consecutive nonces change only 7.5% of the
        // word → near-target wins cluster → trivial grind). A sharper kill than SAC's R=1
        // mean=0.366. R≥2 is clean on BOTH surfaces (bias≈0.01, step≈0.5). Locks the finding.
        assert!(
            curve[1].word_monobit_bias > 0.4 && curve[1].nonce_step_avalanche < 0.2,
            "R=1 must be degenerate on PoW surfaces (bias>0.4, step<0.2), got bias={:.4} step={:.4}",
            curve[1].word_monobit_bias, curve[1].nonce_step_avalanche
        );
        for r in 2..=FULL_ROUNDS as usize {
            assert!(
                curve[r].word_monobit_bias < 0.05 && (curve[r].nonce_step_avalanche - 0.5).abs() < 0.03,
                "R={r} PoW surfaces must be clean (bias<0.05, step≈0.5±0.03), got bias={:.4} step={:.4}",
                curve[r].word_monobit_bias, curve[r].nonce_step_avalanche
            );
        }
        // Tight sweep on the DEPLOYED round count (R=7 ≡ BLAKE3): difficulty surface must be
        // uniform and consecutive grind attempts independent.
        let g7 = blake4_grind_bias(FULL_ROUNDS, 65_536);
        println!(
            "BLAKE4 grind  R=7 (tight, 65536 nonces)  word_monobit_bias={:.4}  nonce_step_avalanche={:.4}",
            g7.word_monobit_bias, g7.nonce_step_avalanche
        );
        assert!(
            g7.word_monobit_bias < 0.02,
            "R=7 target word must be unbiased (difficulty surface uniform), got bias={:.4}",
            g7.word_monobit_bias
        );
        assert!(
            (g7.nonce_step_avalanche - 0.5).abs() < 0.01,
            "R=7 consecutive grind attempts must be independent (≈0.5), got step={:.4}",
            g7.nonce_step_avalanche
        );
    }

    /// Differential survey R=1..7 — the worst-case (min) output avalanche over a Δ set of
    /// nonce single-bit + multi-bit patterns. This is the trail instrument SAC (single-bit
    /// averaged) and grind-bias (nonce-step) miss. The deployed R=7 MUST have NO low-weight
    /// trail (worst-case avalanche stays ≈0.5 for every Δ).
    #[test]
    fn differential_survey_finds_no_trail_at_full_rounds() {
        let mut curve = [DiffBias { min_avalanche: 0.0, worst_delta_weight: 0 };
            (FULL_ROUNDS + 1) as usize];
        for r in 1..=FULL_ROUNDS {
            curve[r as usize] = blake4_diff_bias(r, 128);
            println!(
                "BLAKE4 diff   R={r}  min_avalanche={:.4}  worst_Δweight={}  (ideal min=0.5000)",
                curve[r as usize].min_avalanche, curve[r as usize].worst_delta_weight
            );
        }
        // MEASURED 2026-06-22 (128 samples/Δ over ~160 Δ): R=1 has a DOMINANT trail —
        // worst-case differential avalanche 0.023 (a Δ produces ~2% output change). R≥2 is
        // clean across single-bit AND multi-bit Δ (worst-case ≈0.492–0.495 ≈ noise floor of a
        // min over ~160 estimates). NOTE: this is a SAMPLED screen, not an exhaustive trail
        // search — it catches a dominant trail (and does, at R=1) but absence here is NOT a
        // proof of differential security. Locks the R=1-trail / R≥2-clean finding.
        assert!(
            curve[1].min_avalanche < 0.10,
            "R=1 must expose a dominant differential trail (worst-case avalanche <0.10), got {:.4}",
            curve[1].min_avalanche
        );
        for r in 2..=FULL_ROUNDS as usize {
            assert!(
                curve[r].min_avalanche > 0.45,
                "R={r} must show no low-weight trail in the Δ set (worst-case >0.45), got {:.4}",
                curve[r].min_avalanche
            );
        }
        // Tight pass on the DEPLOYED round count: BLAKE3 has no usable low-weight differential,
        // so the worst-case Δ in our set must still diffuse to ~half the output.
        let d7 = blake4_diff_bias(FULL_ROUNDS, 256);
        println!(
            "BLAKE4 diff   R=7 (tight, 256 samples)  min_avalanche={:.4}  worst_Δweight={}",
            d7.min_avalanche, d7.worst_delta_weight
        );
        assert!(
            d7.min_avalanche > 0.45,
            "R=7 must show NO low-weight differential trail (worst-case avalanche >0.45), got {:.4}",
            d7.min_avalanche
        );
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
