//! bridge.rs — the SIGIL <-> Polygon lock/unlock bridge surface.
//!
//! Mirrors [`crate::send::SendBridge`]'s proven confirm-on-settle pending-pool
//! shape (same reason: the braid mints several competing candidates per
//! height before settlement picks a winner — a `.drain()`-style pool loses
//! anything embedded in an orphaned sibling). Both directions here settle as
//! plain `SigilTx::Send` transactions against one fixed, non-signing custody
//! address (`BRIDGE_VAULT_WALLET`) — the same "custody wallet is just
//! another `WalletId`" pattern `sigil-usds::VAULT` / `sigil-bank::
//! CREDIT_VAULT_WALLET` already use, so the vault's balance is provably
//! included in `wallet_state_root` exactly like any user wallet, and the
//! chain's normal overflow/21M-cap checks at `commit_state_transition`
//! apply uniformly — no separate bridge-specific accounting to trust.
//!
//! ## Two directions, two different authenticators
//!
//! **Lock** (SIGIL -> Polygon): a real user, signing with their own key,
//! sending real SIGIL to the well-known vault address. This is exactly a
//! normal wallet send — no new signing scheme, except the signed message
//! ALSO binds the destination Polygon address, so nothing (a compromised
//! relayer, a MITM'd request) can silently redirect where the wrapped
//! tokens land without invalidating the signature.
//!
//! **Unlock** (Polygon -> SIGIL): nobody holds the vault's private key —
//! it's a synthetic, non-signing custody address, same as `sigil-usds::
//! VAULT`. Authorization instead comes from a dedicated, ADMIN-ROTATABLE
//! relayer wallet, whose signature is checked against the CURRENT
//! `relayer_wallet` (not baked in at construction), so a compromised
//! relayer key can be rotated out by the admin without redeploying
//! anything. `polygon_burn_tx` is the relayer's claim of which Polygon burn
//! this unlock corresponds to — deduped in `processed_burns` so the exact
//! same burn can never be unlocked twice, mirroring the mining path's
//! `share_seen` replay guard.
//!
//! ## Trust model, explicitly
//!
//! - `admin_wallet` is fixed at construction (env-configured — see
//!   `sigil-node/src/main.rs`) and can pause the bridge or rotate the
//!   relayer. It can NEVER move funds directly — no admin-authorized unlock
//!   path exists on purpose, so a compromised/careless admin key can freeze
//!   the bridge but not drain the vault.
//! - `relayer_wallet` does the day-to-day unlocking, capped in scope to
//!   exactly that (it cannot pause itself, cannot rotate itself, cannot
//!   touch `admin_wallet`).
//! - Neither role can EVER re-route a `lock` — that's authenticated solely
//!   by the depositing user's own signature, which the vault-mutation
//!   itself is derived from.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Serialize;
use sigil_header::{PubKeyBytes, SigScheme, SignatureBytes};
use sigil_state::WalletId;
use sigil_tx::{SigilTx, SignedTx};

/// Fixed, non-signing custody address — nobody holds this key, exactly like
/// `sigil-usds::VAULT` (`[0x0B; 32]`) / `sigil-bank::CREDIT_VAULT_WALLET`
/// (`[0xCF; 32]`). Chosen distinct from both.
pub const BRIDGE_VAULT_WALLET: WalletId = [0xB2u8; 32];

/// How many candidate mints a pending lock/unlock may ride before it is dropped,
/// and how long it may stay pending. **Both were the reason no bridge lock could
/// EVER complete** — same bug, same cause, as the one already fixed in
/// `shielded.rs` on 2026-08-24; that fix was simply never propagated here.
///
/// **Why the old 60s was unreachable, arithmetically.** A tx only retires via
/// `confirm_applied`, which `dag_drain_apply` calls only for a candidate that has
/// landed on the SETTLED spine. Settlement is gated on `Braid::finalized_height()`,
/// which is `tip - final_depth` with `final_depth = 512` (`sigil-dagknight`'s
/// `BraidConfig`). At the block rate this node actually runs — measured 8.2-8.6
/// blk/s — that is `512 / 8.4 ≈ 61s` from a candidate being proposed to it being
/// eligible to settle *at the very best*. A tx that gives up at 60s therefore dies
/// roughly one second BEFORE the earliest moment it could possibly land. Not a race,
/// not an edge case: no lock could ever succeed.
///
/// Confirmed by direct live reproduction, exactly as the shielded bug was — a real
/// user lock of 49.47260948 SIGIL on 2026-08-26 logged
/// `✗ bridge lock gave up after 402 attempts / 60.1s hash=806884e5...`, the vault
/// balance stayed `0`, and the relayer had nothing backed to mint.
///
/// And 61s is only the FLOOR. `computed_final` additionally clamps the finality
/// line to `pending_floor - 1`, so a single pending block whose parent never
/// arrives pins finality until the `pending_max_tip_lag` / `max_window` escape
/// hatches fire — minutes, not seconds. 40 minutes gives real margin over that
/// worst case rather than over the happy path, which is precisely the value and
/// the reasoning `shielded.rs` settled on.
///
/// `MAX_ATTEMPTS` is raised in step so it cannot become the new accidental limiter:
/// the offer cadence is one per candidate mint (~8.5/s measured), so covering a
/// 2_400s window needs ~20k offers. This is a runaway backstop, not the real bound —
/// `MAX_AGE` is deliberately the binding one, since it is the quantity tied to
/// finality.
const MAX_ATTEMPTS: u32 = 30_000;
/// How long a bridge lock/unlock may stay pending before it is dropped.
const MAX_AGE: Duration = Duration::from_secs(2_400);

