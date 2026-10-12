//! v7 SPEND, MILESTONE 1 — the single-input Rescue-Prime spend AIR (2026-10-12).
//!
//! The v7 note ([`crate::note_v7`]) replaces every 64-bit MiMC value with a 4-element
//! (256-bit) Rescue-Prime `Rp64_256` digest. This module is the in-circuit half: a STARK
//! that proves, for ONE input note, exactly what [`crate::spend_full_v5`] proves for a v1
//! note on the input side —
//!
//! ```text
//!   pk    = H(DOM_PK ‖ sk)                 owner key, from the hidden 256-bit spend key
//!   inner = H(DOM_VAL ‖ value ‖ blinding)  the value commitment (hidden)
//!   cm    = merge(inner, pk)               the owner-bound leaf (hidden)
//!   root  = climb(cm, path)                15 levels, direction bits ⇒ position   PUBLIC
//!   nf    = H(DOM_NF ‖ sk ‖ position)      the SAME sk and the SAME position       PUBLIC
//! ```
//!
//! — with every hash the real `Rp64_256` permutation, constrained round by round, so the
//! off-circuit tree a node keeps and the in-circuit climb agree by construction.
//!
//! # Scope — read this before trusting it
//!
//! M1 binds: value opening, owner key, leaf, membership, nullifier, and position (the
//! nullifier's position is the leaf index the direction bits spell). It does NOT yet have
//! outputs, conservation, a public value lane or a range check — those are M2 and are
//! what makes this a *spend* rather than a *membership-plus-nullifier* proof. Until M2 is
//! in, nothing here may be wired into a wallet, the API or consensus. The public inputs
//! are `(root, nf)` only.
//!
//! # Layout — one permutation per 8 rows, laid end to end
//!
//! A Rescue round maps a 12-element state to the next; 7 rounds make a permutation. One
//! round per row, so a permutation occupies 8 rows (row 0 = input state, row 7 = output;
//! the digest is `state[4..8]`). 19 permutations in sequence:
//!
//! ```text
//!   P0   rows   0..8    pk    = H(DOM_PK ‖ sk)
//!   P1   rows   8..16   inner = H(DOM_VAL ‖ value ‖ blinding)
//!   P2   rows  16..24   cm    = merge(inner, pk)
//!   P3…P17  rows 24..144   Merkle climb, level k in P(3+k): merge(node, sib) or merge(sib, node)
//!   P18  rows 144..152  nf    = H(DOM_NF ‖ sk ‖ position)
//!   rows 152..256       reserved randomness (zk mask, see `zk_mask`)
//! ```
//!
//! Inside a permutation the transition is the Rescue round relation, written so that the
//! inverse S-box never has to be computed forward:
//!
//! ```text
//!   (INV_MDS · (next − ARK2[r]))^7  ==  MDS · cur^7 + ARK1[r]      (12 constraints, degree 7)
//! ```
//!
//! At each permutation boundary (row 8p+7 → 8p+8) a "load" constraint pins the next input
//! state to witness columns held constant down the whole real region (`sk`, `blinding`,
//! `value`, `pk`, `position`) or to the digest that was just produced. Those held columns
//! are the binding pivots, the same trick v3→v5 use with MiMC: one witness column, several
//! bindings, so the key that derives `pk` is provably the key in the nullifier, and the
//! position in the nullifier is provably the index the climb spelled.
//!
//! The direction bits are boolean and their weighted sum is driven from `position` to zero
//! across the climb (the v5 accumulator), so `position` cannot be chosen independently of
//! the path.
//!
//! Constraint degrees are upper bounds (winterfell 0.9's debug exact-degree check trips on
//! witness-dependent selector products, as it does for every AIR in this family); the
//! proving tests therefore run release-compiled and assert soundness through the verifier.

use winterfell::FieldExtension;
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
use crate::zk_mask::{exemptions_for, fill_random_row, padded_len, zk_rows_for, HidingProver, Secret};

