//! sigil-events — typed event ledger.
//!
//! See `SIGIL_GENESIS_v0.md` §4. Every protocol-level mutation that a wallet,
//! indexer, or block explorer cares about flows through [`SigilEvent`]. The
//! crate's two jobs:
//!
//! 1. **Deterministic encoding.** Two nodes given the same tx set MUST produce
//!    bit-identical event bytes → identical leaf hashes → identical Merkle
//!    root. Phase 0 uses canonical JSON (serde_json with sorted keys via the
//!    natural enum-tag layout); P3 swaps in bincode for ~3× compression.
//!
//! 2. **Inclusion proofs.** Anyone holding `(event, MerkleProof, header)` can
//!    verify the event occurred at that height with those exact parameters
//!    without trusting an indexer. [`prove_inclusion`] + [`verify_inclusion`].

#![warn(missing_docs)]

/// Decimal-string codec for `u128` — works around serde_json's lack of
/// u128 wire support. Apply with `#[serde(with = "u128_str")]`.
pub mod u128_str {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &u128, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

use serde::{Deserialize, Serialize};

use sigil_header::ValidatorId;
use sigil_state::{ContractId, PoolId, TokenId, WalletId};

/// Compact type tag for indexing in flux-db (`events_by_type` CF).
pub type EventTag = u8;

/// All on-chain events SIGIL emits per block. Order matches genesis §4. Add
/// new variants AT THE END to preserve tag compatibility for indexers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum SigilEvent {
    /// A wallet sent tokens to another wallet.
    Send {
        /// Sender wallet.
        from: WalletId,
        /// Recipient wallet.
        to: WalletId,
        /// Amount sent in token's base units.
        #[serde(with = "u128_str")]
    amount: u128,
        /// Token sent. All-zero = native SIGIL.
        token: TokenId,
        /// Fee paid in the native token.
        #[serde(with = "u128_str")]
    fee: u128,
    },
    /// Mirror side of a Send — emitted on the recipient's account index.
    Receive {
        /// Sender wallet.
        from: WalletId,
        /// Recipient wallet.
        to: WalletId,
        /// Amount received in token's base units.
        #[serde(with = "u128_str")]
    amount: u128,
        /// Token received. All-zero = native SIGIL.
        token: TokenId,
    },
    /// DEX swap executed via a pool.
    SwapExecuted {
        /// Pool ID swap routed through.
        pool: PoolId,
        /// Input token.
        in_token: TokenId,
        /// Input amount.
        #[serde(with = "u128_str")]
    in_amt: u128,
        /// Output token.
        out_token: TokenId,
        /// Output amount delivered to swapper.
        #[serde(with = "u128_str")]
    out_amt: u128,
        /// Realized slippage vs quoted, in bps.
        slippage_bps: u16,
        /// Total fee paid in native token.
        #[serde(with = "u128_str")]
    fee_paid: u128,
    },
    /// LP deposited liquidity into a pool.
    LpDeposited {
        /// Target pool.
        pool: PoolId,
        /// Amount of token A added.
        #[serde(with = "u128_str")]
    amt_a: u128,
        /// Amount of token B added.
        #[serde(with = "u128_str")]
    amt_b: u128,
        /// LP shares minted to the depositor.
        #[serde(with = "u128_str")]
    shares_received: u128,
    },
    /// LP burned shares and withdrew underlying.
    LpWithdrawn {
        /// Source pool.
        pool: PoolId,
        /// Shares burned.
        #[serde(with = "u128_str")]
    shares_burned: u128,
        /// Token A withdrawn.
        #[serde(with = "u128_str")]
    amt_a: u128,
        /// Token B withdrawn.
        #[serde(with = "u128_str")]
    amt_b: u128,
        /// Fees realized during withdrawal.
        #[serde(with = "u128_str")]
    fees_realized: u128,
    },
    /// VM contract method call.
    ContractCall {
        /// Contract ID called.
        contract: ContractId,
        /// 4-byte method selector.
        method: [u8; 4],
        /// Gas consumed.
        gas_used: u64,
        /// BLAKE3 of return value bytes.
        result_hash: [u8; 32],
    },
    /// VM contract deployed.
    ContractDeploy {
        /// Wallet that deployed.
        creator: WalletId,
        /// New contract's ID.
        contract_id: ContractId,
        /// BLAKE3 of deployed bytecode.
        bytecode_hash: [u8; 32],
        /// Gas consumed.
        gas_used: u64,
    },
    /// Mining reward minted at this height.
    MintReward {
        /// Miner who produced the block.
        miner: ValidatorId,
        /// Block height the reward sealed at.
        height: u64,
        /// Reward amount in native token.
        #[serde(with = "u128_str")]
    amount: u128,
    },
    /// New token deployed (Quillon-compatible deploy_token surface).
    TokenDeployed {
        /// Wallet that created the token.
        creator: WalletId,
        /// Display ticker.
        ticker: String,
        /// Token decimals.
        decimals: u8,
        /// Initial supply minted to creator.
        #[serde(with = "u128_str")]
    initial_supply: u128,
    },
    /// Validator joined the consensus set.
    ValidatorJoined {
        /// New validator's ID.
        validator: ValidatorId,
        /// Stake locked.
        #[serde(with = "u128_str")]
    stake: u128,
    },
    /// Validator exited the consensus set, stake refunded.
    ValidatorLeft {
        /// Exiting validator.
        validator: ValidatorId,
        /// Stake returned.
        #[serde(with = "u128_str")]
    refunded_stake: u128,
    },
    /// A confidential (mixer-routed) send happened. None of `from`, `to`, or
    /// `amount` are exposed to non-participants — only the cleartext fee, a
    /// `token_hint` (or all-zero for multi-token), the number of inputs/
    /// outputs, and the BLAKE3 commitment-to-the-proof. Mirrors the shape
    /// of `sigil_mixer::ShieldedSendTxData` so wallet indexers and explorers
    /// can render activity without breaking privacy.
    ///
    /// Per Viktor's 2026-05-29 directive ("all transactions private, no opt-in
    /// for transparent"), this is the variant `SigilTx::Send` will collapse
    /// into; appended at the end (tag 11) to preserve indexer compatibility
    /// with the existing transparent `Send`.
    ShieldedSend {
        /// Single-token tx: the token-id involved. All-zero if multi-token.
        token_hint: TokenId,
        /// Cleartext fee paid in native SIGIL.
        #[serde(with = "u128_str")]
        fee: u128,
        /// Number of input nullifiers (= commitments spent). Cleartext so
        /// indexers can estimate anonymity-set turnover without learning
        /// the spender.
        n_inputs: u32,
        /// Number of output commitments (= new shielded notes). Cleartext.
        n_outputs: u32,
        /// BLAKE3 of `sigil_mixer::ShieldedSendTxData::proof.proof_bytes`.
        /// Lets indexers / verifiers cross-reference a tx's privacy proof
        /// without carrying the full proof bytes in the event log.
        proof_digest: [u8; 32],
        /// Pool's commitment-set root (Sparse Merkle root) the proof was
        /// generated against. Empty in Phase 0 (pool is a BTreeSet, not an
        /// SMT). Lets verifiers ensure the proof anchored to the correct
        /// pool snapshot at block apply time.
        pool_root_at_proof: [u8; 32],
    },
}

