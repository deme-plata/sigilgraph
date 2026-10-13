//! v7 WALLET (2026-10-12): the 256-bit twin of [`crate::wallet`] — keys, a note store,
//! deposits, scanning and spend building for [`crate::note_v7`] notes.
//!
//! Same seed, different keys: every v7 secret is derived under its own domain, so a wallet
//! that has both generations never shares a key between them. The shapes mirror v1 on
//! purpose (`OwnedNoteV7`, `NoteStoreV7`, `shield_note_v7`, `build_spend_v7`) so the wasm,
//! Android and sigil-top call sites change their types, not their logic.
//!
//! What this does NOT do: talk to a node, or decide which generation to use — the caller
//! reads `/v1/shielded/anchor` (`v7` present ⇒ deposit here) and keeps v1 notes in the v1
//! store until they are spent or moved.

use std::collections::BTreeSet;

use winterfell::math::{fields::f64::BaseElement, FieldElement, StarkField};

use crate::note_cipher::{self, CipherError, NoteCiphertext, NotePlaintextV7};
use crate::note_v7::{padding_leaf, Digest4, NoteV7, NoteV7Error, PoolTreeV7, RANGE_BITS};
use crate::spend_full_v7::{build_spend_full_v7_trace, v7_options, OutputV7, SpendFullV7Prover, N_OUTS};
use crate::spend_full_v7_2::{build_spend_full_v7x2_trace, SpendFullV7x2Prover, N_INS};
use crate::wallet::{derive, output_derivation_index};
use crate::zk_mask::HidingProver;

/// Four limbs under one domain, each limb its own sub-domain.
fn derive4(seed: &[u8; 32], domain: &str, index: u64) -> [BaseElement; 4] {
    [0u8, 1, 2, 3].map(|k| derive(seed, &format!("v7-{domain}-{k}"), index))
}

fn limbs(e: &[BaseElement; 4]) -> [u64; 4] {
    e.map(|x| x.as_int())
}

/// The epoch-scoped spent-set key — MUST equal `sigil_state::shielded::scoped_nullifier`
/// (pinned by a test in that crate). Epoch 0 is the identity.
pub fn scoped_nullifier_v7(epoch: u32, nf: [u8; 32]) -> [u8; 32] {
    if epoch == 0 {
        return nf;
    }
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-shielded-nullifier-epoch-v1");
    h.update(&epoch.to_le_bytes());
    h.update(&nf);
    *h.finalize().as_bytes()
}

/// A v7 account: everything derives from the 32-byte seed.
#[derive(Clone)]
pub struct AccountV7 {
    seed: [u8; 32],
}

impl AccountV7 {
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self { seed }
    }
    /// The 256-bit spend key.
    pub fn spend_key(&self) -> [BaseElement; 4] {
        derive4(&self.seed, "spend-key", 0)
    }
    /// The 256-bit blinding of the self-made note at `index`.
    pub fn blinding(&self, index: u64) -> [BaseElement; 4] {
        derive4(&self.seed, "blinding", index)
    }
    /// The owner key senders commit notes to (`H(DOM_PK ‖ spend_key)`), wire = 32 bytes.
    pub fn owner_pk(&self) -> Digest4 {
        NoteV7 { value: BaseElement::ZERO, blinding: [BaseElement::ZERO; 4], spend_key: self.spend_key() }.owner_pk()
    }
    pub fn note(&self, index: u64, value: u64) -> Result<NoteV7, NoteV7Error> {
        NoteV7::new(value, limbs(&self.blinding(index)), limbs(&self.spend_key()))
    }
    pub fn note_with_blinding(&self, blinding: [BaseElement; 4], value: u64) -> Result<NoteV7, NoteV7Error> {
        NoteV7::new(value, limbs(&blinding), limbs(&self.spend_key()))
    }
    /// The nullifier this account reveals when it spends the note at `position`.
    pub fn nullifier_at(&self, position: u64) -> Digest4 {
        NoteV7 { value: BaseElement::ZERO, blinding: [BaseElement::ZERO; 4], spend_key: self.spend_key() }.nullifier(position)
    }
    /// The delivery identity — the SAME X25519 key a v1 address publishes.
    pub fn enc_identity(&self) -> flux_swarm_secret::SecretIdentity {
        note_cipher::enc_identity_from_seed(&self.seed)
    }
    pub fn pk_enc_hex(&self) -> String {
        crate::wallet::ShieldedAccount::from_seed(self.seed).address(&self.seed).pk_enc
    }
}

