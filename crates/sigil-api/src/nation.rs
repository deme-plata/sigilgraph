//! nation.rs — **SIGIL-Nation citizen + welfare endpoints**.
//!
//! The operator's vision (2026-08-31): citizens of the SIGIL nation receive
//! periodic welfare, financed by a carve of the mining dev fee (the SIGIL
//! leg — `sigil_bank::welfare`) and by QUG/QUGUSD on Quillon Graph (the
//! operational leg, off this chain). These endpoints expose the SIGIL leg:
//!
//! - `GET  /v1/nation/status`         — treasury, policy constants, activation
//! - `GET  /v1/nation/citizen`        — one wallet's citizenship + claim window
//! - `POST /v1/nation/attest`         — master-signed `SigilTx::CitizenAttest`
//! - `POST /v1/nation/welfare/claim`  — citizen-signed `SigilTx::WelfareClaim`
//!
//! The two POST routes take a full [`SignedTx`] (same wire shape as
//! `POST /v1/transactions`) but add a variant check and a **dry-run apply at
//! the current tip height**, so a wallet gets "welfare cooldown: next claim
//! at height N" instead of a silently-dropped mempool entry. Consensus
//! enforcement lives in `sigil_tx::apply_tx_at` — these handlers refuse
//! early with the same errors the producer would raise, never instead of it.

use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use sigil_bank::welfare as wf;
use sigil_state::NATIVE;
use sigil_tx::{apply_tx_at, SigilTx, SignedTx};

use crate::{hex32, ApiResponse, AppState, SubmitResponse};

/// Policy + treasury snapshot for the nation dashboard.
#[derive(Debug, Serialize)]
pub struct NationStatusResponse {
    /// Is the nation-welfare feature active at the current tip?
    pub active: bool,
    /// Height the feature activates (consensus constant).
    pub activation_height: u64,
    /// Current tip height as this node sees it (0 if no tip yet).
    pub height: u64,
    /// Welfare treasury balance, in glyphs (10-decimal base units), as a string.
    pub treasury_glyphs: String,
    /// Welfare treasury wallet, 64-hex.
    pub treasury_wallet: String,
    /// Stipend per claim, in glyphs, as a string. ⚠️ Legacy field kept for
    /// UI compatibility — since the sUSD ruling the stipend is DENOMINATED
    /// in USDS (`stipend_usd_e8`); this glyph constant is only the fee
    /// ceiling on a claim.
    pub stipend_glyphs: String,
    /// What claims actually pay out: `"USDS"` (operator ruling 2026-08-31 —
    /// certainty over volatility).
    pub payout_asset: &'static str,
    /// Stipend per claim in USDS base units (1e8 == $1.00), as a string.
    pub stipend_usd_e8: String,
    /// Current oracle price (USD×1e8 per SIGIL), as a string. `"0"` means
    /// the oracle is unfed and every claim will refuse (fail closed) until
    /// the authority pushes a price (`SigilTx::OraclePush` /
    /// `POST /v1/nation/oracle/push_wallet`).
    pub oracle_price_usd_e8: String,
    /// Minimum blocks between two claims by the same citizen.
    pub claim_interval_blocks: u64,
    /// Mining-reward welfare carve in basis points (taken out of the dev fee).
    pub welfare_bps: u64,
    /// The nation authority (the chain's master wallet), 64-hex — the only
    /// wallet whose `CitizenAttest` consensus accepts. Empty if the chain
    /// has no master committed. Lets the wallet UI show the attest panel
    /// only to the authority.
    pub authority_wallet: String,
    /// Where the money comes from, for humans.
    pub financed_by: &'static str,
}

#[flux_api_macros::api(GET, "/v1/nation/status", summary = "SIGIL-Nation welfare treasury + policy status")]
pub async fn nation_status(State(st): State<AppState>) -> Json<ApiResponse<NationStatusResponse>> {
    let height = st.mining.tip().map(|t| t.height).unwrap_or(0);
    let (treasury, authority, oracle_price) = st
        .state
        .read()
        .map(|s| {
            (
                s.balance_of(&wf::WELFARE_WALLET, &NATIVE),
                s.master_wallet(),
                sigil_oracle::read_price(&s),
            )
        })
        .unwrap_or((0, None, 0));
    ApiResponse::ok(NationStatusResponse {
        active: wf::welfare_active(height),
        activation_height: wf::WELFARE_FROM_HEIGHT,
        height,
        treasury_glyphs: treasury.to_string(),
        treasury_wallet: hex::encode(wf::WELFARE_WALLET),
        stipend_glyphs: wf::WELFARE_STIPEND_GLYPHS.to_string(),
        payout_asset: "USDS",
        stipend_usd_e8: wf::WELFARE_STIPEND_USD_E8.to_string(),
        oracle_price_usd_e8: oracle_price.to_string(),
        claim_interval_blocks: wf::WELFARE_CLAIM_INTERVAL_BLOCKS,
        welfare_bps: wf::WELFARE_MINING_FEE_BPS as u64,
        authority_wallet: authority.map(hex::encode).unwrap_or_default(),
        financed_by: "mining dev-fee carve (200 of the 750-bps dev fee; master nets 550), paid out as USDS; QUG/QUGUSD welfare runs on Quillon Graph",
    })
}

