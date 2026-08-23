//! SHIELDED WALLET (PV-1 step 5, 2026-08-23) — key derivation, note tracking, proving.
//!
//! What a wallet must do to use the shielded pool, and this module provides:
//!
//!   1. derive a spend key from a seed, deterministically, so notes survive a reinstall;
//!   2. know which notes it owns and at what leaf positions;
//!   3. detect which of its notes have been spent;
//!   4. build the STARK a spend requires.
//!
//! # The receiving gap, stated plainly
//!
//! A note commitment is `compress2(value, blinding)`. It is a *hiding* commitment — that
//! is the point — so an observer, including the intended recipient, cannot read `value` or
//! `blinding` out of it. A wallet can therefore only recognise notes whose preimage it
//! already knows: the ones it created itself.
//!
//! That covers self-custody completely — shield in, hold privately, spend to your own
//! change notes, unshield out — and **it does not cover receiving from someone else**.
//! For that, the sender must transmit `(value, blinding)` to the recipient over a channel
//! the chain does not read: in Zcash this is a note ciphertext encrypted to the
//! recipient's viewing key and carried in the block, which the wallet trial-decrypts.
//!
//! **SIGIL has no note-ciphertext layer yet.** Until it does, a shielded send to a third
//! party is only spendable if the sender passes the blinding out of band. This module
//! therefore exposes a local [`NoteStore`] — what this wallet knows it created — rather
//! than a chain scanner, and that is a deliberate limit rather than an oversight. See
//! [`NoteStore::scan_owned`] for the honest bound on what scanning can and cannot find.
//!
//! # Determinism
//!
//! Blindings come from `BLAKE3(seed ‖ domain ‖ index)`, so the entire note history is
//! recoverable from the seed plus the note *values* — the values are the one thing that
//! must be backed up alongside it, because they cannot be recovered from the chain.

use std::collections::BTreeSet;

use winterfell::math::fields::f64::BaseElement;
use winterfell::math::FieldElement;
use winterfell::Prover;

use crate::mimc::{compress2, mimc_options};
use crate::note_v1::{from_wire, to_wire, Note, NoteError, ShieldedPoolTree, RANGE_BITS};
use crate::spend_full_v2::{
    build_spend_full_v2_trace, SpendFullV2Prover, N_OUTS,
};

/// Reduce 32 bytes to a Goldilocks element, rejecting nothing (always canonical).
///
/// Takes the low 8 bytes modulo p. Uniform enough for a blinding — its job is to make the
/// commitment hiding, and any value with ~64 bits of entropy does that.
fn field_from_bytes(b: &[u8; 32]) -> BaseElement {
    let mut lo = [0u8; 8];
    lo.copy_from_slice(&b[..8]);
    BaseElement::new(u64::from_le_bytes(lo) % 0xFFFF_FFFF_0000_0001)
}

fn derive(seed: &[u8; 32], domain: &str, index: u64) -> BaseElement {
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-shielded-wallet-v1");
    h.update(domain.as_bytes());
    h.update(seed);
    h.update(&index.to_le_bytes());
    field_from_bytes(h.finalize().as_bytes())
}

/// A shielded account: one seed, from which every key and blinding descends.
#[derive(Clone)]
pub struct ShieldedAccount {
    seed: [u8; 32],
}

impl ShieldedAccount {
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self { seed }
    }

    /// The spend key. Every nullifier this account can produce derives from it, so
    /// leaking it means leaking the account's whole spend history.
    pub fn spend_key(&self) -> BaseElement {
        derive(&self.seed, "spend-key", 0)
    }

    /// The blinding for the note at `index`. Deterministic, so notes survive a reinstall
    /// from the seed alone.
    pub fn blinding(&self, index: u64) -> BaseElement {
        derive(&self.seed, "blinding", index)
    }

    /// Construct the note this account would create at `index` holding `value`.
    pub fn note(&self, index: u64, value: u64) -> Result<Note, NoteError> {
        if value >= (1u64 << RANGE_BITS) {
            return Err(NoteError::AmountOutOfRange { got: value, bits: RANGE_BITS as u32 });
        }
        Ok(Note {
            value: BaseElement::new(value),
            blinding: self.blinding(index),
            spend_key: self.spend_key(),
        })
    }

    /// The nullifier revealed when spending the note at leaf `position`.
    pub fn nullifier_at(&self, position: u64) -> BaseElement {
        compress2(self.spend_key(), BaseElement::new(position))
    }
}

