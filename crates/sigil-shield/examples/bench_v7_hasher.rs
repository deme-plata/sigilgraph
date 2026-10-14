//! Option C, step 0 (Viktor 2026-10-14: native post-quantum recursion). What does the v7 spend
//! proof cost when its INTERNAL hash — the Merkle commitments to the trace and constraint
//! evaluations, and the Fiat-Shamir coin — is Rescue-Prime instead of BLAKE3?
//!
//! Same AIR, same trace, same options; only the prover's `HashFn` differs. A recursive verifier
//! must re-run every hash of the proof it checks inside a circuit: Rescue-Prime is field
//! arithmetic and cheap there, BLAKE3 is bit operations and is not. (BLAKE4, the mining hash, is
//! BLAKE3 at full rounds and stays the mining hash; it is not involved here.)
//!
//! Measurement only — nothing here is used by a node or a wallet. Arms run interleaved so box
//! load hits them alike; single-threaded (winterfell without `concurrent`), like one phone core.
//! `fluxc run -p sigil-shield --profile release-fast --example bench_v7_hasher [-- ROUNDS]`

use std::marker::PhantomData;
use std::time::Instant;

use sigil_shield::mimc::ACCEPT_BITS;
use sigil_shield::note_v7::{padding_leaf, NoteV7, PoolTreeV7};
use sigil_shield::spend_full_v7::*;
use winterfell::{
    crypto::{hashers::{Blake3_256, Rp64_256, RpJive64_256}, DefaultRandomCoin, ElementHasher},
    math::{fields::f64::BaseElement, FieldElement},
    matrix::ColMatrix,
    AcceptableOptions, AuxRandElements, ConstraintCompositionCoefficients, DefaultConstraintEvaluator,
    DefaultTraceLde, Proof, ProofOptions, Prover, StarkDomain, TraceInfo, TracePolyTable, TraceTable,
};

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
    ];
    for r in 0..rounds {
        // rotate the order each round so no arm always runs first
        for k in 0..3 {
            match (k + r) % 3 {
                0 => run::<Blake3_256<BaseElement>>(&mut arms[0], trace(), &pi),
                1 => run::<Rp64_256>(&mut arms[1], trace(), &pi),
                _ => run::<RpJive64_256>(&mut arms[2], trace(), &pi),
            }
        }
    }
    let base = median(&arms[0].prove_ms);
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
}
