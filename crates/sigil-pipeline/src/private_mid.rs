//! Leg 2 — **private SIGIL computation**. The admitted BTC deposit becomes a hidden note
//! owned by the shield key the Bitcoin tx named. Value then moves under a real v5 hiding
//! STARK to the exit vault. The Ethereum destination is not on any chain: it is sealed
//! inside the note ciphertext only the vault can open ([`ExitIntent`]).
//!
//! What the chain sees for the whole leg: one nullifier, two hiding commitments, a fee,
//! and a proof that they conserve value. What it does not see: who paid, how much went to
//! the vault versus change, and where on Ethereum it is going.

use sigil_shield::note_cipher::{seal_note, try_open_note, NoteCiphertext, NotePlaintext, ShieldedAddress};
use sigil_shield::note_v1::{padding_leaf, to_wire, verify_spend_wire, Note, RANGE_BITS};
use sigil_shield::wallet::{build_spend, shield_note, NoteStore, ShieldedAccount, SpendBundle};
use winterfell::math::{fields::f64::BaseElement, FieldElement};

use crate::btc_in::VerifiedBtcDeposit;
use crate::eth_out::{eth_address_hex, parse_eth_address};
use crate::PipelineError;

/// The live pool capacity (`/v1/shielded/anchor` → 32,768). Proofs are built over a tree
/// of this depth so the demo exercises the same circuit size the chain verifies.
pub const POOL_CAPACITY: usize = 1 << 15;

/// How many glyphs one satoshi buys. A POLICY input — pinned by the operator or fed by an
/// oracle — never derived here. The pool holds native SIGIL only, so a BTC deposit must
/// become SIGIL-denominated value to enter it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    pub glyphs_per_sat: u64,
}

impl Rate {
    /// 1 sat → 1 glyph. The identity rate the demo uses; not a price.
    pub const UNIT: Rate = Rate { glyphs_per_sat: 1 };

    pub fn glyphs(&self, sats: u64) -> Option<u64> {
        sats.checked_mul(self.glyphs_per_sat).filter(|g| *g < (1u64 << RANGE_BITS))
    }
}

/// The shield public key of an account, in the 32-byte wire form a BTC memo carries.
pub fn shield_pk_wire(account: &ShieldedAccount) -> [u8; 32] {
    to_wire(account.public_key())
}

/// Mint the deposit note. Refuses to mint to any key other than the one the proven BTC tx
/// named — a note minted to a key nobody holds is value burned, and a note minted to a key
/// the depositor did NOT name is theft.
pub fn mint_deposit_note(
    depositor: &ShieldedAccount,
    store: &mut NoteStore,
    dep: &VerifiedBtcDeposit,
    rate: Rate,
) -> Result<(u64, [u8; 32]), PipelineError> {
    let have = shield_pk_wire(depositor);
    if have != dep.shield_pk {
        return Err(PipelineError::WrongShieldKey {
            named: hex::encode(dep.shield_pk),
            have: hex::encode(have),
        });
    }
    let glyphs = rate.glyphs(dep.sats).ok_or(PipelineError::DepositTooLarge { sats: dep.sats })?;
    Ok(shield_note(depositor, store, glyphs)?)
}

/// A pool of `POOL_CAPACITY` leaves: the given commitments first, padding after — the
/// same layout `sigil-state` hands the verifier.
pub fn padded_pool(cms: &[[u8; 32]]) -> Vec<[u8; 32]> {
    let mut v = cms.to_vec();
    for i in v.len()..POOL_CAPACITY {
        v.push(to_wire(padding_leaf(i as u64)));
    }
    v
}

/// Where the settled value should land. Travels ONLY inside the sealed note ciphertext.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitIntent {
    pub chain_id: u64,
    pub eth_dest: [u8; 20],
}

impl ExitIntent {
    pub const MEMO_PREFIX: &'static str = "SIGILX-exit/v0";

    pub fn encode(&self) -> String {
        format!("{}|chain={}|eth={}", Self::MEMO_PREFIX, self.chain_id, eth_address_hex(&self.eth_dest))
    }

    pub fn decode(memo: &str) -> Result<Self, PipelineError> {
        let mut it = memo.trim_end_matches('\0').split('|');
        if it.next() != Some(Self::MEMO_PREFIX) {
            return Err(PipelineError::BadIntent("missing prefix".into()));
        }
        let mut chain_id = None;
        let mut eth = None;
        for part in it {
            if let Some(v) = part.strip_prefix("chain=") {
                chain_id = Some(v.parse::<u64>().map_err(|e| PipelineError::BadIntent(e.to_string()))?);
            } else if let Some(v) = part.strip_prefix("eth=") {
                eth = Some(parse_eth_address(v)?);
            }
        }
        Ok(Self {
            chain_id: chain_id.ok_or_else(|| PipelineError::BadIntent("no chain".into()))?,
            eth_dest: eth.ok_or_else(|| PipelineError::BadIntent("no eth dest".into()))?,
        })
    }
}

