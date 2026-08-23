//! FULLY OWNER-BOUND SPEND — inputs AND outputs (2026-08-23).
//!
//! `spend_full_v3` bound the INPUT note to an owner but left outputs as
//! `compress2(out_value, blinding)` — un-owned, exactly the shape whose flaw v3 was
//! written to fix. Every change note, and every note sent to someone else, therefore
//! inherited it: anyone learning `(value, blinding)` could spend, each with a different
//! nullifier. Migrating the wallet onto v3 would have relocated the bug, not removed it.
//!
//! v4 makes an output commitment owner-bound too:
//!
//! ```text
//!   inner_out = compress2(out_value, out_blinding)
//!   cm_out    = compress2(inner_out, pk_recipient)
//! ```
//!
//! `pk_recipient` is a HIDDEN witness, not a public input. Publishing it would bind the
//! note correctly but name the recipient on chain, which defeats the purpose — amounts
//! private, payment graph legible. Keeping it in-circuit costs one extra lane per output
//! and keeps the recipient unlinkable.
//!
//! A sender may set a `pk_recipient` the recipient does not control; that only makes the
//! note unspendable, harming the sender's own intent, and is not a chain safety property.
//!
//! (v3 header follows.)
//! OWNER-BOUND SPEND — closes the missing-ownership flaw in `spend_full_v2` (2026-08-23).
//!
//! # The flaw this exists to fix
//!
//! In v2 a note commitment is `compress2(value, blinding)`. Nothing binds it to an owner,
//! and nothing in the AIR constrains `spend_key` at all — it is a free witness feeding the
//! nullifier lane. So ANY party who learns `(value, blinding)` can produce a verifying
//! spend, each with a DIFFERENT nullifier derived from their own invented key. The
//! spent-set cannot stop the second spend, because it never sees the same nullifier twice.
//! Demonstrated directly: two unrelated keys both verified against one note.
//!
//! That is survivable only while every note is self-created (nobody else knows a
//! blinding), which is why it was invisible for self-custody. It makes RECEIVING
//! impossible: the instant a sender transmits `(value, blinding)` to a recipient — which
//! is exactly what a note-ciphertext layer does — both parties can spend the note.
//!
//! # The fix
//!
//! The note commits to an owner public key, and spending proves knowledge of the matching
//! secret, in-circuit, using the SAME secret that derives the nullifier:
//!
//! ```text
//!   pk    = compress2(sk, PK_DOMAIN)              owner public key (one-way from sk)
//!   inner = compress2(value, blinding)            the value commitment
//!   cm    = compress2(inner, pk)                  the Merkle leaf — now owner-bound
//!   nf    = compress2(sk, position)               unchanged
//! ```
//!
//! Knowing `(value, blinding)` is no longer sufficient: a spender must also exhibit `sk`
//! with `compress2(sk, PK_DOMAIN) == pk`, and `compress2` is one-way, so `pk` does not
//! yield it. A recipient can now be handed `(value, blinding)` safely — the sender cannot
//! spend the note it created for someone else, because the sender lacks the recipient's
//! `sk`. That is the precondition for note ciphertexts, and it is why this had to land
//! before them.
//!
//! # Structure (no extra segment)
//!
//! The two new compressions run as PARALLEL lanes over the same trace length rather than
//! as extra segments, so the trace geometry — and therefore the pool depth — is unchanged.
//! Each lane's output is tied to a held-constant witness column by a selector at row
//! `ROUNDS`, and the main commitment lane is tied to those constants at row 0. The same
//! held-constant pivot trick the output binding already uses.
//!
//! Inherited from v2 and still enforced: output↔commitment binding and per-output private
//! range. See that module for why those cannot be a separate proof.
//!
//! (v2 header follows.)
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
use crate::mimc::{compress2, pow7, round_constants, ACCEPT_BITS, ROUNDS};
use crate::spend_full::path_position;

const SEG: usize = 64;

/// Outputs per spend. Fixed so the trace width is a compile-time constant; a spend with
/// fewer real outputs pads with zero-value notes (see [`build_spend_full_v4_trace`]).
pub const N_OUTS: usize = 2;