/// Query for `GET /v1/nation/citizen`.
#[derive(Debug, Deserialize)]
pub struct CitizenQuery {
    /// 64-hex wallet to look up.
    pub wallet: String,
}

/// One wallet's citizenship + welfare-claim window.
#[derive(Debug, Serialize)]
pub struct CitizenResponse {
    pub wallet: String,
    /// Attested in the borger registry?
    pub citizen: bool,
    /// Height of the last welfare claim (0 = never claimed).
    pub last_claim_height: u64,
    /// First height the next claim is allowed.
    pub next_claim_height: u64,
    /// Would a claim submitted right now succeed? (active + eligible +
    /// treasury covers the stipend)
    pub claimable_now: bool,
}

#[flux_api_macros::api(GET, "/v1/nation/citizen", summary = "Citizenship + welfare-claim window for one wallet")]
pub async fn nation_citizen(
    State(st): State<AppState>,
    Query(q): Query<CitizenQuery>,
) -> Json<ApiResponse<CitizenResponse>> {
    let Some(wallet) = hex32(&q.wallet) else {
        return ApiResponse::err("wallet must be 64-hex");
    };
    let height = st.mining.tip().map(|t| t.height).unwrap_or(0);
    let Ok(s) = st.state.read() else {
        return ApiResponse::err("state lock poisoned");
    };
    let citizen = s.contract_slot(&wf::BORGER_REGISTRY, &wallet) != [0u8; 32];
    let last = wf::decode_claim_height(&s.contract_slot(&wf::WELFARE_LEDGER, &wallet));
    let treasury = s.balance_of(&wf::WELFARE_WALLET, &NATIVE);
    drop(s);
    ApiResponse::ok(CitizenResponse {
        wallet: q.wallet,
        citizen,
        last_claim_height: last,
        next_claim_height: wf::next_claim_height(last),
        claimable_now: citizen
            && wf::claim_eligible(last, height)
            && treasury >= wf::WELFARE_STIPEND_GLYPHS,
    })
}

/// Shared submit path for the two nation POST routes: variant check →
/// signature verify → dry-run apply at tip height (friendly errors) →
/// mempool ingest. The dry run uses the SAME `apply_tx_at` the producer
/// uses, so an accepted tx cannot be refused later for a reason this
/// endpoint didn't already surface (barring races on the claim window).
fn submit_nation_tx(
    st: &AppState,
    tx: SignedTx,
    want: &'static str,
    matches_variant: bool,
) -> Json<ApiResponse<SubmitResponse>> {
    if !matches_variant {
        return ApiResponse::err(format!("this route only accepts a {want} transaction"));
    }
    if let Err(e) = tx.verify_signature() {
        return ApiResponse::err(format!("signature invalid: {e:?}"));
    }
    let height = st.mining.tip().map(|t| t.height).unwrap_or(0);
    {
        let Ok(s) = st.state.read() else {
            return ApiResponse::err("state lock poisoned");
        };
        if let Err(e) = apply_tx_at(&s, &tx, height) {
            return ApiResponse::err(format!("{e}"));
        }
    }
    let tx_hash = hex::encode(tx.tx.hash());
    let accepted = st.mempool.ingest(vec![tx]).accepted > 0;
    ApiResponse::ok(SubmitResponse {
        tx_hash,
        accepted,
        note: if accepted {
            "queued for the next braid block".into()
        } else {
            "rejected at mempool ingest (dup / nonce / precheck)".into()
        },
    })
}

