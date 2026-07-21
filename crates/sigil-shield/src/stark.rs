//! Transparent zk-STARK validity engine for SIGIL private transfers — the ceremony-free
//! path (no trusted setup, so mainnet needs no multi-party ceremony to launch).
//!
//! Why this module exists: Quillon's `q-crypto-advanced::circle_stark` DECLARED a
//! winterfell dependency ("uses winterfell as base") but its `prove` never once called
//! `winterfell::prove` — a wrapper around a library it never invoked. That is why private
//! transactions never worked there. THIS module actually calls `winterfell::prove` and
//! `winterfell::verify`, and the tests prove a real proof round-trips AND that a tampered
//! proof / wrong public input are rejected — the three non-negotiables from
//! `docs/SIGIL_PRIVACY_ARCHITECTURE_v0.md`.
//!
//! P1 SPIKE: the constraint here is the canonical winterfell computation (x → x³ + 42 for
//! N steps) — enough to prove the STARK plumbing end-to-end today. The shielded-transfer
//! AIR (value conservation + Merkle membership + nullifier) replaces the transition
//! function; the prover/verifier wiring below is exactly what it will use.

use winterfell::{
    crypto::{hashers::Blake3_256, DefaultRandomCoin},
    math::{fields::f128::BaseElement, FieldElement, ToElements},
    matrix::ColMatrix,
    Air, AirContext, Assertion, AuxRandElements, ConstraintCompositionCoefficients,
    DefaultConstraintEvaluator, DefaultTraceLde, EvaluationFrame, FieldExtension, ProofOptions,
    Prover, StarkDomain, Trace, TraceInfo, TracePolyTable, TraceTable,
    TransitionConstraintDegree,
};

/// Public inputs bound into the proof: the starting value and the claimed result.
#[derive(Clone)]
pub struct PublicInputs {
    pub start: BaseElement,
    pub result: BaseElement,
}

impl ToElements<BaseElement> for PublicInputs {
    fn to_elements(&self) -> Vec<BaseElement> {
        vec![self.start, self.result]
    }
}

/// The AIR (arithmetic intermediate representation) of the P1 spike computation.
pub struct WorkAir {
    context: AirContext<BaseElement>,
    start: BaseElement,
    result: BaseElement,
}

impl Air for WorkAir {
    type BaseField = BaseElement;
    type PublicInputs = PublicInputs;
    type GkrProof = ();
    type GkrVerifier = ();

    fn new(trace_info: TraceInfo, pub_inputs: PublicInputs, options: ProofOptions) -> Self {
        assert_eq!(1, trace_info.width());
        let degrees = vec![TransitionConstraintDegree::new(3)];
        WorkAir {
            context: AirContext::new(trace_info, degrees, 2, options),
            start: pub_inputs.start,
            result: pub_inputs.result,
        }
    }

    fn evaluate_transition<E: FieldElement + From<Self::BaseField>>(
        &self,
        frame: &EvaluationFrame<E>,
        _periodic_values: &[E],
        result: &mut [E],
    ) {
        let current = &frame.current()[0];
        let expected_next = current.exp(3u32.into()) + E::from(42u32);
        result[0] = frame.next()[0] - expected_next;
    }

    fn get_assertions(&self) -> Vec<Assertion<Self::BaseField>> {
        let last = self.trace_length() - 1;
        vec![
            Assertion::single(0, 0, self.start),
            Assertion::single(0, last, self.result),
        ]
    }

    fn context(&self) -> &AirContext<Self::BaseField> {
        &self.context
    }
}

/// The prover — holds STARK protocol parameters and drives `winterfell::prove`.
pub struct WorkProver {
    options: ProofOptions,
}

impl WorkProver {
    pub fn new(options: ProofOptions) -> Self {
        Self { options }
    }
}

impl Prover for WorkProver {
    type BaseField = BaseElement;
    type Air = WorkAir;
    type Trace = TraceTable<Self::BaseField>;
    type HashFn = Blake3_256<Self::BaseField>;
    type RandomCoin = DefaultRandomCoin<Self::HashFn>;
    type TraceLde<E: FieldElement<BaseField = Self::BaseField>> = DefaultTraceLde<E, Self::HashFn>;
    type ConstraintEvaluator<'a, E: FieldElement<BaseField = Self::BaseField>> =
        DefaultConstraintEvaluator<'a, Self::Air, E>;

