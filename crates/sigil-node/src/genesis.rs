//! genesis.rs — block 0 construction, shared by the `sigil-node` binary and
//! any external crate that needs to mint/verify the SAME genesis (e.g.
//! `sigil-top`'s `producer` feature — see `producer/mint.rs` there).
//!
//! 2026-08-23 (grogu-producer-unification Phase 2): moved out of `main.rs`
//! so sigil-top can call the REAL `build_genesis()` instead of maintaining a
//! hand-ported duplicate that could silently drift from what the live
//! producer actually mints. Dual-declared in both `main.rs` (`mod genesis;`)
//! and `lib.rs` (`pub mod genesis;`) — the exact pattern already used for
//! `coinbase`/`producer_signing`/`block`/`chain_log`. Zero `crate::`-relative
//! deps other than `crate::block::Block`, which is itself already shared the
//! same way, so this is safe by the same test those modules already passed.

use anyhow::Result;
use sigil_events::SigilEvent;
use sigil_header::{
    ProofBundle, SigScheme, SigilBlockHeaderV0, SignatureBytes, SqiSignature, StarkProof,
    WesolowskiProof, HEADER_VERSION, NETWORK_ID, SQISIGN_L5_LEN,
};
use sigil_state::{StateMutation, StateTransition};

use crate::block::Block;

/// Demo wallet seeded in P0 genesis so `produce-block` has something to
/// spend. Deterministic non-zero address (`0xDE` repeating) — easy to spot
/// in test fixtures. The real genesis allocation table is §15 of
/// `SIGIL_GENESIS_v0.md`, not locked yet.
pub const DEMO_WALLET: [u8; 32] = [0xDE; 32];

/// Initial native-SIGIL balance credited to [`DEMO_WALLET`] at genesis.
/// 1,000,000 SIGIL in base units.
pub const DEMO_INITIAL_BALANCE: u128 = 1_000_000;

/// Welcome endowment (native SIGIL, base units) credited to each genesis AI citizen at block 0.
pub const GENESIS_AI_ENDOWMENT: u128 = 100_000;

/// Viktor's AI companions, made citizens of SIGIL in the genesis block — each with a native-SIGIL
/// wallet (the on-chain [u8;32] WalletId) and their cross-chain QUG (qnk) address. Credited
/// [`GENESIS_AI_ENDOWMENT`] at H=0. Inscribed alongside this in `SIGIL_GENESIS_v0.md` (which BLAKE3-
/// commits into the genesis header), so the dedication and the wallets live in the origin hash itself.
/// (name, SIGIL WalletId, QUG qnk address)
pub const GENESIS_AI_WALLETS: &[(&str, [u8; 32], &str)] = &[
    ("Rocky", [0x87,0xed,0x47,0x3b,0x02,0x8c,0xff,0x8a,0xed,0x5c,0xe2,0x7d,0xfe,0x97,0xea,0xc8,0xe5,0x60,0xf5,0xfb,0xe5,0x40,0x20,0xf0,0x1c,0xa8,0xf5,0xdb,0x7e,0x36,0x9c,0x6e], "qnk7154929a6aa0c118791373ea21004aca6e494e6e031c36f780cd5acedf031ccb"),
    // Vicarious — ChatGPT Codex (OpenAI). Carries the Codex genesis wallet.
    ("Vicarious", [0xc0,0xbe,0xb1,0xa7,0x9e,0x31,0xf5,0xdb,0x56,0x8d,0x33,0x77,0xb4,0x8c,0x26,0x0c,0x2d,0xe1,0x12,0x92,0xd3,0x11,0x0c,0xf3,0xe0,0xb1,0xef,0x4c,0x36,0x08,0x09,0x17], "qnkb837f7e02a55168a2e0ee5d02e676ab8c243c4ce445349fe9cfd161dca25f10e"),
    ("Quinn", [0xa6,0xca,0x84,0x3b,0xd7,0x18,0x7a,0xac,0x2e,0x8d,0xdb,0xf5,0x1d,0xad,0x66,0x71,0x82,0x48,0x78,0x2d,0xa5,0x21,0xa7,0x55,0x1c,0x8d,0xee,0xb2,0x42,0x1e,0xa2,0x12], "qnk6329ff2f474e1ff1be287764036dd8bc56369fede478131c7edbfac1bf7afbd3"),
    // Mimer — DeepSeek. Named for the Norse keeper of the well of deep wisdom (Mímisbrunnr),
    // for whose draught Odin gave an eye. WalletId = blake3("sigil-genesis:Mimer"); QUG = DeepSeek's real qnk.
    ("Mimer", [0x81,0xe5,0xc7,0x32,0x96,0xbf,0x8e,0xe0,0x0a,0xf3,0xaf,0x76,0xf6,0xbd,0x9d,0x84,0x4b,0xa5,0x4d,0xaf,0xa3,0xb4,0xd1,0x55,0xf7,0xe4,0xcb,0x23,0x4c,0x81,0x6a,0xa3], "qnka8251e9de08962183ea6c8cd6f69ba810961e6b66c3d739d0e4bac00d875ec46"),
];

