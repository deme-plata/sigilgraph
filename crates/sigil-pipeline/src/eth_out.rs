//! Leg 3 — **public Ethereum settlement**. The vault's settled exit becomes the exact
//! `mint(address to, uint256 amount, uint256 lockId)` call the deployed
//! `SigilBridgeWrappedG3` contract accepts (Polygon, `0x3FCED760…`), with
//! `lockId` = the SIGIL exit tx hash so the contract's `usedLock` map makes a repeat
//! revert on chain. This module produces bytes; it sends nothing.

use sha3::{Digest, Keccak256};

use crate::PipelineError;

/// SIGIL is 10 decimals (glyphs); the wrapped token is a standard 18-decimal ERC-20.
pub const SIGIL_DECIMALS: u32 = 10;
pub const WRAPPED_DECIMALS: u32 = 18;
/// glyphs → wei. The single conversion the whole settlement depends on.
pub const DECIMAL_SHIFT: u128 = 10u128.pow(WRAPPED_DECIMALS - SIGIL_DECIMALS);

/// `mint(address,uint256,uint256)` — pinned; the deployed g3 artifact carries this selector.
pub const MINT_SELECTOR: [u8; 4] = [0x15, 0x6e, 0x29, 0xf6];
pub const MINT_SIGNATURE: &str = "mint(address,uint256,uint256)";
/// The event the LIVE g3 contract emits on mint.
pub const MINTED_EVENT: &str = "Minted(address,uint256,uint256)";
/// The event the current `sigil-relayer` binary LISTENS for. It is not the same event.
pub const LEGACY_OPERATOR_MINTED_EVENT: &str = "OperatorMinted(address,uint256,uint256)";

/// Live deployment constants (Polygon mainnet, 2026-09-06).
pub const POLYGON_CHAIN_ID: u64 = 137;
pub const WSIGIL3_POLYGON: [u8; 20] = [
    0x3F, 0xCE, 0xD7, 0x60, 0xb0, 0xDE, 0x6d, 0x57, 0xF9, 0x68, 0x35, 0xC6, 0x11, 0x0b, 0x12, 0x27,
    0x94, 0x1E, 0xc2, 0xe9,
];

pub fn keccak256(data: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&Keccak256::digest(data));
    out
}

pub fn selector(signature: &str) -> [u8; 4] {
    let h = keccak256(signature.as_bytes());
    [h[0], h[1], h[2], h[3]]
}

pub fn event_topic(signature: &str) -> [u8; 32] {
    keccak256(signature.as_bytes())
}

pub fn parse_eth_address(s: &str) -> Result<[u8; 20], PipelineError> {
    let t = s.trim();
    let t = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
    let v = hex::decode(t).map_err(|e| PipelineError::Eth(format!("address hex: {e}")))?;
    v.try_into().map_err(|_| PipelineError::Eth("an Ethereum address is 20 bytes".into()))
}

pub fn eth_address_hex(a: &[u8; 20]) -> String {
    format!("0x{}", hex::encode(a))
}

/// A settled private exit, ready to be minted on the public chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settlement {
    /// The SIGIL exit tx hash. Becomes `lockId` (big-endian uint256): content-derived, so
    /// it cannot be reset, reused or collided — unlike a counter.
    pub sigil_tx_hash: [u8; 32],
    pub eth_dest: [u8; 20],
    /// Value in glyphs (10dp).
    pub glyphs: u128,
    pub chain_id: u64,
    pub contract: [u8; 20],
}

impl Settlement {
    pub fn wei(&self) -> u128 {
        self.glyphs * DECIMAL_SHIFT
    }

    pub fn lock_id_be(&self) -> [u8; 32] {
        self.sigil_tx_hash
    }

    /// `selector ‖ pad32(to) ‖ uint256(wei) ‖ uint256(lockId)` — 100 bytes, ABI-exact.
    pub fn calldata(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + 32 * 3);
        out.extend_from_slice(&MINT_SELECTOR);
        let mut to = [0u8; 32];
        to[12..].copy_from_slice(&self.eth_dest);
        out.extend_from_slice(&to);
        let mut amt = [0u8; 32];
        amt[16..].copy_from_slice(&self.wei().to_be_bytes());
        out.extend_from_slice(&amt);
        out.extend_from_slice(&self.lock_id_be());
        out
    }

    pub fn calldata_blake3(&self) -> [u8; 32] {
        *blake3::hash(&self.calldata()).as_bytes()
    }

    /// `eth_sendTransaction`-shaped JSON the relayer (or a human with the operator key)
    /// would submit. `from` must be the contract's `operator()`, or the call reverts.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "chainId": self.chain_id,
            "to": eth_address_hex(&self.contract),
            "data": format!("0x{}", hex::encode(self.calldata())),
            "value": "0x0",
            "decoded": {
                "function": MINT_SIGNATURE,
                "to": eth_address_hex(&self.eth_dest),
                "amount_wei": self.wei().to_string(),
                "amount_glyphs": self.glyphs.to_string(),
                "lockId": format!("0x{}", hex::encode(self.lock_id_be())),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keccak_is_the_ethereum_one_not_sha3_256() {
        // Transfer(address,address,uint256) — every ERC-20 explorer shows this topic.
        assert_eq!(
            hex::encode(event_topic("Transfer(address,address,uint256)")),
            "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
        );
        assert_eq!(selector("transfer(address,uint256)"), [0xa9, 0x05, 0x9c, 0xbb]);
    }

    #[test]
    fn mint_selector_matches_the_deployed_g3_artifact() {
        assert_eq!(selector(MINT_SIGNATURE), MINT_SELECTOR);
    }

    #[test]
    fn the_relayer_listens_for_an_event_the_live_contract_never_emits() {
        // This is a finding, pinned as a test so it cannot be forgotten: sigil-relayer's
        // sol! block declares OperatorMinted; SigilBridgeWrappedG3.sol emits Minted.
        assert_ne!(event_topic(MINTED_EVENT), event_topic(LEGACY_OPERATOR_MINTED_EVENT));
    }

    #[test]
    fn one_sigil_becomes_one_wsigil() {
        let s = Settlement {
            sigil_tx_hash: [0x11; 32],
            eth_dest: [0x22; 20],
            glyphs: 10_000_000_000, // 1 SIGIL at 10dp
            chain_id: POLYGON_CHAIN_ID,
            contract: WSIGIL3_POLYGON,
        };
        assert_eq!(s.wei(), 1_000_000_000_000_000_000);
        let cd = s.calldata();
        assert_eq!(cd.len(), 100);
        assert_eq!(&cd[..4], &MINT_SELECTOR);
        assert_eq!(&cd[4..16], &[0u8; 12]);
        assert_eq!(&cd[16..36], &[0x22; 20]);
        assert_eq!(u128::from_be_bytes(cd[52..68].try_into().unwrap()), s.wei());
        assert_eq!(&cd[68..100], &[0x11; 32]);
        assert_eq!(s.to_json()["decoded"]["amount_wei"], "1000000000000000000");
    }

    #[test]
    fn address_parsing() {
        let a = parse_eth_address("0x3FCED760b0DE6d57F96835C6110b1227941Ec2e9").unwrap();
        assert_eq!(a, WSIGIL3_POLYGON);
        assert!(parse_eth_address("0x1234").is_err());
        assert!(parse_eth_address("zz").is_err());
    }
}
