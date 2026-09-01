//! usds_bridge.rs — the USDS <-> Polygon lock/unlock bridge surface.
//!
//! A second, independent instance of the EXACT pattern [`crate::bridge`]
//! already proved live for native SIGIL — same confirm-on-settle pending-pool
//! shape, same admin/relayer trust split, same "vault is just another
//! `WalletId`" custody model. Deliberately a SEPARATE module rather than a
//! generalized "any token" bridge: this locks [`sigil_usds::USDS`] (not
//! `NATIVE`) into its OWN vault address, under its OWN admin/relayer wallets,
//! with its OWN signed-message namespace (`usds_bridge_lock`, not
//! `bridge_lock`). That isolation is deliberate, not laziness — a
//! compromised USDS-bridge relayer key can drain only the USDS vault, never
//! the native-SIGIL one, and vice versa; and reusing the SAME signed-message
//! prefix across two different vaults would let a signature meant for one
//! bridge be replayed against the other if their fields ever happened to
//! line up. See `bridge.rs`'s own docs for the full trust-model writeup —
//! everything there applies here verbatim, just for USDS instead of SIGIL.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Serialize;
use sigil_header::{PubKeyBytes, SigScheme, SignatureBytes};
use sigil_state::WalletId;
use sigil_tx::{SigilTx, SignedTx};
use sigil_usds::USDS;

/// Fixed, non-signing custody address for the USDS bridge — distinct from
/// `bridge::BRIDGE_VAULT_WALLET` (`[0xB2;32]`), `sigil_usds::VAULT`
/// (`[0x0B;32]`), and `sigil_bank::CREDIT_VAULT_WALLET` (`[0xCF;32]`).
pub const USDS_BRIDGE_VAULT_WALLET: WalletId = [0xB4u8; 32];

/// **The retry budget is bounded by FINALITY, not by wall-clock guesswork.**
/// A pending tx retires only via `confirm_applied`, which the producer calls only
/// for a candidate that has landed on the SETTLED spine — and settlement is gated on
/// `Braid::finalized_height()` = `tip - final_depth`, with `final_depth = 512`
/// (`sigil_dagknight::BraidConfig`). At the rate this network actually produces —
/// **measured 6.28 blk/s live on Epsilon, 2026-08-26** — that is `512 / 6.28 =`
/// **~81.5 seconds** before a freshly-proposed candidate can settle *at the very best*.
///
/// The previous value here was **60s**, i.e. BELOW that floor: a tx gave up ~21s before
/// the earliest instant it could possibly land, so it was not a race or an edge case —
/// nothing submitted through this path could ever complete. That is exactly how the
/// SIGIL->Polygon bridge went its entire life without minting once
/// (`✗ bridge lock gave up after 402 attempts / 60.1s`, vault balance `0`), and how
/// shielded registration never landed a note before its own 2026-08-24 fix.
///
/// The old doc comment claimed "a fresh candidate is minted roughly every producer tick,
/// tens of ms — a generous multi-second budget". **That mental model was the bug.** The
/// offer cadence is one per CANDIDATE MINT (measured ~6.7/s, matching the block rate),
/// and the thing being waited on is finality, not ticks.
///
/// 2_400s (40 min) is the value `shielded.rs` already settled on, and gives real margin
/// over the WORST case rather than the happy path: `computed_final` additionally clamps
/// the finality line to `pending_floor - 1`, so a single pending block whose parent never
/// arrives stalls finality for minutes until the `pending_max_tip_lag` / `max_window`
/// escape hatches fire.
///
/// `MAX_ATTEMPTS` is sized so it cannot become the new accidental limiter — at ~9 offers/s
/// worst case, covering 2_400s needs ~21.6k. It is a runaway backstop; `MAX_AGE` is
/// deliberately the binding bound, because that is the one tied to finality.
/// (This is not hypothetical: `shielded.rs` raised MAX_AGE to 2_400s on 2026-08-24 but
/// left `MAX_ATTEMPTS = 600`, so it simply started dying at 92s instead of 60s —
/// `✗ shielded tx gave up after 600 attempts / 92.0s`, observed live.)
const MAX_ATTEMPTS: u32 = 30_000;
/// How long a pending tx may stay pending before it is dropped. Must exceed the
/// finality lag documented on `MAX_ATTEMPTS` above.
const MAX_AGE: Duration = Duration::from_secs(2_400);