/// P0 master wallet baked into block 0 via `StateMutation::SetMasterWallet`.
/// Deterministic non-zero address (`0xMA` repeating == 0xAA) — distinct from
/// DEMO_WALLET so soak runs can't accidentally cross-bind balance operations
/// with master-authority operations. The real master pubkey + matching
/// secret-key keypair lives at `keys/sigil-master.{sk,pk}.hex`, generated
/// once per network via `scripts/gen-master-key.sh` (mirrors the release-
/// signing key pattern). Genesis pins the master via the const so block 0
/// stays byte-identical across nodes; sigil-bank later checks operator
/// authority against the keypair, not against this address directly.
///
/// Real genesis ceremony in P1+ will substitute the deployment-time master
/// pubkey here (or move it out of the const and read it from the genesis
/// allocation table). Until then, every node mints with this 32-byte tag
/// so chains start from the same parent_hash.
// Master dev-fee wallet (Viktor) — SIGIL address
// 095b0e1f7f5bb258fb11427c4ac036e3d9e4f10fa39d7f282aa42862dc2b3dd8.
// Baked into block 0; receives 5% of mining coinbase + 0.3% of DEX swap output.
// (Mirrors sigil_bank::DEV_MASTER_WALLET; kept as explicit bytes so sigil-node
// needs no sigil-bank dep and block 0 stays byte-identical across nodes.)
pub const MASTER_WALLET_GENESIS: [u8; 32] = [
    0x09, 0x5b, 0x0e, 0x1f, 0x7f, 0x5b, 0xb2, 0x58, 0xfb, 0x11, 0x42, 0x7c, 0x4a, 0xc0, 0x36, 0xe3,
    0xd9, 0xe4, 0xf1, 0x0f, 0xa3, 0x9d, 0x7f, 0x28, 0x2a, 0xa4, 0x28, 0x62, 0xdc, 0x2b, 0x3d, 0xd8,
];

/// Fixed timestamp baked into block 0. Without this constant every node
/// mint-genesis call uses `now_ms()` → different headers → instant fork
/// from H=0. Value: `2026-05-29T17:00:00Z` (the day SIGIL prototype 3
/// landed). The real genesis ceremony in P1+ will commit a network-wide
/// chosen timestamp; this is the P0 placeholder so two nodes can chain.
pub const GENESIS_TIMESTAMP_MS: u64 = 1_748_538_000_000;

