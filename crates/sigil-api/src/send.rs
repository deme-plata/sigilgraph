//! send.rs — the wallet-authenticated send surface.
//!
//! Mirrors [`crate::mining::MiningBridge`]'s proven shape: authenticate here,
//! hand the producer a ready-to-include tx. `apply_tx` (`sigil_tx`) only ever
//! calls `SignedTx::precheck` — a length/binding sanity check — never
//! `verify_signature`; that's the SAME idiom `sigil_tx::wrap_op` already uses
//! to feed `AuthorizedBatch` ops (verified once, upstream, by a different
//! signature) into `apply_tx` with a placeholder Ed25519Hot signature. This
//! bridge is that idiom for the HTTP path: the wallet doesn't sign
//! `SigilTx::hash()` (that would need it to replicate Rust's serde_json
//! canonical encoding byte-for-byte — a needless, fragile ask for a phase-0
//! wallet). It signs a stable RPC message instead — the SAME
//! `sigil-rpc/v1|<action>|<fields>|nonce=<n>` / raw-pubkey-as-address scheme
//! `window.sigilSign` already uses (`gui/sigil-wallet-tron-embedded.html`) —
//! and `submit` verifies exactly that.
//!
//! Queueing (rather than routing through `Mempool::ingest`, which insists on
//! a `SigilTx::hash()` signature) also means a send lands in the VERY NEXT
//! block the producer mints — one mutex-guarded drain per tick, no
//! re-verification, no mempool-wide lock contention. That's the whole
//! "highest performance" ask: the fastest a send can possibly land is the
//! producer's own block cadence, and this puts it exactly there.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sigil_header::{PubKeyBytes, SigScheme, SignatureBytes};
use sigil_state::{WalletId, NATIVE};
use sigil_tx::{SigilTx, SignedTx};

struct Queued {
    tx: SigilTx,
}

/// Authenticated, not-yet-minted sends, plus the per-wallet replay guard.
#[derive(Default)]
pub struct SendBridge {
    queue: Mutex<VecDeque<Queued>>,
    /// Last-accepted `req_nonce` per sender. The wallet sends `Date.now()`
    /// (milliseconds) as the nonce, so "strictly greater than last accepted"
    /// is both the replay guard and a free per-wallet ordering check — no
    /// separate sequence counter needed.
    nonce_watermark: Mutex<HashMap<WalletId, u64>>,
}

/// Why a submitted send was rejected before ever reaching the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    BadFromAddress,
    BadToAddress,
    UnsupportedToken,
    ZeroAmount,
    BadSignatureEncoding,
    SignatureInvalid,
    ReplayedNonce,
}

impl SendError {
    pub fn message(self) -> &'static str {
        match self {
            SendError::BadFromAddress => "from must be a 64-hex address",
            SendError::BadToAddress => "to must be a 64-hex address",
            SendError::UnsupportedToken => "only token \"SIGIL\" is accepted on this endpoint",
            SendError::ZeroAmount => "amount must be > 0",
            SendError::BadSignatureEncoding => "sig must be 128 hex chars (64 bytes)",
            SendError::SignatureInvalid => "signature does not match the sending wallet",
            SendError::ReplayedNonce => "req_nonce must be greater than the last accepted nonce for this wallet",
        }
    }
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

impl SendBridge {
    pub fn new() -> Self { Self::default() }

