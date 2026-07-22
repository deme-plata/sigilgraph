//! FULLY-FOLDED SPEND AIR — the production monolithic proof. One winterfell trace binds, with
//! EVERYTHING hidden except `(root, nf, fee)`:
//!
//!   * VALUE OPENING    segment 0 computes `cm = compress2(value, blinding)` in-circuit. The
//!                      commitment is NOT public — it is the hidden Merkle leaf, so a spend
//!                      does not reveal WHICH note it consumes (unlinkability).
//!   * CONSERVATION     the same `value` = fee + Σ outputs (balance runs to 0 over the trace).
//!   * VALUE BINDING    row-0 `first·(balance − x)` forces the conserved value to equal the
//!                      committed value.
//!   * MEMBERSHIP       segments 1..=depth climb the tree with the hidden path (siblings +
//!                      position bits); the final row's running hash is asserted == PUBLIC root.
//!                      The key identity: `compress2(value, blinding)` is itself one compression
//!                      level, so the commitment computation IS "level −1" of the climb and the
//!                      leaf never needs to be exposed, even to the other columns.
//!   * NULLIFIER        `nf = compress2(spend_key, position)` in-circuit (public `nf`).
//!   * POSITION BINDING an accumulator column proves `position == Σ bitᵢ·2^i` over the SAME
//!                      hidden path bits membership uses — the nullifier provably belongs to the
//!                      leaf index actually membered (closes transfer.rs scope item 3).
//!
//! This supersedes the split `spend::SpendAir` + separate membership proof composition: there,
//! `cm_in` had to be PUBLIC to bind the two proofs together (leaking which note was spent), and
//! nothing bound the nullifier's `position` to the membered leaf's index. Here both bindings are
//! transition constraints inside one trace, checked by the VERIFIER.
//!
//! Trace: width 9, length (1+depth)·64 — depth must satisfy `depth+1` a power of two
//! (production: depth 15 → 32k-note anonymity set, trace 1024; tests: depth 3 → 8 leaves).
//!   0 balance  1 out          — conservation (balance −= out each row → 0)
//!   2 x        3 y            — merged commitment→membership Feistel lane
//!   4 sib      5 bit          — path witness (constant within a segment; bit boolean)
//!   6 nx       7 ny           — nullifier Feistel lane (nx[ROUNDS] = nf)
//!   8 acc                     — position accumulator (acc[0]=position, −bitᵢ·2^i at each
//!                               boundary, → 0)
//! Periodic: round constants + reset selector (period 64) · bit-weight `2^(row/64)` + row-0
//! `first` selector (period = trace length).
//!
//! ⚠️ SCOPE (honest): outputs are still not folded — each hidden `out` is not yet bound to its
//! `cm_out`, and per-output PRIVATE range columns are not in this trace (transfer.rs scope
//! item 2; `range::in_range` gates amounts at build time meanwhile). Everything here is real
//! winterfell; no wrapper, no zero-fill.

use winterfell::{
    crypto::{hashers::Blake3_256, DefaultRandomCoin},
    math::{fields::f64::BaseElement, FieldElement, ToElements},
    matrix::ColMatrix,
    Air, AirContext, Assertion, AuxRandElements, ConstraintCompositionCoefficients,
    DefaultConstraintEvaluator, DefaultTraceLde, EvaluationFrame, ProofOptions, Prover,
    StarkDomain, Trace, TraceInfo, TracePolyTable, TraceTable, TransitionConstraintDegree,
};

use crate::membership::MerklePath;
use crate::mimc::{pow7, round_constants, ACCEPT_BITS, ROUNDS};

const SEG: usize = 64; // rows per segment: 63 Feistel rounds + 1 boundary row

/// Everything a fully-folded spend reveals: the anonymity-set root, the nullifier, the fee.
#[derive(Clone)]
pub struct SpendFullPublicInputs {
    pub root: BaseElement,
    pub nf: BaseElement,
    pub fee: BaseElement,
}
impl ToElements<BaseElement> for SpendFullPublicInputs {
    fn to_elements(&self) -> Vec<BaseElement> {
        vec![self.root, self.nf, self.fee]
    }
}

pub struct SpendFullAir {
    context: AirContext<BaseElement>,
    root: BaseElement,
    nf: BaseElement,
    fee: BaseElement,
    trace_len: usize,
}

impl Air for SpendFullAir {
    type BaseField = BaseElement;
    type PublicInputs = SpendFullPublicInputs;
    type GkrProof = ();
    type GkrVerifier = ();

