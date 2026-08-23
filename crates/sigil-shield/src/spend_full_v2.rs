//! OUTPUT-BOUND FULLY-FOLDED SPEND — closes `transfer.rs` scope item 2 (2026-08-23).
//!
//! [`crate::spend_full`] proves everything about the INPUT side of a spend (value opening,
//! conservation, membership, nullifier, position) but leaves the OUTPUT side unbound: the
//! hidden `out` values are subtracted from the balance and never tied to the output
//! commitments that consensus inserts into the note tree. That is a mint vector. A spender
//! could honestly conserve `100 = 3 fee + 50 + 47` while handing consensus commitments to
//! notes worth 500 and 470 — every individual check passes and the supply inflates.
//!
//! This AIR closes that hole by folding the outputs into the SAME trace. Two properties are
//! added per output, both verifier-checked:
//!
//!   * **OUTPUT ↔ COMMITMENT** — `cm_out_i == compress2(out_value_i, out_blinding_i)` is
//!     computed in-circuit, and `out_value_i` is bound to the very value subtracted from the
//!     balance in the conservation lane. The commitment is PUBLIC (consensus must insert it
//!     into the tree); the value and blinding stay hidden.
//!   * **PRIVATE RANGE** — `out_value_i < 2^RANGE_BITS` by in-circuit bit decomposition,
//!     with the amount kept WITNESS. The standalone [`crate::range`] AIR proves the same
//!     bound but takes the amount PUBLIC, which is useless for a shielded output.
//!
//! # Why this could not be a second, separate proof
//!
//! Composing a standalone "output" AIR alongside `spend_full` would require the out values
//! to be public in both traces in order to bind them to each other — exactly the leak that
//! motivated `spend_full` in the first place (its own docs: in the split path `cm_in` had to
//! be PUBLIC to bind the two proofs together, revealing which note was spent). Binding
//! hidden values across proof boundaries needs the values in one trace. Hence the fold.
//!
//! # Why the range bound matters for conservation
//!
//! Goldilocks is p ≈ 2^64. Conservation is field arithmetic, so without a range bound a
//! prover picks outputs whose INTEGER sum exceeds the input but whose FIELD sum wraps to it
//! — forged money that satisfies every constraint. Bounding each amount by 2^58 keeps
//! `fee + Σ outputs < (N_OUTS+1)·2^58 ≪ p`, so field conservation implies integer
//! conservation. The bound must satisfy `(N_OUTS+1)·2^RANGE_BITS < p`; at N_OUTS = 2 and
//! RANGE_BITS = 58 there is a factor of ~2^4 of headroom.
//!
//! # Trace layout
//!
//! ```text
//! width 9 + 5·N_OUTS, length (1+depth)·64  [depth+1 a power of two]
//!   0..=8                    identical to spend_full (see that module)
//!   9+5i+0  hv_i   value held constant — the binding pivot
//!   9+5i+1  ox_i   output-commitment Feistel x lane  (ox_i[ROUNDS] == cm_out_i)
//!   9+5i+2  oy_i   output-commitment Feistel y lane
//!   9+5i+3  rem_i  range remainder (LSB-first shift)
//!   9+5i+4  bit_i  range bit
//! ```
//!
//! `hv_i` is what makes the binding work: it is constrained constant, tied at row 0 to the
//! commitment lane's input AND to the range decomposition's start, and tied at row `i+1` to
//! the conservation lane's subtraction. One witness column, three bindings — so the value
//! that is conserved, the value that is committed, and the value that is range-checked are
//! provably the same value.

use winterfell::{
    crypto::{hashers::Blake3_256, DefaultRandomCoin},
    math::{fields::f64::BaseElement, FieldElement, StarkField, ToElements},
    matrix::ColMatrix,
    Air, AirContext, Assertion, AuxRandElements, ConstraintCompositionCoefficients,
    DefaultConstraintEvaluator, DefaultTraceLde, EvaluationFrame, ProofOptions, Prover,
    StarkDomain, Trace, TraceInfo, TracePolyTable, TraceTable, TransitionConstraintDegree,
};