/// One note this wallet believes it owns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedNote {
    /// Derivation index — which blinding this note uses.
    pub index: u64,
    pub value: u64,
    /// Leaf position in the pool, once the note has actually landed on chain.
    pub position: Option<u64>,
    pub spent: bool,
}

/// The wallet's local record of its own notes.
///
/// This is authoritative for *this* wallet and reconstructible from the seed plus the
/// values. It is NOT a chain scanner — see the module docs on the receiving gap.
#[derive(Clone, Debug, Default)]
pub struct NoteStore {
    pub notes: Vec<OwnedNote>,
    next_index: u64,
}

impl NoteStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a note this wallet is about to create, returning its derivation index.
    pub fn allocate(&mut self, value: u64) -> u64 {
        let index = self.next_index;
        self.next_index += 1;
        self.notes.push(OwnedNote { index, value, position: None, spent: false });
        index
    }

    /// Locate this wallet's notes in the on-chain commitment list and record their leaf
    /// positions.
    ///
    /// Only finds notes whose `(value, blinding)` this wallet already knows — i.e. ones it
    /// created. A note someone else sent to this wallet is invisible here, because its
    /// commitment is hiding and no ciphertext carries the preimage. Returns how many
    /// positions were newly resolved.
    pub fn scan_owned(
        &mut self,
        account: &ShieldedAccount,
        pool_commitments: &[[u8; 32]],
    ) -> usize {
        let mut found = 0;
        for n in self.notes.iter_mut().filter(|n| n.position.is_none()) {
            let Ok(note) = account.note(n.index, n.value) else { continue };
            let cm = to_wire(note.commitment());
            if let Some(pos) = pool_commitments.iter().position(|c| *c == cm) {
                n.position = Some(pos as u64);
                found += 1;
            }
        }
        found
    }

    /// Mark notes spent by checking their nullifiers against the chain's spent set.
    ///
    /// This is how a wallet notices a spend it did not initiate on this device — the
    /// nullifier is public, so the chain tells us, without telling anyone else which note
    /// it belonged to.
    pub fn mark_spent(
        &mut self,
        account: &ShieldedAccount,
        spent_nullifiers: &BTreeSet<[u8; 32]>,
    ) -> usize {
        let mut newly = 0;
        for n in self.notes.iter_mut() {
            if n.spent {
                continue;
            }
            let Some(pos) = n.position else { continue };
            if spent_nullifiers.contains(&to_wire(account.nullifier_at(pos))) {
                n.spent = true;
                newly += 1;
            }
        }
        newly
    }

    /// Total spendable value: notes that have landed and are not yet spent.
    pub fn balance(&self) -> u128 {
        self.notes
            .iter()
            .filter(|n| n.position.is_some() && !n.spent)
            .map(|n| n.value as u128)
            .sum()
    }

    /// The first unspent, landed note covering `at_least`.
    pub fn select(&self, at_least: u64) -> Option<&OwnedNote> {
        self.notes
            .iter()
            .find(|n| n.position.is_some() && !n.spent && n.value >= at_least)
    }
}

/// Everything the API needs to submit a shielded spend.
#[derive(Clone, Debug)]
pub struct SpendBundle {
    pub anchor: [u8; 32],
    pub nullifier: [u8; 32],
    pub cm_outs: Vec<[u8; 32]>,
    /// The public value: a fee for a shielded send, or the withdrawn amount for an
    /// unshield — the circuit treats them identically.
    pub public_value: u128,
    pub proof: Vec<u8>,
    /// Derivation indices of the change notes, so the caller can record them.
    pub out_indices: Vec<u64>,
}

/// Errors from building a spend.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpendBuildError {
    #[error("no unspent note covering {needed}")]
    NoSuitableNote { needed: u64 },
    #[error("note has not landed on chain yet (no leaf position)")]
    NoteNotOnChain,
    #[error("outputs must total exactly value - public_value ({expected}), got {got}")]
    NonConserving { expected: u64, got: u64 },
    #[error("expected {expected} outputs, got {got}")]
    WrongOutputCount { expected: usize, got: usize },
    #[error("the pool's leaves do not contain this note at its recorded position")]
    PositionMismatch,
    #[error(transparent)]
    Note(#[from] NoteError),
}

