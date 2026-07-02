//! Exact integer Laurent polynomials ℤ[t, t⁻¹].
//!
//! Coefficients are `i128` and every arithmetic step is CHECKED — an overflow
//! panics loudly instead of silently wrapping to a wrong invariant.
//! Representation invariant: the zero polynomial is `coeffs == []` (with
//! `min_exp == 0`); any nonzero polynomial has nonzero first and last
//! coefficients (`coeffs[k]` = coefficient of `t^(min_exp + k)`).

use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LaurentPoly {
    min_exp: i32,
    coeffs: Vec<i128>,
}

impl LaurentPoly {
    pub fn zero() -> Self {
        Self { min_exp: 0, coeffs: Vec::new() }
    }

    pub fn one() -> Self {
        Self::term(1, 0)
    }

    /// The monomial `t`.
    pub fn t() -> Self {
        Self::term(1, 1)
    }

    /// The monomial `t^exp`.
    pub fn t_pow(exp: i32) -> Self {
        Self::term(1, exp)
    }

    /// The monomial `coeff · t^exp`.
    pub fn term(coeff: i128, exp: i32) -> Self {
        if coeff == 0 {
            Self::zero()
        } else {
            Self { min_exp: exp, coeffs: vec![coeff] }
        }
    }

    /// Build from a dense coefficient slice starting at `t^min_exp`;
    /// trims leading/trailing zeros to restore the representation invariant.
    pub fn from_coeffs(min_exp: i32, coeffs: Vec<i128>) -> Self {
        let mut p = Self { min_exp, coeffs };
        p.trim();
        p
    }

    fn trim(&mut self) {
        let lead_zeros = self.coeffs.iter().take_while(|&&c| c == 0).count();
        if lead_zeros == self.coeffs.len() {
            self.coeffs.clear();
            self.min_exp = 0;
            return;
        }
        self.coeffs.drain(..lead_zeros);
        self.min_exp += lead_zeros as i32;
        while self.coeffs.last() == Some(&0) {
            self.coeffs.pop();
        }
    }

    pub fn is_zero(&self) -> bool {
        self.coeffs.is_empty()
    }

    /// Lowest exponent with nonzero coefficient (None for zero).
    pub fn min_exp(&self) -> Option<i32> {
        if self.is_zero() { None } else { Some(self.min_exp) }
    }

    /// Highest exponent with nonzero coefficient (None for zero).
    pub fn max_exp(&self) -> Option<i32> {
        if self.is_zero() { None } else { Some(self.min_exp + self.coeffs.len() as i32 - 1) }
    }

    /// Coefficient of `t^exp`.
    pub fn coeff(&self, exp: i32) -> i128 {
        let idx = exp - self.min_exp;
        if idx < 0 || idx as usize >= self.coeffs.len() {
            0
        } else {
            self.coeffs[idx as usize]
        }
    }

    pub fn neg(&self) -> Self {
        Self {
            min_exp: self.min_exp,
            coeffs: self.coeffs.iter().map(|c| c.checked_neg().expect("i128 neg overflow")).collect(),
        }
    }

    pub fn add(&self, rhs: &Self) -> Self {
        if self.is_zero() {
            return rhs.clone();
        }
        if rhs.is_zero() {
            return self.clone();
        }
        let lo = self.min_exp.min(rhs.min_exp);
        let hi = self.max_exp().unwrap().max(rhs.max_exp().unwrap());
        let mut out = vec![0i128; (hi - lo + 1) as usize];
        for (k, &c) in self.coeffs.iter().enumerate() {
            let idx = (self.min_exp - lo) as usize + k;
            out[idx] = out[idx].checked_add(c).expect("i128 add overflow");
        }
        for (k, &c) in rhs.coeffs.iter().enumerate() {
            let idx = (rhs.min_exp - lo) as usize + k;
            out[idx] = out[idx].checked_add(c).expect("i128 add overflow");
        }
        Self::from_coeffs(lo, out)
    }

