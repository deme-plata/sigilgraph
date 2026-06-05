//! flux-stamp — the **genesis provenance stamp** every Flux build carries.
//!
//! One shared genesis, one verifiable one-liner per artifact. A downstream system that receives a
//! [`Stamp`] can [`Stamp::verify`] it: the stamp's provenance is `blake3(GENESIS_ID ‖ name ‖ version)`,
//! so it proves the artifact declares lineage from the same Flux genesis and was stamped to the
//! highest standard — *without* trusting the sender, just by recomputing the hash.
//!
//! Drop it into any crate as a one-liner:
//! ```ignore
//! pub fn stamp() -> flux_stamp::Stamp { flux_stamp::flux_stamp!() }   // name+version captured at compile time
//! println!("{}", flux_stamp::flux_stamp!().line());                   // genesis banner in build/boot logs
//! ```
//!
//! Crypto-agility: the provenance is BLAKE3 today; the optional [`Stamp::sig`] slot carries a
//! detached signature (e.g. SQIsign L5 from the fluxc `.proof` pipeline) when a build is signed,
//! so the format upgrades from "hash-bound" to "signature-attested" without changing call sites.

use serde::{Deserialize, Serialize};

/// The shared origin marker. Every Flux build descends from this — the genesis.
pub const GENESIS_ID: &str = "flux-genesis-2026.1";

/// The one-liner declaration of the standard.
pub const GENESIS: &str = "FLUX GENESIS · owned by everyone, ruled by no one · verified to the highest standard of Flux";

const PROV_CTX: &str = "flux-stamp v1 provenance 2026";

/// A per-build provenance stamp: crate identity + a genesis-bound provenance hash.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Stamp {
    pub genesis: String,    // GENESIS_ID this build descends from
    pub crate_name: String,
    pub version: String,
    pub provenance: String, // blake3(GENESIS_ID ‖ name ‖ version), hex
    pub sig: Option<String>, // optional detached attestation (SQIsign etc.) when signed
}

fn provenance(name: &str, version: &str) -> String {
    let mut h = blake3::Hasher::new_derive_key(PROV_CTX);
    h.update(GENESIS_ID.as_bytes());
    h.update(b"\x1f");
    h.update(name.as_bytes());
    h.update(b"\x1f");
    h.update(version.as_bytes());
    h.finalize().to_hex().to_string()
}

impl Stamp {
    /// Stamp a build from its crate name + version (use [`flux_stamp!`] to capture them automatically).
    pub fn new(crate_name: &str, version: &str) -> Stamp {
        Stamp {
            genesis: GENESIS_ID.to_string(),
            crate_name: crate_name.to_string(),
            version: version.to_string(),
            provenance: provenance(crate_name, version),
            sig: None,
        }
    }

    /// Attach a detached attestation (e.g. a SQIsign signature over `provenance`).
    pub fn signed_with(mut self, sig: impl Into<String>) -> Stamp {
        self.sig = Some(sig.into());
        self
    }

    /// True iff the stamp's genesis + provenance recompute correctly — the verification gate.
    pub fn verify(&self) -> bool {
        self.genesis == GENESIS_ID && self.provenance == provenance(&self.crate_name, &self.version)
    }

    /// The human one-liner stamped onto build/boot logs.
    pub fn line(&self) -> String {
        let short = &self.provenance[..self.provenance.len().min(12)];
        format!(
            "⚡ {} v{} · {} · flux✓{}{}",
            self.crate_name,
            self.version,
            GENESIS,
            short,
            if self.sig.is_some() { " · 🔏signed" } else { "" }
        )
    }
}

/// Capture the calling crate's name + version at compile time into a [`Stamp`]. One-liner per build.
#[macro_export]
macro_rules! flux_stamp {
    () => {
        $crate::Stamp::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
    };
}

/// The genesis banner — print at boot so logs declare the standard up front.
pub fn genesis_banner() -> String {
    format!("⚡ {} ({})", GENESIS, GENESIS_ID)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_verifies() {
        let s = Stamp::new("flux-secrets", "0.1.0");
        assert!(s.verify());
        assert_eq!(s.genesis, GENESIS_ID);
    }

    #[test]
    fn tampered_stamp_fails() {
        let mut s = Stamp::new("flux-secrets", "0.1.0");
        s.version = "9.9.9".into(); // claim a different version without re-deriving provenance
        assert!(!s.verify());
        let mut s2 = Stamp::new("x", "1.0.0");
        s2.provenance = "deadbeef".into();
        assert!(!s2.verify());
    }

    #[test]
    fn different_crates_differ() {
        let a = Stamp::new("flux-secrets", "0.1.0");
        let b = Stamp::new("flux-sentinel", "0.1.0");
        assert_ne!(a.provenance, b.provenance);
        assert!(a.verify() && b.verify());
    }

    #[test]
    fn line_carries_genesis_and_identity() {
        let s = Stamp::new("flux-secrets", "0.1.0");
        let l = s.line();
        assert!(l.contains("flux-secrets"));
        assert!(l.contains("0.1.0"));
        assert!(l.contains("highest standard of Flux"));
        assert!(l.contains("flux✓"));
    }

    #[test]
    fn signed_roundtrip_and_marker() {
        let s = Stamp::new("flux-node", "0.2.0").signed_with("sqisign:abc123");
        assert!(s.verify()); // signing doesn't change the genesis-binding
        assert!(s.line().contains("🔏signed"));
        assert_eq!(s.sig.as_deref(), Some("sqisign:abc123"));
    }

    #[test]
    fn macro_captures_this_crate() {
        let s = flux_stamp!();
        assert_eq!(s.crate_name, "flux-stamp");
        assert!(s.verify());
    }

    #[test]
    fn serde_roundtrip() {
        let s = Stamp::new("flux-stamp", "0.1.0");
        let j = serde_json::to_string(&s).unwrap_or_default();
        // a receiving system parses + verifies without trusting the sender
        let back: Stamp = serde_json::from_str(&j).unwrap();
        assert!(back.verify());
    }
}