#[flux_api_macros::api(POST, "/v1/nation/attest", summary = "Master-signed: attest a wallet as a SIGIL-Nation citizen")]
pub async fn nation_attest(
    State(st): State<AppState>,
    Json(tx): Json<SignedTx>,
) -> Json<ApiResponse<SubmitResponse>> {
    let ok = matches!(tx.tx, SigilTx::CitizenAttest { .. });
    submit_nation_tx(&st, tx, "CitizenAttest", ok)
}

#[flux_api_macros::api(POST, "/v1/nation/welfare/claim", summary = "Citizen-signed: claim the periodic welfare stipend")]
pub async fn nation_welfare_claim(
    State(st): State<AppState>,
    Json(tx): Json<SignedTx>,
) -> Json<ApiResponse<SubmitResponse>> {
    let ok = matches!(tx.tx, SigilTx::WelfareClaim { .. });
    submit_nation_tx(&st, tx, "WelfareClaim", ok)
}

// ── NationBridge: the wallet-friendly claim/attest queue ────────────────────
//
// The web wallet cannot build a consensus `SignedTx` (BLAKE3 account binding
// + byte-exact serde encoding), so — exactly like send/shield/dex/usds — it
// signs the canonical RPC message `sigil-rpc/v1|{action}|{fields}|nonce={n}`
// with its Ed25519 key (wallet address == the raw pubkey), the bridge
// authenticates HERE, and the producer embeds a placeholder-signature
// `SignedTx` (apply_tx only prechecks; real auth already happened). Same
// non-destructive snapshot/confirm contract as `SendBridge` — see its docs.

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sigil_state::WalletId;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_ATTEMPTS: u32 = 30_000;
const MAX_AGE: Duration = Duration::from_secs(2_400);

struct NationPending {
    tx: SigilTx,
    attempts: u32,
    first_seen: Instant,
}

/// Why a nation submission was rejected before reaching the pending pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NationSubmitError {
    BadAddress,
    BadCprHash,
    BadSignatureEncoding,
    SignatureInvalid,
    ReplayedNonce,
}

impl NationSubmitError {
    /// Human-usable message for the flat wallet JSON responses.
    pub fn message(self) -> &'static str {
        match self {
            NationSubmitError::BadAddress => "address must be a 64-hex wallet",
            NationSubmitError::BadCprHash => "cpr_hash must be 64 hex chars and non-zero",
            NationSubmitError::BadSignatureEncoding => "sig must be 128 hex chars (64 bytes)",
            NationSubmitError::SignatureInvalid => "signature does not match the signing wallet",
            NationSubmitError::ReplayedNonce => "req_nonce must be greater than the last accepted nonce for this wallet",
        }
    }
}

/// Wallet-authenticated queue for `WelfareClaim` and `CitizenAttest`.
pub struct NationBridge {
    pending: Mutex<HashMap<[u8; 32], NationPending>>,
    nonce_watermark: Mutex<HashMap<WalletId, u64>>,
}

impl Default for NationBridge {
    fn default() -> Self {
        Self { pending: Mutex::new(HashMap::new()), nonce_watermark: Mutex::new(HashMap::new()) }
    }
}

impl NationBridge {
    pub fn new() -> Self { Self::default() }

    fn verify_and_watermark(
        &self,
        signer: &WalletId,
        msg: &str,
        sig_hex: &str,
        req_nonce: u64,
    ) -> Result<(), NationSubmitError> {
        let sig_hex = sig_hex.strip_prefix("0x").unwrap_or(sig_hex);
        let sig_bytes = hex::decode(sig_hex).map_err(|_| NationSubmitError::BadSignatureEncoding)?;
        let sig_arr: [u8; 64] =
            sig_bytes.try_into().map_err(|_| NationSubmitError::BadSignatureEncoding)?;
        let vk = VerifyingKey::from_bytes(signer).map_err(|_| NationSubmitError::SignatureInvalid)?;
        vk.verify(msg.as_bytes(), &Signature::from_bytes(&sig_arr))
            .map_err(|_| NationSubmitError::SignatureInvalid)?;
        let mut wm = self.nonce_watermark.lock().unwrap();
        let last = wm.get(signer).copied().unwrap_or(0);
        if req_nonce <= last {
            return Err(NationSubmitError::ReplayedNonce);
        }
        wm.insert(*signer, req_nonce);
        Ok(())
    }

    fn queue(&self, tx: SigilTx) -> [u8; 32] {
        let tx_hash = tx.hash();
        self.pending.lock().unwrap().entry(tx_hash).or_insert_with(|| NationPending {
            tx,
            attempts: 0,
            first_seen: Instant::now(),
        });
        tx_hash
    }

