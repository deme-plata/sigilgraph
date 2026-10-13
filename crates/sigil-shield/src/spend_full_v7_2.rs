//! v7 MERGE — the TWO-input, two-output Rescue-Prime spend AIR (2026-10-13).
//!
//! What [`crate::spend_full_v6`] is to [`crate::spend_full_v5`], this is to
//! [`crate::spend_full_v7`]: the same single-input circuit with a THIRD Rescue lane that
//! opens a second note of the SAME wallet, climbs to the SAME root, and reveals a second
//! nullifier. It is the transaction a one-input wallet cannot make — two notes in, one
//! payment plus change out — and therefore the only operation that raises a wallet's
//! single-payment ceiling.
//!
//! ```text
//!   lane A  pk     = H(DOM_PK ‖ sk)                  the wallet's owner key (shared by both inputs)
//!           inner  = H(DOM_VAL ‖ value  ‖ blinding)   note 1 (hidden)
//!           cm     = merge(inner, pk)   root = climb(cm, path)              PUBLIC root
//!           nf     = H(DOM_NF ‖ sk ‖ position)                             PUBLIC nf
//!   lane C  inner2 = H(DOM_VAL ‖ value2 ‖ blinding2)  note 2 (hidden), the SAME pk
//!           cm2    = merge(inner2, pk)  root = climb(cm2, path2)            the SAME root
//!           nf2    = H(DOM_NF ‖ sk ‖ position2)                            PUBLIC nf2
//!   lane B  cm_i   = merge(H(DOM_VAL ‖ out_i ‖ b_i), pk_i), i = 0, 1       PUBLIC cm_outs
//!   money   value + value2 = public_value + out_0 + out_1,  out_i < 2^RANGE_BITS
//!   inputs  (position − position2) · inv = 1                              the two notes DIFFER
//! ```
//!
//! # The duplicate-input hazard, closed in the circuit
//!
//! The v6 MiMC merge accepts one note fed in as both inputs (each block is valid on its own
//! and conservation simply sums the value twice); the only tell is the repeated nullifier,
//! which consensus must therefore refuse outside the circuit. Here both inputs share the
//! spend key, so `nf == nf2 ⇔ position == position2`, and one held witness column — the
//! inverse of the position difference — makes equal positions UNSATISFIABLE. The wire and
//! consensus checks stay (defence in depth), but a v7 merge of one note is not a proof.
//!
//! # Layout
//!
//! Lane A and lane B are [`crate::spend_full_v7`]'s columns 0..67, unchanged, with the
//! conservation constraint widened by `value2`. Lane C occupies 67..92: 12 state columns,
//! `blinding2[4]`, `value2`, `position2` (held), `sib2[4]`, `bit2` (segment-constant),
//! `acc2` (position accumulator) and `inv` (held). Lane C's first permutation (rows 0..8)
//! carries nothing — the owner key is lane A's — and from row 8 on it runs lane A's schedule
//! exactly: inner2, cm2, the 15-level climb, nf2, loaded by the SAME periodic selectors.
//!
//! Degrees, proof size and prover cost: one more degree-7 lane; the trace stays 256 rows.
//! The constraint-degree bounds and the release-only proving tests follow the v7 module.

use winterfell::{
    crypto::{hashers::Blake3_256, hashers::Rp64_256, DefaultRandomCoin},
    math::{fields::f64::BaseElement, FieldElement, ToElements},
    matrix::ColMatrix,
    Air, AirContext, Assertion, AuxRandElements, ConstraintCompositionCoefficients,
    DefaultConstraintEvaluator, DefaultTraceLde, EvaluationFrame, ProofOptions, Prover,
    StarkDomain, TraceInfo, TracePolyTable, TraceTable, TransitionConstraintDegree,
};

use crate::mimc::ACCEPT_BITS;
use crate::note_v7::{Digest4, MerklePathV7, NoteV7, DOM_NF, DOM_PK, DOM_VAL};
pub use crate::spend_full_v7::OutputV7;
use crate::spend_full_v7::{DEPTH, N_OUTS, RANGE_BITS, REAL_LEN, TRACE_LEN};
#[cfg(test)]
use crate::spend_full_v7::v7_options;
use crate::zk_mask::{exemptions_for, fill_random_row, padded_len, zk_rows_for, HidingProver, Secret};

/// Two inputs.
pub const N_INS: usize = 2;

const STATE: usize = 12;
const SEG: usize = 8;

// Lane A (input 1) — identical to spend_full_v7.
const COL_STATE: usize = 0;
const COL_SK: usize = 12;
const COL_B: usize = 16;
const COL_VALUE: usize = 20;
const COL_PK: usize = 21;
const COL_POS: usize = 25;
const COL_SIB: usize = 26;
const COL_BIT: usize = 30;
const COL_ACC: usize = 31;
// Lane B (outputs) — identical to spend_full_v7.
const COL_SB: usize = 32;
const COL_OUT_BASE: usize = 44;
const COLS_PER_OUT: usize = 9;
const COL_PUBVAL: usize = COL_OUT_BASE + COLS_PER_OUT * N_OUTS; // 62
const COL_RANGE_BASE: usize = COL_PUBVAL + 1; // 63
const WIDTH_V7: usize = COL_RANGE_BASE + 2 * N_OUTS; // 67
// Lane C (input 2).
const COL_SC: usize = WIDTH_V7; // 67..79  Rescue state, lane C
const COL_B2: usize = COL_SC + STATE; // 79..83  blinding 2, held
const COL_VALUE2: usize = COL_B2 + 4; // 83      value 2, held
const COL_POS2: usize = COL_VALUE2 + 1; // 84      position 2, held
const COL_SIB2: usize = COL_POS2 + 1; // 85..89  sibling 2, segment-constant
const COL_BIT2: usize = COL_SIB2 + 4; // 89      direction bit 2, segment-constant
const COL_ACC2: usize = COL_BIT2 + 1; // 90      position accumulator 2
const COL_INV: usize = COL_ACC2 + 1; // 91      (position − position2)^-1, held
/// Trace width.
pub const WIDTH: usize = COL_INV + 1; // 92

const fn col_out(i: usize) -> usize { COL_OUT_BASE + COLS_PER_OUT * i }
const fn col_ob(i: usize) -> usize { COL_OUT_BASE + COLS_PER_OUT * i + 1 }
const fn col_opk(i: usize) -> usize { COL_OUT_BASE + COLS_PER_OUT * i + 5 }
const fn col_rem(i: usize) -> usize { COL_RANGE_BASE + 2 * i }
const fn col_rbit(i: usize) -> usize { COL_RANGE_BASE + 2 * i + 1 }

const ROW_PK: usize = SEG - 1;
const ROW_INNER: usize = 2 * SEG - 1;
const ROW_CM: usize = 3 * SEG - 1;
const ROW_ROOT: usize = (3 + DEPTH) * SEG - 1;
const ROW_NF: usize = REAL_LEN - 1;
const fn row_inner_out(i: usize) -> usize { 2 * SEG * i + SEG - 1 }
const fn row_cm_out(i: usize) -> usize { 2 * SEG * i + 2 * SEG - 1 }

