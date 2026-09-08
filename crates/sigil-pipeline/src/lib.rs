//! sigil-pipeline — **public Bitcoin → private SIGIL computation → public Ethereum settlement.**
//!
//! Three chains, three trust models, one receipt. Think of it as a tunnel with a lit
//! entrance, a lit exit, and a dark middle:
//!
//! ```text
//!   ┌───────────── PUBLIC ─────────────┐ ┌──────── PRIVATE ────────┐ ┌───────── PUBLIC ─────────┐
//!   │ Bitcoin: a tx pays BTC and names │ │ SIGIL: the deposit is a  │ │ Polygon/Ethereum: the     │
//!   │ a SHIELD KEY in an OP_RETURN     │ │ hidden note. It moves    │ │ vault's settled exit is   │
//!   │ memo. Anyone can verify the SPV  │ │ under a STARK that proves│ │ minted as wSIGIL3 by the  │
//!   │ proof: PoW ‖ prev-link ‖ merkle. │ │ conservation and hides   │ │ contract's operator, keyed│
//!   │                                  │ │ amounts, parties, and    │ │ on the SIGIL tx hash so a │
//!   │ what is public: txid, sats,      │ │ the ETH destination      │ │ mint can never repeat.    │
//!   │ shield pk (unlinkable to ETH)    │ │ (sealed to the vault).   │ │ what is public: to, wei.  │
//!   └──────────────────────────────────┘ └──────────────────────────┘ └───────────────────────────┘
//!            btc_in.rs                          private_mid.rs                    eth_out.rs
//!                              attestation.rs: h0 ─▶ h1 ─▶ h2 (BLAKE3 chain over all three)
//! ```
//!
//! **Why the middle is dark and the ends are lit.** A Bitcoin tx and an Ethereum mint are
//! both public forever. If the BTC memo named the ETH address, the two would be linked on
//! chain and the SIGIL leg would hide nothing. So the BTC tx names only a *shield public
//! key*; the Ethereum destination travels **inside the sealed note ciphertext** to the
//! vault ([`private_mid::ExitIntent`]). Only the vault can open it. The chain sees a
//! nullifier, two hiding commitments, and a proof.
//!
//! # What is REAL here, and what is still pretend
//!
//! Real, exercised by tests through the same code consensus uses:
//! - Bitcoin leg: [`sigil_bridge::SpvProof::verify`] — real dSHA256 PoW on every header,
//!   `prev_block` linkage, confirmation depth, merkle inclusion, plus the mainnet
//!   difficulty floor. The Bitcoin **genesis header** is pinned as a live vector.
//! - SIGIL leg: [`sigil_shield::wallet::build_spend`] produces a **v5 hiding STARK**, and
//!   [`sigil_shield::note_v1::verify_spend_wire`] — the exact chokepoint the node calls —
//!   verifies it. The exit intent is sealed with the production note cipher.
//! - Ethereum leg: the calldata is byte-exact for the deployed `SigilBridgeWrappedG3`
//!   (`mint(address,uint256,uint256)`, selector `0x156e29f6`, lockId = SIGIL tx hash).
//!
//! Pretend (specified, NOT wired — see "wired in is the only done"):
//! - No sigil-node route accepts a BTC SPV proof. `BtcDeposit` is not a `SigilTx` variant
//!   and `commit_state_transition` has no arm for it. Minting a note from BTC is a
//!   **consensus change** and needs an operator decision and a height gate.
//! - The bridge vault does not yet scan the pool for private exits; today's lock is a
//!   transparent `Shield`. The vault-side `vault_open_exit` here is the recogniser it would run.
//! - The relayer that would send the mint is deliberately held (DO-NOT-START drop-in).
//! - `bitcoind` on Epsilon is crash-looping on a corrupt block DB (needs `-reindex-chainstate`),
//!   so no real mainnet proof can be fetched from this box yet.
//! - The BTC→SIGIL **rate** is a policy input ([`private_mid::Rate`]), not an oracle read.

pub mod attestation;
pub mod btc_in;
pub mod eth_out;
pub mod private_mid;