    pub fn sub(&self, rhs: &Self) -> Self {
        self.add(&rhs.neg())
    }

    pub fn mul(&self, rhs: &Self) -> Self {
        if self.is_zero() || rhs.is_zero() {
            return Self::zero();
        }
        let mut out = vec![0i128; self.coeffs.len() + rhs.coeffs.len() - 1];
        for (i, &a) in self.coeffs.iter().enumerate() {
            if a == 0 {
                continue;
            }
            for (j, &b) in rhs.coeffs.iter().enumerate() {
                let prod = a.checked_mul(b).expect("i128 mul overflow");
                out[i + j] = out[i + j].checked_add(prod).expect("i128 add overflow");
            }
        }
        Self::from_coeffs(self.min_exp + rhs.min_exp, out)
    }

    /// Exact division: returns `Some(q)` iff `self == q · rhs` with
    /// `q ∈ ℤ[t,t⁻¹]`, else `None`. (Exact division is well-defined in the
    /// integral domain ℤ[t,t⁻¹]; this is what fraction-free Bareiss needs.)
    pub fn div_exact(&self, rhs: &Self) -> Option<Self> {
        if rhs.is_zero() {
            return None;
        }
        if self.is_zero() {
            return Some(Self::zero());
        }
        let ds = self.coeffs.len() - 1;
        let dr = rhs.coeffs.len() - 1;
        if ds < dr {
            return None;
        }
        let dq = ds - dr;
        let mut rem = self.coeffs.clone();
        let mut q = vec![0i128; dq + 1];
        let lead = *rhs.coeffs.last().unwrap();
        for k in (0..=dq).rev() {
            let c = rem[k + dr];
            if c == 0 {
                continue;
            }
            if c % lead != 0 {
                return None;
            }
            let qk = c / lead;
            q[k] = qk;
            for (j, &rc) in rhs.coeffs.iter().enumerate() {
                let prod = qk.checked_mul(rc).expect("i128 mul overflow");
                rem[k + j] = rem[k + j].checked_sub(prod).expect("i128 sub overflow");
            }
        }
        if rem.iter().any(|&c| c != 0) {
            return None;
        }
        Some(Self::from_coeffs(self.min_exp - rhs.min_exp, q))
    }

    /// Canonical representative up to the units ±t^k of ℤ[t,t⁻¹]:
    /// leading (highest-exponent) coefficient positive; exponents centered
    /// (`min_exp = −span/2`) when the span is even — so a symmetric knot
    /// polynomial like the trefoil's prints as `t − 1 + t⁻¹` — and anchored at
    /// `min_exp = 0` when the span is odd (links). Zero stays zero.
    pub fn normalize_alexander(&self) -> Self {
        if self.is_zero() {
            return Self::zero();
        }
        let mut coeffs = self.coeffs.clone();
        if *coeffs.last().unwrap() < 0 {
            for c in coeffs.iter_mut() {
                *c = c.checked_neg().expect("i128 neg overflow");
            }
        }
        let span = coeffs.len() as i32 - 1;
        let min_exp = if span % 2 == 0 { -(span / 2) } else { 0 };
        Self { min_exp, coeffs }
    }

    /// Value at t = 1 (sum of coefficients) — Δ(1) = ±1 for any knot.
    pub fn eval_one(&self) -> i128 {
        self.coeffs
            .iter()
            .fold(0i128, |acc, &c| acc.checked_add(c).expect("i128 add overflow"))
    }

    /// Coefficient-palindrome test — Alexander polynomials satisfy
    /// Δ(t) ≐ Δ(t⁻¹), i.e. the coefficient vector is symmetric.
    pub fn is_palindromic(&self) -> bool {
        let n = self.coeffs.len();
        (0..n / 2).all(|k| self.coeffs[k] == self.coeffs[n - 1 - k])
    }
}