// Periodic columns — the v7 layout.
const PC_ARK1: usize = 0;
const PC_ARK2: usize = 12;
const PC_S: usize = 24;
const PC_FIRST: usize = 25;
const PC_LD_A: usize = 26;
const PC_LD_B: usize = 27;
const PC_LD_C: usize = 28;
const PC_LD_D: usize = 29;
const PC_PW: usize = 30;
const PC_LD_O: usize = 31;
const PC_RSEL: usize = 32;
const N_PERIODIC: usize = 33;

/// Lane A constraints, lane B constraints, lane C constraints.
const N_LANE_A: usize = 98;
const N_LANE_B: usize = 12 + 48 + 19 + 1 + 6;
const N_LANE_C: usize = 12 + 7 + 5 + 1 + 1 + 12 + 12 + 12 + 12 + 1 + 1;

/// The verifier's view: the shared root, BOTH nullifiers, the public value, the outputs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpendFullV7x2PublicInputs {
    pub root: [BaseElement; 4],
    pub nf: [BaseElement; 4],
    pub nf2: [BaseElement; 4],
    pub public_value: BaseElement,
    pub cm_outs: [[BaseElement; 4]; N_OUTS],
}
impl ToElements<BaseElement> for SpendFullV7x2PublicInputs {
    fn to_elements(&self) -> Vec<BaseElement> {
        let mut v = self.root.to_vec();
        v.extend_from_slice(&self.nf);
        v.extend_from_slice(&self.nf2);
        v.push(self.public_value);
        for cm in &self.cm_outs {
            v.extend_from_slice(cm);
        }
        v
    }
}

fn mat_vec<E: FieldElement + From<BaseElement>>(m: &[[BaseElement; STATE]; STATE], v: &[E; STATE]) -> [E; STATE] {
    let mut out = [E::ZERO; STATE];
    for i in 0..STATE {
        let mut acc = E::ZERO;
        for j in 0..STATE {
            acc += E::from(m[i][j]) * v[j];
        }
        out[i] = acc;
    }
    out
}

fn pow7<E: FieldElement>(x: E) -> E {
    let x2 = x * x;
    let x4 = x2 * x2;
    x4 * x2 * x
}

fn round_constraints<E: FieldElement + From<BaseElement>>(
    cur: &[E], nxt: &[E], base: usize, periodic: &[E], s: E, out: &mut [E],
) {
    let one = E::ONE;
    let mut cur7 = [E::ZERO; STATE];
    for j in 0..STATE {
        cur7[j] = pow7(cur[base + j]);
    }
    let rhs = mat_vec(&Rp64_256::MDS, &cur7);
    let mut shifted = [E::ZERO; STATE];
    for j in 0..STATE {
        shifted[j] = nxt[base + j] - periodic[PC_ARK2 + j];
    }
    let lhs_lin = mat_vec(&Rp64_256::INV_MDS, &shifted);
    for j in 0..STATE {
        out[j] = (one - s) * (pow7(lhs_lin[j]) - rhs[j] - periodic[PC_ARK1 + j]);
    }
}

pub struct SpendFullV7x2Air {
    context: AirContext<BaseElement>,
    pi: SpendFullV7x2PublicInputs,
}

impl Air for SpendFullV7x2Air {
    type BaseField = BaseElement;
    type PublicInputs = SpendFullV7x2PublicInputs;
    type GkrProof = ();
    type GkrVerifier = ();

    fn new(trace_info: TraceInfo, pub_inputs: SpendFullV7x2PublicInputs, options: ProofOptions) -> Self {
        assert_eq!(WIDTH, trace_info.width());
        let n = trace_info.length();
        assert_eq!(n, TRACE_LEN, "v7 merge traces are exactly {TRACE_LEN} rows");
        let d7 = || TransitionConstraintDegree::with_cycles(7, vec![SEG]);
        let d1 = || TransitionConstraintDegree::new(1);
        let d1s = || TransitionConstraintDegree::with_cycles(1, vec![SEG]);
        let d1n = || TransitionConstraintDegree::with_cycles(1, vec![n]);
        let d2n = || TransitionConstraintDegree::with_cycles(2, vec![n]);
        let mut degrees = Vec::with_capacity(N_LANE_A + N_LANE_B + N_LANE_C);
        // ── lane A ──
        for _ in 0..STATE { degrees.push(d7()); }
        for _ in COL_SK..COL_SIB { degrees.push(d1()); }
        for _ in 0..5 { degrees.push(d1s()); }
        degrees.push(TransitionConstraintDegree::new(2));
        degrees.push(TransitionConstraintDegree::with_cycles(1, vec![SEG, n]));
        for _ in 0..(12 + 4 + 12 + 12) { degrees.push(d1n()); }
        for _ in 0..12 { degrees.push(d2n()); }
        for _ in 0..13 { degrees.push(d1n()); }
        // ── lane B ──
        for _ in 0..STATE { degrees.push(d7()); }
        for _ in 0..(12 + 12 + 12 + 12) { degrees.push(d1n()); }
        for _ in 0..(COLS_PER_OUT * N_OUTS + 1) { degrees.push(d1()); }
        degrees.push(d1n());
        for _ in 0..N_OUTS {
            degrees.push(d1n());
            degrees.push(TransitionConstraintDegree::new(2));
            degrees.push(d1n());
        }
        // ── lane C ──
        for _ in 0..STATE { degrees.push(d7()); }                       // round
        for _ in 0..7 { degrees.push(d1()); }                           // held: b2[4], value2, pos2, inv
        for _ in 0..5 { degrees.push(d1s()); }                          // sib2, bit2 segment-constant
        degrees.push(TransitionConstraintDegree::new(2));               // bit2 boolean
        degrees.push(TransitionConstraintDegree::with_cycles(1, vec![SEG, n])); // acc2
        for _ in 0..(12 + 12) { degrees.push(d1n()); }                  // ldA, ldB loads
        for _ in 0..12 { degrees.push(d2n()); }                         // ldC
        for _ in 0..13 { degrees.push(d1n()); }                         // ldD, first·(acc2 − pos2)
        degrees.push(TransitionConstraintDegree::new(2));               // (pos − pos2)·inv − 1
        debug_assert_eq!(degrees.len(), N_LANE_A + N_LANE_B + N_LANE_C);
        let num_assertions = 4 + 4 + 1 + 1 + 4 * N_OUTS + N_OUTS + 4 + 4 + 1;
        SpendFullV7x2Air {
            context: AirContext::new(trace_info, degrees, num_assertions, options)
                .set_num_transition_exemptions(exemptions_for(n, REAL_LEN)),
            pi: pub_inputs,
        }
    }