struct Pending {
    tx: SigilTx,
    attempts: u32,
    first_seen: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsdsBridgeError {
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

impl UsdsBridgeError {
    pub fn message(self) -> &'static str {
        match self {
            UsdsBridgeError::BadWalletAddress => "wallet address must be 64-hex",
            UsdsBridgeError::ZeroAmount => "amount must be > 0",
            UsdsBridgeError::BadSignatureEncoding => "sig must be 128 hex chars (64 bytes)",
            UsdsBridgeError::SignatureInvalid => "signature does not match the claimed wallet",
            UsdsBridgeError::ReplayedNonce => "req_nonce must be greater than the last accepted nonce for this wallet",
            UsdsBridgeError::BadDestAddress => "dest_polygon_address must be a non-empty 0x-prefixed 20-byte hex address",
            UsdsBridgeError::NotActivated => "USDS bridge has no relayer configured yet — locking would deposit into a vault nobody can mint against",
            UsdsBridgeError::Paused => "USDS bridge is paused by the admin wallet",
            UsdsBridgeError::NotAdmin => "actor does not match the configured admin wallet",
            UsdsBridgeError::NotRelayer => "actor does not match the currently configured relayer wallet",
            UsdsBridgeError::BurnAlreadyProcessed => "this polygon_burn_tx has already been unlocked — refusing a double-unlock",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UsdsLockRecord {
    pub id: u64,
    pub from: String,
    pub amount: u128,
    pub dest_polygon_address: String,
    pub tx_hash: String,
    pub ts_ms: u64,
}

fn verify_sig(actor: &WalletId, msg: &str, sig_hex: &str) -> Result<(), UsdsBridgeError> {
    let sig_hex = sig_hex.strip_prefix("0x").unwrap_or(sig_hex);
    let sig_bytes = hex::decode(sig_hex).map_err(|_| UsdsBridgeError::BadSignatureEncoding)?;
    let sig_arr: [u8; 64] = sig_bytes.try_into().map_err(|_| UsdsBridgeError::BadSignatureEncoding)?;
    let vk = VerifyingKey::from_bytes(actor).map_err(|_| UsdsBridgeError::SignatureInvalid)?;
    let sig = Signature::from_bytes(&sig_arr);
    vk.verify(msg.as_bytes(), &sig).map_err(|_| UsdsBridgeError::SignatureInvalid)
}

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

pub struct UsdsBridgeBridge {
    lock_pending: Mutex<HashMap<[u8; 32], Pending>>,
    unlock_pending: Mutex<HashMap<[u8; 32], Pending>>,
    nonce_watermark: Mutex<HashMap<WalletId, u64>>,
    locks: Mutex<Vec<UsdsLockRecord>>,
    next_lock_id: AtomicU64,
    processed_burns: Mutex<HashSet<String>>,
    relayer_wallet: Mutex<Option<WalletId>>,
    admin_wallet: Option<WalletId>,
    paused: AtomicBool,
}

impl UsdsBridgeBridge {
    /// `admin_wallet`/`relayer_wallet` come from env at node startup
    /// (`SIGIL_USDS_BRIDGE_ADMIN_WALLET`/`SIGIL_USDS_BRIDGE_RELAYER_WALLET`,
    /// deliberately SEPARATE env vars from the native bridge's — see module
    /// docs for why the two bridges never share a trust boundary).
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

    fn check_nonce(&self, actor: WalletId, req_nonce: u64) -> Result<(), UsdsBridgeError> {
        let mut wm = self.nonce_watermark.lock().unwrap();
        let last = wm.get(&actor).copied().unwrap_or(0);
        if req_nonce <= last {
            return Err(UsdsBridgeError::ReplayedNonce);
        }
        wm.insert(actor, req_nonce);
        Ok(())
    }

    /// User-signed: lock USDS into the vault, bound to a Polygon destination.
    /// Message: `sigil-rpc/v1|usds_bridge_lock|{from}|{amount}|
    /// {dest_polygon_address}|nonce={req_nonce}`.
    pub fn submit_lock(
        &self,
        from_hex: &str,
        amount: u128,
        dest_polygon_address: &str,
        sig_hex: &str,
        req_nonce: u64,
    ) -> Result<UsdsLockRecord, UsdsBridgeError> {
        if self.relayer_wallet.lock().unwrap().is_none() {
            return Err(UsdsBridgeError::NotActivated);
        }
        if self.paused.load(Ordering::SeqCst) {
            return Err(UsdsBridgeError::Paused);
        }
        let from = crate::hex32(from_hex).ok_or(UsdsBridgeError::BadWalletAddress)?;
        if amount == 0 {
            return Err(UsdsBridgeError::ZeroAmount);
        }
        let dest = dest_polygon_address.trim();
        let dest_norm = dest.strip_prefix("0x").unwrap_or(dest);
        if dest_norm.len() != 40 || !dest_norm.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(UsdsBridgeError::BadDestAddress);
        }

        let msg = format!("sigil-rpc/v1|usds_bridge_lock|{from_hex}|{amount}|{dest}|nonce={req_nonce}");
        verify_sig(&from, &msg, sig_hex)?;
        self.check_nonce(from, req_nonce)?;

        let tx = SigilTx::Send { from, to: USDS_BRIDGE_VAULT_WALLET, amount, token: USDS, fee: 0 };
        let tx_hash = tx.hash();
        self.lock_pending.lock().unwrap().entry(tx_hash).or_insert_with(|| Pending {
            tx,
            attempts: 0,
            first_seen: Instant::now(),
        });

        let id = self.next_lock_id.fetch_add(1, Ordering::SeqCst);
        let rec = UsdsLockRecord {
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

    /// Relayer-signed: release USDS from the vault to `to`, claiming it
    /// corresponds to the Polygon burn `polygon_burn_tx`. Message:
    /// `sigil-rpc/v1|usds_bridge_unlock|{to}|{amount}|{polygon_burn_tx}|
    /// nonce={req_nonce}`.
    pub fn submit_unlock(
        &self,
        actor_hex: &str,
        to_hex: &str,
        amount: u128,
        polygon_burn_tx: &str,
        sig_hex: &str,
        req_nonce: u64,
    ) -> Result<[u8; 32], UsdsBridgeError> {
        if self.paused.load(Ordering::SeqCst) {
            return Err(UsdsBridgeError::Paused);
        }
        let actor = crate::hex32(actor_hex).ok_or(UsdsBridgeError::BadWalletAddress)?;
        let to = crate::hex32(to_hex).ok_or(UsdsBridgeError::BadWalletAddress)?;
        if amount == 0 {
            return Err(UsdsBridgeError::ZeroAmount);
        }
        let current_relayer = self.relayer_wallet.lock().unwrap().ok_or(UsdsBridgeError::NotRelayer)?;
        if actor != current_relayer {
            return Err(UsdsBridgeError::NotRelayer);
        }

        let msg = format!("sigil-rpc/v1|usds_bridge_unlock|{to_hex}|{amount}|{polygon_burn_tx}|nonce={req_nonce}");
        verify_sig(&actor, &msg, sig_hex)?;
        self.check_nonce(actor, req_nonce)?;

        {
            let mut seen = self.processed_burns.lock().unwrap();
            if !seen.insert(polygon_burn_tx.to_string()) {
                return Err(UsdsBridgeError::BurnAlreadyProcessed);
            }
        }

        let tx = SigilTx::Send { from: USDS_BRIDGE_VAULT_WALLET, to, amount, token: USDS, fee: 0 };
        let tx_hash = tx.hash();
        self.unlock_pending.lock().unwrap().entry(tx_hash).or_insert_with(|| Pending {
            tx,
            attempts: 0,
            first_seen: Instant::now(),
        });
        Ok(tx_hash)
    }

    /// Admin-signed: freeze/unfreeze both directions. Message:
    /// `sigil-rpc/v1|usds_bridge_pause|{paused}|nonce={req_nonce}`.
    pub fn set_paused(&self, admin_hex: &str, paused: bool, sig_hex: &str, req_nonce: u64) -> Result<(), UsdsBridgeError> {
        let admin = crate::hex32(admin_hex).ok_or(UsdsBridgeError::BadWalletAddress)?;
        let configured = self.admin_wallet.ok_or(UsdsBridgeError::NotAdmin)?;
        if admin != configured {
            return Err(UsdsBridgeError::NotAdmin);
        }
        let msg = format!("sigil-rpc/v1|usds_bridge_pause|{paused}|nonce={req_nonce}");
        verify_sig(&admin, &msg, sig_hex)?;
        self.check_nonce(admin, req_nonce)?;
        self.paused.store(paused, Ordering::SeqCst);
        Ok(())
    }

    /// Admin-signed: swap the relayer key. Message:
    /// `sigil-rpc/v1|usds_bridge_rotate_relayer|{new_relayer}|nonce={req_nonce}`.
    pub fn rotate_relayer(&self, admin_hex: &str, new_relayer_hex: &str, sig_hex: &str, req_nonce: u64) -> Result<(), UsdsBridgeError> {
        let admin = crate::hex32(admin_hex).ok_or(UsdsBridgeError::BadWalletAddress)?;
        let configured = self.admin_wallet.ok_or(UsdsBridgeError::NotAdmin)?;
        if admin != configured {
            return Err(UsdsBridgeError::NotAdmin);
        }
        let new_relayer = crate::hex32(new_relayer_hex).ok_or(UsdsBridgeError::BadWalletAddress)?;
        let msg = format!("sigil-rpc/v1|usds_bridge_rotate_relayer|{new_relayer_hex}|nonce={req_nonce}");
        verify_sig(&admin, &msg, sig_hex)?;
        self.check_nonce(admin, req_nonce)?;
        *self.relayer_wallet.lock().unwrap() = Some(new_relayer);
        Ok(())
    }

    pub fn locks_since(&self, since_id: u64) -> Vec<UsdsLockRecord> {
        self.locks.lock().unwrap().iter().filter(|r| r.id > since_id).cloned().collect()
    }

    pub fn is_paused(&self) -> bool { self.paused.load(Ordering::SeqCst) }
    pub fn relayer_hex(&self) -> Option<String> { self.relayer_wallet.lock().unwrap().map(hex::encode) }
    pub fn admin_hex(&self) -> Option<String> { self.admin_wallet.map(hex::encode) }
    pub fn vault_hex() -> String { hex::encode(USDS_BRIDGE_VAULT_WALLET) }
    pub fn lock_count(&self) -> usize { self.locks.lock().unwrap().len() }

    /// Snapshot every still-pending lock AND unlock, same non-destructive
    /// shape as `bridge::BridgeBridge::snapshot_for_mint`.
    pub fn snapshot_for_mint(&self) -> Vec<SignedTx> {
        let mut out = Vec::new();
        for (pool, label) in [(&self.lock_pending, "lock"), (&self.unlock_pending, "unlock")] {
            let mut guard = pool.lock().unwrap();
            guard.retain(|hash, p| {
                if p.attempts >= MAX_ATTEMPTS || p.first_seen.elapsed() >= MAX_AGE {
                    eprintln!(
                        "\u{2717} usds bridge {label} gave up after {} attempts / {:.1}s hash={}",
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
    fn a_correctly_signed_lock_moves_usds_not_native() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let (_relayer_sk, relayer) = signer();
        let bridge = UsdsBridgeBridge::new(None, Some(relayer));
        let msg = format!("sigil-rpc/v1|usds_bridge_lock|{from_hex}|1000|{DEST}|nonce=1");
        let sig = sign(&sk, &msg);

        let rec = bridge.submit_lock(&from_hex, 1000, DEST, &sig, 1).unwrap();
        assert_eq!(rec.amount, 1000);
        let snap = bridge.snapshot_for_mint();
        assert_eq!(snap.len(), 1);
        match &snap[0].tx {
            SigilTx::Send { from: f, to, amount, token, .. } => {
                assert_eq!(*f, from);
                assert_eq!(*to, USDS_BRIDGE_VAULT_WALLET);
                assert_eq!(*amount, 1000);
                assert_eq!(*token, USDS, "must move USDS, not native SIGIL");
            }
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[test]
    fn lock_rejected_before_a_relayer_is_ever_configured() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let bridge = UsdsBridgeBridge::new(None, None);
        let msg = format!("sigil-rpc/v1|usds_bridge_lock|{from_hex}|1000|{DEST}|nonce=1");
        let sig = sign(&sk, &msg);
        assert_eq!(bridge.submit_lock(&from_hex, 1000, DEST, &sig, 1).unwrap_err(), UsdsBridgeError::NotActivated);
    }

    #[test]
    fn a_native_bridge_signature_cannot_authorize_a_usds_bridge_lock() {
        // The whole point of the separate message namespace: signing the
        // NATIVE bridge's "bridge_lock" message must NOT authorize a
        // "usds_bridge_lock" action, even for the same wallet/amount/dest,
        // because the signed bytes differ.
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let (_relayer_sk, relayer) = signer();
        let bridge = UsdsBridgeBridge::new(None, Some(relayer));
        let native_msg = format!("sigil-rpc/v1|bridge_lock|{from_hex}|1000|{DEST}|nonce=1");
        let sig = sign(&sk, &native_msg);
        assert_eq!(
            bridge.submit_lock(&from_hex, 1000, DEST, &sig, 1).unwrap_err(),
            UsdsBridgeError::SignatureInvalid
        );
    }

    #[test]
    fn nonce_watermark_is_strictly_increasing_per_wallet() {
        // The nonce guard gates every bridge op (lock/unlock/pause/rotate) — it stops
        // a captured, validly-signed request from being replayed. The watermark must
        // require a STRICTLY greater nonce, per wallet.
        let bridge = UsdsBridgeBridge::new(None, None);
        let alice = [0xAAu8; 32];
        let bob = [0xBBu8; 32];

        // nonce 0 is never valid — the watermark starts at 0 and requires >.
        assert!(matches!(bridge.check_nonce(alice, 0), Err(UsdsBridgeError::ReplayedNonce)));
        // First real nonce accepted; replaying it under the same nonce is rejected.
        assert!(bridge.check_nonce(alice, 1).is_ok());
        assert!(
            matches!(bridge.check_nonce(alice, 1), Err(UsdsBridgeError::ReplayedNonce)),
            "a captured signed request must not replay under its own nonce"
        );
        // A jump forward is fine; an older nonce below the watermark is rejected.
        assert!(bridge.check_nonce(alice, 5).is_ok());
        assert!(matches!(bridge.check_nonce(alice, 3), Err(UsdsBridgeError::ReplayedNonce)));
        assert!(bridge.check_nonce(alice, 6).is_ok());
        // Watermarks are independent per wallet — alice's high nonce doesn't gate bob.
        assert!(bridge.check_nonce(bob, 1).is_ok());
    }

    #[test]
    fn relayer_unlock_round_trip_and_double_unlock_rejected() {
        let (relayer_sk, relayer) = signer();
        let relayer_hex = hex::encode(relayer);
        let (_uk, to) = signer();
        let to_hex = hex::encode(to);
        let bridge = UsdsBridgeBridge::new(None, Some(relayer));

        let msg = format!("sigil-rpc/v1|usds_bridge_unlock|{to_hex}|500|0xburn1|nonce=1");
        let sig = sign(&relayer_sk, &msg);
        let hash = bridge.submit_unlock(&relayer_hex, &to_hex, 500, "0xburn1", &sig, 1).unwrap();

        let snap = bridge.snapshot_for_mint();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].tx.hash(), hash);

        let msg2 = format!("sigil-rpc/v1|usds_bridge_unlock|{to_hex}|500|0xburn1|nonce=2");
        let sig2 = sign(&relayer_sk, &msg2);
        assert_eq!(
            bridge.submit_unlock(&relayer_hex, &to_hex, 500, "0xburn1", &sig2, 2).unwrap_err(),
            UsdsBridgeError::BurnAlreadyProcessed
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
        let bridge = UsdsBridgeBridge::new(Some(admin), Some(relayer));

        let bad_msg = "sigil-rpc/v1|usds_bridge_pause|true|nonce=1".to_string();
        let bad_sig = sign(&attacker_sk, &bad_msg);
        assert_eq!(bridge.set_paused(&attacker_hex, true, &bad_sig, 1).unwrap_err(), UsdsBridgeError::NotAdmin);

        let msg = "sigil-rpc/v1|usds_bridge_pause|true|nonce=1".to_string();
        let sig = sign(&admin_sk, &msg);
        bridge.set_paused(&admin_hex, true, &sig, 1).unwrap();
        assert!(bridge.is_paused());

        let lock_msg = format!("sigil-rpc/v1|usds_bridge_lock|{user_hex}|10|{DEST}|nonce=1");
        let lock_sig = sign(&user_sk, &lock_msg);
        assert_eq!(bridge.submit_lock(&user_hex, 10, DEST, &lock_sig, 1).unwrap_err(), UsdsBridgeError::Paused);

        let unlock_msg = format!("sigil-rpc/v1|usds_bridge_unlock|{user_hex}|10|0xburn3|nonce=1");
        let unlock_sig = sign(&relayer_sk, &unlock_msg);
        assert_eq!(
            bridge.submit_unlock(&relayer_hex, &user_hex, 10, "0xburn3", &unlock_sig, 1).unwrap_err(),
            UsdsBridgeError::Paused
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
        let bridge = UsdsBridgeBridge::new(Some(admin), Some(old_relayer));

        let msg = format!("sigil-rpc/v1|usds_bridge_rotate_relayer|{new_relayer_hex}|nonce=1");
        let sig = sign(&admin_sk, &msg);
        bridge.rotate_relayer(&admin_hex, &new_relayer_hex, &sig, 1).unwrap();
        assert_eq!(bridge.relayer_hex(), Some(new_relayer_hex));

        let unlock_msg = format!("sigil-rpc/v1|usds_bridge_unlock|{to_hex}|10|0xburn4|nonce=1");
        let unlock_sig = sign(&old_relayer_sk, &unlock_msg);
        assert_eq!(
            bridge.submit_unlock(&old_relayer_hex, &to_hex, 10, "0xburn4", &unlock_sig, 1).unwrap_err(),
            UsdsBridgeError::NotRelayer
        );
    }

    #[test]
    fn confirm_applied_retires_only_named_hashes_from_both_pools() {
        let (sk1, from1) = signer();
        let from1_hex = hex::encode(from1);
        let (relayer_sk, relayer) = signer();
        let relayer_hex = hex::encode(relayer);
        let (_uk, to) = signer();
        let to_hex = hex::encode(to);
        let bridge = UsdsBridgeBridge::new(None, Some(relayer));

        let lock_msg = format!("sigil-rpc/v1|usds_bridge_lock|{from1_hex}|10|{DEST}|nonce=1");
        let lock_sig = sign(&sk1, &lock_msg);
        bridge.submit_lock(&from1_hex, 10, DEST, &lock_sig, 1).unwrap();

        let unlock_msg = format!("sigil-rpc/v1|usds_bridge_unlock|{to_hex}|20|0xburn5|nonce=1");
        let unlock_sig = sign(&relayer_sk, &unlock_msg);
        let unlock_hash = bridge.submit_unlock(&relayer_hex, &to_hex, 20, "0xburn5", &unlock_sig, 1).unwrap();

        assert_eq!(bridge.snapshot_for_mint().len(), 2);
        bridge.confirm_applied(&[unlock_hash]);
        let snap = bridge.snapshot_for_mint();
        assert_eq!(snap.len(), 1);
        match &snap[0].tx {
            SigilTx::Send { to, .. } => assert_eq!(*to, USDS_BRIDGE_VAULT_WALLET),
            other => panic!("expected the lock Send, got {other:?}"),
        }
    }
}
