//! eth.rs — read-only EVM ERC-20 balance lookups. USDC first (the wallet's
//! "show my USDC" ask), across whichever EVM chain the caller asks for —
//! Ethereum mainnet and Polygon PoS today (Polygon: same RPC shape, same
//! ERC-20 ABI, cheaper/faster — this box's operator wants to build there).
//! Adding a third chain is a `Chain` variant + two constants, nothing else.
//!
//! Proxies `eth_call` through a hosted RPC (Alchemy, keyed via
//! `/root/.config/alchemy/api_key`, with free keyless endpoints as a
//! fallback) — NOT this box's own reth+lighthouse pair. That node is real
//! and running (Ethereum mainnet, chain id 1) but still mid-sync in a way
//! that matters: reth stages Headers/Bodies first (fast) then Execution
//! (slow — it actually replays every historical tx to build state).
//! Measured live: Headers at block ~25.69M, Execution at ~5.57M, and
//! `eth_blockNumber` returns 0 — meaning contract-state reads (ERC-20
//! `balanceOf`, exactly what this module needs) are not yet servable
//! locally. `ETH_RPC_URL` overrides the Ethereum endpoint list with a single
//! URL — point it at `http://127.0.0.1:8545` once Execution catches up.
//! There is no local Polygon node on this box, so Polygon always goes
//! through Alchemy/public RPC.
//!
//! Deliberately blocking + minimal (`ureq`, not `reqwest`): see the Cargo.toml
//! comment for why this crate is careful about HTTP-client weight.

use std::time::Duration;

/// `balanceOf(address)` — first 4 bytes of `keccak256("balanceOf(address)")`.
/// Same selector on every EVM chain (it's the function signature, not
/// chain-specific), so this stays a single constant.
const SELECTOR_BALANCE_OF: &str = "0x70a08231";
const USDC_DECIMALS: u32 = 6;

/// Where the Alchemy API key lives — same convention as every other API key
/// on this box (`/root/.config/<service>/api_key`, chmod 600, never in git,
/// never logged). Read fresh on every call rather than cached in a static:
/// this endpoint is low-volume, so the extra syscall is free, and it means a
/// key rotation (`echo new_key > api_key`) takes effect without a restart.
const ALCHEMY_KEY_PATH: &str = "/root/.config/alchemy/api_key";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chain {
    Ethereum,
    Polygon,
}

impl Chain {
    /// Accepts common spellings; defaults to Ethereum when unspecified so
    /// existing callers (no `chain` param) keep working unchanged.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "ethereum" | "eth" | "mainnet" | "ethereum-mainnet" => Some(Chain::Ethereum),
            "polygon" | "matic" | "polygon-pos" => Some(Chain::Polygon),
            _ => None,
        }
    }

    fn alchemy_subdomain(self) -> &'static str {
        match self {
            Chain::Ethereum => "eth-mainnet",
            Chain::Polygon => "polygon-mainnet",
        }
    }

    fn public_fallbacks(self) -> &'static [&'static str] {
        match self {
            Chain::Ethereum => &["https://ethereum-rpc.publicnode.com", "https://1rpc.io/eth"],
            Chain::Polygon => &["https://polygon-bor-rpc.publicnode.com", "https://1rpc.io/matic"],
        }
    }

    /// Native USDC — Circle-issued directly on-chain, NOT the older bridged
    /// USDC.e (a different contract, smaller balances these days). Verified
    /// LIVE before hardcoding (not assumed from memory): `symbol()` decodes
    /// to "USDC", `decimals()` decodes to 6, on both chains below.
    fn usdc_contract(self) -> &'static str {
        match self {
            Chain::Ethereum => "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
            Chain::Polygon => "0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359",
        }
    }

    /// Explicit-override env var name — only Ethereum has one today (the
    /// path to this box's own reth node once it catches up); Polygon has no
    /// local node on this box, so it always goes through Alchemy/public.
    fn override_env(self) -> Option<&'static str> {
        match self {
            Chain::Ethereum => Some("ETH_RPC_URL"),
            Chain::Polygon => None,
        }
    }
}

/// Endpoints tried IN ORDER until one answers:
///   1. The chain's override env var, if set (Ethereum only — see `Chain::override_env`).
///   2. Alchemy, if a key is on disk — authenticated, far more reliable than
///      the free tier below; this is the normal path once a key exists.
///   3. Two free, keyless public endpoints — the fallback of last resort,
///      kept so this never goes fully dark if Alchemy has an outage or the
///      key is ever rotated out without a replacement.
fn rpc_endpoints(chain: Chain) -> Vec<String> {
    if let Some(env_name) = chain.override_env() {
        if let Ok(u) = std::env::var(env_name) {
            return vec![u];
        }
    }
    let mut out = Vec::new();
    if let Ok(key) = std::fs::read_to_string(ALCHEMY_KEY_PATH) {
        let key = key.trim();
        if !key.is_empty() {
            out.push(format!("https://{}.g.alchemy.com/v2/{key}", chain.alchemy_subdomain()));
        }
    }
    out.extend(chain.public_fallbacks().iter().map(|s| s.to_string()));
    out
}

