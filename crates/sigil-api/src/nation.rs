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
    /// Stipend per claim, in glyphs, as a string.
    pub stipend_glyphs: String,
    /// Minimum blocks between two claims by the same citizen.
    pub claim_interval_blocks: u64,
    /// Mining-reward welfare carve in basis points (taken out of the dev fee).
    pub welfare_bps: u64,
    /// Where the money comes from, for humans.
    pub financed_by: &'static str,
}

#[flux_api_macros::api(GET, "/v1/nation/status", summary = "SIGIL-Nation welfare treasury + policy status")]
pub async fn nation_status(State(st): State<AppState>) -> Json<ApiResponse<NationStatusResponse>> {
    let height = st.mining.tip().map(|t| t.height).unwrap_or(0);
    let treasury = st
        .state
        .read()
        .map(|s| s.balance_of(&wf::WELFARE_WALLET, &NATIVE))
        .unwrap_or(0);
    ApiResponse::ok(NationStatusResponse {
        active: wf::welfare_active(height),
        activation_height: wf::WELFARE_FROM_HEIGHT,
        height,
        treasury_glyphs: treasury.to_string(),
        treasury_wallet: hex::encode(wf::WELFARE_WALLET),
        stipend_glyphs: wf::WELFARE_STIPEND_GLYPHS.to_string(),
        claim_interval_blocks: wf::WELFARE_CLAIM_INTERVAL_BLOCKS,
        welfare_bps: wf::WELFARE_MINING_FEE_BPS as u64,
        financed_by: "mining dev-fee carve (200 of the master's 500 bps); QUG/QUGUSD welfare runs on Quillon Graph",
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
