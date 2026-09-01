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
//! ## Why this is a confirm-on-settle pending pool, not a pop-once queue
//!
//! SIGIL's braid mints SEVERAL competing candidate blocks at the same height
//! before settlement (`dag_drain_apply`, in `sigil-node`) picks a winner —
//! measured live: three candidates at one height, racing off the same
//! parent, only one of which became part of the settled spine. A `.drain()`
//! that removes a pending send the instant it's handed to ONE candidate
//! loses that send forever the moment that specific candidate is the one
//! that orphans — even though nothing ever rejected it. (This is exactly
//! what happened to the first real send tested against this endpoint: a
//! real txid came back, the tx was genuinely computed and embedded in a
//! real block, and it still never landed, because that block wasn't the one
//! the DAG kept.)
//!
//! So `snapshot_for_mint` is non-destructive: every still-pending send rides
//! along on EVERY candidate the producer mints, at every height, until
//! `confirm_applied` retires it — which the caller (`sigil-node`) invokes
//! ONLY for the tx hashes actually carried by whichever specific candidate
//! is confirmed on the settled spine, never for an orphaned sibling. A send
//! that will never succeed (insufficient balance forever, e.g.) doesn't
//! retry forever either — `MAX_ATTEMPTS`/`MAX_AGE` give up and log loudly.
//!
//! This is also the fast path for real throughput: every pending send is
//! embedded in EVERY candidate at the current tick (tens of candidates/sec
//! at this braid's cadence), so the expected time-to-land is one settled
//! block, not one lucky draw — no added latency from retry backoff, no
//! round-trip HTTP re-submission, just riding the producer's own cadence.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sigil_header::{PubKeyBytes, SigScheme, SignatureBytes};
use sigil_state::{WalletId, NATIVE};
use sigil_tx::{SigilTx, SignedTx};

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

/// Authenticated, not-yet-confirmed sends, plus the per-wallet replay guard.
/// Keyed by `SigilTx::hash()` — content-addressed, so resubmitting the exact
/// same (from,to,amount,token) while it's already pending is a safe no-op
/// rather than a duplicate entry.
pub struct SendBridge {
    pending: Mutex<HashMap<[u8; 32], Pending>>,
    /// Last-accepted `req_nonce` per sender. The wallet sends `Date.now()`
    /// (milliseconds) as the nonce, so "strictly greater than last accepted"
    /// is both the replay guard and a free per-wallet ordering check — no
    /// separate sequence counter needed.
    nonce_watermark: Mutex<HashMap<WalletId, u64>>,
}

impl Default for SendBridge {
    fn default() -> Self {
        Self { pending: Mutex::new(HashMap::new()), nonce_watermark: Mutex::new(HashMap::new()) }
    }
}

/// Why a submitted send was rejected before ever reaching the pending pool.
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
    // ASCII guard (DoS hardening): a crafted 64-BYTE non-ASCII string would split a UTF-8
    // boundary in the byte-slice below and PANIC — a request must never crash the API.
    if s.len() != 64 || !s.is_ascii() { return None; }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