    fn get_pub_inputs(&self, trace: &Self::Trace) -> PublicInputs {
        let last = trace.length() - 1;
        PublicInputs { start: trace.get(0, 0), result: trace.get(0, last) }
    }

    fn options(&self) -> &ProofOptions {
        &self.options
    }

    fn new_trace_lde<E: FieldElement<BaseField = Self::BaseField>>(
        &self,
        trace_info: &TraceInfo,
        main_trace: &ColMatrix<Self::BaseField>,
        domain: &StarkDomain<Self::BaseField>,
    ) -> (Self::TraceLde<E>, TracePolyTable<E>) {
        DefaultTraceLde::new(trace_info, main_trace, domain)
    }

    fn new_evaluator<'a, E: FieldElement<BaseField = Self::BaseField>>(
        &self,
        air: &'a Self::Air,
        aux_rand_elements: Option<AuxRandElements<E>>,
        composition_coefficients: ConstraintCompositionCoefficients<E>,
    ) -> Self::ConstraintEvaluator<'a, E> {
        DefaultConstraintEvaluator::new(air, aux_rand_elements, composition_coefficients)
    }
}

/// Build the execution trace of the spike computation for `n` steps.
pub fn build_do_work_trace(start: BaseElement, n: usize) -> TraceTable<BaseElement> {
    let mut trace = TraceTable::new(1, n);
    trace.fill(
        |state| state[0] = start,
        |_, state| state[0] = state[0].exp(3u32.into()) + BaseElement::new(42),
    );
    trace
}

/// ~96-bit conjectured security, transparent (no trusted setup).
pub fn spike_options() -> ProofOptions {
    ProofOptions::new(32, 8, 0, FieldExtension::None, 8, 31)
}

type Hasher = Blake3_256<BaseElement>;
type Coin = DefaultRandomCoin<Hasher>;

/// Verify a proof against public inputs. Ok(()) only if winterfell accepts.
pub fn verify_spike(
    proof: winterfell::Proof,
    pub_inputs: PublicInputs,
) -> Result<(), winterfell::VerifierError> {
    let min = winterfell::AcceptableOptions::MinConjecturedSecurity(95);
    winterfell::verify::<WorkAir, Hasher, Coin>(proof, pub_inputs, &min)
}

// ══════════════════════════════════════════════════════════════════════════════════
//  SHIELDED-TRANSFER AIR — value conservation with a public fee binding.
//
//  Statement proved (amounts hidden): "there exist input funds `total_in` and a schedule
//  of subtractions `out[0..]` (private) such that starting from `total_in` and subtracting
//  each `out[i]` per step, the balance reaches EXACTLY zero, and out[0] equals the public
//  `fee`." Trace: col0 = running balance, col1 = amount subtracted this step. Transition
//  ties balance_next = balance_cur − out_cur; boundary forces balance = 0 at the end
//  (Σ out = total_in ⇒ inputs = outputs + fee), and out[0] = fee is asserted against the
//  public input. This is the money-conservation core — REAL winterfell constraints, not a
//  wrapper — and it round-trips + rejects tampering below.
//
//  ⚠️ SCOPE (honest, per the audit discipline): this AIR proves the transfer ARITHMETIC.
//  Two increments make it fully unforgeable, each needing a STARK-friendly hash gadget
//  (Rescue/Poseidon) in the trace — the next lanes:
//    • RANGE CHECK (bit-decompose each amount < 2^64) — closes field-wrap "negative amount".
//    • MEMBERSHIP + NULLIFIER — bind inputs to real notes in the committed tree, derive nf.
//  Until those land, do NOT treat a conservation proof alone as a spendable-note proof.
// ══════════════════════════════════════════════════════════════════════════════════

/// Public inputs for a shielded transfer: the transparent fee (bound into the trace).
#[derive(Clone)]
pub struct TransferPublicInputs {
    pub fee: BaseElement,
}

impl ToElements<BaseElement> for TransferPublicInputs {
    fn to_elements(&self) -> Vec<BaseElement> {
        vec![self.fee]
    }
}

pub struct ShieldTransferAir {
    context: AirContext<BaseElement>,
    fee: BaseElement,
}

impl Air for ShieldTransferAir {
    type BaseField = BaseElement;
    type PublicInputs = TransferPublicInputs;
    type GkrProof = ();
    type GkrVerifier = ();