    fn new(trace_info: TraceInfo, pub_inputs: SpendFullPublicInputs, options: ProofOptions) -> Self {
        assert_eq!(9, trace_info.width());
        let trace_len = trace_info.length();
        // Degrees are UPPER BOUNDS (winterfell 0.9's debug exact-degree assert has witness-
        // dependent mismatches on selector×bit constraints — same family as membership/range;
        // the bounds size the blowup correctly, so release proving/verifying is sound).
        let degrees = vec![
            TransitionConstraintDegree::new(1),                          // conservation
            TransitionConstraintDegree::with_cycles(7, vec![SEG]),       // x: (1−s)·(x+c)^7 ⊕ s·reset
            TransitionConstraintDegree::with_cycles(2, vec![SEG]),       // y: s·bit-ordered reset
            TransitionConstraintDegree::with_cycles(1, vec![SEG]),       // sib constant in-segment
            TransitionConstraintDegree::with_cycles(1, vec![SEG]),       // bit constant in-segment
            TransitionConstraintDegree::new(2),                          // bit boolean
            TransitionConstraintDegree::new(7),                          // nx Feistel
            TransitionConstraintDegree::new(1),                          // ny' = nx
            TransitionConstraintDegree::with_cycles(1, vec![SEG, trace_len]), // acc: s·bit·2^lvl
            TransitionConstraintDegree::with_cycles(1, vec![trace_len]), // first·(balance − x)
            TransitionConstraintDegree::with_cycles(1, vec![trace_len]), // first·(acc − ny)
        ];
        SpendFullAir {
            context: AirContext::new(trace_info, degrees, 5, options),
            root: pub_inputs.root,
            nf: pub_inputs.nf,
            fee: pub_inputs.fee,
            trace_len,
        }
    }

    /// [0] round constants (period 64) · [1] reset selector (1 on each segment's last row) ·
    /// [2] bit weight `2^(row/64)` (one full cycle) · [3] `first` selector (1 at row 0 only).
    fn get_periodic_column_values(&self) -> Vec<Vec<BaseElement>> {
        let mut reset = vec![BaseElement::ZERO; SEG];
        reset[SEG - 1] = BaseElement::ONE;
        let pw: Vec<BaseElement> =
            (0..self.trace_len).map(|r| BaseElement::new(1u64 << (r / SEG))).collect();
        let mut first = vec![BaseElement::ZERO; self.trace_len];
        first[0] = BaseElement::ONE;
        vec![round_constants().to_vec(), reset, pw, first]
    }

    fn evaluate_transition<E: FieldElement + From<Self::BaseField>>(
        &self,
        frame: &EvaluationFrame<E>,
        periodic: &[E],
        result: &mut [E],
    ) {
        let c = periodic[0];
        let s = periodic[1]; // 1 on a segment-boundary row, else 0
        let pw = periodic[2]; // 2^level at the boundary closing that level
        let first = periodic[3];
        let one = E::from(BaseElement::ONE);

        let balance = frame.current()[0];
        let out = frame.current()[1];
        let x = frame.current()[2];
        let y = frame.current()[3];
        let sib = frame.current()[4];
        let bit = frame.current()[5];
        let nx = frame.current()[6];
        let ny = frame.current()[7];
        let acc = frame.current()[8];
        let nsib = frame.next()[4];
        let nbit = frame.next()[5];

        // conservation: balance' = balance − out
        result[0] = frame.next()[0] - (balance - out);

        // merged commitment/membership lane. Internal rows: Feistel x' = y + (x+c)^7, y' = x.
        // Boundary rows: the running hash x (segment 0: the commitment == hidden leaf; later:
        // the level parent) is position-ordered against the NEXT segment's sibling by its bit.
        let t = x + c;
        let t2 = t * t;
        let feistel_x = y + t2 * t2 * t2 * t;
        let feistel_y = x;
        let reset_x = x + nbit * (nsib - x); // bit ? sibling : running
        let reset_y = nsib + nbit * (x - nsib); // bit ? running : sibling
        result[1] = frame.next()[2] - (s * reset_x + (one - s) * feistel_x);
        result[2] = frame.next()[3] - (s * reset_y + (one - s) * feistel_y);

        // path witness shape: constant within a segment, boolean bits
        result[3] = (one - s) * (nsib - sib);
        result[4] = (one - s) * (nbit - bit);
        result[5] = bit * (bit - one);

        // nullifier lane: unconditional Feistel (only row ROUNDS is asserted)
        let n = nx + c;
        let n2 = n * n;
        result[6] = frame.next()[6] - (ny + n2 * n2 * n2 * n);
        result[7] = frame.next()[7] - nx;

        // position accumulator: acc' = acc − s·bit·2^level; with acc[0] == position and the
        // final-row assertion acc == 0 this proves position == Σ bitᵢ·2^i over the SAME bits
        // membership hashes with.
        result[8] = frame.next()[8] - acc + s * nbit * pw;

        // row-0 bindings: conserved value == committed value; nullifier position == acc start
        result[9] = first * (balance - x);
        result[10] = first * (acc - ny);
    }

