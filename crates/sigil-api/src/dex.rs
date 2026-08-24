//! dex.rs — the wallet-authenticated DEX surface: swap, add liquidity,
//! remove liquidity.
//!
//! Same shape as [`crate::send::SendBridge`] and [`crate::bridge::BridgeBridge`]:
//! authenticate an RPC-style signed message here, hand the producer a
//! ready-to-include [`SigilTx`]. `apply_tx` (`sigil_tx`) already knows how to
//! turn `SigilTx::Swap` / `LpDeposit` / `LpWithdraw` into real pool-state
//! mutations through the `commit_state_transition` chokepoint — this module
//! adds NO new consensus logic, only the HTTP-facing authenticate-and-queue
//! step those tx types were missing. See `send` module docs for the full
//! "confirm-on-settle pending pool, not a pop-once queue" reasoning (SIGIL's
//! braid mints several competing candidates per height; a destructive queue
//! would lose a swap the instant its candidate gets orphaned, even though it
//! was never rejected) — the exact same argument applies here, so this
//! bridge reuses that pattern rather than inventing a second one.
//!
//! ## Why `add_liquidity` derives the pool id instead of trusting one
//!
//! `SigilTx::LpDeposit` carries an explicit `pool: PoolId` field, but nothing
//! requires a caller to pick it honestly — a client with no chain history
//! could coin an id that happens to collide with an existing pool's, or that
//! deliberately targets the wrong one. Rather than trust a client-supplied
//! id, `submit_lp_deposit` derives it itself via
//! [`sigil_state::derive_pool_id`] from the (token_a, token_b, fee_bps) the
//! caller signed — the same pair + fee always maps to the same pool, so
//! there is no coordinate for a client to get wrong, and `apply_tx`'s
//! existing "verify token_a/token_b/fee_bps against the existing pool on a
//! non-first deposit" check becomes structurally impossible to trip by
//! honest use (a mismatch would mean two different derivations, which can't
//! happen from the same triple).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sigil_header::{PubKeyBytes, SigScheme, SignatureBytes};
use sigil_state::{derive_pool_id, PoolId, TokenId, WalletId};
use sigil_tx::{SigilTx, SignedTx};

use crate::hex32;

/// Same generous, non-hair-trigger retry budget as `SendBridge`/`BridgeBridge`
/// — a fresh candidate mints roughly every producer tick (tens of ms at this
/// braid's cadence), so this is seconds of real wall-clock, not a race.
const MAX_ATTEMPTS: u32 = 2_000;
const MAX_AGE: Duration = Duration::from_secs(60);

/// Basis-point denominator sanity ceiling. `fee_bps` is caller-chosen on a
/// pool's first deposit (see `SigilTx::LpDeposit` docs) — reject anything
/// that couldn't possibly be a real fee (>100%) before it ever reaches the
/// chokepoint, same "fail loud before touching money" posture as the other
/// bridges' input validation.
const MAX_FEE_BPS: u16 = 10_000;

struct Pending {
    tx: SigilTx,
    attempts: u32,
    first_seen: Instant,
}

/// Authenticated, not-yet-confirmed DEX actions, plus the per-wallet replay
/// guard. Keyed by `SigilTx::hash()` — content-addressed, so resubmitting the
/// exact same signed action while it's already pending is a safe no-op.
pub struct DexBridge {
    pending: Mutex<HashMap<[u8; 32], Pending>>,
    /// Last-accepted `req_nonce` per sender, shared across ALL THREE actions
    /// (swap / lp_deposit / lp_withdraw) — same "strictly increasing" replay
    /// guard as `SendBridge`, just one watermark per wallet regardless of
    /// which DEX action it's spending on. A wallet that swaps then adds
    /// liquidity in the same second must still use two different `req_nonce`
    /// values (the wallet already does this — `Date.now()`-based nonces are
    /// monotonic per caller, not per-action).
    nonce_watermark: Mutex<HashMap<WalletId, u64>>,
}

impl Default for DexBridge {
    fn default() -> Self {
        Self { pending: Mutex::new(HashMap::new()), nonce_watermark: Mutex::new(HashMap::new()) }
    }
}

/// Why a submitted DEX action was rejected before ever reaching the pending pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DexError {
    BadFromAddress,
    BadPoolAddress,
    BadTokenAddress,
    BadDirection,
    ZeroAmount,
    FeeTooHigh,
    BadSignatureEncoding,
    SignatureInvalid,
    ReplayedNonce,
}

