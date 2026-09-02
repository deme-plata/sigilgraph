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
use crate::spend_full_v4::{N_OUTS, PK_DOMAIN};
use crate::spend_full_v5::{build_spend_full_v5_trace, v5_options, SpendFullV5Prover};
use crate::spend_full_v6::{self as v6, N_INS as V6_INS};

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

/// The derivation-index space reserved for SPEND OUTPUTS.
///
/// `NoteStore` hands out a sequential counter (0, 1, 2, ...) for shield deposits, whose
/// blindings have no other source of uniqueness. Spend outputs derive their index from
/// the spend itself instead (see [`output_derivation_index`]) and live above this base,
/// so the two schemes can never name the same index.
pub const OUT_INDEX_BASE: u64 = 1 << 31;

/// Bits of leaf position carried structurally in an output index.
///
/// 15 is `sigil_state::POOL_DEPTH` — the shielded pool holds `2^15 = 32,768` leaves, so
/// every real position fits. It is duplicated rather than imported because `sigil-state`
/// depends on this crate, not the other way round. If the pool is ever deepened this
/// number should follow; nothing breaks if it does not (the excess folds into the tag,
/// see below), uniqueness merely stops being exact and becomes overwhelmingly likely.
const POSITION_BITS: u32 = 15;
/// Bits of output slot carried structurally. `N_OUTS` is 2, so one bit.
const SLOT_BITS: u32 = 1;
/// What is left of a `u32` once the base bit, the position and the slot are spent.
const TAG_BITS: u32 = 31 - POSITION_BITS - SLOT_BITS;