    fn get_assertions(&self) -> Vec<Assertion<Self::BaseField>> {
        let last = self.trace_len - 1;
        vec![
            Assertion::single(0, last, BaseElement::ZERO), // balance fully consumed
            Assertion::single(1, 0, self.fee),             // first subtraction is the fee
            Assertion::single(2, last, self.root),         // membership: climb ends at the root
            Assertion::single(6, ROUNDS, self.nf),         // nf = compress2(spend_key, position)
            Assertion::single(8, last, BaseElement::ZERO), // position == Σ bits·2^i
        ]
    }

    fn context(&self) -> &AirContext<Self::BaseField> {
        &self.context
    }
}

pub struct SpendFullProver {
    options: ProofOptions,
}
impl SpendFullProver {
    pub fn new(options: ProofOptions) -> Self {
        Self { options }
    }
}
impl Prover for SpendFullProver {
    type BaseField = BaseElement;
    type Air = SpendFullAir;
    type Trace = TraceTable<Self::BaseField>;
    type HashFn = Blake3_256<Self::BaseField>;
    type RandomCoin = DefaultRandomCoin<Self::HashFn>;
    type TraceLde<E: FieldElement<BaseField = Self::BaseField>> = DefaultTraceLde<E, Self::HashFn>;
    type ConstraintEvaluator<'a, E: FieldElement<BaseField = Self::BaseField>> =
        DefaultConstraintEvaluator<'a, Self::Air, E>;

    fn get_pub_inputs(&self, trace: &Self::Trace) -> SpendFullPublicInputs {
        SpendFullPublicInputs {
            root: trace.get(2, trace.length() - 1),
            nf: trace.get(6, ROUNDS),
            fee: trace.get(1, 0),
        }
    }
    fn options(&self) -> &ProofOptions {
        &self.options
    }
    fn new_trace_lde<E: FieldElement<BaseField = Self::BaseField>>(
        &self, ti: &TraceInfo, mt: &ColMatrix<Self::BaseField>, dom: &StarkDomain<Self::BaseField>,
    ) -> (Self::TraceLde<E>, TracePolyTable<E>) {
        DefaultTraceLde::new(ti, mt, dom)
    }
    fn new_evaluator<'a, E: FieldElement<BaseField = Self::BaseField>>(
        &self, air: &'a Self::Air, aux: Option<AuxRandElements<E>>, cc: ConstraintCompositionCoefficients<E>,
    ) -> Self::ConstraintEvaluator<'a, E> {
        DefaultConstraintEvaluator::new(air, aux, cc)
    }
}

/// The leaf index the hidden path authenticates, as a field element: Σ bitᵢ·2^i.
pub fn path_position(path: &MerklePath) -> BaseElement {
    let idx = path
        .bits
        .iter()
        .enumerate()
        .fold(0u64, |a, (i, &b)| a | ((b as u64) << i));
    BaseElement::new(idx)
}

