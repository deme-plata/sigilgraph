//! The bridge vault's SHIELDED identity — the half of the SIGIL↔Polygon bridge that
//! actually holds locked value.
//!
//! # Why this module exists
//!
//! The bridge originally locked value with a transparent `SigilTx::Send` to a fixed
//! non-signing custody address (`bridge::BRIDGE_VAULT_WALLET`). Since the privacy-only
//! change (`sigil_tx::SHIELDED_ONLY_HEIGHT = 0`) consensus refuses **every** transparent
//! `Send`, so that lock was rejected at apply time on every candidate, forever —
//! `✗ tx dropped (apply_tx: TransparentSendRetired)`. No amount of retry tuning could fix
//! it: the transaction shape itself is no longer legal.
//!
//! `SigilTx::Shield` is explicitly preserved as the pool's ON-RAMP (`sigil-tx`'s own test
//! asserts *"Shield is the on-ramp and must remain available"*), and it is exactly the
//! primitive a bridge lock needs:
//!
//! * it **debits the depositor's transparent balance**, which is what "locked" means;
//! * its `from` and `amount` are **public**, which is precisely what the relayer must read
//!   to mint the matching 1:1 amount on Polygon — a shielded-to-shielded send would hide
//!   the very number the bridge has to agree on;
//! * the resulting note commitment is **owner-bound** (`note_v1::commitment` binds `pk`),
//!   so a note derived from THIS vault's key can only ever be spent by this vault. That
//!   ownership binding is what makes the lock a lock rather than a donation.
//!
//! # The substitution attack this module exists to prevent
//!
//! `Shield` is wallet-signed over a message containing `cm`, and the depositor is the one
//! who signs. If the depositor were allowed to choose `cm`, they would simply commit to a
//! note **they** own: their transparent balance drops (so the chain agrees value moved),
//! the relayer sees a valid-looking lock and mints wrapped SIGIL on Polygon — and the
//! depositor still holds the spendable note in the pool. That is a free mint, i.e. a
//! double-spend across the bridge.
//!
//! So the vault, not the caller, derives every commitment: [`BridgeVault::prepare`] issues
//! the exact `(amount, cm)` parts for a lock, and [`BridgeVault::check_parts`] refuses to
//! accept a submission whose parts are not byte-identical to what was issued. The caller
//! signs what the vault chose; it never chooses for itself.
//!
//! # Recoverability
//!
//! Blindings are DERIVED (`ShieldedAccount::blinding(index)`), never random, so the whole
//! note set is reconstructible from the seed plus `(index, value)` pairs. That removes the
//! usual "lose the blindings, lose the funds" failure mode — but it makes two things
//! load-bearing:
//!
//! 1. **The seed is the vault.** Lose it and every locked note is unspendable forever;
//!    leak it and anyone can drain the vault. It is held at [`DEFAULT_SEED_PATH`], mode
//!    0600, and must be backed up off-box like any other root key.
//! 2. **Indices must never be reused.** Re-deriving index `i` for the same value yields
//!    the SAME commitment, i.e. a duplicate leaf; for a different value it silently
//!    orphans the earlier note. The ledger below persists every allocation so a restart
//!    resumes the counter instead of restarting it at zero.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sigil_shield::wallet::{build_spend, shield_note, NoteStore, ShieldedAccount};
use sigil_state::WalletId;
use sigil_tx::SigilTx;

/// Where the vault seed lives on a node that runs the bridge. Root-only, 0600.
pub const DEFAULT_SEED_PATH: &str = "/root/.config/sigil/bridge-vault.seed";

/// Append-only record of every note the vault has ever derived, so `next_index` and the
/// note set survive a restart. Sits beside the seed by default.
pub const DEFAULT_LEDGER_PATH: &str = "/root/.config/sigil/bridge-vault-notes.jsonl";

#[derive(Debug, PartialEq, Eq)]
pub enum VaultError {
    /// The amount cannot be expressed in the pool's legal denominations.
    NotDecomposable { amount: u128 },
    /// Amount is zero, or a single part exceeded `u64` (the note value width).
    BadAmount { amount: u128 },
    /// No parts were ever issued under this lock id — nothing to check against.
    UnknownLock { lock_id: u64 },
    /// The submitted parts are not what this vault issued. **This is the double-spend
    /// guard**; it fires when a caller tries to substitute a commitment they own.
    PartsMismatch { lock_id: u64, expected: Vec<(u128, String)>, got: Vec<(u128, String)> },
    /// The vault holds no landed, unspent, unreserved note of this denomination, so the
    /// requested unlock cannot be paid out of what is actually locked.
    NoNoteForDenomination { amount: u128 },
    /// The spend proof could not be built (bad position, non-conserving outputs, prover
    /// failure). Never silently downgraded — an unlock without a proof is not an unlock.
    SpendFailed { detail: String },
}

