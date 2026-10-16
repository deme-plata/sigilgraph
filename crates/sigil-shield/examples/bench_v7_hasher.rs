//! Option C, step 0 (Viktor 2026-10-14: native post-quantum recursion). What does the v7 spend
//! proof cost when its INTERNAL hash — the Merkle commitments to the trace and constraint
//! evaluations, and the Fiat-Shamir coin — is Rescue-Prime instead of BLAKE3?
//!
//! Same AIR, same trace, same options; only the prover's `HashFn` differs. A recursive verifier
//! must re-run every hash of the proof it checks inside a circuit: Rescue-Prime is field
//! arithmetic and cheap there, BLAKE3 is bit operations and is not. (BLAKE4, the mining hash, is
//! BLAKE3 at full rounds and stays the mining hash; it is not involved here.)
//!
//! 2026-10-16 (Viktor: "measure it on the phone, next to Poseidon2"): a fourth arm, Poseidon2
//! (Plonky3 `p3-goldilocks` 0.8.0, width 12, the crate's standard Grain-LFSR constants, 8 full +
//! 22 partial rounds) wrapped in the SAME sponge layout as Rp64_256 (capacity 0..4 holding the
//! input length, rate 4..12, digest 4..8), plus a raw "one Merkle merge" timing per hash. On
//! aarch64 the Plonky3 permutation is its hand-written NEON path; winterfell's Rescue is plain
//! Rust on every platform. The Poseidon2 arm pays a field-format conversion per permutation
//! (winterfell keeps Goldilocks in Montgomery form, Plonky3 does not); the raw table shows its size.
//!
//! Measurement only — nothing here is used by a node or a wallet. Arms run interleaved so box
//! load hits them alike; single-threaded (winterfell without `concurrent`), like one phone core.
//! `fluxc run -p sigil-shield --release --example bench_v7_hasher [-- ROUNDS]` (use --release:
//! the phone wallet ships opt-level 3 + LTO; release-fast is opt-level 1 and skews the ratios).

use std::hint::black_box;
use std::marker::PhantomData;
use std::sync::LazyLock;
use std::time::Instant;

use p3_field::PrimeField64;
use p3_goldilocks::{default_goldilocks_poseidon2_12, Goldilocks, Poseidon2Goldilocks};
use p3_symmetric::Permutation;

use sigil_shield::mimc::ACCEPT_BITS;
use sigil_shield::note_v7::{padding_leaf, NoteV7, PoolTreeV7};
use sigil_shield::spend_full_v7::*;
use winterfell::{
    crypto::{hashers::{Blake3_256, Rp64_256, RpJive64_256}, DefaultRandomCoin, Digest, ElementHasher, Hasher},
    math::{fields::f64::BaseElement, FieldElement, StarkField},
    matrix::ColMatrix,
    AcceptableOptions, AuxRandElements, ConstraintCompositionCoefficients, DefaultConstraintEvaluator,
    DefaultTraceLde, Proof, ProofOptions, Prover, StarkDomain, TraceInfo, TracePolyTable, TraceTable,
    ByteReader, ByteWriter, Deserializable, DeserializationError, Serializable,
};

// ── Poseidon2 as a winterfell hasher ────────────────────────────────────────────────────────────

static P2: LazyLock<Poseidon2Goldilocks<12>> = LazyLock::new(default_goldilocks_poseidon2_12);

fn p2_permute(state: &mut [BaseElement; 12]) {
    let mut s: [Goldilocks; 12] = core::array::from_fn(|i| Goldilocks::new(state[i].as_int()));
    P2.permute_mut(&mut s);
    for i in 0..12 {
        state[i] = BaseElement::new(s[i].as_canonical_u64());
    }
}

#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
struct P2Digest([BaseElement; 4]);
impl Digest for P2Digest {
    fn as_bytes(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (i, e) in self.0.iter().enumerate() {
            out[i * 8..i * 8 + 8].copy_from_slice(&e.as_int().to_le_bytes());
        }
        out
    }
}
impl Serializable for P2Digest {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        for e in &self.0 {
            e.write_into(target);
        }
    }
}
impl Deserializable for P2Digest {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        Ok(Self([
            BaseElement::read_from(source)?,
            BaseElement::read_from(source)?,
            BaseElement::read_from(source)?,
            BaseElement::read_from(source)?,
        ]))
    }
}