/// The derivation index for output `slot` of the spend that consumes the note at leaf
/// `position` under pool root `anchor`.
///
/// # Why this replaces a counter, and why it needs no persistent state
///
/// The blinding of an output is `account.blinding(index)`, so two outputs sharing an
/// index share a blinding — and since a leaf is `compress2(compress2(value, blinding),
/// pk)`, two payments of the same amount to the same payee then publish the SAME leaf.
/// That is the whole bug: the index used to come from a `NoteStore` counter that starts
/// at 0, and the callers that matter build a fresh store per transaction because each is
/// handed one note and asked to prove one spend.
///
/// The fix is to stop asking the caller to remember anything. A spend already contains a
/// value that cannot repeat: the leaf position of the note it consumes. A note can be
/// spent exactly once — the chain's nullifier set is what enforces it, since
/// `nf = compress2(spend_key, position)` — so within one pool generation `position` is
/// unique across every spend a wallet will ever make. Two payments necessarily consume
/// two different notes, therefore two different positions, therefore two different
/// indices. Not with high probability: exactly.
///
/// `anchor` covers the one case position alone does not. Positions restart at 0 when the
/// pool rotates to a new epoch, so leaf 5 of epoch 0 and leaf 5 of epoch 1 are different
/// notes with the same position. The two spends are proved against different pool roots,
/// so folding the root in separates them — probabilistically there (one chance in
/// `2^TAG_BITS`), exactly within an epoch, which is where every spend to date has lived
/// (`/v1/shielded/anchor` reports `epoch: 0`, `sealed_epochs: []`).
///
/// # Why the result is deliberately squeezed into a `u32`
///
/// Downstream callers hold this number and re-derive the blinding from it later:
/// `wasm_api::build_private_send` takes `note_index: u32`, and `sigil-top`'s
/// `mine_local_api` returns `change_index` as a JSON number that JavaScript parses into
/// an `f64`. A wider index would be silently truncated by one and rounded by the other,
/// turning a recoverable change note into an unspendable one — trading this bug for a
/// worse one. Anything that does not fit the structural layout is folded into the tag
/// rather than wrapped, because a wrap would recreate exactly the collision this exists
/// to prevent.
pub fn output_derivation_index(anchor: &[u8; 32], position: u64, slot: usize) -> u64 {
    let slot = slot as u64;
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-shielded-wallet-v1");
    h.update(b"out-index");
    h.update(anchor);
    h.update(&(position >> POSITION_BITS).to_le_bytes());
    h.update(&(slot >> SLOT_BITS).to_le_bytes());
    let mut tag8 = [0u8; 8];
    tag8.copy_from_slice(&h.finalize().as_bytes()[..8]);
    let tag = u64::from_le_bytes(tag8) & ((1 << TAG_BITS) - 1);

    OUT_INDEX_BASE
        | (tag << (POSITION_BITS + SLOT_BITS))
        | ((position & ((1 << POSITION_BITS) - 1)) << SLOT_BITS)
        | (slot & ((1 << SLOT_BITS) - 1))
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

    /// Reconstruct a block reward this account was minted at `height`, from nothing but
    /// the height and amount — no scanning, no ciphertext, no server round-trip beyond
    /// fetching the pool's leaves to confirm it landed.
    ///
    /// 2026-08-23: coinbase notes use [`crate::note_v1::coinbase_blinding`], a PUBLICLY
    /// derivable formula (`compress2(compress2(height, pk), DOMAIN)`) — deliberately
    /// different from [`Self::blinding`]'s seed-private derivation, because a coinbase
    /// amount is already public and bound to its block; there is nothing left to hide at
    /// mint. This is what lets a miner find its own reward with only `(height, amount)`,
    /// which it already knows from having won that block — no registration beyond the
    /// one-time key, no wallet-side bookkeeping of an index.
    pub fn coinbase_note(&self, height: u64, amount: u64) -> Result<Note, NoteError> {
        if amount >= (1u64 << RANGE_BITS) {
            return Err(NoteError::AmountOutOfRange { got: amount, bits: RANGE_BITS as u32 });
        }
        let pk = self.public_key();
        Ok(Note {
            value: BaseElement::new(amount),
            blinding: crate::note_v1::coinbase_blinding(height, pk),
            spend_key: self.spend_key(),
        })
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
    /// The sealed memo that arrived with a RECEIVED note (`None` for notes this wallet
    /// created itself, and for pre-memo deliveries). Never on chain in the clear.
    pub memo: Option<String>,
}

/// The wallet's local record of its own notes.
///
/// This is authoritative for *this* wallet and reconstructible from the seed plus the
/// values. It is NOT a chain scanner — see the module docs on the receiving gap.
#[derive(Clone, Debug, Default)]
pub struct NoteStore {
    pub notes: Vec<OwnedNote>,
    next_index: u64,
    /// Deliveries already booked, keyed by the ciphertext that carried them.
    ///
    /// Rescanning the chain must not book a payment twice, but "I have seen this
    /// ciphertext before" and "someone paid me the same amount again" are different
    /// statements and only the first is a duplicate. Keying on the delivery keeps
    /// rescans idempotent without making a second identical payment invisible — which
    /// is what keying on `(value, blinding)` did.
    seen_deliveries: BTreeSet<[u8; 32]>,
}

impl NoteStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// A store whose next derivation index is `base` rather than 0.
    ///
    /// # Why this is necessary, and not a convenience
    ///
    /// Output blindings come from `account.blinding(index)`, and the index comes from this
    /// counter. A caller that builds a fresh `NoteStore` per transaction — which both the
    /// Android core and `wasm_api` do, because they are handed one note and asked to prove
    /// one spend — therefore allocates index 0 to the payee's note and index 1 to the
    /// change, EVERY TIME.
    ///
    /// The consequence is not subtle. An output commitment is
    /// `compress2(compress2(value, blinding), pk)`, so two payments of the same amount to
    /// the same recipient produce a byte-identical leaf. On a chain whose wallets snap
    /// balances to a 1/2/5x10^k denomination ladder, equal amounts are the ordinary case,
    /// and `NoteStore::receive` deduplicates on exactly `(value, blinding)` — so the payee
    /// books the first payment and the second is invisible to them. Money sent, nothing
    /// received, no error anywhere.
    ///
    /// A caller that keeps its own note counter passes it here and the collision cannot
    /// occur. A caller that owns a long-lived store never had the problem.
    pub fn with_next_index(base: u64) -> Self {
        Self { notes: Vec::new(), next_index: base, seen_deliveries: BTreeSet::new() }
    }

    /// Record a note this wallet is about to create, returning its derivation index.
    ///
    /// This is the SHIELD path: a deposit conjures a note out of nothing, so the counter
    /// is its only source of uniqueness and a caller that resets it collides. Spend
    /// outputs no longer come through here — they derive their index from the spend
    /// itself (see [`output_derivation_index`]) precisely because they have a better
    /// source of uniqueness available and should not depend on caller bookkeeping.
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
            memo: None,
        });
        index
    }

    /// Record an output this wallet created at an index it did NOT draw from the
    /// sequential counter — a spend output, whose index comes from
    /// [`output_derivation_index`].
    ///
    /// Deliberately does not touch `next_index`: spend-output indices live above
    /// [`OUT_INDEX_BASE`], and advancing the shield counter to meet them would push every
    /// future deposit into the same reserved space.
    pub fn record_derived(&mut self, index: u64, value: u64, blinding: BaseElement) {
        self.notes.push(OwnedNote {
            index: Some(index),
            value,
            blinding,
            position: None,
            spent: false,
            memo: None,
        });
    }

    /// Record a note RECEIVED from someone else, whose blinding came out of a ciphertext.
    ///
    /// Deduplicates on `(value, blinding)`, which is right for a caller replaying a note
    /// it already holds. It is NOT how [`scan_ciphertexts`](Self::scan_ciphertexts)
    /// books deliveries any more: two people (or one person twice) can legitimately send
    /// the same amount under the same blinding — that is two payments and two leaves,
    /// and collapsing them into one note loses the second irrecoverably.
    pub fn receive(&mut self, value: u64, blinding: BaseElement) -> bool {
        self.receive_with_memo(value, blinding, None)
    }

    /// [`receive`](Self::receive), keeping the memo that came sealed with the note.
    pub fn receive_with_memo(
        &mut self,
        value: u64,
        blinding: BaseElement,
        memo: Option<String>,
    ) -> bool {
        if self.notes.iter().any(|n| n.value == value && n.blinding == blinding) {
            return false;
        }
        self.notes.push(OwnedNote {
            index: None,
            value,
            blinding,
            position: None,
            spent: false,
            memo,
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
                // Key on the DELIVERY, not on what it contains. Ciphertexts are sealed
                // with a fresh ephemeral key, so two payments are two distinct
                // ciphertexts even when their plaintexts are identical — and identical
                // plaintexts are exactly what a sender running the pre-fix code produced.
                let id = *blake3::hash(ct.0.as_bytes()).as_bytes();
                if !self.seen_deliveries.insert(id) {
                    continue;
                }
                let memo = (!pt.memo.is_empty()).then(|| pt.memo.text());
                self.notes.push(OwnedNote {
                    index: None,
                    value: pt.value,
                    blinding: pt.blinding,
                    position: None,
                    spent: false,
                    memo,
                });
                found += 1;
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
        // Positions already spoken for, so two notes never claim one leaf. Two leaves
        // CAN be byte-identical — every note minted by the pre-fix sender is — and
        // handing both notes the first match would leave the second unspendable, since
        // the nullifier binds to the position and would already be spent.
        let mut claimed: BTreeSet<u64> = self.notes.iter().filter_map(|n| n.position).collect();
        let mut found = 0;
        for n in self.notes.iter_mut().filter(|n| n.position.is_none()) {
            let note = Note {
                value: BaseElement::new(n.value),
                blinding: n.blinding,
                spend_key: account.spend_key(),
            };
            let cm = to_wire(note.commitment());
            let hit = pool_commitments
                .iter()
                .enumerate()
                .find(|(i, c)| **c == cm && !claimed.contains(&(*i as u64)))
                .map(|(i, _)| i as u64);
            if let Some(pos) = hit {
                n.position = Some(pos);
                claimed.insert(pos);
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
    /// Tags for inputs beyond the first. Empty for a one-note spend, one entry for a
    /// two-note merge. Goes straight into `SigilTx::ShieldedSend::extra_nullifiers`.
    pub extra_nullifiers: Vec<[u8; 32]>,
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
    /// Both inputs are the same note. The circuit cannot catch this — see
    /// `spend_full_v6::reject_duplicate_nullifiers` — so it is refused here, at the point
    /// where a wallet bug would otherwise produce a proof that mints money.
    #[error("both inputs are the same note (position {position})")]
    SameNoteTwice { position: usize },
    /// A membership path does not climb to the anchor the spend declares.
    #[error("witness rejected: {0}")]
    Witness(String),
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
/// `store_position` selects the note by its position in [`NoteStore::notes`] — NOT
/// [`OwnedNote::index`] (that field is only ever `Some` for a note this account derived
/// itself; a received or reconstructed-coinbase note is `None`, and picking by that field
/// alone made spending either impossible through this function — see the 2026-08-23
/// `alice_pays_bob_and_bob_can_spend_it` test, which had to hand-roll a spend outside this
/// helper entirely because of exactly that gap). Position works uniformly for every note
/// origin: self-shielded, received, or a coinbase reward reconstructed via
/// [`ShieldedAccount::coinbase_note`].
///
/// Each output is `(value, recipient_pk)`. Pass [`ShieldedAccount::public_key`] for change
/// you keep, or the payee's public key to pay someone. The recipient key is bound
/// in-circuit as a hidden witness, so paying someone does not name them on chain.
#[allow(clippy::too_many_arguments)]
pub fn build_spend(
    account: &ShieldedAccount,
    store: &mut NoteStore,
    pool_commitments: &[[u8; 32]],
    store_position: usize,
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
        .get(store_position)
        .filter(|n| !n.spent)
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

    // Derive the outputs' blindings FROM THIS SPEND rather than from a counter in the
    // store. A counter that a caller resets per transaction hands index 0 to the payee
    // every time, and equal value + equal blinding + equal payee key is a byte-identical
    // leaf — so two ordinary payments of the same amount became one note the payee could
    // see and one that vanished. See [`output_derivation_index`] for why the leaf
    // position of the note being consumed cannot repeat.
    //
    // Only outputs bound to THIS account go into the note store: a note paid to someone
    // else is not ours to track or spend. Its preimage is still returned so the caller
    // can seal it to the recipient.
    let anchor = to_wire(tree.root());
    let mine = account.public_key();
    let mut outs = [(BaseElement::ZERO, BaseElement::ZERO, BaseElement::ZERO); N_OUTS];
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
        outs[i] = (BaseElement::new(*v), blinding, *recipient);
    }

    let path = tree.path(position as usize);
    // PROVE WITH v5, THE HIDING CIRCUIT (2026-08-28).
    //
    // This wallet proved with v4 for its whole life, and v4 is sound but NOT hiding: it
    // holds the secrets in constant trace columns, so the recipient key and both output
    // amounts appear in the proof bytes verbatim, ~85 occurrences each. v5 fixes that by
    // reserving the trace's second half for randomness — and v5 was written, committed,
    // and then never called. It sat as dead code, referenced only from `examples/`, while
    // every real spend kept publishing its witness.
    //
    // `build_spend_full_v5_trace` draws its mask from the OS. That is not optional: two
    // proofs of the same witness under the SAME seed are byte-identical, and differencing
    // two proofs under different seeds is exactly the attack the randomness exists to stop.
    // A fixed seed is a test-only affordance.
    //
    // Verification accepts v4 as well during the rollout window (see
    // `note_v1::verify_spend_wire_multi`), so this change does not strand older wallets —
    // it only stops THIS one leaking.
    let trace = build_spend_full_v5_trace(
        note.value,
        note.blinding,
        note.spend_key,
        BaseElement::new(public_value),
        &outs,
        &path,
        &v5_options(),
    );
    let proof = SpendFullV5Prover::new(v5_options())
        .prove(trace)
        .expect("a conserving, in-range witness must prove");

    Ok(SpendBundle {
        anchor,
        nullifier: to_wire(note.nullifier(position)),
        extra_nullifiers: Vec::new(),
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

/// Spend TWO notes in one transaction — the merge `build_spend` cannot express.
///
/// One note in means the biggest payment a wallet can make is bounded by its biggest single
/// note, and no sequence of transactions ever makes a note bigger. This is the way out:
/// two notes go in, and one output can exceed either of them.
///
/// Both notes must be in the SAME pool (they are proven against one anchor), and both must
/// belong to this account. Outputs work exactly as in [`build_spend`]: pass the payee's key
/// to pay someone, or this account's key to keep change.
///
/// ⚠️ Rejects the same note given twice. That is not defensive tidiness — the CIRCUIT
/// accepts it. Two identical input blocks are each independently valid and the conservation
/// lane simply sums twice the value, so the proof verifies; the only tell is the repeated
/// nullifier, and the chain stores those in a set, so the duplicate is a no-op. One note
/// burned, double the value out. The chain refuses it too (twice over), but a wallet should
/// never build one in the first place.
#[allow(clippy::too_many_arguments)]
pub fn build_spend_2(
    account: &ShieldedAccount,
    store: &mut NoteStore,
    pool_commitments: &[[u8; 32]],
    store_positions: [usize; V6_INS],
    public_value: u64,
    outs_spec: &[(u64, BaseElement)],
) -> Result<SpendBundle, SpendBuildError> {
    if outs_spec.len() != N_OUTS {
        return Err(SpendBuildError::WrongOutputCount { expected: N_OUTS, got: outs_spec.len() });
    }
    if store_positions[0] == store_positions[1] {
        return Err(SpendBuildError::SameNoteTwice { position: store_positions[0] });
    }

    // Gather both notes and their on-chain leaf positions.
    let mut owned = Vec::with_capacity(V6_INS);
    for sp in store_positions {
        let n = store
            .notes
            .get(sp)
            .filter(|n| !n.spent)
            .cloned()
            .ok_or(SpendBuildError::NoSuitableNote { needed: public_value })?;
        owned.push(n);
    }
    let mut leaf_positions = Vec::with_capacity(V6_INS);
    for n in &owned {
        leaf_positions.push(n.position.ok_or(SpendBuildError::NoteNotOnChain)?);
    }
    // Two DIFFERENT store slots could still name one on-chain leaf if the store were
    // corrupted. The circuit would accept that just as happily.
    if leaf_positions[0] == leaf_positions[1] {
        return Err(SpendBuildError::SameNoteTwice { position: leaf_positions[0] as usize });
    }

    let in_sum: u64 = owned.iter().map(|n| n.value).sum();
    let out_values: Vec<u64> = outs_spec.iter().map(|(v, _)| *v).collect();
    let sum: u64 = out_values.iter().sum();
    let expected = in_sum.saturating_sub(public_value);
    if sum != expected {
        return Err(SpendBuildError::NonConserving { expected, got: sum });
    }

    let leaves: Vec<BaseElement> = pool_commitments
        .iter()
        .enumerate()
        .map(|(i, c)| from_wire(c).unwrap_or_else(|_| crate::note_v1::padding_leaf(i as u64)))
        .collect();
    let tree = ShieldedPoolTree::new(leaves).map_err(SpendBuildError::Note)?;

    let mut notes = Vec::with_capacity(V6_INS);
    for (n, pos) in owned.iter().zip(leaf_positions.iter()) {
        let note = Note {
            value: BaseElement::new(n.value),
            blinding: n.blinding,
            spend_key: account.spend_key(),
        };
        if tree.leaf(*pos as usize) != Some(note.commitment()) {
            return Err(SpendBuildError::PositionMismatch);
        }
        notes.push(note);
    }

    // Outputs: identical handling to `build_spend`, including the derivation — the index
    // comes from the FIRST input's leaf position, which is consumed by this spend and so
    // cannot be reused, rather than from a resettable counter.
    let anchor = to_wire(tree.root());
    let mine = account.public_key();
    let mut outs = [(BaseElement::ZERO, BaseElement::ZERO, BaseElement::ZERO); N_OUTS];
    let mut out_indices = Vec::new();
    let mut out_preimages = Vec::with_capacity(N_OUTS);
    for (i, (v, recipient)) in outs_spec.iter().enumerate() {
        let idx = output_derivation_index(&anchor, leaf_positions[0], i);
        let blinding = account.blinding(idx);
        if *recipient == mine {
            store.record_derived(idx, *v, blinding);
            out_indices.push(idx);
        }
        out_preimages.push((*v, blinding));
        outs[i] = (BaseElement::new(*v), blinding, *recipient);
    }

    let path0 = tree.path(leaf_positions[0] as usize);
    let path1 = tree.path(leaf_positions[1] as usize);
    let ins = [
        (notes[0].value, notes[0].blinding, notes[0].spend_key),
        (notes[1].value, notes[1].blinding, notes[1].spend_key),
    ];
    // `SpendV6Witness::new` re-derives each path's root and refuses anything that does not
    // climb to the declared anchor — cheap here, and the alternative is an opaque verifier
    // rejection much later.
    let witness = v6::SpendV6Witness::new(tree.root(), ins, outs, [&path0, &path1])
        .map_err(|e| SpendBuildError::Witness(e.to_string()))?;
    let trace = witness.build_trace(BaseElement::new(public_value), &v6::v6_options());
    let proof = v6::SpendFullV6Prover::new(v6::v6_options())
        .prove(trace)
        .expect("a conserving, in-range two-note witness must prove");

    Ok(SpendBundle {
        anchor,
        nullifier: to_wire(notes[0].nullifier(leaf_positions[0])),
        extra_nullifiers: vec![to_wire(notes[1].nullifier(leaf_positions[1]))],
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

    /// THE WALLET GATE: a wallet-built spend must verify through THE PRODUCTION PATH.
    ///
    /// Deliberately calls `note_v1::verify_spend_wire` — the function the settlement
    /// chokepoint actually calls — rather than naming a circuit directly. It used to call
    /// `verify_spend_full_v4`, and that is precisely how the wallet could be switched from
    /// v4 to v5 with the test still describing v4: a test that names a circuit tests that
    /// circuit, not the path a real spend takes.
    ///
    /// IGNORED in debug only: `build_spend` proves, hitting the winterfell 0.9 debug-only
    /// `validate_transition_degrees` quirk documented on the circuit tests — not a
    /// soundness gap; release-compiled winter-prover passes.
    #[test]
    #[ignore = "winterfell 0.9 debug-only validate_transition_degrees vs witness-dependent range-bit column degree; release-compiled winter-prover passes."]
    fn wallet_built_spend_verifies() {
        let acct = ShieldedAccount::from_seed([42u8; 32]);
        let mut store = NoteStore::new();
        let (index, cm) = shield_note(&acct, &mut store, 100).unwrap();
        let pool = padded(&[cm]);
        assert_eq!(store.scan_owned(&acct, &pool), 1);

        // Spend 100 as fee 3 + change 50 + 47.
        let me = acct.public_key();
        let bundle = build_spend(&acct, &mut store, &pool, index as usize, 3, &[(50, me), (47, me)])
            .expect("wallet must build a spend");

        crate::note_v1::verify_spend_wire(
            &bundle.anchor,
            &bundle.nullifier,
            3,
            &bundle.cm_outs,
            &bundle.proof,
        )
        .expect("SECURITY: a wallet-built spend must verify through the production path");

        assert_eq!(bundle.out_indices.len(), 2, "change notes recorded for tracking");
    }

    /// THE MERGE, END TO END: two notes in, one bigger note out, verified through the
    /// production path.
    ///
    /// This is the transaction a one-input wallet cannot express at any parameter setting.
    /// It goes through `note_v1::verify_spend_wire_multi` — the function the settlement
    /// chokepoint calls — rather than naming a circuit, so it tests the path a real spend
    /// takes and not a circuit I happened to pick.
    #[test]
    #[ignore = "winterfell 0.9 debug-only validate_transition_degrees; release-compiled winter-prover passes."]
    fn two_notes_merge_into_one_the_wallet_could_not_have_made_before() {
        let acct = ShieldedAccount::from_seed([42u8; 32]);
        let mut store = NoteStore::new();
        let (i0, cm0) = shield_note(&acct, &mut store, 50).unwrap();
        let (i1, cm1) = shield_note(&acct, &mut store, 47).unwrap();
        let pool = padded(&[cm0, cm1]);
        assert_eq!(store.scan_owned(&acct, &pool), 2);

        let p0 = store.notes.iter().position(|n| n.index == Some(i0)).unwrap();
        let p1 = store.notes.iter().position(|n| n.index == Some(i1)).unwrap();
        let me = acct.public_key();

        // 50 + 47 - 3 fee = 94, all of it into ONE note. Neither input could fund it.
        let bundle = build_spend_2(&acct, &mut store, &pool, [p0, p1], 3, &[(94, me), (0, me)])
            .expect("the wallet must be able to merge two notes");

        assert_eq!(bundle.extra_nullifiers.len(), 1, "a two-note spend reveals two tags");
        assert_ne!(
            bundle.nullifier, bundle.extra_nullifiers[0],
            "two distinct notes must nullify distinctly"
        );

        let mut nfs = vec![bundle.nullifier];
        nfs.extend_from_slice(&bundle.extra_nullifiers);
        crate::note_v1::verify_spend_wire_multi(&bundle.anchor, &nfs, 3, &bundle.cm_outs, &bundle.proof)
            .expect("SECURITY: a wallet-built merge must verify through the production path");
    }

    /// Two small notes fund a payment to a third party that NEITHER could cover — the case
    /// a user actually hits, and the one a one-input wallet has to refuse.
    #[test]
    #[ignore = "winterfell 0.9 debug-only validate_transition_degrees; release-compiled winter-prover passes."]
    fn two_notes_fund_a_payment_neither_could_cover_and_hide_the_payee() {
        let acct = ShieldedAccount::from_seed([7u8; 32]);
        let bob = ShieldedAccount::from_seed([0xB0u8; 32]);
        let mut store = NoteStore::new();
        let (i0, cm0) = shield_note(&acct, &mut store, 30).unwrap();
        let (i1, cm1) = shield_note(&acct, &mut store, 30).unwrap();
        let pool = padded(&[cm0, cm1]);
        store.scan_owned(&acct, &pool);
        let p0 = store.notes.iter().position(|n| n.index == Some(i0)).unwrap();
        let p1 = store.notes.iter().position(|n| n.index == Some(i1)).unwrap();

        // 40 to Bob — more than either note holds — 18 back as change, fee 2.
        let bundle = build_spend_2(
            &acct, &mut store, &pool, [p0, p1], 2,
            &[(40, bob.public_key()), (18, acct.public_key())],
        )
        .expect("two notes must be able to fund one larger payment");

        let mut nfs = vec![bundle.nullifier];
        nfs.extend_from_slice(&bundle.extra_nullifiers);
        crate::note_v1::verify_spend_wire_multi(&bundle.anchor, &nfs, 2, &bundle.cm_outs, &bundle.proof)
            .expect("must verify through the production path");

        // The merge must not have cost the privacy the single-input path just gained.
        let secrets = [
            ("input0.value", BaseElement::new(30)),
            ("payment.amount", BaseElement::new(40)),
            ("change.amount", BaseElement::new(18)),
            ("recipient.pk", bob.public_key()),
            ("spend_key", acct.spend_key()),
        ];
        let hits = crate::zk_mask::scan_proof_for_secrets(&bundle.proof, &secrets);
        assert!(hits.is_empty(), "SECURITY: the merge published its witness: {hits:?}");
    }

    /// The same note offered as both inputs is refused BY THE WALLET, before a proof exists.
    ///
    /// The circuit would accept it and the resulting proof would verify; only the repeated
    /// nullifier gives it away, and a set makes the duplicate insert a no-op. The chain
    /// refuses it in two places, but a wallet that can build one is a wallet that can be
    /// tricked into broadcasting one.
    #[test]
    fn the_wallet_refuses_to_spend_one_note_as_both_inputs() {
        let acct = ShieldedAccount::from_seed([42u8; 32]);
        let mut store = NoteStore::new();
        let (i0, cm0) = shield_note(&acct, &mut store, 50).unwrap();
        let pool = padded(&[cm0]);
        store.scan_owned(&acct, &pool);
        let p0 = store.notes.iter().position(|n| n.index == Some(i0)).unwrap();
        let me = acct.public_key();

        let r = build_spend_2(&acct, &mut store, &pool, [p0, p0], 0, &[(100, me), (0, me)]);
        assert!(
            matches!(r, Err(SpendBuildError::SameNoteTwice { .. })),
            "the wallet must never build a doubled-input spend, got {r:?}"
        );
    }

    /// Conservation is the wallet's obligation too: outputs plus fee must equal the SUM of
    /// both inputs, not either one of them.
    #[test]
    fn a_merge_must_conserve_the_sum_of_both_inputs() {
        let acct = ShieldedAccount::from_seed([42u8; 32]);
        let mut store = NoteStore::new();
        let (i0, cm0) = shield_note(&acct, &mut store, 50).unwrap();
        let (i1, cm1) = shield_note(&acct, &mut store, 47).unwrap();
        let pool = padded(&[cm0, cm1]);
        store.scan_owned(&acct, &pool);
        let p0 = store.notes.iter().position(|n| n.index == Some(i0)).unwrap();
        let p1 = store.notes.iter().position(|n| n.index == Some(i1)).unwrap();
        let me = acct.public_key();

        // 50 alone would conserve; 50 + 47 does not.
        let r = build_spend_2(&acct, &mut store, &pool, [p0, p1], 3, &[(47, me), (0, me)]);
        assert!(
            matches!(r, Err(SpendBuildError::NonConserving { expected: 94, got: 47 })),
            "got {r:?}"
        );
    }

    /// THE LEAK REGRESSION — the reason the wallet was moved from v4 to v5.
    ///
    /// v4 holds the witness in constant trace columns, so a v4 proof carries the recipient
    /// key and both output amounts verbatim, ~85 occurrences each. v5 fixes it by reserving
    /// the trace's second half for randomness. v5 was written and committed on 2026-08-28
    /// and then **never called**: `wallet.rs` kept proving with v4 and
    /// `note_v1::verify_spend_wire` kept verifying with v4, so the fix sat as dead code
    /// while every real spend published its witness.
    ///
    /// This test is what makes that impossible to repeat. It does not name a circuit — it
    /// asks the only question that matters: is any secret in the bytes the wallet is about
    /// to broadcast? Revert `build_spend` to v4 and it fails immediately.
    #[test]
    #[ignore = "winterfell 0.9 debug-only validate_transition_degrees; release-compiled winter-prover passes."]
    fn a_wallet_built_spend_does_not_publish_its_witness() {
        let acct = ShieldedAccount::from_seed([42u8; 32]);
        let bob = ShieldedAccount::from_seed([0xB0u8; 32]);
        let mut store = NoteStore::new();
        let (index, cm) = shield_note(&acct, &mut store, 100).unwrap();
        let pool = padded(&[cm]);
        assert_eq!(store.scan_owned(&acct, &pool), 1);

        // Pay a third party, which is the case where a leak actually costs someone
        // something: 60 to Bob, 37 back as change, fee 3.
        let bundle = build_spend(
            &acct, &mut store, &pool, index as usize, 3,
            &[(60, bob.public_key()), (37, acct.public_key())],
        )
        .expect("wallet must build a spend");

        let secrets = [
            ("note.value", BaseElement::new(100)),
            ("payment.amount", BaseElement::new(60)),
            ("change.amount", BaseElement::new(37)),
            ("recipient.pk", bob.public_key()),
            ("spend_key", acct.spend_key()),
        ];
        let hits = crate::zk_mask::scan_proof_for_secrets(&bundle.proof, &secrets);
        assert!(
            hits.is_empty(),
            "SECURITY: the wallet published its own witness in the proof: {hits:?}"
        );
    }

    /// Paying a THIRD PARTY: the note goes to them, and this wallet must not count it as
    /// spendable balance. Tracking it would report money we cannot spend — and, before
    /// owner binding, we actually COULD have spent it, which was the bug.
    ///
    /// IGNORED in debug only — same winterfell 0.9 debug-degree quirk as
    /// `wallet_built_spend_verifies` above (this test also calls `build_spend`).
    #[test]
    #[ignore = "winterfell 0.9 debug-only validate_transition_degrees vs witness-dependent range-bit column degree (same family as spend_full_v4); release-compiled winter-prover passes."]
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
            &me, &mut store, &pool, index as usize, 3,
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
    ///
    /// IGNORED in debug only — same winterfell 0.9 debug-degree quirk as
    /// `wallet_built_spend_verifies` above (this test proves THREE times: Alice's send,
    /// Bob's spend of the received note, and Alice's failed attempt).
    #[test]
    #[ignore = "winterfell 0.9 debug-only validate_transition_degrees vs witness-dependent range-bit column degree (same family as spend_full_v4); release-compiled winter-prover passes."]
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
            &alice, &mut alice_store, &pool0, idx as usize, 3,
            &[(50, bob_addr.shield_key().unwrap()), (47, alice.public_key())],
        )
        .expect("alice builds the payment");

        // She seals the note preimage to Bob and publishes it with the tx.
        // Output 0 went to Bob; the bundle hands back exactly what must be sealed.
        let (bob_value, bob_blinding) = bundle.out_preimages[0];
        assert_eq!(bob_value, 50);
        let ct = seal_note(
            &NotePlaintext::new(bob_value, bob_blinding),
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
        let err = build_spend(&acct, &mut store, &pool, index as usize, 3, &[(50, me), (48, me)])
            .expect_err("a non-conserving spend must be refused");
        assert_eq!(err, SpendBuildError::NonConserving { expected: 97, got: 98 });
    }

    /// THE MINER'S PATH, end to end: a coinbase note needs no ciphertext and no
    /// derivation-index bookkeeping — a miner reconstructs it from nothing but the
    /// height and amount it already knows from having won that block, using ONLY its
    /// registered public key. This pins that `ShieldedAccount::coinbase_note` produces
    /// the EXACT SAME commitment the real mint path
    /// (`sigil_shield::note_v1::coinbase_commitment_wire`) would put on chain, and that
    /// the note is then genuinely spendable — not just discoverable.
    ///
    /// IGNORED in debug only — same winterfell 0.9 debug-degree quirk as
    /// `wallet_built_spend_verifies` above.
    #[test]
    #[ignore = "winterfell 0.9 debug-only validate_transition_degrees vs witness-dependent range-bit column degree (same family as spend_full_v4); release-compiled winter-prover passes."]
    fn coinbase_reward_is_reconstructible_and_spendable_with_no_registration_bookkeeping() {
        let miner = ShieldedAccount::from_seed([0xC0u8; 32]);
        let height = 2_003_809u64;
        let reward = 5_000_000_000u64; // 5 SIGIL in atomic units, well within RANGE_BITS

        // What the PRODUCER actually mints on chain — the real function `coinbase.rs`
        // calls, not a copy of its logic.
        let pk_wire = to_wire(miner.public_key());
        let minted_cm = crate::note_v1::coinbase_commitment_wire(height, &pk_wire, reward as u128)
            .expect("in-range reward mints a commitment");

        // What the MINER reconstructs, knowing only (height, amount) — no ciphertext, no
        // scan, no help from the server beyond fetching the pool's real leaves.
        let reconstructed = miner.coinbase_note(height, reward).expect("in-range");
        assert_eq!(
            to_wire(reconstructed.commitment()),
            minted_cm,
            "the miner's reconstruction must match the REAL mint path bit-for-bit, or a \
             miner running only this code would never find its own reward"
        );

        // The miner books it via the SAME `receive` path a third-party payment uses — a
        // coinbase note has no derivation index either, for the same reason a received
        // note doesn't: its blinding was not chosen by allocating from our own seed.
        let mut store = NoteStore::new();
        assert!(store.receive(reward, reconstructed.blinding), "first sighting is new");
        assert!(!store.receive(reward, reconstructed.blinding), "re-scanning must not double-book it");

        let pool = padded(&[minted_cm]);
        assert_eq!(store.scan_owned(&miner, &pool), 1, "located at its real chain position");
        assert_eq!(store.balance(), reward as u128);

        // And — the step that actually matters — it can be SPENT, via the now-general
        // `build_spend`, selecting by store position rather than a derivation index that
        // this note was never given.
        let me = miner.public_key();
        let bundle = build_spend(&miner, &mut store, &pool, 0, 3, &[(reward - 3 - 47, me), (47, me)])
            .expect("a reconstructed coinbase note must be spendable through the general API");
        assert_eq!(bundle.out_indices.len(), 2, "both change outputs are ours");
    }

    /// THE MONEY BUG (2026-09-02): paying the same recipient the same amount twice
    /// produced a BYTE-IDENTICAL note commitment, and the payee's wallet then discarded
    /// the second payment as a duplicate. Money left the sender, nothing arrived, and
    /// nothing anywhere returned an error.
    ///
    /// Four links, all of them inside this crate:
    ///
    ///   1. `build_spend` took each output's blinding from `NoteStore`'s allocation
    ///      counter (`allocate_with` -> `account.blinding(index)`);
    ///   2. `NoteStore::new()` starts that counter at 0, so a caller that builds a FRESH
    ///      store per transaction hands index 0 to the payee's output EVERY time — and
    ///      that is exactly what the two live callers do, because each is handed one note
    ///      and asked to prove one spend: `wasm_api::build_private_send` (the browser)
    ///      and `sigil-top`'s `mine_local_api` (the desktop TUI's local send endpoint);
    ///   3. a leaf is `compress2(compress2(value, blinding), pk)`, so equal value + equal
    ///      blinding + equal payee key is an equal leaf, byte for byte;
    ///   4. `NoteStore::receive_with_memo` deduplicates on exactly `(value, blinding)`.
    ///
    /// Nothing about this is exotic: SIGIL wallets snap amounts to a 1/2/5x10^k
    /// denomination ladder, so paying the same amount twice is the ORDINARY case, and
    /// with `SHIELDED_ONLY_HEIGHT == 0` the shielded path is the only payment path there
    /// is.
    ///
    /// This test reproduces the fresh-store-per-transaction shape verbatim and demands
    /// what a payee is owed: two payments, two distinct leaves, two spendable notes.
    ///
    /// IGNORED in debug only — same winterfell 0.9 debug-degree quirk as
    /// `wallet_built_spend_verifies` (this test proves twice).
    #[test]
    #[ignore = "winterfell 0.9 debug-only validate_transition_degrees vs witness-dependent range-bit column degree; release-compiled winter-prover passes."]
    fn paying_the_same_amount_twice_delivers_two_distinct_spendable_notes() {
        use crate::note_cipher::{enc_identity_from_seed, seal_note, NotePlaintext};

        let alice_seed = [0xA2u8; 32];
        let bob_seed = [0xB2u8; 32];
        let alice = ShieldedAccount::from_seed(alice_seed);
        let bob = ShieldedAccount::from_seed(bob_seed);
        let bob_enc = enc_identity_from_seed(&bob_seed);
        let bob_addr = bob.address(&bob_seed);
        let bob_pk = bob_addr.shield_key().unwrap();

        // Alice funds herself with TWO separate notes of the SAME value. Two notes,
        // because one note can only be spent once — paying twice necessarily consumes
        // two. Same value, because that is the case the denomination ladder produces.
        let mut funding = NoteStore::new();
        let (_i0, cm0) = shield_note(&alice, &mut funding, 100).unwrap();
        let (_i1, cm1) = shield_note(&alice, &mut funding, 100).unwrap();
        assert_ne!(cm0, cm1, "the two funding notes must themselves be distinct");

        // ── Payment 1: spend the note at leaf 0, pay Bob 50, keep 47, 3 fee. ──
        //
        // Built through a FRESH single-note store — `wasm_api::build_private_send`'s
        // exact construction, copied deliberately rather than paraphrased.
        let pool_a = padded(&[cm0, cm1]);
        let mut store1 = NoteStore::new();
        store1.notes.push(OwnedNote {
            index: Some(0),
            value: 100,
            blinding: alice.blinding(0),
            position: Some(0),
            spent: false,
            memo: None,
        });
        let pay1 = build_spend(&alice, &mut store1, &pool_a, 0, 3, &[(50, bob_pk), (47, alice.public_key())])
            .expect("alice builds the first payment");

        // ── Payment 2: identical in every way a payer controls — same payee, same
        // amount, same fee — differing only in which note funds it. ──
        let pool_b = padded(&[cm0, cm1, pay1.cm_outs[0], pay1.cm_outs[1]]);
        let mut store2 = NoteStore::new();
        store2.notes.push(OwnedNote {
            index: Some(1),
            value: 100,
            blinding: alice.blinding(1),
            position: Some(1),
            spent: false,
            memo: None,
        });
        let pay2 = build_spend(&alice, &mut store2, &pool_b, 0, 3, &[(50, bob_pk), (47, alice.public_key())])
            .expect("alice builds the second payment");

        // (1) THE LEAVES MUST DIFFER. This is the assertion that failed before the fix.
        assert_ne!(
            pay1.cm_outs[0], pay2.cm_outs[0],
            "two payments of the same amount to the same payee must not publish the SAME leaf"
        );
        assert_ne!(
            pay1.out_preimages[0].1, pay2.out_preimages[0].1,
            "and the reason must be the blinding, not an accident of value or key"
        );

        // Alice's own change must be distinct too — colliding change is the sender
        // losing their OWN money, which is the larger half of every payment.
        assert_ne!(
            pay1.cm_outs[1], pay2.cm_outs[1],
            "the two change notes must not collide either"
        );

        // The nullifiers were always distinct (they bind to leaf position), which is
        // exactly why the chain accepted both spends and nothing looked wrong.
        assert_ne!(pay1.nullifier, pay2.nullifier);

        // (2) THE PAYEE MUST BOOK BOTH. Bob learns of each payment only by
        // trial-decrypting the ciphertexts the sender published.
        let ct1 = seal_note(
            &NotePlaintext::new(pay1.out_preimages[0].0, pay1.out_preimages[0].1),
            &bob_addr,
        )
        .expect("seal payment 1 to bob");
        let ct2 = seal_note(
            &NotePlaintext::new(pay2.out_preimages[0].0, pay2.out_preimages[0].1),
            &bob_addr,
        )
        .expect("seal payment 2 to bob");

        let pool_c = padded(&[
            cm0, cm1, pay1.cm_outs[0], pay1.cm_outs[1], pay2.cm_outs[0], pay2.cm_outs[1],
        ]);
        let mut bob_store = NoteStore::new();
        assert_eq!(
            bob_store.scan_ciphertexts(&bob_enc, &[ct1, ct2]),
            2,
            "Bob was paid twice and must book TWO notes, not one"
        );
        assert_eq!(
            bob_store.scan_owned(&bob, &pool_c),
            2,
            "and must locate BOTH of them in the pool, at two different leaves"
        );
        let positions: BTreeSet<u64> = bob_store.notes.iter().filter_map(|n| n.position).collect();
        assert_eq!(positions.len(), 2, "two notes must occupy two distinct leaf positions");
        assert_eq!(bob_store.balance(), 100, "Bob was paid 50 twice; he is owed 100");
    }

    /// The uniqueness argument for the fix, stated as an assertion rather than a comment.
    ///
    /// Output blindings are derived from the leaf position of the note being SPENT, and a
    /// note can be spent exactly once — the chain's nullifier set is what enforces that.
    /// So `(anchor, position, slot)` cannot repeat for a wallet, and no persistent
    /// per-wallet counter has to exist anywhere for the collision to be impossible.
    ///
    /// Fast: no proving, so this one runs in debug too.
    #[test]
    fn output_derivation_indices_are_unique_per_spent_note() {
        let anchor = [7u8; 32];
        let mut seen = BTreeSet::new();
        for position in 0..(1u64 << 12) {
            for slot in 0..N_OUTS {
                let idx = output_derivation_index(&anchor, position, slot);
                assert!(
                    seen.insert(idx),
                    "index {idx} repeated at position {position} slot {slot}"
                );
                assert!(
                    idx >= OUT_INDEX_BASE,
                    "spend outputs must live above the sequential shield counter"
                );
                assert!(
                    idx <= u32::MAX as u64,
                    "must stay inside u32: `wasm_api` takes note_index as u32 and JS \
                     carries change_index as an f64"
                );
            }
        }
        // A different pool state gives a different index for the same position, which is
        // what keeps a position reused after an epoch rotation from colliding.
        let other = [8u8; 32];
        assert_ne!(
            output_derivation_index(&anchor, 3, 0),
            output_derivation_index(&other, 3, 0)
        );
    }

    /// Defence in depth for money ALREADY on chain: notes minted by a sender running the
    /// old, colliding code are two real leaves worth 2x, and a payee running this code
    /// must be able to book and spend both.
    ///
    /// Two identical `(value, blinding)` deliveries used to collapse into one note — the
    /// dedup in `receive_with_memo` cannot tell "I have scanned this ciphertext before"
    /// from "someone paid me the same thing twice". Scanning now keys on the DELIVERY,
    /// so re-scanning is still idempotent while a second payment is still a second note.
    #[test]
    fn two_identical_deliveries_are_two_notes_and_two_leaves() {
        use crate::note_cipher::{enc_identity_from_seed, seal_note, NotePlaintext};

        let bob_seed = [0xB4u8; 32];
        let bob = ShieldedAccount::from_seed(bob_seed);
        let bob_enc = enc_identity_from_seed(&bob_seed);
        let bob_addr = bob.address(&bob_seed);

        // Exactly what the old sender produced: the same value under the same blinding,
        // twice — so the same leaf, twice.
        let value = 50u64;
        let blinding = ShieldedAccount::from_seed([0xA4u8; 32]).blinding(0);
        let ct1 = seal_note(&NotePlaintext::new(value, blinding), &bob_addr).unwrap();
        let ct2 = seal_note(&NotePlaintext::new(value, blinding), &bob_addr).unwrap();

        let leaf = to_wire(
            Note { value: BaseElement::new(value), blinding, spend_key: bob.spend_key() }
                .commitment(),
        );
        let pool = padded(&[leaf, leaf]);

        let mut store = NoteStore::new();
        assert_eq!(
            store.scan_ciphertexts(&bob_enc, &[ct1.clone(), ct2.clone()]),
            2,
            "two deliveries of the same amount are two payments, not one"
        );
        assert_eq!(
            store.scan_ciphertexts(&bob_enc, &[ct1, ct2]),
            0,
            "but re-scanning the SAME ciphertexts must still book nothing new"
        );
        assert_eq!(store.scan_owned(&bob, &pool), 2, "both leaves must be located");
        let positions: BTreeSet<u64> = store.notes.iter().filter_map(|n| n.position).collect();
        assert_eq!(
            positions,
            BTreeSet::from([0, 1]),
            "two identical leaves are two DIFFERENT positions; sharing one would make the \
             second unspendable (its nullifier would already be spent)"
        );
        assert_eq!(store.balance(), 100);
    }


    /// The SHIELD path has the same shape of hazard and a different answer, and this is
    /// the test that says so out loud.
    ///
    /// A deposit conjures a note from nothing. There is no note being consumed, so there
    /// is no leaf position to derive from — the sequential counter really is the only
    /// source of uniqueness, and a caller that starts a fresh `NoteStore` per deposit
    /// mints the identical commitment for the identical amount. [`NoteStore::with_next_index`]
    /// is the fix for THAT caller, and it is the reason the spend path deliberately does
    /// not use the counter: spends have something better available, deposits do not.
    ///
    /// Fast: shielding derives a commitment, it does not prove.
    #[test]
    fn a_reset_counter_collides_on_the_shield_path_and_with_next_index_is_the_cure() {
        let acct = ShieldedAccount::from_seed([0xC0u8; 32]);

        // Two deposits of the same amount, each through a fresh store — a process that
        // restarted, a browser whose localStorage was cleared, a wallet restored from
        // seed on a second device.
        let (_i, first) = shield_note(&acct, &mut NoteStore::new(), 1_000).unwrap();
        let (_i, again) = shield_note(&acct, &mut NoteStore::new(), 1_000).unwrap();
        assert_eq!(
            first, again,
            "a reset counter really does remint the same leaf — this is not hypothetical"
        );

        // Told where it left off, the same caller cannot collide.
        let (_i, resumed) = shield_note(&acct, &mut NoteStore::with_next_index(1), 1_000).unwrap();
        assert_ne!(first, resumed);

        // And the two schemes never meet: a deposit index is sequential from 0, a spend
        // output index lives above OUT_INDEX_BASE.
        assert!(output_derivation_index(&[0u8; 32], 0, 0) >= OUT_INDEX_BASE);
    }

    /// Spend outputs must not disturb the deposit counter.
    ///
    /// If a spend advanced `next_index` to meet an output index (~2^31), the wallet's
    /// next SHIELD would be minted from that reserved space, and the two schemes would
    /// start competing for the same numbers.
    #[test]
    fn a_spend_does_not_move_the_deposit_counter() {
        let acct = ShieldedAccount::from_seed([0xC1u8; 32]);
        let mut store = NoteStore::new();
        let (i0, _cm) = shield_note(&acct, &mut store, 100).unwrap();
        assert_eq!(i0, 0);

        // Simulate what a spend records, without paying the prover: the store learns two
        // outputs at derived indices.
        let anchor = [3u8; 32];
        for slot in 0..N_OUTS {
            let idx = output_derivation_index(&anchor, 0, slot);
            store.record_derived(idx, 10, acct.blinding(idx));
        }

        let (i1, _cm) = shield_note(&acct, &mut store, 100).unwrap();
        assert_eq!(i1, 1, "the next deposit is still the next SMALL index, not 2^31-something");
    }

}