/// Same options as v5/v6: blowup 8 carries a degree-7 lane with one period-8 selector.
pub fn v7_options() -> ProofOptions {
    ProofOptions::new(84, 8, 16, FieldExtension::Quadratic, 8, 31)
}

/// Rescue state width.
const STATE: usize = 12;
/// Rows per permutation: 7 rounds + the input row.
const SEG: usize = 8;
/// Pool depth: `sigil_state::POOL_CAPACITY` = 32,768 = 2^15 leaves.
pub const DEPTH: usize = 15;
/// pk, inner, cm, DEPTH climb levels, nf.
const N_PERM: usize = 3 + DEPTH + 1;
/// Rows that carry the computation.
pub const REAL_LEN: usize = N_PERM * SEG; // 152
/// Rows including the reserved randomness. Fixed, so the exemption count is a constant the
/// prover and verifier agree on; `build_*` asserts `padded_len` lands here.
pub const TRACE_LEN: usize = 256;

// Column layout.
const COL_STATE: usize = 0; // 0..12   Rescue state
const COL_SK: usize = 12; // 12..16  spend key, held
const COL_B: usize = 16; // 16..20  blinding, held
const COL_VALUE: usize = 20; // value, held
const COL_PK: usize = 21; // 21..25  owner pk, held (bound to P0's digest)
const COL_POS: usize = 25; // position, held
const COL_SIB: usize = 26; // 26..30  sibling digest, constant within a segment
const COL_BIT: usize = 30; // direction bit, constant within a segment
const COL_ACC: usize = 31; // position accumulator
/// Trace width.
pub const WIDTH: usize = 32;

/// Rows where a permutation's digest is read.
const ROW_PK: usize = SEG - 1; // 7
const ROW_INNER: usize = 2 * SEG - 1; // 15
const ROW_CM: usize = 3 * SEG - 1; // 23
const ROW_ROOT: usize = (3 + DEPTH) * SEG - 1; // 143
const ROW_NF: usize = REAL_LEN - 1; // 151

// Periodic column indices.
const PC_ARK1: usize = 0; // 0..12, period 8
const PC_ARK2: usize = 12; // 12..24, period 8
const PC_S: usize = 24; // boundary selector, period 8 (1 at row 7)
const PC_FIRST: usize = 25; // row 0
const PC_LD_A: usize = 26; // row 7:   P0 → P1
const PC_LD_B: usize = 27; // row 15:  P1 → P2
const PC_LD_C: usize = 28; // rows 23+8k, k < DEPTH: climb loads
const PC_LD_D: usize = 29; // row 143: P17 → P18
const PC_PW: usize = 30; // 2^k at row 23+8k, else 0

/// What the verifier is handed: the anonymity-set root and the nullifier, 4 elements each.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpendFullV7PublicInputs {
    pub root: [BaseElement; 4],
    pub nf: [BaseElement; 4],
}
impl ToElements<BaseElement> for SpendFullV7PublicInputs {
    fn to_elements(&self) -> Vec<BaseElement> {
        let mut v = self.root.to_vec();
        v.extend_from_slice(&self.nf);
        v
    }
}

/// `out[i] = Σ_j m[i][j]·v[j]` — the orientation `Rp64_256::apply_mds` uses, pinned by
/// `air_round_relation_matches_rp64_256` below.
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

pub struct SpendFullV7Air {
    context: AirContext<BaseElement>,
    root: [BaseElement; 4],
    nf: [BaseElement; 4],
}

impl Air for SpendFullV7Air {
    type BaseField = BaseElement;
    type PublicInputs = SpendFullV7PublicInputs;
    type GkrProof = ();
    type GkrVerifier = ();

