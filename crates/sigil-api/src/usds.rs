//! usds.rs — the wallet-authenticated USDS mint/redeem surface.
//!
//! Same shape as [`crate::send::SendBridge`] / [`crate::dex::DexBridge`]:
//! authenticate an RPC-style signed message here, hand the producer a
//! ready-to-include [`SigilTx`]. All the real math (the 105% collateral
//! buffer + the sigil-bank protocol fee) lives in `sigil_usds::plan_mint` /
//! `plan_redeem`, which `apply_tx` calls when the tx is actually applied —
//! this bridge adds no new consensus logic, only the authenticate-and-queue
//! step, same division of labor as every other bridge in this crate. See
//! `send` module docs for why a confirm-on-settle pending pool (not a
//! pop-once queue) is required on this braid.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sigil_header::{PubKeyBytes, SigScheme, SignatureBytes};
use sigil_state::WalletId;
use sigil_tx::{SigilTx, SignedTx};

use crate::hex32;

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

/// Authenticated, not-yet-confirmed USDS mint/redeem actions, plus the
/// per-wallet replay guard (shared across both actions — same "one
/// watermark per wallet" reasoning as `DexBridge`).
pub struct UsdsBridge {
    pending: Mutex<HashMap<[u8; 32], Pending>>,
    nonce_watermark: Mutex<HashMap<WalletId, u64>>,
}