    /// Authenticate + queue a citizen's welfare claim. The wallet signs
    /// exactly `sigilSign(priv,'welfare_claim',[wallet,fee],reqNonce)`:
    /// `sigil-rpc/v1|welfare_claim|{wallet}|{fee}|nonce={req_nonce}` —
    /// `wallet_hex` reused VERBATIM in the message (see `SendBridge::submit`
    /// for why casing must not be re-encoded).
    pub fn submit_claim(
        &self,
        wallet_hex: &str,
        fee: u128,
        sig_hex: &str,
        req_nonce: u64,
    ) -> Result<[u8; 32], NationSubmitError> {
        let citizen = crate::hex32(wallet_hex).ok_or(NationSubmitError::BadAddress)?;
        let msg = format!("sigil-rpc/v1|welfare_claim|{wallet_hex}|{fee}|nonce={req_nonce}");
        self.verify_and_watermark(&citizen, &msg, sig_hex, req_nonce)?;
        Ok(self.queue(SigilTx::WelfareClaim { citizen, fee }))
    }

    /// Authenticate + queue a citizen attestation, signed by the nation
    /// authority's wallet (consensus additionally enforces authority ==
    /// master wallet at apply). Message:
    /// `sigil-rpc/v1|citizen_attest|{authority}|{citizen}|{cpr_hash}|{fee}|nonce={req_nonce}`.
    pub fn submit_attest(
        &self,
        authority_hex: &str,
        citizen_hex: &str,
        cpr_hash_hex: &str,
        fee: u128,
        sig_hex: &str,
        req_nonce: u64,
    ) -> Result<[u8; 32], NationSubmitError> {
        let authority = crate::hex32(authority_hex).ok_or(NationSubmitError::BadAddress)?;
        let citizen = crate::hex32(citizen_hex).ok_or(NationSubmitError::BadAddress)?;
        let cpr_hash = crate::hex32(cpr_hash_hex).ok_or(NationSubmitError::BadCprHash)?;
        if cpr_hash == [0u8; 32] {
            return Err(NationSubmitError::BadCprHash);
        }
        let msg = format!(
            "sigil-rpc/v1|citizen_attest|{authority_hex}|{citizen_hex}|{cpr_hash_hex}|{fee}|nonce={req_nonce}"
        );
        self.verify_and_watermark(&authority, &msg, sig_hex, req_nonce)?;
        Ok(self.queue(SigilTx::CitizenAttest { authority, citizen, cpr_hash, fee }))
    }

    /// Authenticate + queue an oracle price push, signed by the nation
    /// authority's wallet (consensus additionally enforces authority ==
    /// master wallet at apply). Message:
    /// `sigil-rpc/v1|oracle_push|{authority}|{price_usd_e8}|{fee}|nonce={req_nonce}`.
    pub fn submit_oracle_push(
        &self,
        authority_hex: &str,
        price_usd_e8: u128,
        fee: u128,
        sig_hex: &str,
        req_nonce: u64,
    ) -> Result<[u8; 32], NationSubmitError> {
        let authority = crate::hex32(authority_hex).ok_or(NationSubmitError::BadAddress)?;
        let msg = format!(
            "sigil-rpc/v1|oracle_push|{authority_hex}|{price_usd_e8}|{fee}|nonce={req_nonce}"
        );
        self.verify_and_watermark(&authority, &msg, sig_hex, req_nonce)?;
        Ok(self.queue(SigilTx::OraclePush { authority, price_usd_e8, fee }))
    }

    /// Snapshot every still-pending nation tx for the producer's CURRENT
    /// mint attempt — non-destructive, same contract as
    /// `SendBridge::snapshot_for_mint` (see its docs for why).
    pub fn snapshot_for_mint(&self) -> Vec<SignedTx> {
        let mut guard = self.pending.lock().unwrap();
        let mut out = Vec::with_capacity(guard.len());
        guard.retain(|hash, p| {
            if p.attempts >= MAX_ATTEMPTS || p.first_seen.elapsed() >= MAX_AGE {
                eprintln!(
                    "✗ nation tx gave up after {} attempts / {:.1}s (likely refused at apply — \
                     not a citizen, cooldown, or treasury underfunded) hash={}",
                    p.attempts, p.first_seen.elapsed().as_secs_f64(), hex::encode(hash)
                );
                return false;
            }
            p.attempts += 1;
            out.push(crate::send::to_signed(p.tx.clone()));
            true
        });
        out
    }

