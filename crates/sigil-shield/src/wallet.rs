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
use crate::spend_full_v4::{build_spend_full_v4_trace, SpendFullV4Prover, N_OUTS, PK_DOMAIN};

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

    /// This account's full shielded ADDRESS: the circuit key plus the encryption key a
    /// payer seals the note ciphertext to. This is what a user shares to get paid.
    pub fn address(&self, seed: &[u8; 32]) -> crate::note_cipher::ShieldedAddress {
        let enc = crate::note_cipher::enc_identity_from_seed(seed);
        crate::note_cipher::ShieldedAddress::new(self.public_key(), &enc.public_hex())
    }

    /// This account's PUBLIC key — the address others bind notes to when paying it.
    ///
    /// Safe to publish: `compress2` is one-way, so it never exposes the spend key. This
    /// is what a sender needs (and all they need) to create a note only this account can
    /// spend.
    pub fn public_key(&self) -> BaseElement {
        compress2(self.spend_key(), BaseElement::new(PK_DOMAIN))
    }
}

/// One note this wallet believes it owns.
///
/// The blinding is stored rather than re-derived, because a RECEIVED note's blinding was
/// chosen by the sender and cannot be regenerated from our seed. Self-created notes still
/// derive theirs deterministically; `index` records which derivation produced it, and is
/// `None` for a received note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedNote {
    /// Derivation index for a self-created note; `None` when received from someone else.
    pub index: Option<u64>,
    pub value: u64,
    pub blinding: BaseElement,
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
    pub fn allocate_with(&mut self, account: &ShieldedAccount, value: u64) -> u64 {
        let index = self.next_index;
        self.next_index += 1;
        let blinding = account.blinding(index);
        self.notes.push(OwnedNote {
            index: Some(index),
            value,
            blinding,
            position: None,
            spent: false,
        });
        index
    }

    /// Record a note RECEIVED from someone else, whose blinding came out of a ciphertext.
    ///
    /// Deduplicates on `(value, blinding)`: a wallet re-scans the chain routinely and must
    /// not book the same payment twice.
    pub fn receive(&mut self, value: u64, blinding: BaseElement) -> bool {
        if self.notes.iter().any(|n| n.value == value && n.blinding == blinding) {
            return false;
        }
        self.notes.push(OwnedNote {
            index: None,
            value,
            blinding,
            position: None,
            spent: false,
        });
        true
    }

    /// Scan a batch of published ciphertexts for notes addressed to us, recording each one
    /// we can open. Returns how many NEW notes were found.
    ///
    /// Trial decryption is the whole mechanism: the AEAD tag fails for everyone else, so a
    /// successful open IS the ownership proof. Nothing marks a ciphertext as ours, which
    /// is why an observer cannot tell who was paid.
    pub fn scan_ciphertexts(
        &mut self,
        enc_id: &flux_swarm_secret::SecretIdentity,
        ciphertexts: &[crate::note_cipher::NoteCiphertext],
    ) -> usize {
        let mut found = 0;
        for ct in ciphertexts {
            if let Ok(pt) = crate::note_cipher::try_open_note(ct, enc_id) {
                if self.receive(pt.value, pt.blinding) {
                    found += 1;
                }
            }
        }
        found
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
            let note = Note {
                value: BaseElement::new(n.value),
                blinding: n.blinding,
                spend_key: account.spend_key(),
            };
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
    /// Derivation indices of the outputs bound to THIS account, so the caller can record
    /// them. Outputs paid to someone else are not tracked here — this wallet cannot spend
    /// them and has no business holding their preimages.
    pub out_indices: Vec<u64>,
    /// Per-output `(value, blinding)`, in the same order as `cm_outs`.
    ///
    /// The sender needs these to seal a note ciphertext to each recipient — a payment the
    /// recipient cannot open is value burned. Returning them is not a leak: the sender
    /// chose these values, and owner binding means knowing them confers no ability to
    /// spend the note.
    pub out_preimages: Vec<(u64, BaseElement)>,
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
///
/// Each output is `(value, recipient_pk)`. Pass [`ShieldedAccount::public_key`] for change
/// you keep, or the payee's public key to pay someone. The recipient key is bound
/// in-circuit as a hidden witness, so paying someone does not name them on chain.
#[allow(clippy::too_many_arguments)]
pub fn build_spend(
    account: &ShieldedAccount,
    store: &mut NoteStore,
    pool_commitments: &[[u8; 32]],
    note_index: u64,
    public_value: u64,
    outs_spec: &[(u64, BaseElement)],
) -> Result<SpendBundle, SpendBuildError> {
    if outs_spec.len() != N_OUTS {
        return Err(SpendBuildError::WrongOutputCount {
            expected: N_OUTS,
            got: outs_spec.len(),
        });
    }
    let out_values: Vec<u64> = outs_spec.iter().map(|(v, _)| *v).collect();

    let owned = store
        .notes
        .iter()
        .find(|n| n.index == Some(note_index) && !n.spent)
        .cloned()
        .ok_or(SpendBuildError::NoSuitableNote { needed: public_value })?;
    let position = owned.position.ok_or(SpendBuildError::NoteNotOnChain)?;

    let sum: u64 = out_values.iter().sum();
    let expected = owned.value.saturating_sub(public_value);
    if sum != expected {
        return Err(SpendBuildError::NonConserving { expected, got: sum });
    }

    let note = Note {
        value: BaseElement::new(owned.value),
        blinding: owned.blinding,
        spend_key: account.spend_key(),
    };

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

    // Allocate outputs and derive their blindings. Only outputs bound to THIS account go
    // into the note store: a note paid to someone else is not ours to track or spend.
    let mine = account.public_key();
    let mut outs = [(BaseElement::ZERO, BaseElement::ZERO, BaseElement::ZERO); N_OUTS];
    let mut out_indices = Vec::new();
    let mut out_preimages = Vec::with_capacity(N_OUTS);
    for (i, (v, recipient)) in outs_spec.iter().enumerate() {
        let idx = store.allocate_with(account, *v);
        let blinding = account.blinding(idx);
        if *recipient == mine {
            out_indices.push(idx);
        } else {
            // Not ours — drop the placeholder so `balance()` does not count value we
            // cannot spend. The preimage is still returned so the caller can seal it.
            store.notes.retain(|n| n.index != Some(idx));
        }
        out_preimages.push((*v, blinding));
        outs[i] = (BaseElement::new(*v), blinding, *recipient);
    }

    let path = tree.path(position as usize);
    let trace = build_spend_full_v4_trace(
        note.value,
        note.blinding,
        note.spend_key,
        BaseElement::new(public_value),
        &outs,
        &path,
    );
    let proof = SpendFullV4Prover::new(mimc_options())
        .prove(trace)
        .expect("a conserving, in-range witness must prove");

    Ok(SpendBundle {
        anchor: to_wire(tree.root()),
        nullifier: to_wire(note.nullifier(position)),
        // the OWNER-BOUND output commitment: compress2(compress2(value, blinding), pk)
        cm_outs: outs
            .iter()
            .map(|(v, b, pk)| to_wire(compress2(compress2(*v, *b), *pk)))
            .collect(),
        public_value: public_value as u128,
        proof: proof.to_bytes(),
        out_indices,
        out_preimages,
    })
}

/// Derivation-index helper for a freshly shielded deposit: allocate the note, return its
/// index and the commitment to publish.
pub fn shield_note(
    account: &ShieldedAccount,
    store: &mut NoteStore,
    value: u64,
) -> Result<(u64, [u8; 32]), NoteError> {
    let index = store.allocate_with(account, value);
    let note = account.note(index, value)?;
    Ok((index, to_wire(note.commitment())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::note_v1::padding_leaf;
    use crate::spend_full_v4::{verify_spend_full_v4, SpendFullV4PublicInputs};

    const CAPACITY: usize = 1 << 15;

    fn to_field(e: BaseElement) -> BaseElement { e }

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
        my_store.allocate_with(&mine, 500); // I know a 500 exists; I do not know its blinding
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
        let me = acct.public_key();
        let bundle = build_spend(&acct, &mut store, &pool, index, 3, &[(50, me), (47, me)])
            .expect("wallet must build a spend");

        let root = from_wire(&bundle.anchor).unwrap();
        let nf = from_wire(&bundle.nullifier).unwrap();
        let cm_outs = [
            from_wire(&bundle.cm_outs[0]).unwrap(),
            from_wire(&bundle.cm_outs[1]).unwrap(),
        ];
        let proof = winterfell::Proof::from_bytes(&bundle.proof).expect("decode");
        verify_spend_full_v4(
            proof,
            SpendFullV4PublicInputs { root, nf, fee: BaseElement::new(3), cm_outs },
        )
        .expect("SECURITY: a wallet-built spend must verify under the production circuit");

        assert_eq!(bundle.out_indices.len(), 2, "change notes recorded for tracking");
    }

    /// Paying a THIRD PARTY: the note goes to them, and this wallet must not count it as
    /// spendable balance. Tracking it would report money we cannot spend — and, before
    /// owner binding, we actually COULD have spent it, which was the bug.
    #[test]
    fn a_note_paid_to_someone_else_is_not_our_balance() {
        let me = ShieldedAccount::from_seed([42u8; 32]);
        let bob = ShieldedAccount::from_seed([0xB0u8; 32]);
        let mut store = NoteStore::new();
        let (index, cm) = shield_note(&me, &mut store, 100).unwrap();
        let pool = padded(&[cm]);
        store.scan_owned(&me, &pool);
        assert_eq!(store.balance(), 100);

        // 50 to Bob, 47 back to me, fee 3.
        let bundle = build_spend(
            &me, &mut store, &pool, index, 3,
            &[(50, bob.public_key()), (47, me.public_key())],
        ).expect("build");
        assert_eq!(bundle.out_indices.len(), 1, "only OUR output is tracked");

        // Bob's commitment must be bound to Bob, not to us.
        let bob_cm = from_wire(&bundle.cm_outs[0]).unwrap();
        assert_ne!(
            bob_cm,
            compress2(compress2(BaseElement::new(50), me.blinding(bundle.out_indices[0])), me.public_key()),
            "the note paid to Bob must not be bound to our key"
        );
    }

    /// THE RECEIVING GATE — the property that made "shielded payments" false until now.
    ///
    /// Alice pays Bob. Bob has never seen the note, cannot read its commitment, and knows
    /// nothing but his own keys. He must be able to (1) discover it by trial-decryption,
    /// (2) locate it in the pool, and (3) SPEND it. Step 3 is the one that matters: a
    /// wallet that can see a payment but not spend it has not received anything.
    ///
    /// This also pins the security half — Alice CREATED the note and knows its full
    /// preimage, yet cannot spend it, because the leaf binds Bob's key. Before owner
    /// binding both of them could have spent it, with different nullifiers.
    #[test]
    fn alice_pays_bob_and_bob_can_spend_it() {
        use crate::note_cipher::{enc_identity_from_seed, seal_note, NotePlaintext};

        let alice_seed = [0xA1u8; 32];
        let bob_seed = [0xB0u8; 32];
        let alice = ShieldedAccount::from_seed(alice_seed);
        let bob = ShieldedAccount::from_seed(bob_seed);
        let bob_enc = enc_identity_from_seed(&bob_seed);
        let bob_addr = bob.address(&bob_seed);

        // Alice shields 100 and finds her note.
        let mut alice_store = NoteStore::new();
        let (idx, cm) = shield_note(&alice, &mut alice_store, 100).unwrap();
        let pool0 = padded(&[cm]);
        assert_eq!(alice_store.scan_owned(&alice, &pool0), 1);

        // Alice pays Bob 50, keeps 47 as change, 3 fee.
        let bundle = build_spend(
            &alice, &mut alice_store, &pool0, idx, 3,
            &[(50, bob_addr.shield_key().unwrap()), (47, alice.public_key())],
        )
        .expect("alice builds the payment");

        // She seals the note preimage to Bob and publishes it with the tx.
        // Output 0 went to Bob; the bundle hands back exactly what must be sealed.
        let (bob_value, bob_blinding) = bundle.out_preimages[0];
        assert_eq!(bob_value, 50);
        let ct = seal_note(
            &NotePlaintext { value: bob_value, blinding: bob_blinding },
            &bob_addr,
        )
        .expect("seal to bob");

        // The chain now holds Alice's original note plus the two outputs.
        let pool1 = padded(&[cm, bundle.cm_outs[0], bundle.cm_outs[1]]);

        // ── Bob's side: he knows only his seed. ──
        let mut bob_store = NoteStore::new();
        assert_eq!(
            bob_store.scan_ciphertexts(&bob_enc, &[ct.clone()]),
            1,
            "Bob must discover the payment by trial-decryption alone"
        );
        assert_eq!(
            bob_store.scan_owned(&bob, &pool1),
            1,
            "and locate it in the pool — proving the commitment really is bound to HIS key"
        );
        assert_eq!(bob_store.balance(), 50, "Bob's spendable balance is the payment");

        // (3) Bob SPENDS it — the step that makes this a real receipt.
        let bobs_note_index = bob_store.notes[0].index;
        assert!(bobs_note_index.is_none(), "a received note has no derivation index");
        let received = bob_store.notes[0].clone();
        let note = Note {
            value: BaseElement::new(received.value),
            blinding: received.blinding,
            spend_key: bob.spend_key(),
        };
        let position = received.position.expect("located");
        let leaves: Vec<BaseElement> = pool1
            .iter()
            .enumerate()
            .map(|(i, c)| from_wire(c).unwrap_or_else(|_| padding_leaf(i as u64)))
            .collect();
        let tree = ShieldedPoolTree::new(leaves).unwrap();
        let path = tree.path(position as usize);
        let me = bob.public_key();
        let outs = [(BaseElement::new(20), bob.blinding(0), me),
                    (BaseElement::new(25), bob.blinding(1), me)];
        let trace = crate::spend_full_v4::build_spend_full_v4_trace(
            note.value, note.blinding, note.spend_key, BaseElement::new(5), &outs, &path,
        );
        let proof = crate::spend_full_v4::SpendFullV4Prover::new(mimc_options())
            .prove(trace)
            .expect("bob must be able to prove a spend of the note he received");
        crate::spend_full_v4::verify_spend_full_v4(
            proof,
            crate::spend_full_v4::SpendFullV4PublicInputs {
                root: tree.root(),
                nf: to_field(note.nullifier(position)),
                fee: BaseElement::new(5),
                cm_outs: [compress2(compress2(outs[0].0, outs[0].1), me),
                          compress2(compress2(outs[1].0, outs[1].1), me)],
            },
        )
        .expect("SECURITY: Bob must be able to SPEND a note he only received");

        // ── and the security half: Alice cannot spend what she paid away. ──
        let alice_trace = crate::spend_full_v4::build_v4_trace_unchecked(
            note.value, note.blinding, alice.spend_key(), BaseElement::new(5), &outs, &path,
        );
        let alice_verdict = match std::panic::catch_unwind(|| {
            crate::spend_full_v4::SpendFullV4Prover::new(mimc_options()).prove(alice_trace)
        }) {
            Err(_) | Ok(Err(_)) => Err(()),
            Ok(Ok(p)) => crate::spend_full_v4::verify_spend_full_v4(
                p,
                crate::spend_full_v4::SpendFullV4PublicInputs {
                    root: tree.root(),
                    nf: compress2(alice.spend_key(), BaseElement::new(position)),
                    fee: BaseElement::new(5),
                    cm_outs: [compress2(compress2(outs[0].0, outs[0].1), me),
                              compress2(compress2(outs[1].0, outs[1].1), me)],
                },
            ).map_err(|_| ()),
        };
        assert!(
            alice_verdict.is_err(),
            "SECURITY: the SENDER knows the full preimage and must still not be able to \
             spend the note she paid away"
        );
    }

    #[test]
    fn non_conserving_spend_is_refused_with_a_legible_error() {
        let acct = ShieldedAccount::from_seed([42u8; 32]);
        let mut store = NoteStore::new();
        let (index, cm) = shield_note(&acct, &mut store, 100).unwrap();
        let pool = padded(&[cm]);
        store.scan_owned(&acct, &pool);
        let me = acct.public_key();
        let err = build_spend(&acct, &mut store, &pool, index, 3, &[(50, me), (48, me)])
            .expect_err("a non-conserving spend must be refused");
        assert_eq!(err, SpendBuildError::NonConserving { expected: 97, got: 98 });
    }
}