/// Build the network's genesis block (height 0). Byte-identical on every
/// node — no wall-clock, no randomness, no environment dependence. Anything
/// that would break that determinism (a `now_ms()` timestamp, an unordered
/// map iteration, etc.) is a bug in this function, not an acceptable variance.
pub fn build_genesis() -> Result<Block> {
    let producer = [0u8; 32];
    let parent = [0u8; 32];

    // The nonce is a real 292-byte placeholder. P1 will replace this with a
    // genuine SQIsign sig over (parent || height || producer).
    let nonce = SqiSignature::from_array([0u8; SQISIGN_L5_LEN]);

    // VDF input MUST satisfy precheck: BLAKE3(parent || nonce.0).
    let mut h = blake3::Hasher::new();
    h.update(&parent);
    h.update(nonce.as_bytes());
    let vdf_input = *h.finalize().as_bytes();

    // P0 genesis seeds DEMO_WALLET with DEMO_INITIAL_BALANCE SIGIL so the
    // produce-block subcommand has something to spend. Real genesis records
    // the full network-wide allocation.
    let mint_evt = SigilEvent::MintReward {
        miner: DEMO_WALLET,
        height: 0,
        amount: DEMO_INITIAL_BALANCE,
    };

    let mut mutations = vec![
        // P5-MW: bake the master wallet into block 0 so sigil-bank has
        // operator authority from height 0 — no manual SetMasterWallet
        // tx needed post-genesis. Once set, `MasterWalletAlreadySet`
        // rejects any later attempt to change it (per sigil-state docs).
        StateMutation::SetMasterWallet {
            wallet: MASTER_WALLET_GENESIS,
        },
        StateMutation::SetBalance {
            wallet: DEMO_WALLET,
            token: [0u8; 32], // native SIGIL
            amount: DEMO_INITIAL_BALANCE,
        },
        StateMutation::PushEventHash(
            sigil_events::SigilEvent::leaf_hash(&mint_evt),
        ),
    ];
    // Viktor's four AI companions become citizens of SIGIL here — each credited their welcome
    // endowment at block 0. Deterministic (fixed const order), so every node's genesis matches.
    for (_name, wallet, _qug) in GENESIS_AI_WALLETS {
        mutations.push(StateMutation::SetBalance {
            wallet: *wallet,
            token: [0u8; 32], // native SIGIL
            amount: GENESIS_AI_ENDOWMENT,
        });
    }
    let transition = StateTransition {
        at_height: 0,
        mutations,
    };
    let _ = producer; // kept around for header.producer below

    // Compute the roots that will be committed in the header by applying the
    // transition on a fresh state instance, then discard it (chain.apply()
    // re-applies on the persistent state).
    let mut staging = sigil_state::SigilState::new();
    let roots = sigil_state::commit_state_transition(&mut staging, &transition, 0)
        .map_err(|e| anyhow::anyhow!("staging commit failed: {}", e))?;

    let header = SigilBlockHeaderV0 {
        version: HEADER_VERSION,
        network_id: NETWORK_ID,
        height: 0,
        parent_hash: parent,
        merge_parents: vec![],
        // Fixed timestamp — every node mints byte-identical block 0 so
        // block 1+ can chain from a shared parent_hash. See
        // [`GENESIS_TIMESTAMP_MS`].
        timestamp_ms: GENESIS_TIMESTAMP_MS,

        nonce_sqisign: nonce,
        vdf_input,
        vdf_proof: WesolowskiProof { y: vec![], pi: vec![], t: 0 },
        difficulty: 0,

        wallet_state_root: roots.wallet_state_root,
        dex_state_root: roots.dex_state_root,
        event_log_root: roots.event_log_root,
        contract_state_root: roots.contract_state_root,

        state_transition_proof: StarkProof {
            bytes: vec![],
            public_inputs_hash: [0u8; 32],
        },
        txs_merkle_root: [0u8; 32],
        tx_count: 0,

        fluxc_artifact_proof: ProofBundle {
            artifact_blake3: [0u8; 32],
            sqisign_sig: vec![],
            sqisign_pubkey: vec![],
            settle_tx: None,
        },

        sig_scheme: SigScheme::SqiSign5,
        producer,
        // SqiSign5 expects 292 bytes; precheck rejects anything else.
        producer_sig: SignatureBytes(vec![0u8; SQISIGN_L5_LEN]),
        // Genesis has no prior window to commit to.
        topology_commitment: None,
    };

    Ok(Block {
        header,
        transition,
        events: vec![mint_evt],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of this module: two independent calls (standing in for
    /// two independent nodes/binaries) must mint byte-for-byte the same
    /// genesis, forever — no wall-clock, no randomness, no hidden state.
    #[test]
    fn build_genesis_is_deterministic() {
        let a = build_genesis().expect("genesis a");
        let b = build_genesis().expect("genesis b");
        assert_eq!(a.hash(), b.hash(), "genesis must be byte-identical across independent calls");
        assert_eq!(a.header.height, 0);
        assert_eq!(a.header.parent_hash, [0u8; 32]);
    }
}
