//! Epistemic provenance: where did this number actually come from?
//!
//! The v4 paper's central discipline is that every quantity is labelled by how
//! it was obtained, and that placeholders are never presented as measurements.
//! Table 1 of the paper does this in prose. This module does it in the type
//! system, so a dashboard, an alert rule, or another crate can *ask* whether a
//! reading is trustworthy instead of having to know the paper by heart.
//!
//! The rule this enforces: **a derived quantity is never more trustworthy than
//! its worst input.** [`Provenance::worst`] is the join operation on the
//! lattice, and [`Tracked::combine`] applies it automatically when you compute
//! with tracked values.

use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;

/// A short human note attached to a tracked value.
///
/// `Cow<'static, str>` rather than `&'static str` so the containing structs can
/// derive `Deserialize`: serde always deserializes a `Cow` into the owned
/// variant, while construction from a literal stays allocation-free.
pub type Note = Cow<'static, str>;

/// How a number entered the gauge.
///
/// Ordered from most to least trustworthy. `Ord` follows that order, so
/// `a.max(b)` yields the *less* trustworthy of the two — which is exactly the
/// propagation rule we want.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// Read directly from a live counter (libp2p peer count, block height,
    /// byte counters). Paper Table 1 code `M`.
    Measured,
    /// Computed from measured inputs by a formula in the paper. Paper `M*`/`QM`.
    Derived,
    /// A protocol constant fixed by the design, not by observation
    /// (kappa = 18, tau_confirm = 100, w_obs = 1.0). Paper code `P`.
    Protocol,
    /// A hardcoded stand-in for something that *should* be measured but isn't
    /// wired yet (n_total, delta, f/n, mesh degree). Paper code `H`.
    /// Any result touching one of these is a model, not a measurement.
    Placeholder,
    /// No value could be obtained at all — the input was missing, or the
    /// measurement window was structurally too small to produce a meaningful
    /// answer. Reported as `None`, never silently as `0.0`.
    Unavailable,
}

impl Provenance {
    /// The least trustworthy of two provenances. This is how uncertainty
    /// propagates through a formula.
    pub fn worst(self, other: Self) -> Self {
        if self > other {
            self
        } else {
            other
        }
    }

    /// Fold `worst` across a slice.
    pub fn worst_of(items: &[Provenance]) -> Provenance {
        items
            .iter()
            .copied()
            .fold(Provenance::Measured, Provenance::worst)
    }

    /// True when the value is safe to alert on without a human first reading a
    /// caveat.
    ///
    /// Protocol constants count: the paper's Table 1 deliberately separates `P`
    /// (a value fixed by the design, like `kappa = 18`) from `H` (a guess
    /// standing in for a measurement nobody wired). A `P` is *known*. Only `H`
    /// and "no data at all" make a reading untrustworthy.
    pub fn is_operational(self) -> bool {
        matches!(
            self,
            Provenance::Measured | Provenance::Derived | Provenance::Protocol
        )
    }

    /// Single-letter code matching the paper's Table 1.
    pub fn code(self) -> &'static str {
        match self {
            Provenance::Measured => "M",
            Provenance::Derived => "M*",
            Provenance::Protocol => "P",
            Provenance::Placeholder => "H",
            Provenance::Unavailable => "N",
        }
    }
}

impl fmt::Display for Provenance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Provenance::Measured => "measured",
            Provenance::Derived => "derived",
            Provenance::Protocol => "protocol",
            Provenance::Placeholder => "placeholder",
            Provenance::Unavailable => "unavailable",
        };
        f.write_str(s)
    }
}

/// A number that remembers where it came from and why.
///
/// `note` is a short human sentence explaining the source or the caveat. It is
/// what gets printed in the honest-report rendering, so write it for a reader
/// who has not read the paper.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tracked<T> {
    pub value: T,
    pub provenance: Provenance,
    pub note: Note,
}

impl<T> Tracked<T> {
    pub fn measured(value: T, note: &'static str) -> Self {
        Self { value, provenance: Provenance::Measured, note: Cow::Borrowed(note) }
    }