use crate::membership::MerklePath;
use crate::mimc::{pow7, round_constants, ACCEPT_BITS, ROUNDS};
use crate::spend_full::path_position;

const SEG: usize = 64;

/// Outputs per spend. Fixed so the trace width is a compile-time constant; a spend with
/// fewer real outputs pads with zero-value notes (see [`build_spend_full_v2_trace`]).
pub const N_OUTS: usize = 2;

/// Per-amount range bound. See the module docs for why this must satisfy
/// `(N_OUTS+1)·2^RANGE_BITS < p`.
pub const RANGE_BITS: usize = 58;

/// Columns before the per-output block.
const BASE_COLS: usize = 9;
/// Columns per output: hv, ox, oy, rem, bit.
const COLS_PER_OUT: usize = 5;
/// Total trace width.
pub const WIDTH: usize = BASE_COLS + COLS_PER_OUT * N_OUTS;

const fn col_hv(i: usize) -> usize { BASE_COLS + COLS_PER_OUT * i }
const fn col_ox(i: usize) -> usize { BASE_COLS + COLS_PER_OUT * i + 1 }
const fn col_oy(i: usize) -> usize { BASE_COLS + COLS_PER_OUT * i + 2 }
const fn col_rem(i: usize) -> usize { BASE_COLS + COLS_PER_OUT * i + 3 }
const fn col_bit(i: usize) -> usize { BASE_COLS + COLS_PER_OUT * i + 4 }

/// What a v2 spend reveals: the anonymity-set root, the nullifier, the fee, and the output
/// commitments. The output VALUES stay hidden; only their commitments are public, because
/// consensus must insert those into the note tree.
#[derive(Clone)]
pub struct SpendFullV2PublicInputs {
    pub root: BaseElement,
    pub nf: BaseElement,
    pub fee: BaseElement,
    pub cm_outs: [BaseElement; N_OUTS],
}
impl ToElements<BaseElement> for SpendFullV2PublicInputs {
    fn to_elements(&self) -> Vec<BaseElement> {
        let mut v = vec![self.root, self.nf, self.fee];
        v.extend_from_slice(&self.cm_outs);
        v
    }
}

pub struct SpendFullV2Air {
    context: AirContext<BaseElement>,
    root: BaseElement,
    nf: BaseElement,
    fee: BaseElement,
    cm_outs: [BaseElement; N_OUTS],
    trace_len: usize,
}

impl Air for SpendFullV2Air {
    type BaseField = BaseElement;
    type PublicInputs = SpendFullV2PublicInputs;
    type GkrProof = ();
    type GkrVerifier = ();

    fn new(trace_info: TraceInfo, pub_inputs: SpendFullV2PublicInputs, options: ProofOptions) -> Self {
        assert_eq!(WIDTH, trace_info.width());
        let trace_len = trace_info.length();
        // Degrees are UPPER BOUNDS, matching spend_full's rationale: winterfell 0.9's
        // debug exact-degree assert has witness-dependent mismatches on selector×bit
        // constraints. The bounds size the blowup correctly, so release proving/verifying
        // is sound.
        let mut degrees = vec![
            TransitionConstraintDegree::new(1),                               // conservation
            TransitionConstraintDegree::with_cycles(7, vec![SEG]),            // x lane
            TransitionConstraintDegree::with_cycles(2, vec![SEG]),            // y lane
            TransitionConstraintDegree::with_cycles(1, vec![SEG]),            // sib constant
            TransitionConstraintDegree::with_cycles(1, vec![SEG]),            // bit constant
            TransitionConstraintDegree::new(2),                               // bit boolean
            TransitionConstraintDegree::new(7),                               // nx Feistel
            TransitionConstraintDegree::new(1),                               // ny' = nx
            TransitionConstraintDegree::with_cycles(1, vec![SEG, trace_len]), // acc
            TransitionConstraintDegree::with_cycles(1, vec![trace_len]),      // first·(balance−x)
            TransitionConstraintDegree::with_cycles(1, vec![trace_len]),      // first·(acc−ny)
        ];
        for _ in 0..N_OUTS {
            degrees.push(TransitionConstraintDegree::new(1));                          // hv constant
            degrees.push(TransitionConstraintDegree::with_cycles(7, vec![SEG]));       // ox Feistel
            degrees.push(TransitionConstraintDegree::new(1));                          // oy' = ox
            degrees.push(TransitionConstraintDegree::with_cycles(1, vec![trace_len])); // first·(ox−hv)
            degrees.push(TransitionConstraintDegree::with_cycles(1, vec![trace_len])); // osel·(out−hv)
            degrees.push(TransitionConstraintDegree::with_cycles(1, vec![trace_len])); // rsel·shift
            degrees.push(TransitionConstraintDegree::new(2));                          // bit boolean
            degrees.push(TransitionConstraintDegree::with_cycles(1, vec![trace_len])); // first·(rem−hv)
        }
        let num_assertions = 5 + 2 * N_OUTS;
        SpendFullV2Air {
            context: AirContext::new(trace_info, degrees, num_assertions, options),
            root: pub_inputs.root,
            nf: pub_inputs.nf,
            fee: pub_inputs.fee,
            cm_outs: pub_inputs.cm_outs,
            trace_len,
        }
    }