    fn new(trace_info: TraceInfo, pub_inputs: SpendFullV7PublicInputs, options: ProofOptions) -> Self {
        assert_eq!(WIDTH, trace_info.width());
        let n = trace_info.length();
        assert_eq!(n, TRACE_LEN, "v7 M1 traces are exactly {TRACE_LEN} rows");
        let mut degrees = Vec::with_capacity(98);
        // [0..12)   Rescue round relation, masked off at segment boundaries
        for _ in 0..STATE {
            degrees.push(TransitionConstraintDegree::with_cycles(7, vec![SEG]));
        }
        // [12..26)  held columns: sk(4) b(4) value pk(4) pos
        for _ in COL_SK..COL_SIB {
            degrees.push(TransitionConstraintDegree::new(1));
        }
        // [26..31)  sib(4) + bit constant within a segment
        for _ in 0..5 {
            degrees.push(TransitionConstraintDegree::with_cycles(1, vec![SEG]));
        }
        // [31]      bit boolean
        degrees.push(TransitionConstraintDegree::new(2));
        // [32]      acc' = acc − s·nbit·pw
        degrees.push(TransitionConstraintDegree::with_cycles(1, vec![SEG, n]));
        // [33..45)  first: P0 input = (5,0,0,0, DOM_PK, sk, 0,0,0)
        // [45..49)  ldA: pk_held = P0 digest
        // [49..61)  ldA: P1 input = (6,0,0,0, DOM_VAL, value, b, 0,0)
        // [61..73)  ldB: P2 input = (8,0,0,0, inner, pk)
        for _ in 0..(12 + 4 + 12 + 12) {
            degrees.push(TransitionConstraintDegree::with_cycles(1, vec![n]));
        }
        // [73..85)  ldC: climb input = (8,0,0,0, bit ? (sib, node) : (node, sib))
        for _ in 0..12 {
            degrees.push(TransitionConstraintDegree::with_cycles(2, vec![n]));
        }
        // [85..97)  ldD: P18 input = (6,0,0,0, DOM_NF, sk, pos, 0,0)
        // [97]      first: acc = pos
        for _ in 0..13 {
            degrees.push(TransitionConstraintDegree::with_cycles(1, vec![n]));
        }
        debug_assert_eq!(degrees.len(), 98);
        let num_assertions = 4 + 4 + 1;
        SpendFullV7Air {
            context: AirContext::new(trace_info, degrees, num_assertions, options)
                .set_num_transition_exemptions(exemptions_for(n, REAL_LEN)),
            root: pub_inputs.root,
            nf: pub_inputs.nf,
        }
    }