/// Build and prove a shielded spend.
///
/// `pool_commitments` must be the FULL padded leaf set the chain is anchored on — the
/// proof is against a specific tree, so a wallet working from a stale or differently
/// padded view will produce a proof that cannot verify. `out_values` must total
/// `note.value - public_value` exactly; the circuit enforces this and would reject
/// otherwise, but failing here gives a legible error instead of an opaque rejection.
#[allow(clippy::too_many_arguments)]
pub fn build_spend(
    account: &ShieldedAccount,
    store: &mut NoteStore,
    pool_commitments: &[[u8; 32]],
    note_index: u64,
    public_value: u64,
    out_values: &[u64],
) -> Result<SpendBundle, SpendBuildError> {
    if out_values.len() != N_OUTS {
        return Err(SpendBuildError::WrongOutputCount {
            expected: N_OUTS,
            got: out_values.len(),
        });
    }

    let owned = store
        .notes
        .iter()
        .find(|n| n.index == note_index && !n.spent)
        .cloned()
        .ok_or(SpendBuildError::NoSuitableNote { needed: public_value })?;
    let position = owned.position.ok_or(SpendBuildError::NoteNotOnChain)?;

    let sum: u64 = out_values.iter().sum();
    let expected = owned.value.saturating_sub(public_value);
    if sum != expected {
        return Err(SpendBuildError::NonConserving { expected, got: sum });
    }

    let note = account.note(owned.index, owned.value)?;

    // Rebuild the tree the chain is anchored on and confirm our note really sits where we
    // think it does. Proving against a mismatched position yields an unverifiable proof
    // with no useful error, so check it here.
    let leaves: Vec<BaseElement> = pool_commitments
        .iter()
        .enumerate()
        .map(|(i, c)| from_wire(c).unwrap_or_else(|_| crate::note_v1::padding_leaf(i as u64)))
        .collect();
    let tree = ShieldedPoolTree::new(leaves).map_err(SpendBuildError::Note)?;
    if tree.leaf(position as usize) != Some(note.commitment()) {
        return Err(SpendBuildError::PositionMismatch);
    }

    // Allocate change notes and derive their blindings.
    let mut outs = [(BaseElement::ZERO, BaseElement::ZERO); N_OUTS];
    let mut out_indices = Vec::with_capacity(N_OUTS);
    for (i, v) in out_values.iter().enumerate() {
        let idx = store.allocate(*v);
        out_indices.push(idx);
        outs[i] = (BaseElement::new(*v), account.blinding(idx));
    }

    let path = tree.path(position as usize);
    let trace = build_spend_full_v2_trace(
        note.value,
        note.blinding,
        note.spend_key,
        BaseElement::new(public_value),
        &outs,
        &path,
    );
    let proof = SpendFullV2Prover::new(mimc_options())
        .prove(trace)
        .expect("a conserving, in-range witness must prove");

    Ok(SpendBundle {
        anchor: to_wire(tree.root()),
        nullifier: to_wire(note.nullifier(position)),
        cm_outs: outs.iter().map(|(v, b)| to_wire(compress2(*v, *b))).collect(),
        public_value: public_value as u128,
        proof: proof.to_bytes(),
        out_indices,
    })
}