    fn new(trace_info: TraceInfo, pub_inputs: TransferPublicInputs, options: ProofOptions) -> Self {
        assert_eq!(2, trace_info.width());
        // balance_next − (balance_cur − out_cur) = 0 → linear, degree 1.
        let degrees = vec![TransitionConstraintDegree::new(1)];
        ShieldTransferAir {
            context: AirContext::new(trace_info, degrees, 2, options),
            fee: pub_inputs.fee,
        }
    }

    fn evaluate_transition<E: FieldElement + From<Self::BaseField>>(
        &self,
        frame: &EvaluationFrame<E>,
        _periodic_values: &[E],
        result: &mut [E],
    ) {
        let balance = frame.current()[0];
        let out = frame.current()[1];
        // running balance decreases by `out` each step
        result[0] = frame.next()[0] - (balance - out);
    }

    fn get_assertions(&self) -> Vec<Assertion<Self::BaseField>> {
        let last = self.trace_length() - 1;
        vec![
            Assertion::single(0, last, BaseElement::ZERO),
            Assertion::single(1, 0, self.fee),
        ]
    }

    fn context(&self) -> &AirContext<Self::BaseField> {
        &self.context
    }
}

pub struct ShieldTransferProver {
    options: ProofOptions,
}
impl ShieldTransferProver {
    pub fn new(options: ProofOptions) -> Self { Self { options } }
}

impl Prover for ShieldTransferProver {
    type BaseField = BaseElement;
    type Air = ShieldTransferAir;
    type Trace = TraceTable<Self::BaseField>;
    type HashFn = Blake3_256<Self::BaseField>;
    type RandomCoin = DefaultRandomCoin<Self::HashFn>;
    type TraceLde<E: FieldElement<BaseField = Self::BaseField>> = DefaultTraceLde<E, Self::HashFn>;
    type ConstraintEvaluator<'a, E: FieldElement<BaseField = Self::BaseField>> =
        DefaultConstraintEvaluator<'a, Self::Air, E>;

    fn get_pub_inputs(&self, trace: &Self::Trace) -> TransferPublicInputs {
        // fee is the first subtraction (column 1, row 0)
        TransferPublicInputs { fee: trace.get(1, 0) }
    }
    fn options(&self) -> &ProofOptions { &self.options }
    fn new_trace_lde<E: FieldElement<BaseField = Self::BaseField>>(
        &self, trace_info: &TraceInfo, main_trace: &ColMatrix<Self::BaseField>, domain: &StarkDomain<Self::BaseField>,
    ) -> (Self::TraceLde<E>, TracePolyTable<E>) {
        DefaultTraceLde::new(trace_info, main_trace, domain)
    }
    fn new_evaluator<'a, E: FieldElement<BaseField = Self::BaseField>>(
        &self, air: &'a Self::Air, aux: Option<AuxRandElements<E>>, cc: ConstraintCompositionCoefficients<E>,
    ) -> Self::ConstraintEvaluator<'a, E> {
        DefaultConstraintEvaluator::new(air, aux, cc)
    }
}

/// Build a transfer trace. `total_in` = summed input notes (private); `outs` = per-step
/// subtractions where `outs[0]` MUST be the fee, `outs[1..]` the output amounts, zero-padded
/// to a power-of-two length ≥ 8. The final balance is forced to zero by the AIR assertion —
/// callers must pass a conserving schedule (Σ outs == total_in) or proving fails.
pub fn build_transfer_trace(total_in: BaseElement, outs: &[BaseElement]) -> TraceTable<BaseElement> {
    let n = outs.len();
    assert!(n >= 8 && n.is_power_of_two(), "trace length must be a power of two ≥ 8");
    let outs = outs.to_vec();
    let mut trace = TraceTable::new(2, n);
    trace.fill(
        |state| {
            state[0] = total_in;    // row 0 balance
            state[1] = outs[0];     // row 0 subtraction (the fee)
        },
        |step, state| {
            // advance to row step+1: new balance = balance − out(step); next subtraction
            state[0] = state[0] - outs[step];
            state[1] = if step + 1 < n { outs[step + 1] } else { BaseElement::ZERO };
        },
    );
    trace
}

type TransferCoin = DefaultRandomCoin<Blake3_256<BaseElement>>;