struct Pending {
    tx: SigilTx,
    attempts: u32,
    first_seen: Instant,
    /// For an unlock: the vault note leaf position this transaction consumes. Kept so a
    /// give-up can hand the note back instead of stranding it as permanently reserved.
    vault_position: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeError {
    BadWalletAddress,
    ZeroAmount,
    BadSignatureEncoding,
    SignatureInvalid,
    ReplayedNonce,
    BadDestAddress,
    NotActivated,
    Paused,
    NotAdmin,
    NotRelayer,
    BurnAlreadyProcessed,
    /// No shielded vault configured — the lock path is unavailable rather than silently
    /// emitting a transparent `Send` consensus would drop.
    VaultNotConfigured,
    /// `submit_lock` was called for a lock id `prepare_lock` never issued.
    LockNotPrepared { lock_id: u64 },
    /// The vault refused the submission — carries the specific reason.
    VaultRejected { detail: String },
}

impl BridgeError {
    pub fn message(&self) -> String {
        match self {
            BridgeError::BadWalletAddress => "wallet address must be 64-hex",
            BridgeError::ZeroAmount => "amount must be > 0",
            BridgeError::BadSignatureEncoding => "sig must be 128 hex chars (64 bytes)",
            BridgeError::SignatureInvalid => "signature does not match the claimed wallet",
            BridgeError::ReplayedNonce => "req_nonce must be greater than the last accepted nonce for this wallet",
            BridgeError::BadDestAddress => "dest_polygon_address must be a non-empty 0x-prefixed 20-byte hex address",
            BridgeError::NotActivated => "bridge has no relayer configured yet — locking would deposit into a vault nobody can mint against",
            BridgeError::Paused => "bridge is paused by the admin wallet",
            BridgeError::NotAdmin => "actor does not match the configured admin wallet",
            BridgeError::NotRelayer => "actor does not match the currently configured relayer wallet",
            BridgeError::BurnAlreadyProcessed => "this polygon_burn_tx has already been unlocked — refusing a double-unlock",
            BridgeError::VaultNotConfigured =>
                "bridge vault is not configured on this node — locking is unavailable \
                 (it will NOT fall back to a transparent send, which consensus retired)",
            BridgeError::LockNotPrepared { .. } =>
                "no prepared lock with this id — POST /v1/bridge/lock/prepare first to obtain \
                 the note commitments to sign",
            BridgeError::VaultRejected { detail } => return detail.clone(),
        }
        .to_string()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LockRecord {
    pub id: u64,
    pub from: String,
    pub amount: u128,
    pub dest_polygon_address: String,
    /// The first part's tx hash — kept as `tx_hash` so existing consumers still parse.
    pub tx_hash: String,
    /// Every `Shield` tx this lock queued, one per denomination part. The relayer must
    /// see ALL of them settled before minting, or it would credit more than was locked.
    #[serde(default)]
    pub part_tx_hashes: Vec<String>,
    pub ts_ms: u64,
    /// True once EVERY part has landed on the settled spine.
    ///
    /// **This is the anti-unbacked-mint field.** The relayer previously minted straight
    /// from the lock RECORD, which exists the instant a request is accepted — so a lock
    /// whose transactions never settled (as happened for the entire life of the
    /// transparent-`Send` lock) would still have been minted on Polygon, creating wrapped
    /// SIGIL backed by nothing. A record is not a receipt; this flag is.
    #[serde(default)]
    pub settled: bool,
}

fn hex32(s: &str) -> Option<WalletId> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    // ASCII guard (DoS hardening): a crafted 64-BYTE non-ASCII string would split a UTF-8
    // boundary in the byte-slice below and PANIC — a request must never crash the API.
    if s.len() != 64 || !s.is_ascii() { return None; }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn verify_sig(actor: &WalletId, msg: &str, sig_hex: &str) -> Result<(), BridgeError> {
    let sig_hex = sig_hex.strip_prefix("0x").unwrap_or(sig_hex);
    let sig_bytes = hex::decode(sig_hex).map_err(|_| BridgeError::BadSignatureEncoding)?;
    let sig_arr: [u8; 64] = sig_bytes.try_into().map_err(|_| BridgeError::BadSignatureEncoding)?;
    let vk = VerifyingKey::from_bytes(actor).map_err(|_| BridgeError::SignatureInvalid)?;
    let sig = Signature::from_bytes(&sig_arr);
    vk.verify(msg.as_bytes(), &sig).map_err(|_| BridgeError::SignatureInvalid)
}

/// Placeholder-signed wrapper: real authentication already happened in
/// `verify_sig` above, before this is ever called. `apply_tx` only calls
/// `SignedTx::precheck` (a length/binding sanity check) on `Send`, never
/// `verify_signature` — the same idiom `send::SendBridge` already relies on.
fn to_signed(tx: SigilTx) -> SignedTx {
    let payer = tx.fee_payer();
    SignedTx {
        tx,
        from_pubkey: payer,
        nonce: 0,
        sig_scheme: SigScheme::Ed25519Hot,
        sig: SignatureBytes(vec![0u8; SigScheme::Ed25519Hot.expected_sig_len()]),
        pubkey: PubKeyBytes(Vec::new()),
    }
}

pub struct BridgeBridge {
    lock_pending: Mutex<HashMap<[u8; 32], Pending>>,
    unlock_pending: Mutex<HashMap<[u8; 32], Pending>>,
    nonce_watermark: Mutex<HashMap<WalletId, u64>>,
    locks: Mutex<Vec<LockRecord>>,
    next_lock_id: AtomicU64,
    processed_burns: Mutex<HashSet<String>>,
    relayer_wallet: Mutex<Option<WalletId>>,
    admin_wallet: Option<WalletId>,
    paused: AtomicBool,
    /// The shielded vault that actually custodies locked value. `None` means the
    /// shielded lock path is not configured on this node, and `prepare_lock` /
    /// `submit_lock` refuse rather than silently falling back to the retired
    /// transparent `Send` (which consensus would drop at mint anyway).
    vault: Mutex<Option<std::sync::Arc<crate::bridge_vault::BridgeVault>>>,
    /// Tx hashes confirmed on the settled spine, so a lock can be reported as backed.
    settled_tx: Mutex<HashSet<[u8; 32]>>,
}

impl BridgeBridge {
    /// `admin_wallet`/`relayer_wallet` come from env at node startup (see
    /// `sigil-node/src/main.rs`) — never hardcoded here. `None` for either
    /// leaves the corresponding action permanently rejected (`NotAdmin`/
    /// `NotActivated`) until configured; this bridge is inert, not
    /// unsafe-by-default, when unconfigured.
    pub fn new(admin_wallet: Option<WalletId>, relayer_wallet: Option<WalletId>) -> Self {
        Self {
            lock_pending: Mutex::new(HashMap::new()),
            unlock_pending: Mutex::new(HashMap::new()),
            nonce_watermark: Mutex::new(HashMap::new()),
            locks: Mutex::new(Vec::new()),
            next_lock_id: AtomicU64::new(1),
            processed_burns: Mutex::new(HashSet::new()),
            relayer_wallet: Mutex::new(relayer_wallet),
            admin_wallet,
            paused: AtomicBool::new(false),
            vault: Mutex::new(None),
            settled_tx: Mutex::new(HashSet::new()),
        }
    }

    fn check_nonce(&self, actor: WalletId, req_nonce: u64) -> Result<(), BridgeError> {
        let mut wm = self.nonce_watermark.lock().unwrap();
        let last = wm.get(&actor).copied().unwrap_or(0);
        if req_nonce <= last {
            return Err(BridgeError::ReplayedNonce);
        }
        wm.insert(actor, req_nonce);
        Ok(())
    }

    /// User-signed: lock real SIGIL into the vault, bound to a Polygon
    /// destination address. Message: `sigil-rpc/v1|bridge_lock|{from}|
    /// {amount}|{dest_polygon_address}|nonce={req_nonce}`.
    pub fn submit_lock(
        &self,
        lock_id: u64,
        from_hex: &str,
        amount: u128,
        dest_polygon_address: &str,
        parts: &[(u128, String)],
        sig_hex: &str,
        req_nonce: u64,
    ) -> Result<LockRecord, BridgeError> {
        if self.relayer_wallet.lock().unwrap().is_none() {
            return Err(BridgeError::NotActivated);
        }
        if self.paused.load(Ordering::SeqCst) {
            return Err(BridgeError::Paused);
        }
        let from = hex32(from_hex).ok_or(BridgeError::BadWalletAddress)?;
        if amount == 0 {
            return Err(BridgeError::ZeroAmount);
        }
        let dest = dest_polygon_address.trim();
        let dest_norm = dest.strip_prefix("0x").unwrap_or(dest);
        if dest_norm.len() != 40 || !dest_norm.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(BridgeError::BadDestAddress);
        }

        // The lock is a SHIELD into vault-owned notes, never a transparent `Send`:
        // consensus retired those (`SHIELDED_ONLY_HEIGHT`), so the old shape was dropped at
        // mint on every candidate forever. See `crate::bridge_vault` for the full argument
        // and for why the caller must not be allowed to choose the commitments.
        let vault = self
            .vault
            .lock()
            .unwrap()
            .clone()
            .ok_or(BridgeError::VaultNotConfigured)?;

        let issued = vault
            .issued_for(lock_id)
            .ok_or(BridgeError::LockNotPrepared { lock_id })?;

        // Signature binds depositor, total, Polygon destination AND every commitment. The
        // destination must be inside the signed message or a lock could be redirected to
        // someone else's Polygon address after the fact.
        let mut msg = format!("sigil-rpc/v1|bridge_lock_shielded|{from_hex}|{amount}|{dest}");
        for (part_amount, cm) in parts {
            msg.push('|');
            msg.push_str(&part_amount.to_string());
            msg.push('|');
            msg.push_str(cm);
        }
        msg.push_str(&format!("|nonce={req_nonce}"));
        verify_sig(&from, &msg, sig_hex)?;

        // THE double-spend guard. Checked after the signature so an unauthenticated caller
        // cannot probe which commitments a lock id issued.
        vault
            .check_parts(lock_id, parts)
            .map_err(|e| BridgeError::VaultRejected { detail: e.message() })?;

        // The parts must still add up to the amount being minted on the other chain.
        // `check_parts` already pins them to what we issued, but this asserts the
        // relationship the BRIDGE cares about rather than trusting `prepare` forever.
        let parts_total: u128 = parts.iter().map(|(a, _)| *a).sum();
        if parts_total != amount {
            return Err(BridgeError::VaultRejected {
                detail: format!(
                    "prepared parts total {parts_total} but the lock claims {amount}"
                ),
            });
        }

        self.check_nonce(from, req_nonce)?;

        // One `Shield` per denominated part. All are queued together; each rides every
        // candidate until it settles, exactly like the old single tx did.
        let mut tx_hashes = Vec::with_capacity(issued.len());
        {
            let mut pending = self.lock_pending.lock().unwrap();
            for (part_amount, cm) in parts {
                let cm_b = hex32(cm).ok_or(BridgeError::BadWalletAddress)?;
                let tx = SigilTx::Shield {
                    from,
                    amount: *part_amount,
                    cm: cm_b,
                    fee: 0,
                };
                let tx_hash = tx.hash();
                tx_hashes.push(tx_hash);
                pending.entry(tx_hash).or_insert_with(|| Pending {
                    tx,
                    attempts: 0,
                    first_seen: Instant::now(),
                    // A lock consumes no vault note — it CREATES one.
                    vault_position: None,
                });
            }
        }

        let rec = LockRecord {
            id: lock_id,
            from: from_hex.to_string(),
            amount,
            dest_polygon_address: dest.to_string(),
            // The FIRST part's hash identifies the lock for receipts; `part_tx_hashes`
            // carries them all so the relayer can verify every piece landed.
            tx_hash: hex::encode(tx_hashes[0]),
            part_tx_hashes: tx_hashes.iter().map(hex::encode).collect(),
            ts_ms: crate::now_ms(),
            // Requested, not yet backed. `locks_since` recomputes this from what has
            // actually settled; the stored value is only ever the honest starting point.
            settled: false,
        };
        self.locks.lock().unwrap().push(rec.clone());
        vault.forget(lock_id);
        Ok(rec)
    }

    /// Phase 1 of a lock: reserve a lock id and derive the vault-owned commitments the
    /// depositor must shield into.
    ///
    /// Split into two phases because the depositor signs over the commitments, and the
    /// commitments must be chosen by the VAULT (see `bridge_vault`'s module docs — a
    /// caller-chosen commitment is a free mint). So the caller has to be told what to sign
    /// before it can sign it.
    pub fn prepare_lock(
        &self,
        amount: u128,
    ) -> Result<(u64, Vec<crate::bridge_vault::IssuedPart>), BridgeError> {
        if self.relayer_wallet.lock().unwrap().is_none() {
            return Err(BridgeError::NotActivated);
        }
        if self.paused.load(Ordering::SeqCst) {
            return Err(BridgeError::Paused);
        }
        if amount == 0 {
            return Err(BridgeError::ZeroAmount);
        }
        let vault = self
            .vault
            .lock()
            .unwrap()
            .clone()
            .ok_or(BridgeError::VaultNotConfigured)?;
        let lock_id = self.next_lock_id.fetch_add(1, Ordering::SeqCst);
        let parts = vault
            .prepare(lock_id, amount)
            .map_err(|e| BridgeError::VaultRejected { detail: e.message() })?;
        Ok((lock_id, parts))
    }

    /// Install the shielded vault. Separate from `new` so a node without a vault seed
    /// still constructs, and so the seed is read once at startup rather than per request.
    pub fn set_vault(&self, vault: std::sync::Arc<crate::bridge_vault::BridgeVault>) {
        *self.vault.lock().unwrap() = Some(vault);
    }

    /// The vault's shielded public key, for diagnostics / `/v1/bridge/status`.
    pub fn vault_pubkey_hex(&self) -> Option<String> {
        self.vault.lock().unwrap().as_ref().map(|v| v.public_key_hex())
    }

    /// Relayer-signed: release SIGIL from the vault to `to`, claiming it
    /// corresponds to the Polygon burn `polygon_burn_tx`. `actor_hex` must
    /// match the CURRENT `relayer_wallet` — checked here, not baked in.
    /// `polygon_burn_tx` is deduped so the same burn can never unlock twice.
    /// Message: `sigil-rpc/v1|bridge_unlock|{to}|{amount}|{polygon_burn_tx}|
    /// nonce={req_nonce}`.
    ///
    /// `pool_commitments` / `spent_nullifiers` are the live shielded-pool view, passed in
    /// by the handler: the payout is an `Unshield` spending one of the vault's own notes,
    /// and both the Merkle path and the "is this note still unspent" question can only be
    /// answered against current chain state.
    ///
    /// Returns one hash per denomination part — an unlock of an arbitrary amount is
    /// several transactions (see [`crate::bridge_vault::BridgeVault::build_unshield`]).
    pub fn submit_unlock(
        &self,
        actor_hex: &str,
        to_hex: &str,
        amount: u128,
        polygon_burn_tx: &str,
        sig_hex: &str,
        req_nonce: u64,
        pool_commitments: &[[u8; 32]],
        spent_nullifiers: &std::collections::BTreeSet<[u8; 32]>,
    ) -> Result<Vec<[u8; 32]>, BridgeError> {
        if self.paused.load(Ordering::SeqCst) {
            return Err(BridgeError::Paused);
        }
        let actor = hex32(actor_hex).ok_or(BridgeError::BadWalletAddress)?;
        let to = hex32(to_hex).ok_or(BridgeError::BadWalletAddress)?;
        if amount == 0 {
            return Err(BridgeError::ZeroAmount);
        }
        let current_relayer = self.relayer_wallet.lock().unwrap().ok_or(BridgeError::NotRelayer)?;
        if actor != current_relayer {
            return Err(BridgeError::NotRelayer);
        }

        let msg = format!("sigil-rpc/v1|bridge_unlock|{to_hex}|{amount}|{polygon_burn_tx}|nonce={req_nonce}");
        verify_sig(&actor, &msg, sig_hex)?;
        self.check_nonce(actor, req_nonce)?;

        let vault = self
            .vault
            .lock()
            .unwrap()
            .clone()
            .ok_or(BridgeError::VaultNotConfigured)?;

        // Three-step burn dedup, in this order for two independent reasons.
        //
        // CHECK first: a replayed burn must be refused before any proving happens.
        // Building the payout runs a STARK per denomination part, so accepting the work
        // and discarding it afterwards would turn a replay into a free CPU-exhaustion
        // lever — and it would report the wrong error, hiding the replay behind whatever
        // the vault happened to say about note availability.
        if self.processed_burns.lock().unwrap().contains(polygon_burn_tx) {
            return Err(BridgeError::BurnAlreadyProcessed);
        }

        // BUILD second: a burn recorded against an unlock that could not be built would be
        // permanently un-retryable. An entry in `processed_burns` means "this burn has been
        // paid", and a failed build has paid nothing.
        let parts = vault
            .build_unshield(pool_commitments, spent_nullifiers, to, amount)
            .map_err(|e| BridgeError::VaultRejected { detail: e.message() })?;

        // INSERT last, and re-check: the lock was released while proving, so a concurrent
        // relayer could have claimed this burn in between. `insert` returning false is
        // that race; hand the notes straight back rather than stranding them.
        if !self.processed_burns.lock().unwrap().insert(polygon_burn_tx.to_string()) {
            let positions: Vec<u64> = parts.iter().map(|p| p.position).collect();
            vault.release_positions(&positions);
            return Err(BridgeError::BurnAlreadyProcessed);
        }

        let mut hashes = Vec::with_capacity(parts.len());
        let mut pending = self.unlock_pending.lock().unwrap();
        for part in parts {
            let tx_hash = part.tx.hash();
            pending.entry(tx_hash).or_insert_with(|| Pending {
                tx: part.tx,
                attempts: 0,
                first_seen: Instant::now(),
                vault_position: Some(part.position),
            });
            hashes.push(tx_hash);
        }
        Ok(hashes)
    }

    /// Admin-signed: freeze/unfreeze both `submit_lock` and `submit_unlock`.
    /// Message: `sigil-rpc/v1|bridge_pause|{paused}|nonce={req_nonce}`.
    pub fn set_paused(&self, admin_hex: &str, paused: bool, sig_hex: &str, req_nonce: u64) -> Result<(), BridgeError> {
        let admin = hex32(admin_hex).ok_or(BridgeError::BadWalletAddress)?;
        let configured = self.admin_wallet.ok_or(BridgeError::NotAdmin)?;
        if admin != configured {
            return Err(BridgeError::NotAdmin);
        }
        let msg = format!("sigil-rpc/v1|bridge_pause|{paused}|nonce={req_nonce}");
        verify_sig(&admin, &msg, sig_hex)?;
        self.check_nonce(admin, req_nonce)?;
        self.paused.store(paused, Ordering::SeqCst);
        Ok(())
    }

    /// Admin-signed: swap the relayer key (e.g. after a suspected
    /// compromise) without touching the vault or any pending mutation.
    /// Message: `sigil-rpc/v1|bridge_rotate_relayer|{new_relayer}|
    /// nonce={req_nonce}`.
    pub fn rotate_relayer(&self, admin_hex: &str, new_relayer_hex: &str, sig_hex: &str, req_nonce: u64) -> Result<(), BridgeError> {
        let admin = hex32(admin_hex).ok_or(BridgeError::BadWalletAddress)?;
        let configured = self.admin_wallet.ok_or(BridgeError::NotAdmin)?;
        if admin != configured {
            return Err(BridgeError::NotAdmin);
        }
        let new_relayer = hex32(new_relayer_hex).ok_or(BridgeError::BadWalletAddress)?;
        let msg = format!("sigil-rpc/v1|bridge_rotate_relayer|{new_relayer_hex}|nonce={req_nonce}");
        verify_sig(&admin, &msg, sig_hex)?;
        self.check_nonce(admin, req_nonce)?;
        *self.relayer_wallet.lock().unwrap() = Some(new_relayer);
        Ok(())
    }

    pub fn locks_since(&self, since_id: u64) -> Vec<LockRecord> {
        let settled = self.settled_tx.lock().unwrap();
        self.locks
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.id > since_id)
            .cloned()
            .map(|mut r| {
                // A lock counts as backed only when EVERY part landed. Partial settlement
                // is explicitly not enough: minting the full amount against a subset would
                // credit more on Polygon than was ever locked here.
                r.settled = !r.part_tx_hashes.is_empty()
                    && r.part_tx_hashes.iter().all(|h| {
                        crate::bridge::hex32(h).map(|b| settled.contains(&b)).unwrap_or(false)
                    });
                r
            })
            .collect()
    }

