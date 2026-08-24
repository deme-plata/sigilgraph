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
use sigil_state::{WalletId, NATIVE};
use sigil_tx::{SigilTx, SignedTx};

/// Fixed, non-signing custody address — nobody holds this key, exactly like
/// `sigil-usds::VAULT` (`[0x0B; 32]`) / `sigil-bank::CREDIT_VAULT_WALLET`
/// (`[0xCF; 32]`). Chosen distinct from both.
pub const BRIDGE_VAULT_WALLET: WalletId = [0xB2u8; 32];

const MAX_ATTEMPTS: u32 = 2_000;
const MAX_AGE: Duration = Duration::from_secs(60);

struct Pending {
    tx: SigilTx,
    attempts: u32,
    first_seen: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

impl BridgeError {
    pub fn message(self) -> &'static str {
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
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LockRecord {
    pub id: u64,
    pub from: String,
    pub amount: u128,
    pub dest_polygon_address: String,
    pub tx_hash: String,
    pub ts_ms: u64,
}

fn hex32(s: &str) -> Option<WalletId> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if s.len() != 64 { return None; }
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
        from_hex: &str,
        amount: u128,
        dest_polygon_address: &str,
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

        let msg = format!("sigil-rpc/v1|bridge_lock|{from_hex}|{amount}|{dest}|nonce={req_nonce}");
        verify_sig(&from, &msg, sig_hex)?;
        self.check_nonce(from, req_nonce)?;

        let tx = SigilTx::Send { from, to: BRIDGE_VAULT_WALLET, amount, token: NATIVE, fee: 0 };
        let tx_hash = tx.hash();
        self.lock_pending.lock().unwrap().entry(tx_hash).or_insert_with(|| Pending {
            tx,
            attempts: 0,
            first_seen: Instant::now(),
        });

        let id = self.next_lock_id.fetch_add(1, Ordering::SeqCst);
        let rec = LockRecord {
            id,
            from: from_hex.to_string(),
            amount,
            dest_polygon_address: dest.to_string(),
            tx_hash: hex::encode(tx_hash),
            ts_ms: crate::now_ms(),
        };
        self.locks.lock().unwrap().push(rec.clone());
        Ok(rec)
    }

    /// Relayer-signed: release SIGIL from the vault to `to`, claiming it
    /// corresponds to the Polygon burn `polygon_burn_tx`. `actor_hex` must
    /// match the CURRENT `relayer_wallet` — checked here, not baked in.
    /// `polygon_burn_tx` is deduped so the same burn can never unlock twice.
    /// Message: `sigil-rpc/v1|bridge_unlock|{to}|{amount}|{polygon_burn_tx}|
    /// nonce={req_nonce}`.
    pub fn submit_unlock(
        &self,
        actor_hex: &str,
        to_hex: &str,
        amount: u128,
        polygon_burn_tx: &str,
        sig_hex: &str,
        req_nonce: u64,
    ) -> Result<[u8; 32], BridgeError> {
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

        {
            let mut seen = self.processed_burns.lock().unwrap();
            if !seen.insert(polygon_burn_tx.to_string()) {
                return Err(BridgeError::BurnAlreadyProcessed);
            }
        }

        let tx = SigilTx::Send { from: BRIDGE_VAULT_WALLET, to, amount, token: NATIVE, fee: 0 };
        let tx_hash = tx.hash();
        self.unlock_pending.lock().unwrap().entry(tx_hash).or_insert_with(|| Pending {
            tx,
            attempts: 0,
            first_seen: Instant::now(),
        });
        Ok(tx_hash)
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
        self.locks.lock().unwrap().iter().filter(|r| r.id > since_id).cloned().collect()
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

    #[test]
    fn a_correctly_signed_lock_is_accepted_when_activated() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let (_relayer_sk, relayer) = signer();
        let bridge = BridgeBridge::new(None, Some(relayer));
        let msg = format!("sigil-rpc/v1|bridge_lock|{from_hex}|1000|{DEST}|nonce=1");
        let sig = sign(&sk, &msg);

        let rec = bridge.submit_lock(&from_hex, 1000, DEST, &sig, 1).unwrap();
        assert_eq!(rec.amount, 1000);
        assert_eq!(rec.dest_polygon_address, DEST);
        assert_eq!(bridge.lock_count(), 1);

        let snap = bridge.snapshot_for_mint();
        assert_eq!(snap.len(), 1);
        match &snap[0].tx {
            SigilTx::Send { from: f, to, amount, .. } => {
                assert_eq!(*f, from);
                assert_eq!(*to, BRIDGE_VAULT_WALLET);
                assert_eq!(*amount, 1000);
            }
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[test]
    fn lock_rejected_before_a_relayer_is_ever_configured() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let bridge = BridgeBridge::new(None, None);
        let msg = format!("sigil-rpc/v1|bridge_lock|{from_hex}|1000|{DEST}|nonce=1");
        let sig = sign(&sk, &msg);
        assert_eq!(bridge.submit_lock(&from_hex, 1000, DEST, &sig, 1).unwrap_err(), BridgeError::NotActivated);
    }

    #[test]
    fn tampered_dest_address_fails_verification() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let (_rk, relayer) = signer();
        let bridge = BridgeBridge::new(None, Some(relayer));
        // Signed for DEST but submitted with a different destination —
        // exactly the "relayer redirects the payout" attack this closes.
        let msg = format!("sigil-rpc/v1|bridge_lock|{from_hex}|1000|{DEST}|nonce=1");
        let sig = sign(&sk, &msg);
        let other_dest = "0xdeaddeaddeaddeaddeaddeaddeaddeaddeaddead";
        assert_eq!(
            bridge.submit_lock(&from_hex, 1000, other_dest, &sig, 1).unwrap_err(),
            BridgeError::SignatureInvalid
        );
    }

