//! In-circuit algebraic hash for shielded membership — MiMC (x⁷ S-box, Goldilocks f64).
//!
//! To prove Merkle membership in zero-knowledge, the hash up the path must be recomputed
//! INSIDE the STARK. That needs a hash whose round function is a low-degree polynomial so
//! it can be an AIR transition constraint. We use **MiMC-p/p** with exponent 7:
//!   round r:   state ← (state + c_r)⁷
//! x⁷ is a permutation over Goldilocks (gcd(7, p−1)=1, unlike x³), so this is a real
//! permutation; with enough rounds it is a collision-resistant compression in a sponge.
//!
//! Why MiMC and not Rescue here: Rescue (winterfell's Rp64_256, 12-wide, MDS, dual S-box)
//! is more efficient but far more error-prone to constrain by hand — one wrong constant is
//! silently unsound. MiMC's single-S-box round is a one-line constraint we can get provably
//! correct and TEST against an independent reference. Rescue is a drop-in efficiency upgrade
//! later (same circuit shape, different round function).
//!
//! ⚠️ SECURITY PARAMETER: round count is set for the permutation to be complete over a
//! 64-bit field; production must pin ROUNDS to a published MiMC analysis for the target
//! security level. The CIRCUIT is sound for whatever `mimc_permute` computes (the in==ref
//! test guarantees the AIR matches the reference); ROUNDS governs the HASH's CR strength.

use winterfell::{
    crypto::{hashers::Blake3_256, DefaultRandomCoin},
    math::{fields::f64::BaseElement, FieldElement, StarkField, ToElements},
    matrix::ColMatrix,
    Air, AirContext, Assertion, AuxRandElements, ConstraintCompositionCoefficients,
    DefaultConstraintEvaluator, DefaultTraceLde, EvaluationFrame, FieldExtension, ProofOptions,
    Prover, StarkDomain, Trace, TraceInfo, TracePolyTable, TraceTable, TransitionConstraintDegree,
};

/// 63 rounds → trace length 64 (power of two), 63 transitions. Well past the ~23-round
/// completeness floor for x⁷ over a 64-bit field; see the SECURITY PARAMETER note above.
pub const ROUNDS: usize = 63;
pub const TRACE_LEN: usize = 64;
const SBOX: u64 = 7;

/// Deterministic round constants (identical off-circuit and in the AIR's periodic column,
/// so the two can never diverge). splitmix64 over the round index, reduced mod the field.
pub fn round_constants() -> [BaseElement; TRACE_LEN] {
    let mut out = [BaseElement::ZERO; TRACE_LEN];
    for (i, slot) in out.iter_mut().enumerate() {
        let mut z = (i as u64).wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        *slot = BaseElement::new(z);
    }
    out
}

fn pow7(x: BaseElement) -> BaseElement {
    let x2 = x * x;
    let x4 = x2 * x2;
    x4 * x2 * x // x⁶ · x = x⁷
}

/// Off-circuit reference: the MiMC permutation of `x`. The AIR must reproduce this exactly.
pub fn mimc_permute(x: BaseElement) -> BaseElement {
    let c = round_constants();
    let mut s = x;
    for &ci in c.iter().take(ROUNDS) {
        s = pow7(s + ci);
    }
    s
}

// ── in-circuit AIR: prove output == MiMC(input) ────────────────────────────────────────

#[derive(Clone)]
pub struct MimcPublicInputs {
    pub input: BaseElement,
    pub output: BaseElement,
}
impl ToElements<BaseElement> for MimcPublicInputs {
    fn to_elements(&self) -> Vec<BaseElement> {
        vec![self.input, self.output]
    }
}

pub struct MimcAir {
    context: AirContext<BaseElement>,
    input: BaseElement,
    output: BaseElement,
}

impl Air for MimcAir {
    type BaseField = BaseElement;
    type PublicInputs = MimcPublicInputs;
    type GkrProof = ();
    type GkrVerifier = ();

    fn new(trace_info: TraceInfo, pub_inputs: MimcPublicInputs, options: ProofOptions) -> Self {
        assert_eq!(1, trace_info.width());
        // constraint: next − (cur + c)⁷ = 0 → degree 7.
        let degrees = vec![TransitionConstraintDegree::new(SBOX as usize)];
        MimcAir {
            context: AirContext::new(trace_info, degrees, 2, options),
            input: pub_inputs.input,
            output: pub_inputs.output,
        }
    }

    /// Feed the round constants one-per-step as a periodic column.
    fn get_periodic_column_values(&self) -> Vec<Vec<BaseElement>> {
        vec![round_constants().to_vec()]
    }

    fn evaluate_transition<E: FieldElement + From<Self::BaseField>>(
        &self,
        frame: &EvaluationFrame<E>,
        periodic_values: &[E],
        result: &mut [E],
    ) {
        let c = periodic_values[0];
        let s = frame.current()[0] + c;
        let s2 = s * s;
        let s4 = s2 * s2;
        let sbox = s4 * s2 * s; // (cur + c)⁷
        result[0] = frame.next()[0] - sbox;
    }