    pub fn is_paused(&self) -> bool { self.paused.load(Ordering::SeqCst) }
    pub fn relayer_hex(&self) -> Option<String> { self.relayer_wallet.lock().unwrap().map(hex::encode) }
    pub fn admin_hex(&self) -> Option<String> { self.admin_wallet.map(hex::encode) }
    pub fn vault_hex() -> String { hex::encode(BRIDGE_VAULT_WALLET) }
    pub fn lock_count(&self) -> usize { self.locks.lock().unwrap().len() }

    /// Snapshot every still-pending lock AND unlock for the producer's
    /// CURRENT mint attempt — same non-destructive shape as `SendBridge::
    /// snapshot_for_mint`, called once per candidate block.
    pub fn snapshot_for_mint(&self) -> Vec<SignedTx> {
        let mut out = Vec::new();
        for (pool, label) in [(&self.lock_pending, "lock"), (&self.unlock_pending, "unlock")] {
            let mut guard = pool.lock().unwrap();
            guard.retain(|hash, p| {
                if p.attempts >= MAX_ATTEMPTS || p.first_seen.elapsed() >= MAX_AGE {
                    eprintln!(
                        "✗ bridge {label} gave up after {} attempts / {:.1}s hash={}",
                        p.attempts, p.first_seen.elapsed().as_secs_f64(), hex::encode(hash)
                    );
                    // An abandoned unlock never spent its note, so the reservation must be
                    // released or that note is stranded for the life of the process.
                    if let (Some(pos), Some(v)) =
                        (p.vault_position, self.vault.lock().unwrap().clone())
                    {
                        v.release_positions(&[pos]);
                    }
                    return false;
                }
                p.attempts += 1;
                out.push(to_signed(p.tx.clone()));
                true
            });
        }
        out
    }