    /// Authenticate a wallet's send request and, on success, queue it for
    /// the next block. Returns the `SigilTx` hash (the block-level tx id the
    /// wallet's receipt/status views key on) so the caller can ack it back
    /// immediately — the tx is queued, not yet mined, when this returns.
    ///
    /// `from_hex`/`to_hex` are re-used VERBATIM (not re-encoded from the
    /// parsed bytes) when rebuilding the signed message: casing must match
    /// exactly what the wallet actually signed, and the wallet always
    /// lower-cases both before signing (`doSend`'s `.toLowerCase()`), so this
    /// only matters if a caller ever sends mixed-case hex — reusing the raw
    /// string keeps that a signature failure instead of a silent mismatch.
    pub fn submit(
        &self,
        from_hex: &str,
        to_hex: &str,
        amount: u128,
        token: &str,
        sig_hex: &str,
        req_nonce: u64,
    ) -> Result<[u8; 32], SendError> {
        let from = hex32(from_hex).ok_or(SendError::BadFromAddress)?;
        let to = hex32(to_hex).ok_or(SendError::BadToAddress)?;
        if !token.eq_ignore_ascii_case("SIGIL") {
            return Err(SendError::UnsupportedToken);
        }
        if amount == 0 {
            return Err(SendError::ZeroAmount);
        }

        let sig_hex_trimmed = sig_hex.strip_prefix("0x").unwrap_or(sig_hex);
        let sig_bytes = hex::decode(sig_hex_trimmed).map_err(|_| SendError::BadSignatureEncoding)?;
        let sig_arr: [u8; 64] =
            sig_bytes.try_into().map_err(|_| SendError::BadSignatureEncoding)?;

        // Exactly `window.sigilSign(priv,'send',[from,to,'SIGIL',base.toString()],reqNonce)`:
        //   sigil-rpc/v1|send|{from}|{to}|SIGIL|{amount}|nonce={req_nonce}
        let msg = format!("sigil-rpc/v1|send|{from_hex}|{to_hex}|SIGIL|{amount}|nonce={req_nonce}");

        let vk = VerifyingKey::from_bytes(&from).map_err(|_| SendError::SignatureInvalid)?;
        let sig = Signature::from_bytes(&sig_arr);
        vk.verify(msg.as_bytes(), &sig).map_err(|_| SendError::SignatureInvalid)?;

        {
            let mut wm = self.nonce_watermark.lock().unwrap();
            let last = wm.get(&from).copied().unwrap_or(0);
            if req_nonce <= last {
                return Err(SendError::ReplayedNonce);
            }
            wm.insert(from, req_nonce);
        }

        let tx = SigilTx::Send { from, to, amount, token: NATIVE, fee: 0 };
        let tx_hash = tx.hash();
        self.queue.lock().unwrap().push_back(Queued { tx });
        Ok(tx_hash)
    }

    /// Drain everything queued — called once per producer tick, mirroring
    /// `MiningBridge::take_solve`. Each becomes a `SignedTx` with a
    /// placeholder Ed25519Hot signature (`apply_tx` only calls `precheck`,
    /// which checks sig LENGTH and the `from_pubkey == fee_payer()` binding —
    /// both satisfied here — never the actual signature; real authentication
    /// already happened in `submit`).
    pub fn drain(&self) -> Vec<SignedTx> {
        self.queue
            .lock()
            .unwrap()
            .drain(..)
            .map(|q| {
                let payer = q.tx.fee_payer();
                SignedTx {
                    tx: q.tx,
                    from_pubkey: payer,
                    nonce: 0,
                    sig_scheme: SigScheme::Ed25519Hot,
                    sig: SignatureBytes(vec![0u8; SigScheme::Ed25519Hot.expected_sig_len()]),
                    pubkey: PubKeyBytes(Vec::new()),
                }
            })
            .collect()
    }