    /// Retire hashes carried by a candidate confirmed on the settled spine.
    pub fn confirm_applied(&self, hashes: &[[u8; 32]]) {
        let mut guard = self.pending.lock().unwrap();
        for h in hashes {
            guard.remove(h);
        }
    }

    /// Is this tx hash still pending in the bridge?
    pub fn contains(&self, hash: &[u8; 32]) -> bool {
        self.pending.lock().unwrap().contains_key(hash)
    }
}

/// Body for `POST /v1/nation/welfare/claim_wallet` — the wallet's flat
/// signed request (same family as `SendRequest`).
#[derive(Debug, Deserialize)]
pub struct WalletClaimRequest {
    /// 64-hex citizen wallet (== raw Ed25519 pubkey).
    pub wallet: String,
    /// Fee in glyphs, deducted from the stipend. The wallet sends 0.
    /// u64 on the wire (fits any sane fee) — serde_json's derived u128 is a
    /// known trap in this workspace, see sigil-tx's `u128_str` note.
    #[serde(default)]
    pub fee: u64,
    /// 128-hex Ed25519 signature over the canonical RPC message.
    pub sig: String,
    /// Client-chosen strictly-increasing nonce (the wallet uses `Date.now()`).
    pub req_nonce: u64,
}

/// Body for `POST /v1/nation/attest_wallet`.
#[derive(Debug, Deserialize)]
pub struct WalletAttestRequest {
    /// 64-hex authority wallet — must be the chain's master wallet.
    pub authority: String,
    /// 64-hex wallet being attested as a citizen.
    pub citizen: String,
    /// 64-hex non-zero hash of the citizen's civil identity.
    pub cpr_hash: String,
    /// Fee in glyphs, paid by the authority. The wallet sends 0. u64 on the
    /// wire for the same reason as `WalletClaimRequest::fee`.
    #[serde(default)]
    pub fee: u64,
    /// 128-hex Ed25519 signature over the canonical RPC message.
    pub sig: String,
    /// Client-chosen strictly-increasing nonce.
    pub req_nonce: u64,
}

/// Dry-run a nation tx against live state at tip height so the wallet gets
/// the real refusal reason ("cooldown until height N") instead of a queued
/// tx that silently never lands. Returns None when it would apply.
fn dry_run_reason(st: &AppState, tx: &SigilTx) -> Option<String> {
    let height = st.mining.tip().map(|t| t.height).unwrap_or(0);
    let s = st.state.read().ok()?;
    apply_tx_at(&s, &crate::send::to_signed(tx.clone()), height)
        .err()
        .map(|e| format!("{e}"))
}

#[flux_api_macros::api(POST, "/v1/nation/welfare/claim_wallet", summary = "Wallet-signed: claim the welfare stipend (flat JSON, sigil-rpc/v1 message)")]
pub async fn nation_claim_wallet(
    State(st): State<AppState>,
    Json(req): Json<WalletClaimRequest>,
) -> Json<serde_json::Value> {
    let Some(citizen) = hex32(&req.wallet) else {
        return Json(serde_json::json!({ "ok": false, "error": "wallet must be 64-hex" }));
    };
    if let Some(reason) = dry_run_reason(&st, &SigilTx::WelfareClaim { citizen, fee: req.fee as u128 }) {
        return Json(serde_json::json!({ "ok": false, "error": reason }));
    }
    match st.nation.submit_claim(&req.wallet, req.fee as u128, &req.sig, req.req_nonce) {
        Ok(tx_hash) => Json(serde_json::json!({
            "ok": true,
            "txid": hex::encode(tx_hash),
            "note": "queued for the next braid block",
        })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.message() })),
    }
}

#[flux_api_macros::api(POST, "/v1/nation/attest_wallet", summary = "Master-wallet-signed: attest a citizen (flat JSON, sigil-rpc/v1 message)")]
pub async fn nation_attest_wallet(
    State(st): State<AppState>,
    Json(req): Json<WalletAttestRequest>,
) -> Json<serde_json::Value> {
    let (Some(authority), Some(citizen), Some(cpr_hash)) =
        (hex32(&req.authority), hex32(&req.citizen), hex32(&req.cpr_hash))
    else {
        return Json(serde_json::json!({ "ok": false, "error": "authority/citizen/cpr_hash must be 64-hex" }));
    };
    let tx = SigilTx::CitizenAttest { authority, citizen, cpr_hash, fee: req.fee as u128 };
    if let Some(reason) = dry_run_reason(&st, &tx) {
        return Json(serde_json::json!({ "ok": false, "error": reason }));
    }
    match st.nation.submit_attest(&req.authority, &req.citizen, &req.cpr_hash, req.fee as u128, &req.sig, req.req_nonce) {
        Ok(tx_hash) => Json(serde_json::json!({
            "ok": true,
            "txid": hex::encode(tx_hash),
            "note": "queued for the next braid block",
        })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.message() })),
    }
}