impl SigilEvent {
    /// Compact type tag for the `events_by_type` flux-db CF. Stable across
    /// versions (new variants must append, never reorder).
    pub fn tag(&self) -> EventTag {
        match self {
            SigilEvent::Send            { .. } => 0,
            SigilEvent::Receive         { .. } => 1,
            SigilEvent::SwapExecuted    { .. } => 2,
            SigilEvent::LpDeposited     { .. } => 3,
            SigilEvent::LpWithdrawn     { .. } => 4,
            SigilEvent::ContractCall    { .. } => 5,
            SigilEvent::ContractDeploy  { .. } => 6,
            SigilEvent::MintReward      { .. } => 7,
            SigilEvent::TokenDeployed   { .. } => 8,
            SigilEvent::ValidatorJoined { .. } => 9,
            SigilEvent::ValidatorLeft   { .. } => 10,
            SigilEvent::ShieldedSend    { .. } => 11,
        }
    }

    /// Deterministic encoding. Phase 0 uses canonical JSON (serde_json with
    /// the enum tagged by `kind`). P3 swaps in bincode — every consumer of
    /// this bytes-output must move in lockstep.
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// BLAKE3 of the encoded bytes — what gets pushed into the SMT/Merkle
    /// state for `event_log_root`.
    pub fn leaf_hash(&self) -> [u8; 32] {
        *blake3::hash(&self.encode()).as_bytes()
    }
}