    fn get_periodic_column_values(&self) -> Vec<Vec<BaseElement>> {
        let n = TRACE_LEN;
        let mut cols: Vec<Vec<BaseElement>> = Vec::with_capacity(N_PERIODIC);
        for ark in [&Rp64_256::ARK1, &Rp64_256::ARK2] {
            for j in 0..STATE {
                let mut c = vec![BaseElement::ZERO; SEG];
                for r in 0..Rp64_256::NUM_ROUNDS {
                    c[r] = ark[r][j];
                }
                cols.push(c);
            }
        }
        let mut s = vec![BaseElement::ZERO; SEG];
        s[SEG - 1] = BaseElement::ONE;
        cols.push(s);
        let one_at = |rows: &[usize]| {
            let mut c = vec![BaseElement::ZERO; n];
            for &r in rows {
                c[r] = BaseElement::ONE;
            }
            c
        };
        cols.push(one_at(&[0]));
        cols.push(one_at(&[ROW_PK]));
        cols.push(one_at(&[ROW_INNER]));
        let climb_rows: Vec<usize> = (0..DEPTH).map(|k| ROW_CM + SEG * k).collect();
        cols.push(one_at(&climb_rows));
        cols.push(one_at(&[ROW_ROOT]));
        let mut pw = vec![BaseElement::ZERO; n];
        for (k, &r) in climb_rows.iter().enumerate() {
            pw[r] = BaseElement::new(1u64 << k);
        }
        cols.push(pw);
        cols.push(one_at(&[ROW_CM]));
        let rsel: Vec<usize> = (0..RANGE_BITS).collect();
        cols.push(one_at(&rsel));
        debug_assert_eq!(cols.len(), N_PERIODIC);
        cols
    }