#[derive(Clone, Debug)]
pub struct OwnedNoteV7 {
    pub index: Option<u64>,
    pub value: u64,
    pub blinding: [BaseElement; 4],
    pub position: Option<u64>,
    pub spent: bool,
    pub memo: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct NoteStoreV7 {
    pub notes: Vec<OwnedNoteV7>,
    next_index: u64,
    seen_deliveries: BTreeSet<[u8; 32]>,
}

impl NoteStoreV7 {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_next_index(base: u64) -> Self {
        Self { next_index: base, ..Default::default() }
    }
    /// Book the next derivation index for a self-made note worth `value`.
    pub fn allocate_with(&mut self, account: &AccountV7, value: u64) -> u64 {
        let index = self.next_index;
        self.next_index += 1;
        self.notes.push(OwnedNoteV7 { index: Some(index), value, blinding: account.blinding(index), position: None, spent: false, memo: None });
        index
    }
    pub fn record_derived(&mut self, index: u64, value: u64, blinding: [BaseElement; 4]) {
        self.notes.push(OwnedNoteV7 { index: Some(index), value, blinding, position: None, spent: false, memo: None });
    }
    /// Trial-decrypt v7 ciphertexts; each one that opens is a note paid to us.
    pub fn scan_ciphertexts(&mut self, enc_id: &flux_swarm_secret::SecretIdentity, ciphertexts: &[NoteCiphertext]) -> usize {
        let mut found = 0;
        for ct in ciphertexts {
            if let Ok(pt) = note_cipher::try_open_note_v7(ct, enc_id) {
                let id = *blake3::hash(ct.0.as_bytes()).as_bytes();
                if !self.seen_deliveries.insert(id) {
                    continue;
                }
                let memo = (!pt.memo.is_empty()).then(|| pt.memo.text());
                self.notes.push(OwnedNoteV7 { index: None, value: pt.value, blinding: pt.blinding.0, position: None, spent: false, memo });
                found += 1;
            }
        }
        found
    }
    /// Resolve leaf positions for notes whose `(value, blinding)` we know, against the v7
    /// epoch's real leaves (unpadded, in position order).
    pub fn scan_owned(&mut self, account: &AccountV7, leaves: &[[u8; 32]]) -> usize {
        let mut claimed: BTreeSet<u64> = self.notes.iter().filter_map(|n| n.position).collect();
        let mut found = 0;
        for n in self.notes.iter_mut().filter(|n| n.position.is_none()) {
            let Ok(note) = account.note_with_blinding(n.blinding, n.value) else { continue };
            let cm = note.commitment().to_wire();
            let hit = leaves.iter().enumerate().find(|(i, c)| **c == cm && !claimed.contains(&(*i as u64))).map(|(i, _)| i as u64);
            if let Some(pos) = hit {
                n.position = Some(pos);
                claimed.insert(pos);
                found += 1;
            }
        }
        found
    }
    /// Mark notes spent against the chain's spent set (`/v1/shielded/nullifiers`), which
    /// holds EPOCH-SCOPED keys; `epoch` is the v7 epoch these notes live in.
    pub fn mark_spent(&mut self, account: &AccountV7, spent_scoped: &BTreeSet<[u8; 32]>, epoch: u32) -> usize {
        let mut newly = 0;
        for n in self.notes.iter_mut() {
            if n.spent {
                continue;
            }
            let Some(pos) = n.position else { continue };
            if spent_scoped.contains(&scoped_nullifier_v7(epoch, account.nullifier_at(pos).to_wire())) {
                n.spent = true;
                newly += 1;
            }
        }
        newly
    }
    pub fn balance(&self) -> u128 {
        self.notes.iter().filter(|n| n.position.is_some() && !n.spent).map(|n| n.value as u128).sum()
    }
    pub fn select(&self, at_least: u64) -> Option<&OwnedNoteV7> {
        self.notes.iter().find(|n| n.position.is_some() && !n.spent && n.value >= at_least)
    }
}

/// What a v7 deposit sends to `/v1/shield`: the commitment and its opening.
#[derive(Clone, Debug)]
pub struct ShieldV7 {
    pub index: u64,
    pub cm: [u8; 32],
    pub blinding: [u8; 32],
    pub pk_shield: [u8; 32],
}

/// Book a note and produce the deposit the door expects.
pub fn shield_note_v7(account: &AccountV7, store: &mut NoteStoreV7, value: u64) -> Result<ShieldV7, NoteV7Error> {
    let index = store.allocate_with(account, value);
    let note = account.note(index, value)?;
    Ok(ShieldV7 { index, cm: note.commitment().to_wire(), blinding: Digest4(note.blinding).to_wire(), pk_shield: note.owner_pk().to_wire() })
}

#[derive(Clone, Debug)]
pub struct SpendBundleV7 {
    pub anchor: [u8; 32],
    pub nullifier: [u8; 32],
    /// The second input's nullifier for a merge (`build_spend_2_v7`); empty for a single spend.
    pub extra_nullifiers: Vec<[u8; 32]>,
    pub cm_outs: Vec<[u8; 32]>,
    pub public_value: u128,
    pub proof: Vec<u8>,
    /// Derivation indices of outputs paid back to this account (change).
    pub out_indices: Vec<u64>,
    /// `(value, blinding)` per output, so the caller can seal them to their recipients.
    pub out_preimages: Vec<(u64, [BaseElement; 4])>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpendBuildErrorV7 {
    #[error("expected {expected} outputs, got {got}")]
    WrongOutputCount { expected: usize, got: usize },
    #[error("no unspent note at that store slot")]
    NoSuitableNote,
    #[error("the note has not landed on chain (no position)")]
    NoteNotOnChain,
    #[error("outputs must sum to {expected} (note value minus public value), got {got}")]
    NonConserving { expected: u64, got: u64 },
    #[error("the pool's leaf at the note's position is not this note")]
    PositionMismatch,
    #[error("both inputs name the same on-chain note")]
    SameNoteTwice,
    #[error("amount out of range (2^{RANGE_BITS})")]
    OutOfRange,
    #[error(transparent)]
    Note(#[from] NoteV7Error),
}

/// Prove a v7 spend of the note at `store_slot` against the pool's v7 epoch (`leaves` =
/// the real leaves in position order; padded to `capacity` here). `outs_spec` names the
/// recipient owner key per output — this account's own key keeps change, which is booked
/// in the store under a spend-derived index (never a reusable counter).
pub fn build_spend_v7(
    account: &AccountV7,
    store: &mut NoteStoreV7,
    leaves: &[[u8; 32]],
    capacity: usize,
    store_slot: usize,
    public_value: u64,
    outs_spec: &[(u64, Digest4)],
) -> Result<SpendBundleV7, SpendBuildErrorV7> {
    if outs_spec.len() != N_OUTS {
        return Err(SpendBuildErrorV7::WrongOutputCount { expected: N_OUTS, got: outs_spec.len() });
    }
    let owned = store.notes.get(store_slot).filter(|n| !n.spent).cloned().ok_or(SpendBuildErrorV7::NoSuitableNote)?;
    let position = owned.position.ok_or(SpendBuildErrorV7::NoteNotOnChain)?;
    let sum: u64 = outs_spec.iter().map(|(v, _)| *v).sum();
    let expected = owned.value.saturating_sub(public_value);
    if sum != expected {
        return Err(SpendBuildErrorV7::NonConserving { expected, got: sum });
    }
    if outs_spec.iter().any(|(v, _)| *v >= (1u64 << RANGE_BITS)) || public_value >= (1u64 << RANGE_BITS) {
        return Err(SpendBuildErrorV7::OutOfRange);
    }
    let note = account.note_with_blinding(owned.blinding, owned.value)?;
    let mut digests: Vec<Digest4> = leaves.iter().map(|c| Digest4::from_wire(c).unwrap_or_else(|_| padding_leaf())).collect();
    if digests.len() > capacity {
        return Err(SpendBuildErrorV7::PositionMismatch);
    }
    digests.resize(capacity, padding_leaf());
    let tree = PoolTreeV7::new(digests)?;
    let path = tree.path(position as usize)?;
    if path.leaf != note.commitment() {
        return Err(SpendBuildErrorV7::PositionMismatch);
    }
    let anchor = tree.root().to_wire();
    let mine = account.owner_pk();
    let mut outs = Vec::with_capacity(N_OUTS);
    let mut out_indices = Vec::new();
    let mut out_preimages = Vec::with_capacity(N_OUTS);
    for (i, (v, recipient)) in outs_spec.iter().enumerate() {
        let idx = output_derivation_index(&anchor, position, i);
        let blinding = account.blinding(idx);
        if *recipient == mine {
            store.record_derived(idx, *v, blinding);
            out_indices.push(idx);
        }
        out_preimages.push((*v, blinding));
        outs.push(OutputV7 { value: BaseElement::new(*v), blinding, recipient_pk: *recipient });
    }
    let outs: [OutputV7; N_OUTS] = [outs[0], outs[1]];
    let trace = build_spend_full_v7_trace(&note, &path, BaseElement::new(public_value), &outs, &v7_options());
    let proof = SpendFullV7Prover::new(v7_options()).prove_hiding(trace).expect("a conserving, in-range witness must prove and hide");
    Ok(SpendBundleV7 {
        anchor,
        nullifier: note.nullifier(position).to_wire(),
        extra_nullifiers: Vec::new(),
        cm_outs: outs.iter().map(|o| o.commitment().to_wire()).collect(),
        public_value: public_value as u128,
        proof: proof.to_bytes(),
        out_indices,
        out_preimages,
    })
}

/// Prove a v7 MERGE: TWO of this account's notes (store slots `slots`) against the pool's v7
/// epoch, two outputs as in [`build_spend_v7`]. The transaction that raises a wallet's
/// single-payment ceiling — `spend_full_v7_2`. Both inputs must be different landed notes.
pub fn build_spend_2_v7(
    account: &AccountV7,
    store: &mut NoteStoreV7,
    leaves: &[[u8; 32]],
    capacity: usize,
    slots: [usize; N_INS],
    public_value: u64,
    outs_spec: &[(u64, Digest4)],
) -> Result<SpendBundleV7, SpendBuildErrorV7> {
    if outs_spec.len() != N_OUTS {
        return Err(SpendBuildErrorV7::WrongOutputCount { expected: N_OUTS, got: outs_spec.len() });
    }
    if slots[0] == slots[1] {
        return Err(SpendBuildErrorV7::SameNoteTwice);
    }
    let mut owned = Vec::with_capacity(N_INS);
    let mut positions = [0u64; N_INS];
    for (i, slot) in slots.iter().enumerate() {
        let n = store.notes.get(*slot).filter(|n| !n.spent).cloned().ok_or(SpendBuildErrorV7::NoSuitableNote)?;
        positions[i] = n.position.ok_or(SpendBuildErrorV7::NoteNotOnChain)?;
        owned.push(n);
    }
    if positions[0] == positions[1] {
        return Err(SpendBuildErrorV7::SameNoteTwice);
    }
    let have = owned[0].value.checked_add(owned[1].value).ok_or(SpendBuildErrorV7::OutOfRange)?;
    let sum: u64 = outs_spec.iter().map(|(v, _)| *v).sum();
    let expected = have.saturating_sub(public_value);
    if sum != expected {
        return Err(SpendBuildErrorV7::NonConserving { expected, got: sum });
    }
    if outs_spec.iter().any(|(v, _)| *v >= (1u64 << RANGE_BITS)) || public_value >= (1u64 << RANGE_BITS) {
        return Err(SpendBuildErrorV7::OutOfRange);
    }
    let notes = [
        account.note_with_blinding(owned[0].blinding, owned[0].value)?,
        account.note_with_blinding(owned[1].blinding, owned[1].value)?,
    ];
    let mut digests: Vec<Digest4> = leaves.iter().map(|c| Digest4::from_wire(c).unwrap_or_else(|_| padding_leaf())).collect();
    if digests.len() > capacity {
        return Err(SpendBuildErrorV7::PositionMismatch);
    }
    digests.resize(capacity, padding_leaf());
    let tree = PoolTreeV7::new(digests)?;
    let paths = [tree.path(positions[0] as usize)?, tree.path(positions[1] as usize)?];
    for (p, n) in paths.iter().zip(notes.iter()) {
        if p.leaf != n.commitment() {
            return Err(SpendBuildErrorV7::PositionMismatch);
        }
    }
    let anchor = tree.root().to_wire();
    let mine = account.owner_pk();
    let mut outs = Vec::with_capacity(N_OUTS);
    let mut out_indices = Vec::new();
    let mut out_preimages = Vec::with_capacity(N_OUTS);
    for (i, (v, recipient)) in outs_spec.iter().enumerate() {
        // derived from the FIRST input's position — consumed exactly once, so never reused
        let idx = output_derivation_index(&anchor, positions[0], i);
        let blinding = account.blinding(idx);
        if *recipient == mine {
            store.record_derived(idx, *v, blinding);
            out_indices.push(idx);
        }
        out_preimages.push((*v, blinding));
        outs.push(OutputV7 { value: BaseElement::new(*v), blinding, recipient_pk: *recipient });
    }
    let outs: [OutputV7; N_OUTS] = [outs[0], outs[1]];
    let trace = build_spend_full_v7x2_trace(&notes, &paths, BaseElement::new(public_value), &outs, &v7_options());
    let proof = SpendFullV7x2Prover::new(v7_options()).prove_hiding(trace).expect("a conserving, in-range, two-note witness must prove and hide");
    Ok(SpendBundleV7 {
        anchor,
        nullifier: notes[0].nullifier(positions[0]).to_wire(),
        extra_nullifiers: vec![notes[1].nullifier(positions[1]).to_wire()],
        cm_outs: outs.iter().map(|o| o.commitment().to_wire()).collect(),
        public_value: public_value as u128,
        proof: proof.to_bytes(),
        out_indices,
        out_preimages,
    })
}

/// Seal a v7 output to its recipient's delivery key.
pub fn seal_output_v7(value: u64, blinding: [BaseElement; 4], pk_enc_hex: &str, memo: &str) -> Result<NoteCiphertext, CipherError> {
    note_cipher::seal_note_v7(&NotePlaintextV7::new(value, Digest4(blinding)).with_memo(memo)?, pk_enc_hex)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::note_v7::verify_spend_wire_v7;

    const CAP: usize = 1 << 15;
    const FEE: u64 = 100_000;

    #[test]
    fn derivation_is_deterministic_and_separated_from_v1() {
        let a = AccountV7::from_seed([7u8; 32]);
        let b = AccountV7::from_seed([8u8; 32]);
        assert_eq!(a.spend_key(), AccountV7::from_seed([7u8; 32]).spend_key());
        assert_ne!(a.spend_key(), b.spend_key());
        assert_ne!(a.blinding(0), a.blinding(1));
        assert_ne!(a.spend_key(), a.blinding(0));
        let limbs = a.spend_key();
        assert!(limbs.iter().map(|x| x.as_int()).collect::<BTreeSet<u64>>().len() == 4, "four distinct limbs");
        let v1 = crate::wallet::ShieldedAccount::from_seed([7u8; 32]);
        assert!(!limbs.contains(&v1.spend_key()), "v7 keys never reuse a v1 key");
        assert_eq!(a.owner_pk().to_wire().len(), 32);
    }

    /// Deposit → land → scan → spend (real STARK) → wire verify → change found → spent marked.
    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn deposit_scan_spend_and_change_round_trip() {
        let alice = AccountV7::from_seed([0xA7; 32]);
        let bob = AccountV7::from_seed([0xB7; 32]);
        let mut store = NoteStoreV7::new();
        let dep = shield_note_v7(&alice, &mut store, 5_000_000).unwrap();
        // the opening the door recomputes
        assert_eq!(crate::note_v7::commitment_from_opening_wire(5_000_000, &dep.blinding, &dep.pk_shield).unwrap(), dep.cm);
        // it lands at position 3 of a v7 epoch
        let mut leaves: Vec<[u8; 32]> = (0..3u64).map(|i| NoteV7::new(i + 1, [i; 4], [9, 9, 9, i]).unwrap().commitment().to_wire()).collect();
        leaves.push(dep.cm);
        assert_eq!(store.scan_owned(&alice, &leaves), 1);
        assert_eq!(store.notes[0].position, Some(3));
        assert_eq!(store.balance(), 5_000_000);

        let bundle = build_spend_v7(&alice, &mut store, &leaves, CAP, 0, FEE, &[(2_000_000, bob.owner_pk()), (5_000_000 - 2_000_000 - FEE, alice.owner_pk())]).unwrap();
        verify_spend_wire_v7(&bundle.anchor, &[bundle.nullifier], FEE as u128, &bundle.cm_outs, &bundle.proof).expect("the wallet's spend verifies at the wire");
        assert_eq!(bundle.out_indices.len(), 1, "one change note booked");
        assert_eq!(store.notes.len(), 2);
        assert_eq!(store.notes[1].value, 5_000_000 - 2_000_000 - FEE);
        assert!(store.notes[1].position.is_none());
        // the outputs land; the change is found; the input is marked spent in epoch 1
        leaves.extend_from_slice(&bundle.cm_outs);
        assert_eq!(store.scan_owned(&alice, &leaves), 1, "change note located by its commitment");
        assert_eq!(store.notes[1].position, Some(5));
        let spent: BTreeSet<[u8; 32]> = [scoped_nullifier_v7(1, bundle.nullifier)].into_iter().collect();
        assert_eq!(store.mark_spent(&alice, &spent, 1), 1);
        assert_eq!(store.balance(), (5_000_000 - 2_000_000 - FEE) as u128);
        // a wrong slot / unlanded note are refused before any proving
        assert!(matches!(build_spend_v7(&alice, &mut store, &leaves, CAP, 7, FEE, &[(1, bob.owner_pk()), (1, alice.owner_pk())]), Err(SpendBuildErrorV7::NoSuitableNote)));
    }

    /// MERGE: two landed notes → one payment + change, proven by `spend_full_v7_2`, accepted at
    /// the wire by ARITY; one note twice is refused before proving; both inputs end up spent.
    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn two_notes_merge_into_one_payment_and_verify_at_the_wire() {
        let alice = AccountV7::from_seed([0xA7; 32]);
        let bob = AccountV7::from_seed([0xB7; 32]);
        let mut store = NoteStoreV7::new();
        let d1 = shield_note_v7(&alice, &mut store, 3_000_000).unwrap();
        let d2 = shield_note_v7(&alice, &mut store, 4_000_000).unwrap();
        let mut leaves: Vec<[u8; 32]> = vec![[1u8; 32], d1.cm, [2u8; 32], d2.cm];
        assert_eq!(store.scan_owned(&alice, &leaves), 2);
        // neither note alone covers 6,000,000 + fee; together they do
        assert!(build_spend_v7(&alice, &mut store, &leaves, CAP, 0, FEE, &[(6_000_000, bob.owner_pk()), (0, alice.owner_pk())]).is_err());
        let change = 3_000_000 + 4_000_000 - 6_000_000 - FEE;
        let bundle = build_spend_2_v7(&alice, &mut store, &leaves, CAP, [0, 1], FEE, &[(6_000_000, bob.owner_pk()), (change, alice.owner_pk())]).unwrap();
        assert_eq!(bundle.extra_nullifiers.len(), 1);
        assert_ne!(bundle.nullifier, bundle.extra_nullifiers[0]);
        let nfs = [bundle.nullifier, bundle.extra_nullifiers[0]];
        verify_spend_wire_v7(&bundle.anchor, &nfs, FEE as u128, &bundle.cm_outs, &bundle.proof).expect("the merge verifies at the wire");
        // the same proof declared as a single-input spend is a shape refusal, not a crash
        assert!(verify_spend_wire_v7(&bundle.anchor, &nfs[..1], FEE as u128, &bundle.cm_outs, &bundle.proof).is_err());
        // a repeated nullifier is refused before the proof is parsed
        assert!(matches!(
            verify_spend_wire_v7(&bundle.anchor, &[nfs[0], nfs[0]], FEE as u128, &bundle.cm_outs, &bundle.proof),
            Err(crate::note_v1::WireVerifyError::DuplicateNullifier)
        ));
        // the builder refuses one note twice
        assert!(matches!(build_spend_2_v7(&alice, &mut store, &leaves, CAP, [0, 0], FEE, &[(1, bob.owner_pk()), (1, alice.owner_pk())]), Err(SpendBuildErrorV7::SameNoteTwice)));
        // change lands and is found; both inputs are marked spent under the epoch's keys
        leaves.extend_from_slice(&bundle.cm_outs);
        assert_eq!(store.scan_owned(&alice, &leaves), 1);
        let spent: BTreeSet<[u8; 32]> = nfs.iter().map(|n| scoped_nullifier_v7(1, *n)).collect();
        assert_eq!(store.mark_spent(&alice, &spent, 1), 2);
        assert_eq!(store.balance(), change as u128);
    }

    /// Bob receives: Alice seals the output to Bob's delivery key; Bob's store opens it,
    /// finds its position by his own spend key, and nobody else can open it.
    #[test]
    fn received_v7_note_is_discovered_by_trial_decryption() {
        let alice = AccountV7::from_seed([0xA7; 32]);
        let bob = AccountV7::from_seed([0xB7; 32]);
        let carol = AccountV7::from_seed([0xC7; 32]);
        let blinding = alice.blinding(42);
        let ct = seal_output_v7(777_000, blinding, &bob.pk_enc_hex(), "for the coffee").unwrap();
        let mut bob_store = NoteStoreV7::new();
        assert_eq!(bob_store.scan_ciphertexts(&bob.enc_identity(), &[ct.clone()]), 1);
        assert_eq!(bob_store.notes[0].memo.as_deref(), Some("for the coffee"));
        assert_eq!(bob_store.scan_ciphertexts(&bob.enc_identity(), &[ct.clone()]), 0, "a delivery counts once");
        let mut carol_store = NoteStoreV7::new();
        assert_eq!(carol_store.scan_ciphertexts(&carol.enc_identity(), &[ct.clone()]), 0);
        // the note's commitment, as the chain holds it
        let cm = NoteV7::new(777_000, limbs(&blinding), limbs(&bob.spend_key())).unwrap().commitment().to_wire();
        assert_eq!(bob_store.scan_owned(&bob, &[[1u8; 32], cm]), 1);
        assert_eq!(bob_store.notes[0].position, Some(1));
        // a v1/v2 envelope addressed to Bob is not mistaken for a v7 note
        let v1 = note_cipher::seal_note(&note_cipher::NotePlaintext::new(5, BaseElement::new(5)), &crate::wallet::ShieldedAccount::from_seed([0xB7; 32]).address(&[0xB7; 32])).unwrap();
        assert!(matches!(note_cipher::try_open_note_v7(&v1, &bob.enc_identity()), Err(CipherError::MalformedPlaintext)));
    }
}
