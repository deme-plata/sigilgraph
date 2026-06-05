//! Genus-2 hyperelliptic Jacobian — the ASIC-hardest VDF group (the drop-in).
//!
//! ## Why genus-2
//! A Wesolowski VDF needs a group of **unknown order** so the exponent `2^t`
//! can't be reduced to shortcut the work. The two no-trusted-setup choices are
//! the class group of an imaginary quadratic field and the Jacobian of a
//! **genus-2 hyperelliptic curve** `y^2 = f(x)` with `deg f = 5`. The genus-2
//! Jacobian is the most ASIC-hostile: group elements are *reduced divisors* in
//! Mumford representation, and a doubling is a branch-heavy sequence of
//! field operations (Cantor's algorithm / Lange's explicit formulas) that does
//! not map cleanly onto fixed-function silicon — so a CPU stays competitive,
//! preserving the egalitarian one-fast-core-one-vote property of the Ω lane.
//!
//! ## Representation (Mumford)
//! A reduced divisor on a genus-2 curve is a pair of polynomials `(u, v)` over
//! the base field `F_p`:
//!   * `u(x)` monic, `deg u <= 2`,
//!   * `v(x)` with `deg v < deg u`, satisfying `u | (v^2 - f)`.
//! The identity is `(1, 0)`. Group law = compose-then-reduce (Cantor); the VDF
//! only needs **doubling** (`D := 2D`), which Lange gives as explicit field
//! formulas (~`1I + 22M` per double for the generic case, plus the degenerate
//! `deg u < 2` branches).
//!
//! ## Status — STRUCTURED, NOT YET REFERENCE-VALIDATED
//! The element type and the curve parameters are defined here so the rest of
//! `flux-vdf` (the Wesolowski protocol, which is group-agnostic) is a drop-in
//! away from running on genus-2. The doubling formulas are intricate and a
//! wrong implementation is worse than none, so [`GenusTwoJacobian`] does **not**
//! yet implement [`super::VdfGroup`]: it must first pass (a) group-axiom fuzzing
//! (random `D`: `2(2D)` via doubling vs. via the addition law agree; identity
//! and reduction invariants hold) and (b) known-answer vectors against a
//! reference (e.g. a Sage `HyperellipticCurve` Jacobian). Until then,
//! [`super::ModSquaring`] is the working VDF group. This is the v0.3.1 task.

use num_bigint::BigUint;

/// Genus-2 curve `y^2 = x^5 + a3 x^3 + a2 x^2 + a1 x + a0` over `F_p`
/// (`a4 = 0` WLOG by translation). `p` is a large prime; the Jacobian order is
/// believed hard to compute, giving the unknown-order property the VDF needs.
#[derive(Clone, Debug)]
pub struct Genus2Curve {
    pub p: BigUint,
    pub a3: BigUint,
    pub a2: BigUint,
    pub a1: BigUint,
    pub a0: BigUint,
}

/// A reduced divisor in Mumford representation: `u` monic deg<=2 (coeffs
/// `[u0, u1]` with implicit leading 1, or deg<2 flagged), `v` deg<deg(u).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Divisor {
    /// u(x) = x^deg_u + u1 x + u0  (deg_u in {0,1,2})
    pub u: Vec<BigUint>,
    /// v(x) = v1 x + v0  (deg < deg_u)
    pub v: Vec<BigUint>,
    pub deg_u: u8,
}

impl Divisor {
    /// The Jacobian identity element, divisor `(1, 0)`.
    pub fn identity() -> Self {
        Divisor { u: vec![BigUint::from(1u32)], v: vec![], deg_u: 0 }
    }
}

/// The genus-2 Jacobian as a (future) VDF group. Holds the curve; the squaring
/// is divisor doubling. NOT wired to `VdfGroup` until validated (see module doc).
pub struct GenusTwoJacobian {
    pub curve: Genus2Curve,
}

impl GenusTwoJacobian {
    pub fn new(curve: Genus2Curve) -> Self {
        Self { curve }
    }

    /// Divisor doubling `D -> 2D` (Lange's genus-2 explicit formulas).
    ///
    /// NOT YET IMPLEMENTED — returns `None`. Wiring this (with the generic case
    /// plus the `deg_u < 2` degenerate branches, then group-axiom fuzzing and
    /// reference vectors) turns `GenusTwoJacobian` into a `VdfGroup` and swaps
    /// the production VDF off `ModSquaring`. The Wesolowski protocol above needs
    /// zero changes — that's the whole point of the trait.
    pub fn double(&self, _d: &Divisor) -> Option<Divisor> {
        None
    }
}
