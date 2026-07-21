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
}