    pub fn derived(value: T, note: &'static str) -> Self {
        Self { value, provenance: Provenance::Derived, note: Cow::Borrowed(note) }
    }

    pub fn protocol(value: T, note: &'static str) -> Self {
        Self { value, provenance: Provenance::Protocol, note: Cow::Borrowed(note) }
    }

    pub fn placeholder(value: T, note: &'static str) -> Self {
        Self { value, provenance: Provenance::Placeholder, note: Cow::Borrowed(note) }
    }

    /// Mark a value as unavailable while still carrying a fallback so callers
    /// that ignore provenance do not divide by zero. Anything reading
    /// `provenance` will see the value must not be trusted.
    pub fn unavailable(fallback: T, note: &'static str) -> Self {
        Self { value: fallback, provenance: Provenance::Unavailable, note: Cow::Borrowed(note) }
    }

    /// Apply a function to the value, keeping the provenance and replacing the
    /// note.
    pub fn map<U>(self, note: &'static str, f: impl FnOnce(T) -> U) -> Tracked<U> {
        Tracked { value: f(self.value), provenance: self.provenance, note: Cow::Borrowed(note) }
    }

    /// Combine two tracked values. The result inherits the worse provenance —
    /// the whole point of the type.
    ///
    /// `T` is inferred from `f`'s return type, so `Tracked::combine(...)` needs
    /// no turbofish.
    pub fn combine<A, B>(
        a: &Tracked<A>,
        b: &Tracked<B>,
        note: &'static str,
        f: impl FnOnce(&A, &B) -> T,
    ) -> Tracked<T> {
        Tracked {
            value: f(&a.value, &b.value),
            provenance: a.provenance.worst(b.provenance),
            note: Cow::Borrowed(note),
        }
    }

    /// Downgrade the provenance if `worse` is less trustworthy.
    pub fn degrade(mut self, worse: Provenance) -> Self {
        self.provenance = self.provenance.worst(worse);
        self
    }

    pub fn is_operational(&self) -> bool {
        self.provenance.is_operational()
    }
}

impl<T: Copy> Tracked<T> {
    pub fn get(&self) -> T {
        self.value
    }

    /// The value only if it is trustworthy enough to act on.
    pub fn operational(&self) -> Option<T> {
        if self.provenance.is_operational() {
            Some(self.value)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worst_picks_least_trustworthy() {
        assert_eq!(
            Provenance::Measured.worst(Provenance::Placeholder),
            Provenance::Placeholder
        );
        assert_eq!(
            Provenance::Placeholder.worst(Provenance::Unavailable),
            Provenance::Unavailable
        );
        assert_eq!(
            Provenance::Measured.worst(Provenance::Derived),
            Provenance::Derived
        );
    }

    #[test]
    fn worst_of_empty_is_measured() {
        assert_eq!(Provenance::worst_of(&[]), Provenance::Measured);
    }

    #[test]
    fn combine_inherits_worse_provenance() {
        let good = Tracked::measured(4.0_f64, "peers");
        let bad = Tracked::placeholder(50.0_f64, "n_total is hardcoded");
        let ratio = Tracked::combine(&good, &bad, "peers/total", |a, b| a / b);
        assert_eq!(ratio.provenance, Provenance::Placeholder);
        assert!((ratio.value - 0.08).abs() < 1e-12);
    }

    #[test]
    fn protocol_constants_are_operational_but_placeholders_are_not() {
        assert!(Provenance::Protocol.is_operational());
        assert!(!Provenance::Placeholder.is_operational());
        assert!(!Provenance::Unavailable.is_operational());
    }

    #[test]
    fn operational_gate_blocks_placeholders() {
        let p = Tracked::placeholder(1.0_f64, "hardcoded");
        assert!(p.operational().is_none());
        let m = Tracked::measured(1.0_f64, "live counter");
        assert_eq!(m.operational(), Some(1.0));
    }

    #[test]
    fn table1_codes_match_paper() {
        assert_eq!(Provenance::Measured.code(), "M");
        assert_eq!(Provenance::Protocol.code(), "P");
        assert_eq!(Provenance::Placeholder.code(), "H");
    }
}