impl Default for UsdsBridge {
    fn default() -> Self {
        Self { pending: Mutex::new(HashMap::new()), nonce_watermark: Mutex::new(HashMap::new()) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsdsBridgeError {
    BadFromAddress,
    ZeroAmount,
    BadSignatureEncoding,
    SignatureInvalid,
    ReplayedNonce,
}

impl UsdsBridgeError {
    pub fn message(self) -> &'static str {
        match self {
            UsdsBridgeError::BadFromAddress => "from must be a 64-hex address",
            UsdsBridgeError::ZeroAmount => "amount must be > 0",
            UsdsBridgeError::BadSignatureEncoding => "sig must be 128 hex chars (64 bytes)",
            UsdsBridgeError::SignatureInvalid => "signature does not match the sending wallet",
            UsdsBridgeError::ReplayedNonce => "req_nonce must be greater than the last accepted nonce for this wallet",
        }
    }
}

fn verify_sig(actor: &WalletId, msg: &str, sig_hex: &str) -> Result<(), UsdsBridgeError> {
    let sig_hex = sig_hex.strip_prefix("0x").unwrap_or(sig_hex);
    let sig_bytes = hex::decode(sig_hex).map_err(|_| UsdsBridgeError::BadSignatureEncoding)?;
    let sig_arr: [u8; 64] = sig_bytes.try_into().map_err(|_| UsdsBridgeError::BadSignatureEncoding)?;
    let vk = VerifyingKey::from_bytes(actor).map_err(|_| UsdsBridgeError::SignatureInvalid)?;
    let sig = Signature::from_bytes(&sig_arr);
    vk.verify(msg.as_bytes(), &sig).map_err(|_| UsdsBridgeError::SignatureInvalid)
}

fn check_nonce(watermark: &Mutex<HashMap<WalletId, u64>>, from: WalletId, req_nonce: u64) -> Result<(), UsdsBridgeError> {
    let mut wm = watermark.lock().unwrap();
    let last = wm.get(&from).copied().unwrap_or(0);
    if req_nonce <= last {
        return Err(UsdsBridgeError::ReplayedNonce);
    }
    wm.insert(from, req_nonce);
    Ok(())
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

impl UsdsBridge {
    pub fn new() -> Self {
        Self::default()
    }

    /// `sigil-rpc/v1|usds_mint|{from}|{sigil_amount}|nonce={req_nonce}`
    pub fn submit_mint(
        &self,
        from_hex: &str,
        sigil_amount: u128,
        sig_hex: &str,
        req_nonce: u64,
    ) -> Result<[u8; 32], UsdsBridgeError> {
        let from = hex32(from_hex).ok_or(UsdsBridgeError::BadFromAddress)?;
        if sigil_amount == 0 {
            return Err(UsdsBridgeError::ZeroAmount);
        }
        let msg = format!("sigil-rpc/v1|usds_mint|{from_hex}|{sigil_amount}|nonce={req_nonce}");
        verify_sig(&from, &msg, sig_hex)?;
        check_nonce(&self.nonce_watermark, from, req_nonce)?;

        let tx = SigilTx::UsdsMint { from, sigil_amount, fee: 0 };
        Ok(self.queue(tx))
    }

    /// `sigil-rpc/v1|usds_redeem|{from}|{usds_amount}|nonce={req_nonce}`
    pub fn submit_redeem(
        &self,
        from_hex: &str,
        usds_amount: u128,
        sig_hex: &str,
        req_nonce: u64,
    ) -> Result<[u8; 32], UsdsBridgeError> {
        let from = hex32(from_hex).ok_or(UsdsBridgeError::BadFromAddress)?;
        if usds_amount == 0 {
            return Err(UsdsBridgeError::ZeroAmount);
        }
        let msg = format!("sigil-rpc/v1|usds_redeem|{from_hex}|{usds_amount}|nonce={req_nonce}");
        verify_sig(&from, &msg, sig_hex)?;
        check_nonce(&self.nonce_watermark, from, req_nonce)?;

        let tx = SigilTx::UsdsRedeem { from, usds_amount, fee: 0 };
        Ok(self.queue(tx))
    }

    fn queue(&self, tx: SigilTx) -> [u8; 32] {
        let tx_hash = tx.hash();
        self.pending.lock().unwrap().entry(tx_hash).or_insert_with(|| Pending {
            tx,
            attempts: 0,
            first_seen: Instant::now(),
        });
        tx_hash
    }

    /// Snapshot every still-pending mint/redeem for the producer's CURRENT
    /// mint attempt — same non-destructive shape as every other bridge here.
    pub fn snapshot_for_mint(&self) -> Vec<SignedTx> {
        let mut guard = self.pending.lock().unwrap();
        let mut out = Vec::with_capacity(guard.len());
        guard.retain(|hash, p| {
            if p.attempts >= MAX_ATTEMPTS || p.first_seen.elapsed() >= MAX_AGE {
                eprintln!(
                    "\u{2717} usds action gave up after {} attempts / {:.1}s (still not landed) hash={}",
                    p.attempts, p.first_seen.elapsed().as_secs_f64(), hex::encode(hash)
                );
                return false;
            }
            p.attempts += 1;
            out.push(to_signed(p.tx.clone()));
            true
        });
        out
    }

    pub fn confirm_applied(&self, hashes: &[[u8; 32]]) {
        if hashes.is_empty() {
            return;
        }
        let mut guard = self.pending.lock().unwrap();
        for h in hashes {
            guard.remove(h);
        }
    }

    pub fn pending_len(&self) -> usize {
        self.pending.lock().unwrap().len()
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

    #[test]
    fn a_correctly_signed_mint_is_accepted_and_queued() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let msg = format!("sigil-rpc/v1|usds_mint|{from_hex}|1000|nonce=1");
        let sig = sign(&sk, &msg);

        let bridge = UsdsBridge::new();
        let hash = bridge.submit_mint(&from_hex, 1000, &sig, 1).unwrap();
        assert_eq!(bridge.pending_len(), 1);

        let snap = bridge.snapshot_for_mint();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].tx.hash(), hash);
        assert!(snap[0].precheck().is_ok(), "placeholder-signed tx must pass precheck");
        match &snap[0].tx {
            SigilTx::UsdsMint { from: f, sigil_amount, .. } => {
                assert_eq!(*f, from);
                assert_eq!(*sigil_amount, 1000);
            }
            other => panic!("expected UsdsMint, got {other:?}"),
        }
        assert_eq!(bridge.pending_len(), 1, "NOT destructive");
    }

    #[test]
    fn a_correctly_signed_redeem_is_accepted_and_queued() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let msg = format!("sigil-rpc/v1|usds_redeem|{from_hex}|500|nonce=1");
        let sig = sign(&sk, &msg);

        let bridge = UsdsBridge::new();
        let hash = bridge.submit_redeem(&from_hex, 500, &sig, 1).unwrap();
        let snap = bridge.snapshot_for_mint();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].tx.hash(), hash);
        match &snap[0].tx {
            SigilTx::UsdsRedeem { from: f, usds_amount, .. } => {
                assert_eq!(*f, from);
                assert_eq!(*usds_amount, 500);
            }
            other => panic!("expected UsdsRedeem, got {other:?}"),
        }
    }

    #[test]
    fn confirm_applied_retires_only_the_named_hashes() {
        let (sk1, from1) = signer();
        let (sk2, from2) = signer();
        let from1_hex = hex::encode(from1);
        let from2_hex = hex::encode(from2);
        let msg1 = format!("sigil-rpc/v1|usds_mint|{from1_hex}|10|nonce=1");
        let msg2 = format!("sigil-rpc/v1|usds_mint|{from2_hex}|20|nonce=1");
        let sig1 = sign(&sk1, &msg1);
        let sig2 = sign(&sk2, &msg2);

        let bridge = UsdsBridge::new();
        let h1 = bridge.submit_mint(&from1_hex, 10, &sig1, 1).unwrap();
        let h2 = bridge.submit_mint(&from2_hex, 20, &sig2, 1).unwrap();
        assert_eq!(bridge.pending_len(), 2);

        bridge.confirm_applied(&[h2]);
        assert_eq!(bridge.pending_len(), 1);
        let snap = bridge.snapshot_for_mint();
        assert_eq!(snap[0].tx.hash(), h1);
    }

    #[test]
    fn a_tampered_amount_fails_verification() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let msg = format!("sigil-rpc/v1|usds_mint|{from_hex}|1000|nonce=1");
        let sig = sign(&sk, &msg);

        let bridge = UsdsBridge::new();
        let err = bridge.submit_mint(&from_hex, 9_000_000, &sig, 1).unwrap_err();
        assert_eq!(err, UsdsBridgeError::SignatureInvalid);
        assert_eq!(bridge.pending_len(), 0);
    }

    #[test]
    fn someone_elses_signature_cannot_authorize_a_mint() {
        let (_attacker_sk, attacker_addr) = signer();
        let (victim_sk, _victim_addr) = signer();
        let attacker_hex = hex::encode(attacker_addr);
        let msg = format!("sigil-rpc/v1|usds_mint|{attacker_hex}|500|nonce=1");
        let sig = sign(&victim_sk, &msg);

        let bridge = UsdsBridge::new();
        let err = bridge.submit_mint(&attacker_hex, 500, &sig, 1).unwrap_err();
        assert_eq!(err, UsdsBridgeError::SignatureInvalid);
    }

    #[test]
    fn a_replayed_nonce_is_rejected_even_with_a_valid_signature() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let msg = format!("sigil-rpc/v1|usds_mint|{from_hex}|10|nonce=5");
        let sig = sign(&sk, &msg);

        let bridge = UsdsBridge::new();
        bridge.submit_mint(&from_hex, 10, &sig, 5).unwrap();
        let err = bridge.submit_mint(&from_hex, 10, &sig, 5).unwrap_err();
        assert_eq!(err, UsdsBridgeError::ReplayedNonce);
    }

    #[test]
    fn zero_amount_is_rejected_before_touching_the_pending_pool() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let msg = format!("sigil-rpc/v1|usds_mint|{from_hex}|0|nonce=1");
        let sig = sign(&sk, &msg);

        let bridge = UsdsBridge::new();
        assert_eq!(
            bridge.submit_mint(&from_hex, 0, &sig, 1).unwrap_err(),
            UsdsBridgeError::ZeroAmount
        );
    }

    #[test]
    fn malformed_addresses_and_signatures_are_rejected_cleanly() {
        let bridge = UsdsBridge::new();
        assert_eq!(
            bridge.submit_mint("not-hex", 1, &"00".repeat(64), 1).unwrap_err(),
            UsdsBridgeError::BadFromAddress
        );
        let ok32 = "11".repeat(32);
        assert_eq!(
            bridge.submit_mint(&ok32, 1, "zz", 1).unwrap_err(),
            UsdsBridgeError::BadSignatureEncoding
        );
    }
}
