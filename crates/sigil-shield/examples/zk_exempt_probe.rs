//! Minimal reproduction: does winterfell 0.9 accept a trace whose tail is unconstrained
//! randomness, exempted via `set_num_transition_exemptions`?
//!
//! One column, one constraint (x -> x^3 + 42). If THIS verifies, reserved-random-row
//! masking is sound in winterfell and spend_full_v5's failure is specific to its AIR.
//! If it does not, the mechanism itself has a constraint I have not satisfied.

use winterfell::crypto::{hashers::Blake3_256, DefaultRandomCoin};
use winterfell::math::{fields::f64::BaseElement, FieldElement, StarkField, ToElements};
use winterfell::matrix::ColMatrix;
use winterfell::{
    Air, AirContext, Assertion, AuxRandElements, ConstraintCompositionCoefficients,
    DefaultConstraintEvaluator, DefaultTraceLde, EvaluationFrame, FieldExtension, ProofOptions,
    Prover, StarkDomain, Trace, TraceInfo, TracePolyTable, TraceTable, TransitionConstraintDegree,
};

type H = Blake3_256<BaseElement>;
type Coin = DefaultRandomCoin<H>;

/// Declared transition-constraint degree. The TRUE degree of x -> x^3+42 is 3; declaring
/// higher is always sound (degrees are upper bounds) and is how you BUY exemption budget:
/// winterfell sizes the constraint-evaluation domain from `min_blowup_factor()` of the
/// DECLARED degree, and every spare slot in that domain pays for one exemption.
static DECLARED_DEGREE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(3);

#[derive(Clone)]
struct Pub { start: BaseElement, result: BaseElement, real_len: usize }
impl ToElements<BaseElement> for Pub {
    fn to_elements(&self) -> Vec<BaseElement> {
        vec![self.start, self.result, BaseElement::new(self.real_len as u64)]
    }
}

struct ZkAir { ctx: AirContext<BaseElement>, start: BaseElement, result: BaseElement, real_len: usize }

impl Air for ZkAir {
    type BaseField = BaseElement;
    type PublicInputs = Pub;
    type GkrProof = ();
    type GkrVerifier = ();

    fn new(ti: TraceInfo, p: Pub, o: ProofOptions) -> Self {
        let n = ti.length();
        let d = DECLARED_DEGREE.load(std::sync::atomic::Ordering::Relaxed);
        let n = ti.length();
        let degrees = vec![
            TransitionConstraintDegree::with_cycles(d, vec![8]),
            TransitionConstraintDegree::with_cycles(1, vec![n]),
        ];
        // Exempt every row from `real_len - 1` onward as a frame origin: those frames
        // straddle or sit inside the random tail and must not be constrained.
        let exemptions = n - p.real_len + 1;
        let ctx = AirContext::new(ti, degrees, 2, o).set_num_transition_exemptions(exemptions);
        ZkAir { ctx, start: p.start, result: p.result, real_len: p.real_len }
    }

    fn evaluate_transition<E: FieldElement + From<Self::BaseField>>(
        &self, frame: &EvaluationFrame<E>, periodic: &[E], result: &mut [E],
    ) {
        let cur = &frame.current()[0];
        // periodic[0] = short-cycle constant, periodic[1] = the row-0 `first` selector.
        // `first` is zero everywhere except row 0, so this second constraint is vacuous in
        // the random tail — exactly like v5's `first·(...)` family.
        result[0] = frame.next()[0] - (cur.exp(3u32.into()) + E::from(42u32) + periodic[0]);
        if result.len() > 1 {
            result[1] = periodic[1] * (*cur - E::from(BaseElement::new(3)));
        }
    }

    /// Mirrors spend_full_v5's shape: a short-cycle column (like MiMC round constants) and a
    /// full-trace-length column (like its `first` row-0 selector). These are the structural
    /// features the minimal probe was missing.
    fn get_periodic_column_values(&self) -> Vec<Vec<BaseElement>> {
        let n = self.ctx.trace_len();
        let short: Vec<BaseElement> = (0..8u64).map(BaseElement::new).collect();
        let mut first = vec![BaseElement::ZERO; n];
        first[0] = BaseElement::ONE;
        vec![short, first]
    }

    fn get_assertions(&self) -> Vec<Assertion<BaseElement>> {
        vec![
            Assertion::single(0, 0, self.start),
            Assertion::single(0, self.real_len - 1, self.result),
        ]
    }
    fn context(&self) -> &AirContext<BaseElement> { &self.ctx }
}

struct P { options: ProofOptions, real_len: usize }
impl Prover for P {
    type BaseField = BaseElement;
    type Air = ZkAir;
    type Trace = TraceTable<BaseElement>;
    type HashFn = H;
    type RandomCoin = Coin;
    type TraceLde<E: FieldElement<BaseField = BaseElement>> = DefaultTraceLde<E, H>;
    type ConstraintEvaluator<'a, E: FieldElement<BaseField = BaseElement>> =
        DefaultConstraintEvaluator<'a, ZkAir, E>;