impl DexError {
    pub fn message(self) -> &'static str {
        match self {
            DexError::BadFromAddress => "from must be a 64-hex address",
            DexError::BadPoolAddress => "pool must be a 64-hex pool id",
            DexError::BadTokenAddress => "token must be a 64-hex token id",
            DexError::BadDirection => "dir must be \"AtoB\" or \"BtoA\"",
            DexError::ZeroAmount => "amount must be > 0",
            DexError::FeeTooHigh => "fee_bps must be <= 10000 (100%)",
            DexError::BadSignatureEncoding => "sig must be 128 hex chars (64 bytes)",
            DexError::SignatureInvalid => "signature does not match the sending wallet",
            DexError::ReplayedNonce => "req_nonce must be greater than the last accepted nonce for this wallet",
        }
    }
}

fn verify_sig(actor: &WalletId, msg: &str, sig_hex: &str) -> Result<(), DexError> {
    let sig_hex = sig_hex.strip_prefix("0x").unwrap_or(sig_hex);
    let sig_bytes = hex::decode(sig_hex).map_err(|_| DexError::BadSignatureEncoding)?;
    let sig_arr: [u8; 64] = sig_bytes.try_into().map_err(|_| DexError::BadSignatureEncoding)?;
    let vk = VerifyingKey::from_bytes(actor).map_err(|_| DexError::SignatureInvalid)?;
    let sig = Signature::from_bytes(&sig_arr);
    vk.verify(msg.as_bytes(), &sig).map_err(|_| DexError::SignatureInvalid)
}

/// Replay guard: accept iff `req_nonce` strictly exceeds this wallet's stored
/// watermark, then advance it. Shared by all three actions below.
fn check_nonce(watermark: &Mutex<HashMap<WalletId, u64>>, from: WalletId, req_nonce: u64) -> Result<(), DexError> {
    let mut wm = watermark.lock().unwrap();
    let last = wm.get(&from).copied().unwrap_or(0);
    if req_nonce <= last {
        return Err(DexError::ReplayedNonce);
    }
    wm.insert(from, req_nonce);
    Ok(())
}

/// Placeholder-signed wrapper: real authentication already happened in
/// `verify_sig` above, before this is ever called. `apply_tx` only calls
/// `SignedTx::precheck` on these tx kinds (a length/binding sanity check),
/// never `verify_signature` — the same idiom `send::SendBridge` and
/// `bridge::BridgeBridge` already rely on for the HTTP-authenticated path.
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

impl DexBridge {
    pub fn new() -> Self {
        Self::default()
    }

    /// `sigil-rpc/v1|swap|{from}|{pool}|{dir}|{in_amt}|{min_out}|nonce={req_nonce}`
    ///
    /// `dir` (`"AtoB"` / `"BtoA"`) is exactly what the wallet's already-shipped
    /// `window.sigilSign(...,'swap',[from,pool.id,dir,amountIn,minOut],reqNonce)`
    /// signs (`gui/sigil-wallet-tron-embedded.html`, same convention
    /// `sigil-rpcd` used) — so the signed message here MUST use `dir`
    /// literally, not the resolved token id, or every real wallet signature
    /// would fail to verify against a message it never signed. `in_token` is
    /// therefore resolved by the CALLER (the HTTP handler, which reads the
    /// live pool's `token_a`/`token_b` — this bridge is deliberately
    /// stateless, same as `SendBridge`/`BridgeBridge`) and passed in
    /// separately for the tx itself; the two must agree on the same `dir`.
    pub fn submit_swap(
        &self,
        from_hex: &str,
        pool_hex: &str,
        dir_hex: &str,
        in_token: TokenId,
        in_amt: u128,
        min_out: u128,
        sig_hex: &str,
        req_nonce: u64,
    ) -> Result<[u8; 32], DexError> {
        let from = hex32(from_hex).ok_or(DexError::BadFromAddress)?;
        let pool: PoolId = hex32(pool_hex).ok_or(DexError::BadPoolAddress)?;
        if dir_hex != "AtoB" && dir_hex != "BtoA" {
            return Err(DexError::BadDirection);
        }
        if in_amt == 0 {
            return Err(DexError::ZeroAmount);
        }

        let msg = format!(
            "sigil-rpc/v1|swap|{from_hex}|{pool_hex}|{dir_hex}|{in_amt}|{min_out}|nonce={req_nonce}"
        );
        verify_sig(&from, &msg, sig_hex)?;
        check_nonce(&self.nonce_watermark, from, req_nonce)?;

        let tx = SigilTx::Swap { from, pool, in_token, in_amt, min_out, fee: 0 };
        Ok(self.queue(tx))
    }