impl fmt::Display for LaurentPoly {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_zero() {
            return write!(f, "0");
        }
        let mut first = true;
        for (k, &c) in self.coeffs.iter().enumerate().rev() {
            if c == 0 {
                continue;
            }
            let exp = self.min_exp + k as i32;
            if first {
                if c < 0 {
                    write!(f, "-")?;
                }
                first = false;
            } else if c < 0 {
                write!(f, " - ")?;
            } else {
                write!(f, " + ")?;
            }
            let a = c.unsigned_abs();
            match exp {
                0 => write!(f, "{a}")?,
                1 if a == 1 => write!(f, "t")?,
                1 => write!(f, "{a}·t")?,
                e if a == 1 => write!(f, "t^{e}")?,
                e => write!(f, "{a}·t^{e}")?,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_mul_basic() {
        // (t + 1)(t⁻¹ + 1) = t + 2 + t⁻¹
        let a = LaurentPoly::from_coeffs(0, vec![1, 1]);
        let b = LaurentPoly::from_coeffs(-1, vec![1, 1]);
        let p = a.mul(&b);
        assert_eq!(p, LaurentPoly::from_coeffs(-1, vec![1, 2, 1]));
        assert_eq!(p.eval_one(), 4);
        assert!(p.is_palindromic());
    }

    #[test]
    fn trim_and_accessors() {
        let p = LaurentPoly::from_coeffs(0, vec![0, 1, 0]);
        assert_eq!(p, LaurentPoly::t());
        assert_eq!(p.min_exp(), Some(1));
        assert_eq!(p.max_exp(), Some(1));
        assert_eq!(p.coeff(1), 1);
        assert_eq!(p.coeff(0), 0);
        assert!(LaurentPoly::from_coeffs(7, vec![0, 0]).is_zero());
    }

    #[test]
    fn div_exact_roundtrip() {
        let a = LaurentPoly::from_coeffs(-2, vec![3, 0, -1, 2]);
        let b = LaurentPoly::from_coeffs(1, vec![-1, 2, 5]);
        let prod = a.mul(&b);
        assert_eq!(prod.div_exact(&b), Some(a.clone()));
        assert_eq!(prod.div_exact(&a), Some(b));
    }

    #[test]
    fn div_exact_rejects_inexact() {
        let a = LaurentPoly::from_coeffs(0, vec![1, 0, 1]); // t² + 1
        let b = LaurentPoly::from_coeffs(0, vec![1, 1]); // t + 1
        assert_eq!(a.div_exact(&b), None); // remainder 2
        let c = LaurentPoly::from_coeffs(0, vec![2, 1]); // t + 2
        assert_eq!(LaurentPoly::from_coeffs(0, vec![1, 1]).div_exact(&c), None); // 1/2 ∉ ℤ
        assert_eq!(a.div_exact(&LaurentPoly::zero()), None);
        assert_eq!(LaurentPoly::zero().div_exact(&b), Some(LaurentPoly::zero()));
    }

    #[test]
    fn normalize_alexander_canonical() {
        // −t² + t − 1 (any unit multiple) → t − 1 + t⁻¹
        let raw = LaurentPoly::from_coeffs(0, vec![-1, 1, -1]);
        let n = raw.normalize_alexander();
        assert_eq!(n, LaurentPoly::from_coeffs(-1, vec![1, -1, 1]));
        // Unit multiples normalize identically.
        let shifted = raw.mul(&LaurentPoly::t_pow(5)).neg();
        assert_eq!(shifted.normalize_alexander(), n);
        assert_eq!(LaurentPoly::zero().normalize_alexander(), LaurentPoly::zero());
    }

    #[test]
    fn display_smoke() {
        let p = LaurentPoly::from_coeffs(-1, vec![1, -3, 1]);
        assert_eq!(p.to_string(), "t - 3 + t^-1");
        assert_eq!(LaurentPoly::zero().to_string(), "0");
    }
}