/// `0x` + 40 hex chars → left-padded 64-hex-char ABI word (what `eth_call`
/// calldata needs for an `address` argument). Same shape on every EVM chain.
fn address_to_abi_word(addr: &str) -> Option<String> {
    let a = addr.strip_prefix("0x").unwrap_or(addr);
    if a.len() != 40 || !a.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("{:0>64}", a.to_lowercase()))
}

fn hex_to_u128(hex: &str) -> Result<u128, String> {
    let h = hex.strip_prefix("0x").unwrap_or(hex);
    let h = if h.is_empty() { "0" } else { h };
    u128::from_str_radix(h, 16).map_err(|e| format!("balance overflows u128 or isn't hex: {e}"))
}

fn eth_call(rpc: &str, to: &str, data: &str) -> Result<String, String> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_call",
        "params": [{"to": to, "data": data}, "latest"],
        "id": 1,
    });
    let resp: serde_json::Value = ureq::post(rpc)
        .set("Content-Type", "application/json")
        .timeout(Duration::from_secs(8))
        .send_json(body)
        .map_err(|e| format!("request failed: {e}"))?
        .into_json()
        .map_err(|e| format!("response not JSON: {e}"))?;
    if let Some(err) = resp.get("error") {
        return Err(format!("RPC error: {err}"));
    }
    resp.get("result")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "RPC response missing \"result\"".to_string())
}

/// Try each configured endpoint in order until one answers.
fn eth_call_with_fallback(chain: Chain, to: &str, data: &str) -> Result<String, String> {
    let endpoints = rpc_endpoints(chain);
    let mut last_err = String::new();
    for rpc in &endpoints {
        match eth_call(rpc, to, data) {
            Ok(r) => return Ok(r),
            Err(e) => last_err = format!("{rpc}: {e}"),
        }
    }
    Err(format!("every configured RPC endpoint failed; last error: {last_err}"))
}

/// Query an address's raw USDC balance (base units, 6 decimals) on `chain`.
pub fn usdc_balance_raw(eth_address: &str, chain: Chain) -> Result<u128, String> {
    let word = address_to_abi_word(eth_address)
        .ok_or("address must be a 20-byte hex EVM address (0x + 40 hex chars)")?;
    let data = format!("{SELECTOR_BALANCE_OF}{word}");
    let result = eth_call_with_fallback(chain, chain.usdc_contract(), &data)?;
    hex_to_u128(&result)
}

/// Format raw base units as a decimal USDC amount string, e.g. `1234.567890`.
pub fn format_usdc(raw: u128) -> String {
    let scale = 10u128.pow(USDC_DECIMALS);
    format!("{}.{:0>width$}", raw / scale, raw % scale, width = USDC_DECIMALS as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_parse_accepts_common_spellings_and_defaults_to_ethereum() {
        assert_eq!(Chain::parse(""), Some(Chain::Ethereum));
        assert_eq!(Chain::parse("ethereum"), Some(Chain::Ethereum));
        assert_eq!(Chain::parse("ETH"), Some(Chain::Ethereum));
        assert_eq!(Chain::parse("mainnet"), Some(Chain::Ethereum));
        assert_eq!(Chain::parse("polygon"), Some(Chain::Polygon));
        assert_eq!(Chain::parse("MATIC"), Some(Chain::Polygon));
        assert_eq!(Chain::parse("bsc"), None, "unsupported chains must be rejected, not silently mapped");
    }

    #[test]
    fn each_chain_has_a_distinct_usdc_contract_and_alchemy_subdomain() {
        assert_ne!(Chain::Ethereum.usdc_contract(), Chain::Polygon.usdc_contract());
        assert_ne!(Chain::Ethereum.alchemy_subdomain(), Chain::Polygon.alchemy_subdomain());
    }

    #[test]
    fn only_ethereum_has_a_local_node_override_path() {
        assert!(Chain::Ethereum.override_env().is_some());
        assert!(Chain::Polygon.override_env().is_none(), "no local Polygon node on this box");
    }

    #[test]
    fn address_to_abi_word_pads_and_lowercases() {
        let w = address_to_abi_word("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        assert_eq!(w.len(), 64);
        assert!(w.ends_with("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"));
        assert!(w.starts_with(&"0".repeat(24)));
    }

    #[test]
    fn address_to_abi_word_rejects_wrong_length_or_non_hex() {
        assert!(address_to_abi_word("0x1234").is_none());
        assert!(address_to_abi_word(&("g".repeat(40))).is_none());
    }

    #[test]
    fn hex_to_u128_parses_a_left_padded_32_byte_word() {
        let word = format!("{:0>64}", "3b9aca00"); // 1_000_000_000 in hex, 64-char ABI word
        assert_eq!(hex_to_u128(&word).unwrap(), 1_000_000_000);
    }

    #[test]
    fn hex_to_u128_handles_zero() {
        assert_eq!(hex_to_u128(&"0".repeat(64)).unwrap(), 0);
    }

    #[test]
    fn format_usdc_places_the_decimal_point_at_6_places() {
        assert_eq!(format_usdc(1_234_567_890), "1234.567890");
        assert_eq!(format_usdc(0), "0.000000");
        assert_eq!(format_usdc(1), "0.000001");
        assert_eq!(format_usdc(1_000_000), "1.000000");
    }
}