    /// `sigil-rpc/v1|lp_deposit|{from}|{token_a}|{token_b}|{amt_a}|{amt_b}|{fee_bps}|nonce={req_nonce}`
    ///
    /// No `pool` field in the signed message — the pool id is DERIVED from
    /// `(token_a, token_b, fee_bps)` after auth, not supplied by the caller
    /// (see module docs). Returns `(tx_hash, pool_id)` so the caller can show
    /// the wallet exactly which pool it just deposited into.
    pub fn submit_lp_deposit(
        &self,
        from_hex: &str,
        token_a_hex: &str,
        token_b_hex: &str,
        amt_a: u128,
        amt_b: u128,
        fee_bps: u16,
        sig_hex: &str,
        req_nonce: u64,
    ) -> Result<([u8; 32], PoolId), DexError> {
        let from = hex32(from_hex).ok_or(DexError::BadFromAddress)?;
        let token_a: TokenId = hex32(token_a_hex).ok_or(DexError::BadTokenAddress)?;
        let token_b: TokenId = hex32(token_b_hex).ok_or(DexError::BadTokenAddress)?;
        if amt_a == 0 || amt_b == 0 {
            return Err(DexError::ZeroAmount);
        }
        if fee_bps > MAX_FEE_BPS {
            return Err(DexError::FeeTooHigh);
        }

        let msg = format!(
            "sigil-rpc/v1|lp_deposit|{from_hex}|{token_a_hex}|{token_b_hex}|{amt_a}|{amt_b}|{fee_bps}|nonce={req_nonce}"
        );
        verify_sig(&from, &msg, sig_hex)?;
        check_nonce(&self.nonce_watermark, from, req_nonce)?;

        let pool = derive_pool_id(&token_a, &token_b, fee_bps);
        let tx = SigilTx::LpDeposit { from, pool, token_a, token_b, amt_a, amt_b, fee_bps, fee: 0 };
        let hash = self.queue(tx);
        Ok((hash, pool))
    }

