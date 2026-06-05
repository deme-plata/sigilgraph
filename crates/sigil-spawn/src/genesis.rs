//! Genesis block derivation + the emission-schedule preview.
//!
//! The genesis hash binds the chain's identity and economic parameters into a
//! single commitment, so two chains with different specs can never share a
//! genesis. The emission schedule lets a caller (or AI) eyeball the full supply
//! curve and confirm it stays under `max_supply` before spawning.

use crate::identity::ChainIdentity;
use crate::spec::ChainSpec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Genesis {
    /// blake3 over the canonical genesis preimage (hex).
    pub hash: String,
    pub timestamp: u64,
    pub message: String,
    /// Coins allocated at genesis (premine total).
    pub initial_supply: u128,
    pub difficulty_bits: u32,
}

impl Genesis {
    pub fn build(spec: &ChainSpec, id: &ChainIdentity) -> Self {
        let mut pre = Vec::new();
        pre.extend_from_slice(id.network_id.as_bytes());
        pre.extend_from_slice(spec.name.as_bytes());
        pre.extend_from_slice(spec.ticker.as_bytes());
        pre.extend_from_slice(&spec.max_supply.to_le_bytes());
        pre.extend_from_slice(&spec.genesis_timestamp.to_le_bytes());
        pre.extend_from_slice(spec.genesis_message.as_bytes());
        pre.extend_from_slice(&spec.premine_total().to_le_bytes());
        let hash = hex::encode(blake3::hash(&pre).as_bytes());

        Self {
            hash,
            timestamp: spec.genesis_timestamp,
            message: spec.genesis_message.clone(),
            initial_supply: spec.premine_total(),
            difficulty_bits: spec.initial_difficulty_bits,
        }
    }
}

/// One halving era in the emission preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmissionEra {
    pub era: u32,
    pub start_height: u64,
    pub reward: u128,
    pub era_emitted: u128,
    /// Running total including the genesis premine.
    pub cumulative: u128,
}

/// Walk the halving schedule until the block reward truncates to zero.
pub fn emission_schedule(spec: &ChainSpec) -> Vec<EmissionEra> {
    let mut out = Vec::new();
    let mut reward = spec.initial_block_reward;
    let mut height = 0u64;
    let mut cumulative = spec.premine_total();
    let mut era = 0u32;
    while reward > 0 && era < 256 {
        let era_emitted = reward.saturating_mul(spec.halving_interval_blocks as u128);
        cumulative = cumulative.saturating_add(era_emitted);
        out.push(EmissionEra { era, start_height: height, reward, era_emitted, cumulative });
        height = height.saturating_add(spec.halving_interval_blocks);
        reward /= 2;
        era += 1;
    }
    out
}
