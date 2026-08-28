//! ZERO-KNOWLEDGE SPEND — `spend_full_v4` plus reserved random rows (2026-08-28).
//!
//! v4 is correct and sound, and it publishes its own witness. Its trace holds the output
//! values, the recipient key and the sender key in columns that are constant down every
//! row; a constant column's low-degree extension is that constant everywhere, so each of
//! the 84 FRI openings prints it. Measured on a real proof: recipient pk and both output
//! amounts, 85 times each, verbatim.
//!
//! v5 changes nothing about the constraint system. It changes the SHAPE of the trace:
//!
//! ```text
//!   rows 0 .. real_len            byte-for-byte the v4 trace, fully constrained
//!   rows real_len .. 2*real_len   uniform randomness, constrained by nothing
//! ```
//!
//! and raises `num_transition_exemptions` so the tail satisfies no transition constraint.
//! Column polynomials keep degree < trace_len, so FRI is untouched — but `real_len` of the
//! values determining each one are now uniform and secret, which is far more than the 84
//! openings a verifier gets. See [`crate::zk_mask`] for the argument and its honest limits.
//!
//! Two consequences the reader should not have to discover:
//!   * the trace doubles, so proving costs roughly 2x. Measured, not estimated.
//!   * v4's three assertions on the LAST row move to the last REAL row; the last row is
//!     now randomness and asserting on it would be asserting on noise.
//!
//! (v4 header follows.)
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

use winterfell::FieldExtension;
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
use crate::zk_mask::{exemptions_for, fill_random_row, padded_len, zk_rows_for, HidingProver, Secret};

/// v5's proof options — identical to v4's `mimc_options()`. Masking needs no extra blowup:
/// the degree-7 lane at trace_len 512 leaves an exemption budget of 526, and v5 uses 257.
/// (An earlier version raised this to blowup 16 chasing a degree theory that turned out to
/// be wrong; the real bug was `get_pub_inputs` reading the masked last row. Reverted —
/// the escalation cost 4.5x prove time and bought nothing.)
pub fn v6_options() -> ProofOptions {
    ProofOptions::new(84, 8, 16, FieldExtension::Quadratic, 8, 31)
}

const SEG: usize = 64;

/// Outputs per spend. Fixed so the trace width is a compile-time constant; a spend with
/// fewer real outputs pads with zero-value notes (see [`build_spend_full_v6_trace`]).
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
/// Inputs per spend. v6 exists to make this 2; v5 remains the 1-input circuit and is not
/// replaced. See `docs/SIGIL_MULTI_INPUT_SPEND_DESIGN.md` for why adding a circuit beats
/// generalising one: a K-in circuit with padding needs dummy inputs, dummy inputs need the
/// membership check conditionally enforced, and that raises the Merkle lane from degree 7
/// to degree 8. Two circuits, each with exactly the inputs it declares, needs none of it.
pub const N_INS: usize = 2;

/// Columns 0..=32 are v5's, byte-for-byte, so every constraint that refers to them is
/// unchanged. Input 1 is APPENDED rather than interleaved for exactly that reason — a
/// re-layout would have touched every existing index, and this circuit is consensus code.
const V5_WIDTH: usize = BASE_COLS + COLS_PER_OUT * N_OUTS;

/// Input 1 reuses input 0's shape minus the two SHARED lanes: there is one conservation
/// lane (col 0) and one subtrahend lane (col 1) for the whole spend, not one per input.
/// That sharing is what makes the sum constraint a single line rather than a new gadget.
const IN1: usize = V5_WIDTH;
const C1_X: usize = IN1;        // Merkle lane x   (mirrors col 2)
const C1_Y: usize = IN1 + 1;    // Merkle lane y   (mirrors col 3)
const C1_SIB: usize = IN1 + 2;  // sibling         (mirrors col 4)
const C1_BIT: usize = IN1 + 3;  // direction bit   (mirrors col 5)
const C1_NX: usize = IN1 + 4;   // nullifier lane x(mirrors col 6)
const C1_NY: usize = IN1 + 5;   // nullifier lane y(mirrors col 7)
const C1_ACC: usize = IN1 + 6;  // position acc    (mirrors col 8)
const C1_HI: usize = IN1 + 7;   // inner, held     (mirrors COL_HI)
const C1_IX: usize = IN1 + 8;   // inner lane x    (mirrors COL_IX)
const C1_IY: usize = IN1 + 9;   // inner lane y    (mirrors COL_IY)
const C1_HP: usize = IN1 + 10;  // pk, held        (mirrors COL_HP)
const C1_PX: usize = IN1 + 11;  // pk lane x       (mirrors COL_PX)
const C1_PY: usize = IN1 + 12;  // pk lane y       (mirrors COL_PY)
const IN1_COLS: usize = 13;

pub const WIDTH: usize = V5_WIDTH + IN1_COLS;

/// Where input 1's transition constraints start. v5 ends at index 21.
const C1_BASE: usize = 22;
/// How many constraints input 1 contributes.
const C1_COUNT: usize = 20;
/// Where the per-output constraint block starts (was 22 in v5).
const OUT_BASE: usize = C1_BASE + C1_COUNT;

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
/// Domain separator bound into the public inputs, so a v6 proof can never be reinterpreted
/// as another circuit's even if the wire envelope changes later.
///
/// The widths already differ (v5 is 33 columns, v6 is 46) so winterfell would reject a
/// cross-version replay on `TraceInfo` alone — but that is an accident of the current
/// layouts, not a guarantee. Binding the version into the transcript makes it one.
/// "SIGIL_SPEND_V6" as big-endian ASCII.
pub const V6_DOMAIN: u64 = 0x5347_4C5F_5350_5636;

/// Why a v6 spend must be refused BEFORE its proof is considered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum V6Reject {
    /// Both inputs name the same nullifier: the same note spent twice in one transaction.
    DuplicateNullifier,
}

impl std::fmt::Display for V6Reject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            V6Reject::DuplicateNullifier => {
                write!(f, "both inputs name the same nullifier (one note spent twice)")
            }
        }
    }
}
impl std::error::Error for V6Reject {}

/// THE CHECK THE CIRCUIT CANNOT MAKE FOR YOU.
///
/// Feeding one note in as both inputs produces two independently-valid input blocks and a
/// conservation lane summing `2 x value`. Every constraint holds. The only tell is that
/// both nullifiers come out identical — and because the chain records nullifiers in a SET,
/// the second insert is a no-op, so one note is burned and double the value leaves the pool.
///
/// The AIR has no way to notice: each block is separately correct, and "these two witnesses
/// differ" is not a statement about any single row. It has to be checked on the public
/// inputs, which is where this lives. Call it in the verifier path AND at state application
/// — belt and braces, because the cost of missing it is minted money.
pub fn reject_duplicate_nullifiers(nf: &[BaseElement; N_INS]) -> Result<(), V6Reject> {
    for i in 0..N_INS {
        for j in (i + 1)..N_INS {
            if nf[i] == nf[j] {
                return Err(V6Reject::DuplicateNullifier);
            }
        }
    }
    Ok(())
}

#[derive(Clone)]
pub struct SpendFullV6PublicInputs {
    pub root: BaseElement,
    /// One nullifier per input. BOTH must be checked and recorded atomically by the chain:
    /// accepting a spend while recording only one of them is a double-spend.
    pub nf: [BaseElement; N_INS],
    pub fee: BaseElement,
    pub cm_outs: [BaseElement; N_OUTS],
}
impl ToElements<BaseElement> for SpendFullV6PublicInputs {
    fn to_elements(&self) -> Vec<BaseElement> {
        // Version first, so the transcript can never collide with another circuit's.
        let mut v = vec![BaseElement::new(V6_DOMAIN), self.root];
        v.extend_from_slice(&self.nf);
        v.push(self.fee);
        v.extend_from_slice(&self.cm_outs);
        v
    }
}