/// Body for `POST /v1/nation/oracle/push_wallet`.
#[derive(Debug, Deserialize)]
pub struct WalletOraclePushRequest {
    /// 64-hex authority wallet — must be the chain's master wallet.
    pub authority: String,
    /// Price in USD×1e8 per whole SIGIL (u64 on the wire — see
    /// `WalletClaimRequest::fee` for the u128 serde trap). $184B/SIGIL of
    /// headroom is enough.
    pub price_usd_e8: u64,
    /// Fee in glyphs, paid by the authority. The wallet sends 0.
    #[serde(default)]
    pub fee: u64,
    /// 128-hex Ed25519 signature over the canonical RPC message.
    pub sig: String,
    /// Client-chosen strictly-increasing nonce.
    pub req_nonce: u64,
}

#[flux_api_macros::api(POST, "/v1/nation/oracle/push_wallet", summary = "Master-wallet-signed: push the SIGIL/USD oracle price the sUSD welfare payout mints at")]
pub async fn nation_oracle_push_wallet(
    State(st): State<AppState>,
    Json(req): Json<WalletOraclePushRequest>,
) -> Json<serde_json::Value> {
    let Some(authority) = hex32(&req.authority) else {
        return Json(serde_json::json!({ "ok": false, "error": "authority must be 64-hex" }));
    };
    let tx = SigilTx::OraclePush {
        authority,
        price_usd_e8: req.price_usd_e8 as u128,
        fee: req.fee as u128,
    };
    if let Some(reason) = dry_run_reason(&st, &tx) {
        return Json(serde_json::json!({ "ok": false, "error": reason }));
    }
    match st.nation.submit_oracle_push(&req.authority, req.price_usd_e8 as u128, req.fee as u128, &req.sig, req.req_nonce) {
        Ok(tx_hash) => Json(serde_json::json!({
            "ok": true,
            "txid": hex::encode(tx_hash),
            "note": "queued for the next braid block",
        })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.message() })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_state::{commit_state_transition, SigilState, StateMutation, StateTransition};
    use std::sync::{Arc, RwLock};

    fn app_with_state(s: SigilState) -> AppState {
        AppState::new(
            Arc::new(sigil_narwhal_mempool::MempoolBackend::legacy()),
            Arc::new(RwLock::new(s)),
        )
    }

    #[tokio::test]
    async fn status_reports_policy_and_treasury() {
        let mut s = SigilState::new();
        commit_state_transition(&mut s, &StateTransition { at_height: 0, mutations: vec![
            StateMutation::SetBalance { wallet: wf::WELFARE_WALLET, token: NATIVE, amount: 123 },
        ] }, 0).unwrap();
        let st = app_with_state(s);
        let resp = nation_status(State(st)).await;
        let d = resp.0.data.unwrap();
        assert_eq!(d.treasury_glyphs, "123");
        assert_eq!(d.activation_height, wf::WELFARE_FROM_HEIGHT);
        assert_eq!(d.welfare_bps, wf::WELFARE_MINING_FEE_BPS as u64);
        assert!(!d.active, "no tip yet → height 0 → inactive");
    }

    #[tokio::test]
    async fn citizen_lookup_distinguishes_attested_wallets() {
        let alice = [0x11u8; 32];
        let mut s = SigilState::new();
        commit_state_transition(&mut s, &StateTransition { at_height: 0, mutations: vec![
            StateMutation::SetContractSlot { contract: wf::BORGER_REGISTRY, slot: alice, value: [0x42; 32] },
        ] }, 0).unwrap();
        let st = app_with_state(s);
        let r = nation_citizen(State(st.clone()), Query(CitizenQuery { wallet: hex::encode(alice) })).await;
        let d = r.0.data.unwrap();
        assert!(d.citizen);
        assert_eq!(d.last_claim_height, 0);
        assert_eq!(d.next_claim_height, wf::WELFARE_FROM_HEIGHT);
        let r2 = nation_citizen(State(st), Query(CitizenQuery { wallet: hex::encode([0x22u8; 32]) })).await;
        assert!(!r2.0.data.unwrap().citizen);
    }
}