/// Poseidon2 (width 12) in Rp64_256's sponge layout: capacity 0..4 (element 0 = input length),
/// rate 4..12, digest 4..8.
struct Poseidon2_256;
impl Poseidon2_256 {
    fn squeeze(mut state: [BaseElement; 12]) -> P2Digest {
        p2_permute(&mut state);
        P2Digest(state[4..8].try_into().unwrap())
    }
    fn absorb(elements: &[BaseElement]) -> P2Digest {
        let mut state = [BaseElement::ZERO; 12];
        state[0] = BaseElement::new(elements.len() as u64);
        let mut i = 0;
        for &e in elements {
            state[4 + i] += e;
            i += 1;
            if i == 8 {
                p2_permute(&mut state);
                i = 0;
            }
        }
        if i > 0 {
            p2_permute(&mut state);
        }
        P2Digest(state[4..8].try_into().unwrap())
    }
}
impl Hasher for Poseidon2_256 {
    type Digest = P2Digest;
    const COLLISION_RESISTANCE: u32 = 128;

    fn hash(bytes: &[u8]) -> P2Digest {
        // 7-byte chunks (always below the modulus); the last chunk gets a 1-byte terminator.
        let n = bytes.len().div_ceil(7).max(1);
        let mut elements = Vec::with_capacity(n);
        let chunks: Vec<&[u8]> = if bytes.is_empty() { vec![&[][..]] } else { bytes.chunks(7).collect() };
        for (k, c) in chunks.iter().enumerate() {
            let mut buf = [0u8; 8];
            buf[..c.len()].copy_from_slice(c);
            if k == n - 1 {
                buf[c.len()] = 1;
            }
            elements.push(BaseElement::new(u64::from_le_bytes(buf)));
        }
        Self::absorb(&elements)
    }
    fn merge(values: &[P2Digest; 2]) -> P2Digest {
        let mut state = [BaseElement::ZERO; 12];
        state[0] = BaseElement::new(8);
        state[4..8].copy_from_slice(&values[0].0);
        state[8..12].copy_from_slice(&values[1].0);
        Self::squeeze(state)
    }
    fn merge_with_int(seed: P2Digest, value: u64) -> P2Digest {
        let mut state = [BaseElement::ZERO; 12];
        state[4..8].copy_from_slice(&seed.0);
        state[8] = BaseElement::new(value);
        if value < BaseElement::MODULUS {
            state[0] = BaseElement::new(5);
        } else {
            state[9] = BaseElement::new(value / BaseElement::MODULUS);
            state[0] = BaseElement::new(6);
        }
        Self::squeeze(state)
    }
}
impl ElementHasher for Poseidon2_256 {
    type BaseField = BaseElement;
    fn hash_elements<E: FieldElement<BaseField = BaseElement>>(elements: &[E]) -> P2Digest {
        Self::absorb(E::slice_as_base_elements(elements))
    }
}

/// The production v7 prover with its hash swapped. Public inputs are read by the real prover.
struct HProver<H> {
    inner: SpendFullV7Prover,
    options: ProofOptions,
    _h: PhantomData<H>,
}
impl<H> HProver<H> {
    fn new() -> Self {
        Self { inner: SpendFullV7Prover::new(v7_options()), options: bench_options(), _h: PhantomData }
    }
}
impl<H: ElementHasher<BaseField = BaseElement> + Send + Sync> Prover for HProver<H> {
    type BaseField = BaseElement;
    type Air = SpendFullV7Air;
    type Trace = TraceTable<BaseElement>;
    type HashFn = H;
    type RandomCoin = DefaultRandomCoin<H>;
    type TraceLde<E: FieldElement<BaseField = BaseElement>> = DefaultTraceLde<E, H>;
    type ConstraintEvaluator<'a, E: FieldElement<BaseField = BaseElement>> =
        DefaultConstraintEvaluator<'a, SpendFullV7Air, E>;

