//! The SIGIL mainnet charter, and the compiler holding the code to it.
//!
//! `MAINNET_CHARTER.txt` (workspace root) is a contract written for people, with
//! parameter lines a program can read (`* key = value`). This crate does two things:
//!
//! 1. [`charter_hash`] is the BLAKE3 hash of its exact bytes. The mainnet genesis block
//!    commits it, so the chain's identity is bound to its contract.
//! 2. The `const` assertions below read the parameter lines at COMPILE time and compare
//!    them with the constants the node actually uses. If they disagree, nothing that
//!    depends on this crate compiles. A contract the code does not obey is decoration;
//!    this makes disobedience a build error.
//!
//! To change a number: change the charter AND the code constant. Changing only one
//! fails the build and names the parameter.

use sigil_bank::welfare;

/// The charter's exact bytes, as hashed into the mainnet genesis block.
pub const CHARTER: &[u8] = include_bytes!("../../../MAINNET_CHARTER.txt");

/// BLAKE3 of [`CHARTER`].
pub fn charter_hash() -> [u8; 32] {
    *blake3::hash(CHARTER).as_bytes()
}

/// Byte offset just past `"* <key> = "` at the start of a line, or `usize::MAX`.
const fn find_value(key: &str) -> usize {
    let c = CHARTER;
    let k = key.as_bytes();
    let mut i = 0;
    while i + 2 + k.len() + 3 <= c.len() {
        let line_start = i == 0 || c[i - 1] == b'\n';
        if line_start && c[i] == b'*' && c[i + 1] == b' ' {
            let mut j = 0;
            let mut same = true;
            while j < k.len() {
                if c[i + 2 + j] != k[j] {
                    same = false;
                    break;
                }
                j += 1;
            }
            let p = i + 2 + k.len();
            if same && c[p] == b' ' && c[p + 1] == b'=' && c[p + 2] == b' ' {
                return p + 3;
            }
        }
        i += 1;
    }
    usize::MAX
}

/// The integer value of a parameter line. Fails compilation if the key is missing or
/// the value is not a plain decimal integer.
pub const fn param_u128(key: &str) -> u128 {
    let start = find_value(key);
    assert!(start != usize::MAX, "charter parameter missing");
    let c = CHARTER;
    let mut i = start;
    let mut v: u128 = 0;
    let mut digits = 0;
    while i < c.len() && c[i] >= b'0' && c[i] <= b'9' {
        v = v * 10 + (c[i] - b'0') as u128;
        i += 1;
        digits += 1;
    }
    assert!(digits > 0, "charter parameter is not an integer");
    assert!(i == c.len() || c[i] == b'\n' || c[i] == b'\r' || c[i] == b' ', "charter parameter has trailing junk");
    v
}

/// Does a parameter line's value equal `expected` exactly?
pub const fn param_is(key: &str, expected: &str) -> bool {
    let start = find_value(key);
    if start == usize::MAX {
        return false;
    }
    let c = CHARTER;
    let e = expected.as_bytes();
    let mut j = 0;
    while j < e.len() {
        if start + j >= c.len() || c[start + j] != e[j] {
            return false;
        }
        j += 1;
    }
    let end = start + e.len();
    end == c.len() || c[end] == b'\n' || c[end] == b'\r' || c[end] == b' '
}

const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

// ── The code must match the charter. Each line names what drifted. ───────────────────

const _: () = assert!(param_u128("decimals") == sigil_state::SIGIL_DECIMALS as u128, "charter: decimals");
const _: () = assert!(param_u128("max_supply_glyphs") == sigil_state::MAX_SUPPLY, "charter: max_supply_glyphs");
const _: () = assert!(param_u128("seconds_per_halving") == sigil_emission::controller::SECONDS_PER_HALVING as u128, "charter: seconds_per_halving");
const _: () = assert!(param_u128("max_reward_per_block_glyphs") == sigil_emission::controller::ABSOLUTE_MAX_REWARD_PER_BLOCK, "charter: max_reward_per_block_glyphs");
const _: () = assert!(param_u128("dev_fee_bps_initial") == sigil_bank::MASTER_MINING_FEE_BPS, "charter: dev_fee_bps_initial");
// sigil-g3 family (2026-10-19): switches already live on g2 are live from genesis there, so the
// welfare split starts at block 0 by design — checked as exactly that, never skipped. Every
// other network (g2, r1, m1) stays bound to the charter's height.
const _: () = assert!(
    if sigil_header::IS_G3 {
        welfare::WELFARE_FROM_HEIGHT == 0
    } else {
        param_u128("welfare_from_height") == welfare::WELFARE_FROM_HEIGHT as u128
    },
    "charter: welfare_from_height"
);
const _: () = assert!(param_u128("dev_fee_bps") == welfare::MASTER_MINING_FEE_BPS_V2, "charter: dev_fee_bps");
const _: () = assert!(param_u128("welfare_bps") == welfare::WELFARE_MINING_FEE_BPS, "charter: welfare_bps");
const _: () = assert!(param_u128("commons_bps") == sigil_bank::COMMONS_MINING_FEE_BPS, "charter: commons_bps");
const _: () = assert!(param_u128("dex_protocol_fee_bps") == sigil_bank::MASTER_SWAP_FEE_BPS, "charter: dex_protocol_fee_bps");
// The genesis block's zero allocation is checked by a test in sigil-node (it needs the
// real genesis builder); here we only require the charter to say zero.
const _: () = assert!(param_u128("premine_glyphs") == 0, "charter: premine_glyphs must be 0");
// The network line binds only the network the charter is for.
const _: () = assert!(
    !str_eq(sigil_header::NETWORK_ID_STR, "sigil-m1") || param_is("network_id", "sigil-m1"),
    "charter: network_id"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_parameter_line_parses() {
        for key in [
            "decimals", "max_supply_glyphs", "premine_glyphs", "seconds_per_halving",
            "max_reward_per_block_glyphs", "dev_fee_bps_initial", "welfare_from_height",
            "dev_fee_bps", "welfare_bps", "commons_bps", "dex_protocol_fee_bps",
        ] {
            let _ = param_u128(key);
        }
        assert!(param_is("network_id", "sigil-m1"));
    }

    #[test]
    fn prose_mentioning_a_key_is_not_a_parameter_line() {
        // "welfare_from_height" appears in prose before its parameter line; only the
        // "* key = " form may be read.
        assert_eq!(param_u128("welfare_from_height"), 900_000);
        assert_eq!(find_value("not_a_parameter"), usize::MAX);
    }

    #[test]
    fn the_hash_is_of_the_exact_bytes() {
        assert_eq!(charter_hash(), *blake3::hash(CHARTER).as_bytes());
        assert_ne!(charter_hash(), [0u8; 32]);
    }
}