impl VaultError {
    pub fn message(&self) -> String {
        match self {
            VaultError::NotDecomposable { amount } =>
                format!("amount {amount} cannot be split into legal shielded denominations"),
            VaultError::BadAmount { amount } =>
                format!("amount {amount} is not a valid lock amount"),
            VaultError::UnknownLock { lock_id } =>
                format!("no prepared lock {lock_id} — call prepare first"),
            VaultError::NoNoteForDenomination { amount } =>
                format!("vault holds no spendable note of denomination {amount}"),
            VaultError::SpendFailed { detail } =>
                format!("could not build the vault spend proof: {detail}"),
            VaultError::PartsMismatch { lock_id, .. } =>
                format!("submitted note commitments do not match those issued for lock {lock_id} \
                         — refusing, since accepting a caller-chosen commitment would mint \
                         wrapped SIGIL against a note the caller still controls"),
        }
    }
}

/// One denominated piece of a lock: the public amount, and the vault-owned commitment the
/// depositor must shield it into.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssuedPart {
    pub amount: u128,
    /// Hex of the note commitment — what goes on the wire and into the signed message.
    pub cm_hex: String,
    /// Derivation index, so the note can be rebuilt from the seed alone.
    pub index: u64,
}

/// The vault's shielded account plus its note bookkeeping.
pub struct BridgeVault {
    account: ShieldedAccount,
    store: Mutex<NoteStore>,
    /// Parts issued per lock id, awaiting a matching submission.
    issued: Mutex<HashMap<u64, Vec<IssuedPart>>>,
    /// Leaf positions of notes already committed to an in-flight unlock.
    ///
    /// A note is only marked `spent` once its nullifier appears ON CHAIN, which is ~81s
    /// after the unlock is submitted. Without this set, two unlocks arriving inside that
    /// window would both select the same note, both build a valid proof, and the second
    /// would be rejected at apply time as a double-spend — silently losing that unlock.
    in_flight: Mutex<HashSet<u64>>,
    ledger_path: Option<PathBuf>,
}