    /// [0] round constants (period 64) · [1] reset selector · [2] bit weight `2^(row/64)` ·
    /// [3] `first` (row 0 only) · [4] `rsel` (rows < RANGE_BITS) · [5+i] `osel_i` (row i+1).
    fn get_periodic_column_values(&self) -> Vec<Vec<BaseElement>> {
        let mut reset = vec![BaseElement::ZERO; SEG];
        reset[SEG - 1] = BaseElement::ONE;
        let pw: Vec<BaseElement> =
            (0..self.trace_len).map(|r| BaseElement::new(1u64 << (r / SEG))).collect();
        let mut first = vec![BaseElement::ZERO; self.trace_len];
        first[0] = BaseElement::ONE;
        let rsel: Vec<BaseElement> = (0..self.trace_len)
            .map(|r| if r < RANGE_BITS { BaseElement::ONE } else { BaseElement::ZERO })
            .collect();
        let mut cols = vec![round_constants().to_vec(), reset, pw, first, rsel];
        for i in 0..N_OUTS {
            let mut osel = vec![BaseElement::ZERO; self.trace_len];
            // subs[0] is the fee, so output i is subtracted at row i+1.
            osel[i + 1] = BaseElement::ONE;
            cols.push(osel);
        }
        cols
    }

    fn evaluate_transition<E: FieldElement + From<Self::BaseField>>(
        &self,
        frame: &EvaluationFrame<E>,
        periodic: &[E],
        result: &mut [E],
    ) {
        let c = periodic[0];
        let s = periodic[1];
        let pw = periodic[2];
        let first = periodic[3];
        let rsel = periodic[4];
        let one = E::from(BaseElement::ONE);
        let two = E::from(BaseElement::new(2));

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

        // ── input side: identical to spend_full ──────────────────────────────────────
        result[0] = frame.next()[0] - (balance - out);

        let t = x + c;
        let t2 = t * t;
        let feistel_x = y + t2 * t2 * t2 * t;
        let feistel_y = x;
        let reset_x = x + nbit * (nsib - x);
        let reset_y = nsib + nbit * (x - nsib);
        result[1] = frame.next()[2] - (s * reset_x + (one - s) * feistel_x);
        result[2] = frame.next()[3] - (s * reset_y + (one - s) * feistel_y);

        result[3] = (one - s) * (nsib - sib);
        result[4] = (one - s) * (nbit - bit);
        result[5] = bit * (bit - one);

        let n = nx + c;
        let n2 = n * n;
        result[6] = frame.next()[6] - (ny + n2 * n2 * n2 * n);
        result[7] = frame.next()[7] - nx;

        result[8] = frame.next()[8] - acc + s * nbit * pw;
        result[9] = first * (balance - x);
        result[10] = first * (acc - ny);

        // ── output side: the new bindings ────────────────────────────────────────────
        for i in 0..N_OUTS {
            let base = 11 + 8 * i;
            let osel = periodic[5 + i];

            let hv = frame.current()[col_hv(i)];
            let ox = frame.current()[col_ox(i)];
            let oy = frame.current()[col_oy(i)];
            let rem = frame.current()[col_rem(i)];
            let rbit = frame.current()[col_bit(i)];

            // hv is the binding pivot: constant across the trace so the same witness value
            // can be tied to three different rows' worth of constraints.
            result[base] = frame.next()[col_hv(i)] - hv;

            // cm_out_i = compress2(out_value_i, out_blinding_i), computed in-circuit.
            let ot = ox + c;
            let ot2 = ot * ot;
            result[base + 1] = frame.next()[col_ox(i)] - (oy + ot2 * ot2 * ot2 * ot);
            result[base + 2] = frame.next()[col_oy(i)] - ox;

            // BINDING 1: the commitment lane's left input IS the held value.
            result[base + 3] = first * (ox - hv);
            // BINDING 2: the value subtracted in the conservation lane at row i+1 IS the
            // held value. This is what stops a spender committing to amounts other than
            // the ones that balanced.
            result[base + 4] = osel * (out - hv);

            // RANGE: LSB-first shift, rem' = (rem − bit)/2, gated to the first RANGE_BITS rows.
            result[base + 5] = rsel * (rem - rbit - two * frame.next()[col_rem(i)]);
            result[base + 6] = rbit * (rbit - one);
            // BINDING 3: the decomposition starts at the held value.
            result[base + 7] = first * (rem - hv);
        }
    }

