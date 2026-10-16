//! v7 WIDE MERGE — the K-input, two-output Rescue-Prime spend AIR (2026-10-16).
//!
//! Viktor, 2026-10-16: *"go ahead with the wider merge circuit on r1"*. A payment spends at most
//! two notes today, so a wallet holding thousands of small mined notes must merge them two at a
//! time — ~37,000 proofs for Viktor's phone. With K inputs one proof consumes K notes, so the
//! same clean-up takes (n−1)/(K−1) proofs, and most payments need no merge at all.
//!
//! This is [`crate::spend_full_v7_2`] with its second-input lane repeated K−1 times:
//!
//! ```text
//!   lane A     pk = H(DOM_PK ‖ sk); input 0 opened, climbed to root, nf_0 revealed
//!   lane j     input j (j = 1..K): inner_j, cm_j = merge(inner_j, pk) — the SAME owner key —
//!              climb(cm_j, path_j) = the SAME root, nf_j = H(DOM_NF ‖ sk ‖ position_j)
//!   lane B     the two outputs, exactly as in v7
//!   money      Σ value_j = public_value + out_0 + out_1,   out_i < 2^RANGE_BITS
//!   distinct   (position_i − position_j) · inv_ij = 1   for every pair i < j
//! ```
//!
//! # Layout
//!
//! Columns 0..67 are v7's lanes A and B. Input j ≥ 1 owns 24 columns from
//! `67 + 24·(j−1)`: Rescue state (12), blinding (4), value, position (held), sibling (4), bit
//! (segment-constant), position accumulator. After the K−1 lanes come K(K−1)/2 held inverse
//! columns, one per input pair. At K = 2 this is byte-for-byte [`crate::spend_full_v7_2`]'s
//! layout (92 columns) — pinned by a test — so the wide circuit generalises the deployed merge
//! rather than replacing it.
//!
//! Distinctness is pairwise, K(K−1)/2 columns: 6 at K = 4, 28 at K = 8, 120 at K = 16. A
//! sorted-positions form (K−1 range-checked gaps) is cheaper for large K and is the follow-up
//! if K = 16 is chosen; the measurements decide.
//!
//! Trace length stays 256 rows; every input lane runs lane A's schedule with the SAME periodic
//! selectors, so K only widens the trace.

use winterfell::{
    crypto::{hashers::Blake3_256, hashers::Rp64_256, DefaultRandomCoin},
    math::{fields::f64::BaseElement, FieldElement, StarkField, ToElements},
    matrix::ColMatrix,
    Air, AirContext, Assertion, AuxRandElements, ConstraintCompositionCoefficients,
    DefaultConstraintEvaluator, DefaultTraceLde, EvaluationFrame, ProofOptions, Prover,
    StarkDomain, TraceInfo, TracePolyTable, TraceTable, TransitionConstraintDegree,
};

use crate::mimc::ACCEPT_BITS;
use crate::note_v7::{Digest4, MerklePathV7, NoteV7, DOM_NF, DOM_PK, DOM_VAL};
pub use crate::spend_full_v7::OutputV7;
use crate::spend_full_v7::{DEPTH, N_OUTS, RANGE_BITS, REAL_LEN, TRACE_LEN};
use crate::zk_mask::{exemptions_for, fill_random_row, padded_len, zk_rows_for, HidingProver, Secret};

/// The widest merge this layout can prove: winterfell 0.9 caps a trace at 255 columns, and
/// width(7) = 232 while width(8) = 263 (measured 2026-10-16 — the bench panicked at K = 8). Wider
/// needs inputs laid out down the rows instead of across the columns.
pub const MAX_INS: usize = 7;

/// The wide-merge arities the NODE accepts (Viktor 2026-10-16: "go ahead with k=4 on r1"). One
/// size, not 3..=7: every accepted input count is visible on chain and splits the anonymity set.
pub const ACCEPTED_WIDE: &[usize] = &[4];

const STATE: usize = 12;
const SEG: usize = 8;

// Lane A (input 0) — identical to spend_full_v7.
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

// An extra input lane: offsets within its 24 columns.
const LANE_W: usize = 24;
const L_STATE: usize = 0;
const L_B: usize = 12;
const L_VALUE: usize = 16;
const L_POS: usize = 17;
const L_SIB: usize = 18;
const L_BIT: usize = 22;
const L_ACC: usize = 23;

/// First column of input `j`'s lane (j ≥ 1).
const fn lane(j: usize) -> usize { WIDTH_V7 + LANE_W * (j - 1) }
/// Number of input pairs.
const fn pairs(k: usize) -> usize { k * (k - 1) / 2 }
/// Column of the held inverse for the pair (i, j), i < j, in a K-input trace.
fn col_inv(k: usize, i: usize, j: usize) -> usize {
    debug_assert!(i < j && j < k);
    // pairs in order (0,1),(0,2)…(0,k-1),(1,2)… — index of (i, j)
    let before: usize = (0..i).map(|r| k - 1 - r).sum();
    WIDTH_V7 + LANE_W * (k - 1) + before + (j - i - 1)
}
/// Column holding input `j`'s position.
const fn col_pos(j: usize) -> usize { if j == 0 { COL_POS } else { lane(j) + L_POS } }
/// Column holding input `j`'s value.
const fn col_value(j: usize) -> usize { if j == 0 { COL_VALUE } else { lane(j) + L_VALUE } }

