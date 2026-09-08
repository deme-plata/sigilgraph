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
pub mod external;
pub mod intent;
pub mod private_mid;
pub mod receipt;
pub mod solver;

pub use attestation::Attestation;
pub use btc_in::{admit_deposit, BtcPolicy, VerifiedBtcDeposit};
pub use eth_out::Settlement;
pub use external::{verify_external, AssetId, ExternalChain, ExternalPolicy, ExternalProof, ExternalTransition, VerifiedExternalDeposit};
pub use intent::{CrossChainIntent, ExitIntentV1, Privacy};
pub use private_mid::{ExitIntent, PrivateExit, Rate};
pub use receipt::UniversalReceipt;
pub use solver::{solve, ExecutionPlan, Leg, Market, Pool, RouteError};

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
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("no executable route: {0}")]
    Route(String),
    #[error("the vault opened an exit whose memo does not match the intent it is settling ({0})")]
    IntentMismatch(String),
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


/// **v1: execute a [`CrossChainIntent`] end to end, offline.** External proof → hidden
/// note → solver plan → shielded transit with the v1 memo sealed to the vault → consensus
/// verify → vault opens → settlement calldata for the plan's mint leg → universal receipt.
///
/// The DEX leg (if any) is QUOTED here from the market snapshot and recorded in the plan;
/// executing it needs the vault's EVM key and a router call, which this crate does not hold.
#[allow(clippy::too_many_arguments)]
pub fn execute_intent(
    ledger: &mut BridgeLedger,
    proof: &ExternalProof,
    policy: &ExternalPolicy,
    intent: &CrossChainIntent,
    market: &Market,
    now: u64,
    depositor: &ShieldedAccount,
    vault: &ShieldedAccount,
    vault_enc: &flux_swarm_secret::SecretIdentity,
    settlement_contract: [u8; 20],
) -> Result<UniversalReceipt, PipelineError> {
    // ── plan first: an intent with no route must fail before any proof is consumed ──────
    let plan = solve(intent, market, now)?;
    let transit = plan
        .legs
        .iter()
        .find_map(|l| if let Leg::ShieldedTransit { glyphs, fee_glyphs } = l { Some((*glyphs, *fee_glyphs)) } else { None })
        .ok_or_else(|| PipelineError::Route("plan has no shielded transit leg".into()))?;

    // ── leg 1: verified external state in ──────────────────────────────────────────────
    let dep = verify_external(ledger, proof, policy)?;
    if dep.amount != intent.amount_in || dep.asset != intent.input || dep.source != intent.source {
        return Err(PipelineError::IntentMismatch(format!(
            "proof says {} {:?} on {:?}, intent says {} {:?} on {:?}",
            dep.amount, dep.asset, dep.source, intent.amount_in, intent.input, intent.source
        )));
    }
    let btc_view = VerifiedBtcDeposit {
        txid: dep.source_id,
        block_hash: [0u8; 32],
        confirmations: dep.finality,
        sats: dep.amount,
        shield_pk: dep.shield_pk,
        supply_root: dep.supply_root,
    };
    let btc_view = match proof {
        ExternalProof::BitcoinSpv(spv) => VerifiedBtcDeposit { block_hash: spv.block_hash().unwrap_or([0u8; 32]), ..btc_view },
        _ => btc_view,
    };

    // ── leg 2: private SIGIL, v1 memo sealed to the vault ──────────────────────────────
    let mut store = NoteStore::new();
    let (index, deposit_cm) = private_mid::mint_deposit_note(depositor, &mut store, &btc_view, market.rate)?;
    let pool = private_mid::padded_pool(&[deposit_cm]);
    let vault_addr = ShieldedAddress::new(vault.public_key(), &vault_enc.public_hex());
    let memo = intent.exit_memo();
    let exit = private_mid::build_private_exit_memo(
        depositor, &mut store, &pool, index, transit.0, transit.1, &vault_addr, &memo.encode(),
    )?;
    private_mid::verify_private_exit(&exit)?;
    let (opened_value, opened_memo) = private_mid::vault_open_exit_memo(vault, vault_enc, &exit)?;
    let opened = ExitIntentV1::decode(&opened_memo)?;
    if opened != memo || opened.intent_id != plan.intent_id {
        return Err(PipelineError::IntentMismatch("sealed memo does not match the intent".into()));
    }
    if now > opened.deadline {
        return Err(PipelineError::Route(format!("deadline {} passed at settlement ({now})", opened.deadline)));
    }

    // ── leg 3: settlement calldata for the mint leg ────────────────────────────────────
    let wei = plan
        .legs
        .iter()
        .find_map(|l| if let Leg::MintWrapped { wei, .. } = l { Some(*wei) } else { None })
        .ok_or_else(|| PipelineError::Route("plan has no mint leg".into()))?;
    let settlement = Settlement {
        sigil_tx_hash: exit.exit_tx_hash(),
        eth_dest: opened.recipient,
        glyphs: opened_value as u128,
        chain_id: match opened.chain { ExternalChain::Polygon => eth_out::POLYGON_CHAIN_ID, ExternalChain::Ethereum => 1, _ => 0 },
        contract: settlement_contract,
    };
    if settlement.wei() != wei {
        return Err(PipelineError::IntentMismatch(format!("vault note {} wei vs plan {} wei", settlement.wei(), wei)));
    }
    let att = Attestation::seal(&btc_view, deposit_cm, &exit, &settlement);
    Ok(UniversalReceipt::seal(att, plan, dep.amount, dep.asset))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btc_in::synthetic;
    use sigil_shield::note_cipher::enc_identity_from_seed;

    #[test]
    #[cfg_attr(debug_assertions, ignore = "STARK prover: run with --profile release-fast")]
    fn an_intent_executes_end_to_end_and_yields_a_verifiable_universal_receipt() {
        let alice = ShieldedAccount::from_seed([21u8; 32]);
        let vault = ShieldedAccount::from_seed([98u8; 32]);
        let vault_enc = enc_identity_from_seed(&[98u8; 32]);
        let intent = CrossChainIntent {
            source: ExternalChain::Bitcoin, input: AssetId::Btc, amount_in: 250_000,
            destination: ExternalChain::Polygon, output: AssetId::Usdc, min_output: 1,
            recipient: [0xd7; 20], privacy: Privacy::ShieldedTransit, deadline: 2_000_000_000, nonce: 7,
        };
        let market = Market {
            rate: Rate::UNIT,
            transit_fee_glyphs: 1,
            pools: vec![Pool { chain: ExternalChain::Polygon, a: AssetId::WSigil, b: AssetId::Usdc, reserve_a: 1_000_000 * 10u128.pow(18), reserve_b: 1_000_000 * 10u128.pow(6), fee_bps: 30 }],
        };
        let proof = ExternalProof::BitcoinSpv(synthetic::deposit_proof(250_000, private_mid::shield_pk_wire(&alice), 6));
        let r = execute_intent(&mut BridgeLedger::new(), &proof, &ExternalPolicy::synthetic(), &intent, &market, 1_700_000_000, &alice, &vault, &vault_enc, eth_out::WSIGIL3_POLYGON).unwrap();
        assert!(r.verify(), "H0→H1→H2→H3 must close");
        assert_eq!(r.asset_out, AssetId::Usdc);
        assert_eq!(r.amount_in, 250_000);
        assert_eq!(r.plan.legs.len(), 5, "ingress, transit, mint, swap, deliver");
        // tamper: change the claimed output → H3 breaks
        let mut bad = r.clone();
        bad.amount_out = "1".into();
        assert!(!bad.verify());

        // a routable intent that does not describe THIS proof is refused after verification
        let mut i2 = intent.clone(); i2.amount_in = 200_000;
        assert!(matches!(execute_intent(&mut BridgeLedger::new(), &proof, &ExternalPolicy::synthetic(), &i2, &market, 1_700_000_000, &alice, &vault, &vault_enc, eth_out::WSIGIL3_POLYGON), Err(PipelineError::IntentMismatch(_))));
        // an unroutable intent fails BEFORE the proof is consumed: the ledger stays clean
        let mut ledger = BridgeLedger::new();
        let mut i3 = intent.clone(); i3.amount_in = 1;
        assert!(matches!(execute_intent(&mut ledger, &proof, &ExternalPolicy::synthetic(), &i3, &market, 1_700_000_000, &alice, &vault, &vault_enc, eth_out::WSIGIL3_POLYGON), Err(PipelineError::Route(_))));
        assert!(execute_intent(&mut ledger, &proof, &ExternalPolicy::synthetic(), &intent, &market, 1_700_000_000, &alice, &vault, &vault_enc, eth_out::WSIGIL3_POLYGON).is_ok(), "the proof was not burned by the failed attempt");
        // and the same proof cannot execute twice against one ledger (replay guard)
        assert!(matches!(execute_intent(&mut ledger, &proof, &ExternalPolicy::synthetic(), &intent, &market, 1_700_000_000, &alice, &vault, &vault_enc, eth_out::WSIGIL3_POLYGON), Err(PipelineError::Btc(sigil_bridge::BridgeError::ReplayedDeposit))));
    }
}