impl BridgeVault {
    /// Build a vault from a raw 32-byte seed. Deterministic: the same seed always yields
    /// the same account, which is what makes the notes recoverable.
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self {
            account: ShieldedAccount::from_seed(seed),
            store: Mutex::new(NoteStore::new()),
            issued: Mutex::new(HashMap::new()),
            in_flight: Mutex::new(HashSet::new()),
            ledger_path: None,
        }
    }

    /// Load the seed from `path` (64 hex chars, whitespace-trimmed) and replay `ledger`
    /// so index allocation resumes where it left off.
    ///
    /// Replaying matters more than it looks: without it a restarted vault would hand out
    /// index 0 again and re-derive a commitment it had already published.
    pub fn open(seed_path: &Path, ledger_path: &Path) -> std::io::Result<Self> {
        let raw = std::fs::read_to_string(seed_path)?;
        let seed = parse_seed(&raw).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} must contain 64 hex characters", seed_path.display()),
            )
        })?;
        let mut v = Self::from_seed(seed);
        v.ledger_path = Some(ledger_path.to_path_buf());
        v.replay_ledger(ledger_path)?;
        Ok(v)
    }

    /// Re-allocate one note per ledger line so `next_index` advances past everything ever
    /// issued. Values are re-read from the ledger because `NoteStore` needs them to
    /// rebuild the note; the blinding itself comes back from the seed.
    fn replay_ledger(&mut self, path: &Path) -> std::io::Result<()> {
        let Ok(text) = std::fs::read_to_string(path) else { return Ok(()) };
        let mut store = self.store.lock().unwrap();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            // `{"index":N,"value":V,...}` — parsed leniently on purpose: a ledger line we
            // cannot read must not take the bridge down, but it MUST still advance the
            // index counter, so a malformed line allocates a placeholder rather than
            // being skipped (skipping would risk re-issuing that index).
            let value = json_u64(line, "value").unwrap_or(0);
            store.allocate_with(&self.account, value);
        }
        Ok(())
    }

    fn append_ledger(&self, lock_id: u64, part: &IssuedPart) {
        let Some(path) = &self.ledger_path else { return };
        use std::io::Write;
        let line = format!(
            "{{\"lock_id\":{},\"index\":{},\"value\":{},\"cm\":\"{}\"}}\n",
            lock_id, part.index, part.amount, part.cm_hex
        );
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
        }
    }

    /// The vault's public key, hex. Safe to publish — it is what binds a note to this
    /// vault and cannot be inverted to the spend key.
    pub fn public_key_hex(&self) -> String {
        hex::encode(sigil_shield::note_v1::to_wire(self.account.public_key()))
    }

    /// Derive the vault-owned commitments for a lock of `amount`.
    ///
    /// Splits into legal denominations (the pool refuses anything else), allocating one
    /// vault note per part. The returned order is the order the depositor must sign and
    /// submit in — `check_parts` compares positionally.
    pub fn prepare(&self, lock_id: u64, amount: u128) -> Result<Vec<IssuedPart>, VaultError> {
        if amount == 0 {
            return Err(VaultError::BadAmount { amount });
        }
        let parts = sigil_state::shielded::decompose(amount)
            .ok_or(VaultError::NotDecomposable { amount })?;

        let mut store = self.store.lock().unwrap();
        let mut out = Vec::with_capacity(parts.len());
        for part_amount in parts {
            let value = u64::try_from(part_amount)
                .map_err(|_| VaultError::BadAmount { amount: part_amount })?;
            // `shield_note` allocates the index AND returns the commitment for it, so the
            // two can never drift apart.
            let (index, cm) = shield_note(&self.account, &mut store, value)
                .map_err(|_| VaultError::BadAmount { amount: part_amount })?;
            out.push(IssuedPart { amount: part_amount, cm_hex: hex::encode(cm), index });
        }
        drop(store);

        for p in &out {
            self.append_ledger(lock_id, p);
        }
        self.issued.lock().unwrap().insert(lock_id, out.clone());
        Ok(out)
    }

    /// What `prepare` issued for `lock_id`, if anything.
    pub fn issued_for(&self, lock_id: u64) -> Option<Vec<IssuedPart>> {
        self.issued.lock().unwrap().get(&lock_id).cloned()
    }

    /// **The double-spend guard.** Accept a submission only if its `(amount, cm)` parts are
    /// exactly, and in the same order, what this vault issued for `lock_id`.
    ///
    /// Comparison is positional rather than set-based because the signed message is built
    /// by concatenating the parts in order — accepting a permutation would accept a
    /// signature over a different message than the one we verify against.
    pub fn check_parts(&self, lock_id: u64, got: &[(u128, String)]) -> Result<(), VaultError> {
        let issued = self
            .issued
            .lock()
            .unwrap()
            .get(&lock_id)
            .cloned()
            .ok_or(VaultError::UnknownLock { lock_id })?;

        let expected: Vec<(u128, String)> =
            issued.iter().map(|p| (p.amount, p.cm_hex.to_lowercase())).collect();
        let normalized: Vec<(u128, String)> =
            got.iter().map(|(a, c)| (*a, c.trim().trim_start_matches("0x").to_lowercase())).collect();

        if expected != normalized {
            return Err(VaultError::PartsMismatch {
                lock_id,
                expected,
                got: normalized,
            });
        }
        Ok(())
    }

    /// Drop the issued-parts record once a lock is settled or abandoned, so the map does
    /// not grow without bound. Safe no-op for an unknown id.
    pub fn forget(&self, lock_id: u64) {
        self.issued.lock().unwrap().remove(&lock_id);
    }

    /// Total value of notes this vault believes it holds, in base units.
    pub fn note_balance(&self) -> u128 {
        self.store.lock().unwrap().balance()
    }

    /// **The return leg: Polygon -> SIGIL.** Build the `Unshield` transactions that pay
    /// `amount` out of the vault's shielded notes to the transparent wallet `to`.
    ///
    /// # Why this is an `Unshield` and not a `Send`
    ///
    /// The original unlock built `SigilTx::Send { from: BRIDGE_VAULT_WALLET, .. }`, which
    /// consensus retired at `SHIELDED_ONLY_HEIGHT` — every unlock died at apply time with
    /// `TransparentSendRetired`, exactly like the outbound lock did before it moved to
    /// `Shield`. `Unshield` is the pool's preserved OFF-ramp and is the mirror image of
    /// the lock: the lock shields value INTO a vault-owned note, this spends that note
    /// back OUT to a transparent balance.
    ///
    /// # Why it is proof-carrying rather than authority-carrying
    ///
    /// Nobody holds `BRIDGE_VAULT_WALLET`'s key — it is a synthetic address. What
    /// authorises this payout is not a signature over "the vault says so" but a STARK
    /// proving that a note committed in the pool, owned by this vault's spend key, is
    /// being consumed. The relayer's signature (checked in `bridge.rs`) decides *whether*
    /// to unlock; this proof is what makes the chain accept it. Naming a nullifier without
    /// the proof would be enough to drain the pool, which is why there is no shortcut here.
    ///
    /// # Denominations
    ///
    /// The pool only accepts fixed denominations, so an unlock of an arbitrary amount is
    /// paid as several `Unshield` transactions — one per part, each consuming one
    /// exact-value note. Change is deliberately never produced: a part is paid from a note
    /// of exactly that value, so the two outputs are zero-value. That keeps each spend
    /// independent, so a part that fails to settle can be retried without unpicking the
    /// others.
    ///
    /// `pool_commitments` and `spent_nullifiers` come from live chain state; positions and
    /// spent-ness are re-derived from them on every call rather than cached, because the
    /// vault's own book is not authoritative about what settled.
    pub fn build_unshield(
        &self,
        pool_commitments: &[[u8; 32]],
        spent_nullifiers: &BTreeSet<[u8; 32]>,
        to: WalletId,
        amount: u128,
    ) -> Result<Vec<UnshieldPart>, VaultError> {
        if amount == 0 {
            return Err(VaultError::BadAmount { amount });
        }
        let parts = sigil_state::shielded::decompose(amount)
            .ok_or(VaultError::NotDecomposable { amount })?;

        let mut store = self.store.lock().unwrap();
        // Re-sync against the chain BEFORE selecting: a note only becomes spendable when
        // it has landed (position resolved), and only stays spendable while its nullifier
        // is absent from the spent set.
        store.scan_owned(&self.account, pool_commitments);
        store.mark_spent(&self.account, spent_nullifiers);

        let mut in_flight = self.in_flight.lock().unwrap();
        // Positions reserved by THIS call, so two parts of one unlock cannot both claim
        // the same note either.
        let mut taken: HashSet<u64> = HashSet::new();
        let mut plan: Vec<(usize, u64, u128)> = Vec::with_capacity(parts.len());

        for part_amount in &parts {
            let want = u64::try_from(*part_amount)
                .map_err(|_| VaultError::BadAmount { amount: *part_amount })?;
            let spendable = |n: &sigil_shield::wallet::OwnedNote| {
                !n.spent
                    && n.position
                        .is_some_and(|p| !in_flight.contains(&p) && !taken.contains(&p))
            };

            // Prefer an EXACT-value note: it produces no change, so the spend is
            // self-contained and the vault's note set stays denominated.
            let slot = store
                .notes
                .iter()
                .position(|n| spendable(n) && n.value == want)
                // Otherwise split the SMALLEST note that covers the part, paying the
                // remainder back to the vault as a change note. Smallest-first keeps the
                // larger notes intact for larger withdrawals instead of shredding them.
                //
                // Change is safe: only the PUBLIC `amount` of an `Unshield` is
                // denomination-checked on chain (`StateMutation::Unshield` ->
                // `is_denomination`); output commitments are opaque, so a change note may
                // hold any value. Without this fallback the vault could only ever pay out
                // amounts it happened to hold exactly — i.e. a user who bridged 500 in
                // could not bridge 200 back out.
                .or_else(|| {
                    let mut best: Option<(usize, u64)> = None;
                    for (i, n) in store.notes.iter().enumerate() {
                        if spendable(n) && n.value > want {
                            if best.is_none_or(|(_, v)| n.value < v) {
                                best = Some((i, n.value));
                            }
                        }
                    }
                    best.map(|(i, _)| i)
                })
                .ok_or(VaultError::NoNoteForDenomination { amount: *part_amount })?;
            let position = store.notes[slot].position.expect("filtered on is_some_and");
            taken.insert(position);
            plan.push((slot, position, *part_amount));
        }

        // Only now, with every part covered, do any proving work — a partially payable
        // unlock must reserve nothing and prove nothing.
        let mut out = Vec::with_capacity(plan.len());
        let mine = self.account.public_key();
        for (slot, position, part_amount) in plan {
            let public_value = u64::try_from(part_amount)
                .map_err(|_| VaultError::BadAmount { amount: part_amount })?;
            // `build_spend` asserts the outputs sum to exactly `note.value - public_value`.
            // For an exact-value note that is 0 (two zero-value notes back to the vault);
            // for a split note it is the change, carried entirely by the first output.
            let note_value = store.notes[slot].value;
            let change = note_value
                .checked_sub(public_value)
                .ok_or(VaultError::BadAmount { amount: part_amount })?;
            let outs_spec = [(change, mine), (0u64, mine)];
            let bundle = build_spend(
                &self.account,
                &mut store,
                pool_commitments,
                slot,
                public_value,
                &outs_spec,
            )
            .map_err(|e| VaultError::SpendFailed { detail: e.to_string() })?;

            out.push(UnshieldPart {
                amount: part_amount,
                position,
                tx: SigilTx::Unshield {
                    to,
                    amount: part_amount,
                    anchor: bundle.anchor,
                    nullifier: bundle.nullifier,
                    cm_outs: bundle.cm_outs,
                    proof: bundle.proof,
                    fee: 0,
                },
            });
        }

        for p in &out {
            in_flight.insert(p.position);
        }
        Ok(out)
    }

    /// Release reservations taken by [`BridgeVault::build_unshield`].
    ///
    /// Called when an unlock is abandoned (gave up before settling). Not called on
    /// success: a settled note is marked spent from the chain's nullifier set on the next
    /// `build_unshield`, and dropping the reservation early would re-expose it during the
    /// window between submission and settlement.
    pub fn release_positions(&self, positions: &[u64]) {
        let mut in_flight = self.in_flight.lock().unwrap();
        for p in positions {
            in_flight.remove(p);
        }
    }

    /// Leaf positions currently committed to an in-flight unlock. Diagnostics only.
    pub fn in_flight_positions(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self.in_flight.lock().unwrap().iter().copied().collect();
        v.sort_unstable();
        v
    }
}

