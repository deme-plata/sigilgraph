//! Block — header + state-transition + emitted events, composed end-to-end.
//!
//! The header is what gossipsubs across the network. The state transition and
//! event list are kept locally so producers can build the header's roots and
//! light clients can verify inclusion proofs against the header. Phase 0
//! `Block` is in-memory only; persistence to flux-db CFs lands in P3.

use serde::{Deserialize, Serialize};

use sigil_events::SigilEvent;
use sigil_header::{SigilBlockHeaderV0, BlockHash};
use sigil_state::{StateRoots, StateTransition};

/// One full block — header committed to the chain plus the local-only
/// supporting data needed to verify or replay it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    /// The committed header — this is what other nodes see.
    pub header: SigilBlockHeaderV0,
    /// State mutations that produced the header's roots.
    pub transition: StateTransition,
    /// Typed events emitted by the transition. Their leaf hashes appear in
    /// `transition.mutations` as `PushEventHash` entries, but the typed
    /// versions are kept here so wallets + indexers can decode them.
    pub events: Vec<SigilEvent>,
}

impl Block {
    /// Convenience accessor — block hash is just the header hash.
    pub fn hash(&self) -> BlockHash {
        self.header.hash()
    }

    /// Sanity check the block against the four roots the header committed.
    /// This is the "did the producer actually compute the roots they signed?"
    /// gate — every full node runs it before accepting a block.
    pub fn check_roots_match(&self, computed: &StateRoots) -> bool {
        self.header.wallet_state_root   == computed.wallet_state_root &&
        self.header.dex_state_root      == computed.dex_state_root &&
        self.header.event_log_root      == computed.event_log_root &&
        self.header.contract_state_root == computed.contract_state_root
    }
}

/// Test-only: build a tiny `n`-block chain of well-formed-but-fake blocks for
/// serde / snapshot round-trip tests (`snapshot::tests`). Mirrors
/// `sigil_header::tests::fake_header` — heights run 1..=n, every other field is a
/// cheap deterministic constant. NOT consensus-valid; only the serde shape +
/// equality matter here.
#[cfg(test)]
pub(crate) fn __test_chain(n: u64) -> Vec<Block> {
    use sigil_header::{
        ProofBundle, SigScheme, SignatureBytes, SqiSignature, StarkProof, WesolowskiProof,
        HEADER_VERSION, NETWORK_ID, SQISIGN_L5_LEN,
    };
    (1..=n)
        .map(|height| {
            let parent: BlockHash = [(height % 251) as u8; 32];
            let nonce = SqiSignature::from_array([7u8; SQISIGN_L5_LEN]);
            let mut h = blake3::Hasher::new();
            h.update(&parent);
            h.update(nonce.as_bytes());
            let vdf_input: [u8; 32] = *h.finalize().as_bytes();
            let header = SigilBlockHeaderV0 {
                version: HEADER_VERSION,
                network_id: NETWORK_ID,
                height,
                parent_hash: parent,
                merge_parents: vec![],
                timestamp_ms: 1_780_000_000_000 + height,
                nonce_sqisign: nonce,
                vdf_input,
                vdf_proof: WesolowskiProof { y: vec![], pi: vec![], t: 100 },
                difficulty: 0,
                wallet_state_root: [0u8; 32],
                dex_state_root: [0u8; 32],
                event_log_root: [0u8; 32],
                contract_state_root: [0u8; 32],
                state_transition_proof: StarkProof { bytes: vec![], public_inputs_hash: [0u8; 32] },
                txs_merkle_root: [0u8; 32],
                tx_count: 0,
                fluxc_artifact_proof: ProofBundle {
                    artifact_blake3: [0u8; 32],
                    sqisign_sig: vec![],
                    sqisign_pubkey: vec![],
                    settle_tx: None,
                },
                sig_scheme: SigScheme::SqiSign5,
                producer: [0u8; 32],
                producer_sig: SignatureBytes(vec![0u8; SQISIGN_L5_LEN]),
            };
            Block {
                header,
                transition: StateTransition { at_height: height, mutations: vec![] },
                events: vec![],
            }
        })
        .collect()
}
