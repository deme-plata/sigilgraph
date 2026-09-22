//! sigil-updater — release announcement + verification + binary swap.
//!
//! Three responsibilities:
//!
//!   1. [`announcement`] — define the `ReleaseAnnouncement` wire schema, the
//!      canonical signing-bytes derivation, serde round-trip.
//!   2. [`verify`]       — check BLAKE3(binary) matches announcement,
//!                          SQIsign verifies signature, format precheck.
//!   3. [`apply`]        — write new binary to staging path + atomic swap
//!                          + .bak rollback (matches flux-arena-agent v0.1.5).
//!
//! Transport (HTTP poll, gossipsub push, even manual scp) is the caller's
//! concern. The split keeps Phase 0 testable without flux-p2p running:
//! you can `cargo test` this crate in isolation with synthetic fixtures.
//!
//! Phase 1 will wrap this in a `sigil-updater publish | subscribe` CLI bin
//! that drives flux-sigil-net's `/sigil/g0/release` gossipsub topic.

pub mod announcement;
pub mod apply;
pub mod transport;
pub mod verify;

pub use announcement::{ReleaseAnnouncement, UpdaterError, ANNOUNCEMENT_SCHEMA_VERSION};
pub use apply::{apply_to_target, ApplyOutcome};
pub use transport::{
    handle_release_message, is_strictly_newer, BinaryFetcher, ClosureFetcher,
    CurlFetcher, HandledRelease,
};
pub use verify::{verify_announcement, verify_announcement_pinned, verify_binary_bytes, VerifyOk};