pub use attestation::Attestation;
pub use btc_in::{admit_deposit, BtcPolicy, VerifiedBtcDeposit};
pub use eth_out::Settlement;
pub use private_mid::{ExitIntent, PrivateExit, Rate};

use sigil_bridge::BridgeLedger;
use sigil_shield::note_cipher::ShieldedAddress;
use sigil_shield::wallet::{NoteStore, ShieldedAccount};

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("bitcoin leg: {0}")]
    Btc(#[from] sigil_bridge::BridgeError),
    #[error("bitcoin leg: {0}")]
    Spv(#[from] sigil_bridge::ProofError),
    #[error("deposit of {sats} sats exceeds the note range (2^58 glyphs after conversion)")]
    DepositTooLarge { sats: u64 },
    #[error("the BTC memo names shield key {named}, but the depositor account is {have} — a note minted to the wrong key is value burned")]
    WrongShieldKey { named: String, have: String },
    #[error("shield: {0}")]
    Note(#[from] sigil_shield::note_v1::NoteError),
    #[error("shield: could not build the private spend: {0:?}")]
    Spend(sigil_shield::wallet::SpendBuildError),
    #[error("shield: the private spend does not verify through the consensus chokepoint: {0:?}")]
    Verify(sigil_shield::note_v1::WireVerifyError),
    #[error("cipher: {0:?}")]
    Cipher(sigil_shield::note_cipher::CipherError),
    #[error("exit intent memo is malformed: {0}")]
    BadIntent(String),
    #[error("exit note does not belong to the vault (commitment mismatch) — refusing to settle value the vault cannot spend")]
    NotVaultNote,
    #[error("ethereum leg: {0}")]
    Eth(String),
    #[error("the deposited note never appeared in the pool at any position")]
    NoteNotInPool,
}

/// One end-to-end offline run: verify a BTC deposit, mint the hidden note, move it privately
/// to the vault with the Ethereum destination sealed in, verify the spend the way consensus
/// would, open it as the vault, and encode the exact Polygon mint. Returns the attestation
/// chain binding the three legs. This is what the demo and the end-to-end test drive.
#[allow(clippy::too_many_arguments)]
pub fn run_offline(
    ledger: &mut BridgeLedger,
    proof: &sigil_bridge::SpvProof,
    policy: &BtcPolicy,
    rate: Rate,
    depositor: &ShieldedAccount,
    vault: &ShieldedAccount,
    vault_enc: &flux_swarm_secret::SecretIdentity,
    exit_glyphs: u64,
    fee_glyphs: u64,
    intent: ExitIntent,
    settlement_contract: [u8; 20],
) -> Result<Attestation, PipelineError> {
    // ── leg 1: public Bitcoin ──────────────────────────────────────────────────────────
    let dep = admit_deposit(ledger, proof, policy)?;

    // ── leg 2: private SIGIL ───────────────────────────────────────────────────────────
    let mut store = NoteStore::new();
    let (index, deposit_cm) = private_mid::mint_deposit_note(depositor, &mut store, &dep, rate)?;
    let pool = private_mid::padded_pool(&[deposit_cm]);
    let vault_addr = ShieldedAddress::new(vault.public_key(), &vault_enc.public_hex());
    let exit = private_mid::build_private_exit(
        depositor, &mut store, &pool, index, exit_glyphs, fee_glyphs, &vault_addr, &intent,
    )?;
    private_mid::verify_private_exit(&exit)?;
    let (opened_value, opened_intent) = private_mid::vault_open_exit(vault, vault_enc, &exit)?;

    // ── leg 3: public Ethereum ─────────────────────────────────────────────────────────
    let settlement = Settlement {
        sigil_tx_hash: exit.exit_tx_hash(),
        eth_dest: opened_intent.eth_dest,
        glyphs: opened_value as u128,
        chain_id: opened_intent.chain_id,
        contract: settlement_contract,
    };

    Ok(Attestation::seal(&dep, deposit_cm, &exit, &settlement))
}
