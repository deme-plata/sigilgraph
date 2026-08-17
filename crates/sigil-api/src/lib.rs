//! sigil-api — the braid node's money API.
//!
//! A proper axum surface (the Quillon `q-api-server` pattern), annotated with
//! flux-api's `#[api]` so an OpenAPI 3.1 spec + typed TS/Python/Rust/Go SDKs
//! generate for free — the thing plain axum lacks. Replaces the hand-rolled
//! `sigil-rpcd` HTTP once it serves the full money surface.
//!
//! Design borrowed from Quillon (file:line refs in the port notes):
//!   * uniform `ApiResponse<T> { ok, data, error, ts }` envelope,
//!   * intrinsic auth — a `SignedTx` carries its own ed25519 signature, verified
//!     on every node (`verify_signature`), no server-trust bypass (Quillon's
//!     `send_signed` model, which suits sigil-tx's already-client-signed txs),
//!   * tower middleware tuned for money: concurrency cap, timeout, CORS, body limit,
//!   * reads (`balance`, `supply`) are open; mutations require a valid signature.

use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    extract::{Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use sigil_narwhal_mempool::MempoolBackend;
use sigil_state::{SigilState, WalletId, MAX_SUPPLY, NATIVE};
use sigil_tx::SignedTx;

pub mod mining;
use mining::{MiningBridge, SubmitOutcome};

pub mod send;
use send::SendBridge;

pub mod eth;

/// Shared braid state the API reads/writes. `state` is published by the producer
/// after each block-apply (a consistent read snapshot); `mempool` is the SAME
/// `Arc<MempoolBackend>` the producer pulls txs from when it mints the next
/// block — not a coincidence of construction, but the whole point of
/// `MempoolBackend` existing (SIGIL_BRAIDPOOL_v1_1.md Phase B): both crates
/// holding the SAME handle makes it structurally impossible for a transaction
/// accepted here to land somewhere the producer never looks. `mining` is the
/// seam the producer publishes its frontier into and pops verified solves from.
#[derive(Clone)]
pub struct AppState {
    pub mempool: Arc<MempoolBackend>,
    pub state: Arc<RwLock<SigilState>>,
    pub mining: Arc<MiningBridge>,
    /// The wallet-authenticated send queue — see `send` module docs. The
    /// producer drains it once per tick, same shape as `mining`.
    pub send: Arc<SendBridge>,
}

impl AppState {
    /// Build the shared state from the producer's mempool + state handles, with
    /// a fresh mining bridge.
    pub fn new(mempool: Arc<MempoolBackend>, state: Arc<RwLock<SigilState>>) -> Self {
        Self { mempool, state, mining: Arc::new(MiningBridge::new()), send: Arc::new(SendBridge::new()) }
    }
}

/// Uniform response envelope (Quillon `ApiResponse<T>`): every endpoint returns
/// this shape, timestamped. Application failures come back with `ok:false` +
/// `error` (HTTP 200); only malformed input uses a real 4xx.
#[derive(Debug, Serialize, Deserialize)]
pub struct ApiResponse<T> {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub ts: u64,
}

impl<T> ApiResponse<T> {
    pub fn ok(data: T) -> Json<Self> {
        Json(Self { ok: true, data: Some(data), error: None, ts: now_ms() })
    }
    pub fn err(msg: impl Into<String>) -> Json<Self> {
        Json(Self { ok: false, data: None, error: Some(msg.into()), ts: now_ms() })
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub(crate) fn hex32(s: &str) -> Option<WalletId> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if s.len() != 64 { return None; }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

// ── request/response bodies (serde; flux-api derives their OpenAPI schema) ──
#[derive(Debug, Deserialize)]
pub struct BalanceQuery {
    /// 64-hex wallet address.
    pub wallet: String,
    /// Optional 64-hex token id; default native SIGIL.
    pub token: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BalanceResponse {
    pub wallet: String,
    pub token: String,
    /// Balance in base units (8 decimals).
    pub balance: String,
}

#[derive(Debug, Serialize)]
pub struct SupplyResponse {
    pub native_supply: String,
    pub max_supply: String,
    pub minted_pct: f64,
}

#[derive(Debug, Serialize)]
pub struct SubmitResponse {
    pub tx_hash: String,
    pub accepted: bool,
    pub note: String,
}

/// Body for `POST /v1/send` — the wallet's lightweight signed-send request
/// (`gui/sigil-wallet-tron-embedded.html`'s `doSend`). NOT a `SignedTx`: the
/// wallet signs a stable RPC message (see `send` module docs), not
/// `SigilTx::hash()`, so this is its own shape, verified by `SendBridge`.
#[derive(Debug, Deserialize)]
pub struct SendRequest {
    /// 64-hex sender address (== the sender's raw Ed25519 pubkey bytes).
    pub from: String,
    /// 64-hex recipient address.
    pub to: String,
    /// Amount in base units (8 decimals).
    pub amount: u128,
    /// Must be `"SIGIL"` — only the native token is accepted on this route.
    pub token: String,
    /// 128-hex Ed25519 signature over the canonical RPC message.
    pub sig: String,
    /// Client-chosen strictly-increasing nonce (the wallet uses `Date.now()`).
    pub req_nonce: u64,
}

#[derive(Debug, Serialize)]
pub struct TxStatusResponse {
    pub tx_hash: String,
    /// "mempool" (waiting) | "unknown" (not seen). "applied" arrives when block
    /// indexing lands (follow-on).
    pub status: String,
}

// ── handlers (annotated → OpenAPI + SDKs) ───────────────────────────────────

#[flux_api_macros::api(GET, "/v1/health", summary = "Liveness probe")]
pub async fn health() -> Json<ApiResponse<&'static str>> {
    ApiResponse::ok("sigil-api")
}

#[flux_api_macros::api(GET, "/v1/balance", summary = "Get a wallet's balance (base units)")]
pub async fn balance(
    State(st): State<AppState>,
    Query(q): Query<BalanceQuery>,
) -> Json<ApiResponse<BalanceResponse>> {
    let Some(wallet) = hex32(&q.wallet) else {
        return ApiResponse::err("wallet must be 64-hex");
    };
    let token = match q.token.as_deref() {
        None => NATIVE,
        Some(t) => match hex32(t) {
            Some(tk) => tk,
            None => return ApiResponse::err("token must be 64-hex"),
        },
    };
    let bal = st.state.read().map(|s| s.balance_of(&wallet, &token)).unwrap_or(0);
    ApiResponse::ok(BalanceResponse {
        wallet: hex::encode(wallet),
        token: hex::encode(token),
        balance: bal.to_string(),
    })
}

#[flux_api_macros::api(GET, "/v1/supply", summary = "Native supply + 21M cap progress")]
pub async fn supply(State(st): State<AppState>) -> Json<ApiResponse<SupplyResponse>> {
    let minted = st.state.read().map(|s| s.native_supply()).unwrap_or(0);
    let pct = (minted as f64) * 100.0 / (MAX_SUPPLY as f64);
    ApiResponse::ok(SupplyResponse {
        native_supply: minted.to_string(),
        max_supply: MAX_SUPPLY.to_string(),
        minted_pct: pct,
    })
}

#[flux_api_macros::api(POST, "/v1/transactions", summary = "Submit a signed transaction into the braid mempool")]
pub async fn submit_transaction(
    State(st): State<AppState>,
    Json(tx): Json<SignedTx>,
) -> Json<ApiResponse<SubmitResponse>> {
    let tx_hash = hex::encode(tx.tx.hash());
    // Intrinsic auth: the tx carries its own signature; verify on THIS node —
    // no trust bypass (Quillon's send_signed model).
    if let Err(e) = tx.verify_signature() {
        return ApiResponse::err(format!("signature invalid: {e:?}"));
    }
    // Ingest into the shared mempool → the producer pulls it into the next block.
    // `MempoolBackend::ingest` handles its own internal locking (and dispatches
    // to whichever tx backend SIGIL_BRAIDPOOL selected) — no Mutex to poison
    // handle here at this call site.
    let accepted = st.mempool.ingest(vec![tx]).accepted > 0;
    ApiResponse::ok(SubmitResponse {
        tx_hash,
        accepted,
        note: if accepted { "queued for the next braid block".into() }
              else { "rejected at mempool ingest (dup / nonce / precheck)".into() },
    })
}

#[flux_api_macros::api(GET, "/v1/transactions/:hash", summary = "Transaction status")]
pub async fn tx_status(
    State(st): State<AppState>,
    axum::extract::Path(hash): axum::extract::Path<String>,
) -> Json<ApiResponse<TxStatusResponse>> {
    let Some(h) = hex32(&hash) else {
        return ApiResponse::err("hash must be 64-hex");
    };
    let in_pool = st.mempool.contains(&h);
    ApiResponse::ok(TxStatusResponse {
        tx_hash: hash,
        status: if in_pool { "mempool".into() } else { "unknown".into() },
    })
}

/// The wallet-friendly signed-send endpoint. Deliberately returns a FLAT
/// JSON body (`{ok,txid,height,ts_ms}` / `{ok,error}`), not the generic
/// `ApiResponse<T>` envelope (whose payload nests under `.data`) — the
/// wallet's `doSend`/`showReceipt` read `j.txid`/`j.height`/`j.ts_ms`/
/// `j.error` directly off the top-level response object.
///
/// Authentication + queueing happen in `SendBridge::submit` (see its docs
/// for why this isn't routed through `Mempool::ingest`/`SignedTx::
/// verify_signature`). Height is always `null` here: this handler returns
/// the instant a send is authenticated and queued, before the producer has
/// minted the block that includes it — same "accepted, not yet final"
/// semantics `submit_transaction`'s mempool-ingest already has.
#[flux_api_macros::api(POST, "/v1/send", summary = "Wallet-signed native SIGIL send (verify + queue for the next block)")]
pub async fn send_handler(
    State(st): State<AppState>,
    Json(req): Json<SendRequest>,
) -> Json<serde_json::Value> {
    match st.send.submit(&req.from, &req.to, req.amount, &req.token, &req.sig, req.req_nonce) {
        Ok(tx_hash) => Json(serde_json::json!({
            "ok": true,
            "txid": hex::encode(tx_hash),
            "height": null,
            "ts_ms": now_ms(),
            "note": "queued for the next braid block",
        })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.message() })),
    }
}

/// `?address=0x...` — a 20-byte hex EVM address, NOT a SIGIL wallet address.
/// Different curve, different derivation; there is no relationship between
/// the two, so this is a separate address a caller pastes in (e.g. their
/// MetaMask address), not anything derivable from a SIGIL keypair.
/// `?chain=ethereum|polygon` — optional, defaults to `ethereum`.
#[derive(Debug, Deserialize)]
pub struct UsdcBalanceQuery {
    pub address: String,
    pub chain: Option<String>,
}

/// Read-only USDC balance lookup, on Ethereum mainnet or Polygon PoS — see
/// `eth` module docs for the "why a public RPC, not our own reth node"
/// story. Runs the blocking `ureq` call on a `spawn_blocking` thread so a
/// slow/unreachable public endpoint can never stall the axum runtime this
/// crate's money endpoints share.
#[flux_api_macros::api(GET, "/v1/eth/usdc", summary = "Read-only USDC balance for an EVM address on Ethereum or Polygon (via public RPC)")]
pub async fn eth_usdc_handler(Query(q): Query<UsdcBalanceQuery>) -> Json<serde_json::Value> {
    let chain_str = q.chain.unwrap_or_default();
    let Some(chain) = eth::Chain::parse(&chain_str) else {
        return Json(serde_json::json!({
            "ok": false,
            "error": format!("unsupported chain \"{chain_str}\" — try \"ethereum\" or \"polygon\""),
        }));
    };
    let address = q.address;
    match tokio::task::spawn_blocking(move || eth::usdc_balance_raw(&address, chain)).await {
        Ok(Ok(raw)) => Json(serde_json::json!({
            "ok": true,
            "chain": chain_str_canonical(chain),
            "symbol": "USDC",
            "decimals": 6,
            "balance_raw": raw.to_string(),
            "balance": eth::format_usdc(raw),
        })),
        Ok(Err(e)) => Json(serde_json::json!({ "ok": false, "error": e })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": format!("internal: {e}") })),
    }
}

fn chain_str_canonical(chain: eth::Chain) -> &'static str {
    match chain {
        eth::Chain::Ethereum => "ethereum",
        eth::Chain::Polygon => "polygon",
    }
}

// ── mining on the braid ─────────────────────────────────────────────────────
//
// The same `flux-miner` challenge/submit contract `sigil-rpcd` serves, bound to
// the braid frontier instead of a separate linear chain. An existing miner needs
// only a new URL. Wire-compatible paths are mounted alongside the `/v1/` ones so
// a rig configured for `/api/v1/mining/*` works unchanged.

#[derive(Debug, Deserialize)]
pub struct ChallengeQuery {
    /// 64-hex miner wallet (canonical lowercase — the header commits to it).
    pub wallet: Option<String>,
    /// Self-reported Lane-A hashes/s; summed across live miners into `net_hps`.
    pub hps: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct MinersResponse {
    pub height: Option<u64>,
    pub net_hps: f64,
    pub live_miners: usize,
    pub blocks_accepted: u64,
    pub shares_accepted: u64,
    pub queued_solves: usize,
    pub rejects: Vec<(String, u64)>,
}

#[flux_api_macros::api(GET, "/v1/mining/challenge", summary = "Dual-lane challenge bound to the braid frontier")]
pub async fn mining_challenge(
    State(st): State<AppState>,
    Query(q): Query<ChallengeQuery>,
) -> Result<Json<flux_miner::client::Challenge>, StatusCode> {
    // WIRE-COMPAT (P1 fix): the shipping flux-miner client deserializes the
    // response body DIRECTLY into `Challenge` (`client.rs` → `.json::<Challenge>()`),
    // exactly as `sigil-rpcd` serves it. Wrapping it in the `ApiResponse` envelope
    // made every miner silently fail to parse the reply and produce 0 shares while
    // still registering as a live miner. Return the RAW `Challenge`, like rpcd.
    let wallet = q.wallet.as_deref().and_then(hex32);
    if let Some(hps) = q.hps {
        st.mining.report_hps(wallet, Some(hps), now_ms());
    }
    match st.mining.challenge_for(wallet, now_ms()) {
        Some(c) => Ok(Json(c)),
        // No mineable frontier yet → a real 503 so the client's `.error_for_status()?`
        // retries cleanly instead of trying to parse an envelope as a Challenge.
        None => Err(StatusCode::SERVICE_UNAVAILABLE),
    }
}

#[flux_api_macros::api(POST, "/v1/mining/submit", summary = "Submit a solved dual-lane share; a full solve mints a braid block")]
pub async fn mining_submit(
    State(st): State<AppState>,
    Json(sub): Json<flux_miner::client::Submission>,
) -> Json<flux_miner::client::SubmitResult> {
    // WIRE-COMPAT (P1 fix): the client reads the response body as a raw
    // `SubmitResult` (`client.rs` → `.json::<SubmitResult>()`), matching rpcd.
    // No `ApiResponse` envelope here for the same reason as `mining_challenge`.
    use flux_miner::client::SubmitResult;
    let result = match st.mining.submit(&sub) {
        SubmitOutcome::Block { .. } => {
            SubmitResult { accepted: true, reason: None, share: false }
        }
        SubmitOutcome::Share { .. } => {
            SubmitResult { accepted: true, reason: None, share: true }
        }
        SubmitOutcome::Rejected { kind, detail } => SubmitResult {
            accepted: false,
            reason: Some(format!("{}: {}", kind.as_str(), detail)),
            share: false,
        },
    };
    Json(result)
}

#[flux_api_macros::api(GET, "/v1/mining/miners", summary = "Live mining power and accept/reject counters")]
pub async fn mining_miners(State(st): State<AppState>) -> Json<ApiResponse<MinersResponse>> {
    let (net_hps, live_miners, blocks, shares, rejects) = st.mining.stats(now_ms());
    ApiResponse::ok(MinersResponse {
        height: st.mining.tip().map(|t| t.height),
        net_hps,
        live_miners,
        blocks_accepted: blocks,
        shares_accepted: shares,
        queued_solves: st.mining.queued_solves(),
        rejects,
    })
}

/// Build the money router with tower middleware tuned for a money workload
/// (Quillon's stack: concurrency cap, timeout, permissive CORS, body limit).
pub fn router(state: AppState) -> Router {
    use tower_http::cors::CorsLayer;
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/balance", get(balance))
        .route("/v1/supply", get(supply))
        .route("/v1/transactions", post(submit_transaction))
        .route("/v1/transactions/:hash", get(tx_status))
        .route("/v1/send", post(send_handler))
        .route("/v1/eth/usdc", get(eth_usdc_handler))
        .route("/v1/mining/challenge", get(mining_challenge))
        .route("/v1/mining/submit", post(mining_submit))
        .route("/v1/mining/miners", get(mining_miners))
        // Wire-compatible aliases: a rig pointed at the rpcd paths keeps working
        // when its URL moves to the braid node.
        .route("/api/v1/mining/challenge", get(mining_challenge))
        .route("/api/v1/mining/submit", post(mining_submit))
        .route("/api/v1/mining/miners", get(mining_miners))
        // Wallet-compatible aliases (2026-08-16): sigil-top's embedded wallet
        // (gui/sigil-wallet-tron-embedded.html) calls these exact /api/v1/...
        // paths same-origin through its proxy, which defaults to rpcd
        // (SIGIL_NODE_URL unset -> :8099) — dead since 2026-08-15, frozen at
        // height 325651, while this braid has been live and growing the whole
        // time. The wallet was showing stale/wrong balance and transaction
        // data with no visible error (operator-reported live). fetchBal()'s
        // parser already handles EITHER response shape defensively
        // (`j.balance` for rpcd's flat body, `j.data.balance` for this API's
        // wrapped ApiResponse envelope), so no client-side change is needed —
        // only routing the request here actually reaches live data. Only
        // balance is aliased in this pass (the specific complaint); a real
        // /api/v1/status + /api/v1/recent + /api/v1/search port (transaction
        // history needs indexing this API doesn't have yet) is separate,
        // larger follow-up work, not done here.
        .route("/api/v1/balance", get(balance))
        .route("/api/v1/supply", get(supply))
        // The wallet's `doSend` posts here directly (gui/sigil-wallet-tron-
        // embedded.html) — this is the fix for the "HTTP 200 send feature
        // doesn't work" report: the route didn't exist before, so every send
        // 404'd, and serve.rs's proxy (see its own fix) was masking that 404
        // as a fake "200 OK" with an unparseable body.
        .route("/api/v1/send", post(send_handler))
        .route("/api/v1/eth/usdc", get(eth_usdc_handler))
        .layer(axum::extract::DefaultBodyLimit::max(50 * 1024 * 1024))
        .layer(tower_http::timeout::TimeoutLayer::new(Duration::from_secs(30)))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Spawn the API on `addr` (e.g. "0.0.0.0:8181"), sharing the producer's state.
/// Non-blocking: runs on the caller's tokio runtime.
pub async fn serve(addr: &str, state: AppState) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router(state).into_make_service()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AppState {
        AppState::new(
            Arc::new(MempoolBackend::legacy()),
            Arc::new(RwLock::new(SigilState::new())),
        )
    }

    /// Fund a wallet through the public chokepoint (the only external path).
    fn fund(st: &AppState, w: WalletId, amount: u128) {
        use sigil_state::{commit_state_transition, StateMutation, StateTransition};
        let mut s = st.state.write().unwrap();
        let h = 0;
        commit_state_transition(
            &mut s,
            &StateTransition {
                at_height: h,
                mutations: vec![StateMutation::SetBalance { wallet: w, token: NATIVE, amount }],
            },
            h,
        ).unwrap();
    }

    #[tokio::test]
    async fn balance_reads_shared_state() {
        let st = state();
        let w = [0xABu8; 32];
        fund(&st, w, 42_00000000);
        let resp = balance(
            State(st.clone()),
            Query(BalanceQuery { wallet: hex::encode(w), token: None }),
        ).await;
        assert!(resp.0.ok);
        assert_eq!(resp.0.data.unwrap().balance, "4200000000");
    }

    #[tokio::test]
    async fn supply_reports_cap() {
        let st = state();
        fund(&st, [1u8; 32], 1_00000000);
        let resp = supply(State(st)).await;
        let d = resp.0.data.unwrap();
        assert_eq!(d.native_supply, "100000000");
        assert_eq!(d.max_supply, MAX_SUPPLY.to_string());
        assert!(d.minted_pct > 0.0 && d.minted_pct < 0.01);
    }

    #[tokio::test]
    async fn bad_wallet_rejected() {
        let resp = balance(
            State(state()),
            Query(BalanceQuery { wallet: "nothex".into(), token: None }),
        ).await;
        assert!(!resp.0.ok);
        assert!(resp.0.error.unwrap().contains("64-hex"));
    }
}
