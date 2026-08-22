//! errors.rs — a real error type for callers who want more than the
//! `Option`/`bool` returns the rest of this crate deliberately uses for its
//! primary APIs (e.g. `certificate::AvailabilityCertificateV1::try_certify`,
//! `dissemination::reed_solomon::reassemble_batch`). Exists for callers
//! assembling diagnostics — logs, metrics, an eventual RPC error surface —
//! who need to know WHY something failed, not just THAT it did. Matches the
//! `DaError` §11's pseudocode names (`AvailabilityCertificateV1::verify(&self,
//! committee) -> Result<(), DaError>`).

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaError {
    /// BFT data-availability mode is disabled below the `n>=4` floor
    /// (SIGIL_BRAIDPOOL_v1_1.md §3.1) — see `committee::bft_active`.
    BftInactive,
    /// Fewer than `quorum_threshold(n)` (or `committee::availability_quorum`)
    /// distinct valid acks were present.
    NoQuorum,
    /// The signer is not a member of the committee being checked against.
    NotMember,
    /// A validator's ack was dropped because that validator already
    /// contributed one (only the first counts toward quorum).
    DuplicateSigner,
    /// The ack's signature does not verify against the claimed signer's
    /// pubkey.
    SignatureInvalid,
    /// The signed `shard_index` does not match the deterministic assignment
    /// independently recomputed for that validator (§3.5's "do both" check).
    ShardMismatch,
    /// An ack was presented against a `BatchStatementV1` other than the one
    /// it was actually signed for.
    StatementMismatch,
}

impl fmt::Display for DaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::BftInactive => "BFT data-availability mode is disabled below n=4 (SIGIL_BRAIDPOOL_v1_1.md §3.1)",
            Self::NoQuorum => "fewer than the required quorum of valid acks",
            Self::NotMember => "signer is not a member of the committee",
            Self::DuplicateSigner => "validator already contributed an ack for this batch",
            Self::SignatureInvalid => "ack signature does not verify",
            Self::ShardMismatch => "signed shard_index does not match the deterministic assignment",
            Self::StatementMismatch => "ack was signed for a different batch statement",
        };
        f.write_str(s)
    }
}

impl std::error::Error for DaError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_has_a_non_empty_display() {
        let all = [
            DaError::BftInactive,
            DaError::NoQuorum,
            DaError::NotMember,
            DaError::DuplicateSigner,
            DaError::SignatureInvalid,
            DaError::ShardMismatch,
            DaError::StatementMismatch,
        ];
        for e in all {
            assert!(!e.to_string().is_empty(), "{e:?} must have a real message");
        }
    }
}