    fn get_assertions(&self) -> Vec<Assertion<Self::BaseField>> {
        let last = self.trace_len - 1;
        let mut a = vec![
            Assertion::single(0, last, BaseElement::ZERO),
            Assertion::single(1, 0, self.fee),
            Assertion::single(2, last, self.root),
            Assertion::single(6, ROUNDS, self.nf),
            Assertion::single(8, last, BaseElement::ZERO),
        ];
        for i in 0..N_OUTS {
            // the in-circuit output commitment equals the public one
            a.push(Assertion::single(col_ox(i), ROUNDS, self.cm_outs[i]));
            // the range decomposition is exhausted ⇒ value < 2^RANGE_BITS
            a.push(Assertion::single(col_rem(i), RANGE_BITS, BaseElement::ZERO));
        }
        a
    }

    fn context(&self) -> &AirContext<Self::BaseField> {
        &self.context
    }
}

pub struct SpendFullV2Prover {
    options: ProofOptions,
}
impl SpendFullV2Prover {
    pub fn new(options: ProofOptions) -> Self {
        Self { options }
    }
}
impl Prover for SpendFullV2Prover {
    type BaseField = BaseElement;
    type Air = SpendFullV2Air;
    type Trace = TraceTable<Self::BaseField>;
    type HashFn = Blake3_256<Self::BaseField>;
    type RandomCoin = DefaultRandomCoin<Self::HashFn>;
    type TraceLde<E: FieldElement<BaseField = Self::BaseField>> = DefaultTraceLde<E, Self::HashFn>;
    type ConstraintEvaluator<'a, E: FieldElement<BaseField = Self::BaseField>> =
        DefaultConstraintEvaluator<'a, Self::Air, E>;