    fn get_periodic_column_values(&self) -> Vec<Vec<BaseElement>> {
        let n = TRACE_LEN;
        let mut cols: Vec<Vec<BaseElement>> = Vec::with_capacity(31);
        // ARK1 / ARK2 per round, period 8; the boundary row carries zero (masked anyway).
        for j in 0..STATE {
            let mut c = vec![BaseElement::ZERO; SEG];
            for r in 0..Rp64_256::NUM_ROUNDS {
                c[r] = Rp64_256::ARK1[r][j];
            }
            cols.push(c);
        }
        for j in 0..STATE {
            let mut c = vec![BaseElement::ZERO; SEG];
            for r in 0..Rp64_256::NUM_ROUNDS {
                c[r] = Rp64_256::ARK2[r][j];
            }
            cols.push(c);
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
        let s = periodic[PC_S];
        let first = periodic[PC_FIRST];
        let ld_a = periodic[PC_LD_A];
        let ld_b = periodic[PC_LD_B];
        let ld_c = periodic[PC_LD_C];
        let ld_d = periodic[PC_LD_D];
        let pw = periodic[PC_PW];

        let mut st = [E::ZERO; STATE];
        let mut st_next = [E::ZERO; STATE];
        st.copy_from_slice(&cur[COL_STATE..COL_STATE + STATE]);
        st_next.copy_from_slice(&nxt[COL_STATE..COL_STATE + STATE]);

        // ── Rescue round: (INV_MDS·(next − ARK2))^7 == MDS·cur^7 + ARK1 ──────────────
        let mut cur7 = [E::ZERO; STATE];
        for j in 0..STATE {
            cur7[j] = pow7(st[j]);
        }
        let rhs = mat_vec(&Rp64_256::MDS, &cur7);
        let mut shifted = [E::ZERO; STATE];
        for j in 0..STATE {
            shifted[j] = st_next[j] - periodic[PC_ARK2 + j];
        }
        let lhs_lin = mat_vec(&Rp64_256::INV_MDS, &shifted);
        for j in 0..STATE {
            result[j] = (one - s) * (pow7(lhs_lin[j]) - rhs[j] - periodic[PC_ARK1 + j]);
        }

        // ── held columns ─────────────────────────────────────────────────────────────
        let mut i = 12;
        for c in COL_SK..COL_SIB {
            result[i] = nxt[c] - cur[c];
            i += 1;
        }
        // ── per-segment constants ────────────────────────────────────────────────────
        for c in COL_SIB..=COL_BIT {
            result[i] = (one - s) * (nxt[c] - cur[c]);
            i += 1;
        }
        let bit = cur[COL_BIT];
        result[i] = bit * (bit - one);
        i += 1;
        // the position accumulator subtracts the NEXT segment's bit, weighted by its level
        let nbit = nxt[COL_BIT];
        result[i] = nxt[COL_ACC] - cur[COL_ACC] + s * nbit * pw;
        i += 1;

        let five = E::from(BaseElement::new(5));
        let six = E::from(BaseElement::new(6));
        let eight = E::from(BaseElement::new(8));
        let dom_pk = E::from(BaseElement::new(DOM_PK));
        let dom_val = E::from(BaseElement::new(DOM_VAL));
        let dom_nf = E::from(BaseElement::new(DOM_NF));

        // ── first: P0 input = (5,0,0,0, DOM_PK, sk0..3, 0,0,0) ───────────────────────
        let p0 = [
            st[0] - five, st[1], st[2], st[3],
            st[4] - dom_pk,
            st[5] - cur[COL_SK], st[6] - cur[COL_SK + 1], st[7] - cur[COL_SK + 2], st[8] - cur[COL_SK + 3],
            st[9], st[10], st[11],
        ];
        for v in p0 {
            result[i] = first * v;
            i += 1;
        }
        // ── ldA: pk_held = digest(P0); P1 input = (6,0,0,0, DOM_VAL, value, b0..3, 0,0) ─
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
        for v in p1 {
            result[i] = ld_a * v;
            i += 1;
        }
        // ── ldB: P2 input = (8,0,0,0, inner = digest(P1), pk_held) ───────────────────
        let p2 = [
            st_next[0] - eight, st_next[1], st_next[2], st_next[3],
            st_next[4] - st[4], st_next[5] - st[5], st_next[6] - st[6], st_next[7] - st[7],
            st_next[8] - cur[COL_PK], st_next[9] - cur[COL_PK + 1], st_next[10] - cur[COL_PK + 2], st_next[11] - cur[COL_PK + 3],
        ];
        for v in p2 {
            result[i] = ld_b * v;
            i += 1;
        }
        // ── ldC: climb input = (8,0,0,0, left, right), node = digest just produced ────
        //   bit = 1 ⇒ node is the RIGHT child: (sib, node); bit = 0 ⇒ (node, sib)
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
        for v in p3 {
            result[i] = ld_c * v;
            i += 1;
        }
        // ── ldD: P18 input = (6,0,0,0, DOM_NF, sk0..3, pos, 0,0) ─────────────────────
        let p4 = [
            st_next[0] - six, st_next[1], st_next[2], st_next[3],
            st_next[4] - dom_nf,
            st_next[5] - cur[COL_SK], st_next[6] - cur[COL_SK + 1], st_next[7] - cur[COL_SK + 2], st_next[8] - cur[COL_SK + 3],
            st_next[9] - cur[COL_POS],
            st_next[10], st_next[11],
        ];
        for v in p4 {
            result[i] = ld_d * v;
            i += 1;
        }
        // ── first: the accumulator starts at the held position ───────────────────────
        result[i] = first * (cur[COL_ACC] - cur[COL_POS]);
        i += 1;
        debug_assert_eq!(i, result.len());
    }

    fn get_assertions(&self) -> Vec<Assertion<Self::BaseField>> {
        let mut a = Vec::with_capacity(9);
        for k in 0..4 {
            a.push(Assertion::single(COL_STATE + 4 + k, ROW_ROOT, self.root[k]));
        }
        for k in 0..4 {
            a.push(Assertion::single(COL_STATE + 4 + k, ROW_NF, self.nf[k]));
        }
        // every direction bit has been subtracted: position == Σ bit_k·2^k
        a.push(Assertion::single(COL_ACC, ROW_NF, BaseElement::ZERO));
        a
    }

    fn context(&self) -> &AirContext<Self::BaseField> {
        &self.context
    }
}

pub struct SpendFullV7Prover {
    options: ProofOptions,
}
impl SpendFullV7Prover {
    pub fn new(options: ProofOptions) -> Self {
        Self { options }
    }
}
impl Prover for SpendFullV7Prover {
    type BaseField = BaseElement;
    type Air = SpendFullV7Air;
    type Trace = TraceTable<Self::BaseField>;
    type HashFn = Blake3_256<Self::BaseField>;
    type RandomCoin = DefaultRandomCoin<Self::HashFn>;
    type TraceLde<E: FieldElement<BaseField = Self::BaseField>> = DefaultTraceLde<E, Self::HashFn>;
    type ConstraintEvaluator<'a, E: FieldElement<BaseField = Self::BaseField>> =
        DefaultConstraintEvaluator<'a, Self::Air, E>;