    fn get_pub_inputs(&self, t: &Self::Trace) -> SpendFullV7PublicInputs {
        self.inner.get_pub_inputs(t)
    }
    fn options(&self) -> &ProofOptions {
        &self.options
    }
    fn new_trace_lde<E: FieldElement<BaseField = BaseElement>>(
        &self, ti: &TraceInfo, mt: &ColMatrix<BaseElement>, dom: &StarkDomain<BaseElement>,
    ) -> (Self::TraceLde<E>, TracePolyTable<E>) {
        DefaultTraceLde::new(ti, mt, dom)
    }
    fn new_evaluator<'a, E: FieldElement<BaseField = BaseElement>>(
        &self, air: &'a SpendFullV7Air, aux: Option<AuxRandElements<E>>, cc: ConstraintCompositionCoefficients<E>,
    ) -> Self::ConstraintEvaluator<'a, E> {
        DefaultConstraintEvaluator::new(air, aux, cc)
    }
}

/// v7_options() with the grinding factor taken from GRIND (default 16 = production). Grinding is
/// a proof-of-work the PROVER does with the proof's own hash; queries × log2(blowup) already
/// exceed the hash/field cap, so grinding buys no conjectured security here (checked by the
/// verifier's MinConjecturedSecurity(ACCEPT_BITS) below either way).
fn bench_options() -> ProofOptions {
    let g: u32 = std::env::var("GRIND").ok().and_then(|v| v.parse().ok()).unwrap_or(16);
    ProofOptions::new(84, 8, g, winterfell::FieldExtension::Quadratic, 8, 31)
}

struct Arm {
    name: &'static str,
    prove_ms: Vec<f64>,
    verify_ms: Vec<f64>,
    bytes: usize,
}

fn run<H: ElementHasher<BaseField = BaseElement> + Send + Sync>(
    arm: &mut Arm, trace: TraceTable<BaseElement>, pi: &SpendFullV7PublicInputs,
) {
    let t = Instant::now();
    let proof: Proof = HProver::<H>::new().prove(trace).expect("prove");
    arm.prove_ms.push(t.elapsed().as_secs_f64() * 1e3);
    arm.bytes = proof.to_bytes().len();
    let t = Instant::now();
    winterfell::verify::<SpendFullV7Air, H, DefaultRandomCoin<H>>(
        proof, pi.clone(), &AcceptableOptions::MinConjecturedSecurity(ACCEPT_BITS),
    )
    .expect("verify");
    arm.verify_ms.push(t.elapsed().as_secs_f64() * 1e3);
}

fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s[s.len() / 2]
}