    fn get_assertions(&self) -> Vec<Assertion<Self::BaseField>> {
        // input at step 0; output after ROUNDS rounds (row ROUNDS = last real state).
        vec![
            Assertion::single(0, 0, self.input),
            Assertion::single(0, ROUNDS, self.output),
        ]
    }

    fn context(&self) -> &AirContext<Self::BaseField> {
        &self.context
    }
}

pub struct MimcProver {
    options: ProofOptions,
}
impl MimcProver {
    pub fn new(options: ProofOptions) -> Self {
        Self { options }
    }
}

impl Prover for MimcProver {
    type BaseField = BaseElement;
    type Air = MimcAir;
    type Trace = TraceTable<Self::BaseField>;
    type HashFn = Blake3_256<Self::BaseField>;
    type RandomCoin = DefaultRandomCoin<Self::HashFn>;
    type TraceLde<E: FieldElement<BaseField = Self::BaseField>> = DefaultTraceLde<E, Self::HashFn>;
    type ConstraintEvaluator<'a, E: FieldElement<BaseField = Self::BaseField>> =
        DefaultConstraintEvaluator<'a, Self::Air, E>;

    fn get_pub_inputs(&self, trace: &Self::Trace) -> MimcPublicInputs {
        MimcPublicInputs { input: trace.get(0, 0), output: trace.get(0, ROUNDS) }
    }
    fn options(&self) -> &ProofOptions {
        &self.options
    }
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

/// Build the MiMC execution trace for input `x`: row i = state after i rounds.
pub fn build_mimc_trace(x: BaseElement) -> TraceTable<BaseElement> {
    let c = round_constants();
    let mut trace = TraceTable::new(1, TRACE_LEN);
    trace.fill(
        |state| state[0] = x,
        |step, state| {
            // advance to row step+1 by applying round `step` (identity past ROUNDS so the
            // padding rows keep the output fixed and the final assertion stays valid).
            if step < ROUNDS {
                state[0] = pow7(state[0] + c[step]);
            }
        },
    );
    trace
}

/// A short 64-row trace caps FRI security, so we lift it with a quadratic field extension
/// (proving over a ~128-bit extension) + more queries. On the longer path-verification trace
/// this reaches production levels; here it comfortably clears the ACCEPT floor below.
pub fn mimc_options() -> ProofOptions {
    ProofOptions::new(84, 8, 16, FieldExtension::Quadratic, 8, 31)
}

/// Verifier accept threshold. 90-bit conjectured floor — reached with the options above on
/// this trace; production pins both to ≥100 once the full path AIR sets the trace length.
const ACCEPT_BITS: u32 = 90;

type Coin = DefaultRandomCoin<Blake3_256<BaseElement>>;

pub fn verify_mimc(
    proof: winterfell::Proof,
    pub_inputs: MimcPublicInputs,
) -> Result<(), winterfell::VerifierError> {
    let min = winterfell::AcceptableOptions::MinConjecturedSecurity(ACCEPT_BITS);
    winterfell::verify::<MimcAir, Blake3_256<BaseElement>, Coin>(proof, pub_inputs, &min)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference is deterministic + a genuine permutation (injective on samples).
    #[test]
    fn mimc_reference_is_deterministic_and_injective() {
        let a = mimc_permute(BaseElement::new(12345));
        assert_eq!(a, mimc_permute(BaseElement::new(12345)), "deterministic");
        let mut seen = std::collections::HashSet::new();
        for i in 0..2000u64 {
            assert!(seen.insert(mimc_permute(BaseElement::new(i)).as_int()), "collision at {i}");
        }
    }

    /// THE IN-CIRCUIT HASH GATE: the STARK proves output == MiMC(input) for the SAME
    /// reference, and rejects a wrong output and a tampered proof. If the AIR's round
    /// constraint disagreed with `mimc_permute`, proving would fail — so a green gate is
    /// proof the in-circuit hash equals the reference (no silent divergence).
    #[test]
    fn in_circuit_mimc_matches_reference_and_rejects_tampering() {
        let x = BaseElement::new(0xC0FFEE);
        let y = mimc_permute(x);
        let trace = build_mimc_trace(x);
        // trace's last real row IS the reference output — the circuit computes real MiMC.
        assert_eq!(trace.get(0, ROUNDS), y, "trace output must equal the reference hash");

        let proof = MimcProver::new(mimc_options()).prove(trace).expect("prove MiMC");
        verify_mimc(proof.clone(), MimcPublicInputs { input: x, output: y }).expect("valid must verify");

        // wrong output rejected
        assert!(verify_mimc(proof.clone(), MimcPublicInputs { input: x, output: y + BaseElement::ONE }).is_err(),
            "SECURITY: a proof must not verify against a wrong hash output");

        // tampered proof rejected
        let mut bytes = proof.to_bytes();
        let mid = bytes.len() / 2; bytes[mid] ^= 0xFF;
        let rejected = match winterfell::Proof::from_bytes(&bytes) {
            Ok(p) => verify_mimc(p, MimcPublicInputs { input: x, output: y }).is_err(),
            Err(_) => true,
        };
        assert!(rejected, "SECURITY: a tampered MiMC proof must not verify");
    }
}