/// The private exit: a proven spend of the deposit note into `[to-vault, change]`, plus the
/// ciphertext that lets the vault (and nobody else) open its note and read the intent.
#[derive(Debug, Clone)]
pub struct PrivateExit {
    pub bundle: SpendBundle,
    /// Sealed `(value, blinding, memo=ExitIntent)` for `bundle.cm_outs[0]`.
    pub sealed_to_vault: NoteCiphertext,
    pub fee_glyphs: u64,
}

impl PrivateExit {
    /// The content hash a node would key this transaction on: BLAKE3 over the public
    /// inputs and the proof. This is what becomes the Polygon `lockId`.
    pub fn exit_tx_hash(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"sigil-pipeline/exit/v0");
        h.update(&self.bundle.anchor);
        h.update(&self.bundle.nullifier);
        for nf in &self.bundle.extra_nullifiers {
            h.update(nf);
        }
        for cm in &self.bundle.cm_outs {
            h.update(cm);
        }
        h.update(&self.bundle.public_value.to_le_bytes());
        h.update(&self.bundle.proof);
        *h.finalize().as_bytes()
    }
}

/// Spend the deposit note: `exit_glyphs` to the vault's shield key, the rest back to the
/// depositor, `fee_glyphs` public. The recipient key is a hidden witness in-circuit — paying
/// the vault does not name the vault on chain.
#[allow(clippy::too_many_arguments)]
pub fn build_private_exit(
    depositor: &ShieldedAccount,
    store: &mut NoteStore,
    pool: &[[u8; 32]],
    note_index: u64,
    exit_glyphs: u64,
    fee_glyphs: u64,
    vault: &ShieldedAddress,
    intent: &ExitIntent,
) -> Result<PrivateExit, PipelineError> {
    // Resolve the note's leaf position — the nullifier binds to it.
    if store.scan_owned(depositor, pool) == 0 && store.notes.iter().all(|n| n.position.is_none()) {
        return Err(PipelineError::NoteNotInPool);
    }
    let store_pos = store
        .notes
        .iter()
        .position(|n| n.index == Some(note_index))
        .ok_or(PipelineError::NoteNotInPool)?;
    let total = store.notes[store_pos].value;
    let change = total
        .checked_sub(exit_glyphs)
        .and_then(|c| c.checked_sub(fee_glyphs))
        .ok_or(PipelineError::Spend(sigil_shield::wallet::SpendBuildError::NoSuitableNote { needed: exit_glyphs + fee_glyphs }))?;
    let vault_pk = vault.shield_key().map_err(PipelineError::Cipher)?;
    let bundle = build_spend(
        depositor,
        store,
        pool,
        store_pos,
        fee_glyphs,
        &[(exit_glyphs, vault_pk), (change, depositor.public_key())],
    )
    .map_err(PipelineError::Spend)?;
    let (v0, b0) = bundle.out_preimages[0];
    debug_assert_eq!(v0, exit_glyphs);
    let pt = NotePlaintext::new(v0, b0).with_memo(&intent.encode()).map_err(PipelineError::Cipher)?;
    let sealed_to_vault = seal_note(&pt, vault).map_err(PipelineError::Cipher)?;
    Ok(PrivateExit { bundle, sealed_to_vault, fee_glyphs })
}

/// Verify the exit exactly as consensus does: through `note_v1::verify_spend_wire`.
pub fn verify_private_exit(exit: &PrivateExit) -> Result<(), PipelineError> {
    verify_spend_wire(
        &exit.bundle.anchor,
        &exit.bundle.nullifier,
        exit.bundle.public_value,
        &exit.bundle.cm_outs,
        &exit.bundle.proof,
    )
    .map_err(PipelineError::Verify)
}

/// The vault side: open the ciphertext, recover `(value, blinding)` and the intent, and
/// PROVE the note is ours by recomputing the owner-bound commitment with our spend key.
/// A ciphertext that opens but whose commitment is not in `cm_outs` is someone trying to
/// get the vault to settle value the vault cannot spend.
pub fn vault_open_exit(
    vault: &ShieldedAccount,
    vault_enc: &flux_swarm_secret::SecretIdentity,
    exit: &PrivateExit,
) -> Result<(u64, ExitIntent), PipelineError> {
    let pt = try_open_note(&exit.sealed_to_vault, vault_enc).map_err(PipelineError::Cipher)?;
    let note = Note { value: BaseElement::new(pt.value), blinding: pt.blinding, spend_key: vault.spend_key() };
    let cm = to_wire(note.commitment());
    if !exit.bundle.cm_outs.iter().any(|c| *c == cm) {
        return Err(PipelineError::NotVaultNote);
    }
    let intent = ExitIntent::decode(&pt.memo.text())?;
    Ok((pt.value, intent))
}

