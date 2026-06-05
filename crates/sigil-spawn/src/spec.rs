//! [`ChainSpec`] — the full knob-set for spawning a SIGIL-family PoW chain.
//!
//! Every field is `pub` and serde-(de)serializable so an AI or a human can set
//! them from JSON with zero ceremony. Sensible defaults mean you only override
//! what you care about: `ChainSpec::new("Wickescoin", "WICK")` already yields a
//! valid 21M-cap, 8-decimal, Blake4-PoW chain.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Proof-of-work hash used by the miner. Blake4 is the SIGIL default
/// (see flux-miner: 1Φ ≡ 1 EH/s hashpower unit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PowAlgo {
    Blake3,
    Blake4,
    Sha3,
    /// Memory-hard, ASIC-resistant profile.
    RandomFlux,
}

impl Default for PowAlgo {
    fn default() -> Self {
        PowAlgo::Blake4
    }
}

/// A premine / genesis allocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Allocation {
    pub address: String,
    /// Base units (already scaled by `decimals`).
    pub amount: u128,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SpecError {
    #[error("name must be non-empty")]
    EmptyName,
    #[error("ticker must be 2–8 uppercase chars, got '{0}'")]
    BadTicker(String),
    #[error("decimals must be ≤ 18, got {0}")]
    BadDecimals(u8),
    #[error("max_supply must be > 0")]
    ZeroMaxSupply,
    #[error("initial_block_reward must be > 0")]
    ZeroReward,
    #[error("target_block_time_secs must be > 0")]
    ZeroBlockTime,
    #[error("halving_interval_blocks must be > 0")]
    ZeroHalving,
    #[error("emission ({emitted}) + premine ({premine}) exceeds max_supply ({cap})")]
    SupplyExceedsCap { emitted: u128, premine: u128, cap: u128 },
}

/// The complete spawn specification — 26 knobs across 6 groups.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainSpec {
    // ── identity / branding ──────────────────────────────────────────────
    pub name: String,            // 1  human name, e.g. "Wickescoin"
    pub ticker: String,          // 2  symbol, e.g. "WICK"
    pub decimals: u8,            // 3  base-unit precision

    // ── supply / emission ────────────────────────────────────────────────
    pub max_supply: u128,        // 4  hard cap, base units
    pub initial_block_reward: u128, // 5  reward at height 0, base units
    pub halving_interval_blocks: u64, // 6  blocks per halving
    pub tail_emission: u128,     // 7  perpetual per-block reward after halvings hit 0 (0 = none)
    pub premine: Vec<Allocation>,// 8  genesis allocations

    // ── proof-of-work / difficulty ───────────────────────────────────────
    pub pow_algo: PowAlgo,       // 9  hash function
    pub target_block_time_secs: u64, // 10  desired spacing
    pub difficulty_window_blocks: u64, // 11  retarget window
    pub initial_difficulty_bits: u32,  // 12  leading-zero-bits target at genesis
    pub max_difficulty_change_pct: u32, // 13  clamp per retarget (e.g. 25 = ±25%)

    // ── block / consensus ────────────────────────────────────────────────
    pub max_block_bytes: u32,    // 14  block size cap
    pub coinbase_maturity: u64,  // 15  blocks before coinbase is spendable
    pub genesis_timestamp: u64,  // 16  unix secs
    pub genesis_message: String, // 17  embedded headline / nonce

    // ── network / p2p ────────────────────────────────────────────────────
    pub base_p2p_port: u16,      // 18  port the spawner offsets from (per-name)
    pub base_rpc_port: u16,      // 19  ditto for RPC
    pub bootstrap_peers: Vec<String>, // 20  initial dial list
    pub dns_seeds: Vec<String>,  // 21  optional DNS seeds

    // ── fees / governance ────────────────────────────────────────────────
    pub min_tx_fee: u128,        // 22  base units
    pub dev_fee_bps: u16,        // 23  basis points of block reward to dev addr
    pub dev_address: Option<String>, // 24  recipient of dev fee
    pub treasury_address: Option<String>, // 25  on-chain treasury
    pub governance_enabled: bool, // 26  on-chain voting toggle
}

impl ChainSpec {
    /// A valid spec from just a name + ticker; everything else gets a 21M-cap,
    /// 8-decimal, ~2-minute-block Blake4 default.
    pub fn new(name: impl Into<String>, ticker: impl Into<String>) -> Self {
        let decimals = 8u8;
        let unit = 10u128.pow(decimals as u32);
        Self {
            name: name.into(),
            ticker: ticker.into(),
            decimals,
            max_supply: 21_000_000 * unit,
            initial_block_reward: 50 * unit,
            halving_interval_blocks: 210_000,
            tail_emission: 0,
            premine: Vec::new(),
            pow_algo: PowAlgo::default(),
            target_block_time_secs: 120,
            difficulty_window_blocks: 2016,
            initial_difficulty_bits: 20,
            max_difficulty_change_pct: 25,
            max_block_bytes: 1_000_000,
            coinbase_maturity: 100,
            genesis_timestamp: 0,
            genesis_message: String::new(),
            base_p2p_port: 9500,
            base_rpc_port: 9600,
            bootstrap_peers: Vec::new(),
            dns_seeds: Vec::new(),
            min_tx_fee: 1_000,
            dev_fee_bps: 0,
            dev_address: None,
            treasury_address: None,
            governance_enabled: false,
        }
    }

    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("ChainSpec serializes")
    }

    /// One base unit = 10^decimals.
    pub fn unit(&self) -> u128 {
        10u128.pow(self.decimals as u32)
    }

    /// Total premine across all allocations.
    pub fn premine_total(&self) -> u128 {
        self.premine.iter().map(|a| a.amount).sum()
    }

    /// Total coins emitted via PoW until the halving schedule reaches zero
    /// (excludes tail emission, which is unbounded by design). Integer halving
    /// with truncation, summed exactly by walking each era.
    pub fn emitted_via_halving(&self) -> u128 {
        let mut total: u128 = 0;
        let mut reward = self.initial_block_reward;
        while reward > 0 {
            total = total.saturating_add(reward.saturating_mul(self.halving_interval_blocks as u128));
            reward /= 2;
        }
        total
    }

    /// Validate every invariant. The supply-cap check is the load-bearing one
    /// (cf. SIGIL's hard 21M cap): emission + premine must fit under max_supply.
    pub fn validate(&self) -> Result<(), SpecError> {
        if self.name.trim().is_empty() {
            return Err(SpecError::EmptyName);
        }
        let t = &self.ticker;
        if t.len() < 2 || t.len() > 8 || !t.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
            return Err(SpecError::BadTicker(t.clone()));
        }
        if self.decimals > 18 {
            return Err(SpecError::BadDecimals(self.decimals));
        }
        if self.max_supply == 0 {
            return Err(SpecError::ZeroMaxSupply);
        }
        if self.initial_block_reward == 0 {
            return Err(SpecError::ZeroReward);
        }
        if self.target_block_time_secs == 0 {
            return Err(SpecError::ZeroBlockTime);
        }
        if self.halving_interval_blocks == 0 {
            return Err(SpecError::ZeroHalving);
        }
        let emitted = self.emitted_via_halving();
        let premine = self.premine_total();
        if emitted.saturating_add(premine) > self.max_supply {
            return Err(SpecError::SupplyExceedsCap {
                emitted,
                premine,
                cap: self.max_supply,
            });
        }
        Ok(())
    }
}