    fn evaluate_transition<E: FieldElement + From<Self::BaseField>>(
        &self,
        frame: &EvaluationFrame<E>,
        periodic: &[E],
        result: &mut [E],
    ) {
        let cur = frame.current();
        let nxt = frame.next();
        let one = E::ONE;
        let two = E::from(BaseElement::new(2));
        let s = periodic[PC_S];
        let first = periodic[PC_FIRST];
        let ld_a = periodic[PC_LD_A];
        let ld_b = periodic[PC_LD_B];
        let ld_c = periodic[PC_LD_C];
        let ld_d = periodic[PC_LD_D];
        let pw = periodic[PC_PW];
        let ld_o = periodic[PC_LD_O];
        let rsel = periodic[PC_RSEL];
        let five = E::from(BaseElement::new(5));
        let six = E::from(BaseElement::new(6));
        let eight = E::from(BaseElement::new(8));
        let dom_pk = E::from(BaseElement::new(DOM_PK));
        let dom_val = E::from(BaseElement::new(DOM_VAL));
        let dom_nf = E::from(BaseElement::new(DOM_NF));

        let st = &cur[COL_STATE..COL_STATE + STATE];
        let st_next = &nxt[COL_STATE..COL_STATE + STATE];
        let mut i = 0;

        // ══ lane A — exactly spend_full_v7 ══════════════════════════════════════
        round_constraints(cur, nxt, COL_STATE, periodic, s, &mut result[i..i + STATE]);
        i += STATE;
        for c in COL_SK..COL_SIB {
            result[i] = nxt[c] - cur[c];
            i += 1;
        }
        for c in COL_SIB..=COL_BIT {
            result[i] = (one - s) * (nxt[c] - cur[c]);
            i += 1;
        }
        let bit = cur[COL_BIT];
        result[i] = bit * (bit - one);
        i += 1;
        let nbit = nxt[COL_BIT];
        result[i] = nxt[COL_ACC] - cur[COL_ACC] + s * nbit * pw;
        i += 1;
        let p0 = [
            st[0] - five, st[1], st[2], st[3],
            st[4] - dom_pk,
            st[5] - cur[COL_SK], st[6] - cur[COL_SK + 1], st[7] - cur[COL_SK + 2], st[8] - cur[COL_SK + 3],
            st[9], st[10], st[11],
        ];
        for v in p0 { result[i] = first * v; i += 1; }
        for k in 0..4 {
            result[i] = ld_a * (st[4 + k] - cur[COL_PK + k]);
            i += 1;
        }
        let p1 = [
            st_next[0] - six, st_next[1], st_next[2], st_next[3],
            st_next[4] - dom_val,
            st_next[5] - cur[COL_VALUE],
            st_next[6] - cur[COL_B], st_next[7] - cur[COL_B + 1], st_next[8] - cur[COL_B + 2], st_next[9] - cur[COL_B + 3],
            st_next[10], st_next[11],
        ];
        for v in p1 { result[i] = ld_a * v; i += 1; }
        let p2 = [
            st_next[0] - eight, st_next[1], st_next[2], st_next[3],
            st_next[4] - st[4], st_next[5] - st[5], st_next[6] - st[6], st_next[7] - st[7],
            st_next[8] - cur[COL_PK], st_next[9] - cur[COL_PK + 1], st_next[10] - cur[COL_PK + 2], st_next[11] - cur[COL_PK + 3],
        ];
        for v in p2 { result[i] = ld_b * v; i += 1; }
        let mut p3 = [E::ZERO; STATE];
        p3[0] = st_next[0] - eight;
        p3[1] = st_next[1];
        p3[2] = st_next[2];
        p3[3] = st_next[3];
        for k in 0..4 {
            let node = st[4 + k];
            let sib = nxt[COL_SIB + k];
            let left = node + nbit * (sib - node);
            let right = sib + nbit * (node - sib);
            p3[4 + k] = st_next[4 + k] - left;
            p3[8 + k] = st_next[8 + k] - right;
        }
        for v in p3 { result[i] = ld_c * v; i += 1; }
        let p4 = [
            st_next[0] - six, st_next[1], st_next[2], st_next[3],
            st_next[4] - dom_nf,
            st_next[5] - cur[COL_SK], st_next[6] - cur[COL_SK + 1], st_next[7] - cur[COL_SK + 2], st_next[8] - cur[COL_SK + 3],
            st_next[9] - cur[COL_POS],
            st_next[10], st_next[11],
        ];
        for v in p4 { result[i] = ld_d * v; i += 1; }
        result[i] = first * (cur[COL_ACC] - cur[COL_POS]);
        i += 1;
        debug_assert_eq!(i, N_LANE_A);

        // ══ lane B — exactly spend_full_v7, conservation widened ════════════════
        let sb = &cur[COL_SB..COL_SB + STATE];
        let sb_next = &nxt[COL_SB..COL_SB + STATE];
        round_constraints(cur, nxt, COL_SB, periodic, s, &mut result[i..i + STATE]);
        i += STATE;
        let inner_in = |x: &[E], o: usize| -> [E; STATE] {
            [
                x[0] - six, x[1], x[2], x[3],
                x[4] - dom_val,
                x[5] - cur[col_out(o)],
                x[6] - cur[col_ob(o)], x[7] - cur[col_ob(o) + 1], x[8] - cur[col_ob(o) + 2], x[9] - cur[col_ob(o) + 3],
                x[10], x[11],
            ]
        };
        let cm_in = |x: &[E], o: usize| -> [E; STATE] {
            [
                x[0] - eight, x[1], x[2], x[3],
                x[4] - sb[4], x[5] - sb[5], x[6] - sb[6], x[7] - sb[7],
                x[8] - cur[col_opk(o)], x[9] - cur[col_opk(o) + 1], x[10] - cur[col_opk(o) + 2], x[11] - cur[col_opk(o) + 3],
            ]
        };
        for v in inner_in(sb, 0) { result[i] = first * v; i += 1; }
        for v in cm_in(sb_next, 0) { result[i] = ld_a * v; i += 1; }
        for v in inner_in(sb_next, 1) { result[i] = ld_b * v; i += 1; }
        for v in cm_in(sb_next, 1) { result[i] = ld_o * v; i += 1; }
        for c in COL_OUT_BASE..=COL_PUBVAL {
            result[i] = nxt[c] - cur[c];
            i += 1;
        }
        // ── conservation: value + value2 = public_value + Σ out_i ──
        let mut spent = cur[COL_PUBVAL];
        for o in 0..N_OUTS {
            spent += cur[col_out(o)];
        }
        result[i] = first * (cur[COL_VALUE] + cur[COL_VALUE2] - spent);
        i += 1;
        for o in 0..N_OUTS {
            let rem = cur[col_rem(o)];
            let rb = cur[col_rbit(o)];
            result[i] = rsel * (rem - rb - two * nxt[col_rem(o)]);
            result[i + 1] = rb * (rb - one);
            result[i + 2] = first * (rem - cur[col_out(o)]);
            i += 3;
        }
        debug_assert_eq!(i, N_LANE_A + N_LANE_B);

        // ══ lane C — the second input, lane A's schedule from row 8 ═════════════
        let sc = &cur[COL_SC..COL_SC + STATE];
        let sc_next = &nxt[COL_SC..COL_SC + STATE];
        round_constraints(cur, nxt, COL_SC, periodic, s, &mut result[i..i + STATE]);
        i += STATE;
        // held: blinding2, value2, position2, inv
        for c in COL_B2..=COL_POS2 {
            result[i] = nxt[c] - cur[c];
            i += 1;
        }
        result[i] = nxt[COL_INV] - cur[COL_INV];
        i += 1;
        // per-segment constants: sib2, bit2
        for c in COL_SIB2..=COL_BIT2 {
            result[i] = (one - s) * (nxt[c] - cur[c]);
            i += 1;
        }
        let bit2 = cur[COL_BIT2];
        result[i] = bit2 * (bit2 - one);
        i += 1;
        let nbit2 = nxt[COL_BIT2];
        result[i] = nxt[COL_ACC2] - cur[COL_ACC2] + s * nbit2 * pw;
        i += 1;
        // ldA (row 7): P1' input = (6,0,0,0, DOM_VAL, value2, b2[4], 0,0)
        let q1 = [
            sc_next[0] - six, sc_next[1], sc_next[2], sc_next[3],
            sc_next[4] - dom_val,
            sc_next[5] - cur[COL_VALUE2],
            sc_next[6] - cur[COL_B2], sc_next[7] - cur[COL_B2 + 1], sc_next[8] - cur[COL_B2 + 2], sc_next[9] - cur[COL_B2 + 3],
            sc_next[10], sc_next[11],
        ];
        for v in q1 { result[i] = ld_a * v; i += 1; }
        // ldB (row 15): P2' input = (8,0,0,0, inner2 = digest(P1'), pk_held) — the SAME owner key
        let q2 = [
            sc_next[0] - eight, sc_next[1], sc_next[2], sc_next[3],
            sc_next[4] - sc[4], sc_next[5] - sc[5], sc_next[6] - sc[6], sc_next[7] - sc[7],
            sc_next[8] - cur[COL_PK], sc_next[9] - cur[COL_PK + 1], sc_next[10] - cur[COL_PK + 2], sc_next[11] - cur[COL_PK + 3],
        ];
        for v in q2 { result[i] = ld_b * v; i += 1; }
        // ldC: the climb with its own siblings and bits
        let mut q3 = [E::ZERO; STATE];
        q3[0] = sc_next[0] - eight;
        q3[1] = sc_next[1];
        q3[2] = sc_next[2];
        q3[3] = sc_next[3];
        for k in 0..4 {
            let node = sc[4 + k];
            let sib = nxt[COL_SIB2 + k];
            let left = node + nbit2 * (sib - node);
            let right = sib + nbit2 * (node - sib);
            q3[4 + k] = sc_next[4 + k] - left;
            q3[8 + k] = sc_next[8 + k] - right;
        }
        for v in q3 { result[i] = ld_c * v; i += 1; }
        // ldD (row 143): P18' input = (6,0,0,0, DOM_NF, sk[4], position2, 0,0) — the SAME spend key
        let q4 = [
            sc_next[0] - six, sc_next[1], sc_next[2], sc_next[3],
            sc_next[4] - dom_nf,
            sc_next[5] - cur[COL_SK], sc_next[6] - cur[COL_SK + 1], sc_next[7] - cur[COL_SK + 2], sc_next[8] - cur[COL_SK + 3],
            sc_next[9] - cur[COL_POS2],
            sc_next[10], sc_next[11],
        ];
        for v in q4 { result[i] = ld_d * v; i += 1; }
        result[i] = first * (cur[COL_ACC2] - cur[COL_POS2]);
        i += 1;
        // the two notes differ: (position − position2) has an inverse
        result[i] = (cur[COL_POS] - cur[COL_POS2]) * cur[COL_INV] - one;
        i += 1;
        debug_assert_eq!(i, result.len());
    }

    fn get_assertions(&self) -> Vec<Assertion<Self::BaseField>> {
        let mut a = Vec::with_capacity(30);
        for k in 0..4 {
            a.push(Assertion::single(COL_STATE + 4 + k, ROW_ROOT, self.pi.root[k]));
        }
        for k in 0..4 {
            a.push(Assertion::single(COL_STATE + 4 + k, ROW_NF, self.pi.nf[k]));
        }
        a.push(Assertion::single(COL_ACC, ROW_NF, BaseElement::ZERO));
        a.push(Assertion::single(COL_PUBVAL, 0, self.pi.public_value));
        for o in 0..N_OUTS {
            for k in 0..4 {
                a.push(Assertion::single(COL_SB + 4 + k, row_cm_out(o), self.pi.cm_outs[o][k]));
            }
            a.push(Assertion::single(col_rem(o), RANGE_BITS, BaseElement::ZERO));
        }
        // lane C reaches the SAME root and reveals the second nullifier
        for k in 0..4 {
            a.push(Assertion::single(COL_SC + 4 + k, ROW_ROOT, self.pi.root[k]));
        }
        for k in 0..4 {
            a.push(Assertion::single(COL_SC + 4 + k, ROW_NF, self.pi.nf2[k]));
        }
        a.push(Assertion::single(COL_ACC2, ROW_NF, BaseElement::ZERO));
        a
    }