#[allow(dead_code)]
fn _field_element_is_used(e: BaseElement) -> BaseElement { e + BaseElement::ONE }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btc_in::{admit_deposit, synthetic, BtcPolicy};
    use sigil_bridge::BridgeLedger;
    use sigil_shield::note_cipher::enc_identity_from_seed;

    #[test]
    fn intent_round_trips_and_rejects_garbage() {
        let i = ExitIntent { chain_id: 137, eth_dest: [0xd7; 20] };
        assert_eq!(ExitIntent::decode(&i.encode()).unwrap(), i);
        assert!(ExitIntent::decode("hello").is_err());
        assert!(ExitIntent::decode("SIGILX-exit/v0|chain=137").is_err());
    }

    #[test]
    fn note_is_minted_only_to_the_key_bitcoin_named() {
        let alice = ShieldedAccount::from_seed([1u8; 32]);
        let mallory = ShieldedAccount::from_seed([2u8; 32]);
        let proof = synthetic::deposit_proof(5_000, shield_pk_wire(&alice), 6);
        let dep = admit_deposit(&mut BridgeLedger::new(), &proof, &BtcPolicy::synthetic()).unwrap();
        let mut store = NoteStore::new();
        assert!(matches!(
            mint_deposit_note(&mallory, &mut store, &dep, Rate::UNIT),
            Err(PipelineError::WrongShieldKey { .. })
        ));
        let (idx, cm) = mint_deposit_note(&alice, &mut store, &dep, Rate::UNIT).unwrap();
        assert_eq!(idx, 0);
        assert_ne!(cm, [0u8; 32]);
        assert_eq!(store.notes[0].value, 5_000);
    }

    #[test]
    fn rate_respects_the_note_range() {
        assert_eq!(Rate::UNIT.glyphs(21_000_000 * 100_000_000), Some(2_100_000_000_000_000));
        assert_eq!(Rate { glyphs_per_sat: 1 << 40 }.glyphs(1 << 20), None, "2^60 is out of the 58-bit range");
    }

    /// THE PRIVATE LEG, END TO END: a real v5 hiding STARK, verified through the consensus
    /// chokepoint, and the vault opening its note. Needs `debug_assertions` OFF (winterfell
    /// 0.9's degree validation trips in debug) — run with `--profile release-fast`.
    #[test]
    #[cfg_attr(debug_assertions, ignore = "STARK prover: run with --profile release-fast")]
    fn deposit_note_moves_privately_to_the_vault_and_only_the_vault_can_read_the_exit() {
        let alice = ShieldedAccount::from_seed([11u8; 32]);
        let vault = ShieldedAccount::from_seed([99u8; 32]);
        let vault_enc = enc_identity_from_seed(&[99u8; 32]);
        let eve_enc = enc_identity_from_seed(&[66u8; 32]);
        let vault_addr = ShieldedAddress::new(vault.public_key(), &vault_enc.public_hex());

        let proof = synthetic::deposit_proof(100, shield_pk_wire(&alice), 6);
        let dep = admit_deposit(&mut BridgeLedger::new(), &proof, &BtcPolicy::synthetic()).unwrap();
        let mut store = NoteStore::new();
        let (idx, cm) = mint_deposit_note(&alice, &mut store, &dep, Rate::UNIT).unwrap();
        let pool = padded_pool(&[cm]);

        let intent = ExitIntent { chain_id: 137, eth_dest: [0xd7; 20] };
        let exit = build_private_exit(&alice, &mut store, &pool, idx, 60, 3, &vault_addr, &intent).unwrap();
        verify_private_exit(&exit).expect("consensus chokepoint must accept the exit");
        assert_eq!(exit.bundle.public_value, 3);
        assert_eq!(exit.bundle.cm_outs.len(), 2);

        // the vault reads value + destination; Eve cannot
        let (v, got) = vault_open_exit(&vault, &vault_enc, &exit).unwrap();
        assert_eq!(v, 60);
        assert_eq!(got, intent);
        assert!(try_open_note(&exit.sealed_to_vault, &eve_enc).is_err());

        // a tampered proof is refused by the same chokepoint
        let mut bad = exit.clone();
        bad.bundle.proof[8] ^= 1;
        assert!(verify_private_exit(&bad).is_err());
    }
}