fn main() {
    let rounds: usize = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(9);
    let note = NoteV7::new(5_000_000_000, [11, 22, 33, 44], [55, 66, 77, 88]).unwrap();
    let bob = NoteV7::new(0, [0; 4], [1, 2, 3, 4]).unwrap().owner_pk();
    let public_value = 100_000u64;
    let outs = [
        OutputV7::new(3_000_000_000, [101, 102, 103, 104], bob),
        OutputV7::new(5_000_000_000 - 3_000_000_000 - 100_000, [201, 202, 203, 204], note.owner_pk()),
    ];
    let pos = 14_348usize;
    let mut leaves = vec![padding_leaf(); 1 << DEPTH];
    for i in 0..pos {
        leaves[i] = NoteV7::new(i as u64 + 1, [i as u64; 4], [1, 2, 3, i as u64]).unwrap().commitment();
    }
    leaves[pos] = note.commitment();
    let tree = PoolTreeV7::new(leaves).unwrap();
    let path = tree.path(pos).unwrap();
    let pi = public_inputs_for(tree.root(), note.nullifier(pos as u64), public_value, [outs[0].commitment(), outs[1].commitment()]);
    let trace = || build_spend_full_v7_trace(&note, &path, BaseElement::new(public_value), &outs, &v7_options());

    // The production path, once, so the comparison is anchored to what wallets ship today.
    let p = SpendFullV7Prover::new(v7_options()).prove(trace()).expect("prove");
    verify_spend_full_v7(p, pi.clone()).expect("production verifier accepts the BLAKE3 arm's shape");

    let mut arms = [
        Arm { name: "BLAKE3-256 (today)", prove_ms: vec![], verify_ms: vec![], bytes: 0 },
        Arm { name: "Rescue-Prime Rp64_256", prove_ms: vec![], verify_ms: vec![], bytes: 0 },
        Arm { name: "Rescue-Prime Jive (RpJive64_256)", prove_ms: vec![], verify_ms: vec![], bytes: 0 },
        Arm { name: "Poseidon2 (p3, width 12)", prove_ms: vec![], verify_ms: vec![], bytes: 0 },
    ];
    for r in 0..rounds {
        // rotate the order each round so no arm always runs first
        for k in 0..4 {
            match (k + r) % 4 {
                0 => run::<Blake3_256<BaseElement>>(&mut arms[0], trace(), &pi),
                1 => run::<Rp64_256>(&mut arms[1], trace(), &pi),
                2 => run::<RpJive64_256>(&mut arms[2], trace(), &pi),
                _ => run::<Poseidon2_256>(&mut arms[3], trace(), &pi),
            }
        }
        eprintln!("round {}/{rounds} done", r + 1);
    }
    let base = median(&arms[0].prove_ms);
    println!("platform: {} / {}", std::env::consts::ARCH, cpu_name());
    println!("v7 spend, 1 in / {N_OUTS} out, width {WIDTH}, {TRACE_LEN} rows, {rounds} rounds interleaved, single thread, grinding {} bits", bench_options().grinding_factor());
    println!("{:<34} {:>10} {:>8} {:>10} {:>10}", "internal hash", "prove ms", "×", "verify ms", "bytes");
    for a in &arms {
        let m = median(&a.prove_ms);
        println!("{:<34} {:>10.1} {:>8.2} {:>10.2} {:>10}", a.name, m, m / base, median(&a.verify_ms), a.bytes);
    }
    for a in &arms {
        let mut s = a.prove_ms.clone();
        s.sort_by(|x, y| x.partial_cmp(y).unwrap());
        println!("  {} prove min/max {:.1}/{:.1} ms", a.name, s[0], s[s.len() - 1]);
    }
    merge_table();
}

/// One Merkle merge (two 256-bit digests → one) per hash, ns per call, best of 5 batches.
fn merge_table() {
    fn time<F: FnMut()>(iters: u32, mut f: F) -> f64 {
        let mut best = f64::MAX;
        for _ in 0..5 {
            let t = Instant::now();
            for _ in 0..iters {
                f();
            }
            best = best.min(t.elapsed().as_secs_f64() * 1e9 / iters as f64);
        }
        best
    }
    let n = 20_000;
    let b3 = [Blake3_256::<BaseElement>::hash(b"a"), Blake3_256::<BaseElement>::hash(b"b")];
    let rp = [Rp64_256::hash(b"a"), Rp64_256::hash(b"b")];
    let rj = [RpJive64_256::hash(b"a"), RpJive64_256::hash(b"b")];
    let p2 = [Poseidon2_256::hash(b"a"), Poseidon2_256::hash(b"b")];
    let mut raw: [Goldilocks; 12] = core::array::from_fn(|i| Goldilocks::new(i as u64));
    let rows = [
        ("BLAKE3-256 merge", time(n, || { black_box(Blake3_256::<BaseElement>::merge(black_box(&b3))); })),
        ("Rescue-Prime Rp64_256 merge", time(n, || { black_box(Rp64_256::merge(black_box(&rp))); })),
        ("Rescue-Prime Jive merge", time(n, || { black_box(RpJive64_256::merge(black_box(&rj))); })),
        ("Poseidon2 merge (with conversion)", time(n, || { black_box(Poseidon2_256::merge(black_box(&p2))); })),
        ("Poseidon2 raw permutation (p3 only)", time(n, || { P2.permute_mut(black_box(&mut raw)); })),
    ];
    println!("one Merkle merge, ns per call (best of 5 × {n})");
    for (name, ns) in rows {
        println!("  {:<38} {:>9.0} ns", name, ns);
    }
}

fn cpu_name() -> String {
    let text = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    for key in ["Hardware", "model name", "CPU part"] {
        if let Some(l) = text.lines().find(|l| l.starts_with(key)) {
            return l.split(':').nth(1).unwrap_or("").trim().to_string();
        }
    }
    "unknown".into()
}