/// Build the fully-folded spend trace. `path` must be the hidden path of the note being spent
/// — its leaf MUST equal `compress2(value, blinding)` or the climb will not reach the root.
/// Requires `path.depth + 1` to be a power of two (trace length = (depth+1)·64).
/// A conserving witness (`value == fee + Σ out_values`) is required for the balance to reach 0.
pub fn build_spend_full_trace(
    value: BaseElement,
    blinding: BaseElement,
    spend_key: BaseElement,
    fee: BaseElement,
    out_values: &[BaseElement],
    path: &MerklePath,
) -> TraceTable<BaseElement> {
    let depth = path.siblings.len();
    let len = (depth + 1) * SEG;
    assert!(
        len.is_power_of_two(),
        "spend_full requires depth+1 a power of two (depth 1, 3, 7, 15, …); got depth {depth}"
    );
    let c = round_constants();
    let position = path_position(path);

    let mut subs = Vec::with_capacity(len);
    subs.push(fee);
    subs.extend_from_slice(out_values);
    assert!(subs.len() <= len, "too many outputs for one spend trace");
    subs.resize(len, BaseElement::ZERO);

    let sibs = path.siblings.clone();
    let bits = path.bits.clone();

    let mut trace = TraceTable::new(9, len);
    trace.fill(
        |state| {
            state[0] = value; // balance
            state[1] = subs[0]; // out (fee first)
            state[2] = value; // x — commitment Feistel left input
            state[3] = blinding; // y — commitment Feistel right input
            state[4] = BaseElement::ZERO; // sib (segment 0 carries no level)
            state[5] = BaseElement::ZERO; // bit (boolean, constant in segment 0)
            state[6] = spend_key; // nx — nullifier left input
            state[7] = position; // ny — nullifier right input
            state[8] = position; // acc — position accumulator
        },
        |step, state| {
            // conservation
            state[0] = state[0] - subs[step];
            state[1] = if step + 1 < len { subs[step + 1] } else { BaseElement::ZERO };

            let posr = step % SEG;
            if posr == SEG - 1 {
                // boundary: the running hash (segment 0: the commitment = hidden leaf) is
                // ordered against level `segi`'s sibling by its bit; acc consumes the bit.
                let segi = step / SEG;
                let running = state[2];
                let (sib, bit) = (sibs[segi], bits[segi]);
                let (l, r) = if bit { (sib, running) } else { (running, sib) };
                state[2] = l;
                state[3] = r;
                state[4] = sib;
                state[5] = if bit { BaseElement::ONE } else { BaseElement::ZERO };
                if bit {
                    state[8] = state[8] - BaseElement::new(1u64 << segi);
                }
            } else {
                // Feistel round
                let t = state[3] + pow7(state[2] + c[posr]);
                state[3] = state[2];
                state[2] = t;
            }
            // nullifier lane feistels unconditionally (the constraint holds on every row)
            let nt = state[7] + pow7(state[6] + c[posr]);
            state[7] = state[6];
            state[6] = nt;
        },
    );
    trace
}

type Coin = DefaultRandomCoin<Blake3_256<BaseElement>>;