    /// `sigil-rpc/v1|lp_withdraw|{from}|{pool}|{shares}|nonce={req_nonce}`
    pub fn submit_lp_withdraw(
        &self,
        from_hex: &str,
        pool_hex: &str,
        shares: u128,
        sig_hex: &str,
        req_nonce: u64,
    ) -> Result<[u8; 32], DexError> {
        let from = hex32(from_hex).ok_or(DexError::BadFromAddress)?;
        let pool: PoolId = hex32(pool_hex).ok_or(DexError::BadPoolAddress)?;
        if shares == 0 {
            return Err(DexError::ZeroAmount);
        }

        let msg = format!("sigil-rpc/v1|lp_withdraw|{from_hex}|{pool_hex}|{shares}|nonce={req_nonce}");
        verify_sig(&from, &msg, sig_hex)?;
        check_nonce(&self.nonce_watermark, from, req_nonce)?;

        let tx = SigilTx::LpWithdraw { from, pool, shares, fee: 0 };
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

    /// Snapshot every still-pending DEX action for the producer's CURRENT
    /// mint attempt — called once per candidate block, non-destructively
    /// (see module docs for why). Entries that have exhausted their retry
    /// budget are dropped here rather than embedded again.
    pub fn snapshot_for_mint(&self) -> Vec<SignedTx> {
        let mut guard = self.pending.lock().unwrap();
        let mut out = Vec::with_capacity(guard.len());
        guard.retain(|hash, p| {
            if p.attempts >= MAX_ATTEMPTS || p.first_seen.elapsed() >= MAX_AGE {
                eprintln!(
                    "\u{2717} dex action gave up after {} attempts / {:.1}s (still not landed) hash={}",
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
    /// carried by a candidate confirmed on the settled spine.
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
    fn a_correctly_signed_swap_is_accepted_and_queued() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let pool_hex = "11".repeat(32);
        let in_token: TokenId = [0xAAu8; 32]; // resolved by the caller, not part of the signed message
        let msg = format!("sigil-rpc/v1|swap|{from_hex}|{pool_hex}|AtoB|1000|900|nonce=1");
        let sig = sign(&sk, &msg);

        let bridge = DexBridge::new();
        let hash = bridge.submit_swap(&from_hex, &pool_hex, "AtoB", in_token, 1000, 900, &sig, 1).unwrap();
        assert_eq!(bridge.pending_len(), 1);

        let snap = bridge.snapshot_for_mint();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].tx.hash(), hash);
        assert!(snap[0].precheck().is_ok(), "placeholder-signed tx must pass precheck");
        match &snap[0].tx {
            SigilTx::Swap { from: f, pool: p, in_token: t, in_amt, min_out, .. } => {
                assert_eq!(*f, from);
                assert_eq!(hex::encode(p), pool_hex);
                assert_eq!(*t, in_token);
                assert_eq!(*in_amt, 1000);
                assert_eq!(*min_out, 900);
            }
            other => panic!("expected Swap, got {other:?}"),
        }
        // NOT destructive.
        assert_eq!(bridge.pending_len(), 1);
    }

    #[test]
    fn a_swap_with_an_invalid_direction_is_rejected() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let pool_hex = "12".repeat(32);
        let msg = format!("sigil-rpc/v1|swap|{from_hex}|{pool_hex}|sideways|10|1|nonce=1");
        let sig = sign(&sk, &msg);
        let bridge = DexBridge::new();
        assert_eq!(
            bridge.submit_swap(&from_hex, &pool_hex, "sideways", [0u8; 32], 10, 1, &sig, 1).unwrap_err(),
            DexError::BadDirection
        );
    }

    #[test]
    fn a_correctly_signed_lp_deposit_derives_a_deterministic_pool_id() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let token_a_hex = "22".repeat(32);
        let token_b_hex = "33".repeat(32);
        let msg = format!(
            "sigil-rpc/v1|lp_deposit|{from_hex}|{token_a_hex}|{token_b_hex}|500|500|30|nonce=1"
        );
        let sig = sign(&sk, &msg);

        let bridge = DexBridge::new();
        let (hash, pool) = bridge
            .submit_lp_deposit(&from_hex, &token_a_hex, &token_b_hex, 500, 500, 30, &sig, 1)
            .unwrap();

        let token_a: TokenId = hex32(&token_a_hex).unwrap();
        let token_b: TokenId = hex32(&token_b_hex).unwrap();
        assert_eq!(pool, derive_pool_id(&token_a, &token_b, 30));

        let snap = bridge.snapshot_for_mint();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].tx.hash(), hash);
        match &snap[0].tx {
            SigilTx::LpDeposit { pool: p, amt_a, amt_b, fee_bps, .. } => {
                assert_eq!(*p, pool);
                assert_eq!(*amt_a, 500);
                assert_eq!(*amt_b, 500);
                assert_eq!(*fee_bps, 30);
            }
            other => panic!("expected LpDeposit, got {other:?}"),
        }
    }

    #[test]
    fn a_correctly_signed_lp_withdraw_is_accepted_and_queued() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let pool_hex = "44".repeat(32);
        let msg = format!("sigil-rpc/v1|lp_withdraw|{from_hex}|{pool_hex}|250|nonce=1");
        let sig = sign(&sk, &msg);

        let bridge = DexBridge::new();
        let hash = bridge.submit_lp_withdraw(&from_hex, &pool_hex, 250, &sig, 1).unwrap();
        let snap = bridge.snapshot_for_mint();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].tx.hash(), hash);
        match &snap[0].tx {
            SigilTx::LpWithdraw { pool: p, shares, .. } => {
                assert_eq!(hex::encode(p), pool_hex);
                assert_eq!(*shares, 250);
            }
            other => panic!("expected LpWithdraw, got {other:?}"),
        }
    }

    #[test]
    fn confirm_applied_retires_only_the_named_hashes() {
        let (sk1, from1) = signer();
        let (sk2, from2) = signer();
        let pool_hex = "55".repeat(32);
        let in_token: TokenId = [0xAAu8; 32];
        let from1_hex = hex::encode(from1);
        let from2_hex = hex::encode(from2);
        let msg1 = format!("sigil-rpc/v1|swap|{from1_hex}|{pool_hex}|AtoB|10|1|nonce=1");
        let msg2 = format!("sigil-rpc/v1|swap|{from2_hex}|{pool_hex}|AtoB|20|1|nonce=1");
        let sig1 = sign(&sk1, &msg1);
        let sig2 = sign(&sk2, &msg2);

        let bridge = DexBridge::new();
        let h1 = bridge.submit_swap(&from1_hex, &pool_hex, "AtoB", in_token, 10, 1, &sig1, 1).unwrap();
        let h2 = bridge.submit_swap(&from2_hex, &pool_hex, "AtoB", in_token, 20, 1, &sig2, 1).unwrap();
        assert_eq!(bridge.pending_len(), 2);

        bridge.confirm_applied(&[h2]);
        assert_eq!(bridge.pending_len(), 1);
        let snap = bridge.snapshot_for_mint();
        assert_eq!(snap[0].tx.hash(), h1, "h1 must still be pending and still offered");
    }

    #[test]
    fn a_tampered_amount_fails_verification() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let pool_hex = "66".repeat(32);
        let in_token: TokenId = [0xAAu8; 32];
        // Signed for in_amt=1000 but submitted as 9_000_000 — the classic
        // "intercept and reamount" attack this signature exists to stop.
        let msg = format!("sigil-rpc/v1|swap|{from_hex}|{pool_hex}|AtoB|1000|1|nonce=1");
        let sig = sign(&sk, &msg);

        let bridge = DexBridge::new();
        let err = bridge.submit_swap(&from_hex, &pool_hex, "AtoB", in_token, 9_000_000, 1, &sig, 1).unwrap_err();
        assert_eq!(err, DexError::SignatureInvalid);
        assert_eq!(bridge.pending_len(), 0);
    }

    #[test]
    fn someone_elses_signature_cannot_authorize_a_swap() {
        let (_attacker_sk, attacker_addr) = signer();
        let (victim_sk, _victim_addr) = signer();
        let attacker_hex = hex::encode(attacker_addr);
        let pool_hex = "77".repeat(32);
        let in_token: TokenId = [0xAAu8; 32];
        let msg = format!("sigil-rpc/v1|swap|{attacker_hex}|{pool_hex}|AtoB|500|1|nonce=1");
        let sig = sign(&victim_sk, &msg); // signed by the WRONG key

        let bridge = DexBridge::new();
        let err = bridge.submit_swap(&attacker_hex, &pool_hex, "AtoB", in_token, 500, 1, &sig, 1).unwrap_err();
        assert_eq!(err, DexError::SignatureInvalid);
    }

    #[test]
    fn a_replayed_nonce_is_rejected_even_with_a_valid_signature() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let pool_hex = "88".repeat(32);
        let in_token: TokenId = [0xAAu8; 32];
        let msg = format!("sigil-rpc/v1|swap|{from_hex}|{pool_hex}|AtoB|10|1|nonce=5");
        let sig = sign(&sk, &msg);

        let bridge = DexBridge::new();
        bridge.submit_swap(&from_hex, &pool_hex, "AtoB", in_token, 10, 1, &sig, 5).unwrap();
        let err = bridge.submit_swap(&from_hex, &pool_hex, "AtoB", in_token, 10, 1, &sig, 5).unwrap_err();
        assert_eq!(err, DexError::ReplayedNonce);
    }

    #[test]
    fn zero_amount_is_rejected_before_touching_the_pending_pool() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let pool_hex = "99".repeat(32);
        let in_token: TokenId = [0xAAu8; 32];
        let msg = format!("sigil-rpc/v1|swap|{from_hex}|{pool_hex}|AtoB|0|0|nonce=1");
        let sig = sign(&sk, &msg);

        let bridge = DexBridge::new();
        assert_eq!(
            bridge.submit_swap(&from_hex, &pool_hex, "AtoB", in_token, 0, 0, &sig, 1).unwrap_err(),
            DexError::ZeroAmount
        );
    }

    #[test]
    fn fee_bps_over_10000_is_rejected_before_touching_the_pending_pool() {
        let (sk, from) = signer();
        let from_hex = hex::encode(from);
        let token_a_hex = "aa".repeat(32);
        let token_b_hex = "bb".repeat(32);
        let msg = format!(
            "sigil-rpc/v1|lp_deposit|{from_hex}|{token_a_hex}|{token_b_hex}|100|100|10001|nonce=1"
        );
        let sig = sign(&sk, &msg);

        let bridge = DexBridge::new();
        assert_eq!(
            bridge
                .submit_lp_deposit(&from_hex, &token_a_hex, &token_b_hex, 100, 100, 10001, &sig, 1)
                .unwrap_err(),
            DexError::FeeTooHigh
        );
    }

    #[test]
    fn malformed_addresses_and_signatures_are_rejected_cleanly() {
        let bridge = DexBridge::new();
        let ok32 = "11".repeat(32);
        let in_token: TokenId = [0xAAu8; 32];
        assert_eq!(
            bridge.submit_swap("not-hex", &ok32, "AtoB", in_token, 1, 1, &"00".repeat(64), 1).unwrap_err(),
            DexError::BadFromAddress
        );
        assert_eq!(
            bridge.submit_swap(&ok32, "short", "AtoB", in_token, 1, 1, &"00".repeat(64), 1).unwrap_err(),
            DexError::BadPoolAddress
        );
        assert_eq!(
            bridge.submit_swap(&ok32, &ok32, "AtoB", in_token, 1, 1, "zz", 1).unwrap_err(),
            DexError::BadSignatureEncoding
        );
    }
}