    pub fn pending_len(&self) -> usize {
        self.queue.lock().unwrap().len()
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

    fn sign_send(sk: &ed25519_dalek::SigningKey, from: &str, to: &str, amount: u128, nonce: u64) -> String {
        use ed25519_dalek::Signer;
        let msg = format!("sigil-rpc/v1|send|{from}|{to}|SIGIL|{amount}|nonce={nonce}");
        hex::encode(sk.sign(msg.as_bytes()).to_bytes())
    }

    #[test]
    fn a_correctly_signed_send_is_accepted_and_queued() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let to_hex = "22".repeat(32);
        let sig = sign_send(&sk, &from_hex, &to_hex, 1_000, 1);

        let bridge = SendBridge::new();
        let hash = bridge.submit(&from_hex, &to_hex, 1_000, "SIGIL", &sig, 1).unwrap();
        assert_eq!(bridge.pending_len(), 1);

        let drained = bridge.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].tx.hash(), hash);
        assert!(drained[0].precheck().is_ok(), "placeholder-signed tx must pass precheck");
        match &drained[0].tx {
            SigilTx::Send { from: f, to: t, amount, token, fee } => {
                assert_eq!(*f, from);
                assert_eq!(hex::encode(t), to_hex);
                assert_eq!(*amount, 1_000);
                assert_eq!(*token, NATIVE);
                assert_eq!(*fee, 0);
            }
            other => panic!("expected Send, got {other:?}"),
        }
        assert_eq!(bridge.pending_len(), 0, "drain must empty the queue");
    }

    #[test]
    fn a_tampered_amount_fails_verification() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let to_hex = "22".repeat(32);
        // Sign for 1_000 but submit 9_000_000 — the classic "intercept and
        // reamount" attack this signature exists to stop.
        let sig = sign_send(&sk, &from_hex, &to_hex, 1_000, 1);

        let bridge = SendBridge::new();
        let err = bridge.submit(&from_hex, &to_hex, 9_000_000, "SIGIL", &sig, 1).unwrap_err();
        assert_eq!(err, SendError::SignatureInvalid);
        assert_eq!(bridge.pending_len(), 0);
    }

    #[test]
    fn someone_elses_signature_cannot_authorize_a_send() {
        let (_attacker_sk, attacker_addr) = signer();
        let (victim_sk, _victim_addr) = signer();
        let attacker_hex = hex::encode(attacker_addr);
        let to_hex = "33".repeat(32);
        // Signed by the victim's key but claiming to be FROM the attacker's
        // wallet — must fail (verify checks the sig against `from`'s key).
        let sig = sign_send(&victim_sk, &attacker_hex, &to_hex, 500, 1);

        let bridge = SendBridge::new();
        let err = bridge.submit(&attacker_hex, &to_hex, 500, "SIGIL", &sig, 1).unwrap_err();
        assert_eq!(err, SendError::SignatureInvalid);
    }

    #[test]
    fn a_replayed_nonce_is_rejected_even_with_a_valid_signature() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let to_hex = "44".repeat(32);
        let sig = sign_send(&sk, &from_hex, &to_hex, 10, 5);

        let bridge = SendBridge::new();
        bridge.submit(&from_hex, &to_hex, 10, "SIGIL", &sig, 5).unwrap();
        // Exact same signed request, replayed verbatim.
        let err = bridge.submit(&from_hex, &to_hex, 10, "SIGIL", &sig, 5).unwrap_err();
        assert_eq!(err, SendError::ReplayedNonce);
    }

    #[test]
    fn a_lower_nonce_than_a_previously_accepted_one_is_rejected() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let to_hex = "55".repeat(32);
        let sig10 = sign_send(&sk, &from_hex, &to_hex, 1, 10);
        let sig3 = sign_send(&sk, &from_hex, &to_hex, 1, 3);

        let bridge = SendBridge::new();
        bridge.submit(&from_hex, &to_hex, 1, "SIGIL", &sig10, 10).unwrap();
        let err = bridge.submit(&from_hex, &to_hex, 1, "SIGIL", &sig3, 3).unwrap_err();
        assert_eq!(err, SendError::ReplayedNonce);
    }

    #[test]
    fn zero_amount_is_rejected_before_touching_the_queue() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let to_hex = "66".repeat(32);
        let sig = sign_send(&sk, &from_hex, &to_hex, 0, 1);
        let bridge = SendBridge::new();
        assert_eq!(
            bridge.submit(&from_hex, &to_hex, 0, "SIGIL", &sig, 1).unwrap_err(),
            SendError::ZeroAmount
        );
    }

    #[test]
    fn non_sigil_token_is_rejected() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let to_hex = "77".repeat(32);
        let sig = sign_send(&sk, &from_hex, &to_hex, 1, 1);
        let bridge = SendBridge::new();
        assert_eq!(
            bridge.submit(&from_hex, &to_hex, 1, "USDX", &sig, 1).unwrap_err(),
            SendError::UnsupportedToken
        );
    }

    #[test]
    fn malformed_addresses_and_signatures_are_rejected_cleanly() {
        let bridge = SendBridge::new();
        assert_eq!(
            bridge.submit("not-hex", &"11".repeat(32), 1, "SIGIL", &"00".repeat(64), 1).unwrap_err(),
            SendError::BadFromAddress
        );
        assert_eq!(
            bridge.submit(&"11".repeat(32), "short", 1, "SIGIL", &"00".repeat(64), 1).unwrap_err(),
            SendError::BadToAddress
        );
        assert_eq!(
            bridge.submit(&"11".repeat(32), &"22".repeat(32), 1, "SIGIL", "zz", 1).unwrap_err(),
            SendError::BadSignatureEncoding
        );
    }
}