    #[test]
    fn relayer_unlock_round_trip_and_double_unlock_rejected() {
        let (relayer_sk, relayer) = signer();
        let relayer_hex = hex::encode(relayer);
        let (_uk, to) = signer();
        let to_hex = hex::encode(to);
        let bridge = BridgeBridge::new(None, Some(relayer));

        let msg = format!("sigil-rpc/v1|bridge_unlock|{to_hex}|500|0xburn1|nonce=1");
        let sig = sign(&relayer_sk, &msg);
        let hash = bridge.submit_unlock(&relayer_hex, &to_hex, 500, "0xburn1", &sig, 1).unwrap();

        let snap = bridge.snapshot_for_mint();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].tx.hash(), hash);
        match &snap[0].tx {
            SigilTx::Send { from, to: t, amount, .. } => {
                assert_eq!(*from, BRIDGE_VAULT_WALLET);
                assert_eq!(*t, to);
                assert_eq!(*amount, 500);
            }
            other => panic!("expected Send, got {other:?}"),
        }

        // Same burn, new nonce — must still be rejected as already processed.
        let msg2 = format!("sigil-rpc/v1|bridge_unlock|{to_hex}|500|0xburn1|nonce=2");
        let sig2 = sign(&relayer_sk, &msg2);
        assert_eq!(
            bridge.submit_unlock(&relayer_hex, &to_hex, 500, "0xburn1", &sig2, 2).unwrap_err(),
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
            bridge.submit_unlock(&attacker_hex, &to_hex, 500, "0xburn2", &sig, 1).unwrap_err(),
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
        let bridge = BridgeBridge::new(Some(admin), Some(relayer));

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
        let lock_msg = format!("sigil-rpc/v1|bridge_lock|{user_hex}|10|{DEST}|nonce=1");
        let lock_sig = sign(&user_sk, &lock_msg);
        assert_eq!(bridge.submit_lock(&user_hex, 10, DEST, &lock_sig, 1).unwrap_err(), BridgeError::Paused);

        // Unlock blocked while paused.
        let unlock_msg = format!("sigil-rpc/v1|bridge_unlock|{user_hex}|10|0xburn3|nonce=1");
        let unlock_sig = sign(&relayer_sk, &unlock_msg);
        assert_eq!(
            bridge.submit_unlock(&relayer_hex, &user_hex, 10, "0xburn3", &unlock_sig, 1).unwrap_err(),
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
            bridge.submit_unlock(&old_relayer_hex, &to_hex, 10, "0xburn4", &unlock_sig, 1).unwrap_err(),
            BridgeError::NotRelayer
        );
    }

    #[test]
    fn replayed_nonce_is_rejected_across_all_signed_actions() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let (_rk, relayer) = signer();
        let bridge = BridgeBridge::new(None, Some(relayer));
        let msg = format!("sigil-rpc/v1|bridge_lock|{from_hex}|10|{DEST}|nonce=5");
        let sig = sign(&sk, &msg);
        bridge.submit_lock(&from_hex, 10, DEST, &sig, 5).unwrap();
        let err = bridge.submit_lock(&from_hex, 10, DEST, &sig, 5).unwrap_err();
        assert_eq!(err, BridgeError::ReplayedNonce);
    }

    #[test]
    fn confirm_applied_retires_only_named_hashes_from_both_pools() {
        let (sk1, from1) = signer();
        let from1_hex = hex::encode(from1);
        let (relayer_sk, relayer) = signer();
        let relayer_hex = hex::encode(relayer);
        let (_uk, to) = signer();
        let to_hex = hex::encode(to);
        let bridge = BridgeBridge::new(None, Some(relayer));

        let lock_msg = format!("sigil-rpc/v1|bridge_lock|{from1_hex}|10|{DEST}|nonce=1");
        let lock_sig = sign(&sk1, &lock_msg);
        bridge.submit_lock(&from1_hex, 10, DEST, &lock_sig, 1).unwrap();

        let unlock_msg = format!("sigil-rpc/v1|bridge_unlock|{to_hex}|20|0xburn5|nonce=1");
        let unlock_sig = sign(&relayer_sk, &unlock_msg);
        let unlock_hash = bridge.submit_unlock(&relayer_hex, &to_hex, 20, "0xburn5", &unlock_sig, 1).unwrap();

        assert_eq!(bridge.snapshot_for_mint().len(), 2);
        bridge.confirm_applied(&[unlock_hash]);
        // Only the unlock retired; the lock must still be offered.
        let snap = bridge.snapshot_for_mint();
        assert_eq!(snap.len(), 1);
        match &snap[0].tx {
            SigilTx::Send { to, .. } => assert_eq!(*to, BRIDGE_VAULT_WALLET),
            other => panic!("expected the lock Send, got {other:?}"),
        }
    }

    #[test]
    fn locks_since_returns_only_records_after_the_given_id() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let (_rk, relayer) = signer();
        let bridge = BridgeBridge::new(None, Some(relayer));
        for n in 1..=3u64 {
            let msg = format!("sigil-rpc/v1|bridge_lock|{from_hex}|{n}|{DEST}|nonce={n}");
            let sig = sign(&sk, &msg);
            bridge.submit_lock(&from_hex, n as u128, DEST, &sig, n).unwrap();
        }
        let since_1 = bridge.locks_since(1);
        assert_eq!(since_1.len(), 2);
        assert!(since_1.iter().all(|r| r.id > 1));
    }
}