/// Position-bound proof that a specific event is in a block's event log.
/// Sized as `(siblings.len() == log2(N))` where N is the padded event count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MerkleProof {
    /// Position of the event in the block-scoped event list, 0-indexed.
    pub index: u32,
    /// Total number of events in this block (pre-padding). The verifier
    /// re-derives the padding rule from this.
    pub total: u32,
    /// Sibling hashes from leaf to root, low-to-high level.
    pub siblings: Vec<[u8; 32]>,
}

/// Errors when verifying an inclusion proof.
#[derive(Debug, thiserror::Error)]
pub enum ProofError {
    /// `proof.index >= proof.total`.
    #[error("event index {index} out of range for total {total}")]
    IndexOutOfRange {
        /// Event index from the proof.
        index: u32,
        /// Total events from the proof.
        total: u32,
    },
    /// Wrong number of siblings for the declared `total`.
    #[error("proof has {got} siblings, expected {expected} for total={total}")]
    WrongDepth {
        /// Siblings the proof actually carries.
        got: usize,
        /// Siblings expected from `total`.
        expected: usize,
        /// Reported total events.
        total: u32,
    },
    /// Computed root didn't match the expected `event_log_root`.
    #[error("computed root != expected event_log_root")]
    RootMismatch,
}

/// Build a proof that `events[index]` is committed under the Merkle root
/// produced by hashing `events` with the same padding rule used in
/// [`sigil_state::commit_state_transition`].
///
/// Returns `None` if `index >= events.len()`.
pub fn prove_inclusion(events: &[SigilEvent], index: usize) -> Option<MerkleProof> {
    if index >= events.len() {
        return None;
    }
    let leaves: Vec<[u8; 32]> = events.iter().map(|e| e.leaf_hash()).collect();
    let mut layer = leaves;
    let total = layer.len() as u32;
    let mut idx = index;
    let mut siblings = Vec::new();

    while layer.len() > 1 {
        if layer.len() % 2 == 1 {
            let last = *layer.last().unwrap();
            layer.push(last);
        }
        let sib_idx = if idx % 2 == 0 { idx + 1 } else { idx - 1 };
        siblings.push(layer[sib_idx]);
        let mut next = Vec::with_capacity(layer.len() / 2);
        for pair in layer.chunks(2) {
            let mut h = blake3::Hasher::new();
            h.update(&pair[0]);
            h.update(&pair[1]);
            next.push(*h.finalize().as_bytes());
        }
        layer = next;
        idx /= 2;
    }

    Some(MerkleProof {
        index: index as u32,
        total,
        siblings,
    })
}

/// Verify a proof against an expected `event_log_root` for a given event.
/// The header carries the root; the wallet carries the event + proof.
pub fn verify_inclusion(
    event: &SigilEvent,
    proof: &MerkleProof,
    expected_root: [u8; 32],
) -> Result<(), ProofError> {
    if proof.index >= proof.total {
        return Err(ProofError::IndexOutOfRange {
            index: proof.index,
            total: proof.total,
        });
    }
    let expected_depth = ceil_log2(proof.total.max(1) as usize);
    if proof.siblings.len() != expected_depth {
        return Err(ProofError::WrongDepth {
            got: proof.siblings.len(),
            expected: expected_depth,
            total: proof.total,
        });
    }

    let mut acc = event.leaf_hash();
    let mut idx = proof.index as usize;
    for sib in &proof.siblings {
        let mut h = blake3::Hasher::new();
        if idx % 2 == 0 {
            h.update(&acc);
            h.update(sib);
        } else {
            h.update(sib);
            h.update(&acc);
        }
        acc = *h.finalize().as_bytes();
        idx /= 2;
    }

    if acc == expected_root {
        Ok(())
    } else {
        Err(ProofError::RootMismatch)
    }
}