/// Trace width of a K-input merge: 67 + 24·(K−1) + K(K−1)/2. K = 2 → 92 (= spend_full_v7_2).
pub const fn width(k: usize) -> usize { WIDTH_V7 + LANE_W * (k - 1) + pairs(k) }

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
const fn row_cm_out(i: usize) -> usize { 2 * SEG * i + 2 * SEG - 1 }
const fn row_inner_out(i: usize) -> usize { 2 * SEG * i + SEG - 1 }

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

const N_LANE_A: usize = 98;
const N_LANE_B: usize = 12 + 48 + 19 + 1 + 6;
/// Per extra input: round 12, held 6 (b4, value, pos), segment 5, bool 1, acc 1, loads 48, first·(acc−pos) 1.
const N_LANE_X: usize = 12 + 6 + 5 + 1 + 1 + 12 + 12 + 12 + 12 + 1;
/// Per pair: inverse held 1, (pos_i − pos_j)·inv − 1.
const N_PAIR: usize = 2;
fn n_constraints(k: usize) -> usize { N_LANE_A + N_LANE_B + (k - 1) * N_LANE_X + pairs(k) * N_PAIR }

/// The verifier's view: the shared root, EVERY nullifier (input order), the public value, the outputs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpendFullV7kPublicInputs {
    pub root: [BaseElement; 4],
    pub nfs: Vec<[BaseElement; 4]>,
    pub public_value: BaseElement,
    pub cm_outs: [[BaseElement; 4]; N_OUTS],
}
impl ToElements<BaseElement> for SpendFullV7kPublicInputs {
    fn to_elements(&self) -> Vec<BaseElement> {
        let mut v = self.root.to_vec();
        for nf in &self.nfs {
            v.extend_from_slice(nf);
        }
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

pub struct SpendFullV7kAir {
    context: AirContext<BaseElement>,
    pi: SpendFullV7kPublicInputs,
    k: usize,
}

impl Air for SpendFullV7kAir {
    type BaseField = BaseElement;
    type PublicInputs = SpendFullV7kPublicInputs;
    type GkrProof = ();
    type GkrVerifier = ();

    fn new(trace_info: TraceInfo, pub_inputs: SpendFullV7kPublicInputs, options: ProofOptions) -> Self {
        let k = pub_inputs.nfs.len();
        assert!((2..=MAX_INS).contains(&k), "a wide merge takes 2..={MAX_INS} inputs, not {k}");
        assert_eq!(width(k), trace_info.width());
        let n = trace_info.length();
        assert_eq!(n, TRACE_LEN, "v7 merge traces are exactly {TRACE_LEN} rows");
        let d7 = || TransitionConstraintDegree::with_cycles(7, vec![SEG]);
        let d1 = || TransitionConstraintDegree::new(1);
        let d1s = || TransitionConstraintDegree::with_cycles(1, vec![SEG]);
        let d1n = || TransitionConstraintDegree::with_cycles(1, vec![n]);
        let d2n = || TransitionConstraintDegree::with_cycles(2, vec![n]);
        let mut degrees = Vec::with_capacity(n_constraints(k));
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
        // ── each extra input lane ──
        for _ in 1..k {
            for _ in 0..STATE { degrees.push(d7()); }                       // round
            for _ in 0..6 { degrees.push(d1()); }                           // held: b[4], value, pos
            for _ in 0..5 { degrees.push(d1s()); }                          // sib, bit segment-constant
            degrees.push(TransitionConstraintDegree::new(2));               // bit boolean
            degrees.push(TransitionConstraintDegree::with_cycles(1, vec![SEG, n])); // acc
            for _ in 0..(12 + 12) { degrees.push(d1n()); }                  // ldA, ldB loads
            for _ in 0..12 { degrees.push(d2n()); }                         // ldC
            for _ in 0..13 { degrees.push(d1n()); }                         // ldD, first·(acc − pos)
        }
        // ── distinctness, every pair ──
        for _ in 0..pairs(k) {
            degrees.push(d1());                                             // inverse held
            degrees.push(TransitionConstraintDegree::new(2));               // (pos_i − pos_j)·inv − 1
        }
        debug_assert_eq!(degrees.len(), n_constraints(k));
        let num_assertions = 4 + 4 + 1 + 1 + 4 * N_OUTS + N_OUTS + (k - 1) * (4 + 4 + 1);
        SpendFullV7kAir {
            context: AirContext::new(trace_info, degrees, num_assertions, options)
                .set_num_transition_exemptions(exemptions_for(n, REAL_LEN)),
            pi: pub_inputs,
            k,
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
        let k_in = self.k;
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

        // ══ lane B — exactly spend_full_v7, conservation over every input ═══════
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
        // ── conservation: Σ value_j = public_value + Σ out_i ──
        let mut spent = cur[COL_PUBVAL];
        for o in 0..N_OUTS {
            spent += cur[col_out(o)];
        }
        let mut have = E::ZERO;
        for j in 0..k_in {
            have += cur[col_value(j)];
        }
        result[i] = first * (have - spent);
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

        // ══ input lanes 1..K — lane A's schedule from row 8 ═════════════════════
        for j in 1..k_in {
            let base = lane(j);
            let sc = &cur[base + L_STATE..base + L_STATE + STATE];
            let sc_next = &nxt[base + L_STATE..base + L_STATE + STATE];
            round_constraints(cur, nxt, base + L_STATE, periodic, s, &mut result[i..i + STATE]);
            i += STATE;
            // held: blinding, value, position
            for c in base + L_B..=base + L_POS {
                result[i] = nxt[c] - cur[c];
                i += 1;
            }
            // per-segment constants: sibling, bit
            for c in base + L_SIB..=base + L_BIT {
                result[i] = (one - s) * (nxt[c] - cur[c]);
                i += 1;
            }
            let bitj = cur[base + L_BIT];
            result[i] = bitj * (bitj - one);
            i += 1;
            let nbitj = nxt[base + L_BIT];
            result[i] = nxt[base + L_ACC] - cur[base + L_ACC] + s * nbitj * pw;
            i += 1;
            // ldA (row 7): inner_j input = (6,0,0,0, DOM_VAL, value_j, b_j[4], 0,0)
            let q1 = [
                sc_next[0] - six, sc_next[1], sc_next[2], sc_next[3],
                sc_next[4] - dom_val,
                sc_next[5] - cur[base + L_VALUE],
                sc_next[6] - cur[base + L_B], sc_next[7] - cur[base + L_B + 1], sc_next[8] - cur[base + L_B + 2], sc_next[9] - cur[base + L_B + 3],
                sc_next[10], sc_next[11],
            ];
            for v in q1 { result[i] = ld_a * v; i += 1; }
            // ldB (row 15): cm_j input = (8,0,0,0, inner_j, pk) — the SAME owner key
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
                let sib = nxt[base + L_SIB + k];
                let left = node + nbitj * (sib - node);
                let right = sib + nbitj * (node - sib);
                q3[4 + k] = sc_next[4 + k] - left;
                q3[8 + k] = sc_next[8 + k] - right;
            }
            for v in q3 { result[i] = ld_c * v; i += 1; }
            // ldD: nf_j input = (6,0,0,0, DOM_NF, sk[4], position_j, 0,0) — the SAME spend key
            let q4 = [
                sc_next[0] - six, sc_next[1], sc_next[2], sc_next[3],
                sc_next[4] - dom_nf,
                sc_next[5] - cur[COL_SK], sc_next[6] - cur[COL_SK + 1], sc_next[7] - cur[COL_SK + 2], sc_next[8] - cur[COL_SK + 3],
                sc_next[9] - cur[base + L_POS],
                sc_next[10], sc_next[11],
            ];
            for v in q4 { result[i] = ld_d * v; i += 1; }
            result[i] = first * (cur[base + L_ACC] - cur[base + L_POS]);
            i += 1;
        }

        // ══ every pair of inputs differs ════════════════════════════════════════
        for a in 0..k_in {
            for b in (a + 1)..k_in {
                let c = col_inv(k_in, a, b);
                result[i] = nxt[c] - cur[c];
                result[i + 1] = (cur[col_pos(a)] - cur[col_pos(b)]) * cur[c] - one;
                i += 2;
            }
        }
        debug_assert_eq!(i, result.len());
    }

    fn get_assertions(&self) -> Vec<Assertion<Self::BaseField>> {
        let mut a = Vec::with_capacity(20 + 9 * (self.k - 1));
        for k in 0..4 {
            a.push(Assertion::single(COL_STATE + 4 + k, ROW_ROOT, self.pi.root[k]));
        }
        for k in 0..4 {
            a.push(Assertion::single(COL_STATE + 4 + k, ROW_NF, self.pi.nfs[0][k]));
        }
        a.push(Assertion::single(COL_ACC, ROW_NF, BaseElement::ZERO));
        a.push(Assertion::single(COL_PUBVAL, 0, self.pi.public_value));
        for o in 0..N_OUTS {
            for k in 0..4 {
                a.push(Assertion::single(COL_SB + 4 + k, row_cm_out(o), self.pi.cm_outs[o][k]));
            }
            a.push(Assertion::single(col_rem(o), RANGE_BITS, BaseElement::ZERO));
        }
        // every input lane reaches the SAME root and reveals its nullifier
        for j in 1..self.k {
            let base = lane(j);
            for k in 0..4 {
                a.push(Assertion::single(base + L_STATE + 4 + k, ROW_ROOT, self.pi.root[k]));
            }
            for k in 0..4 {
                a.push(Assertion::single(base + L_STATE + 4 + k, ROW_NF, self.pi.nfs[j][k]));
            }
            a.push(Assertion::single(base + L_ACC, ROW_NF, BaseElement::ZERO));
        }
        a
    }

    fn context(&self) -> &AirContext<Self::BaseField> {
        &self.context
    }
}

pub struct SpendFullV7kProver {
    options: ProofOptions,
}
impl SpendFullV7kProver {
    pub fn new(options: ProofOptions) -> Self {
        Self { options }
    }
}
/// Inputs in a trace, from its width (inverse of [`width`]).
fn k_of_width(w: usize) -> usize {
    (2..=MAX_INS).find(|&k| width(k) == w).expect("not a wide-merge trace width")
}
impl Prover for SpendFullV7kProver {
    type BaseField = BaseElement;
    type Air = SpendFullV7kAir;
    type Trace = TraceTable<Self::BaseField>;
    type HashFn = Blake3_256<Self::BaseField>;
    type RandomCoin = DefaultRandomCoin<Self::HashFn>;
    type TraceLde<E: FieldElement<BaseField = Self::BaseField>> = DefaultTraceLde<E, Self::HashFn>;
    type ConstraintEvaluator<'a, E: FieldElement<BaseField = Self::BaseField>> =
        DefaultConstraintEvaluator<'a, Self::Air, E>;

    fn get_pub_inputs(&self, trace: &Self::Trace) -> SpendFullV7kPublicInputs {
        use winterfell::Trace as _;
        let k = k_of_width(trace.main_segment().num_cols());
        let mut root = [BaseElement::ZERO; 4];
        let mut nfs = vec![[BaseElement::ZERO; 4]; k];
        let mut cm_outs = [[BaseElement::ZERO; 4]; N_OUTS];
        for x in 0..4 {
            root[x] = trace.get(COL_STATE + 4 + x, ROW_ROOT);
            nfs[0][x] = trace.get(COL_STATE + 4 + x, ROW_NF);
            for j in 1..k {
                nfs[j][x] = trace.get(lane(j) + L_STATE + 4 + x, ROW_NF);
            }
            for o in 0..N_OUTS {
                cm_outs[o][x] = trace.get(COL_SB + 4 + x, row_cm_out(o));
            }
        }
        SpendFullV7kPublicInputs { root, nfs, public_value: trace.get(COL_PUBVAL, 0), cm_outs }
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

impl HidingProver for SpendFullV7kProver {
    fn secrets(&self, trace: &Self::Trace) -> Vec<Secret> {
        use winterfell::Trace as _;
        let m = trace.main_segment();
        let k = k_of_width(m.num_cols());
        let mut v: Vec<Secret> = vec![("value", m.get(COL_VALUE, 0))];
        for x in 0..4 {
            v.push(("spend key", m.get(COL_SK + x, 0)));
            v.push(("blinding", m.get(COL_B + x, 0)));
            v.push(("owner pk", m.get(COL_PK + x, 0)));
            v.push(("inner commitment", m.get(COL_STATE + 4 + x, ROW_INNER)));
            v.push(("commitment", m.get(COL_STATE + 4 + x, ROW_CM)));
            for o in 0..N_OUTS {
                v.push(("output blinding", m.get(col_ob(o) + x, 0)));
                v.push(("recipient pk", m.get(col_opk(o) + x, 0)));
                v.push(("output inner", m.get(COL_SB + 4 + x, row_inner_out(o))));
            }
        }
        for j in 1..k {
            let base = lane(j);
            v.push(("input value", m.get(base + L_VALUE, 0)));
            for x in 0..4 {
                v.push(("input blinding", m.get(base + L_B + x, 0)));
                v.push(("input inner commitment", m.get(base + L_STATE + 4 + x, ROW_INNER)));
                v.push(("input commitment", m.get(base + L_STATE + 4 + x, ROW_CM)));
            }
        }
        for a in 0..k {
            for b in (a + 1)..k {
                v.push(("position gap inverse", m.get(col_inv(k, a, b), 0)));
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

/// Build the K-input merge trace: K notes of the SAME spend key, their paths in the SAME tree.
/// Off-circuit facts are asserted here so a malformed witness fails loudly.
pub fn build_spend_full_v7k_trace_seeded(
    notes: &[NoteV7],
    paths: &[MerklePathV7],
    public_value: BaseElement,
    outs: &[OutputV7; N_OUTS],
    options: &ProofOptions,
    mask_seed: [u8; 32],
) -> TraceTable<BaseElement> {
    let k = notes.len();
    assert!((2..=MAX_INS).contains(&k), "a wide merge takes 2..={MAX_INS} inputs");
    assert_eq!(paths.len(), k, "one path per input");
    for n in &notes[1..] {
        assert_eq!(n.spend_key, notes[0].spend_key, "a v7 merge spends notes of ONE wallet");
    }
    for (n, p) in notes.iter().zip(paths.iter()) {
        assert_eq!(p.leaf, n.commitment(), "each path must authenticate its note's commitment");
        assert_eq!(p.compute_root(), paths[0].compute_root(), "every path must climb to the same root");
    }
    let mut seen = std::collections::BTreeSet::new();
    for p in paths {
        assert!(seen.insert(p.position()), "the inputs must be different notes");
    }
    let bound = 1u128 << RANGE_BITS;
    let spent: u128 = public_value.as_int() as u128 + outs.iter().map(|o| o.value.as_int() as u128).sum::<u128>();
    let have: u128 = notes.iter().map(|n| n.value.as_int() as u128).sum();
    assert_eq!(spent, have, "non-conserving witness: public_value + Σ outputs must equal Σ inputs");
    assert!((public_value.as_int() as u128) < bound, "public value exceeds the range bound");
    for o in outs {
        assert!((o.value.as_int() as u128) < bound, "output value exceeds the range bound");
    }
    build_v7k_trace_unchecked(notes, paths, public_value, outs, options, mask_seed)
}

/// Same without the off-circuit sanity checks, so tests can hand the prover a witness that is
/// wrong on purpose (including one note twice).
pub fn build_v7k_trace_unchecked(
    notes: &[NoteV7],
    paths: &[MerklePathV7],
    public_value: BaseElement,
    outs: &[OutputV7; N_OUTS],
    options: &ProofOptions,
    mask_seed: [u8; 32],
) -> TraceTable<BaseElement> {
    let k = notes.len();
    assert!((2..=MAX_INS).contains(&k) && paths.len() == k);
    for p in paths {
        assert_eq!(p.siblings.len(), DEPTH, "v7 is fixed at depth {DEPTH}");
        assert_eq!(p.bits.len(), DEPTH);
    }
    let len = padded_len(REAL_LEN, options);
    assert_eq!(len, TRACE_LEN, "these options pad {REAL_LEN} rows to {len}, not {TRACE_LEN}");
    assert!(len - REAL_LEN >= zk_rows_for(options), "reserved randomness must exceed the opening count");

    let sk = notes[0].spend_key;
    let pk = notes[0].owner_pk().0;
    let positions: Vec<BaseElement> = paths.iter().map(|p| BaseElement::new(p.position())).collect();
    let sibs: Vec<Vec<[BaseElement; 4]>> = paths.iter().map(|p| p.siblings.iter().map(|d| d.0).collect()).collect();
    let bits: Vec<Vec<bool>> = paths.iter().map(|p| p.bits.clone()).collect();
    let outs = *outs;
    let w = width(k);

    let mut trace = TraceTable::new(w, len);
    trace.fill(
        |state| {
            for x in state.iter_mut() {
                *x = BaseElement::ZERO;
            }
            state[0] = BaseElement::new(5);
            state[4] = BaseElement::new(DOM_PK);
            state[5..9].copy_from_slice(&sk);
            state[COL_SK..COL_SK + 4].copy_from_slice(&sk);
            state[COL_B..COL_B + 4].copy_from_slice(&notes[0].blinding);
            state[COL_VALUE] = notes[0].value;
            state[COL_PK..COL_PK + 4].copy_from_slice(&pk);
            state[COL_POS] = positions[0];
            state[COL_ACC] = positions[0];
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
            // input lanes: an idle first permutation (zeros); held witnesses from row 0
            for j in 1..k {
                let base = lane(j);
                state[base + L_B..base + L_B + 4].copy_from_slice(&notes[j].blinding);
                state[base + L_VALUE] = notes[j].value;
                state[base + L_POS] = positions[j];
                state[base + L_ACC] = positions[j];
            }
            // pairwise gap inverses; a zero gap (one note twice) has none — write zero so the
            // tampered-witness test can try to prove and be refused
            for a in 0..k {
                for b in (a + 1)..k {
                    let gap = positions[a] - positions[b];
                    state[col_inv(k, a, b)] = if gap == BaseElement::ZERO { BaseElement::ZERO } else { gap.inv() };
                }
            }
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
                let mut bases = vec![COL_STATE, COL_SB];
                bases.extend((1..k).map(|j| lane(j) + L_STATE));
                for base in bases {
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
            // input lanes 1..K — lane A's schedule
            for j in 1..k {
                let base = lane(j);
                let mut digest_j = [BaseElement::ZERO; 4];
                digest_j.copy_from_slice(&state[base + L_STATE + 4..base + L_STATE + 8]);
                let mut sc = [BaseElement::ZERO; STATE];
                match p {
                    0 => {
                        sc[0] = BaseElement::new(6);
                        sc[4] = BaseElement::new(DOM_VAL);
                        sc[5] = notes[j].value;
                        sc[6..10].copy_from_slice(&notes[j].blinding);
                    }
                    1 => {
                        sc[0] = BaseElement::new(8);
                        sc[4..8].copy_from_slice(&digest_j);
                        sc[8..12].copy_from_slice(&pk);
                    }
                    p if p >= 2 && p < 2 + DEPTH => {
                        let lvl = p - 2;
                        let (sib, bit) = (sibs[j][lvl], bits[j][lvl]);
                        sc[0] = BaseElement::new(8);
                        if bit {
                            sc[4..8].copy_from_slice(&sib);
                            sc[8..12].copy_from_slice(&digest_j);
                        } else {
                            sc[4..8].copy_from_slice(&digest_j);
                            sc[8..12].copy_from_slice(&sib);
                        }
                        state[base + L_SIB..base + L_SIB + 4].copy_from_slice(&sib);
                        state[base + L_BIT] = if bit { BaseElement::ONE } else { BaseElement::ZERO };
                        if bit {
                            state[base + L_ACC] -= BaseElement::new(1u64 << lvl);
                        }
                    }
                    p if p == 2 + DEPTH => {
                        sc[0] = BaseElement::new(6);
                        sc[4] = BaseElement::new(DOM_NF);
                        sc[5..9].copy_from_slice(&sk);
                        sc[9] = positions[j];
                        state[base + L_SIB..base + L_SIB + 4].copy_from_slice(&[BaseElement::ZERO; 4]);
                        state[base + L_BIT] = BaseElement::ZERO;
                    }
                    _ => unreachable!("row {} is past the last real permutation", step + 1),
                }
                state[base + L_STATE..base + L_STATE + STATE].copy_from_slice(&sc);
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
                    st[5] = notes[0].value;
                    st[6..10].copy_from_slice(&notes[0].blinding);
                }
                1 => {
                    st[0] = BaseElement::new(8);
                    st[4..8].copy_from_slice(&digest);
                    st[8..12].copy_from_slice(&pk);
                }
                p if p >= 2 && p < 2 + DEPTH => {
                    let lvl = p - 2;
                    let (sib, bit) = (sibs[0][lvl], bits[0][lvl]);
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
                        state[COL_ACC] -= BaseElement::new(1u64 << lvl);
                    }
                }
                p if p == 2 + DEPTH => {
                    st[0] = BaseElement::new(6);
                    st[4] = BaseElement::new(DOM_NF);
                    st[5..9].copy_from_slice(&sk);
                    st[9] = positions[0];
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
pub fn build_spend_full_v7k_trace(
    notes: &[NoteV7],
    paths: &[MerklePathV7],
    public_value: BaseElement,
    outs: &[OutputV7; N_OUTS],
    options: &ProofOptions,
) -> TraceTable<BaseElement> {
    use rand::RngCore;
    let mut seed = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut seed);
    build_spend_full_v7k_trace_seeded(notes, paths, public_value, outs, options, seed)
}

type Coin = DefaultRandomCoin<Blake3_256<BaseElement>>;

pub use crate::spend_full_v7::V7VerifyError;

/// Verify a K-input merge. K is read from the public inputs; the proof's trace width must be
/// exactly [`width`]`(K)`, checked BEFORE the AIR is built (winterfell asserts on width).
pub fn verify_spend_full_v7k(proof: winterfell::Proof, pub_inputs: SpendFullV7kPublicInputs) -> Result<(), V7VerifyError> {
    let k = pub_inputs.nfs.len();
    if !(2..=MAX_INS).contains(&k) {
        return Err(V7VerifyError::WrongTraceWidth { expected: width(2), got: 0 });
    }
    let ti = proof.trace_info();
    let got = ti.main_trace_width();
    if got != width(k) {
        return Err(V7VerifyError::WrongTraceWidth { expected: width(k), got });
    }
    let got = ti.length();
    if got != TRACE_LEN {
        return Err(V7VerifyError::WrongTraceLength { expected: TRACE_LEN, got });
    }
    if (pub_inputs.public_value.as_int() as u128) >= (1u128 << RANGE_BITS) {
        return Err(V7VerifyError::PublicValueOutOfRange);
    }
    let min = winterfell::AcceptableOptions::MinConjecturedSecurity(ACCEPT_BITS);
    winterfell::verify::<SpendFullV7kAir, Blake3_256<BaseElement>, Coin>(proof, pub_inputs, &min)
        .map_err(V7VerifyError::Verifier)
}

/// The off-circuit view of the public inputs, for callers that hold the notes.
pub fn public_inputs_for_k(root: Digest4, nfs: &[Digest4], public_value: u64, cm_outs: [Digest4; N_OUTS]) -> SpendFullV7kPublicInputs {
    SpendFullV7kPublicInputs {
        root: root.0,
        nfs: nfs.iter().map(|d| d.0).collect(),
        public_value: BaseElement::new(public_value),
        cm_outs: [cm_outs[0].0, cm_outs[1].0],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::note_v7::{padding_leaf, PoolTreeV7};
    use crate::spend_full_v7::v7_options;

    fn e(v: u64) -> BaseElement { BaseElement::new(v) }
    const SK: [u64; 4] = [0xAAAA, 0xBBBB, 0xCCCC, 0xDDDD];

    fn notes(k: usize) -> Vec<NoteV7> {
        (0..k).map(|j| NoteV7::new(1_000_000 * (j as u64 + 1), [0x1111 + j as u64, 0x2222, 0x3333, 0x4444], SK).unwrap()).collect()
    }
    fn bob() -> Digest4 { NoteV7::new(0, [0; 4], [0xB0B, 0xB0B, 0xB0B, 0xB0B]).unwrap().owner_pk() }
    fn positions(k: usize) -> Vec<usize> { (0..k).map(|j| 300 + 997 * j).collect() }
    fn pool(ns: &[NoteV7], ps: &[usize]) -> (PoolTreeV7, Vec<MerklePathV7>) {
        let mut leaves = vec![padding_leaf(); 1 << DEPTH];
        for i in [1usize, 2, 100, 9_000, 32_767] {
            leaves[i] = NoteV7::new(i as u64 + 7, [i as u64; 4], [1, 2, 3, i as u64]).unwrap().commitment();
        }
        for (n, &p) in ns.iter().zip(ps) {
            leaves[p] = n.commitment();
        }
        let tree = PoolTreeV7::new(leaves).unwrap();
        let paths = ps.iter().map(|&p| tree.path(p).unwrap()).collect();
        (tree, paths)
    }
    /// Σ inputs = 7 public + 3,000,000 to Bob + change.
    fn outs(ns: &[NoteV7]) -> (u64, [OutputV7; N_OUTS]) {
        let total: u64 = ns.iter().map(|n| n.value.as_int()).sum();
        (7, [OutputV7::new(3_000_000, [5, 6, 7, 8], bob()), OutputV7::new(total - 3_000_000 - 7, [9, 10, 11, 12], ns[0].owner_pk())])
    }
    fn pi(tree: &PoolTreeV7, ns: &[NoteV7], ps: &[usize], pv: u64, o: &[OutputV7; N_OUTS]) -> SpendFullV7kPublicInputs {
        let nfs: Vec<Digest4> = ns.iter().zip(ps).map(|(n, &p)| n.nullifier(p as u64)).collect();
        public_inputs_for_k(tree.root(), &nfs, pv, [o[0].commitment(), o[1].commitment()])
    }

    /// At K = 2 the layout IS spend_full_v7_2's: same width, and lane 1 sits where lane C does.
    #[test]
    fn k2_layout_is_the_deployed_merge() {
        assert_eq!(width(2), crate::spend_full_v7_2::WIDTH);
        assert_eq!(width(2), 92);
        assert_eq!(lane(1), 67);
        assert_eq!(col_inv(2, 0, 1), 91);
        assert_eq!(n_constraints(2), 98 + 86 + 76);
        assert_eq!((width(4), width(7), width(8)), (145, 232, 263));
        assert!(width(MAX_INS) <= 255 && width(MAX_INS + 1) > 255, "MAX_INS is the 255-column cap");
    }

    #[test]
    fn pair_columns_are_dense_and_unique() {
        for k in 2..=MAX_INS {
            let mut cols: Vec<usize> = Vec::new();
            for a in 0..k { for b in (a + 1)..k { cols.push(col_inv(k, a, b)); } }
            let start = WIDTH_V7 + LANE_W * (k - 1);
            assert_eq!(cols, (start..width(k)).collect::<Vec<_>>(), "K = {k}");
        }
    }

    /// IN-CIRCUIT == OFF-CIRCUIT (debug): every lane's digest rows are its note's primitives.
    #[test]
    fn trace_digests_match_every_note() {
        for k in [3usize, 5] {
            let ns = notes(k);
            let ps = positions(k);
            let (tree, paths) = pool(&ns, &ps);
            let (pv, o) = outs(&ns);
            let t = build_spend_full_v7k_trace_seeded(&ns, &paths, e(pv), &o, &v7_options(), [7u8; 32]);
            let dig = |base: usize, row: usize| -> Digest4 {
                let mut d = [BaseElement::ZERO; 4];
                for x in 0..4 { d[x] = t.get(base + 4 + x, row); }
                Digest4(d)
            };
            assert_eq!(dig(COL_STATE, ROW_CM), ns[0].commitment());
            assert_eq!(dig(COL_STATE, ROW_ROOT), tree.root());
            for j in 1..k {
                let b = lane(j) + L_STATE;
                assert_eq!(dig(b, ROW_INNER), ns[j].inner_commitment(), "K={k} lane {j} inner");
                assert_eq!(dig(b, ROW_CM), ns[j].commitment(), "K={k} lane {j} cm");
                assert_eq!(dig(b, ROW_ROOT), tree.root(), "K={k} lane {j} root");
                assert_eq!(dig(b, ROW_NF), ns[j].nullifier(ps[j] as u64), "K={k} lane {j} nf");
                assert_eq!(t.get(lane(j) + L_ACC, ROW_NF), BaseElement::ZERO);
            }
        }
    }

    /// ROUND TRIP through the verifier at K = 3 and 4, hiding on; nullifiers are lane-bound.
    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn honest_wide_merge_proves_and_verifies() {
        for k in [3usize, 4] {
            let ns = notes(k);
            let ps = positions(k);
            let (tree, paths) = pool(&ns, &ps);
            let (pv, o) = outs(&ns);
            let t = build_spend_full_v7k_trace_seeded(&ns, &paths, e(pv), &o, &v7_options(), [7u8; 32]);
            let proof = SpendFullV7kProver::new(v7_options()).prove_hiding(t).expect("prove (hiding)");
            let good = pi(&tree, &ns, &ps, pv, &o);
            verify_spend_full_v7k(proof.clone(), good.clone()).expect("an honest wide merge must verify");
            let mut bad = good.clone();
            bad.nfs.swap(0, k - 1);
            assert!(verify_spend_full_v7k(proof.clone(), bad).is_err(), "K={k}: nullifiers are lane-bound");
            let mut bad = good.clone();
            bad.nfs[k - 1] = ns[k - 1].nullifier(ps[k - 1] as u64 + 1).0;
            assert!(verify_spend_full_v7k(proof.clone(), bad).is_err(), "K={k}: a wrong last nullifier");
            let mut bad = good;
            bad.nfs.pop();
            assert!(verify_spend_full_v7k(proof, bad).is_err(), "K={k}: fewer nullifiers = another width");
        }
    }

    /// ONE NOTE TWICE (inputs 1 and 3 equal, K = 4) cannot be proven.
    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn a_repeated_note_is_not_a_wide_merge() {
        let mut ns = notes(4);
        let mut ps = positions(4);
        ns[3] = ns[1];
        ps[3] = ps[1];
        let (tree, paths) = pool(&ns[..3], &ps[..3]);
        let paths = vec![paths[0].clone(), paths[1].clone(), paths[2].clone(), paths[1].clone()];
        let (pv, o) = outs(&ns);
        let t = build_v7k_trace_unchecked(&ns, &paths, e(pv), &o, &v7_options(), [7u8; 32]);
        let verdict = match SpendFullV7kProver::new(v7_options()).prove(t) {
            Ok(proof) => verify_spend_full_v7k(proof, pi(&tree, &ns, &ps, pv, &o)).is_err(),
            Err(_) => true,
        };
        assert!(verdict, "a note fed in twice must not yield an accepted proof");
    }

    /// TAMPERED: an inflated third value, and outputs exceeding the inputs, are refused.
    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn tampered_wide_merge_does_not_verify() {
        let ns = notes(3);
        let ps = positions(3);
        let (tree, paths) = pool(&ns, &ps);
        let (pv, o) = outs(&ns);
        let opt = v7_options();
        let mut richer = ns.clone();
        richer[2] = NoteV7::new(9_000_000, ns[2].blinding.map(|x| x.as_int()), SK).unwrap();
        let mut o2 = o;
        o2[1].value = e(o[1].value.as_int() + 9_000_000 - ns[2].value.as_int());
        let t = build_v7k_trace_unchecked(&richer, &paths, e(pv), &o2, &opt, [7u8; 32]);
        let proof = SpendFullV7kProver::new(opt.clone()).prove(t).expect("prove");
        assert!(verify_spend_full_v7k(proof, pi(&tree, &ns, &ps, pv, &o2)).is_err(), "a richer third note must not open its leaf");
        let mut o3 = o;
        o3[0].value = e(3_000_001);
        let t = build_v7k_trace_unchecked(&ns, &paths, e(pv), &o3, &opt, [7u8; 32]);
        let proof = SpendFullV7kProver::new(opt).prove(t).expect("prove");
        assert!(verify_spend_full_v7k(proof, pi(&tree, &ns, &ps, pv, &o3)).is_err(), "outputs exceeding the inputs");
    }

    /// THE NODE'S ENTRY POINT: a 4-input proof through `note_v7::verify_spend_wire_v7` (bytes in,
    /// arity-dispatched); the same proof claimed as 3 inputs is refused by width before any AIR.
    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn wide_merge_through_the_wire() {
        use crate::note_v1::WireVerifyError;
        use crate::note_v7::verify_spend_wire_v7;
        let ns = notes(4);
        let ps = positions(4);
        let (tree, paths) = pool(&ns, &ps);
        let (pv, o) = outs(&ns);
        let t = build_spend_full_v7k_trace_seeded(&ns, &paths, e(pv), &o, &v7_options(), [7u8; 32]);
        let bytes = SpendFullV7kProver::new(v7_options()).prove_hiding(t).expect("prove").to_bytes();
        let nfs: Vec<[u8; 32]> = ns.iter().zip(&ps).map(|(n, &p)| n.nullifier(p as u64).to_wire()).collect();
        let cms = [o[0].commitment().to_wire(), o[1].commitment().to_wire()];
        verify_spend_wire_v7(&tree.root().to_wire(), &nfs, pv as u128, &cms, &bytes).expect("a 4-input merge through the wire");
        // claimed as a 2-input merge: the arity is accepted, the shape is not
        match verify_spend_wire_v7(&tree.root().to_wire(), &nfs[..2], pv as u128, &cms, &bytes) {
            Err(WireVerifyError::WrongTraceWidth { expected, got }) => { assert_eq!(expected, width(2)); assert_eq!(got, width(4)); }
            other => panic!("expected a width refusal, got {other:?}"),
        }
        // claimed as 3 inputs: not an accepted arity at all
        assert!(matches!(verify_spend_wire_v7(&tree.root().to_wire(), &nfs[..3], pv as u128, &cms, &bytes),
            Err(WireVerifyError::UnsupportedInputCount { got: 3 })));
    }
}