/// Per-amount range bound. See the module docs for why this must satisfy
/// `(N_OUTS+1)·2^RANGE_BITS < p`.
pub const RANGE_BITS: usize = 58;

/// Columns before the per-output block.
const BASE_COLS: usize = 15;

/// Domain separator for owner-key derivation: `pk = compress2(sk, PK_DOMAIN)`.
/// A fixed public constant, so `pk` is a deterministic one-way image of `sk`.
pub const PK_DOMAIN: u64 = 0x5349_4749_4C5F_504B; // "SIGIL_PK"

const COL_HI: usize = 9;   // inner = compress2(value, blinding), held constant
const COL_IX: usize = 10;  // inner lane x
const COL_IY: usize = 11;  // inner lane y
const COL_HP: usize = 12;  // pk, held constant
const COL_PX: usize = 13;  // pk lane x
const COL_PY: usize = 14;  // pk lane y
/// Columns per output: hv, rem, bit, iox, ioy, hio, hpo, oox, ooy.
const COLS_PER_OUT: usize = 9;
/// Total trace width.
pub const WIDTH: usize = BASE_COLS + COLS_PER_OUT * N_OUTS;

const fn col_hv(i: usize) -> usize { BASE_COLS + COLS_PER_OUT * i }      // out value, held
const fn col_rem(i: usize) -> usize { BASE_COLS + COLS_PER_OUT * i + 1 } // range remainder
const fn col_bit(i: usize) -> usize { BASE_COLS + COLS_PER_OUT * i + 2 } // range bit
const fn col_iox(i: usize) -> usize { BASE_COLS + COLS_PER_OUT * i + 3 } // inner lane x
const fn col_ioy(i: usize) -> usize { BASE_COLS + COLS_PER_OUT * i + 4 } // inner lane y
const fn col_hio(i: usize) -> usize { BASE_COLS + COLS_PER_OUT * i + 5 } // inner_out, held
const fn col_hpo(i: usize) -> usize { BASE_COLS + COLS_PER_OUT * i + 6 } // recipient pk, held
const fn col_oox(i: usize) -> usize { BASE_COLS + COLS_PER_OUT * i + 7 } // outer lane x
const fn col_ooy(i: usize) -> usize { BASE_COLS + COLS_PER_OUT * i + 8 } // outer lane y

/// What a v2 spend reveals: the anonymity-set root, the nullifier, the fee, and the output
/// commitments. The output VALUES stay hidden; only their commitments are public, because
/// consensus must insert those into the note tree.
#[derive(Clone)]
pub struct SpendFullV4PublicInputs {
    pub root: BaseElement,
    pub nf: BaseElement,
    pub fee: BaseElement,
    pub cm_outs: [BaseElement; N_OUTS],
}
impl ToElements<BaseElement> for SpendFullV4PublicInputs {
    fn to_elements(&self) -> Vec<BaseElement> {
        let mut v = vec![self.root, self.nf, self.fee];
        v.extend_from_slice(&self.cm_outs);
        v
    }
}

pub struct SpendFullV4Air {
    context: AirContext<BaseElement>,
    root: BaseElement,
    nf: BaseElement,
    fee: BaseElement,
    cm_outs: [BaseElement; N_OUTS],
    trace_len: usize,
}

impl Air for SpendFullV4Air {
    type BaseField = BaseElement;
    type PublicInputs = SpendFullV4PublicInputs;
    type GkrProof = ();
    type GkrVerifier = ();