fn ceil_log2(n: usize) -> usize {
    if n <= 1 {
        return 0;
    }
    (usize::BITS - (n - 1).leading_zeros()) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_state::{
        commit_state_transition, SigilState, StateMutation, StateTransition,
    };

    fn fixture() -> Vec<SigilEvent> {
        vec![
            SigilEvent::MintReward { miner: [1u8; 32], height: 1, amount: 50 },
            SigilEvent::Send {
                from: [2u8; 32], to: [3u8; 32], amount: 10, token: [0u8; 32], fee: 1,
            },
            SigilEvent::Receive {
                from: [2u8; 32], to: [3u8; 32], amount: 10, token: [0u8; 32],
            },
            SigilEvent::ValidatorJoined { validator: [4u8; 32], stake: 1000 },
            SigilEvent::ShieldedSend {
                token_hint: [0u8; 32],
                fee: 1,
                n_inputs: 2,
                n_outputs: 2,
                proof_digest: [0xCC; 32],
                pool_root_at_proof: [0u8; 32],
            },
        ]
    }

    #[test]
    fn shielded_send_tag_is_11_and_distinct_from_send() {
        let s = SigilEvent::Send {
            from: [0u8; 32], to: [0u8; 32], amount: 0, token: [0u8; 32], fee: 0,
        };
        let ss = SigilEvent::ShieldedSend {
            token_hint: [0u8; 32], fee: 0, n_inputs: 1, n_outputs: 1,
            proof_digest: [0u8; 32], pool_root_at_proof: [0u8; 32],
        };
        assert_eq!(s.tag(), 0);
        assert_eq!(ss.tag(), 11);
        // Different leaf hashes — these are distinct event kinds and must
        // never collide in the event-log Merkle tree.
        assert_ne!(s.leaf_hash(), ss.leaf_hash());
    }

    #[test]
    fn encode_is_deterministic() {
        let e = SigilEvent::Send {
            from: [1u8; 32], to: [2u8; 32], amount: 100, token: [0u8; 32], fee: 1,
        };
        assert_eq!(e.encode(), e.encode());
    }

    #[test]
    fn tags_are_dense_and_stable() {
        let events = fixture();
        let tags: Vec<EventTag> = events.iter().map(|e| e.tag()).collect();
        // ShieldedSend appended at tag 11 — fixture order matches MintReward
        // (7), Send (0), Receive (1), ValidatorJoined (9), ShieldedSend (11).
        assert_eq!(tags, vec![7, 0, 1, 9, 11]);
    }

    #[test]
    fn inclusion_proof_roundtrip() {
        let events = fixture();
        // Build the same root the state machine would.
        let mut s = SigilState::new();
        let t = StateTransition {
            at_height: 1,
            mutations: events.iter().map(|e| StateMutation::PushEventHash(e.leaf_hash())).collect(),
        };
        let roots = commit_state_transition(&mut s, &t, 1).unwrap();

        for (i, e) in events.iter().enumerate() {
            let proof = prove_inclusion(&events, i).unwrap();
            assert!(
                verify_inclusion(e, &proof, roots.event_log_root).is_ok(),
                "inclusion failed at index {}", i
            );
        }
    }

    #[test]
    fn proof_rejects_tampered_event() {
        let events = fixture();
        let proof = prove_inclusion(&events, 0).unwrap();

        let mut s = SigilState::new();
        let t = StateTransition {
            at_height: 1,
            mutations: events.iter().map(|e| StateMutation::PushEventHash(e.leaf_hash())).collect(),
        };
        let roots = commit_state_transition(&mut s, &t, 1).unwrap();

        // Pretend the event is something else — the proof targets index 0
        // (MintReward) but we present a Send. Hashes must mismatch.
        let tampered = SigilEvent::Send {
            from: [9u8; 32], to: [9u8; 32], amount: 9, token: [0u8; 32], fee: 0,
        };
        assert!(matches!(
            verify_inclusion(&tampered, &proof, roots.event_log_root),
            Err(ProofError::RootMismatch)
        ));
    }

    #[test]
    fn ceil_log2_table() {
        assert_eq!(ceil_log2(0), 0);
        assert_eq!(ceil_log2(1), 0);
        assert_eq!(ceil_log2(2), 1);
        assert_eq!(ceil_log2(3), 2);
        assert_eq!(ceil_log2(4), 2);
        assert_eq!(ceil_log2(5), 3);
        assert_eq!(ceil_log2(8), 3);
        assert_eq!(ceil_log2(9), 4);
    }

    // ── Regression: every SigilEvent variant must JSON-roundtrip ──
    //
    // The P3 demo blocker (rocky msg #33, 2026-05-29) was a bare `u128` field
    // inside a SigilEvent variant — serde_json::from_slice chokes on bare u128
    // with "u128 is not supported". The fix is `#[serde(with = "u128_str")]`
    // on every u128 field. This test exercises every variant with a non-zero
    // u128 value and asserts the byte-roundtrip is identity, so any future
    // variant that forgets the attribute breaks the build.
    #[test]
    fn every_event_variant_json_roundtrips_u128_safely() {
        let w1: WalletId = [1u8; 32];
        let w2: WalletId = [2u8; 32];
        let tok: TokenId = [0u8; 32];
        let pool: PoolId = [3u8; 32];
        let validator: ValidatorId = [4u8; 32];
        let contract: ContractId = [5u8; 32];

        // Pick values that exceed 2^53 so any silent f64 demotion would break
        // round-trip — catches both the bare-u128 bug AND a hypothetical
        // arbitrary_precision-via-Number drift.
        let big: u128 = 1_000_000_000_000_000_000_000_u128;

        let events = vec![
            SigilEvent::Send { from: w1, to: w2, amount: big, token: tok, fee: 7 },
            SigilEvent::Receive { from: w1, to: w2, amount: big, token: tok },
            SigilEvent::SwapExecuted {
                pool, in_token: tok, in_amt: big, out_token: tok,
                out_amt: big - 1, slippage_bps: 30, fee_paid: 3,
            },
            SigilEvent::LpDeposited { pool, amt_a: big, amt_b: big, shares_received: big },
            SigilEvent::LpWithdrawn {
                pool, shares_burned: big, amt_a: big, amt_b: big, fees_realized: 9,
            },
            SigilEvent::ContractCall {
                contract, method: [0xCA, 0xFE, 0xBA, 0xBE],
                gas_used: 12345, result_hash: [0u8; 32],
            },
            SigilEvent::ContractDeploy {
                creator: w1, contract_id: contract,
                bytecode_hash: [0u8; 32], gas_used: 6789,
            },
            SigilEvent::MintReward { miner: validator, height: 42, amount: big },
            SigilEvent::TokenDeployed {
                creator: w1, ticker: "TEST".into(), decimals: 18, initial_supply: big,
            },
            SigilEvent::ValidatorJoined { validator, stake: big },
            SigilEvent::ValidatorLeft { validator, refunded_stake: big },
            SigilEvent::ShieldedSend {
                token_hint: tok,
                fee: 1,
                n_inputs: 2,
                n_outputs: 2,
                proof_digest: [0xAA; 32],
                pool_root_at_proof: [0u8; 32],
            },
        ];

        for ev in &events {
            let bytes = serde_json::to_vec(ev)
                .unwrap_or_else(|e| panic!("serialize {:?}: {}", ev, e));
            let parsed: SigilEvent = serde_json::from_slice(&bytes)
                .unwrap_or_else(|e| panic!("deserialize {:?} (bytes: {}): {}", ev, String::from_utf8_lossy(&bytes), e));
            assert_eq!(*ev, parsed, "roundtrip mismatch for {:?}", ev);
        }
    }
}