    /// Retire the given tx hashes from BOTH pools — called by the producer
    /// ONLY for hashes carried by a candidate confirmed on the settled
    /// spine. Removal is a safe no-op for a hash absent from a given pool.
    pub fn confirm_applied(&self, hashes: &[[u8; 32]]) {
        if hashes.is_empty() { return; }
        // Remember what actually LANDED, so `locks_since` can tell the relayer which locks
        // are genuinely backed by settled value rather than merely requested.
        {
            let mut settled = self.settled_tx.lock().unwrap();
            for h in hashes {
                settled.insert(*h);
            }
        }
        for pool in [&self.lock_pending, &self.unlock_pending] {
            let mut guard = pool.lock().unwrap();
            for h in hashes {
                guard.remove(h);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guard-rail for the bug that made the SIGIL->Polygon bridge unusable from
    /// the day it shipped until 2026-08-26: `MAX_AGE` was 60s while a candidate
    /// needs `final_depth / block_rate` to even become eligible to settle.
    ///
    /// This is a pure arithmetic assertion, deliberately restating the two inputs
    /// rather than importing them — `sigil-api` does not depend on
    /// `sigil-dagknight`, and the point is to fail loudly if someone lowers
    /// `MAX_AGE` back toward the danger zone without re-deriving this.
    ///
    /// `final_depth = 512` (`sigil_dagknight::BraidConfig::default`) at the block
    /// rate this network actually produces (measured 8.2-8.6 blk/s live on
    /// Epsilon) is ~61s to the EARLIEST possible settlement. `MAX_AGE` must clear
    /// that with real margin, because `computed_final` additionally clamps the
    /// finality line to `pending_floor - 1` — one pending block with a missing
    /// parent stalls finality for minutes, not seconds.
    #[test]
    fn max_age_clears_the_worst_case_finality_lag() {
        const FINAL_DEPTH: u64 = 512;
        const SLOWEST_MEASURED_BLOCK_RATE_PER_SEC: u64 = 8;
        let earliest_settlement_secs = FINAL_DEPTH / SLOWEST_MEASURED_BLOCK_RATE_PER_SEC;
        assert!(
            MAX_AGE.as_secs() > earliest_settlement_secs,
            "MAX_AGE {}s must exceed the {}s floor before a candidate can settle — \
             at or below it, NO lock can ever complete",
            MAX_AGE.as_secs(),
            earliest_settlement_secs,
        );
        // Margin, not a hair's breadth: the 60s value failed by ~1s.
        assert!(
            MAX_AGE.as_secs() >= earliest_settlement_secs * 10,
            "MAX_AGE {}s leaves too little margin over the {}s floor for \
             pending-floor finality stalls",
            MAX_AGE.as_secs(),
            earliest_settlement_secs,
        );
    }

    /// `MAX_ATTEMPTS` must not silently become the limiter before `MAX_AGE` does —
    /// the offer cadence is one per candidate mint (~8.5/s measured), so the two
    /// bounds have to be sized against each other or raising `MAX_AGE` alone
    /// achieves nothing.
    #[test]
    fn max_attempts_does_not_bind_before_max_age() {
        const FASTEST_OFFER_CADENCE_PER_SEC: u64 = 9;
        let offers_within_max_age = MAX_AGE.as_secs() * FASTEST_OFFER_CADENCE_PER_SEC;
        assert!(
            u64::from(MAX_ATTEMPTS) >= offers_within_max_age,
            "MAX_ATTEMPTS {} would cut a lock off after ~{}s, before MAX_AGE {}s",
            MAX_ATTEMPTS,
            u64::from(MAX_ATTEMPTS) / FASTEST_OFFER_CADENCE_PER_SEC,
            MAX_AGE.as_secs(),
        );
    }

    fn signer() -> (ed25519_dalek::SigningKey, WalletId) {
        let sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let addr = sk.verifying_key().to_bytes();
        (sk, addr)
    }

    fn sign(sk: &ed25519_dalek::SigningKey, msg: &str) -> String {
        use ed25519_dalek::Signer;
        hex::encode(sk.sign(msg.as_bytes()).to_bytes())
    }

    const DEST: &str = "0x1234567890123456789012345678901234567890";

    /// A bridge with a shielded vault installed — the only configuration in which locking
    /// works at all now that transparent sends are retired.
    fn bridge_with_vault(admin: Option<WalletId>, relayer: Option<WalletId>) -> BridgeBridge {
        let b = BridgeBridge::new(admin, relayer);
        b.set_vault(std::sync::Arc::new(crate::bridge_vault::BridgeVault::from_seed([0x5A; 32])));
        b
    }

    /// Fund the bridge's vault with notes covering `amount` and return the pool view an
    /// unlock is proven against — the commitments in issue order, so leaf position ==
    /// issue order, exactly as a real chain would record them.
    ///
    /// An unlock spends notes the vault already owns, so a test that unlocks without
    /// first locking is testing nothing that can happen in production.
    fn fund_vault(bridge: &BridgeBridge, amount: u128) -> Vec<[u8; 32]> {
        let (_lock_id, parts) = bridge.prepare_lock(amount).expect("vault issues the parts");
        let mut leaves: Vec<[u8; 32]> = parts
            .iter()
            .map(|p| {
                let raw = hex::decode(&p.cm_hex).expect("cm_hex is hex");
                let mut cm = [0u8; 32];
                cm.copy_from_slice(&raw);
                cm
            })
            .collect();
        // Padded to capacity, like the chain's own anchor tree — the circuit proves a
        // fixed-depth path, so a short leaf vector is an invalid pool, not a small one.
        for i in leaves.len()..sigil_state::shielded::POOL_CAPACITY {
            leaves.push(sigil_shield::note_v1::padding_leaf_wire(i as u64));
        }
        leaves
    }

    /// No pool, no spent set — for the auth-failure paths, which must be refused before
    /// the vault is ever consulted.
    fn no_pool() -> (Vec<[u8; 32]>, std::collections::BTreeSet<[u8; 32]>) {
        (Vec::new(), std::collections::BTreeSet::new())
    }

    /// Run the real two-phase lock: prepare (vault issues the commitments), sign over
    /// them, submit. Returns whatever `submit_lock` returned.
    fn do_lock(
        bridge: &BridgeBridge,
        sk: &ed25519_dalek::SigningKey,
        from_hex: &str,
        amount: u128,
        dest: &str,
        nonce: u64,
    ) -> Result<LockRecord, BridgeError> {
        let (lock_id, parts) = bridge.prepare_lock(amount)?;
        let wire: Vec<(u128, String)> =
            parts.iter().map(|p| (p.amount, p.cm_hex.clone())).collect();
        let sig = sign(sk, &lock_sign_message(from_hex, amount, dest, &wire, nonce));
        bridge.submit_lock(lock_id, from_hex, amount, dest, &wire, &sig, nonce)
    }

    /// The exact message `submit_lock` verifies — mirrored here so a drift between the two
    /// shows up as a test failure rather than as an unexplained SignatureInvalid.
    fn lock_sign_message(
        from_hex: &str,
        amount: u128,
        dest: &str,
        parts: &[(u128, String)],
        nonce: u64,
    ) -> String {
        let mut msg = format!("sigil-rpc/v1|bridge_lock_shielded|{from_hex}|{amount}|{dest}");
        for (a, cm) in parts {
            msg.push('|');
            msg.push_str(&a.to_string());
            msg.push('|');
            msg.push_str(cm);
        }
        msg.push_str(&format!("|nonce={nonce}"));
        msg
    }

    #[test]
    fn a_correctly_signed_lock_is_accepted_when_activated() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let (_relayer_sk, relayer) = signer();
        let bridge = bridge_with_vault(None, Some(relayer));

        let rec = do_lock(&bridge, &sk, &from_hex, 1000, DEST, 1).unwrap();
        assert_eq!(rec.amount, 1000);
        assert_eq!(rec.dest_polygon_address, DEST);
        assert_eq!(bridge.lock_count(), 1);

        // Every queued tx must be a Shield — a transparent Send would be dropped at mint
        // by the privacy-only gate, which is the bug this whole path exists to fix.
        let snap = bridge.snapshot_for_mint();
        assert!(!snap.is_empty());
        let mut total = 0u128;
        for st in &snap {
            match &st.tx {
                SigilTx::Shield { from: f, amount, .. } => {
                    assert_eq!(*f, from, "the depositor must be the one shielding");
                    total += *amount;
                }
                other => panic!("bridge lock must queue Shield, got {other:?}"),
            }
        }
        assert_eq!(total, 1000, "the shielded parts must total the locked amount");
        assert_eq!(rec.part_tx_hashes.len(), snap.len());
    }

    /// Consensus must actually ACCEPT what the lock queues, at a height above the
    /// privacy-only activation. This is the assertion whose absence let the old
    /// transparent-Send lock ship broken: it was queued happily and only ever rejected
    /// later, inside the producer.
    #[test]
    fn every_queued_lock_tx_is_accepted_by_consensus_under_the_privacy_only_rule() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let (_rk, relayer) = signer();
        let bridge = bridge_with_vault(None, Some(relayer));
        do_lock(&bridge, &sk, &from_hex, 1000, DEST, 1).unwrap();

        // An empty state means these fail on balance — which is fine and expected. What
        // must NEVER happen is a rejection by the PRIVACY-ONLY gate, because that one is
        // unfixable by funding the wallet: it rejects the transaction SHAPE. That is
        // precisely the failure the transparent-Send lock hit on every candidate forever.
        let state = sigil_state::SigilState::new();
        for st in bridge.snapshot_for_mint() {
            match sigil_tx::apply_tx_at(&state, &st, sigil_tx::SHIELDED_ONLY_HEIGHT + 10_000) {
                Err(sigil_tx::TxApplyError::TransparentSendRetired { .. }) => panic!(
                    "bridge lock queued a transaction shape consensus has retired — \
                     it would be dropped at mint forever, which is the original bug"
                ),
                _ => {}
            }
        }
    }

    /// The double-spend guard, end to end through the bridge rather than the vault alone.
    #[test]
    fn a_lock_submitted_with_caller_chosen_commitments_is_refused() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let (_rk, relayer) = signer();
        let bridge = bridge_with_vault(None, Some(relayer));

        let (lock_id, _issued) = bridge.prepare_lock(1000).unwrap();
        // Attacker swaps in a commitment they control, and signs THAT honestly.
        let forged: Vec<(u128, String)> = vec![(1000, hex::encode([0xAAu8; 32]))];
        let sig = sign(&sk, &lock_sign_message(&from_hex, 1000, DEST, &forged, 1));
        let err = bridge
            .submit_lock(lock_id, &from_hex, 1000, DEST, &forged, &sig, 1)
            .unwrap_err();
        assert!(
            matches!(err, BridgeError::VaultRejected { .. }),
            "caller-chosen commitments must be refused, got {err:?}"
        );
        assert_eq!(bridge.lock_count(), 0, "no lock record may be created");
    }