    fn get_pub_inputs(&self, trace: &Self::Trace) -> SpendFullV2PublicInputs {
        let mut cm_outs = [BaseElement::ZERO; N_OUTS];
        for (i, slot) in cm_outs.iter_mut().enumerate() {
            *slot = trace.get(col_ox(i), ROUNDS);
        }
        SpendFullV2PublicInputs {
            root: trace.get(2, trace.length() - 1),
            nf: trace.get(6, ROUNDS),
            fee: trace.get(1, 0),
            cm_outs,
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

/// Build an output-bound spend trace.
///
/// `outs` is `[(value, blinding); N_OUTS]`. A spend with fewer real outputs passes
/// zero-value notes for the remainder — those still produce a real commitment, which the
/// caller may simply not insert into the tree.
///
/// Requires a conserving witness (`value == fee + Σ out_values`) and every amount within
/// `2^RANGE_BITS`; both are checked here so a malformed witness fails loudly at build
/// rather than producing a proof the verifier will silently reject.
pub fn build_spend_full_v2_trace(
    value: BaseElement,
    blinding: BaseElement,
    spend_key: BaseElement,
    fee: BaseElement,
    outs: &[(BaseElement, BaseElement); N_OUTS],
    path: &MerklePath,
) -> TraceTable<BaseElement> {
    let depth = path.siblings.len();
    let len = (depth + 1) * SEG;
    assert!(
        len.is_power_of_two(),
        "spend_full_v2 requires depth+1 a power of two (depth 1, 3, 7, 15, …); got depth {depth}"
    );
    assert!(len > RANGE_BITS, "trace must be longer than the range decomposition");

    // Conservation and range are the prover's obligations; fail loudly rather than emitting
    // a proof that cannot verify.
    let bound = 1u128 << RANGE_BITS;
    let sum: u128 = outs.iter().map(|(v, _)| v.as_int() as u128).sum::<u128>() + fee.as_int() as u128;
    assert_eq!(
        sum,
        value.as_int() as u128,
        "non-conserving witness: fee + Σ outputs must equal the note value"
    );
    assert!((fee.as_int() as u128) < bound, "fee exceeds the range bound");
    for (v, _) in outs.iter() {
        assert!((v.as_int() as u128) < bound, "output value exceeds the range bound");
    }

    let c = round_constants();
    let position = path_position(path);

    let mut subs = Vec::with_capacity(len);
    subs.push(fee);
    subs.extend(outs.iter().map(|(v, _)| *v));
    assert!(subs.len() <= len, "too many outputs for one spend trace");
    subs.resize(len, BaseElement::ZERO);

    let sibs = path.siblings.clone();
    let bits = path.bits.clone();

    let mut trace = TraceTable::new(WIDTH, len);
    trace.fill(
        |state| {
            state[0] = value;
            state[1] = subs[0];
            state[2] = value;
            state[3] = blinding;
            state[4] = BaseElement::ZERO;
            state[5] = BaseElement::ZERO;
            state[6] = spend_key;
            state[7] = position;
            state[8] = position;
            for i in 0..N_OUTS {
                let (ov, ob) = outs[i];
                state[col_hv(i)] = ov;
                state[col_ox(i)] = ov;
                state[col_oy(i)] = ob;
                state[col_rem(i)] = ov;
                state[col_bit(i)] = BaseElement::new(ov.as_int() & 1);
            }
        },
        |step, state| {
            state[0] = state[0] - subs[step];
            state[1] = if step + 1 < len { subs[step + 1] } else { BaseElement::ZERO };

            let posr = step % SEG;
            if posr == SEG - 1 {
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
                let t = state[3] + pow7(state[2] + c[posr]);
                state[3] = state[2];
                state[2] = t;
            }

            let nt = state[7] + pow7(state[6] + c[posr]);
            state[7] = state[6];
            state[6] = nt;

            for i in 0..N_OUTS {
                // hv stays put — it is the pivot every output binding refers to.
                // output-commitment lane feistels unconditionally (only row ROUNDS asserted)
                let ot = state[col_oy(i)] + pow7(state[col_ox(i)] + c[posr]);
                state[col_oy(i)] = state[col_ox(i)];
                state[col_ox(i)] = ot;

                // range: peel the low bit and halve, then hold at zero
                let cur = state[col_rem(i)].as_int();
                let next = if step < RANGE_BITS { cur >> 1 } else { 0 };
                state[col_rem(i)] = BaseElement::new(next);
                state[col_bit(i)] = BaseElement::new(if step + 1 < RANGE_BITS { next & 1 } else { 0 });
            }
        },
    );
    trace
}

/// Test-only trace builder that SKIPS the prover-obligation assertions in
/// [`build_spend_full_v2_trace`].
///
/// Those assertions protect an honest caller from emitting a doomed proof, but they are
/// Rust, not cryptography — a real attacker writes their own prover and never runs them.
/// To test that the CONSTRAINT SYSTEM rejects a bad witness we must be able to hand the
/// prover exactly what an attacker would. Everything below this point is identical to the
/// checked builder.
#[cfg(test)]
pub(crate) fn build_v2_trace_unchecked(
    value: BaseElement,
    blinding: BaseElement,
    spend_key: BaseElement,
    fee: BaseElement,
    outs: &[(BaseElement, BaseElement); N_OUTS],
    path: &MerklePath,
) -> TraceTable<BaseElement> {
    let depth = path.siblings.len();
    let len = (depth + 1) * SEG;
    let c = round_constants();
    let position = path_position(path);

    let mut subs = Vec::with_capacity(len);
    subs.push(fee);
    subs.extend(outs.iter().map(|(v, _)| *v));
    subs.resize(len, BaseElement::ZERO);

    let sibs = path.siblings.clone();
    let bits = path.bits.clone();

    let mut trace = TraceTable::new(WIDTH, len);
    trace.fill(
        |state| {
            state[0] = value;
            state[1] = subs[0];
            state[2] = value;
            state[3] = blinding;
            state[4] = BaseElement::ZERO;
            state[5] = BaseElement::ZERO;
            state[6] = spend_key;
            state[7] = position;
            state[8] = position;
            for i in 0..N_OUTS {
                let (ov, ob) = outs[i];
                state[col_hv(i)] = ov;
                state[col_ox(i)] = ov;
                state[col_oy(i)] = ob;
                state[col_rem(i)] = ov;
                state[col_bit(i)] = BaseElement::new(ov.as_int() & 1);
            }
        },
        |step, state| {
            state[0] = state[0] - subs[step];
            state[1] = if step + 1 < len { subs[step + 1] } else { BaseElement::ZERO };
            let posr = step % SEG;
            if posr == SEG - 1 {
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
                let t = state[3] + pow7(state[2] + c[posr]);
                state[3] = state[2];
                state[2] = t;
            }
            let nt = state[7] + pow7(state[6] + c[posr]);
            state[7] = state[6];
            state[6] = nt;
            for i in 0..N_OUTS {
                let ot = state[col_oy(i)] + pow7(state[col_ox(i)] + c[posr]);
                state[col_oy(i)] = state[col_ox(i)];
                state[col_ox(i)] = ot;
                let cur = state[col_rem(i)].as_int();
                let next = if step < RANGE_BITS { cur >> 1 } else { 0 };
                state[col_rem(i)] = BaseElement::new(next);
                state[col_bit(i)] = BaseElement::new(if step + 1 < RANGE_BITS { next & 1 } else { 0 });
            }
        },
    );
    trace
}

type Coin = DefaultRandomCoin<Blake3_256<BaseElement>>;

pub fn verify_spend_full_v2(
    proof: winterfell::Proof,
    pub_inputs: SpendFullV2PublicInputs,
) -> Result<(), winterfell::VerifierError> {
    let min = winterfell::AcceptableOptions::MinConjecturedSecurity(ACCEPT_BITS);
    winterfell::verify::<SpendFullV2Air, Blake3_256<BaseElement>, Coin>(proof, pub_inputs, &min)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mimc::{compress2, mimc_options};
    use crate::note_v1::{Note, ShieldedPoolTree};

    fn e(v: u64) -> BaseElement {
        BaseElement::new(v)
    }

    /// Build a depth-3 pool with our note at `position`, returning (pool, note).
    fn pool_with_note(value: u64, blinding: u64, key: u64, position: usize) -> (ShieldedPoolTree, Note) {
        let spender = Note::new(value, blinding, key).expect("in range");
        let mut leaves: Vec<BaseElement> = (0..7)
            .map(|i| Note::new(10 + i, 900 + i, 5000 + i).unwrap().commitment())
            .collect();
        leaves.insert(position, spender.commitment());
        (ShieldedPoolTree::new(leaves).expect("pool"), spender)
    }

    /// THE V2 GATE: an honest output-bound spend proves and verifies, and the public output
    /// commitments the verifier is handed are the real `compress2` of the hidden amounts.
    #[test]
    fn output_bound_spend_proves_and_binds_commitments() {
        let (pool, spender) = pool_with_note(100, 4242, 0xDEAD, 3);
        let fee = e(3);
        let outs = [(e(50), e(777)), (e(47), e(888))];
        let path = pool.path(3);
        let nf = spender.nullifier(3);

        let trace = build_spend_full_v2_trace(
            spender.value, spender.blinding, spender.spend_key, fee, &outs, &path,
        );
        let proof = SpendFullV2Prover::new(mimc_options()).prove(trace).expect("prove v2");

        let cm_outs = [compress2(outs[0].0, outs[0].1), compress2(outs[1].0, outs[1].1)];
        verify_spend_full_v2(
            proof,
            SpendFullV2PublicInputs { root: pool.root(), nf, fee, cm_outs },
        )
        .expect("an honest output-bound spend must verify");
    }

    /// THE MINT VECTOR THIS AIR EXISTS TO CLOSE. The spend conserves honestly
    /// (100 = 3 + 50 + 47) but the prover presents commitments to INFLATED outputs
    /// (500 and 470). Under `spend_full` v1 nothing contradicts this. Here the verifier
    /// must reject, because `cm_out` is bound in-circuit to the conserved amount.
    #[test]
    fn inflated_output_commitment_is_rejected() {
        let (pool, spender) = pool_with_note(100, 4242, 0xDEAD, 3);
        let fee = e(3);
        let honest = [(e(50), e(777)), (e(47), e(888))];
        let path = pool.path(3);
        let nf = spender.nullifier(3);

        let trace = build_spend_full_v2_trace(
            spender.value, spender.blinding, spender.spend_key, fee, &honest, &path,
        );
        let proof = SpendFullV2Prover::new(mimc_options()).prove(trace).expect("prove");

        // Claim commitments to 10× the real amounts.
        let inflated = [compress2(e(500), e(777)), compress2(e(470), e(888))];
        let verdict = verify_spend_full_v2(
            proof,
            SpendFullV2PublicInputs { root: pool.root(), nf, fee, cm_outs: inflated },
        );
        assert!(
            verdict.is_err(),
            "SECURITY: a proof must not verify against output commitments it did not compute — \
             accepting this is a mint vector"
        );
    }

    /// A non-conserving witness must not yield a verifying proof. The builder refuses it
    /// outright (loud failure beats a silently-rejected proof), which is itself the check.
    #[test]
    fn non_conserving_witness_is_refused_at_build() {
        let (pool, spender) = pool_with_note(100, 4242, 0xDEAD, 3);
        let path = pool.path(3);
        // 3 + 50 + 48 = 101 != 100
        let bad = [(e(50), e(777)), (e(48), e(888))];
        let r = std::panic::catch_unwind(|| {
            build_spend_full_v2_trace(
                spender.value, spender.blinding, spender.spend_key, e(3), &bad, &path,
            )
        });
        assert!(r.is_err(), "SECURITY: a non-conserving witness must be refused");
    }

    /// The checked builder refuses an out-of-range output. This tests the BUILDER (a
    /// convenience guard for honest provers), not the constraint system — see
    /// [`in_circuit_range_constraint_rejects_out_of_range_output`] for the real test.
    #[test]
    fn out_of_range_output_is_refused_by_the_builder() {
        let (pool, spender) = pool_with_note(100, 4242, 0xDEAD, 3);
        let path = pool.path(3);
        let bad = [(BaseElement::new(1u64 << RANGE_BITS), e(777)), (e(0), e(888))];
        let r = std::panic::catch_unwind(|| {
            build_spend_full_v2_trace(
                spender.value, spender.blinding, spender.spend_key, e(3), &bad, &path,
            )
        });
        assert!(r.is_err(), "the builder must refuse an out-of-range output");
    }

    /// THE WRAP-AROUND DEFENCE, tested against the CONSTRAINT SYSTEM rather than the
    /// builder — this is the one that matters, because an attacker writes their own prover.
    ///
    /// The witness is chosen so field-arithmetic conservation HOLDS while integer
    /// conservation is violated: the note is worth `2^58 + 3`, the fee is 3, and the
    /// outputs are `2^58` and 0. Every constraint except the range decomposition is
    /// satisfied. Only `rem[RANGE_BITS] == 0` catches it — an amount of exactly `2^58`
    /// still has a 1 bit left after 58 halvings, so the decomposition does not exhaust and
    /// the assertion fails. Without this constraint, that trace is a valid proof of
    /// forged money.
    #[test]
    fn in_circuit_range_constraint_rejects_out_of_range_output() {
        let over = 1u64 << RANGE_BITS;
        // A note big enough that the spend genuinely conserves in INTEGER terms too, so
        // nothing but the range bound can object.
        let spender = Note {
            value: BaseElement::new(over + 3),
            blinding: e(4242),
            spend_key: e(0xDEAD),
        };
        let mut leaves: Vec<BaseElement> = (0..7)
            .map(|i| Note::new(10 + i, 900 + i, 5000 + i).unwrap().commitment())
            .collect();
        leaves.insert(3, spender.commitment());
        let pool = ShieldedPoolTree::new(leaves).expect("pool");
        let path = pool.path(3);
        let fee = e(3);
        let outs = [(BaseElement::new(over), e(777)), (e(0), e(888))];

        // Sanity: this witness IS conserving, so conservation cannot be what rejects it.
        assert_eq!(
            fee.as_int() as u128 + over as u128 + 0u128,
            spender.value.as_int() as u128,
            "test setup: the witness must genuinely conserve"
        );

        let trace = build_v2_trace_unchecked(
            spender.value, spender.blinding, spender.spend_key, fee, &outs, &path,
        );
        let cm_outs = [compress2(outs[0].0, outs[0].1), compress2(outs[1].0, outs[1].1)];
        let nf = spender.nullifier(3);

        // Proving may succeed (winterfell does not validate traces in release); the
        // VERIFIER is what must refuse.
        match std::panic::catch_unwind(|| {
            SpendFullV2Prover::new(mimc_options()).prove(trace)
        }) {
            Err(_) | Ok(Err(_)) => { /* refused at proving — also acceptable */ }
            Ok(Ok(proof)) => {
                let verdict = verify_spend_full_v2(
                    proof,
                    SpendFullV2PublicInputs { root: pool.root(), nf, fee, cm_outs },
                );
                assert!(
                    verdict.is_err(),
                    "SECURITY: the verifier ACCEPTED an output of 2^{RANGE_BITS}, which is \
                     outside the range bound conservation relies on — this is a mint vector"
                );
            }
        }
    }

    /// The v2 proof still enforces everything v1 did: a wrong root must not verify.
    #[test]
    fn wrong_root_still_rejected() {
        let (pool, spender) = pool_with_note(100, 4242, 0xDEAD, 3);
        let fee = e(3);
        let outs = [(e(50), e(777)), (e(47), e(888))];
        let path = pool.path(3);
        let nf = spender.nullifier(3);
        let trace = build_spend_full_v2_trace(
            spender.value, spender.blinding, spender.spend_key, fee, &outs, &path,
        );
        let proof = SpendFullV2Prover::new(mimc_options()).prove(trace).expect("prove");
        let cm_outs = [compress2(outs[0].0, outs[0].1), compress2(outs[1].0, outs[1].1)];
        assert!(
            verify_spend_full_v2(
                proof,
                SpendFullV2PublicInputs { root: e(0xBADC0FFEE), nf, fee, cm_outs },
            ).is_err(),
            "SECURITY: membership must still bind to the real root"
        );
    }
}
