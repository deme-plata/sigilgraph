//! body_mode.rs — Phase F's activation gate (SIGIL_BRAIDPOOL_v1_1.md §3.2,
//! §17: "schema version/height gate; full data-availability gate;
//! digest/reference block body only after DA tests pass").
//!
//! This is the safety rule the whole BraidPool design has repeated since the
//! very first external review pass: a `BlockBatchRef`/`BatchSetRoot`-only
//! block body is UNSAFE below `n=4` validators. A single producer's own
//! certificate only proves IT holds a batch — if it disappears (crash,
//! restart onto a fresh DB, disk loss) before any peer fetched the full
//! batch, a digest-only reference in a committed block points at data that
//! no longer exists anywhere. SIGIL is a single-producer testnet TODAY
//! (Delta/Gamma/Beta confirmed permanently gone, 2026-08-14) — `n=1` — so
//! this gate's real, current output is `InlineTransactions`, always, and
//! that is the CORRECT behavior, not a placeholder waiting to be overridden.
//!
//! `activation_mode` takes `validator_count` as an explicit parameter rather
//! than reading it from anywhere itself: SIGIL doesn't have a formal
//! validator-registry concept yet (peer count from `flux_p2p`'s
//! `NetworkManager::summary()` is raw P2P connectivity, not validator
//! membership) — inventing a fake source here would be worse than making the
//! caller supply a real one once one exists. Wiring a real `validator_count`
//! source is tracked follow-up, not done in this pass.

/// Which shape a block's body takes. `InlineTransactions` is what SIGIL uses
/// today and will keep using until BOTH gates below are satisfied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyMode {
    /// Current, only-safe-today mode: full transaction data travels with
    /// the block, exactly as `sigil-node` already does.
    InlineTransactions,
    /// Transitional: the block commits a `BatchSetRoot`, but the full batch
    /// bytes ALSO travel in an authenticated sidecar with the block — so a
    /// peer can still recover everything from the block message alone, not
    /// from availability-certificate trust.
    AnchoredSidecar,
    /// Only valid once BOTH `n >= 4` AND real per-batch availability has
    /// been certified — see [`activation_mode`]'s doc comment for why both
    /// conditions, not just the validator count, are required.
    CertifiedBatchRefs,
}

/// The gate. Two independent conditions, both required for the "real" mode:
/// - `validator_count >= 4`: the BFT floor `quorum_threshold` (types.rs) is
///   even meaningful at — below this, "certified" just means "the one
///   producer says so," which isn't availability evidence.
/// - `da_certified`: whether availability has ACTUALLY been proven for the
///   SPECIFIC batches this block would reference (e.g. a real
///   `BatchCertificate` reaching quorum for each one) — not just "the
///   network happens to have >= 4 validators in the abstract." A network
///   with 4 validators where nobody has actually run the certification
///   protocol yet must still fall back to `AnchoredSidecar`.
///
/// No environment variable or config flag can skip this — the ONLY inputs
/// are the two booleans-worth of real state, matching the design doc's own
/// rule: "Do not let an environment variable alone bypass this safety gate."
pub fn activation_mode(validator_count: usize, da_certified: bool) -> BodyMode {
    if validator_count < 4 {
        return BodyMode::InlineTransactions;
    }
    if !da_certified {
        return BodyMode::AnchoredSidecar;
    }
    BodyMode::CertifiedBatchRefs
}

/// SIGIL's actual, current, live answer — a named constant rather than a
/// magic literal scattered at call sites, and a single place to update the
/// day this genuinely changes (tracked: CLAUDE.md's Server Gamma/Beta/Delta
/// notes, 2026-08-14).
pub const SIGIL_CURRENT_VALIDATOR_COUNT: usize = 1;

/// What Phase F's gate says for SIGIL as it actually stands today — the
/// assertion this whole module exists to make airtight. If this ever stops
/// being `InlineTransactions` without a deliberate, reviewed change to
/// `SIGIL_CURRENT_VALIDATOR_COUNT`, something has gone wrong.
pub fn sigil_current_body_mode() -> BodyMode {
    activation_mode(SIGIL_CURRENT_VALIDATOR_COUNT, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigil_today_is_inline_transactions_full_stop() {
        assert_eq!(sigil_current_body_mode(), BodyMode::InlineTransactions);
    }

    #[test]
    fn below_four_validators_is_always_inline_regardless_of_da_certified() {
        for n in 0..4 {
            assert_eq!(activation_mode(n, false), BodyMode::InlineTransactions, "n={n}, da_certified=false");
            assert_eq!(
                activation_mode(n, true), BodyMode::InlineTransactions,
                "n={n}, da_certified=true — even CLAIMED certification must not matter below the BFT floor"
            );
        }
    }

    #[test]
    fn four_or_more_validators_without_certification_is_anchored_sidecar() {
        for n in [4, 5, 10, 100] {
            assert_eq!(activation_mode(n, false), BodyMode::AnchoredSidecar, "n={n}");
        }
    }

    #[test]
    fn four_or_more_validators_with_real_certification_is_certified_batch_refs() {
        for n in [4, 5, 10, 100] {
            assert_eq!(activation_mode(n, true), BodyMode::CertifiedBatchRefs, "n={n}");
        }
    }

    #[test]
    fn exactly_at_the_boundary_n_equals_4_is_the_first_eligible_size() {
        assert_eq!(activation_mode(3, true), BodyMode::InlineTransactions);
        assert_eq!(activation_mode(4, true), BodyMode::CertifiedBatchRefs);
    }
}