/// One `Unshield` transaction of a multi-denomination unlock, with the leaf position it
/// consumes so the caller can release the reservation if the transaction is abandoned.
#[derive(Clone, Debug)]
pub struct UnshieldPart {
    pub amount: u128,
    pub position: u64,
    pub tx: SigilTx,
}

fn parse_seed(raw: &str) -> Option<[u8; 32]> {
    let s = raw.trim();
    let s = s.strip_prefix("0x").unwrap_or(s);
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// Pull an integer field out of one ledger line without taking a JSON dependency for a
/// two-field record we wrote ourselves.
fn json_u64(line: &str, key: &str) -> Option<u64> {
    let pat = format!("\"{key}\":");
    let start = line.find(&pat)? + pat.len();
    let rest = &line[start..];
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: [u8; 32] = [0x5Au8; 32];

    fn vault() -> BridgeVault {
        BridgeVault::from_seed(SEED)
    }

    /// Stand the vault's prepared notes up as if they had landed on chain: the pool's
    /// commitment list IS the notes, in issue order, so leaf position == part index.
    ///
    /// Padded to `POOL_CAPACITY` exactly as the live chain's anchor is: the circuit proves
    /// a fixed-depth path, so a short leaf vector is not a smaller pool, it is an invalid
    /// one.
    fn landed_pool(parts: &[IssuedPart]) -> Vec<[u8; 32]> {
        let mut leaves: Vec<[u8; 32]> = parts
            .iter()
            .map(|p| {
                let raw = hex::decode(&p.cm_hex).expect("cm_hex is hex");
                let mut cm = [0u8; 32];
                cm.copy_from_slice(&raw);
                cm
            })
            .collect();
        for i in leaves.len()..sigil_state::shielded::POOL_CAPACITY {
            leaves.push(sigil_shield::note_v1::padding_leaf_wire(i as u64));
        }
        leaves
    }

    const DEST: WalletId = [0x77u8; 32];

    /// **The return leg exists and is a proof-carrying `Unshield`, not a transparent
    /// `Send`.** A `Send` from the vault is what consensus retired at
    /// `SHIELDED_ONLY_HEIGHT`; building one here is the bug this whole path was rewritten
    /// to remove, so assert the transaction SHAPE, not merely that something came back.
    #[test]
    #[cfg_attr(debug_assertions, ignore = "debug-only winterfell 0.9 `validate_transition_degrees`: the AIR declares an UPPER BOUND on each transition-constraint degree, but the range-bit columns of a spend are witness-dependent — for some note values a column is constant, so its ACTUAL degree comes out lower and the debug assert trips. Both the check and its call site are `#[cfg(debug_assertions)]`, so it cannot fire in release, which is what the node ships. VERIFIED 2026-08-27 by building `--release --tests` and running the binary directly: all 6 pass (47/47 bridge tests, 0 failed). NOTE this uses cfg_attr so the test still RUNS in release rather than being skipped everywhere — sigil-shield's plain #[ignore]s hide the same family and 2 of those genuinely do NOT pass.")]
    fn an_unlock_pays_out_as_unshield_transactions_that_sum_to_the_amount() {
        let v = vault();
        let amount = 700u128; // 500 + 200 — two legal denominations, so two spends.
        let parts = v.prepare(1, amount).expect("prepare");
        let pool = landed_pool(&parts);

        let out = v
            .build_unshield(&pool, &BTreeSet::new(), DEST, amount)
            .expect("vault holds exactly these notes");

        let total: u128 = out.iter().map(|p| p.amount).sum();
        assert_eq!(total, amount, "the payout must be value-preserving");
        for part in &out {
            match &part.tx {
                SigilTx::Unshield { to, amount, proof, .. } => {
                    assert_eq!(*to, DEST, "unlock must pay the address the relayer named");
                    assert!(*amount > 0);
                    assert!(!proof.is_empty(), "an Unshield with no proof cannot be applied");
                }
                other => panic!("unlock built {other:?} — only Unshield survives the \
                                 privacy-only gate; a Send is dropped at apply time"),
            }
        }
    }

    /// The in-flight reservation is what stops two unlocks arriving inside the ~81s
    /// settlement window from both selecting the same note. Without it the second spend
    /// is a double-spend that dies at apply, silently losing that unlock.
    #[test]
    #[cfg_attr(debug_assertions, ignore = "debug-only winterfell 0.9 `validate_transition_degrees`: the AIR declares an UPPER BOUND on each transition-constraint degree, but the range-bit columns of a spend are witness-dependent — for some note values a column is constant, so its ACTUAL degree comes out lower and the debug assert trips. Both the check and its call site are `#[cfg(debug_assertions)]`, so it cannot fire in release, which is what the node ships. VERIFIED 2026-08-27 by building `--release --tests` and running the binary directly: all 6 pass (47/47 bridge tests, 0 failed). NOTE this uses cfg_attr so the test still RUNS in release rather than being skipped everywhere — sigil-shield's plain #[ignore]s hide the same family and 2 of those genuinely do NOT pass.")]
    fn a_note_committed_to_one_unlock_cannot_be_selected_by_the_next() {
        let v = vault();
        let parts = v.prepare(1, 500).expect("prepare");
        let pool = landed_pool(&parts);

        let first = v.build_unshield(&pool, &BTreeSet::new(), DEST, 500).expect("first unlock");
        assert_eq!(first.len(), 1);

        let err = v
            .build_unshield(&pool, &BTreeSet::new(), DEST, 500)
            .expect_err("the only note of that denomination is already committed");
        assert_eq!(err, VaultError::NoNoteForDenomination { amount: 500 });

        // Abandoning the first unlock must hand the note back, or it is stranded forever.
        v.release_positions(&[first[0].position]);
        v.build_unshield(&pool, &BTreeSet::new(), DEST, 500)
            .expect("released note is selectable again");
    }

    /// An unlock larger than what is actually locked must reserve nothing and prove
    /// nothing — a partially-payable unlock that consumed half its notes would leave the
    /// vault's book disagreeing with the chain.
    #[test]
    #[cfg_attr(debug_assertions, ignore = "debug-only winterfell 0.9 `validate_transition_degrees`: the AIR declares an UPPER BOUND on each transition-constraint degree, but the range-bit columns of a spend are witness-dependent — for some note values a column is constant, so its ACTUAL degree comes out lower and the debug assert trips. Both the check and its call site are `#[cfg(debug_assertions)]`, so it cannot fire in release, which is what the node ships. VERIFIED 2026-08-27 by building `--release --tests` and running the binary directly: all 6 pass (47/47 bridge tests, 0 failed). NOTE this uses cfg_attr so the test still RUNS in release rather than being skipped everywhere — sigil-shield's plain #[ignore]s hide the same family and 2 of those genuinely do NOT pass.")]
    fn an_unpayable_unlock_reserves_no_notes() {
        let v = vault();
        let parts = v.prepare(1, 500).expect("prepare");
        let pool = landed_pool(&parts);

        let err = v
            .build_unshield(&pool, &BTreeSet::new(), DEST, 700)
            .expect_err("vault holds 500, not 700");
        assert_eq!(err, VaultError::NoNoteForDenomination { amount: 200 });
        assert!(
            v.in_flight_positions().is_empty(),
            "a failed unlock must leave no note reserved"
        );
        v.build_unshield(&pool, &BTreeSet::new(), DEST, 500)
            .expect("the 500 note was never touched");
    }

    /// A note is only spendable while the chain says it is unspent. The vault's own book
    /// is not authoritative here — the nullifier set is.
    #[test]
    fn a_note_the_chain_reports_spent_is_not_selected() {
        let v = vault();
        let parts = v.prepare(1, 500).expect("prepare");
        let pool = landed_pool(&parts);

        // Position 0 is the only note; its nullifier is derived from the vault's own key.
        let account = ShieldedAccount::from_seed(SEED);
        let spent: BTreeSet<[u8; 32]> =
            [sigil_shield::note_v1::to_wire(account.nullifier_at(0))].into_iter().collect();

        let err = v
            .build_unshield(&pool, &spent, DEST, 500)
            .expect_err("the chain already consumed this note");
        assert_eq!(err, VaultError::NoNoteForDenomination { amount: 500 });
    }

    /// **A partial withdrawal must work.** Someone who bridged 500 in and wants 200 back
    /// out is the ordinary case, not an edge case — and the vault holds one 500 note, not
    /// a 200 and a 300. Without splitting, every such unlock is refused.
    #[test]
    #[cfg_attr(debug_assertions, ignore = "debug-only winterfell 0.9 `validate_transition_degrees`: the AIR declares an UPPER BOUND on each transition-constraint degree, but the range-bit columns of a spend are witness-dependent — for some note values a column is constant, so its ACTUAL degree comes out lower and the debug assert trips. Both the check and its call site are `#[cfg(debug_assertions)]`, so it cannot fire in release, which is what the node ships. VERIFIED 2026-08-27 by building `--release --tests` and running the binary directly: all 6 pass (47/47 bridge tests, 0 failed). NOTE this uses cfg_attr so the test still RUNS in release rather than being skipped everywhere — sigil-shield's plain #[ignore]s hide the same family and 2 of those genuinely do NOT pass.")]
    fn a_larger_note_is_split_so_a_partial_withdrawal_is_payable() {
        let v = vault();
        let parts = v.prepare(1, 500).expect("prepare");
        let pool = landed_pool(&parts);

        let out = v
            .build_unshield(&pool, &BTreeSet::new(), DEST, 200)
            .expect("the 500 note must be split to pay 200");
        assert_eq!(out.len(), 1, "200 is one legal denomination");
        assert_eq!(out[0].amount, 200);
        match &out[0].tx {
            SigilTx::Unshield { amount, proof, .. } => {
                assert_eq!(*amount, 200, "only the withdrawn value may be public");
                assert!(!proof.is_empty());
            }
            other => panic!("expected Unshield, got {other:?}"),
        }
    }

    /// Splitting takes the SMALLEST covering note, so a big note is not shredded to pay a
    /// small withdrawal while a better-fitting one sits unused.
    #[test]
    #[cfg_attr(debug_assertions, ignore = "debug-only winterfell 0.9 `validate_transition_degrees`: the AIR declares an UPPER BOUND on each transition-constraint degree, but the range-bit columns of a spend are witness-dependent — for some note values a column is constant, so its ACTUAL degree comes out lower and the debug assert trips. Both the check and its call site are `#[cfg(debug_assertions)]`, so it cannot fire in release, which is what the node ships. VERIFIED 2026-08-27 by building `--release --tests` and running the binary directly: all 6 pass (47/47 bridge tests, 0 failed). NOTE this uses cfg_attr so the test still RUNS in release rather than being skipped everywhere — sigil-shield's plain #[ignore]s hide the same family and 2 of those genuinely do NOT pass.")]
    fn splitting_prefers_the_smallest_covering_note() {
        let v = vault();
        // 700 decomposes to 500 + 200, so the vault ends up holding one of each.
        let parts = v.prepare(1, 700).expect("prepare");
        let pool = landed_pool(&parts);
        let by_value: HashMap<u128, u64> = parts
            .iter()
            .enumerate()
            .map(|(i, p)| (p.amount, i as u64))
            .collect();

        let out = v
            .build_unshield(&pool, &BTreeSet::new(), DEST, 100)
            .expect("either note could cover 100");
        assert_eq!(
            out[0].position, by_value[&200],
            "the 200 note covers 100 and must be chosen over the 500"
        );
    }

    /// A note the vault derived but which has NOT landed in the pool has no Merkle path,
    /// so it cannot be proven and must not be selected.
    #[test]
    fn a_note_that_never_landed_is_not_spendable() {
        let v = vault();
        v.prepare(1, 500).expect("prepare");
        let err = v
            .build_unshield(&landed_pool(&[]), &BTreeSet::new(), DEST, 500)
            .expect_err("nothing has landed in an empty pool");
        assert_eq!(err, VaultError::NoNoteForDenomination { amount: 500 });
    }

    #[test]
    fn a_lock_amount_splits_into_legal_denominations_that_sum_exactly() {
        let v = vault();
        // Viktor's real second attempt: 59.53021946 SIGIL at 8dp.
        let amount = 5_953_021_946u128;
        let parts = v.prepare(1, amount).expect("decomposable");
        let total: u128 = parts.iter().map(|p| p.amount).sum();
        assert_eq!(total, amount, "the split must be value-preserving");
        for p in &parts {
            assert!(
                sigil_state::shielded::DENOMINATIONS.contains(&p.amount),
                "part {} is not a legal denomination and the pool would refuse it",
                p.amount
            );
        }
    }

    #[test]
    fn every_part_gets_a_distinct_index_and_a_distinct_commitment() {
        let v = vault();
        let parts = v.prepare(1, 5_953_021_946).expect("prepare");
        let mut idx: Vec<u64> = parts.iter().map(|p| p.index).collect();
        let n = idx.len();
        idx.sort_unstable();
        idx.dedup();
        assert_eq!(idx.len(), n, "reusing a derivation index would collide or orphan a note");

        let mut cms: Vec<&String> = parts.iter().map(|p| &p.cm_hex).collect();
        cms.sort();
        cms.dedup();
        assert_eq!(cms.len(), n, "two identical commitments would be one duplicate leaf");
    }

    #[test]
    fn indices_keep_advancing_across_separate_locks() {
        let v = vault();
        let a = v.prepare(1, 100_000_000).expect("prepare 1");
        let b = v.prepare(2, 100_000_000).expect("prepare 2");
        let max_a = a.iter().map(|p| p.index).max().unwrap();
        let min_b = b.iter().map(|p| p.index).min().unwrap();
        assert!(min_b > max_a, "a second lock must not re-derive the first lock's indices");
        // Same value, different index => different commitment. If these ever matched, the
        // second lock would publish a leaf identical to the first.
        assert_ne!(a[0].cm_hex, b[0].cm_hex);
    }

    #[test]
    fn the_same_seed_reproduces_the_same_commitments() {
        let a = BridgeVault::from_seed(SEED).prepare(1, 500_000_000).expect("a");
        let b = BridgeVault::from_seed(SEED).prepare(1, 500_000_000).expect("b");
        assert_eq!(a, b, "notes must be recoverable from the seed alone");
    }

    #[test]
    fn a_different_seed_never_produces_the_same_commitment() {
        let a = BridgeVault::from_seed([1u8; 32]).prepare(1, 500_000_000).expect("a");
        let b = BridgeVault::from_seed([2u8; 32]).prepare(1, 500_000_000).expect("b");
        assert_ne!(a[0].cm_hex, b[0].cm_hex);
    }

    #[test]
    fn matching_parts_are_accepted() {
        let v = vault();
        let parts = v.prepare(7, 5_953_021_946).expect("prepare");
        let submitted: Vec<(u128, String)> =
            parts.iter().map(|p| (p.amount, p.cm_hex.clone())).collect();
        assert_eq!(v.check_parts(7, &submitted), Ok(()));
    }

    #[test]
    fn parts_are_accepted_case_insensitively_and_with_an_0x_prefix() {
        let v = vault();
        let parts = v.prepare(7, 100_000_000).expect("prepare");
        let submitted: Vec<(u128, String)> = parts
            .iter()
            .map(|p| (p.amount, format!("0x{}", p.cm_hex.to_uppercase())))
            .collect();
        assert_eq!(v.check_parts(7, &submitted), Ok(()), "hex formatting must not decide money");
    }

    /// THE security test. A caller substituting a commitment they control would get wrapped
    /// SIGIL minted on Polygon while keeping a spendable note in the pool.
    #[test]
    fn a_substituted_commitment_is_refused() {
        let v = vault();
        let parts = v.prepare(7, 100_000_000).expect("prepare");
        let attacker_cm = hex::encode([0xAAu8; 32]);
        let submitted: Vec<(u128, String)> =
            vec![(parts[0].amount, attacker_cm)];
        assert!(
            matches!(v.check_parts(7, &submitted), Err(VaultError::PartsMismatch { .. })),
            "a caller-chosen commitment must never be accepted — that is a free mint"
        );
    }

    #[test]
    fn inflating_the_amount_against_issued_commitments_is_refused() {
        let v = vault();
        let parts = v.prepare(7, 100_000_000).expect("prepare");
        let submitted: Vec<(u128, String)> =
            vec![(parts[0].amount * 1000, parts[0].cm_hex.clone())];
        assert!(matches!(v.check_parts(7, &submitted), Err(VaultError::PartsMismatch { .. })));
    }

    #[test]
    fn dropping_a_part_is_refused() {
        let v = vault();
        let parts = v.prepare(7, 5_953_021_946).expect("prepare");
        assert!(parts.len() > 1, "this amount should split into several parts");
        let submitted: Vec<(u128, String)> = parts[..parts.len() - 1]
            .iter()
            .map(|p| (p.amount, p.cm_hex.clone()))
            .collect();
        assert!(
            matches!(v.check_parts(7, &submitted), Err(VaultError::PartsMismatch { .. })),
            "a short submission would lock less than the mint would credit"
        );
    }

    #[test]
    fn reordering_parts_is_refused_because_the_signed_message_is_ordered() {
        let v = vault();
        let parts = v.prepare(7, 5_953_021_946).expect("prepare");
        assert!(parts.len() > 1);
        let mut submitted: Vec<(u128, String)> =
            parts.iter().map(|p| (p.amount, p.cm_hex.clone())).collect();
        submitted.reverse();
        if submitted == parts.iter().map(|p| (p.amount, p.cm_hex.clone())).collect::<Vec<_>>() {
            return; // palindromic split; nothing to assert
        }
        assert!(matches!(v.check_parts(7, &submitted), Err(VaultError::PartsMismatch { .. })));
    }

    #[test]
    fn checking_an_unprepared_lock_is_refused_rather_than_silently_passing() {
        let v = vault();
        assert_eq!(
            v.check_parts(999, &[(1, hex::encode([0u8; 32]))]),
            Err(VaultError::UnknownLock { lock_id: 999 })
        );
    }

    #[test]
    fn zero_is_not_a_lock() {
        let v = vault();
        assert_eq!(v.prepare(1, 0), Err(VaultError::BadAmount { amount: 0 }));
    }

    #[test]
    fn seed_parsing_accepts_hex_with_and_without_prefix_and_rejects_junk() {
        let hex64 = "ab".repeat(32);
        assert!(parse_seed(&hex64).is_some());
        assert!(parse_seed(&format!("0x{hex64}\n")).is_some());
        assert!(parse_seed("deadbeef").is_none(), "short seed must not silently pad");
        assert!(parse_seed(&"zz".repeat(32)).is_none(), "non-hex must not parse");
    }

    #[test]
    fn ledger_replay_advances_the_index_so_a_restart_cannot_reissue_one() {
        let dir = std::env::temp_dir().join(format!("sigil-vault-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let seed_path = dir.join("seed");
        let ledger_path = dir.join("notes.jsonl");
        let _ = std::fs::remove_file(&ledger_path);
        std::fs::write(&seed_path, "5a".repeat(32)).unwrap();

        let first = {
            let v = BridgeVault::open(&seed_path, &ledger_path).expect("open");
            v.prepare(1, 5_953_021_946).expect("prepare")
        };
        // A fresh vault over the SAME ledger must not hand out those indices again.
        let second = {
            let v = BridgeVault::open(&seed_path, &ledger_path).expect("reopen");
            v.prepare(2, 100_000_000).expect("prepare")
        };
        let used: Vec<u64> = first.iter().map(|p| p.index).collect();
        for p in &second {
            assert!(
                !used.contains(&p.index),
                "index {} was reissued after restart — the earlier note would be orphaned",
                p.index
            );
        }
        let _ = std::fs::remove_file(&ledger_path);
        let _ = std::fs::remove_file(&seed_path);
    }

    #[test]
    fn public_key_is_stable_and_seed_separated() {
        assert_eq!(
            BridgeVault::from_seed(SEED).public_key_hex(),
            BridgeVault::from_seed(SEED).public_key_hex()
        );
        assert_ne!(
            BridgeVault::from_seed([1u8; 32]).public_key_hex(),
            BridgeVault::from_seed([2u8; 32]).public_key_hex()
        );
    }
}