    fn get_pub_inputs(&self, trace: &Self::Trace) -> Pub {
        Pub { start: trace.get(0, 0), result: trace.get(0, self.real_len - 1), real_len: self.real_len }
    }
    fn options(&self) -> &ProofOptions { &self.options }
    fn new_trace_lde<E: FieldElement<BaseField = BaseElement>>(
        &self, ti: &TraceInfo, main: &ColMatrix<BaseElement>, domain: &StarkDomain<BaseElement>,
    ) -> (Self::TraceLde<E>, TracePolyTable<E>) {
        DefaultTraceLde::new(ti, main, domain)
    }
    fn new_evaluator<'a, E: FieldElement<BaseField = BaseElement>>(
        &self, air: &'a ZkAir, aux: Option<AuxRandElements<E>>,
        coeffs: ConstraintCompositionCoefficients<E>,
    ) -> Self::ConstraintEvaluator<'a, E> {
        DefaultConstraintEvaluator::new(air, aux, coeffs)
    }
}

fn build(real_len: usize, total: usize, randomize: bool) -> TraceTable<BaseElement> {
    let mut t = TraceTable::new(1, total);
    t.fill(
        |s| { s[0] = BaseElement::new(3); },
        |step, s| {
            if step + 1 >= real_len {
                s[0] = if randomize {
                    let mut h = blake3::Hasher::new();
                    h.update(&(step as u64).to_le_bytes());
                    BaseElement::new(u64::from_le_bytes(h.finalize().as_bytes()[0..8].try_into().unwrap()))
                } else { BaseElement::ZERO };
                return;
            }
            s[0] = s[0].exp(3u32.into()) + BaseElement::new(42) + BaseElement::new((step % 8) as u64);
        },
    );
    t
}

fn try_case(label: &str, real_len: usize, total: usize, randomize: bool, queries: usize, decl: usize) {
    DECLARED_DEGREE.store(decl, std::sync::atomic::Ordering::Relaxed);
    let o = ProofOptions::new(queries, 8, 16, FieldExtension::Quadratic, 8, 31);
    let trace = build(real_len, total, randomize);
    let result_val = trace.get(0, real_len - 1);
    let prover = P { options: o.clone(), real_len };
    let guarded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| prover.prove(trace)));
    let outcome = match guarded {
        Err(_) => { println!("{label:<44} ⛔ REJECTED BY WINTERFELL (exemption budget)"); return; }
        Ok(r) => r,
    };
    match outcome {
        Err(e) => println!("{label:<44} PROVE FAILED: {e:?}"),
        Ok(proof) => {
            let bytes = proof.to_bytes().len();
            let acc = winterfell::AcceptableOptions::MinConjecturedSecurity(80);
            let pi = Pub { start: BaseElement::new(3), result: result_val, real_len };
            match winterfell::verify::<ZkAir, H, Coin>(proof, pi, &acc) {
                Ok(()) => println!("{label:<44} ✅ VERIFIES   ({bytes} bytes)"),
                Err(e) => println!("{label:<44} ❌ {e:?}"),
            }
        }
    }
}

fn main() {
    println!("=== winterfell 0.9: unconstrained random tail via transition exemptions ===\n");
    // Baseline: no padding at all, exemptions = 1 (the ordinary case).
    println!("true constraint degree is 3; `decl` is what we DECLARE to winterfell\n");
    try_case("baseline  real=64  total=64   ex=1    decl=3", 64, 64, false, 42, 3);
    try_case("pad zeros real=64  total=128  ex=65   decl=3", 64, 128, false, 42, 3);
    try_case("pad zeros real=64  total=128  ex=65   decl=4", 64, 128, false, 42, 4);
    try_case("pad RAND  real=64  total=128  ex=65   decl=4", 64, 128, true, 42, 4);
    try_case("pad RAND  real=128 total=256  ex=129  decl=4", 128, 256, true, 42, 4);
    try_case("pad RAND  real=256 total=512  ex=257  decl=4", 256, 512, true, 84, 4);
    try_case("pad RAND  real=256 total=512  ex=257  decl=8", 256, 512, true, 84, 8);
    println!("\n--- now WITH periodic columns (short-cycle + full-trace `first`), like v5 ---");
    try_case("periodic  real=64  total=128  ex=65   decl=4", 64, 128, true, 42, 4);
    try_case("periodic  real=256 total=512  ex=257  decl=4", 256, 512, true, 84, 4);
    try_case("periodic  real=256 total=512  ex=257  decl=8", 256, 512, true, 84, 8);
}