    fn new(trace_info: TraceInfo, pub_inputs: SpendFullV4PublicInputs, options: ProofOptions) -> Self {
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
            // ── owner binding ──
            TransitionConstraintDegree::new(1),                               // hi constant
            TransitionConstraintDegree::with_cycles(7, vec![SEG]),            // inner lane x
            TransitionConstraintDegree::new(1),                               // inner lane y
            TransitionConstraintDegree::with_cycles(1, vec![trace_len]),      // selr·(ix−hi)
            TransitionConstraintDegree::with_cycles(1, vec![trace_len]),      // first·(x−hi)
            TransitionConstraintDegree::new(1),                               // hp constant
            TransitionConstraintDegree::with_cycles(7, vec![SEG]),            // pk lane x
            TransitionConstraintDegree::new(1),                               // pk lane y
            TransitionConstraintDegree::with_cycles(1, vec![trace_len]),      // selr·(px−hp)
            TransitionConstraintDegree::with_cycles(1, vec![trace_len]),      // first·(y−hp)
            TransitionConstraintDegree::with_cycles(1, vec![trace_len]),      // first·(px−nx)
        ];
        for _ in 0..N_OUTS {
            degrees.push(TransitionConstraintDegree::new(1));                          // hv constant
            degrees.push(TransitionConstraintDegree::with_cycles(7, vec![SEG]));       // iox Feistel
            degrees.push(TransitionConstraintDegree::new(1));                          // ioy' = iox
            degrees.push(TransitionConstraintDegree::with_cycles(1, vec![trace_len])); // first·(iox−hv)
            degrees.push(TransitionConstraintDegree::with_cycles(1, vec![trace_len])); // osel·(out−hv)
            degrees.push(TransitionConstraintDegree::with_cycles(1, vec![trace_len])); // selr·(iox−hio)
            degrees.push(TransitionConstraintDegree::new(1));                          // hio constant
            degrees.push(TransitionConstraintDegree::new(1));                          // hpo constant
            degrees.push(TransitionConstraintDegree::with_cycles(7, vec![SEG]));       // oox Feistel
            degrees.push(TransitionConstraintDegree::new(1));                          // ooy' = oox
            degrees.push(TransitionConstraintDegree::with_cycles(1, vec![trace_len])); // first·(oox−hio)
            degrees.push(TransitionConstraintDegree::with_cycles(1, vec![trace_len])); // first·(ooy−hpo)
            degrees.push(TransitionConstraintDegree::with_cycles(1, vec![trace_len])); // rsel·shift
            degrees.push(TransitionConstraintDegree::new(2));                          // bit boolean
            degrees.push(TransitionConstraintDegree::with_cycles(1, vec![trace_len])); // first·(rem−hv)
        }
        let num_assertions = 5 + 2 * N_OUTS;
        SpendFullV4Air {
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
        // 1 exactly at row ROUNDS — where each parallel lane's compression completes.
        let mut selr = vec![BaseElement::ZERO; self.trace_len];
        selr[ROUNDS] = BaseElement::ONE;
        let mut cols = vec![round_constants().to_vec(), reset, pw, first, rsel, selr];
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
        // The conserved value now lives at the INNER lane's row-0 input, because the main
        // lane starts at (inner, pk) rather than (value, blinding).
        result[9] = first * (balance - frame.current()[COL_IX]);
        result[10] = first * (acc - ny);

        // ── owner binding ────────────────────────────────────────────────────────────
        let selr = periodic[5];
        let hi = frame.current()[COL_HI];
        let ix = frame.current()[COL_IX];
        let iy = frame.current()[COL_IY];
        let hp = frame.current()[COL_HP];
        let px = frame.current()[COL_PX];
        let py = frame.current()[COL_PY];

        // inner = compress2(value, blinding), held in `hi`
        result[11] = frame.next()[COL_HI] - hi;
        let it = ix + c;
        let it2 = it * it;
        result[12] = frame.next()[COL_IX] - (iy + it2 * it2 * it2 * it);
        result[13] = frame.next()[COL_IY] - ix;
        result[14] = selr * (ix - hi);

        // pk = compress2(sk, PK_DOMAIN), held in `hp`
        result[16] = frame.next()[COL_HP] - hp;
        let pt = px + c;
        let pt2 = pt * pt;
        result[17] = frame.next()[COL_PX] - (py + pt2 * pt2 * pt2 * pt);
        result[18] = frame.next()[COL_PY] - px;
        result[19] = selr * (px - hp);

        // the Merkle leaf is compress2(inner, pk): the main lane starts at exactly those
        result[15] = first * (x - hi);
        result[20] = first * (y - hp);

        // THE BINDING THAT CLOSES THE FLAW: the key deriving `pk` is the SAME key deriving
        // the nullifier. Without this the two lanes are unrelated and `sk` stays free.
        result[21] = first * (px - nx);

        // ── output side: the new bindings ────────────────────────────────────────────
        for i in 0..N_OUTS {
            let base = 22 + 15 * i;
            let osel = periodic[6 + i];

            let hv = frame.current()[col_hv(i)];
            let rem = frame.current()[col_rem(i)];
            let rbit = frame.current()[col_bit(i)];
            let iox = frame.current()[col_iox(i)];
            let ioy = frame.current()[col_ioy(i)];
            let hio = frame.current()[col_hio(i)];
            let hpo = frame.current()[col_hpo(i)];
            let oox = frame.current()[col_oox(i)];
            let ooy = frame.current()[col_ooy(i)];

            // hv is the binding pivot: constant, so one witness value ties to the
            // conservation lane, the commitment lane and the range decomposition alike.
            result[base] = frame.next()[col_hv(i)] - hv;

            // inner_out_i = compress2(out_value_i, out_blinding_i)
            let it = iox + c;
            let it2 = it * it;
            result[base + 1] = frame.next()[col_iox(i)] - (ioy + it2 * it2 * it2 * it);
            result[base + 2] = frame.next()[col_ioy(i)] - iox;
            result[base + 3] = first * (iox - hv);
            // the value subtracted in the conservation lane at row i+1 IS the held value
            result[base + 4] = osel * (out - hv);
            result[base + 5] = selr * (iox - hio);

            // cm_out_i = compress2(inner_out_i, pk_recipient_i) — the OWNER BINDING for
            // outputs. `hpo` stays a hidden witness so the recipient is never named.
            result[base + 6] = frame.next()[col_hio(i)] - hio;
            result[base + 7] = frame.next()[col_hpo(i)] - hpo;
            let pt = oox + c;
            let pt2 = pt * pt;
            result[base + 8] = frame.next()[col_oox(i)] - (ooy + pt2 * pt2 * pt2 * pt);
            result[base + 9] = frame.next()[col_ooy(i)] - oox;
            result[base + 10] = first * (oox - hio);
            result[base + 11] = first * (ooy - hpo);

            // RANGE: LSB-first shift, gated to the first RANGE_BITS rows.
            result[base + 12] = rsel * (rem - rbit - two * frame.next()[col_rem(i)]);
            result[base + 13] = rbit * (rbit - one);
            result[base + 14] = first * (rem - hv);
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
            a.push(Assertion::single(col_oox(i), ROUNDS, self.cm_outs[i]));
            // the range decomposition is exhausted ⇒ value < 2^RANGE_BITS
            a.push(Assertion::single(col_rem(i), RANGE_BITS, BaseElement::ZERO));
        }
        a
    }

