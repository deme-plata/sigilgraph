//! The receipt: one BLAKE3 chain over all three legs, so a reader can audit the whole
//! journey from a Bitcoin txid to an Ethereum calldata hash without trusting the bridge.
//! Each link commits to the previous one, so no leg can be swapped after the fact.

use serde::{Deserialize, Serialize};

use crate::btc_in::{display_hash, VerifiedBtcDeposit};
use crate::eth_out::{eth_address_hex, Settlement};
use crate::private_mid::PrivateExit;

pub const ATTESTATION_VERSION: &str = "sigil-pipeline/v0";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BtcLeg {
    pub txid: String,
    pub block_hash: String,
    pub confirmations: u32,
    pub sats: u64,
    pub shield_pk: String,
    pub supply_root: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SigilLeg {
    pub deposit_cm: String,
    pub anchor: String,
    pub nullifier: String,
    pub cm_outs: Vec<String>,
    pub fee_glyphs: u128,
    pub proof_blake3: String,
    pub proof_bytes: usize,
    pub sealed_ct_blake3: String,
    pub exit_tx_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EthLeg {
    pub chain_id: u64,
    pub contract: String,
    pub to: String,
    pub wei: String,
    pub lock_id: String,
    pub calldata_blake3: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Attestation {
    pub v: String,
    pub btc: BtcLeg,
    pub sigil: SigilLeg,
    pub eth: EthLeg,
    /// `h0 = H(btc)`, `h1 = H(h0 ‖ sigil)`, `h2 = H(h1 ‖ eth)`.
    pub chain: [String; 3],
}

fn h(parts: &[&[u8]]) -> [u8; 32] {
    let mut hs = blake3::Hasher::new();
    for p in parts {
        hs.update(p);
    }
    *hs.finalize().as_bytes()
}

impl Attestation {
    pub fn seal(dep: &VerifiedBtcDeposit, deposit_cm: [u8; 32], exit: &PrivateExit, s: &Settlement) -> Self {
        let btc = BtcLeg {
            txid: display_hash(dep.txid),
            block_hash: display_hash(dep.block_hash),
            confirmations: dep.confirmations,
            sats: dep.sats,
            shield_pk: hex::encode(dep.shield_pk),
            supply_root: hex::encode(dep.supply_root),
        };
        let sigil = SigilLeg {
            deposit_cm: hex::encode(deposit_cm),
            anchor: hex::encode(exit.bundle.anchor),
            nullifier: hex::encode(exit.bundle.nullifier),
            cm_outs: exit.bundle.cm_outs.iter().map(hex::encode).collect(),
            fee_glyphs: exit.bundle.public_value,
            proof_blake3: hex::encode(blake3::hash(&exit.bundle.proof).as_bytes()),
            proof_bytes: exit.bundle.proof.len(),
            sealed_ct_blake3: hex::encode(blake3::hash(exit.sealed_to_vault.0.as_bytes()).as_bytes()),
            exit_tx_hash: hex::encode(exit.exit_tx_hash()),
        };
        let eth = EthLeg {
            chain_id: s.chain_id,
            contract: eth_address_hex(&s.contract),
            to: eth_address_hex(&s.eth_dest),
            wei: s.wei().to_string(),
            lock_id: format!("0x{}", hex::encode(s.lock_id_be())),
            calldata_blake3: hex::encode(s.calldata_blake3()),
        };
        let mut a = Self { v: ATTESTATION_VERSION.into(), btc, sigil, eth, chain: [String::new(), String::new(), String::new()] };
        a.chain = a.compute_chain();
        a
    }

    fn compute_chain(&self) -> [String; 3] {
        let b = serde_json::to_vec(&self.btc).expect("serialisable");
        let s = serde_json::to_vec(&self.sigil).expect("serialisable");
        let e = serde_json::to_vec(&self.eth).expect("serialisable");
        let h0 = h(&[ATTESTATION_VERSION.as_bytes(), b"|btc|", &b]);
        let h1 = h(&[&h0, b"|sigil|", &s]);
        let h2 = h(&[&h1, b"|eth|", &e]);
        [hex::encode(h0), hex::encode(h1), hex::encode(h2)]
    }

    /// True iff the chain matches the legs — a swapped or edited leg breaks it.
    pub fn verify_chain(&self) -> bool {
        self.compute_chain() == self.chain
    }

    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(self).expect("serialisable")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_links_are_ordered_and_tamper_evident() {
        let mut a = Attestation {
            v: ATTESTATION_VERSION.into(),
            btc: BtcLeg { txid: "a".into(), block_hash: "b".into(), confirmations: 6, sats: 1, shield_pk: "c".into(), supply_root: "d".into() },
            sigil: SigilLeg { deposit_cm: "e".into(), anchor: "f".into(), nullifier: "g".into(), cm_outs: vec![], fee_glyphs: 0, proof_blake3: "h".into(), proof_bytes: 0, sealed_ct_blake3: "i".into(), exit_tx_hash: "j".into() },
            eth: EthLeg { chain_id: 137, contract: "k".into(), to: "l".into(), wei: "0".into(), lock_id: "m".into(), calldata_blake3: "n".into() },
            chain: Default::default(),
        };
        a.chain = a.compute_chain();
        assert!(a.verify_chain());
        let json = a.to_json_pretty();
        let back: Attestation = serde_json::from_str(&json).unwrap();
        assert!(back.verify_chain());
        a.eth.to = "someone-else".into();
        assert!(!a.verify_chain(), "editing the ETH leg must break h2");
    }
}