    fn get_pub_inputs(&self, trace: &Self::Trace) -> SpendFullV7PublicInputs {
        let mut root = [BaseElement::ZERO; 4];
        let mut nf = [BaseElement::ZERO; 4];
        for k in 0..4 {
            root[k] = trace.get(COL_STATE + 4 + k, ROW_ROOT);
            nf[k] = trace.get(COL_STATE + 4 + k, ROW_NF);
        }
        SpendFullV7PublicInputs { root, nf }
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

impl HidingProver for SpendFullV7Prover {
    fn secrets(&self, trace: &Self::Trace) -> Vec<Secret> {
        use winterfell::Trace as _;
        let m = trace.main_segment();
        let mut v: Vec<Secret> = vec![("value", m.get(COL_VALUE, 0))];
        for k in 0..4 {
            v.push(("spend key", m.get(COL_SK + k, 0)));
            v.push(("blinding", m.get(COL_B + k, 0)));
            v.push(("owner pk", m.get(COL_PK + k, 0)));
            v.push(("inner commitment", m.get(COL_STATE + 4 + k, ROW_INNER)));
            v.push(("commitment", m.get(COL_STATE + 4 + k, ROW_CM)));
        }
        v
    }
    fn zk_reserved_rows(&self, trace: &Self::Trace) -> usize {
        use winterfell::Trace as _;
        trace.main_segment().num_rows() - REAL_LEN
    }
}

/// Build the M1 trace. `mask_seed` must be fresh per proof (see `spend_full_v5`); the
/// seeded form is a test affordance. Off-circuit facts are asserted here so a malformed
/// witness fails loudly instead of producing a proof the verifier rejects.
pub fn build_spend_full_v7_trace_seeded(
    note: &NoteV7,
    path: &MerklePathV7,
    options: &ProofOptions,
    mask_seed: [u8; 32],
) -> TraceTable<BaseElement> {
    assert_eq!(path.leaf, note.commitment(), "the path must authenticate this note's commitment");
    assert_eq!(path.compute_root(), path.compute_root());
    build_v7_trace_unchecked(note, path, options, mask_seed)
}

/// Same as [`build_spend_full_v7_trace_seeded`] without the off-circuit sanity checks, so
/// tests can hand the prover a witness that is wrong on purpose.
pub fn build_v7_trace_unchecked(
    note: &NoteV7,
    path: &MerklePathV7,
    options: &ProofOptions,
    mask_seed: [u8; 32],
) -> TraceTable<BaseElement> {
    assert_eq!(path.siblings.len(), DEPTH, "v7 M1 is fixed at depth {DEPTH}");
    assert_eq!(path.bits.len(), DEPTH);
    let len = padded_len(REAL_LEN, options);
    assert_eq!(len, TRACE_LEN, "these options pad {REAL_LEN} rows to {len}, not {TRACE_LEN}");
    assert!(len - REAL_LEN >= zk_rows_for(options), "reserved randomness must exceed the opening count");

    let sk = note.spend_key;
    let b = note.blinding;
    let value = note.value;
    let pk = note.owner_pk().0;
    let position = BaseElement::new(path.position());
    let sibs: Vec<[BaseElement; 4]> = path.siblings.iter().map(|d| d.0).collect();
    let bits: Vec<bool> = path.bits.clone();

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
        },
        |step, state| {
            if step + 1 >= REAL_LEN {
                fill_random_row(&mask_seed, step + 1, state);
                return;
            }
            let r = step % SEG;
            if r != SEG - 1 {
                let mut st = [BaseElement::ZERO; STATE];
                st.copy_from_slice(&state[COL_STATE..COL_STATE + STATE]);
                Rp64_256::apply_round(&mut st, r);
                state[COL_STATE..COL_STATE + STATE].copy_from_slice(&st);
                return;
            }
            // boundary: permutation p just finished; load p+1's input
            let p = step / SEG;
            let mut digest = [BaseElement::ZERO; 4];
            digest.copy_from_slice(&state[COL_STATE + 4..COL_STATE + 8]);
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
pub fn build_spend_full_v7_trace(note: &NoteV7, path: &MerklePathV7, options: &ProofOptions) -> TraceTable<BaseElement> {
    use rand::RngCore;
    let mut seed = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut seed);
    build_spend_full_v7_trace_seeded(note, path, options, seed)
}

type Coin = DefaultRandomCoin<Blake3_256<BaseElement>>;

/// Why a v7 proof was refused before or by the verifier.
#[derive(Debug)]
pub enum V7VerifyError {
    /// The proof's declared trace width is not this AIR's. Checked BEFORE `Air::new`,
    /// whose `assert_eq!` would otherwise abort the process on a hostile proof.
    WrongTraceWidth { expected: usize, got: usize },
    /// The proof's declared trace length is not this AIR's. Same reason.
    WrongTraceLength { expected: usize, got: usize },
    Verifier(winterfell::VerifierError),
}

pub fn verify_spend_full_v7(proof: winterfell::Proof, pub_inputs: SpendFullV7PublicInputs) -> Result<(), V7VerifyError> {
    let ti = proof.trace_info();
    let got = ti.main_trace_width();
    if got != WIDTH {
        return Err(V7VerifyError::WrongTraceWidth { expected: WIDTH, got });
    }
    let got = ti.length();
    if got != TRACE_LEN {
        return Err(V7VerifyError::WrongTraceLength { expected: TRACE_LEN, got });
    }
    let min = winterfell::AcceptableOptions::MinConjecturedSecurity(ACCEPT_BITS);
    winterfell::verify::<SpendFullV7Air, Blake3_256<BaseElement>, Coin>(proof, pub_inputs, &min)
        .map_err(V7VerifyError::Verifier)
}

/// The off-circuit view of the same public inputs, for callers that hold the note.
pub fn public_inputs_for(root: Digest4, nf: Digest4) -> SpendFullV7PublicInputs {
    SpendFullV7PublicInputs { root: root.0, nf: nf.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::note_v7::{padding_leaf, PoolTreeV7};

    fn e(v: u64) -> BaseElement { BaseElement::new(v) }

    fn note() -> NoteV7 {
        NoteV7::new(1_234_567, [0x1111, 0x2222, 0x3333, 0x4444], [0xAAAA, 0xBBBB, 0xCCCC, 0xDDDD]).unwrap()
    }

    /// A depth-15 pool (the real capacity) with the note at `pos`, padding elsewhere.
    fn pool_with(n: &NoteV7, pos: usize) -> (PoolTreeV7, MerklePathV7) {
        let mut leaves = vec![padding_leaf(); 1 << DEPTH];
        // a few other real-looking leaves so the siblings are not all the padding constant
        for i in [1usize, 2, 100, 9_000, 32_767] {
            leaves[i] = NoteV7::new(i as u64 + 7, [i as u64; 4], [1, 2, 3, i as u64]).unwrap().commitment();
        }
        leaves[pos] = n.commitment();
        let tree = PoolTreeV7::new(leaves).unwrap();
        let path = tree.path(pos).unwrap();
        (tree, path)
    }

    fn seed() -> [u8; 32] { [7u8; 32] }

    /// THE ORIENTATION PIN (runs in debug): one real `apply_round` satisfies the AIR's
    /// round relation with `mat_vec` as written. If `apply_mds` were the transpose this
    /// would fail here, long before any proof.
    #[test]
    fn air_round_relation_matches_rp64_256() {
        let mut st = [BaseElement::ZERO; STATE];
        for (i, x) in st.iter_mut().enumerate() {
            *x = e(0x9E37_79B9_7F4A_7C15u64.wrapping_mul(i as u64 + 3));
        }
        for r in 0..Rp64_256::NUM_ROUNDS {
            let before = st;
            Rp64_256::apply_round(&mut st, r);
            let mut before7 = [BaseElement::ZERO; STATE];
            for j in 0..STATE {
                before7[j] = pow7(before[j]);
            }
            let rhs = mat_vec(&Rp64_256::MDS, &before7);
            let mut shifted = [BaseElement::ZERO; STATE];
            for j in 0..STATE {
                shifted[j] = st[j] - Rp64_256::ARK2[r][j];
            }
            let lhs = mat_vec(&Rp64_256::INV_MDS, &shifted);
            for j in 0..STATE {
                assert_eq!(pow7(lhs[j]), rhs[j] + Rp64_256::ARK1[r][j], "round {r}, element {j}");
            }
        }
    }

    /// IN-CIRCUIT == OFF-CIRCUIT (runs in debug): every digest row of the trace equals the
    /// `note_v7` primitive it stands for, so the AIR and the node's tree agree by construction.
    #[test]
    fn trace_digests_match_note_v7_primitives() {
        let n = note();
        let (tree, path) = pool_with(&n, 21_845);
        let t = build_spend_full_v7_trace_seeded(&n, &path, &v7_options(), seed());
        let dig = |row: usize| -> Digest4 {
            let mut d = [BaseElement::ZERO; 4];
            for k in 0..4 { d[k] = t.get(COL_STATE + 4 + k, row); }
            Digest4(d)
        };
        assert_eq!(dig(ROW_PK), n.owner_pk(), "P0 = owner pk");
        assert_eq!(dig(ROW_INNER), n.inner_commitment(), "P1 = inner");
        assert_eq!(dig(ROW_CM), n.commitment(), "P2 = commitment");
        assert_eq!(dig(ROW_ROOT), tree.root(), "P17 = pool root");
        assert_eq!(dig(ROW_NF), n.nullifier(21_845), "P18 = nullifier");
        assert_eq!(t.get(COL_ACC, ROW_NF), BaseElement::ZERO, "position fully consumed");
        assert_eq!(t.get(COL_POS, 0), e(21_845));
    }

    /// ROUND TRIP + HIDING: an honest spend proves and verifies, through `prove_hiding`,
    /// so the proof bytes are scanned for every secret before being returned.
    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn honest_spend_proves_and_verifies() {
        let n = note();
        let (tree, path) = pool_with(&n, 21_845);
        let t = build_spend_full_v7_trace_seeded(&n, &path, &v7_options(), seed());
        let proof = SpendFullV7Prover::new(v7_options()).prove_hiding(t).expect("prove (hiding)");
        let pi = public_inputs_for(tree.root(), n.nullifier(21_845));
        verify_spend_full_v7(proof, pi).expect("an honest v7 spend must verify");
    }

    /// WRONG PUBLIC INPUTS: the honest proof is rejected against a different root, a
    /// different nullifier, and the right nullifier at the wrong position.
    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn honest_proof_rejected_against_other_public_inputs() {
        let n = note();
        let (tree, path) = pool_with(&n, 5);
        let t = build_spend_full_v7_trace_seeded(&n, &path, &v7_options(), seed());
        let proof = SpendFullV7Prover::new(v7_options()).prove(t).expect("prove");
        let ok = public_inputs_for(tree.root(), n.nullifier(5));
        verify_spend_full_v7(proof.clone(), ok.clone()).expect("sanity");
        let mut bad_root = ok.clone();
        bad_root.root[0] += BaseElement::ONE;
        assert!(verify_spend_full_v7(proof.clone(), bad_root).is_err(), "tampered root");
        let mut bad_nf = ok.clone();
        bad_nf.nf[3] += BaseElement::ONE;
        assert!(verify_spend_full_v7(proof.clone(), bad_nf).is_err(), "tampered nullifier");
        let other_pos = public_inputs_for(tree.root(), n.nullifier(6));
        assert!(verify_spend_full_v7(proof, other_pos).is_err(), "nullifier at another position");
    }

    /// TAMPERED WITNESS: a prover that flips one direction bit (claiming the leaf sits
    /// elsewhere), swaps in a foreign key, or changes the value cannot produce a proof the
    /// verifier accepts for the honest root. Each is a trace the builder would refuse, so
    /// the unchecked builder is used, and the verdict is taken through the VERIFIER.
    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn tampered_witnesses_do_not_verify() {
        let n = note();
        let (tree, path) = pool_with(&n, 777);
        let honest = public_inputs_for(tree.root(), n.nullifier(777));
        let prove = |t: TraceTable<BaseElement>| SpendFullV7Prover::new(v7_options()).prove(t).expect("prove");

        // flipped path bit at level 3 (position claims 777 ^ 8 = 769)
        let mut p = path.clone();
        p.bits[3] = !p.bits[3];
        let t = build_v7_trace_unchecked(&n, &p, &v7_options(), seed());
        let pi = public_inputs_for(tree.root(), n.nullifier(p.position()));
        assert!(verify_spend_full_v7(prove(t), pi).is_err(), "a flipped direction bit must not reach the root");

        // foreign spend key: the leaf is no longer this key's, so the climb misses the root
        let mallory = NoteV7::new(1_234_567, n.blinding.map(|x| x.as_int()), [9, 9, 9, 9]).unwrap();
        let t = build_v7_trace_unchecked(&mallory, &path, &v7_options(), seed());
        let pi = public_inputs_for(tree.root(), mallory.nullifier(777));
        assert!(verify_spend_full_v7(prove(t), pi).is_err(), "a foreign key must not spend a known note");

        // different value under the same blinding and key
        let richer = NoteV7::new(9_999_999, n.blinding.map(|x| x.as_int()), n.spend_key.map(|x| x.as_int())).unwrap();
        let t = build_v7_trace_unchecked(&richer, &path, &v7_options(), seed());
        assert!(verify_spend_full_v7(prove(t), honest).is_err(), "a different value must not open the leaf");
    }

    /// SHAPE GUARD: a proof of another AIR (different width) is refused with an error, not
    /// a process abort, before `Air::new`'s `assert_eq!` is reached.
    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn foreign_shaped_proof_is_refused_not_panicked() {
        use crate::mimc::mimc_options;
        use crate::range::{build_range_trace, RangeProver};
        let t = build_range_trace(e(42));
        let foreign = RangeProver::new(mimc_options()).prove(t).expect("range proof");
        let pi = SpendFullV7PublicInputs { root: [BaseElement::ZERO; 4], nf: [BaseElement::ZERO; 4] };
        match verify_spend_full_v7(foreign, pi) {
            Err(V7VerifyError::WrongTraceWidth { expected, .. }) => assert_eq!(expected, WIDTH),
            Err(V7VerifyError::WrongTraceLength { .. }) => {}
            other => panic!("expected a shape refusal, got {other:?}"),
        }
    }
}