    fn context(&self) -> &AirContext<Self::BaseField> {
        &self.context
    }
}

pub struct SpendFullV4Prover {
    options: ProofOptions,
}
impl SpendFullV4Prover {
    pub fn new(options: ProofOptions) -> Self {
        Self { options }
    }
}
impl Prover for SpendFullV4Prover {
    type BaseField = BaseElement;
    type Air = SpendFullV4Air;
    type Trace = TraceTable<Self::BaseField>;
    type HashFn = Blake3_256<Self::BaseField>;
    type RandomCoin = DefaultRandomCoin<Self::HashFn>;
    type TraceLde<E: FieldElement<BaseField = Self::BaseField>> = DefaultTraceLde<E, Self::HashFn>;
    type ConstraintEvaluator<'a, E: FieldElement<BaseField = Self::BaseField>> =
        DefaultConstraintEvaluator<'a, Self::Air, E>;

    fn get_pub_inputs(&self, trace: &Self::Trace) -> SpendFullV4PublicInputs {
        let mut cm_outs = [BaseElement::ZERO; N_OUTS];
        for (i, slot) in cm_outs.iter_mut().enumerate() {
            *slot = trace.get(col_oox(i), ROUNDS);
        }
        SpendFullV4PublicInputs {
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
/// `outs` is `[(value, blinding, recipient_pk); N_OUTS]`. `recipient_pk` is the owner key
/// the output note is bound to — a spender's OWN pk for change, the payee's for a
/// transfer. It stays a hidden witness, so the recipient is never named on chain. A spend
/// with fewer real outputs passes zero-value notes for the remainder.
///
/// Requires a conserving witness (`value == fee + Σ out_values`) and every amount within
/// `2^RANGE_BITS`; both are checked here so a malformed witness fails loudly at build
/// rather than producing a proof the verifier will silently reject.
pub fn build_spend_full_v4_trace(
    value: BaseElement,
    blinding: BaseElement,
    spend_key: BaseElement,
    fee: BaseElement,
    outs: &[(BaseElement, BaseElement, BaseElement); N_OUTS],
    path: &MerklePath,
) -> TraceTable<BaseElement> {
    let depth = path.siblings.len();
    let len = (depth + 1) * SEG;
    assert!(
        len.is_power_of_two(),
        "spend_full_v4 requires depth+1 a power of two (depth 1, 3, 7, 15, …); got depth {depth}"
    );
    assert!(len > RANGE_BITS, "trace must be longer than the range decomposition");

    // Conservation and range are the prover's obligations; fail loudly rather than emitting
    // a proof that cannot verify.
    let bound = 1u128 << RANGE_BITS;
    let sum: u128 = outs.iter().map(|(v, _, _)| v.as_int() as u128).sum::<u128>() + fee.as_int() as u128;
    assert_eq!(
        sum,
        value.as_int() as u128,
        "non-conserving witness: fee + Σ outputs must equal the note value"
    );
    assert!((fee.as_int() as u128) < bound, "fee exceeds the range bound");
    for (v, _, _) in outs.iter() {
        assert!((v.as_int() as u128) < bound, "output value exceeds the range bound");
    }

    let c = round_constants();
    let position = path_position(path);

    let mut subs = Vec::with_capacity(len);
    subs.push(fee);
    subs.extend(outs.iter().map(|(v, _, _)| *v));
    assert!(subs.len() <= len, "too many outputs for one spend trace");
    subs.resize(len, BaseElement::ZERO);

    let sibs = path.siblings.clone();
    let bits = path.bits.clone();

    let mut trace = TraceTable::new(WIDTH, len);
    trace.fill(
        |state| {
            let inner = compress2(value, blinding);
            let pk = compress2(spend_key, BaseElement::new(PK_DOMAIN));
            state[0] = value;
            state[1] = subs[0];
            // the main lane now starts at (inner, pk); its row-ROUNDS output is the leaf
            state[2] = inner;
            state[3] = pk;
            state[4] = BaseElement::ZERO;
            state[5] = BaseElement::ZERO;
            state[6] = spend_key;
            state[7] = position;
            state[8] = position;
            state[COL_HI] = inner;
            state[COL_IX] = value;
            state[COL_IY] = blinding;
            state[COL_HP] = pk;
            state[COL_PX] = spend_key;
            state[COL_PY] = BaseElement::new(PK_DOMAIN);
            for i in 0..N_OUTS {
                let (ov, ob, opk) = outs[i];
                let inner_out = compress2(ov, ob);
                state[col_hv(i)] = ov;
                state[col_rem(i)] = ov;
                state[col_bit(i)] = BaseElement::new(ov.as_int() & 1);
                state[col_iox(i)] = ov;
                state[col_ioy(i)] = ob;
                state[col_hio(i)] = inner_out;
                state[col_hpo(i)] = opk;
                state[col_oox(i)] = inner_out;
                state[col_ooy(i)] = opk;
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

            // inner + pk lanes feistel unconditionally; `hi`/`hp` stay put as the pivots
            let it = state[COL_IY] + pow7(state[COL_IX] + c[posr]);
            state[COL_IY] = state[COL_IX];
            state[COL_IX] = it;
            let pt = state[COL_PY] + pow7(state[COL_PX] + c[posr]);
            state[COL_PY] = state[COL_PX];
            state[COL_PX] = pt;

            for i in 0..N_OUTS {
                // hv / hio / hpo stay put — they are the pivots the bindings refer to.
                // Both output lanes feistel unconditionally (only row ROUNDS is asserted).
                let it = state[col_ioy(i)] + pow7(state[col_iox(i)] + c[posr]);
                state[col_ioy(i)] = state[col_iox(i)];
                state[col_iox(i)] = it;
                let ot = state[col_ooy(i)] + pow7(state[col_oox(i)] + c[posr]);
                state[col_ooy(i)] = state[col_oox(i)];
                state[col_oox(i)] = ot;

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
/// [`build_spend_full_v4_trace`].
///
/// Those assertions protect an honest caller from emitting a doomed proof, but they are
/// Rust, not cryptography — a real attacker writes their own prover and never runs them.
/// To test that the CONSTRAINT SYSTEM rejects a bad witness we must be able to hand the
/// prover exactly what an attacker would. Everything below this point is identical to the
/// checked builder.
#[cfg(test)]
pub(crate) fn build_v4_trace_unchecked(
    value: BaseElement,
    blinding: BaseElement,
    spend_key: BaseElement,
    fee: BaseElement,
    outs: &[(BaseElement, BaseElement, BaseElement); N_OUTS],
    path: &MerklePath,
) -> TraceTable<BaseElement> {
    let depth = path.siblings.len();
    let len = (depth + 1) * SEG;
    let c = round_constants();
    let position = path_position(path);

    let mut subs = Vec::with_capacity(len);
    subs.push(fee);
    subs.extend(outs.iter().map(|(v, _, _)| *v));
    subs.resize(len, BaseElement::ZERO);

    let sibs = path.siblings.clone();
    let bits = path.bits.clone();

    let mut trace = TraceTable::new(WIDTH, len);
    trace.fill(
        |state| {
            let inner = compress2(value, blinding);
            let pk = compress2(spend_key, BaseElement::new(PK_DOMAIN));
            state[0] = value;
            state[1] = subs[0];
            // the main lane now starts at (inner, pk); its row-ROUNDS output is the leaf
            state[2] = inner;
            state[3] = pk;
            state[4] = BaseElement::ZERO;
            state[5] = BaseElement::ZERO;
            state[6] = spend_key;
            state[7] = position;
            state[8] = position;
            state[COL_HI] = inner;
            state[COL_IX] = value;
            state[COL_IY] = blinding;
            state[COL_HP] = pk;
            state[COL_PX] = spend_key;
            state[COL_PY] = BaseElement::new(PK_DOMAIN);
            for i in 0..N_OUTS {
                let (ov, ob, opk) = outs[i];
                let inner_out = compress2(ov, ob);
                state[col_hv(i)] = ov;
                state[col_rem(i)] = ov;
                state[col_bit(i)] = BaseElement::new(ov.as_int() & 1);
                state[col_iox(i)] = ov;
                state[col_ioy(i)] = ob;
                state[col_hio(i)] = inner_out;
                state[col_hpo(i)] = opk;
                state[col_oox(i)] = inner_out;
                state[col_ooy(i)] = opk;
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

            // inner + pk lanes feistel unconditionally; `hi`/`hp` stay put as the pivots
            let it = state[COL_IY] + pow7(state[COL_IX] + c[posr]);
            state[COL_IY] = state[COL_IX];
            state[COL_IX] = it;
            let pt = state[COL_PY] + pow7(state[COL_PX] + c[posr]);
            state[COL_PY] = state[COL_PX];
            state[COL_PX] = pt;
            for i in 0..N_OUTS {
                let it = state[col_ioy(i)] + pow7(state[col_iox(i)] + c[posr]);
                state[col_ioy(i)] = state[col_iox(i)];
                state[col_iox(i)] = it;
                let ot = state[col_ooy(i)] + pow7(state[col_oox(i)] + c[posr]);
                state[col_ooy(i)] = state[col_oox(i)];
                state[col_oox(i)] = ot;
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

pub fn verify_spend_full_v4(
    proof: winterfell::Proof,
    pub_inputs: SpendFullV4PublicInputs,
) -> Result<(), winterfell::VerifierError> {
    let min = winterfell::AcceptableOptions::MinConjecturedSecurity(ACCEPT_BITS);
    winterfell::verify::<SpendFullV4Air, Blake3_256<BaseElement>, Coin>(proof, pub_inputs, &min)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mimc::mimc_options;

    fn e(v: u64) -> BaseElement { BaseElement::new(v) }
    fn pk_of(sk: BaseElement) -> BaseElement { compress2(sk, BaseElement::new(PK_DOMAIN)) }
    fn leaf(value: BaseElement, blinding: BaseElement, sk: BaseElement) -> BaseElement {
        compress2(compress2(value, blinding), pk_of(sk))
    }
    /// The owner-bound output commitment, off-circuit.
    fn cm_out(v: BaseElement, b: BaseElement, pk: BaseElement) -> BaseElement {
        compress2(compress2(v, b), pk)
    }

    fn pool_with(value: BaseElement, blinding: BaseElement, sk: BaseElement)
        -> (crate::membership::CompressTree, usize)
    {
        let mut leaves: Vec<BaseElement> =
            (0..7).map(|i| crate::note_v1::padding_leaf(i as u64)).collect();
        let position = 3usize;
        leaves.insert(position, leaf(value, blinding, sk));
        (crate::membership::CompressTree::new(leaves), position)
    }

    /// THE V4 GATE: a spend whose outputs are bound to a RECIPIENT's key verifies, and
    /// the recipient key never appears in the public inputs.
    #[test]
    fn owner_bound_outputs_verify_without_naming_the_recipient() {
        let (value, blinding, sk) = (e(100), e(4242), e(0xDEAD));
        let bob = pk_of(e(0xB0B));
        let me = pk_of(sk);
        let (tree, pos) = pool_with(value, blinding, sk);
        let path = tree.path(pos);
        let fee = e(3);
        // 50 to Bob, 47 back to myself as change.
        let outs = [(e(50), e(777), bob), (e(47), e(888), me)];

        let trace = build_spend_full_v4_trace(value, blinding, sk, fee, &outs, &path);
        let proof = SpendFullV4Prover::new(mimc_options()).prove(trace).expect("prove");
        let pub_in = SpendFullV4PublicInputs {
            root: tree.root(),
            nf: compress2(sk, BaseElement::new(pos as u64)),
            fee,
            cm_outs: [cm_out(e(50), e(777), bob), cm_out(e(47), e(888), me)],
        };
        // The recipient key is nowhere in what the verifier is handed.
        assert!(!pub_in.to_elements().contains(&bob), "recipient pk must stay hidden");
        verify_spend_full_v4(proof, pub_in).expect("an honest owner-bound spend must verify");
    }

    /// A foreign key still cannot spend a known note (v3's property, preserved).
    #[test]
    fn a_foreign_key_cannot_spend_a_known_note() {
        let (value, blinding, owner_sk) = (e(100), e(4242), e(0xDEAD));
        let mallory = e(0xBBBB);
        let (tree, pos) = pool_with(value, blinding, owner_sk);
        let path = tree.path(pos);
        let mpk = pk_of(mallory);
        let outs = [(e(50), e(777), mpk), (e(47), e(888), mpk)];
        let trace = build_v4_trace_unchecked(value, blinding, mallory, e(3), &outs, &path);
        let verdict = match std::panic::catch_unwind(|| {
            SpendFullV4Prover::new(mimc_options()).prove(trace)
        }) {
            Err(_) | Ok(Err(_)) => Err(()),
            Ok(Ok(p)) => verify_spend_full_v4(p, SpendFullV4PublicInputs {
                root: tree.root(),
                nf: compress2(mallory, BaseElement::new(pos as u64)),
                fee: e(3),
                cm_outs: [cm_out(e(50), e(777), mpk), cm_out(e(47), e(888), mpk)],
            }).map_err(|_| ()),
        };
        assert!(verdict.is_err(), "SECURITY: (value, blinding) must not be enough to spend");
    }

    /// THE REASON V4 EXISTS: an output commitment that is NOT owner-bound must be
    /// rejected. Under v3 `compress2(out_value, blinding)` was the accepted shape, so a
    /// change note was spendable by anyone who learned its preimage. Here the verifier
    /// must refuse that shape outright.
    #[test]
    fn unowned_output_commitment_is_rejected() {
        let (value, blinding, sk) = (e(100), e(4242), e(0xDEAD));
        let me = pk_of(sk);
        let (tree, pos) = pool_with(value, blinding, sk);
        let path = tree.path(pos);
        let outs = [(e(50), e(777), me), (e(47), e(888), me)];
        let trace = build_spend_full_v4_trace(value, blinding, sk, e(3), &outs, &path);
        let proof = SpendFullV4Prover::new(mimc_options()).prove(trace).expect("prove");
        // v3-shaped (un-owned) output commitments
        let v3_shape = [compress2(e(50), e(777)), compress2(e(47), e(888))];
        assert!(
            verify_spend_full_v4(proof, SpendFullV4PublicInputs {
                root: tree.root(),
                nf: compress2(sk, BaseElement::new(pos as u64)),
                fee: e(3),
                cm_outs: v3_shape,
            }).is_err(),
            "SECURITY: an un-owned output commitment must not verify — that shape is \
             spendable by anyone who learns its preimage"
        );
    }

    /// Inflated output commitments still rejected (v2's property, preserved).
    #[test]
    fn inflated_output_commitment_still_rejected() {
        let (value, blinding, sk) = (e(100), e(4242), e(0xDEAD));
        let me = pk_of(sk);
        let (tree, pos) = pool_with(value, blinding, sk);
        let path = tree.path(pos);
        let outs = [(e(50), e(777), me), (e(47), e(888), me)];
        let trace = build_spend_full_v4_trace(value, blinding, sk, e(3), &outs, &path);
        let proof = SpendFullV4Prover::new(mimc_options()).prove(trace).expect("prove");
        assert!(
            verify_spend_full_v4(proof, SpendFullV4PublicInputs {
                root: tree.root(),
                nf: compress2(sk, BaseElement::new(pos as u64)),
                fee: e(3),
                cm_outs: [cm_out(e(500), e(777), me), cm_out(e(470), e(888), me)],
            }).is_err(),
            "SECURITY: output value binding must survive the v4 restructure"
        );
    }

    /// Membership and the build-time guards did not regress.
    #[test]
    fn root_conservation_and_range_still_enforced() {
        let (value, blinding, sk) = (e(100), e(4242), e(0xDEAD));
        let me = pk_of(sk);
        let (tree, pos) = pool_with(value, blinding, sk);
        let path = tree.path(pos);
        let outs = [(e(50), e(777), me), (e(47), e(888), me)];
        let trace = build_spend_full_v4_trace(value, blinding, sk, e(3), &outs, &path);
        let proof = SpendFullV4Prover::new(mimc_options()).prove(trace).expect("prove");
        assert!(verify_spend_full_v4(proof, SpendFullV4PublicInputs {
            root: e(0xBADC0FFEE),
            nf: compress2(sk, BaseElement::new(pos as u64)),
            fee: e(3),
            cm_outs: [cm_out(e(50), e(777), me), cm_out(e(47), e(888), me)],
        }).is_err(), "wrong root must be rejected");

        assert!(std::panic::catch_unwind(|| {
            build_spend_full_v4_trace(value, blinding, sk, e(3),
                &[(e(50), e(777), me), (e(48), e(888), me)], &path)
        }).is_err(), "non-conserving must be refused");
        let over = BaseElement::new(1u64 << RANGE_BITS);
        assert!(std::panic::catch_unwind(|| {
            build_spend_full_v4_trace(value, blinding, sk, e(3),
                &[(over, e(777), me), (e(0), e(888), me)], &path)
        }).is_err(), "out-of-range must be refused");
    }
}
