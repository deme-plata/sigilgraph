//! Native (Rust) protocol contracts.
//!
//! These are NOT WASM modules — they are chain-native contracts whose state
//! lives in `sigil-state` contract storage slots (committed in
//! `contract_state_root`) and whose every mutation routes through
//! `sigil_state::commit_state_transition`, like every other state write on
//! SIGIL. The WASM lane (`crate::execute`) is for user-deployed code; the
//! contracts here are protocol-level (ported from Quillon's q-vm contracts).

pub mod sigil_share;
