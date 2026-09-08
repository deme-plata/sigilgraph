//! **Universal receipt** — H0 (Bitcoin provenance) → H1 (private SIGIL transition) →
//! H2 (Ethereum settlement) → **H3**, one hash that says: this bitcoin existed, this private
//! transition was valid, this settlement happened, and they are the same economic operation.
//!
//! `H3 = BLAKE3(intent_id ‖ H0 ‖ H1 ‖ H2 ‖ amount_in ‖ amount_out ‖ asset_in ‖ asset_out)`.

use serde::{Deserialize, Serialize};

use crate::attestation::Attestation;
use crate::external::AssetId;
use crate::solver::ExecutionPlan;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UniversalReceipt {
    pub v: String,
    pub intent_id: String,
    pub h0_bitcoin: String,
    pub h1_sigil: String,
    pub h2_ethereum: String,
    pub amount_in: u64,
    pub asset_in: AssetId,
    pub amount_out: String,
    pub asset_out: AssetId,
    pub h3: String,
    /// The per-leg attestation this receipt closes over.
    pub attestation: Attestation,
    pub plan: ExecutionPlan,
}

impl UniversalReceipt {
    pub const VERSION: &'static str = "sigil-pipeline/receipt/v1";

    pub fn h3_bytes(intent_id: &[u8; 32], h0: &[u8; 32], h1: &[u8; 32], h2: &[u8; 32], amount_in: u64, amount_out: u128, asset_in: AssetId, asset_out: AssetId) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(Self::VERSION.as_bytes());
        h.update(intent_id);
        h.update(h0);
        h.update(h1);
        h.update(h2);
        h.update(&amount_in.to_le_bytes());
        h.update(&amount_out.to_le_bytes());
        h.update(asset_in.symbol().as_bytes());
        h.update(b"->");
        h.update(asset_out.symbol().as_bytes());
        *h.finalize().as_bytes()
    }

    pub fn seal(att: Attestation, plan: ExecutionPlan, amount_in: u64, asset_in: AssetId) -> Self {
        let dec = |s: &str| -> [u8; 32] { hex::decode(s).ok().and_then(|v| v.try_into().ok()).expect("attestation hashes are 32-byte hex") };
        let (h0, h1, h2) = (dec(&att.chain[0]), dec(&att.chain[1]), dec(&att.chain[2]));
        let h3 = Self::h3_bytes(&plan.intent_id, &h0, &h1, &h2, amount_in, plan.amount_out, asset_in, plan.asset_out);
        Self {
            v: Self::VERSION.into(),
            intent_id: hex::encode(plan.intent_id),
            h0_bitcoin: att.chain[0].clone(),
            h1_sigil: att.chain[1].clone(),
            h2_ethereum: att.chain[2].clone(),
            amount_in,
            asset_in,
            amount_out: plan.amount_out.to_string(),
            asset_out: plan.asset_out,
            h3: hex::encode(h3),
            attestation: att,
            plan,
        }
    }

    /// Recompute H0–H2 from the legs and H3 from them; false if anything was edited.
    pub fn verify(&self) -> bool {
        if !self.attestation.verify_chain() { return false; }
        let dec = |s: &str| -> Option<[u8; 32]> { hex::decode(s).ok()?.try_into().ok() };
        let (Some(h0), Some(h1), Some(h2), Some(id)) = (dec(&self.h0_bitcoin), dec(&self.h1_sigil), dec(&self.h2_ethereum), dec(&self.intent_id)) else { return false };
        if [self.h0_bitcoin.as_str(), self.h1_sigil.as_str(), self.h2_ethereum.as_str()] != [self.attestation.chain[0].as_str(), self.attestation.chain[1].as_str(), self.attestation.chain[2].as_str()] { return false; }
        let Ok(out) = self.amount_out.parse::<u128>() else { return false };
        hex::encode(Self::h3_bytes(&id, &h0, &h1, &h2, self.amount_in, out, self.asset_in, self.asset_out)) == self.h3
    }

    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(self).expect("serialisable")
    }
}