/// Verify a shielded-transfer proof against the public fee.
pub fn verify_transfer(
    proof: winterfell::Proof,
    pub_inputs: TransferPublicInputs,
) -> Result<(), winterfell::VerifierError> {
    let min = winterfell::AcceptableOptions::MinConjecturedSecurity(95);
    winterfell::verify::<ShieldTransferAir, Blake3_256<BaseElement>, TransferCoin>(proof, pub_inputs, &min)
}

#[cfg(test)]
mod tests {
    use super::*;
    use winterfell::math::StarkField;

    /// THE STARK ACCEPTANCE GATE. A REAL winterfell proof (no wrapper, no zero-fill):
    ///  (1) prove→verify == Ok;
    ///  (2) a WRONG public input is rejected;
    ///  (3) a TAMPERED proof (byte-flipped) is rejected.
    /// Transparent throughout — no trusted setup anywhere in this path.
    #[test]
    fn real_stark_round_trips_and_rejects_tampering() {
        let start = BaseElement::new(3);
        let n = 128;
        let trace = build_do_work_trace(start, n);
        let result = trace.get(0, n - 1);

        let prover = WorkProver::new(spike_options());
        let proof = prover.prove(trace).expect("winterfell::prove");

        // (1) honest proof verifies
        verify_spike(proof.clone(), PublicInputs { start, result })
            .expect("a valid STARK must verify");

        // (2) wrong public input rejected
        assert!(
            verify_spike(proof.clone(), PublicInputs { start, result: result + BaseElement::ONE }).is_err(),
            "SECURITY: a proof must NOT verify against a result it did not prove"
        );

        // (3) tampered proof bytes rejected (corrupt → either from_bytes fails or verify fails)
        let mut bytes = proof.to_bytes();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xFF;
        let rejected = match winterfell::Proof::from_bytes(&bytes) {
            Ok(p) => verify_spike(p, PublicInputs { start, result }).is_err(),
            Err(_) => true,
        };
        assert!(rejected, "SECURITY: a tampered STARK proof must NOT verify");
    }

    /// THE SHIELDED-TRANSFER GATE: a REAL winterfell proof of value conservation with a
    /// public fee binding. (1) an honest conserving transfer verifies; (2) a WRONG fee is
    /// rejected; (3) a NON-conserving schedule fails to prove; (4) a tampered proof is rejected.
    #[test]
    fn shielded_transfer_conserves_and_rejects_tampering() {
        let e = BaseElement::new;
        // inputs total 100; fee 3; outputs 50 + 47; padded to length 8; Σ outs = 100.
        let total_in = e(100);
        let mut outs = vec![e(0); 128]; outs[0]=e(3); outs[1]=e(50); outs[2]=e(47); // fee+outputs, rest padding
        let trace = build_transfer_trace(total_in, &outs);
        let prover = ShieldTransferProver::new(spike_options());
        let proof = prover.prove(trace).expect("prove conserving transfer");

        // (1) honest proof verifies against the true fee
        verify_transfer(proof.clone(), TransferPublicInputs { fee: e(3) })
            .expect("conserving transfer with correct fee must verify");

        // (2) wrong public fee rejected
        assert!(verify_transfer(proof.clone(), TransferPublicInputs { fee: e(4) }).is_err(),
            "SECURITY: a transfer must not verify against a fee it did not commit");

        // (3) a NON-conserving schedule cannot yield a valid proof. In debug builds
        // winterfell's prove() VALIDATES the trace and panics on a bad one; in release it
        // produces an invalid proof the verifier rejects. Either way, proving must not succeed.
        let bad_out = std::panic::catch_unwind(|| {
            let mut bad = vec![e(0); 128]; bad[0]=e(3); bad[1]=e(50); bad[2]=e(48); // Σ=101≠100
            let bad_trace = build_transfer_trace(total_in, &bad);
            ShieldTransferProver::new(spike_options()).prove(bad_trace)
        });
        let non_conserving_ok = match bad_out { Ok(Ok(_)) => false, _ => true };
        assert!(non_conserving_ok, "SECURITY: a non-conserving transfer must not produce a valid proof");

        // (4) tampered proof bytes rejected
        let mut bytes = proof.to_bytes();
        let mid = bytes.len() / 2; bytes[mid] ^= 0xFF;
        let rejected = match winterfell::Proof::from_bytes(&bytes) {
            Ok(p) => verify_transfer(p, TransferPublicInputs { fee: e(3) }).is_err(),
            Err(_) => true,
        };
        assert!(rejected, "SECURITY: a tampered transfer proof must not verify");
    }

}
