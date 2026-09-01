//! Pure hex-formatting helpers extracted from sigil-node/main.rs (god-file split,
//! 2026-09-01). Both take a fixed `&[u8; 32]` (so the `[..4]` slice can never OOB)
//! and are dependency-free — a clean leaf module, no consensus/state logic.

/// Full 64-char lowercase hex of a 32-byte digest.
pub(crate) fn hex_full(b: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for byte in b {
        s.push_str(&format!("{:02x}", byte));
    }
    s
}

/// Short 8-char hex prefix + ellipsis — keeps receiver-log lines readable.
pub(crate) fn hex_short_block(b: &[u8; 32]) -> String {
    let mut s = String::with_capacity(9);
    for byte in &b[..4] {
        s.push_str(&format!("{:02x}", byte));
    }
    s.push('…');
    s
}