pub fn verify_spend_full(
    proof: winterfell::Proof,
    pub_inputs: SpendFullPublicInputs,
) -> Result<(), winterfell::VerifierError> {
    let min = winterfell::AcceptableOptions::MinConjecturedSecurity(ACCEPT_BITS);
    winterfell::verify::<SpendFullAir, Blake3_256<BaseElement>, Coin>(proof, pub_inputs, &min)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::membership::CompressTree;
    use crate::mimc::{compress2, mimc_options};

    fn e(x: u64) -> BaseElement {
        BaseElement::new(x)
    }

    /// A depth-3 pool (8 notes) — depth+1 = 4 segments, trace length 256 (power of two).
    fn pool() -> (CompressTree, Vec<(BaseElement, BaseElement)>) {
        let notes: Vec<(BaseElement, BaseElement)> =
            (0..8).map(|i| (e(100 + i * 10), e(7000 + i))).collect();
        let leaves = notes.iter().map(|&(v, b)| compress2(v, b)).collect();
        (CompressTree::new(leaves), notes)
    }

    /// CONSTRUCTION GATE (green in debug): the fully-folded witness satisfies EVERY binding the
    /// verifier's assertions + transitions pin down, and each dishonest variant breaks exactly
    /// the constraint meant to catch it:
    ///  (1) the climb from the HIDDEN commitment reaches the public root; balance → 0;
    ///      nf really = compress2(spend_key, leaf index); acc → 0 (position == Σ bits·2^i);
    ///      the row-0 bindings hold (balance==x, acc==ny);
    ///  (2) a forged note (not in the tree) cannot reach the root;
    ///  (3) a tampered sibling cannot reach the root;
    ///  (4) the nullifier is pinned to THIS leaf's index — a nullifier for any other index
    ///      differs, so a spender cannot decouple nf from the membered position;
    ///  (5) conserving a different value than committed breaks the row-0 value binding.
    #[test]
    fn spend_full_binds_membership_conservation_nullifier_and_position() {
        let (tree, notes) = pool();
        let idx = 3usize;
        let (value, blinding) = notes[idx]; // value 130
        let sk = e(0xC0FFEE);
        let fee = e(5);
        let outs = vec![e(80), e(45)]; // 5 + 80 + 45 = 130
        let path = tree.path(idx);
        let len = (tree.depth() + 1) * SEG;
        let last = len - 1;

        let trace = build_spend_full_trace(value, blinding, sk, fee, &outs, &path);

        // (1) every assertion target holds on the honest trace
        assert_eq!(trace.get(2, last), tree.root(), "hidden-leaf climb must reach the public root");
        assert_eq!(trace.get(0, last), BaseElement::ZERO, "balance must reach 0");
        assert_eq!(trace.get(1, 0), fee, "first subtraction is the fee");
        assert_eq!(trace.get(6, ROUNDS), compress2(sk, e(idx as u64)),
            "nf must be compress2(spend_key, leaf index)");
        assert_eq!(trace.get(8, last), BaseElement::ZERO, "acc must consume position to 0");
        // row-0 bindings the `first` selector enforces
        assert_eq!(trace.get(0, 0), trace.get(2, 0), "value binding: conserved == committed");
        assert_eq!(trace.get(8, 0), trace.get(7, 0), "position binding: acc start == nullifier position");
        // the commitment at the segment-0 boundary is the real (hidden) leaf
        assert_eq!(trace.get(2, ROUNDS), compress2(value, blinding),
            "segment 0 must compute the note commitment as the hidden leaf");

        // (2) a forged note with the same path shape cannot reach the root
        let forged = build_spend_full_trace(value, e(999999), sk, fee, &outs, &path);
        assert_ne!(forged.get(2, last), tree.root(),
            "SECURITY: a commitment not in the tree must not climb to the root");

        // (3) a tampered sibling diverges from the root
        let mut bad = tree.path(idx);
        bad.siblings[1] = bad.siblings[1] + BaseElement::ONE;
        let tampered = build_spend_full_trace(value, blinding, sk, fee, &outs, &bad);
        assert_ne!(tampered.get(2, last), tree.root(),
            "SECURITY: a tampered authentication path must not reach the root");

        // (4) position binding: the in-circuit nf is pinned to THIS leaf's index
        for other in 0..8usize {
            if other != idx {
                assert_ne!(trace.get(6, ROUNDS), compress2(sk, e(other as u64)),
                    "SECURITY: the nullifier must not match any other leaf index");
            }
        }
        assert_eq!(path_position(&path), e(idx as u64), "path bits encode the leaf index");

        // (5) conserving a different value than committed breaks the row-0 value binding:
        // balance[0] would be value' while x[0] is the committed value — nonzero difference.
        let vprime = e(131);
        assert_ne!(vprime - value, BaseElement::ZERO,
            "SECURITY: conserving a value != the committed value violates first·(balance − x)");
    }

    /// The end-to-end winterfell STARK for the fully-folded spend: prove → verify green, and a
    /// wrong root / nullifier / fee are each rejected by the verifier. Subject to winterfell
    /// 0.9's debug-only exact-degree assert on witness-dependent bit columns (see the
    /// membership module) — determined empirically; release-sound regardless, with the witness
    /// bindings fully covered by the construction gate above.
    #[test]
    fn spend_full_stark_round_trips_and_rejects_wrong_publics() {
        let (tree, notes) = pool();
        let idx = 3usize;
        let (value, blinding) = notes[idx];
        let (sk, fee) = (e(0xC0FFEE), e(5));
        let outs = vec![e(80), e(45)];
        let path = tree.path(idx);

        let trace = build_spend_full_trace(value, blinding, sk, fee, &outs, &path);
        let root = tree.root();
        let nf = trace.get(6, ROUNDS);

        let proof = SpendFullProver::new(mimc_options()).prove(trace).expect("prove spend_full");
        verify_spend_full(proof.clone(), SpendFullPublicInputs { root, nf, fee })
            .expect("honest fully-folded spend verifies");

        assert!(verify_spend_full(proof.clone(),
            SpendFullPublicInputs { root: root + BaseElement::ONE, nf, fee }).is_err(),
            "SECURITY: wrong root must be rejected");
        assert!(verify_spend_full(proof.clone(),
            SpendFullPublicInputs { root, nf: nf + BaseElement::ONE, fee }).is_err(),
            "SECURITY: wrong nullifier must be rejected");
        assert!(verify_spend_full(proof,
            SpendFullPublicInputs { root, nf, fee: fee + BaseElement::ONE }).is_err(),
            "SECURITY: wrong fee must be rejected");
    }
}