    fn context(&self) -> &AirContext<Self::BaseField> {
        &self.context
    }
}

pub struct SpendFullV7x2Prover {
    options: ProofOptions,
}
impl SpendFullV7x2Prover {
    pub fn new(options: ProofOptions) -> Self {
        Self { options }
    }
}
impl Prover for SpendFullV7x2Prover {
    type BaseField = BaseElement;
    type Air = SpendFullV7x2Air;
    type Trace = TraceTable<Self::BaseField>;
    type HashFn = Blake3_256<Self::BaseField>;
    type RandomCoin = DefaultRandomCoin<Self::HashFn>;
    type TraceLde<E: FieldElement<BaseField = Self::BaseField>> = DefaultTraceLde<E, Self::HashFn>;
    type ConstraintEvaluator<'a, E: FieldElement<BaseField = Self::BaseField>> =
        DefaultConstraintEvaluator<'a, Self::Air, E>;

    fn get_pub_inputs(&self, trace: &Self::Trace) -> SpendFullV7x2PublicInputs {
        let mut root = [BaseElement::ZERO; 4];
        let mut nf = [BaseElement::ZERO; 4];
        let mut nf2 = [BaseElement::ZERO; 4];
        let mut cm_outs = [[BaseElement::ZERO; 4]; N_OUTS];
        for k in 0..4 {
            root[k] = trace.get(COL_STATE + 4 + k, ROW_ROOT);
            nf[k] = trace.get(COL_STATE + 4 + k, ROW_NF);
            nf2[k] = trace.get(COL_SC + 4 + k, ROW_NF);
            for o in 0..N_OUTS {
                cm_outs[o][k] = trace.get(COL_SB + 4 + k, row_cm_out(o));
            }
        }
        SpendFullV7x2PublicInputs { root, nf, nf2, public_value: trace.get(COL_PUBVAL, 0), cm_outs }
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

impl HidingProver for SpendFullV7x2Prover {
    fn secrets(&self, trace: &Self::Trace) -> Vec<Secret> {
        use winterfell::Trace as _;
        let m = trace.main_segment();
        let mut v: Vec<Secret> = vec![("value", m.get(COL_VALUE, 0)), ("value 2", m.get(COL_VALUE2, 0)), ("position gap inverse", m.get(COL_INV, 0))];
        for k in 0..4 {
            v.push(("spend key", m.get(COL_SK + k, 0)));
            v.push(("blinding", m.get(COL_B + k, 0)));
            v.push(("blinding 2", m.get(COL_B2 + k, 0)));
            v.push(("owner pk", m.get(COL_PK + k, 0)));
            v.push(("inner commitment", m.get(COL_STATE + 4 + k, ROW_INNER)));
            v.push(("commitment", m.get(COL_STATE + 4 + k, ROW_CM)));
            v.push(("inner commitment 2", m.get(COL_SC + 4 + k, ROW_INNER)));
            v.push(("commitment 2", m.get(COL_SC + 4 + k, ROW_CM)));
            for o in 0..N_OUTS {
                v.push(("output blinding", m.get(col_ob(o) + k, 0)));
                v.push(("recipient pk", m.get(col_opk(o) + k, 0)));
                v.push(("output inner", m.get(COL_SB + 4 + k, row_inner_out(o))));
            }
        }
        for o in 0..N_OUTS {
            v.push(("output value", m.get(col_out(o), 0)));
        }
        v
    }
    fn zk_reserved_rows(&self, trace: &Self::Trace) -> usize {
        use winterfell::Trace as _;
        trace.main_segment().num_rows() - REAL_LEN
    }
}

/// Build the merge trace: two notes of the SAME spend key, their paths in the SAME tree.
/// Off-circuit facts are asserted here so a malformed witness fails loudly.
pub fn build_spend_full_v7x2_trace_seeded(
    notes: &[NoteV7; N_INS],
    paths: &[MerklePathV7; N_INS],
    public_value: BaseElement,
    outs: &[OutputV7; N_OUTS],
    options: &ProofOptions,
    mask_seed: [u8; 32],
) -> TraceTable<BaseElement> {
    assert_eq!(notes[0].spend_key, notes[1].spend_key, "a v7 merge spends two notes of ONE wallet");
    for (n, p) in notes.iter().zip(paths.iter()) {
        assert_eq!(p.leaf, n.commitment(), "each path must authenticate its note's commitment");
    }
    assert_ne!(paths[0].position(), paths[1].position(), "the two inputs must be different notes");
    assert_eq!(paths[0].compute_root(), paths[1].compute_root(), "both paths must climb to the same root");
    let bound = 1u128 << RANGE_BITS;
    let spent: u128 = public_value.as_int() as u128 + outs.iter().map(|o| o.value.as_int() as u128).sum::<u128>();
    let have: u128 = notes.iter().map(|n| n.value.as_int() as u128).sum();
    assert_eq!(spent, have, "non-conserving witness: public_value + Σ outputs must equal value + value2");
    assert!((public_value.as_int() as u128) < bound, "public value exceeds the range bound");
    for o in outs {
        assert!((o.value.as_int() as u128) < bound, "output value exceeds the range bound");
    }
    build_v7x2_trace_unchecked(notes, paths, public_value, outs, options, mask_seed)
}

/// Same without the off-circuit sanity checks, so tests can hand the prover a witness that
/// is wrong on purpose (including the same note twice).
pub fn build_v7x2_trace_unchecked(
    notes: &[NoteV7; N_INS],
    paths: &[MerklePathV7; N_INS],
    public_value: BaseElement,
    outs: &[OutputV7; N_OUTS],
    options: &ProofOptions,
    mask_seed: [u8; 32],
) -> TraceTable<BaseElement> {
    for p in paths {
        assert_eq!(p.siblings.len(), DEPTH, "v7 is fixed at depth {DEPTH}");
        assert_eq!(p.bits.len(), DEPTH);
    }
    let len = padded_len(REAL_LEN, options);
    assert_eq!(len, TRACE_LEN, "these options pad {REAL_LEN} rows to {len}, not {TRACE_LEN}");
    assert!(len - REAL_LEN >= zk_rows_for(options), "reserved randomness must exceed the opening count");

    let sk = notes[0].spend_key;
    let b = notes[0].blinding;
    let value = notes[0].value;
    let pk = notes[0].owner_pk().0;
    let position = BaseElement::new(paths[0].position());
    let sibs: Vec<[BaseElement; 4]> = paths[0].siblings.iter().map(|d| d.0).collect();
    let bits: Vec<bool> = paths[0].bits.clone();
    let b2 = notes[1].blinding;
    let value2 = notes[1].value;
    let position2 = BaseElement::new(paths[1].position());
    let sibs2: Vec<[BaseElement; 4]> = paths[1].siblings.iter().map(|d| d.0).collect();
    let bits2: Vec<bool> = paths[1].bits.clone();
    let gap = position - position2;
    // a zero gap (the same note twice) has no inverse: the constraint is unsatisfiable, and
    // the unchecked builder writes zero so the tampered-witness test can prove and be refused
    let inv = if gap == BaseElement::ZERO { BaseElement::ZERO } else { gap.inv() };
    let outs = *outs;

    let mut trace = TraceTable::new(WIDTH, len);
    trace.fill(
        |state| {
            for x in state.iter_mut() {
                *x = BaseElement::ZERO;
            }
            state[0] = BaseElement::new(5);
            state[4] = BaseElement::new(DOM_PK);
            state[5..9].copy_from_slice(&sk);
            state[COL_SK..COL_SK + 4].copy_from_slice(&sk);
            state[COL_B..COL_B + 4].copy_from_slice(&b);
            state[COL_VALUE] = value;
            state[COL_PK..COL_PK + 4].copy_from_slice(&pk);
            state[COL_POS] = position;
            state[COL_ACC] = position;
            state[COL_SB] = BaseElement::new(6);
            state[COL_SB + 4] = BaseElement::new(DOM_VAL);
            state[COL_SB + 5] = outs[0].value;
            state[COL_SB + 6..COL_SB + 10].copy_from_slice(&outs[0].blinding);
            for (o, out) in outs.iter().enumerate() {
                state[col_out(o)] = out.value;
                state[col_ob(o)..col_ob(o) + 4].copy_from_slice(&out.blinding);
                state[col_opk(o)..col_opk(o) + 4].copy_from_slice(&out.recipient_pk.0);
                state[col_rem(o)] = out.value;
                state[col_rbit(o)] = BaseElement::new(out.value.as_int() & 1);
            }
            state[COL_PUBVAL] = public_value;
            // lane C: an idle first permutation (zeros); its held witnesses from row 0
            state[COL_B2..COL_B2 + 4].copy_from_slice(&b2);
            state[COL_VALUE2] = value2;
            state[COL_POS2] = position2;
            state[COL_ACC2] = position2;
            state[COL_INV] = inv;
        },
        |step, state| {
            if step + 1 >= REAL_LEN {
                fill_random_row(&mask_seed, step + 1, state);
                return;
            }
            for o in 0..N_OUTS {
                let rem = state[col_rem(o)].as_int();
                let next = rem >> 1;
                state[col_rem(o)] = BaseElement::new(next);
                state[col_rbit(o)] = BaseElement::new(next & 1);
            }
            let r = step % SEG;
            if r != SEG - 1 {
                for base in [COL_STATE, COL_SB, COL_SC] {
                    let mut st = [BaseElement::ZERO; STATE];
                    st.copy_from_slice(&state[base..base + STATE]);
                    Rp64_256::apply_round(&mut st, r);
                    state[base..base + STATE].copy_from_slice(&st);
                }
                return;
            }
            let p = step / SEG;
            let mut digest = [BaseElement::ZERO; 4];
            digest.copy_from_slice(&state[COL_STATE + 4..COL_STATE + 8]);
            let mut digest_b = [BaseElement::ZERO; 4];
            digest_b.copy_from_slice(&state[COL_SB + 4..COL_SB + 8]);
            let mut digest_c = [BaseElement::ZERO; 4];
            digest_c.copy_from_slice(&state[COL_SC + 4..COL_SC + 8]);
            // lane B
            {
                let sb = &mut state[COL_SB..COL_SB + STATE];
                match p {
                    0 | 2 => {
                        let o = p / 2;
                        for x in sb.iter_mut() { *x = BaseElement::ZERO; }
                        sb[0] = BaseElement::new(8);
                        sb[4..8].copy_from_slice(&digest_b);
                        sb[8..12].copy_from_slice(&outs[o].recipient_pk.0);
                    }
                    1 => {
                        for x in sb.iter_mut() { *x = BaseElement::ZERO; }
                        sb[0] = BaseElement::new(6);
                        sb[4] = BaseElement::new(DOM_VAL);
                        sb[5] = outs[1].value;
                        sb[6..10].copy_from_slice(&outs[1].blinding);
                    }
                    _ => {}
                }
            }
            // lane C — the second input on lane A's schedule
            {
                let mut sc = [BaseElement::ZERO; STATE];
                match p {
                    0 => {
                        sc[0] = BaseElement::new(6);
                        sc[4] = BaseElement::new(DOM_VAL);
                        sc[5] = value2;
                        sc[6..10].copy_from_slice(&b2);
                    }
                    1 => {
                        sc[0] = BaseElement::new(8);
                        sc[4..8].copy_from_slice(&digest_c);
                        sc[8..12].copy_from_slice(&pk);
                    }
                    p if p >= 2 && p < 2 + DEPTH => {
                        let k = p - 2;
                        let (sib, bit) = (sibs2[k], bits2[k]);
                        sc[0] = BaseElement::new(8);
                        if bit {
                            sc[4..8].copy_from_slice(&sib);
                            sc[8..12].copy_from_slice(&digest_c);
                        } else {
                            sc[4..8].copy_from_slice(&digest_c);
                            sc[8..12].copy_from_slice(&sib);
                        }
                        state[COL_SIB2..COL_SIB2 + 4].copy_from_slice(&sib);
                        state[COL_BIT2] = if bit { BaseElement::ONE } else { BaseElement::ZERO };
                        if bit {
                            state[COL_ACC2] -= BaseElement::new(1u64 << k);
                        }
                    }
                    p if p == 2 + DEPTH => {
                        sc[0] = BaseElement::new(6);
                        sc[4] = BaseElement::new(DOM_NF);
                        sc[5..9].copy_from_slice(&sk);
                        sc[9] = position2;
                        state[COL_SIB2..COL_SIB2 + 4].copy_from_slice(&[BaseElement::ZERO; 4]);
                        state[COL_BIT2] = BaseElement::ZERO;
                    }
                    _ => unreachable!("row {} is past the last real permutation", step + 1),
                }
                state[COL_SC..COL_SC + STATE].copy_from_slice(&sc);
            }
            // lane A
            let st = &mut state[COL_STATE..COL_STATE + STATE];
            for x in st.iter_mut() {
                *x = BaseElement::ZERO;
            }
            match p {
                0 => {
                    st[0] = BaseElement::new(6);
                    st[4] = BaseElement::new(DOM_VAL);
                    st[5] = value;
                    st[6..10].copy_from_slice(&b);
                }
                1 => {
                    st[0] = BaseElement::new(8);
                    st[4..8].copy_from_slice(&digest);
                    st[8..12].copy_from_slice(&pk);
                }
                p if p >= 2 && p < 2 + DEPTH => {
                    let k = p - 2;
                    let (sib, bit) = (sibs[k], bits[k]);
                    st[0] = BaseElement::new(8);
                    if bit {
                        st[4..8].copy_from_slice(&sib);
                        st[8..12].copy_from_slice(&digest);
                    } else {
                        st[4..8].copy_from_slice(&digest);
                        st[8..12].copy_from_slice(&sib);
                    }
                    state[COL_SIB..COL_SIB + 4].copy_from_slice(&sib);
                    state[COL_BIT] = if bit { BaseElement::ONE } else { BaseElement::ZERO };
                    if bit {
                        state[COL_ACC] -= BaseElement::new(1u64 << k);
                    }
                }
                p if p == 2 + DEPTH => {
                    st[0] = BaseElement::new(6);
                    st[4] = BaseElement::new(DOM_NF);
                    st[5..9].copy_from_slice(&sk);
                    st[9] = position;
                    state[COL_SIB..COL_SIB + 4].copy_from_slice(&[BaseElement::ZERO; 4]);
                    state[COL_BIT] = BaseElement::ZERO;
                }
                _ => unreachable!("row {} is past the last real permutation", step + 1),
            }
        },
    );
    trace
}

/// Wallet entry point: fresh OS entropy for the mask.
pub fn build_spend_full_v7x2_trace(
    notes: &[NoteV7; N_INS],
    paths: &[MerklePathV7; N_INS],
    public_value: BaseElement,
    outs: &[OutputV7; N_OUTS],
    options: &ProofOptions,
) -> TraceTable<BaseElement> {
    use rand::RngCore;
    let mut seed = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut seed);
    build_spend_full_v7x2_trace_seeded(notes, paths, public_value, outs, options, seed)
}

type Coin = DefaultRandomCoin<Blake3_256<BaseElement>>;

pub use crate::spend_full_v7::V7VerifyError;

pub fn verify_spend_full_v7x2(proof: winterfell::Proof, pub_inputs: SpendFullV7x2PublicInputs) -> Result<(), V7VerifyError> {
    let ti = proof.trace_info();
    let got = ti.main_trace_width();
    if got != WIDTH {
        return Err(V7VerifyError::WrongTraceWidth { expected: WIDTH, got });
    }
    let got = ti.length();
    if got != TRACE_LEN {
        return Err(V7VerifyError::WrongTraceLength { expected: TRACE_LEN, got });
    }
    if (pub_inputs.public_value.as_int() as u128) >= (1u128 << RANGE_BITS) {
        return Err(V7VerifyError::PublicValueOutOfRange);
    }
    let min = winterfell::AcceptableOptions::MinConjecturedSecurity(ACCEPT_BITS);
    winterfell::verify::<SpendFullV7x2Air, Blake3_256<BaseElement>, Coin>(proof, pub_inputs, &min)
        .map_err(V7VerifyError::Verifier)
}

/// The off-circuit view of the public inputs, for callers that hold the notes.
pub fn public_inputs_for_2(root: Digest4, nfs: [Digest4; N_INS], public_value: u64, cm_outs: [Digest4; N_OUTS]) -> SpendFullV7x2PublicInputs {
    SpendFullV7x2PublicInputs {
        root: root.0,
        nf: nfs[0].0,
        nf2: nfs[1].0,
        public_value: BaseElement::new(public_value),
        cm_outs: [cm_outs[0].0, cm_outs[1].0],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::note_v7::{padding_leaf, PoolTreeV7};
    use winterfell::math::StarkField;

    fn e(v: u64) -> BaseElement { BaseElement::new(v) }
    const SK: [u64; 4] = [0xAAAA, 0xBBBB, 0xCCCC, 0xDDDD];

    fn note_a() -> NoteV7 { NoteV7::new(1_000_000, [0x1111, 0x2222, 0x3333, 0x4444], SK).unwrap() }
    fn note_b() -> NoteV7 { NoteV7::new(2_500_000, [0x5555, 0x6666, 0x7777, 0x8888], SK).unwrap() }
    fn bob() -> Digest4 { NoteV7::new(0, [0; 4], [0xB0B, 0xB0B, 0xB0B, 0xB0B]).unwrap().owner_pk() }
    /// 1,000,000 + 2,500,000 = 7 public + 3,000,000 to Bob + 499,993 change.
    fn outs(n: &NoteV7) -> (u64, [OutputV7; N_OUTS]) {
        (7, [OutputV7::new(3_000_000, [5, 6, 7, 8], bob()), OutputV7::new(499_993, [9, 10, 11, 12], n.owner_pk())])
    }
    fn pool_with(a: &NoteV7, pa: usize, b: &NoteV7, pb: usize) -> (PoolTreeV7, MerklePathV7, MerklePathV7) {
        let mut leaves = vec![padding_leaf(); 1 << DEPTH];
        for i in [1usize, 2, 100, 9_000, 32_767] {
            leaves[i] = NoteV7::new(i as u64 + 7, [i as u64; 4], [1, 2, 3, i as u64]).unwrap().commitment();
        }
        leaves[pa] = a.commitment();
        leaves[pb] = b.commitment();
        let tree = PoolTreeV7::new(leaves).unwrap();
        let p1 = tree.path(pa).unwrap();
        let p2 = tree.path(pb).unwrap();
        (tree, p1, p2)
    }
    fn seed() -> [u8; 32] { [7u8; 32] }
    fn prove(t: TraceTable<BaseElement>) -> winterfell::Proof {
        SpendFullV7x2Prover::new(v7_options()).prove(t).expect("prove")
    }

    /// IN-CIRCUIT == OFF-CIRCUIT (debug): lane C's digest rows are note B's primitives, both
    /// lanes reach the tree's root, both accumulators consume their positions.
    #[test]
    fn trace_digests_match_both_notes() {
        let (a, b) = (note_a(), note_b());
        let (tree, pa, pb) = pool_with(&a, 21_845, &b, 300);
        let (pv, o) = outs(&a);
        let t = build_spend_full_v7x2_trace_seeded(&[a, b], &[pa, pb], e(pv), &o, &v7_options(), seed());
        let dig = |base: usize, row: usize| -> Digest4 {
            let mut d = [BaseElement::ZERO; 4];
            for k in 0..4 { d[k] = t.get(base + 4 + k, row); }
            Digest4(d)
        };
        assert_eq!(dig(COL_STATE, ROW_PK), a.owner_pk());
        assert_eq!(dig(COL_STATE, ROW_CM), a.commitment());
        assert_eq!(dig(COL_SC, ROW_INNER), b.inner_commitment(), "P1' = inner 2");
        assert_eq!(dig(COL_SC, ROW_CM), b.commitment(), "P2' = commitment 2");
        assert_eq!(dig(COL_STATE, ROW_ROOT), tree.root());
        assert_eq!(dig(COL_SC, ROW_ROOT), tree.root(), "lane C climbs to the same root");
        assert_eq!(dig(COL_STATE, ROW_NF), a.nullifier(21_845));
        assert_eq!(dig(COL_SC, ROW_NF), b.nullifier(300), "P18' = nullifier 2");
        assert_eq!(t.get(COL_ACC, ROW_NF), BaseElement::ZERO);
        assert_eq!(t.get(COL_ACC2, ROW_NF), BaseElement::ZERO);
        assert_eq!((t.get(COL_POS, 0) - t.get(COL_POS2, 0)) * t.get(COL_INV, 0), BaseElement::ONE);
        assert_eq!(dig(COL_SB, row_cm_out(0)), o[0].commitment());
        assert_eq!(dig(COL_SB, row_cm_out(1)), o[1].commitment());
    }

    /// ROUND TRIP + HIDING through the verifier.
    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn honest_merge_proves_and_verifies() {
        let (a, b) = (note_a(), note_b());
        let (tree, pa, pb) = pool_with(&a, 21_845, &b, 300);
        let (pv, o) = outs(&a);
        let t = build_spend_full_v7x2_trace_seeded(&[a, b], &[pa, pb], e(pv), &o, &v7_options(), seed());
        let proof = SpendFullV7x2Prover::new(v7_options()).prove_hiding(t).expect("prove (hiding)");
        let pi = public_inputs_for_2(tree.root(), [a.nullifier(21_845), b.nullifier(300)], pv, [o[0].commitment(), o[1].commitment()]);
        verify_spend_full_v7x2(proof.clone(), pi.clone()).expect("an honest v7 merge must verify");
        // swapped nullifiers, a wrong second nullifier, another root: refused
        let mut bad = pi.clone();
        std::mem::swap(&mut bad.nf, &mut bad.nf2);
        assert!(verify_spend_full_v7x2(proof.clone(), bad).is_err(), "the nullifiers are lane-bound");
        let mut bad = pi.clone();
        bad.nf2 = b.nullifier(301).0;
        assert!(verify_spend_full_v7x2(proof.clone(), bad).is_err(), "nullifier 2 at another position");
        let mut bad = pi;
        bad.root[1] += BaseElement::ONE;
        assert!(verify_spend_full_v7x2(proof, bad).is_err(), "tampered root");
    }

    /// THE SAME NOTE TWICE cannot be proven: the position-gap inverse has no value that
    /// satisfies `(pos − pos2)·inv = 1`, so the prover's own constraint check fails (and a
    /// prover that skipped it would produce a proof the verifier rejects).
    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn the_same_note_twice_is_not_a_merge() {
        let a = note_a();
        let (tree, pa, _) = pool_with(&a, 777, &note_b(), 300);
        let o = [OutputV7::new(1_999_993, [5, 6, 7, 8], bob()), OutputV7::new(0, [9, 10, 11, 12], a.owner_pk())];
        let t = build_v7x2_trace_unchecked(&[a, a], &[pa.clone(), pa], e(7), &o, &v7_options(), seed());
        let pi = public_inputs_for_2(tree.root(), [a.nullifier(777), a.nullifier(777)], 7, [o[0].commitment(), o[1].commitment()]);
        let verdict = match SpendFullV7x2Prover::new(v7_options()).prove(t) {
            Ok(proof) => verify_spend_full_v7x2(proof, pi).is_err(),
            Err(_) => true,
        };
        assert!(verdict, "one note fed in as both inputs must not yield an accepted proof");
    }

    /// TAMPERED WITNESSES: a foreign second note (other key), an inflated second value, and
    /// a non-conserving output set are refused through the verifier.
    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn tampered_second_input_does_not_verify() {
        let (a, b) = (note_a(), note_b());
        let (tree, pa, pb) = pool_with(&a, 777, &b, 300);
        let (pv, o) = outs(&a);
        let cms = [o[0].commitment(), o[1].commitment()];
        let opt = v7_options();
        // note B under a foreign key: its leaf is not in the tree, the climb misses the root
        let mallory = NoteV7::new(2_500_000, b.blinding.map(|x| x.as_int()), [9, 9, 9, 9]).unwrap();
        let t = build_v7x2_trace_unchecked(&[a, mallory], &[pa.clone(), pb.clone()], e(pv), &o, &opt, seed());
        let pi = public_inputs_for_2(tree.root(), [a.nullifier(777), mallory.nullifier(300)], pv, cms);
        assert!(verify_spend_full_v7x2(prove(t), pi).is_err(), "a foreign second key must not spend a known note");
        // richer second note under the same blinding, outputs inflated to match
        let richer = NoteV7::new(9_000_000, b.blinding.map(|x| x.as_int()), SK).unwrap();
        let mut o2 = o;
        o2[1].value = e(1_000_000 + 9_000_000 - 3_000_000 - 7);
        let t = build_v7x2_trace_unchecked(&[a, richer], &[pa.clone(), pb.clone()], e(pv), &o2, &opt, seed());
        let pi = public_inputs_for_2(tree.root(), [a.nullifier(777), b.nullifier(300)], pv, [o2[0].commitment(), o2[1].commitment()]);
        assert!(verify_spend_full_v7x2(prove(t), pi).is_err(), "a different second value must not open its leaf");
        // honest inputs, inflated output: conservation fails
        let mut o3 = o;
        o3[0].value = e(3_000_001);
        let t = build_v7x2_trace_unchecked(&[a, b], &[pa, pb], e(pv), &o3, &opt, seed());
        let pi = public_inputs_for_2(tree.root(), [a.nullifier(777), b.nullifier(300)], pv, [o3[0].commitment(), o3[1].commitment()]);
        assert!(verify_spend_full_v7x2(prove(t), pi).is_err(), "outputs exceeding the inputs must not verify");
    }

    /// SHAPE GUARD: a single-input v7 proof is refused by the merge verifier (and vice
    /// versa) by width, before any AIR is constructed.
    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn single_input_proof_is_refused_by_width() {
        use crate::spend_full_v7::{build_spend_full_v7_trace_seeded, SpendFullV7Prover};
        let a = note_a();
        let (tree, pa, _) = pool_with(&a, 5, &note_b(), 300);
        let o = [OutputV7::new(999_993, [5, 6, 7, 8], bob()), OutputV7::new(0, [9, 10, 11, 12], a.owner_pk())];
        let t = build_spend_full_v7_trace_seeded(&a, &pa, e(7), &o, &v7_options(), seed());
        let single = SpendFullV7Prover::new(v7_options()).prove(t).expect("v7 proof");
        let pi = public_inputs_for_2(tree.root(), [a.nullifier(5), a.nullifier(6)], 7, [o[0].commitment(), o[1].commitment()]);
        match verify_spend_full_v7x2(single, pi) {
            Err(V7VerifyError::WrongTraceWidth { expected, got }) => { assert_eq!(expected, WIDTH); assert_eq!(got, WIDTH_V7); }
            other => panic!("expected a width refusal, got {other:?}"),
        }
    }
}
