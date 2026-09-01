//! Release-channel staleness / up-to-date messages — pure string helpers.
//!
//! Extracted verbatim from main.rs. Shared by the update bar in main.rs and the
//! self-update check in self_update.rs, so they live in one place rather than
//! trailing the version constants in the god-file. No state, no I/O.

use super::{version_gt, VERSION};

pub(crate) fn release_channel_stale_msg(channel_version: &str) -> String {
    format!(
        "release channel is stale: channel v{} < this binary v{} — publish/re-sign sigil-top-latest.json",
        channel_version, VERSION
    )
}

pub(crate) fn release_channel_current_msg(channel_version: &str) -> String {
    if version_gt(VERSION, channel_version) {
        format!("⚠ {}", release_channel_stale_msg(channel_version))
    } else {
        format!("✓ up to date (v{VERSION}; channel v{channel_version}) — checked")
    }
}