    #[test]
    fn a_lock_id_that_was_never_prepared_is_refused() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let (_rk, relayer) = signer();
        let bridge = bridge_with_vault(None, Some(relayer));
        let parts: Vec<(u128, String)> = vec![(1000, hex::encode([0x11u8; 32]))];
        let sig = sign(&sk, &lock_sign_message(&from_hex, 1000, DEST, &parts, 1));
        assert!(matches!(
            bridge.submit_lock(4242, &from_hex, 1000, DEST, &parts, &sig, 1).unwrap_err(),
            BridgeError::LockNotPrepared { lock_id: 4242 }
        ));
    }

    #[test]
    fn locking_without_a_vault_refuses_instead_of_emitting_a_retired_transparent_send() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let (_rk, relayer) = signer();
        let bridge = BridgeBridge::new(None, Some(relayer)); // no vault installed
        assert_eq!(
            bridge.prepare_lock(1000).unwrap_err(),
            BridgeError::VaultNotConfigured
        );
        let parts: Vec<(u128, String)> = vec![(1000, hex::encode([0x11u8; 32]))];
        let sig = sign(&sk, &lock_sign_message(&from_hex, 1000, DEST, &parts, 1));
        assert_eq!(
            bridge.submit_lock(1, &from_hex, 1000, DEST, &parts, &sig, 1).unwrap_err(),
            BridgeError::VaultNotConfigured
        );
    }

    #[test]
    fn lock_rejected_before_a_relayer_is_ever_configured() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let bridge = bridge_with_vault(None, None);
        assert_eq!(
            do_lock(&bridge, &sk, &from_hex, 1000, DEST, 1).unwrap_err(),
            BridgeError::NotActivated
        );
    }

    #[test]
    fn tampered_dest_address_fails_verification() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let (_rk, relayer) = signer();
        let bridge = bridge_with_vault(None, Some(relayer));
        // Signed for DEST but submitted with a different destination —
        // exactly the "relayer redirects the payout" attack this closes.
        let (lock_id, parts) = bridge.prepare_lock(1000).unwrap();
        let wire: Vec<(u128, String)> =
            parts.iter().map(|p| (p.amount, p.cm_hex.clone())).collect();
        let sig = sign(&sk, &lock_sign_message(&from_hex, 1000, DEST, &wire, 1));
        let other_dest = "0xdeaddeaddeaddeaddeaddeaddeaddeaddeaddead";
        assert_eq!(
            bridge.submit_lock(lock_id, &from_hex, 1000, other_dest, &wire, &sig, 1).unwrap_err(),
            BridgeError::SignatureInvalid
        );
    }

    #[test]
    #[cfg_attr(debug_assertions, ignore = "debug-only winterfell 0.9 `validate_transition_degrees`: the AIR declares an UPPER BOUND on each transition-constraint degree, but the range-bit columns of a spend are witness-dependent — for some note values a column is constant, so its ACTUAL degree comes out lower and the debug assert trips. Both the check and its call site are `#[cfg(debug_assertions)]`, so it cannot fire in release, which is what the node ships. VERIFIED 2026-08-27 by building `--release --tests` and running the binary directly: all 6 pass (47/47 bridge tests, 0 failed). NOTE this uses cfg_attr so the test still RUNS in release rather than being skipped everywhere — sigil-shield's plain #[ignore]s hide the same family and 2 of those genuinely do NOT pass.")]
    fn relayer_unlock_round_trip_and_double_unlock_rejected() {
        let (relayer_sk, relayer) = signer();
        let relayer_hex = hex::encode(relayer);
        let (_uk, to) = signer();
        let to_hex = hex::encode(to);
        let bridge = bridge_with_vault(None, Some(relayer));
        let pool = fund_vault(&bridge, 500);
        let spent = std::collections::BTreeSet::new();

        let msg = format!("sigil-rpc/v1|bridge_unlock|{to_hex}|500|0xburn1|nonce=1");
        let sig = sign(&relayer_sk, &msg);
        let hashes = bridge
            .submit_unlock(&relayer_hex, &to_hex, 500, "0xburn1", &sig, 1, &pool, &spent)
            .unwrap();
        assert_eq!(hashes.len(), 1, "500 is one legal denomination — one spend");

        let snap = bridge.snapshot_for_mint();
        // The vault funding above also queued the lock's Shield transactions; keep this
        // assertion about the unlock by picking it out rather than counting the pool.
        let unlock = snap
            .iter()
            .find(|t| t.tx.hash() == hashes[0])
            .expect("the unlock must be offered to the producer");
        match &unlock.tx {
            // The payout is a proof-carrying Unshield. A `Send` from the vault is exactly
            // what consensus retired at SHIELDED_ONLY_HEIGHT — if this ever regresses to
            // Send, every unlock silently dies at apply time.
            SigilTx::Unshield { to: t, amount, proof, .. } => {
                assert_eq!(*t, to);
                assert_eq!(*amount, 500);
                assert!(!proof.is_empty(), "an Unshield with no proof cannot be applied");
            }
            other => panic!("expected Unshield, got {other:?}"),
        }

        // Same burn, new nonce — must still be rejected as already processed, and it must
        // be THAT error rather than a downstream complaint about note availability: the
        // replay has to be refused before any proving work is done.
        let msg2 = format!("sigil-rpc/v1|bridge_unlock|{to_hex}|500|0xburn1|nonce=2");
        let sig2 = sign(&relayer_sk, &msg2);
        assert_eq!(
            bridge
                .submit_unlock(&relayer_hex, &to_hex, 500, "0xburn1", &sig2, 2, &pool, &spent)
                .unwrap_err(),
            BridgeError::BurnAlreadyProcessed
        );
    }

    #[test]
    fn non_relayer_cannot_unlock_even_with_a_valid_signature_over_the_message() {
        let (attacker_sk, attacker) = signer();
        let attacker_hex = hex::encode(attacker);
        let (_rk, real_relayer) = signer();
        let (_uk, to) = signer();
        let to_hex = hex::encode(to);
        let bridge = BridgeBridge::new(None, Some(real_relayer));

        let msg = format!("sigil-rpc/v1|bridge_unlock|{to_hex}|500|0xburn2|nonce=1");
        let sig = sign(&attacker_sk, &msg); // attacker signs correctly for THEMSELVES
        assert_eq!(
            bridge.submit_unlock(&attacker_hex, &to_hex, 500, "0xburn2", &sig, 1, &no_pool().0, &no_pool().1).unwrap_err(),
            BridgeError::NotRelayer
        );
    }

    #[test]
    fn pause_blocks_lock_and_unlock_and_only_admin_can_toggle_it() {
        let (admin_sk, admin) = signer();
        let admin_hex = hex::encode(admin);
        let (attacker_sk, attacker) = signer();
        let attacker_hex = hex::encode(attacker);
        let (relayer_sk, relayer) = signer();
        let relayer_hex = hex::encode(relayer);
        let (user_sk, user) = signer();
        let user_hex = hex::encode(user);
        let bridge = bridge_with_vault(Some(admin), Some(relayer));

        // Attacker cannot pause.
        let bad_msg = "sigil-rpc/v1|bridge_pause|true|nonce=1".to_string();
        let bad_sig = sign(&attacker_sk, &bad_msg);
        assert_eq!(bridge.set_paused(&attacker_hex, true, &bad_sig, 1).unwrap_err(), BridgeError::NotAdmin);
        assert!(!bridge.is_paused());

        // Real admin pauses.
        let msg = "sigil-rpc/v1|bridge_pause|true|nonce=1".to_string();
        let sig = sign(&admin_sk, &msg);
        bridge.set_paused(&admin_hex, true, &sig, 1).unwrap();
        assert!(bridge.is_paused());

        // Lock blocked while paused.
        // Paused is checked in phase 1, before any commitment is ever issued.
        assert_eq!(bridge.prepare_lock(10).unwrap_err(), BridgeError::Paused);
        let parts: Vec<(u128, String)> = vec![(10, hex::encode([0x22u8; 32]))];
        let lock_sig = sign(&user_sk, &lock_sign_message(&user_hex, 10, DEST, &parts, 1));
        assert_eq!(
            bridge.submit_lock(1, &user_hex, 10, DEST, &parts, &lock_sig, 1).unwrap_err(),
            BridgeError::Paused
        );

        // Unlock blocked while paused.
        let unlock_msg = format!("sigil-rpc/v1|bridge_unlock|{user_hex}|10|0xburn3|nonce=1");
        let unlock_sig = sign(&relayer_sk, &unlock_msg);
        assert_eq!(
            bridge.submit_unlock(&relayer_hex, &user_hex, 10, "0xburn3", &unlock_sig, 1, &no_pool().0, &no_pool().1).unwrap_err(),
            BridgeError::Paused
        );
    }

    #[test]
    fn admin_can_rotate_relayer_and_old_relayer_loses_authority() {
        let (admin_sk, admin) = signer();
        let admin_hex = hex::encode(admin);
        let (old_relayer_sk, old_relayer) = signer();
        let old_relayer_hex = hex::encode(old_relayer);
        let (_new_sk, new_relayer) = signer();
        let new_relayer_hex = hex::encode(new_relayer);
        let (_uk, to) = signer();
        let to_hex = hex::encode(to);
        let bridge = BridgeBridge::new(Some(admin), Some(old_relayer));

        let msg = format!("sigil-rpc/v1|bridge_rotate_relayer|{new_relayer_hex}|nonce=1");
        let sig = sign(&admin_sk, &msg);
        bridge.rotate_relayer(&admin_hex, &new_relayer_hex, &sig, 1).unwrap();
        assert_eq!(bridge.relayer_hex(), Some(new_relayer_hex));

        // Old relayer's signature is no longer authoritative.
        let unlock_msg = format!("sigil-rpc/v1|bridge_unlock|{to_hex}|10|0xburn4|nonce=1");
        let unlock_sig = sign(&old_relayer_sk, &unlock_msg);
        assert_eq!(
            bridge.submit_unlock(&old_relayer_hex, &to_hex, 10, "0xburn4", &unlock_sig, 1, &no_pool().0, &no_pool().1).unwrap_err(),
            BridgeError::NotRelayer
        );
    }

    #[test]
    fn replayed_nonce_is_rejected_across_all_signed_actions() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let (_rk, relayer) = signer();
        let bridge = bridge_with_vault(None, Some(relayer));
        do_lock(&bridge, &sk, &from_hex, 10, DEST, 5).unwrap();
        // A second lock at the SAME nonce must be refused even though it is freshly
        // prepared and correctly signed over its own (different) commitments.
        let err = do_lock(&bridge, &sk, &from_hex, 10, DEST, 5).unwrap_err();
        assert_eq!(err, BridgeError::ReplayedNonce);
    }

    #[test]
    #[cfg_attr(debug_assertions, ignore = "debug-only winterfell 0.9 `validate_transition_degrees`: the AIR declares an UPPER BOUND on each transition-constraint degree, but the range-bit columns of a spend are witness-dependent — for some note values a column is constant, so its ACTUAL degree comes out lower and the debug assert trips. Both the check and its call site are `#[cfg(debug_assertions)]`, so it cannot fire in release, which is what the node ships. VERIFIED 2026-08-27 by building `--release --tests` and running the binary directly: all 6 pass (47/47 bridge tests, 0 failed). NOTE this uses cfg_attr so the test still RUNS in release rather than being skipped everywhere — sigil-shield's plain #[ignore]s hide the same family and 2 of those genuinely do NOT pass.")]
    fn confirm_applied_retires_only_named_hashes_from_both_pools() {
        let (sk1, from1) = signer();
        let from1_hex = hex::encode(from1);
        let (relayer_sk, relayer) = signer();
        let relayer_hex = hex::encode(relayer);
        let (_uk, to) = signer();
        let to_hex = hex::encode(to);
        let bridge = bridge_with_vault(None, Some(relayer));

        // 10 is a single legal denomination, so this lock queues exactly one Shield —
        // keeping the arithmetic below about confirm_applied, not about splitting.
        let lock = do_lock(&bridge, &sk1, &from1_hex, 10, DEST, 1).unwrap();
        assert_eq!(lock.part_tx_hashes.len(), 1);

        // Give the vault a landed note the unlock can actually spend. `prepare_lock`
        // allocates notes without queueing anything, so this does not disturb the pending
        // pool the assertions below count.
        let pool = fund_vault(&bridge, 20);

        let unlock_msg = format!("sigil-rpc/v1|bridge_unlock|{to_hex}|20|0xburn5|nonce=1");
        let unlock_sig = sign(&relayer_sk, &unlock_msg);
        let unlock_hashes = bridge
            .submit_unlock(
                &relayer_hex, &to_hex, 20, "0xburn5", &unlock_sig, 1,
                &pool, &std::collections::BTreeSet::new(),
            )
            .unwrap();
        assert_eq!(unlock_hashes.len(), 1, "20 is a single legal denomination");

        assert_eq!(bridge.snapshot_for_mint().len(), 2);
        bridge.confirm_applied(&unlock_hashes);
        // Only the unlock retired; the lock must still be offered.
        let snap = bridge.snapshot_for_mint();
        assert_eq!(snap.len(), 1);
        match &snap[0].tx {
            SigilTx::Shield { from: f, .. } => assert_eq!(*f, from1),
            other => panic!("expected the lock Shield, got {other:?}"),
        }
    }

    /// The anti-unbacked-mint invariant: a lock is reported `settled` ONLY after every
    /// one of its parts has been confirmed on the settled spine. Minting from an unsettled
    /// record is exactly how wrapped SIGIL could be created against value that never moved.
    #[test]
    fn a_lock_is_not_reported_settled_until_every_part_has_landed() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let (_rk, relayer) = signer();
        let bridge = bridge_with_vault(None, Some(relayer));

        // An amount that must split into more than one denomination.
        let rec = do_lock(&bridge, &sk, &from_hex, 5_953_021_946, DEST, 1).unwrap();
        assert!(rec.part_tx_hashes.len() > 1, "need a multi-part lock for this test");

        assert!(
            !bridge.locks_since(0)[0].settled,
            "a freshly requested lock is NOT backed yet"
        );

        // Confirm all but one part — still not backed.
        let all: Vec<[u8; 32]> = rec
            .part_tx_hashes
            .iter()
            .map(|h| hex32(h).unwrap())
            .collect();
        bridge.confirm_applied(&all[..all.len() - 1]);
        assert!(
            !bridge.locks_since(0)[0].settled,
            "partial settlement must NOT count — minting the full amount against a subset \
             would credit more on Polygon than was locked"
        );

        // The last part lands: now it is backed.
        bridge.confirm_applied(&all[all.len() - 1..]);
        assert!(bridge.locks_since(0)[0].settled, "all parts landed — lock is backed");
    }

    #[test]
    fn locks_since_returns_only_records_after_the_given_id() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let (_rk, relayer) = signer();
        let bridge = bridge_with_vault(None, Some(relayer));
        for n in 1..=3u64 {
            do_lock(&bridge, &sk, &from_hex, n as u128, DEST, n).unwrap();
        }
        let since_1 = bridge.locks_since(1);
        assert_eq!(since_1.len(), 2);
        assert!(since_1.iter().all(|r| r.id > 1));
    }
}