pub(crate) fn to_signed(tx: SigilTx) -> SignedTx {
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

impl SendBridge {
    pub fn new() -> Self { Self::default() }

    /// Authenticate a wallet's send request and, on success, add it to the
    /// pending pool. Returns the `SigilTx` hash (the block-level tx id the
    /// wallet's receipt/status views key on) so the caller can ack it back
    /// immediately — the tx is pending, not yet settled, when this returns.
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
        self.pending.lock().unwrap().entry(tx_hash).or_insert_with(|| Pending {
            tx,
            attempts: 0,
            first_seen: Instant::now(),
        });
        Ok(tx_hash)
    }

    /// Snapshot every still-pending send for the producer's CURRENT mint
    /// attempt — called once per candidate block, NOT once per settled
    /// height (see module docs for why this must not be destructive).
    /// Each becomes a `SignedTx` with a placeholder Ed25519Hot signature
    /// (`apply_tx` only calls `precheck`, which checks sig LENGTH and the
    /// `from_pubkey == fee_payer()` binding — both satisfied here — never
    /// the actual signature; real authentication already happened in
    /// `submit`). Entries that have exhausted their retry budget are
    /// dropped here (and logged) rather than embedded again.
    pub fn snapshot_for_mint(&self) -> Vec<SignedTx> {
        let mut guard = self.pending.lock().unwrap();
        let mut out = Vec::with_capacity(guard.len());
        guard.retain(|hash, p| {
            if p.attempts >= MAX_ATTEMPTS || p.first_seen.elapsed() >= MAX_AGE {
                eprintln!(
                    "✗ send gave up after {} attempts / {:.1}s (still not landed — likely stuck on \
                     insufficient balance) hash={}",
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

    /// Retire the given tx hashes — called by the producer ONLY for hashes
    /// carried by a candidate that's been confirmed on the settled spine.
    /// Anything not in `hashes` (including sends that rode along on an
    /// orphaned sibling of the same height) stays pending for the next tick.
    pub fn confirm_applied(&self, hashes: &[[u8; 32]]) {
        if hashes.is_empty() { return; }
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

    /// Guard-rail for the defect that silently disabled this whole path: `MAX_AGE` must
    /// exceed the time a candidate needs to become eligible to SETTLE, or nothing
    /// submitted here can ever complete. Deliberately restates its inputs rather than
    /// importing them — `sigil-api` does not depend on `sigil-dagknight`, and the point
    /// is to fail loudly if someone re-derives these without redoing this arithmetic.
    /// `final_depth = 512` at the measured live block rate (6.28 blk/s, 2026-08-26)
    /// = ~81.5s to the EARLIEST possible settlement. The old 60s was below that floor.
    #[test]
    fn max_age_clears_the_worst_case_finality_lag() {
        const FINAL_DEPTH: u64 = 512;
        const SLOWEST_MEASURED_BLOCK_RATE_PER_SEC: u64 = 6;
        let earliest_settlement_secs = FINAL_DEPTH / SLOWEST_MEASURED_BLOCK_RATE_PER_SEC;
        assert!(
            MAX_AGE.as_secs() > earliest_settlement_secs,
            "MAX_AGE {}s is at or below the {}s floor before a candidate can settle — \
             at this value NO tx on this path can ever complete",
            MAX_AGE.as_secs(),
            earliest_settlement_secs,
        );
    }

    /// `MAX_ATTEMPTS` must not bind before `MAX_AGE`. This is not hypothetical: raising
    /// only `MAX_AGE` and leaving the attempt cap is exactly how the shielded path stayed
    /// broken after its own fix (`gave up after 600 attempts / 92.0s`). Offer cadence is
    /// one per candidate mint, ~6.7/s measured, so budget against the fastest plausible.
    #[test]
    fn max_attempts_does_not_bind_before_max_age() {
        const FASTEST_OFFER_CADENCE_PER_SEC: u64 = 9;
        assert!(
            u64::from(MAX_ATTEMPTS) >= MAX_AGE.as_secs() * FASTEST_OFFER_CADENCE_PER_SEC,
            "MAX_ATTEMPTS {} cuts this path off after ~{}s, before MAX_AGE {}s",
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

    fn sign_send(sk: &ed25519_dalek::SigningKey, from: &str, to: &str, amount: u128, nonce: u64) -> String {
        use ed25519_dalek::Signer;
        let msg = format!("sigil-rpc/v1|send|{from}|{to}|SIGIL|{amount}|nonce={nonce}");
        hex::encode(sk.sign(msg.as_bytes()).to_bytes())
    }

    #[test]
    fn a_correctly_signed_send_is_accepted_and_pending() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let to_hex = "22".repeat(32);
        let sig = sign_send(&sk, &from_hex, &to_hex, 1_000, 1);

        let bridge = SendBridge::new();
        let hash = bridge.submit(&from_hex, &to_hex, 1_000, "SIGIL", &sig, 1).unwrap();
        assert_eq!(bridge.pending_len(), 1);

        let snap = bridge.snapshot_for_mint();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].tx.hash(), hash);
        assert!(snap[0].precheck().is_ok(), "placeholder-signed tx must pass precheck");
        match &snap[0].tx {
            SigilTx::Send { from: f, to: t, amount, token, fee } => {
                assert_eq!(*f, from);
                assert_eq!(hex::encode(t), to_hex);
                assert_eq!(*amount, 1_000);
                assert_eq!(*token, NATIVE);
                assert_eq!(*fee, 0);
            }
            other => panic!("expected Send, got {other:?}"),
        }
        // NOT destructive: still pending after a snapshot.
        assert_eq!(bridge.pending_len(), 1);
    }

    #[test]
    fn snapshot_keeps_offering_a_send_across_many_mint_attempts() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let to_hex = "88".repeat(32);
        let sig = sign_send(&sk, &from_hex, &to_hex, 1, 1);
        let bridge = SendBridge::new();
        let hash = bridge.submit(&from_hex, &to_hex, 1, "SIGIL", &sig, 1).unwrap();

        // Simulates several competing candidates minting before ANY of them
        // gets confirmed — the exact scenario that lost the first real send.
        for _ in 0..5 {
            let snap = bridge.snapshot_for_mint();
            assert_eq!(snap.len(), 1, "must keep being offered until confirmed");
            assert_eq!(snap[0].tx.hash(), hash);
        }
        assert_eq!(bridge.pending_len(), 1, "still pending — nothing has confirmed it yet");
    }

    #[test]
    fn confirm_applied_retires_only_the_named_hashes() {
        let (sk1, from1) = signer();
        let (sk2, from2) = signer();
        let to_hex = "99".repeat(32);
        let from1_hex = hex::encode(from1);
        let from2_hex = hex::encode(from2);
        let sig1 = sign_send(&sk1, &from1_hex, &to_hex, 10, 1);
        let sig2 = sign_send(&sk2, &from2_hex, &to_hex, 20, 1);

        let bridge = SendBridge::new();
        let h1 = bridge.submit(&from1_hex, &to_hex, 10, "SIGIL", &sig1, 1).unwrap();
        let h2 = bridge.submit(&from2_hex, &to_hex, 20, "SIGIL", &sig2, 1).unwrap();
        assert_eq!(bridge.pending_len(), 2);

        // Simulates: candidate carrying h1 got orphaned (never confirmed);
        // a LATER candidate carrying only h2 is the one that actually landed.
        bridge.confirm_applied(&[h2]);
        assert_eq!(bridge.pending_len(), 1, "only h2 should be retired");

        let snap = bridge.snapshot_for_mint();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].tx.hash(), h1, "h1 must still be pending and still offered");
    }

    #[test]
    fn a_send_that_never_lands_gives_up_after_the_attempt_budget() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let to_hex = "aa".repeat(32);
        let sig = sign_send(&sk, &from_hex, &to_hex, 1, 1);
        let bridge = SendBridge::new();
        bridge.submit(&from_hex, &to_hex, 1, "SIGIL", &sig, 1).unwrap();

        // The entry is legitimately OFFERED MAX_ATTEMPTS times (attempts 0..MAX_ATTEMPTS-1
        // all pass the `>=` check before incrementing) — it's the (MAX_ATTEMPTS+1)th call
        // that finally sees attempts==MAX_ATTEMPTS and gives up.
        for _ in 0..=MAX_ATTEMPTS {
            bridge.snapshot_for_mint();
        }
        assert_eq!(bridge.pending_len(), 0, "must give up, not retry forever");
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
    fn a_bad_signature_does_not_burn_the_nonce() {
        // submit verifies the signature BEFORE advancing the nonce watermark, so a
        // request that fails auth must NOT consume the nonce — else an attacker who
        // knows a wallet's next nonce could grief it by burning that nonce with a
        // garbage-signed send, blocking the wallet's real send. (Same invariant as
        // the USDS/BTC bridges; this is the most-used money path.)
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let to_hex = "22".repeat(32);
        let bridge = SendBridge::new();

        // Valid-LENGTH but cryptographically wrong signature (64 zero bytes) — fails
        // vk.verify, which runs before the nonce advances.
        let bad_sig = "00".repeat(64);
        assert!(bridge.submit(&from_hex, &to_hex, 1_000, "SIGIL", &bad_sig, 1).is_err());
        assert_eq!(bridge.pending_len(), 0, "a rejected send never enters the pool");

        // The correctly-signed nonce-1 send STILL works — the failed attempt did
        // not consume nonce 1.
        let sig = sign_send(&sk, &from_hex, &to_hex, 1_000, 1);
        assert!(
            bridge.submit(&from_hex, &to_hex, 1_000, "SIGIL", &sig, 1).is_ok(),
            "the bad-sig attempt must not have burned nonce 1"
        );
        assert_eq!(bridge.pending_len(), 1);
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
    fn zero_amount_is_rejected_before_touching_the_pending_pool() {
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