/// Derivation-index helper for a freshly shielded deposit: allocate the note, return its
/// index and the commitment to publish.
pub fn shield_note(
    account: &ShieldedAccount,
    store: &mut NoteStore,
    value: u64,
) -> Result<(u64, [u8; 32]), NoteError> {
    let index = store.allocate(value);
    let note = account.note(index, value)?;
    Ok((index, to_wire(note.commitment())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::note_v1::padding_leaf;
    use crate::spend_full_v2::{verify_spend_full_v2, SpendFullV2PublicInputs};

    const CAPACITY: usize = 1 << 15;

    fn padded(cms: &[[u8; 32]]) -> Vec<[u8; 32]> {
        let mut v = cms.to_vec();
        for i in v.len()..CAPACITY {
            v.push(to_wire(padding_leaf(i as u64)));
        }
        v
    }

    #[test]
    fn derivation_is_deterministic_and_seed_separated() {
        let a = ShieldedAccount::from_seed([7u8; 32]);
        let b = ShieldedAccount::from_seed([8u8; 32]);
        assert_eq!(a.spend_key(), ShieldedAccount::from_seed([7u8; 32]).spend_key());
        assert_ne!(a.spend_key(), b.spend_key(), "different seeds must not collide");
        assert_ne!(a.blinding(0), a.blinding(1), "indices must separate");
        assert_ne!(a.blinding(0), a.spend_key(), "domains must separate");
    }

    #[test]
    fn scan_finds_own_notes_and_tracks_balance() {
        let acct = ShieldedAccount::from_seed([3u8; 32]);
        let mut store = NoteStore::new();
        let (_i0, cm0) = shield_note(&acct, &mut store, 100).unwrap();
        let (_i1, cm1) = shield_note(&acct, &mut store, 250).unwrap();

        assert_eq!(store.balance(), 0, "nothing has landed yet");
        let pool = padded(&[cm0, cm1]);
        assert_eq!(store.scan_owned(&acct, &pool), 2, "both notes located");
        assert_eq!(store.balance(), 350);
        assert_eq!(store.notes[0].position, Some(0));
        assert_eq!(store.notes[1].position, Some(1));
    }

    /// A note this wallet did NOT create is invisible — the honest limit of scanning
    /// without note ciphertexts. If this ever starts passing, the receiving gap is closed
    /// and the module docs need updating.
    #[test]
    fn scan_cannot_see_a_foreign_note() {
        let mine = ShieldedAccount::from_seed([3u8; 32]);
        let theirs = ShieldedAccount::from_seed([9u8; 32]);
        let mut their_store = NoteStore::new();
        let (_i, their_cm) = shield_note(&theirs, &mut their_store, 500).unwrap();

        let mut my_store = NoteStore::new();
        my_store.allocate(500); // I know a 500 exists; I do not know its blinding
        let pool = padded(&[their_cm]);
        assert_eq!(
            my_store.scan_owned(&mine, &pool),
            0,
            "a note whose blinding we don't hold must stay invisible"
        );
        assert_eq!(my_store.balance(), 0);
    }

    #[test]
    fn mark_spent_uses_public_nullifiers() {
        let acct = ShieldedAccount::from_seed([3u8; 32]);
        let mut store = NoteStore::new();
        let (_i, cm) = shield_note(&acct, &mut store, 100).unwrap();
        store.scan_owned(&acct, &padded(&[cm]));
        assert_eq!(store.balance(), 100);

        let mut spent = BTreeSet::new();
        spent.insert(to_wire(acct.nullifier_at(0)));
        assert_eq!(store.mark_spent(&acct, &spent), 1);
        assert_eq!(store.balance(), 0, "a spent note stops counting");
    }

    /// THE WALLET GATE: a wallet-built spend must verify under the production circuit.
    #[test]
    fn wallet_built_spend_verifies() {
        let acct = ShieldedAccount::from_seed([42u8; 32]);
        let mut store = NoteStore::new();
        let (index, cm) = shield_note(&acct, &mut store, 100).unwrap();
        let pool = padded(&[cm]);
        assert_eq!(store.scan_owned(&acct, &pool), 1);

        // Spend 100 as fee 3 + change 50 + 47.
        let bundle = build_spend(&acct, &mut store, &pool, index, 3, &[50, 47])
            .expect("wallet must build a spend");

        let root = from_wire(&bundle.anchor).unwrap();
        let nf = from_wire(&bundle.nullifier).unwrap();
        let cm_outs = [
            from_wire(&bundle.cm_outs[0]).unwrap(),
            from_wire(&bundle.cm_outs[1]).unwrap(),
        ];
        let proof = winterfell::Proof::from_bytes(&bundle.proof).expect("decode");
        verify_spend_full_v2(
            proof,
            SpendFullV2PublicInputs { root, nf, fee: BaseElement::new(3), cm_outs },
        )
        .expect("SECURITY: a wallet-built spend must verify under the production circuit");

        assert_eq!(bundle.out_indices.len(), 2, "change notes recorded for tracking");
    }

    #[test]
    fn non_conserving_spend_is_refused_with_a_legible_error() {
        let acct = ShieldedAccount::from_seed([42u8; 32]);
        let mut store = NoteStore::new();
        let (index, cm) = shield_note(&acct, &mut store, 100).unwrap();
        let pool = padded(&[cm]);
        store.scan_owned(&acct, &pool);
        let err = build_spend(&acct, &mut store, &pool, index, 3, &[50, 48])
            .expect_err("a non-conserving spend must be refused");
        assert_eq!(err, SpendBuildError::NonConserving { expected: 97, got: 98 });
    }
}