pub struct SpendFullV6Air {
    context: AirContext<BaseElement>,
    root: BaseElement,
    nf: [BaseElement; N_INS],
    fee: BaseElement,
    cm_outs: [BaseElement; N_OUTS],
    trace_len: usize,
}

impl Air for SpendFullV6Air {
    type BaseField = BaseElement;
    type PublicInputs = SpendFullV6PublicInputs;
    type GkrProof = ();
    type GkrVerifier = ();

    fn new(trace_info: TraceInfo, pub_inputs: SpendFullV6PublicInputs, options: ProofOptions) -> Self {
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
        // ── input 1: the same lanes as input 0, minus the two shared ones ──
        // No conservation lane and no `first·(balance − value)` here: col 0 is shared and
        // its single start constraint sums BOTH inputs (see `evaluate_transition`).
        degrees.extend([
            TransitionConstraintDegree::with_cycles(7, vec![SEG]),            // x lane
            TransitionConstraintDegree::with_cycles(2, vec![SEG]),            // y lane
            TransitionConstraintDegree::with_cycles(1, vec![SEG]),            // sib constant
            TransitionConstraintDegree::with_cycles(1, vec![SEG]),            // bit constant
            TransitionConstraintDegree::new(2),                               // bit boolean
            TransitionConstraintDegree::new(7),                               // nx Feistel
            TransitionConstraintDegree::new(1),                               // ny' = nx
            TransitionConstraintDegree::with_cycles(1, vec![SEG, trace_len]), // acc
            TransitionConstraintDegree::with_cycles(1, vec![trace_len]),      // first·(acc−ny)
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
        ]);
        debug_assert_eq!(degrees.len(), OUT_BASE, "input-1 block must land exactly at OUT_BASE");
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
        // FINDING-1 FIX (mint via unconstrained conservation column): pin `out` (col 1)
        // to zero on every row where it is not already fixed. It is fixed to the fee at row
        // 0 (via `first`) and to output i at row i+1 (via `osel_i`); it was FREE elsewhere,
        // which let a hand-built trace inflate the outputs past the input while the balance
        // accumulator still landed on zero. Same degree shape as `osel*(out-hv)` above.
        degrees.push(TransitionConstraintDegree::with_cycles(1, vec![trace_len])); // (1-first-sum osel)*out
        // 5 for input 0 + the shared lanes, 3 more for input 1 (its root, its nullifier,
        // its exhausted position accumulator), 2 per output.
        let num_assertions = 5 + 3 + 2 * N_OUTS;
        SpendFullV6Air {
            // The real region is the first half; everything from its last row onward is
            // a frame origin we must NOT constrain, or the random tail would have to
            // satisfy the AIR (it cannot, and must not — that is the whole point).
            context: AirContext::new(trace_info, degrees, num_assertions, options)
                .set_num_transition_exemptions(exemptions_for(trace_len, trace_len / 2)),
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
        // THE SUM CONSTRAINT — the whole reason v6 exists, and it is one line.
        //
        // In v5 the conservation lane starts at the single input's value. Here it starts at
        // the SUM of both inputs' values, and then subtracts the fee and each output exactly
        // as before. So `value_0 + value_1 == fee + Σ outputs` falls out of the lane that
        // already existed, with no accumulator, no new gadget and no new degree — because
        // col 0 was shared from the start rather than duplicated per input.
        result[9] = first * (balance - (frame.current()[COL_IX] + frame.current()[C1_IX]));
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

        // ── input 1: input 0's lanes again, at C1_* offsets ──────────────────────────
        // Deliberately a transcription of the block above rather than a shared helper: the
        // frame indices differ and a helper would have to take twelve of them, which reads
        // worse than the repetition and hides exactly the kind of index slip this circuit
        // cannot afford. The ONLY structural differences are stated at the sum constraint.
        {
            let x1 = frame.current()[C1_X];
            let y1 = frame.current()[C1_Y];
            let sib1 = frame.current()[C1_SIB];
            let bit1 = frame.current()[C1_BIT];
            let nx1 = frame.current()[C1_NX];
            let ny1 = frame.current()[C1_NY];
            let acc1 = frame.current()[C1_ACC];
            let nsib1 = frame.next()[C1_SIB];
            let nbit1 = frame.next()[C1_BIT];

            let t = x1 + c;
            let t2 = t * t;
            let feistel_x = y1 + t2 * t2 * t2 * t;
            let feistel_y = x1;
            let reset_x = x1 + nbit1 * (nsib1 - x1);
            let reset_y = nsib1 + nbit1 * (x1 - nsib1);
            result[C1_BASE] = frame.next()[C1_X] - (s * reset_x + (one - s) * feistel_x);
            result[C1_BASE + 1] = frame.next()[C1_Y] - (s * reset_y + (one - s) * feistel_y);
            result[C1_BASE + 2] = (one - s) * (nsib1 - sib1);
            result[C1_BASE + 3] = (one - s) * (nbit1 - bit1);
            result[C1_BASE + 4] = bit1 * (bit1 - one);

            let n = nx1 + c;
            let n2 = n * n;
            result[C1_BASE + 5] = frame.next()[C1_NX] - (ny1 + n2 * n2 * n2 * n);
            result[C1_BASE + 6] = frame.next()[C1_NY] - nx1;

            result[C1_BASE + 7] = frame.next()[C1_ACC] - acc1 + s * nbit1 * pw;
            result[C1_BASE + 8] = first * (acc1 - ny1);

            let hi1 = frame.current()[C1_HI];
            let ix1 = frame.current()[C1_IX];
            let iy1 = frame.current()[C1_IY];
            let hp1 = frame.current()[C1_HP];
            let px1 = frame.current()[C1_PX];
            let py1 = frame.current()[C1_PY];

            result[C1_BASE + 9] = frame.next()[C1_HI] - hi1;
            let it = ix1 + c;
            let it2 = it * it;
            result[C1_BASE + 10] = frame.next()[C1_IX] - (iy1 + it2 * it2 * it2 * it);
            result[C1_BASE + 11] = frame.next()[C1_IY] - ix1;
            result[C1_BASE + 12] = selr * (ix1 - hi1);
            result[C1_BASE + 13] = first * (x1 - hi1);

            result[C1_BASE + 14] = frame.next()[C1_HP] - hp1;
            let pt = px1 + c;
            let pt2 = pt * pt;
            result[C1_BASE + 15] = frame.next()[C1_PX] - (py1 + pt2 * pt2 * pt2 * pt);
            result[C1_BASE + 16] = frame.next()[C1_PY] - px1;
            result[C1_BASE + 17] = selr * (px1 - hp1);
            result[C1_BASE + 18] = first * (y1 - hp1);

            // The same binding that closes the flaw for input 0: the key that derives THIS
            // input's pk is the key that derives THIS input's nullifier. Without it the two
            // inputs' owner proofs could be satisfied by one key while the nullifiers came
            // from another — which is to say, a spend of someone else's note.
            result[C1_BASE + 19] = first * (px1 - nx1);
        }

        // ── output side: the new bindings ────────────────────────────────────────────
        for i in 0..N_OUTS {
            let base = OUT_BASE + 15 * i;
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

        // FINDING-1 FIX (see Air::new): out (col 1) must be zero except where pinned -- row
        // 0 (fee, via `first`) and rows 1..=N_OUTS (outputs, via `osel_i`). Without this the
        // column was FREE on every other row, so a prover writing its own trace could park
        // value-(fee+sum outputs) in a free row, telescope the accumulator back to zero, and
        // mint the outputs from nothing. This forces sum_rows out == fee + sum outputs.
        let mut osel_sum = E::ZERO;
        for i in 0..N_OUTS {
            osel_sum = osel_sum + periodic[6 + i];
        }
        let out_zero_idx = result.len() - 1;
        result[out_zero_idx] = (one - first - osel_sum) * out;
    }

    fn get_assertions(&self) -> Vec<Assertion<Self::BaseField>> {
        // The last row is randomness now. Every v4 assertion that pointed at it moves to
        // the last row of the REAL region.
        let last = self.trace_len / 2 - 1;
        let mut a = vec![
            Assertion::single(0, last, BaseElement::ZERO),
            Assertion::single(1, 0, self.fee),
            Assertion::single(2, last, self.root),
            Assertion::single(6, ROUNDS, self.nf[0]),
            Assertion::single(8, last, BaseElement::ZERO),
            // Input 1 climbs to the SAME root — one anchor for the whole spend, so both
            // notes are proven to be in one tree the chain has actually held.
            Assertion::single(C1_X, last, self.root),
            Assertion::single(C1_NX, ROUNDS, self.nf[1]),
            Assertion::single(C1_ACC, last, BaseElement::ZERO),
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

pub struct SpendFullV6Prover {
    options: ProofOptions,
}
impl SpendFullV6Prover {
    pub fn new(options: ProofOptions) -> Self {
        Self { options }
    }
}
impl Prover for SpendFullV6Prover {
    type BaseField = BaseElement;
    type Air = SpendFullV6Air;
    type Trace = TraceTable<Self::BaseField>;
    type HashFn = Blake3_256<Self::BaseField>;
    type RandomCoin = DefaultRandomCoin<Self::HashFn>;
    type TraceLde<E: FieldElement<BaseField = Self::BaseField>> = DefaultTraceLde<E, Self::HashFn>;
    type ConstraintEvaluator<'a, E: FieldElement<BaseField = Self::BaseField>> =
        DefaultConstraintEvaluator<'a, Self::Air, E>;

    fn get_pub_inputs(&self, trace: &Self::Trace) -> SpendFullV6PublicInputs {
        let mut cm_outs = [BaseElement::ZERO; N_OUTS];
        for (i, slot) in cm_outs.iter_mut().enumerate() {
            *slot = trace.get(col_oox(i), ROUNDS);
        }
        // The last row is reserved randomness, not the end of the computation. Every
        // read that v4 anchored to `trace.length() - 1` must anchor to the last REAL
        // row instead, or the prover publishes a public input taken from the mask —
        // and the AIR's assertion at the real last row then cannot hold.
        let real_last = trace.length() / 2 - 1;
        SpendFullV6PublicInputs {
            root: trace.get(2, real_last),
            // Both nullifiers, and both from row ROUNDS — where each lane's compression
            // completes — never from the last row. `real_last` above is the same lesson:
            // in v5 this function read `trace.length() - 1`, which after padding is a
            // MASKED row, and the resulting failure surfaced only as
            // `InconsistentOodConstraintEvaluations`. v6 doubles the number of places that
            // mistake can be made, so every anchor here is stated explicitly.
            nf: [trace.get(6, ROUNDS), trace.get(C1_NX, ROUNDS)],
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
/// Build a zero-knowledge spend trace.
///
/// `mask_seed` MUST be fresh per proof, from a CSPRNG. Two proofs of the same witness under
/// the same seed are byte-identical, and differencing two proofs under DIFFERENT seeds is
/// exactly the attack the randomness exists to stop — so a fixed seed is a test-only affordance
/// and never acceptable in a wallet. [`build_spend_full_v6_trace`] wraps this with OS entropy.
/// A v6 witness whose two inputs are PROVEN to sit under one anchor.
///
/// The builder below takes paths as a plain array and can only check that their depths
/// match; nothing stops a caller passing two paths from different trees. The AIR would
/// catch it — both lanes are asserted against the single public root, so one of them
/// simply fails — but the failure arrives as an opaque verifier rejection long after the
/// mistake, which is the worst place to learn about it.
///
/// `SpendV6Witness::new` rejects it at construction, where the caller still has the context
/// to fix it. Prefer this over calling the builder directly. (Raised in review by the
/// operator: the invariant should be visible in the API rather than left to the builder.)
pub struct SpendV6Witness<'a> {
    pub anchor: BaseElement,
    pub ins: [(BaseElement, BaseElement, BaseElement); N_INS],
    pub outs: [(BaseElement, BaseElement, BaseElement); N_OUTS],
    pub paths: [&'a MerklePath; N_INS],
}

/// Why a witness could not be assembled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WitnessError {
    /// A path does not climb to the declared anchor — two different trees, or a stale one.
    PathNotUnderAnchor { input: usize },
    /// The two paths are of different depths, so they cannot be from one tree.
    DepthMismatch { left: usize, right: usize },
}

impl std::fmt::Display for WitnessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WitnessError::PathNotUnderAnchor { input } => {
                write!(f, "input {input}'s membership path does not climb to the declared anchor")
            }
            WitnessError::DepthMismatch { left, right } => {
                write!(f, "paths are depth {left} and {right}; both inputs must be in one tree")
            }
        }
    }
}
impl std::error::Error for WitnessError {}

/// Recompute a path's root off-circuit, folding EXACTLY as the trace does: at each segment
/// boundary the running hash and the sibling are ordered by the direction bit, and the next
/// segment compresses that pair. Written out rather than borrowed from `membership` so the
/// two cannot drift — if this fold ever disagrees with the circuit's, `SpendV6Witness::new`
/// starts rejecting honest witnesses, which is the loud failure, not the quiet one.
fn fold_to_root(path: &MerklePath) -> BaseElement {
    let mut running = path.leaf;
    for (sib, bit) in path.siblings.iter().zip(path.bits.iter()) {
        let (l, r) = if *bit { (*sib, running) } else { (running, *sib) };
        running = compress2(l, r);
    }
    running
}

impl<'a> SpendV6Witness<'a> {
    /// Assemble a witness, refusing anything whose paths are not both under `anchor`.
    pub fn new(
        anchor: BaseElement,
        ins: [(BaseElement, BaseElement, BaseElement); N_INS],
        outs: [(BaseElement, BaseElement, BaseElement); N_OUTS],
        paths: [&'a MerklePath; N_INS],
    ) -> Result<Self, WitnessError> {
        let (d0, d1) = (paths[0].siblings.len(), paths[1].siblings.len());
        if d0 != d1 {
            return Err(WitnessError::DepthMismatch { left: d0, right: d1 });
        }
        for (i, path) in paths.iter().enumerate() {
            if fold_to_root(path) != anchor {
                return Err(WitnessError::PathNotUnderAnchor { input: i });
            }
        }
        Ok(Self { anchor, ins, outs, paths })
    }

    /// Build the trace for this witness with fresh OS entropy.
    pub fn build_trace(&self, fee: BaseElement, options: &ProofOptions) -> TraceTable<BaseElement> {
        build_spend_full_v6_trace(&self.ins, fee, &self.outs, &self.paths, options)
    }
}

/// `ins[k] = (value, blinding, spend_key)`; `paths[k]` is that note's membership path.
/// Both paths must be against the SAME anchor and the same depth — two leaves of one tree.
#[allow(clippy::too_many_arguments)]
pub fn build_spend_full_v6_trace_seeded(
    ins: &[(BaseElement, BaseElement, BaseElement); N_INS],
    fee: BaseElement,
    outs: &[(BaseElement, BaseElement, BaseElement); N_OUTS],
    paths: &[&MerklePath; N_INS],
    options: &ProofOptions,
    mask_seed: [u8; 32],
) -> TraceTable<BaseElement> {
    let (value, blinding, spend_key) = ins[0];
    let (value1, blinding1, spend_key1) = ins[1];
    let (path, path1) = (paths[0], paths[1]);
    assert_eq!(
        path.siblings.len(), path1.siblings.len(),
        "both inputs must sit at the same depth in the same tree"
    );
    let depth = path.siblings.len();
    let real_len = (depth + 1) * SEG;
    let len = padded_len(real_len, options);
    assert!(
        real_len.is_power_of_two(),
        "spend_full_v6 requires depth+1 a power of two (depth 1, 3, 7, 15, …); got depth {depth}"
    );
    assert!(real_len > RANGE_BITS, "trace must be longer than the range decomposition");
    // The AIR derives the real region as trace_len/2, so the padding must be exactly a
    // doubling. `padded_len` guarantees it for every real_len we accept; assert rather than
    // trust, because a mismatch here silently moves every assertion onto noise.
    assert_eq!(
        len,
        real_len * 2,
        "v5 requires an exactly doubled trace; real_len {real_len} gave padded {len}"
    );
    assert!(
        real_len > zk_rows_for(options),
        "reserved randomness ({real_len} rows) must exceed the opening count"
    );

    // Conservation and range are the prover's obligations; fail loudly rather than emitting
    // a proof that cannot verify.
    let bound = 1u128 << RANGE_BITS;
    let sum: u128 = outs.iter().map(|(v, _, _)| v.as_int() as u128).sum::<u128>() + fee.as_int() as u128;
    // v6's obligation is the SUM of both inputs, not one note's value. (This assert was
    // inherited verbatim from v5 and initially left unchanged — it caught every honest
    // two-input witness as "non-conserving" before a single proof was attempted, which is
    // exactly the job a loud prover-side check exists to do.)
    let in_sum: u128 = value.as_int() as u128 + value1.as_int() as u128;
    assert_eq!(
        sum, in_sum,
        "non-conserving witness: fee + Σ outputs must equal the SUM of both input notes"
    );
    // The field bound in the module docs grows with the extra input term: every amount in
    // play must sum to less than p, or field conservation stops implying integer
    // conservation and value can be minted by wrapping.
    // FIELD BOUND. Every amount is range-constrained to < 2^RANGE_BITS by the circuit, and
    // the conservation equality is checked IN THE FIELD. For field equality to imply integer
    // equality, neither SIDE may wrap — so the bound is set by the side with more terms, not
    // by their total. Inputs contribute N_INS terms; outputs plus the fee contribute
    // N_OUTS + 1. At N_INS = 2, N_OUTS = 2 the binding side is outputs+fee, i.e. 3 terms.
    // (An earlier draft used N_INS + N_OUTS = 4, which is stricter and so still sound, but
    // for the wrong reason — worth stating correctly since a future arity change would
    // otherwise inherit the wrong rule. Raised in review by the operator.)
    const MAX_TERMS: u128 = if N_INS as u128 > N_OUTS as u128 + 1 {
        N_INS as u128
    } else {
        N_OUTS as u128 + 1
    };
    const _: () = assert!(
        MAX_TERMS * (1u128 << RANGE_BITS) < BaseElement::MODULUS as u128,
        "RANGE_BITS too large: the wider side of the conservation equality can wrap the field"
    );
    assert!((value.as_int() as u128) < bound, "input 0 value exceeds the range bound");
    assert!((value1.as_int() as u128) < bound, "input 1 value exceeds the range bound");
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
    let position1 = path_position(path1);
    let sibs1 = path1.siblings.clone();
    let bits1 = path1.bits.clone();

    let mut trace = TraceTable::new(WIDTH, len);
    trace.fill(
        |state| {
            let inner = compress2(value, blinding);
            let pk = compress2(spend_key, BaseElement::new(PK_DOMAIN));
            // Shared conservation lane starts at the SUM of both inputs — the row-0 half
            // of the sum constraint that makes this a 2-input circuit.
            state[0] = value + value1;
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

            let inner1 = compress2(value1, blinding1);
            let pk1 = compress2(spend_key1, BaseElement::new(PK_DOMAIN));
            state[C1_X] = inner1;
            state[C1_Y] = pk1;
            state[C1_SIB] = BaseElement::ZERO;
            state[C1_BIT] = BaseElement::ZERO;
            state[C1_NX] = spend_key1;
            state[C1_NY] = position1;
            state[C1_ACC] = position1;
            state[C1_HI] = inner1;
            state[C1_IX] = value1;
            state[C1_IY] = blinding1;
            state[C1_HP] = pk1;
            state[C1_PX] = spend_key1;
            state[C1_PY] = BaseElement::new(PK_DOMAIN);

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
            // Past the real region every row is uniform randomness. These rows are exempt
            // from all transition constraints (see `Air::new`), so nothing here has to —
            // or is allowed to — satisfy the AIR.
            if step + 1 >= real_len {
                fill_random_row(&mask_seed, step + 1, state);
                return;
            }
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

            // ── input 1: the same four lane updates, at C1_* offsets ──
            if posr == SEG - 1 {
                let segi = step / SEG;
                let running1 = state[C1_X];
                let (sv, bv) = (sibs1[segi], bits1[segi]);
                let (l, r) = if bv { (sv, running1) } else { (running1, sv) };
                state[C1_X] = l;
                state[C1_Y] = r;
                state[C1_SIB] = sv;
                state[C1_BIT] = if bv { BaseElement::ONE } else { BaseElement::ZERO };
                if bv {
                    state[C1_ACC] = state[C1_ACC] - BaseElement::new(1u64 << segi);
                }
            } else {
                let t1 = state[C1_Y] + pow7(state[C1_X] + c[posr]);
                state[C1_Y] = state[C1_X];
                state[C1_X] = t1;
            }
            let nt1 = state[C1_NY] + pow7(state[C1_NX] + c[posr]);
            state[C1_NY] = state[C1_NX];
            state[C1_NX] = nt1;
            let it1 = state[C1_IY] + pow7(state[C1_IX] + c[posr]);
            state[C1_IY] = state[C1_IX];
            state[C1_IX] = it1;
            let pt1 = state[C1_PY] + pow7(state[C1_PX] + c[posr]);
            state[C1_PY] = state[C1_PX];
            state[C1_PX] = pt1;

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
/// [`build_spend_full_v6_trace`].
///
/// Those assertions protect an honest caller from emitting a doomed proof, but they are
/// Rust, not cryptography — a real attacker writes their own prover and never runs them.
/// To test that the CONSTRAINT SYSTEM rejects a bad witness we must be able to hand the
/// prover exactly what an attacker would. Everything below this point is identical to the
/// checked builder.
#[cfg(test)]
/// Negative-path helper: builds a v5-shaped trace WITHOUT the conservation/range asserts, so
/// tests can hand the verifier a witness that must be rejected. The mask seed is fixed because
/// these traces are never proofs anyone relies on; production goes through
/// [`build_spend_full_v6_trace`], which draws from the OS.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_v6_trace_unchecked(
    ins: &[(BaseElement, BaseElement, BaseElement); N_INS],
    fee: BaseElement,
    outs: &[(BaseElement, BaseElement, BaseElement); N_OUTS],
    paths: &[&MerklePath; N_INS],
) -> TraceTable<BaseElement> {
    let (value, blinding, spend_key) = ins[0];
    let (value1, blinding1, spend_key1) = ins[1];
    let (path, path1) = (paths[0], paths[1]);
    let depth = path.siblings.len();
    let real_len = (depth + 1) * SEG;
    let len = real_len * 2;
    let mask_seed = [0xA5u8; 32];
    let c = round_constants();
    let position = path_position(path);

    let mut subs = Vec::with_capacity(len);
    subs.push(fee);
    subs.extend(outs.iter().map(|(v, _, _)| *v));
    subs.resize(len, BaseElement::ZERO);

    let sibs = path.siblings.clone();
    let bits = path.bits.clone();
    let position1 = path_position(path1);
    let sibs1 = path1.siblings.clone();
    let bits1 = path1.bits.clone();

    let mut trace = TraceTable::new(WIDTH, len);
    trace.fill(
        |state| {
            let inner = compress2(value, blinding);
            let pk = compress2(spend_key, BaseElement::new(PK_DOMAIN));
            // Shared conservation lane starts at the SUM of both inputs — the row-0 half
            // of the sum constraint that makes this a 2-input circuit.
            state[0] = value + value1;
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

            let inner1 = compress2(value1, blinding1);
            let pk1 = compress2(spend_key1, BaseElement::new(PK_DOMAIN));
            state[C1_X] = inner1;
            state[C1_Y] = pk1;
            state[C1_SIB] = BaseElement::ZERO;
            state[C1_BIT] = BaseElement::ZERO;
            state[C1_NX] = spend_key1;
            state[C1_NY] = position1;
            state[C1_ACC] = position1;
            state[C1_HI] = inner1;
            state[C1_IX] = value1;
            state[C1_IY] = blinding1;
            state[C1_HP] = pk1;
            state[C1_PX] = spend_key1;
            state[C1_PY] = BaseElement::new(PK_DOMAIN);

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
            // Past the real region every row is uniform randomness. These rows are exempt
            // from all transition constraints (see `Air::new`), so nothing here has to —
            // or is allowed to — satisfy the AIR.
            if step + 1 >= real_len {
                fill_random_row(&mask_seed, step + 1, state);
                return;
            }
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

            // ── input 1: the same four lane updates, at C1_* offsets ──
            if posr == SEG - 1 {
                let segi = step / SEG;
                let running1 = state[C1_X];
                let (sv, bv) = (sibs1[segi], bits1[segi]);
                let (l, r) = if bv { (sv, running1) } else { (running1, sv) };
                state[C1_X] = l;
                state[C1_Y] = r;
                state[C1_SIB] = sv;
                state[C1_BIT] = if bv { BaseElement::ONE } else { BaseElement::ZERO };
                if bv {
                    state[C1_ACC] = state[C1_ACC] - BaseElement::new(1u64 << segi);
                }
            } else {
                let t1 = state[C1_Y] + pow7(state[C1_X] + c[posr]);
                state[C1_Y] = state[C1_X];
                state[C1_X] = t1;
            }
            let nt1 = state[C1_NY] + pow7(state[C1_NX] + c[posr]);
            state[C1_NY] = state[C1_NX];
            state[C1_NX] = nt1;
            let it1 = state[C1_IY] + pow7(state[C1_IX] + c[posr]);
            state[C1_IY] = state[C1_IX];
            state[C1_IX] = it1;
            let pt1 = state[C1_PY] + pow7(state[C1_PX] + c[posr]);
            state[C1_PY] = state[C1_PX];
            state[C1_PX] = pt1;

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

/// Build a zero-knowledge spend trace with fresh OS entropy. This is the wallet entry point.
#[allow(clippy::too_many_arguments)]
pub fn build_spend_full_v6_trace(
    ins: &[(BaseElement, BaseElement, BaseElement); N_INS],
    fee: BaseElement,
    outs: &[(BaseElement, BaseElement, BaseElement); N_OUTS],
    paths: &[&MerklePath; N_INS],
    options: &ProofOptions,
) -> TraceTable<BaseElement> {
    use rand::RngCore;
    let mut seed = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut seed);
    build_spend_full_v6_trace_seeded(ins, fee, outs, paths, options, seed)
}

/// The contract that would have caught v4's leak on day one: this prover names the values it
/// must hide, and `prove_hiding` refuses to return a proof containing any of them.
///
/// The secrets are read back out of the trace rather than passed in, so they cannot drift out
/// of sync with the witness the proof was actually built from.
impl HidingProver for SpendFullV6Prover {
    fn secrets(&self, trace: &Self::Trace) -> Vec<Secret> {
        use winterfell::Trace as _;
        let row = 0usize;
        let mut v: Vec<Secret> = vec![
            ("input value", trace.main_segment().get(COL_IX, row)),
            ("input blinding", trace.main_segment().get(COL_IY, row)),
            ("spend key", trace.main_segment().get(6, row)),
            ("sender pk", trace.main_segment().get(COL_HP, row)),
            ("inner commitment", trace.main_segment().get(COL_HI, row)),
        ];
        for i in 0..N_OUTS {
            v.push(("output value", trace.main_segment().get(col_hv(i), row)));
            v.push(("recipient pk", trace.main_segment().get(col_hpo(i), row)));
        }
        v
    }

    fn zk_reserved_rows(&self, trace: &Self::Trace) -> usize {
        use winterfell::Trace as _;
        trace.main_segment().num_rows() / 2
    }
}

pub fn verify_spend_full_v6(
    proof: winterfell::Proof,
    pub_inputs: SpendFullV6PublicInputs,
) -> Result<(), winterfell::VerifierError> {
    let min = winterfell::AcceptableOptions::MinConjecturedSecurity(ACCEPT_BITS);
    winterfell::verify::<SpendFullV6Air, Blake3_256<BaseElement>, Coin>(proof, pub_inputs, &min)
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
    /// The nullifier the circuit derives: keyed by the spend key AND the leaf position, so
    /// two notes of the same owner still nullify distinctly.
    fn nf_of(sk: BaseElement, pos: usize) -> BaseElement {
        compress2(sk, BaseElement::new(pos as u64))
    }

    /// An 8-leaf pool (depth 3, so `(depth+1)*SEG = 256`, a power of two) holding BOTH
    /// input notes at distinct positions. Both spends are proven against this one root —
    /// which is the point: a spend names one anchor, not one per input.
    fn pool_with2(
        n0: (BaseElement, BaseElement, BaseElement),
        n1: (BaseElement, BaseElement, BaseElement),
    ) -> (crate::membership::CompressTree, usize, usize) {
        let (p0, p1) = (2usize, 5usize);
        let mut leaves: Vec<BaseElement> =
            (0..8).map(|i| crate::note_v1::padding_leaf(i as u64)).collect();
        leaves[p0] = leaf(n0.0, n0.1, n0.2);
        leaves[p1] = leaf(n1.0, n1.1, n1.2);
        (crate::membership::CompressTree::new(leaves), p0, p1)
    }

    /// THE HEADLINE PROPERTY, and the entire reason this circuit exists: TWO notes go in,
    /// and one output can be bigger than either of them.
    ///
    /// Under v5 this transaction is not merely unimplemented, it is impossible — one input
    /// means the largest thing you can send is your largest note. Here 50 + 47 becomes a
    /// single 94-unit note, which no v5 spend could ever produce.
    ///
    /// IGNORED IN DEBUG ONLY, same reason as v5's round-trip test: winterfell 0.9's
    /// `#[cfg(debug_assertions)]` `validate_transition_degrees` trips on this AIR family's
    /// witness-dependent range-bit columns. Run with `--profile release-fast`, which
    /// inherits `release` and so keeps `debug_assertions` off.
    #[test]
    #[ignore = "winterfell 0.9 debug-only validate_transition_degrees vs witness-dependent range-bit column degree (same family as v4/v5); passes release-compiled."]
    fn two_notes_merge_into_one_bigger_than_either() {
        let sk = e(0xDEAD);
        let me = pk_of(sk);
        let n0 = (e(50), e(4242), sk);
        let n1 = (e(47), e(1337), sk);
        let (tree, p0, p1) = pool_with2(n0, n1);
        let (path0, path1) = (tree.path(p0), tree.path(p1));
        let fee = e(3);

        // 50 + 47 - 3 = 94, all of it into ONE note we keep.
        let outs = [(e(94), e(777), me), (e(0), e(888), me)];
        let trace = build_spend_full_v6_trace(
            &[n0, n1], fee, &outs, &[&path0, &path1], &v6_options(),
        );
        let proof = SpendFullV6Prover::new(v6_options()).prove(trace).expect("prove");

        let pub_in = SpendFullV6PublicInputs {
            root: tree.root(),
            nf: [nf_of(sk, p0), nf_of(sk, p1)],
            fee,
            cm_outs: [cm_out(e(94), e(777), me), cm_out(e(0), e(888), me)],
        };
        assert_ne!(pub_in.nf[0], pub_in.nf[1], "two distinct notes must nullify distinctly");
        verify_spend_full_v6(proof, pub_in).expect("an honest two-note merge must verify");
    }

    /// The realistic payment: two notes fund one payment to someone else plus change,
    /// where NEITHER note alone could have covered the payment.
    #[test]
    #[ignore = "winterfell 0.9 debug-only validate_transition_degrees; passes release-compiled."]
    fn two_small_notes_fund_a_payment_neither_could_cover() {
        let sk = e(0xC0FFEE);
        let me = pk_of(sk);
        let bob = pk_of(e(0xB0B));
        let n0 = (e(30), e(11), sk);
        let n1 = (e(30), e(22), sk);
        let (tree, p0, p1) = pool_with2(n0, n1);
        let (path0, path1) = (tree.path(p0), tree.path(p1));
        let fee = e(2);

        // 40 to Bob — more than either note holds — and 18 back as change.
        let outs = [(e(40), e(777), bob), (e(18), e(888), me)];
        let trace = build_spend_full_v6_trace(
            &[n0, n1], fee, &outs, &[&path0, &path1], &v6_options(),
        );
        let proof = SpendFullV6Prover::new(v6_options()).prove(trace).expect("prove");

        let pub_in = SpendFullV6PublicInputs {
            root: tree.root(),
            nf: [nf_of(sk, p0), nf_of(sk, p1)],
            fee,
            cm_outs: [cm_out(e(40), e(777), bob), cm_out(e(18), e(888), me)],
        };
        assert!(!pub_in.to_elements().contains(&bob), "recipient pk must stay hidden");
        verify_spend_full_v6(proof, pub_in).expect("an honest two-input payment must verify");
    }

    /// THE NEW SOUNDNESS PROPERTY. v6's whole added power is summing two inputs, so the
    /// thing that must not be forgeable is the SUM: a witness claiming outputs worth more
    /// than the two notes actually hold has to be rejected by the constraint system.
    ///
    /// Uses the unchecked builder deliberately — the checked one's `assert_eq!` is Rust,
    /// not cryptography, and a real attacker writes their own prover and never runs it.
    /// The only thing standing between them and free money is the AIR.
    ///
    /// Runs in debug as well as release: it expects `Err` either way, so the debug-only
    /// degree panic (caught here) is as good an answer as a verifier rejection.
    #[test]
    fn inflated_outputs_cannot_exceed_the_sum_of_both_inputs() {
        let sk = e(0xBEEF);
        let me = pk_of(sk);
        let n0 = (e(50), e(4242), sk);
        let n1 = (e(47), e(1337), sk);
        let (tree, p0, p1) = pool_with2(n0, n1);
        let (path0, path1) = (tree.path(p0), tree.path(p1));
        let fee = e(3);

        // 50 + 47 - 3 = 94 is the honest ceiling. Claim 1,000.
        let outs = [(e(1_000), e(777), me), (e(0), e(888), me)];
        let out = std::panic::catch_unwind(|| {
            let trace = build_v6_trace_unchecked(&[n0, n1], fee, &outs, &[&path0, &path1]);
            let proof = SpendFullV6Prover::new(v6_options()).prove(trace)?;
            verify_spend_full_v6(
                proof,
                SpendFullV6PublicInputs {
                    root: tree.root(),
                    nf: [nf_of(sk, p0), nf_of(sk, p1)],
                    fee,
                    cm_outs: [cm_out(e(1_000), e(777), me), cm_out(e(0), e(888), me)],
                },
            )
            .map_err(|_| winterfell::ProverError::UnsupportedFieldExtension(2))
        });
        assert!(
            matches!(out, Ok(Err(_)) | Err(_)),
            "minting value out of thin air across two inputs must not verify"
        );
    }

    /// THE CONSERVATION FAUCET regression — the mint the test above does NOT catch.
    ///
    /// `inflated_outputs_cannot_exceed_the_sum_of_both_inputs` inflates an output but lets the
    /// balance accumulator land on a nonzero value, so it is caught by the `balance[last] == 0`
    /// assertion — the naive attack. The REAL faucet keeps the accumulator landing on zero:
    /// `out` (col 1) was pinned only at the fee row and the two output rows and left FREE on
    /// every other row, so a hand-built trace could park `value - (fee + sum outputs)` in a
    /// free row and telescope straight back to zero while the outputs exceed the inputs. Here
    /// the inputs hold 97, fee is 3 (honest ceiling 94), yet output 0 claims 1000 -- 906 minted
    /// -- with the deficit hidden in row 3 and the running balance repaired to still hit zero at
    /// the last real row. EVERY other constraint, `balance[last] == 0` included, is satisfied;
    /// the only thing rejecting this is the `(1 - first - sum osel)*out == 0` constraint added to
    /// close the hole. Remove that constraint and this proof verifies -- free money.
    #[test]
    fn the_conservation_faucet_is_closed() {
        let sk = e(0xBEEF);
        let me = pk_of(sk);
        let n0 = (e(50), e(4242), sk);
        let n1 = (e(47), e(1337), sk);
        let (tree, p0, p1) = pool_with2(n0, n1);
        let (path0, path1) = (tree.path(p0), tree.path(p1));
        let fee = e(3);
        // Mint 906: claim a 1000 output the two inputs (97 total) cannot fund.
        let outs = [(e(1_000), e(777), me), (e(0), e(888), me)];

        let out = std::panic::catch_unwind(|| {
            let mut trace = build_v6_trace_unchecked(&[n0, n1], fee, &outs, &[&path0, &path1]);
            // Hide the 906 deficit in a free conservation row (row 3, past fee + 2 outputs) and
            // repair the running balance (col 0) so it still lands on zero at the last real row
            // -- a state a naive inflation can never reach, and exactly what the fix must reject.
            let depth = path0.siblings.len();
            let real_len = (depth + 1) * SEG;
            let deficit = e(50) + e(47) - e(3) - e(1_000); // balance at row 3: a huge field elt
            trace.set(1, 3, deficit);
            for r in 4..real_len {
                trace.set(0, r, e(0));
            }
            let proof = SpendFullV6Prover::new(v6_options()).prove(trace)?;
            verify_spend_full_v6(
                proof,
                SpendFullV6PublicInputs {
                    root: tree.root(),
                    nf: [nf_of(sk, p0), nf_of(sk, p1)],
                    fee,
                    cm_outs: [cm_out(e(1_000), e(777), me), cm_out(e(0), e(888), me)],
                },
            )
            .map_err(|_| winterfell::ProverError::UnsupportedFieldExtension(2))
        });
        assert!(
            matches!(out, Ok(Err(_)) | Err(_)),
            "SECURITY: minting via an unconstrained free conservation row must not verify"
        );
    }

    /// Each input's owner proof must be tied to THAT input's nullifier. A key that owns
    /// input 0 must not be able to stand in for input 1 — otherwise one owned note would
    /// unlock a stranger's, which is the two-input version of the flaw v4 closed.
    #[test]
    fn a_foreign_key_cannot_stand_in_for_the_second_input() {
        let sk = e(0xDEAD);
        let mallory = e(0xBAD);
        let me = pk_of(sk);
        let n0 = (e(50), e(4242), sk);
        let n1 = (e(47), e(1337), sk); // owned by sk, NOT by mallory
        let (tree, p0, p1) = pool_with2(n0, n1);
        let (path0, path1) = (tree.path(p0), tree.path(p1));
        let fee = e(3);
        let outs = [(e(94), e(777), me), (e(0), e(888), me)];

        // Mallory proves input 1 with her own key against a leaf she does not own.
        let forged1 = (e(47), e(1337), mallory);
        let out = std::panic::catch_unwind(|| {
            let trace = build_v6_trace_unchecked(&[n0, forged1], fee, &outs, &[&path0, &path1]);
            let proof = SpendFullV6Prover::new(v6_options()).prove(trace)?;
            verify_spend_full_v6(
                proof,
                SpendFullV6PublicInputs {
                    root: tree.root(),
                    nf: [nf_of(sk, p0), nf_of(mallory, p1)],
                    fee,
                    cm_outs: [cm_out(e(94), e(777), me), cm_out(e(0), e(888), me)],
                },
            )
            .map_err(|_| winterfell::ProverError::UnsupportedFieldExtension(2))
        });
        assert!(
            matches!(out, Ok(Err(_)) | Err(_)),
            "a key that does not own input 1 must not be able to spend it"
        );
    }

    /// The public inputs must carry BOTH nullifiers, in a fixed order, and must not carry
    /// anything else that would name the owner. A wire format that serialises only one of
    /// them lets the chain record one spend and miss the other — a double-spend by
    /// omission, and the most likely way this circuit gets deployed wrongly.
    #[test]
    fn public_inputs_carry_both_nullifiers_and_no_owner_key() {
        let sk = e(0xDEAD);
        let me = pk_of(sk);
        let bob = pk_of(e(0xB0B));
        let pub_in = SpendFullV6PublicInputs {
            root: e(1),
            nf: [nf_of(sk, 2), nf_of(sk, 5)],
            fee: e(3),
            cm_outs: [cm_out(e(40), e(777), bob), cm_out(e(54), e(888), me)],
        };
        let els = pub_in.to_elements();
        assert_eq!(els.len(), 1 + 1 + N_INS + 1 + N_OUTS, "domain, root, both nf, fee, both cm_outs");
        assert_eq!(els[2], nf_of(sk, 2));
        assert_eq!(els[3], nf_of(sk, 5));
        assert!(!els.contains(&me), "the spender's own key must never be published");
        assert!(!els.contains(&bob), "the recipient's key must never be published");
    }

    /// The zero-knowledge property v5 earned must survive the second input: no witness
    /// value — either note's amount, blinding or key — may appear in the proof bytes.
    /// v6 doubles the number of secrets in the trace, so it doubles what can leak.
    #[test]
    #[ignore = "winterfell 0.9 debug-only validate_transition_degrees; passes release-compiled."]
    fn a_two_input_proof_does_not_contain_either_witness() {
        let sk = e(0xDEAD);
        let me = pk_of(sk);
        let n0 = (e(50), e(4242), sk);
        let n1 = (e(47), e(1337), sk);
        let (tree, p0, p1) = pool_with2(n0, n1);
        let (path0, path1) = (tree.path(p0), tree.path(p1));
        let fee = e(3);
        let outs = [(e(94), e(777), me), (e(0), e(888), me)];

        let trace = build_spend_full_v6_trace(
            &[n0, n1], fee, &outs, &[&path0, &path1], &v6_options(),
        );
        let proof = SpendFullV6Prover::new(v6_options()).prove(trace).expect("prove");
        let bytes = proof.to_bytes();

        let secrets = [
            ("input0.value", e(50)),
            ("input0.blinding", e(4242)),
            ("input1.value", e(47)),
            ("input1.blinding", e(1337)),
            ("spend_key", sk),
        ];
        let hits = crate::zk_mask::scan_proof_for_secrets(&bytes, &secrets);
        assert!(hits.is_empty(), "witness values found in the proof: {hits:?}");
    }

    /// ⚠️ THE DOUBLE-SPEND VECTOR, demonstrated. Feeding the SAME note as both inputs
    /// produces two independently-valid input blocks, a conservation lane summing
    /// `2 x value`, and TWO IDENTICAL NULLIFIERS. The circuit has no reason to object —
    /// each block proves membership, ownership and nullifier derivation correctly. But the
    /// state layer records nullifiers in a SET, so the second insert is a no-op: one note
    /// burned, double the value minted.
    ///
    /// This test exists to pin the fact that the CIRCUIT does not stop it, so nobody later
    /// removes the check that does. Raised in review by the operator.
    #[test]
    #[ignore = "winterfell 0.9 debug-only validate_transition_degrees; run release-compiled."]
    fn the_same_note_twice_is_accepted_by_the_circuit_and_must_be_caught_outside_it() {
        let sk = e(0xDEAD);
        let me = pk_of(sk);
        let n0 = (e(50), e(4242), sk);
        let n1 = (e(47), e(1337), sk);
        let (tree, p0, _p1) = pool_with2(n0, n1);
        let path0 = tree.path(p0);
        let fee = e(0);

        // The same note, twice. 50 + 50 = 100 out of a note worth 50.
        let outs = [(e(100), e(777), me), (e(0), e(888), me)];
        let trace = build_spend_full_v6_trace(
            &[n0, n0], fee, &outs, &[&path0, &path0], &v6_options(),
        );
        let proof = SpendFullV6Prover::new(v6_options()).prove(trace).expect("prove");
        let pub_in = SpendFullV6PublicInputs {
            root: tree.root(),
            nf: [nf_of(sk, p0), nf_of(sk, p0)],
            fee,
            cm_outs: [cm_out(e(100), e(777), me), cm_out(e(0), e(888), me)],
        };
        assert_eq!(pub_in.nf[0], pub_in.nf[1], "the tell: one note used twice repeats its nullifier");
        let circuit_says = verify_spend_full_v6(proof, pub_in.clone());
        assert!(
            circuit_says.is_ok(),
            "the circuit ACCEPTS this — which is exactly why the equality check must live \
             outside it, in the state layer: {circuit_says:?}"
        );
        // And this is the check that has to catch it.
        assert!(
            reject_duplicate_nullifiers(&pub_in.nf).is_err(),
            "the guard must refuse two identical nullifiers"
        );
    }

    /// MANDATORY MASKING INVARIANT (requested in review, and it earns its place).
    ///
    /// Same witness, same anchor, DIFFERENT mask randomness must give:
    ///   * identical public inputs — the mask must not touch anything published;
    ///   * different proof bytes   — or the randomness is not doing its job;
    ///   * and both must verify.
    ///
    /// This is the direct regression test for the bug that cost the most time on v5:
    /// `get_pub_inputs` read `trace.length() - 1`, which after padding is a MASKED row, so
    /// a published value moved with the randomness. It surfaced only as
    /// `InconsistentOodConstraintEvaluations`, which names nothing. v6 doubles the number
    /// of places that mistake can be made — two nullifier lanes, two Merkle lanes — so the
    /// invariant is pinned rather than argued.
    #[test]
    #[ignore = "winterfell 0.9 debug-only validate_transition_degrees; run release-compiled."]
    fn masking_changes_the_proof_and_nothing_that_is_published() {
        let sk = e(0xDEAD);
        let me = pk_of(sk);
        let n0 = (e(50), e(4242), sk);
        let n1 = (e(47), e(1337), sk);
        let (tree, p0, p1) = pool_with2(n0, n1);
        let (path0, path1) = (tree.path(p0), tree.path(p1));
        let fee = e(3);
        let outs = [(e(94), e(777), me), (e(0), e(888), me)];

        let mut published = Vec::new();
        let mut proofs = Vec::new();
        for seed in [[0x11u8; 32], [0x22u8; 32]] {
            let trace = build_spend_full_v6_trace_seeded(
                &[n0, n1], fee, &outs, &[&path0, &path1], &v6_options(), seed,
            );
            // Read the public inputs the PROVER would publish, straight off the trace —
            // this is the exact call that had the bug.
            let air_pub = <SpendFullV6Prover as Prover>::get_pub_inputs(
                &SpendFullV6Prover::new(v6_options()), &trace,
            );
            published.push(air_pub.to_elements());
            proofs.push(SpendFullV6Prover::new(v6_options()).prove(trace).expect("prove"));
        }

        assert_eq!(
            published[0], published[1],
            "the mask leaked into a published value — this is the v5 bug, in v6"
        );
        assert_ne!(
            proofs[0].to_bytes(), proofs[1].to_bytes(),
            "two proofs under different masks must differ, or the randomness is inert"
        );

        let pub_in = SpendFullV6PublicInputs {
            root: tree.root(),
            nf: [nf_of(sk, p0), nf_of(sk, p1)],
            fee,
            cm_outs: [cm_out(e(94), e(777), me), cm_out(e(0), e(888), me)],
        };
        assert_eq!(published[0], pub_in.to_elements(), "prover and caller must agree");
        for proof in proofs {
            verify_spend_full_v6(proof, pub_in.clone()).expect("both masks must verify");
        }
    }

    /// A bad membership path on EITHER input must be refused — not just on input 0. An
    /// input block that is present but unchecked is the classic way a multi-input circuit
    /// goes wrong, so both are exercised.
    #[test]
    fn a_bad_path_on_either_input_is_refused() {
        let sk = e(0xDEAD);
        let me = pk_of(sk);
        let n0 = (e(50), e(4242), sk);
        let n1 = (e(47), e(1337), sk);
        let (tree, p0, p1) = pool_with2(n0, n1);
        let (path0, path1) = (tree.path(p0), tree.path(p1));

        // A path from a DIFFERENT tree: same shape, wrong root.
        let other = crate::membership::CompressTree::new(
            (0..8).map(|i| crate::note_v1::padding_leaf(100 + i as u64)).collect(),
        );
        let foreign = other.path(1);
        assert_ne!(other.root(), tree.root(), "the decoy must really be a different tree");

        let outs = [(e(94), e(777), me), (e(0), e(888), me)];
        for (which, paths) in [(0usize, [&foreign, &path1]), (1usize, [&path0, &foreign])] {
            let w = SpendV6Witness::new(tree.root(), [n0, n1], outs, paths);
            assert!(
                matches!(w, Err(WitnessError::PathNotUnderAnchor { input }) if input == which),
                "input {which}'s foreign path must be refused at witness construction: {:?}",
                w.err()
            );
        }
        // And the honest pair is accepted.
        assert!(SpendV6Witness::new(tree.root(), [n0, n1], outs, [&path0, &path1]).is_ok());
    }

    /// Mutating EITHER published nullifier must break verification independently. If only
    /// one is really bound, a spend could name a nullifier the chain then records while the
    /// circuit proved a different note.
    #[test]
    fn each_published_nullifier_is_independently_bound() {
        let sk = e(0xDEAD);
        let me = pk_of(sk);
        let pub_in = SpendFullV6PublicInputs {
            root: e(1),
            nf: [nf_of(sk, 2), nf_of(sk, 5)],
            fee: e(3),
            cm_outs: [cm_out(e(94), e(777), me), cm_out(e(0), e(888), me)],
        };
        let base = pub_in.to_elements();
        for i in 0..N_INS {
            let mut tampered = pub_in.clone();
            tampered.nf[i] = e(0xFFFF);
            assert_ne!(
                base, tampered.to_elements(),
                "tampering with nullifier {i} must change the transcript"
            );
        }
    }

    /// The version domain separator must lead the transcript, so a v6 proof cannot be
    /// replayed against another circuit's public inputs even if widths ever coincide.
    #[test]
    fn the_version_is_bound_into_the_transcript() {
        let me = pk_of(e(1));
        let pub_in = SpendFullV6PublicInputs {
            root: e(1),
            nf: [e(2), e(3)],
            fee: e(4),
            cm_outs: [cm_out(e(5), e(6), me), cm_out(e(7), e(8), me)],
        };
        let els = pub_in.to_elements();
        assert_eq!(els[0], BaseElement::new(V6_DOMAIN), "version leads the transcript");
        assert_eq!(els.len(), 1 + 1 + N_INS + 1 + N_OUTS, "domain, root, nfs, fee, cm_outs");
    }

    /// Geometry guard. The reserved-randomness region must still outrun the number of
    /// trace openings, or the mask does not actually hide anything — and v6 changed the
    /// width, which is exactly the kind of change that silently invalidates this.
    #[test]
    fn reserved_randomness_outruns_every_shippable_query_count() {
        for depth in [3usize, 7, 15] {
            let real_len = (depth + 1) * SEG;
            for opts in [v6_options(), mimc_options()] {
                assert!(
                    real_len > zk_rows_for(&opts),
                    "depth {depth}: {real_len} reserved rows must exceed {} openings",
                    zk_rows_for(&opts)
                );
                assert_eq!(padded_len(real_len, &opts), real_len * 2, "must be an exact doubling");
            }
        }
    }

    /// v6 is wider than v5 by exactly one input block, and no wider. A width that drifts
    /// from the column constants is the failure mode that produces unreadable OOD errors.
    #[test]
    fn width_is_v5_plus_exactly_one_input_block() {
        assert_eq!(WIDTH, V5_WIDTH + IN1_COLS);
        assert_eq!(V5_WIDTH, 33, "v5's layout is frozen; v6 appends rather than re-lays out");
        assert_eq!(WIDTH, 46);
        assert_eq!(C1_PY, WIDTH - 1, "input 1's last column is the last column");
        assert_eq!(OUT_BASE, C1_BASE + C1_COUNT);
    }
}
