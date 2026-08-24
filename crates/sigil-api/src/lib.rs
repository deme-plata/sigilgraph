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

use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use flux_search::{SearchEngine, SearchQuery};

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

pub mod bridge;
use bridge::BridgeBridge;

pub mod usds_bridge;
use usds_bridge::UsdsBridgeBridge;

pub mod dex;
use dex::DexBridge;

pub mod usds;
use usds::UsdsBridge;

pub mod eth;

pub mod dagknight;
/// PV-1 private transfers: shield / shielded-send / unshield.
pub mod shielded;
use dagknight::DagSnapshotBridge;

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
    /// The shielded-transaction queue — private transfers. Same drain contract as
    /// `send`; see the `shielded` module docs for why its spends carry no signature.
    pub shielded: Arc<shielded::ShieldedBridge>,
    /// The SIGIL <-> Polygon lock/unlock bridge — see `bridge` module docs.
    /// Same "always constructed, inert without traffic" shape; drained by
    /// the producer alongside `send`.
    pub bridge: Arc<BridgeBridge>,
    /// The wallet-authenticated swap / add-liquidity / remove-liquidity
    /// queue — see `dex` module docs. Same shape as `send`/`bridge`; drained
    /// by the producer alongside them.
    pub dex: Arc<DexBridge>,
    /// The wallet-authenticated USDS mint/redeem queue — see `usds` module
    /// docs. Same shape as `dex`; drained by the producer alongside it.
    pub usds: Arc<UsdsBridge>,
    /// The USDS <-> Polygon lock/unlock bridge — see `usds_bridge` module
    /// docs. A separate instance from `bridge` (which does native SIGIL),
    /// its own vault/admin/relayer, drained by the producer alongside it.
    pub usds_bridge: Arc<UsdsBridgeBridge>,
    /// Real wallet/transaction/block search (2026-08-20) — populated by
    /// sigil-node's `search_index` background task, which tails `ChainLog`
    /// independently of block-application (see that module's doc comment for
    /// why: it's a reader of the already-durable log, never coupled to the
    /// consensus-sensitive apply path). `Mutex`, not `RwLock`: `SearchEngine`
    /// isn't internally synchronized and `search()` takes `&mut self` even
    /// for a read (it caches), so a read-lock wouldn't be enough anyway.
    pub search: Arc<Mutex<SearchEngine>>,
    /// Read-only, periodically-refreshed GHOSTDAG snapshot for the DagKnight
    /// visualization — see `dagknight` module docs for why `Braid` itself is
    /// never locked or shared directly.
    pub dagknight: Arc<DagSnapshotBridge>,
}

impl AppState {
    /// Build the shared state from the producer's mempool + state handles, with
    /// a fresh mining bridge and an unconfigured (inert) money bridge.
    pub fn new(mempool: Arc<MempoolBackend>, state: Arc<RwLock<SigilState>>) -> Self {
        Self {
            mempool,
            state,
            mining: Arc::new(MiningBridge::new()),
            send: Arc::new(SendBridge::new()),
            shielded: Arc::new(shielded::ShieldedBridge::new()),
            bridge: Arc::new(BridgeBridge::new(None, None)),
            dex: Arc::new(DexBridge::new()),
            usds: Arc::new(UsdsBridge::new()),
            usds_bridge: Arc::new(UsdsBridgeBridge::new(None, None)),
            search: Arc::new(Mutex::new(SearchEngine::new())),
            dagknight: Arc::new(DagSnapshotBridge::new()),
        }
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

/// Public: `sigil-node` uses this to parse `SIGIL_BRIDGE_ADMIN_WALLET`/
/// `SIGIL_BRIDGE_RELAYER_WALLET` env vars into `WalletId`s at startup.
pub fn hex32(s: &str) -> Option<WalletId> {
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

/// Query params for `/v1/search`. `mode=literal` (default) does an exact
/// substring match — the right mode for pasting a hash/address, and the
/// wallet's search bar's main use case. `mode=fuzzy` runs flux-search's real
/// TF-IDF/ranked search for free-text queries ("mint reward", "swap events").
#[derive(Debug, Deserialize)]
pub struct SearchQueryParams {
    pub q: String,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub page: Option<usize>,
    #[serde(default)]
    pub per_page: Option<usize>,
}

#[flux_api_macros::api(GET, "/v1/search", summary = "Search indexed blocks by address/hash (literal) or free text (fuzzy)")]
pub async fn search_handler(
    State(st): State<AppState>,
    Query(q): Query<SearchQueryParams>,
) -> Json<ApiResponse<flux_search::SearchResponse>> {
    if q.q.trim().is_empty() {
        return ApiResponse::err("q must not be empty");
    }
    let page = q.page.unwrap_or(1);
    let per_page = q.per_page.unwrap_or(10);
    let fuzzy = q.mode.as_deref() == Some("fuzzy");
    let Ok(mut engine) = st.search.lock() else {
        return ApiResponse::err("search index unavailable");
    };
    let resp = if fuzzy {
        engine.search(SearchQuery { q: q.q, page, per_page, ..Default::default() })
    } else {
        engine.literal_search(&q.q, page, per_page, false)
    };
    ApiResponse::ok(resp)
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
    // PRIVACY-ONLY (2026-08-23): transparent peer-to-peer sends are retired at
    // `sigil_tx::SHIELDED_ONLY_HEIGHT`. Refuse here with a usable explanation rather than
    // queueing a transaction the producer will reject at mint — a caller who gets `ok:true`
    // and never sees their money land has no way to find out why.
    if sigil_tx::SHIELDED_ONLY_HEIGHT == 0 {
        return Json(serde_json::json!({
            "ok": false,
            "error": "SIGIL is privacy-only: transparent sends are retired. \
                      Use POST /v1/shield to move value into the shielded pool, then \
                      POST /v1/shielded_send to pay privately, and POST /v1/unshield to exit.",
            "retired_at_height": sigil_tx::SHIELDED_ONLY_HEIGHT,
            "use_instead": ["/v1/shield", "/v1/shielded_send", "/v1/unshield"],
        }));
    }
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

/// Publish a wallet's shielded key, so its block rewards are paid into the pool.
///
/// The single highest-leverage call for the network's privacy: a miner registers once and
/// every subsequent reward becomes a pool note owned by them, which is what grows the
/// anonymity set without persuading anyone to change their behaviour.
#[flux_api_macros::api(POST, "/v1/shielded/register", summary = "Register a shielded address so block rewards are paid privately")]
pub async fn shielded_register_handler(
    State(st): State<AppState>,
    Json(req): Json<shielded::RegisterRequest>,
) -> Json<serde_json::Value> {
    match st.shielded.submit_register(
        &req.wallet, &req.pk_shield, &req.pk_encrypt, req.fee, &req.sig, req.req_nonce,
    ) {
        Ok(h) => Json(serde_json::json!({
            "ok": true, "txid": hex::encode(h), "ts_ms": now_ms(),
            "note": "queued; once it lands, block rewards for this wallet mint as shielded notes",
        })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.message() })),
    }
}

/// Move value from a transparent wallet into the shielded pool.
///
/// The caller computes `cm = compress2(amount, blinding)` locally and keeps the blinding —
/// the server never sees it, which is exactly what makes the resulting note private.
#[flux_api_macros::api(POST, "/v1/shield", summary = "Deposit transparent SIGIL into the shielded pool")]
pub async fn shield_handler(
    State(st): State<AppState>,
    Json(req): Json<shielded::ShieldRequest>,
) -> Json<serde_json::Value> {
    match st.shielded.submit_shield(&req.from, req.amount, &req.cm, req.fee, &req.sig, req.req_nonce) {
        Ok(h) => Json(serde_json::json!({
            "ok": true, "txid": hex::encode(h), "ts_ms": now_ms(),
            "note": "queued for the next braid block",
        })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.message() })),
    }
}

/// A shielded → shielded transfer. Carries no sender, no recipient, and no amount.
#[flux_api_macros::api(POST, "/v1/shielded_send", summary = "Private shielded-to-shielded transfer (amounts hidden)")]
pub async fn shielded_send_handler(
    State(st): State<AppState>,
    Json(req): Json<shielded::ShieldedSendRequest>,
) -> Json<serde_json::Value> {
    let proof = match hex::decode(req.proof.trim_start_matches("0x")) {
        Ok(p) => p,
        Err(_) => return Json(serde_json::json!({ "ok": false, "error": "proof must be hex" })),
    };
    match st.shielded.submit_shielded_send(
        &req.anchor, &req.nullifier, &req.cm_outs, req.fee, proof, &req.note_ciphertexts,
    ) {
        Ok(h) => Json(serde_json::json!({
            "ok": true, "txid": hex::encode(h), "ts_ms": now_ms(),
            "note": "queued for the next braid block",
        })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.message() })),
    }
}

/// Move value out of the shielded pool to a transparent wallet.
#[flux_api_macros::api(POST, "/v1/unshield", summary = "Withdraw from the shielded pool to a transparent wallet")]
pub async fn unshield_handler(
    State(st): State<AppState>,
    Json(req): Json<shielded::UnshieldRequest>,
) -> Json<serde_json::Value> {
    let proof = match hex::decode(req.proof.trim_start_matches("0x")) {
        Ok(p) => p,
        Err(_) => return Json(serde_json::json!({ "ok": false, "error": "proof must be hex" })),
    };
    match st.shielded.submit_unshield(
        &req.to, req.amount, &req.anchor, &req.nullifier, &req.cm_outs, proof, req.fee,
    ) {
        Ok(h) => Json(serde_json::json!({
            "ok": true, "txid": hex::encode(h), "ts_ms": now_ms(),
            "note": "queued for the next braid block",
        })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.message() })),
    }
}

/// The current anonymity-set root plus the pool's public counters.
///
/// A wallet needs the anchor to build a spend proof, and the note list to find its own
/// notes — note ownership is not discoverable server-side, so the wallet trial-decrypts
/// locally against commitments it fetches here.
#[flux_api_macros::api(GET, "/v1/shielded/anchor", summary = "Current shielded-pool anchor and note count")]
pub async fn shielded_anchor_handler(State(st): State<AppState>) -> Json<serde_json::Value> {
    let guard = match st.state.read() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let pool = guard.shielded();
    Json(serde_json::json!({
        "ok": true,
        "anchor": hex::encode(pool.current_root()),
        "notes": pool.len(),
        "nullifiers": pool.nullifier_count(),
        "value_locked": pool.value_locked().to_string(),
        "ts_ms": now_ms(),
    }))
}

/// The real (unpadded) note commitments, in leaf order.
///
/// 2026-08-23: `shielded_anchor_handler`'s own doc comment already promised this ("the
/// wallet ... fetches [the note list] here") but only ever returned a COUNT — no wallet
/// or mining client could actually locate its notes or build a spend's inclusion path
/// without the real commitments. This closes that gap. Padding is intentionally NOT sent:
/// it is deterministically derivable client-side from
/// `sigil_shield::note_v1::padding_leaf_wire` (same formula this server uses), so shipping
/// it would just be ~1MB of bytes the client can already compute for free.
/// 2026-08-24: also returns `ciphertexts`, positionally aligned with `leaves` — the
/// piece that turns "here are the commitments" into "here is what you can actually
/// trial-decrypt to find yours". Before this, a wallet could locate notes it created
/// itself but had no way to discover a payment someone else sent it; the commitments
/// alone carry no ciphertext, so this endpoint had nothing to hand back for a received
/// note until `note_ciphertexts` existed to store one.
#[flux_api_macros::api(GET, "/v1/shielded/leaves", summary = "Real (unpadded) note commitments plus delivery ciphertexts, for wallet/miner note discovery and spend proving")]
pub async fn shielded_leaves_handler(State(st): State<AppState>) -> Json<serde_json::Value> {
    let guard = match st.state.read() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let pool = guard.shielded();
    Json(serde_json::json!({
        "ok": true,
        "leaves": pool.notes().iter().map(hex::encode).collect::<Vec<_>>(),
        "ciphertexts": pool.ciphertexts(),
        "capacity": sigil_state::shielded::POOL_CAPACITY,
        "ts_ms": now_ms(),
    }))
}

/// `?wallet=<64-hex>` — the shielded address a payer needs to pay this wallet
/// privately: the circuit key (`pk_shield`, what a note commitment binds to) and the
/// delivery key (`pk_encrypt`, what a note ciphertext is sealed to). Both are public by
/// design — see `sigil_shield::note_cipher`'s module docs on why publishing them costs
/// nothing (they are one-way / cannot be used to spend or decrypt anyone else's notes).
#[derive(Debug, Deserialize)]
pub struct ShieldedAddressQuery {
    pub wallet: String,
}

#[flux_api_macros::api(GET, "/v1/shielded/address", summary = "Look up a wallet's published shielded address (pk_shield + pk_encrypt) so it can be paid privately")]
pub async fn shielded_address_handler(
    State(st): State<AppState>,
    Query(q): Query<ShieldedAddressQuery>,
) -> Json<serde_json::Value> {
    let Some(wallet) = hex32(&q.wallet) else {
        return Json(serde_json::json!({ "ok": false, "error": "wallet must be 64-hex" }));
    };
    let guard = match st.state.read() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let pool = guard.shielded();
    let Some(pk_shield) = pool.shielded_address(&wallet) else {
        return Json(serde_json::json!({
            "ok": false, "error": "wallet has not registered a shielded address",
        }));
    };
    Json(serde_json::json!({
        "ok": true,
        "wallet": hex::encode(wallet),
        "pk_shield": hex::encode(pk_shield),
        "pk_encrypt": pool.encrypt_key(&wallet).map(hex::encode),
        "ts_ms": now_ms(),
    }))
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

// ── the SIGIL <-> Polygon bridge ────────────────────────────────────────────
// See `bridge` module docs for the full trust-model writeup. Every mutating
// route here returns the SAME flat-JSON shape as `send_handler` (not the
// `ApiResponse<T>` envelope) so a caller only ever needs `j.ok`/`j.error`
// plus whatever data field is relevant — consistent with the one other
// wallet-signed mutation route this crate already ships.

#[derive(Debug, Deserialize)]
pub struct BridgeLockRequest {
    pub from: String,
    pub amount: u128,
    pub dest_polygon_address: String,
    pub sig: String,
    pub req_nonce: u64,
}

#[flux_api_macros::api(POST, "/v1/bridge/lock", summary = "Wallet-signed: lock native SIGIL into the bridge vault for minting on Polygon")]
pub async fn bridge_lock_handler(
    State(st): State<AppState>,
    Json(req): Json<BridgeLockRequest>,
) -> Json<serde_json::Value> {
    match st.bridge.submit_lock(&req.from, req.amount, &req.dest_polygon_address, &req.sig, req.req_nonce) {
        Ok(rec) => Json(serde_json::json!({
            "ok": true,
            "lock_id": rec.id,
            "tx_hash": rec.tx_hash,
            "vault": bridge::BridgeBridge::vault_hex(),
            "note": "queued for the next braid block — the relayer mints on Polygon once this settles",
        })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.message() })),
    }
}

#[derive(Debug, Deserialize)]
pub struct LocksSinceQuery {
    #[serde(default)]
    pub since: u64,
}

#[flux_api_macros::api(GET, "/v1/bridge/locks", summary = "Gap-free feed of vault locks for the relayer to poll (?since=<lock_id>)")]
pub async fn bridge_locks_handler(
    State(st): State<AppState>,
    Query(q): Query<LocksSinceQuery>,
) -> Json<ApiResponse<Vec<bridge::LockRecord>>> {
    ApiResponse::ok(st.bridge.locks_since(q.since))
}

#[derive(Debug, Deserialize)]
pub struct BridgeUnlockRequest {
    pub relayer: String,
    pub to: String,
    pub amount: u128,
    pub polygon_burn_tx: String,
    pub sig: String,
    pub req_nonce: u64,
}

#[flux_api_macros::api(POST, "/v1/bridge/unlock", summary = "Relayer-signed: release SIGIL from the vault against a verified Polygon burn")]
pub async fn bridge_unlock_handler(
    State(st): State<AppState>,
    Json(req): Json<BridgeUnlockRequest>,
) -> Json<serde_json::Value> {
    match st.bridge.submit_unlock(&req.relayer, &req.to, req.amount, &req.polygon_burn_tx, &req.sig, req.req_nonce) {
        Ok(hash) => Json(serde_json::json!({
            "ok": true,
            "tx_hash": hex::encode(hash),
            "note": "queued for the next braid block",
        })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.message() })),
    }
}

#[derive(Debug, Deserialize)]
pub struct BridgePauseRequest {
    pub admin: String,
    pub paused: bool,
    pub sig: String,
    pub req_nonce: u64,
}

#[flux_api_macros::api(POST, "/v1/bridge/pause", summary = "Admin-signed: freeze or unfreeze the bridge (lock + unlock both blocked while paused)")]
pub async fn bridge_pause_handler(
    State(st): State<AppState>,
    Json(req): Json<BridgePauseRequest>,
) -> Json<serde_json::Value> {
    match st.bridge.set_paused(&req.admin, req.paused, &req.sig, req.req_nonce) {
        Ok(()) => Json(serde_json::json!({ "ok": true, "paused": req.paused })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.message() })),
    }
}

#[derive(Debug, Deserialize)]
pub struct BridgeRotateRelayerRequest {
    pub admin: String,
    pub new_relayer: String,
    pub sig: String,
    pub req_nonce: u64,
}

#[flux_api_macros::api(POST, "/v1/bridge/rotate_relayer", summary = "Admin-signed: replace the relayer wallet authorized to unlock funds")]
pub async fn bridge_rotate_relayer_handler(
    State(st): State<AppState>,
    Json(req): Json<BridgeRotateRelayerRequest>,
) -> Json<serde_json::Value> {
    match st.bridge.rotate_relayer(&req.admin, &req.new_relayer, &req.sig, req.req_nonce) {
        Ok(()) => Json(serde_json::json!({ "ok": true, "relayer": req.new_relayer })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.message() })),
    }
}

#[derive(Debug, Serialize)]
pub struct BridgeStatusResponse {
    pub vault: String,
    pub vault_balance: String,
    pub relayer: Option<String>,
    pub admin: Option<String>,
    pub paused: bool,
    pub lock_count: usize,
}

#[flux_api_macros::api(GET, "/v1/bridge/status", summary = "Bridge vault balance, relayer/admin wallets, paused state")]
pub async fn bridge_status_handler(State(st): State<AppState>) -> Json<ApiResponse<BridgeStatusResponse>> {
    let vault = bridge::BRIDGE_VAULT_WALLET;
    let vault_balance = st.state.read().map(|s| s.balance_of(&vault, &NATIVE)).unwrap_or(0);
    ApiResponse::ok(BridgeStatusResponse {
        vault: bridge::BridgeBridge::vault_hex(),
        vault_balance: vault_balance.to_string(),
        relayer: st.bridge.relayer_hex(),
        admin: st.bridge.admin_hex(),
        paused: st.bridge.is_paused(),
        lock_count: st.bridge.lock_count(),
    })
}

// ── DEX: swap / add-liquidity / remove-liquidity ────────────────────────────
// Same three routes `sigil-rpcd` served (`/pools /swap /add_liquidity`) — but
// against the REAL braid state (`SigilState::pools`), through the real
// `commit_state_transition` chokepoint, not `sigil-rpcd`'s disconnected,
// long-stale copy. See `dex` module docs for the auth + queueing shape.

/// There is no persistent token-ticker registry on chain yet — `TokenDeploy`
/// only emits an event, it doesn't leave a queryable `TokenId -> ticker`
/// mapping behind (a real follow-on, not invented here). `sym_a`/`sym_b` are
/// therefore best-effort display strings for the wallet's pool picker:
/// `"SIGIL"` for the native token, otherwise the token id's first 8 hex
/// characters — honestly a placeholder, not a real symbol lookup.
fn display_symbol(token: &sigil_state::TokenId) -> String {
    if *token == NATIVE {
        "SIGIL".to_string()
    } else {
        hex::encode(token)[..8].to_uppercase()
    }
}

#[derive(Debug, Serialize)]
pub struct PoolSummary {
    pub id: String,
    pub token_a: String,
    pub token_b: String,
    pub sym_a: String,
    pub sym_b: String,
    pub reserve_a: String,
    pub reserve_b: String,
    pub lp_shares: String,
    pub fee_bps: u16,
}

/// Flat `{ok, pools:[...]}` — NOT the generic `ApiResponse<T>` envelope
/// (whose payload nests under `.data`). The wallet's already-shipped
/// `openSwap()` reads `j.pools` directly off the top-level response
/// (`gui/sigil-wallet-tron-embedded.html`), same reasoning `send_handler`
/// documents for why its own money-moving route breaks the envelope
/// convention: match the client that already exists rather than force a
/// UI change for a shape that isn't actually more consistent, just newer.
#[flux_api_macros::api(GET, "/v1/pools", summary = "List every live DEX pool (reserves, LP shares, fee tier)")]
pub async fn pools_handler(State(st): State<AppState>) -> Json<serde_json::Value> {
    let pools: Vec<PoolSummary> = st.state.read().map(|s| {
        s.pools_iter()
            .map(|(id, p)| PoolSummary {
                id: hex::encode(id),
                token_a: hex::encode(p.token_a),
                token_b: hex::encode(p.token_b),
                sym_a: display_symbol(&p.token_a),
                sym_b: display_symbol(&p.token_b),
                reserve_a: p.reserve_a.to_string(),
                reserve_b: p.reserve_b.to_string(),
                lp_shares: p.lp_shares.to_string(),
                fee_bps: p.fee_bps,
            })
            .collect()
    }).unwrap_or_default();
    Json(serde_json::json!({ "ok": true, "pools": pools }))
}

/// Body for `POST /v1/swap` — the EXACT shape the wallet already ships
/// (`gui/sigil-wallet-tron-embedded.html`'s `doSwap`, field-for-field
/// identical to what `sigil-rpcd` accepted, so the existing UI needed no
/// signing-logic changes to point at the real chain): `dir` is `"AtoB"` or
/// `"BtoA"` — which side of the pool is being sold — not a raw token id.
#[derive(Debug, Deserialize)]
pub struct SwapRequest {
    pub from: String,
    pub pool: String,
    pub dir: String,
    pub amount_in: u128,
    pub min_out: u128,
    pub sig: String,
    pub req_nonce: u64,
}

#[flux_api_macros::api(POST, "/v1/swap", summary = "Wallet-signed: swap one token for another through a real DEX pool")]
pub async fn swap_handler(
    State(st): State<AppState>,
    Json(req): Json<SwapRequest>,
) -> Json<serde_json::Value> {
    let Some(pool_id) = hex32(&req.pool) else {
        return Json(serde_json::json!({ "ok": false, "error": "pool must be a 64-hex pool id" }));
    };
    // Resolve `dir` -> the actual input token from the LIVE pool, before
    // authenticating — the signed message carries `dir` literally (see
    // `dex` module docs), so this resolution never needs to be trusted, only
    // used to build the tx once the signature over `dir` itself checks out.
    let Some(pool) = st.state.read().ok().and_then(|s| s.pool(&pool_id).cloned()) else {
        return Json(serde_json::json!({ "ok": false, "error": "no such pool" }));
    };
    let in_token = match req.dir.as_str() {
        "AtoB" => pool.token_a,
        "BtoA" => pool.token_b,
        _ => return Json(serde_json::json!({ "ok": false, "error": "dir must be \"AtoB\" or \"BtoA\"" })),
    };
    match st.dex.submit_swap(&req.from, &req.pool, &req.dir, in_token, req.amount_in, req.min_out, &req.sig, req.req_nonce) {
        Ok(tx_hash) => Json(serde_json::json!({
            "ok": true,
            "tx_hash": hex::encode(tx_hash),
            "note": "queued for the next braid block",
        })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.message() })),
    }
}

#[derive(Debug, Deserialize)]
pub struct AddLiquidityRequest {
    pub from: String,
    pub token_a: String,
    pub token_b: String,
    pub amt_a: u128,
    pub amt_b: u128,
    /// Basis points, e.g. `30` = 0.30%. Locked in on a pool's first deposit
    /// (see `SigilTx::LpDeposit` docs); ignored-but-must-match on later ones
    /// since the pool id is derived FROM this value.
    pub fee_bps: u16,
    pub sig: String,
    pub req_nonce: u64,
}

#[flux_api_macros::api(POST, "/v1/add_liquidity", summary = "Wallet-signed: deposit a token pair into a real DEX pool, receive LP shares")]
pub async fn add_liquidity_handler(
    State(st): State<AppState>,
    Json(req): Json<AddLiquidityRequest>,
) -> Json<serde_json::Value> {
    match st.dex.submit_lp_deposit(
        &req.from, &req.token_a, &req.token_b, req.amt_a, req.amt_b, req.fee_bps, &req.sig, req.req_nonce,
    ) {
        Ok((tx_hash, pool)) => Json(serde_json::json!({
            "ok": true,
            "tx_hash": hex::encode(tx_hash),
            "pool": hex::encode(pool),
            "note": "queued for the next braid block",
        })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.message() })),
    }
}

#[derive(Debug, Deserialize)]
pub struct RemoveLiquidityRequest {
    pub from: String,
    pub pool: String,
    pub shares: u128,
    pub sig: String,
    pub req_nonce: u64,
}

#[flux_api_macros::api(POST, "/v1/remove_liquidity", summary = "Wallet-signed: burn LP shares, withdraw the underlying token pair")]
pub async fn remove_liquidity_handler(
    State(st): State<AppState>,
    Json(req): Json<RemoveLiquidityRequest>,
) -> Json<serde_json::Value> {
    match st.dex.submit_lp_withdraw(&req.from, &req.pool, req.shares, &req.sig, req.req_nonce) {
        Ok(tx_hash) => Json(serde_json::json!({
            "ok": true,
            "tx_hash": hex::encode(tx_hash),
            "note": "queued for the next braid block",
        })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.message() })),
    }
}

// ── USDS: SIGIL's native $-pegged stablecoin ────────────────────────────────
// See `sigil_usds` crate docs for the full mechanism (105% collateral buffer
// + the same protocol fee DEX swaps pay). Mint/redeem authenticate + queue
// here, exactly like `dex`; the actual math runs once inside `apply_tx` when
// the queued tx is applied — this layer adds no new consensus logic.

#[derive(Debug, Serialize)]
pub struct UsdsStatusResponse {
    /// Committed oracle price (USD×1e8 per SIGIL). `"0"` if never set.
    pub price: String,
    /// SIGIL currently locked in the collateral vault.
    pub vault_sigil: String,
    /// Total USDS in circulation.
    pub usds_supply: String,
}

#[flux_api_macros::api(GET, "/v1/usds/status", summary = "USDS oracle price, vault collateral, and circulating supply")]
pub async fn usds_status_handler(State(st): State<AppState>) -> Json<ApiResponse<UsdsStatusResponse>> {
    let (price, vault_sigil, usds_supply) = st.state.read().map(|s| {
        (
            sigil_oracle::read_price(&s).to_string(),
            s.balance_of(&sigil_usds::VAULT, &NATIVE).to_string(),
            s.balance_of(&sigil_usds::VAULT, &sigil_usds::USDS).to_string(),
        )
    }).unwrap_or_else(|_| ("0".into(), "0".into(), "0".into()));
    // NOTE: `usds_supply` above reads the VAULT's own USDS balance (always
    // 0 — the vault never holds USDS, only the SIGIL backing it); circulating
    // supply is the sum over every OTHER holder, which this crate has no
    // index over yet. Report price + vault collateral now (both real,
    // useful); supply is a real follow-up once an index exists, not faked
    // here in the meantime.
    ApiResponse::ok(UsdsStatusResponse { price, vault_sigil, usds_supply })
}

#[derive(Debug, Deserialize)]
pub struct UsdsMintRequest {
    pub from: String,
    pub sigil_amount: u128,
    pub sig: String,
    pub req_nonce: u64,
}

#[flux_api_macros::api(POST, "/v1/usds/mint", summary = "Wallet-signed: lock SIGIL collateral, mint USDS")]
pub async fn usds_mint_handler(
    State(st): State<AppState>,
    Json(req): Json<UsdsMintRequest>,
) -> Json<serde_json::Value> {
    match st.usds.submit_mint(&req.from, req.sigil_amount, &req.sig, req.req_nonce) {
        Ok(tx_hash) => Json(serde_json::json!({
            "ok": true,
            "tx_hash": hex::encode(tx_hash),
            "note": "queued for the next braid block",
        })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.message() })),
    }
}

#[derive(Debug, Deserialize)]
pub struct UsdsRedeemRequest {
    pub from: String,
    pub usds_amount: u128,
    pub sig: String,
    pub req_nonce: u64,
}

#[flux_api_macros::api(POST, "/v1/usds/redeem", summary = "Wallet-signed: burn USDS, release SIGIL collateral")]
pub async fn usds_redeem_handler(
    State(st): State<AppState>,
    Json(req): Json<UsdsRedeemRequest>,
) -> Json<serde_json::Value> {
    match st.usds.submit_redeem(&req.from, req.usds_amount, &req.sig, req.req_nonce) {
        Ok(tx_hash) => Json(serde_json::json!({
            "ok": true,
            "tx_hash": hex::encode(tx_hash),
            "note": "queued for the next braid block",
        })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.message() })),
    }
}

// ── the USDS <-> Polygon bridge ─────────────────────────────────────────────
// A second instance of the SAME lock/unlock pattern `bridge.rs` already
// proved live — see `usds_bridge` module docs for why it's a separate
// vault/admin/relayer instead of a generalized "any token" bridge. Every
// mutating route here returns the same flat-JSON shape `bridge_lock_handler`
// etc. already use.

#[derive(Debug, Deserialize)]
pub struct UsdsBridgeLockRequest {
    pub from: String,
    pub amount: u128,
    pub dest_polygon_address: String,
    pub sig: String,
    pub req_nonce: u64,
}

#[flux_api_macros::api(POST, "/v1/usds_bridge/lock", summary = "Wallet-signed: lock USDS into the bridge vault for minting on Polygon")]
pub async fn usds_bridge_lock_handler(
    State(st): State<AppState>,
    Json(req): Json<UsdsBridgeLockRequest>,
) -> Json<serde_json::Value> {
    match st.usds_bridge.submit_lock(&req.from, req.amount, &req.dest_polygon_address, &req.sig, req.req_nonce) {
        Ok(rec) => Json(serde_json::json!({
            "ok": true,
            "lock_id": rec.id,
            "tx_hash": rec.tx_hash,
            "vault": usds_bridge::UsdsBridgeBridge::vault_hex(),
            "note": "queued for the next braid block — the relayer mints on Polygon once this settles",
        })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.message() })),
    }
}

#[flux_api_macros::api(GET, "/v1/usds_bridge/locks", summary = "Gap-free feed of USDS vault locks for the relayer to poll (?since=<lock_id>)")]
pub async fn usds_bridge_locks_handler(
    State(st): State<AppState>,
    Query(q): Query<LocksSinceQuery>,
) -> Json<ApiResponse<Vec<usds_bridge::UsdsLockRecord>>> {
    ApiResponse::ok(st.usds_bridge.locks_since(q.since))
}

#[derive(Debug, Deserialize)]
pub struct UsdsBridgeUnlockRequest {
    pub relayer: String,
    pub to: String,
    pub amount: u128,
    pub polygon_burn_tx: String,
    pub sig: String,
    pub req_nonce: u64,
}

#[flux_api_macros::api(POST, "/v1/usds_bridge/unlock", summary = "Relayer-signed: release USDS from the vault against a verified Polygon burn")]
pub async fn usds_bridge_unlock_handler(
    State(st): State<AppState>,
    Json(req): Json<UsdsBridgeUnlockRequest>,
) -> Json<serde_json::Value> {
    match st.usds_bridge.submit_unlock(&req.relayer, &req.to, req.amount, &req.polygon_burn_tx, &req.sig, req.req_nonce) {
        Ok(hash) => Json(serde_json::json!({
            "ok": true,
            "tx_hash": hex::encode(hash),
            "note": "queued for the next braid block",
        })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.message() })),
    }
}

#[flux_api_macros::api(POST, "/v1/usds_bridge/pause", summary = "Admin-signed: freeze or unfreeze the USDS bridge")]
pub async fn usds_bridge_pause_handler(
    State(st): State<AppState>,
    Json(req): Json<BridgePauseRequest>,
) -> Json<serde_json::Value> {
    match st.usds_bridge.set_paused(&req.admin, req.paused, &req.sig, req.req_nonce) {
        Ok(()) => Json(serde_json::json!({ "ok": true, "paused": req.paused })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.message() })),
    }
}

#[flux_api_macros::api(POST, "/v1/usds_bridge/rotate_relayer", summary = "Admin-signed: replace the USDS bridge's relayer wallet")]
pub async fn usds_bridge_rotate_relayer_handler(
    State(st): State<AppState>,
    Json(req): Json<BridgeRotateRelayerRequest>,
) -> Json<serde_json::Value> {
    match st.usds_bridge.rotate_relayer(&req.admin, &req.new_relayer, &req.sig, req.req_nonce) {
        Ok(()) => Json(serde_json::json!({ "ok": true, "relayer": req.new_relayer })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.message() })),
    }
}

#[derive(Debug, Serialize)]
pub struct UsdsBridgeStatusResponse {
    pub vault: String,
    pub vault_balance: String,
    pub relayer: Option<String>,
    pub admin: Option<String>,
    pub paused: bool,
    pub lock_count: usize,
}

#[flux_api_macros::api(GET, "/v1/usds_bridge/status", summary = "USDS bridge vault balance, relayer/admin wallets, paused state")]
pub async fn usds_bridge_status_handler(State(st): State<AppState>) -> Json<ApiResponse<UsdsBridgeStatusResponse>> {
    let vault = usds_bridge::USDS_BRIDGE_VAULT_WALLET;
    let vault_balance = st.state.read().map(|s| s.balance_of(&vault, &sigil_usds::USDS)).unwrap_or(0);
    ApiResponse::ok(UsdsBridgeStatusResponse {
        vault: usds_bridge::UsdsBridgeBridge::vault_hex(),
        vault_balance: vault_balance.to_string(),
        relayer: st.usds_bridge.relayer_hex(),
        admin: st.usds_bridge.admin_hex(),
        paused: st.usds_bridge.is_paused(),
        lock_count: st.usds_bridge.lock_count(),
    })
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
    /// 2026-08-24 (MULTI-RIG fix): identifies THIS physical rig, distinct
    /// from the payout wallet — `flux_miner::client::rig_id()` already sends
    /// this on every fetch. Without it, two rigs mining to one wallet
    /// silently clobbered each other's hashrate report (see
    /// `MiningBridge::hps` field doc). `#[serde(default)]` + `Option` so an
    /// old client that doesn't send `&rig=` still parses exactly as before —
    /// it just falls back to the pre-fix per-wallet clobbering, not broken,
    /// only not-yet-improved.
    #[serde(default)]
    pub rig: Option<String>,
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
    /// This caller's own last-reported Lane-A rate — `0.0` unless `?wallet=`
    /// was passed and that wallet has an unexpired self-report (see
    /// [`mining::MiningBridge::hps_for_wallet_total`]). 2026-08-23: the wallet UI's "my
    /// hashrate" topbar pill was reading this off a per-miner array this
    /// endpoint never returned; this is that missing per-wallet readback.
    pub my_hps: f64,
}

#[derive(Debug, Deserialize)]
pub struct MinersQuery {
    /// 64-hex wallet to look up `my_hps` for. Omit for the aggregate-only view.
    pub wallet: Option<String>,
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
    // Bound + sanitize defensively even though the shipping client already
    // does this (`flux_miner::client::sanitize_rig_id`) — an untrusted caller
    // could send an oversized/adversarial string, and this becomes a HashMap
    // key held for up to HPS_IDLE_MS, so don't trust the wire blindly.
    let rig = q.rig.as_deref().map(|r| {
        r.chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
            .take(24)
            .collect::<String>()
    }).filter(|r| !r.is_empty());
    if let Some(hps) = q.hps {
        st.mining.report_hps(wallet, rig.clone(), Some(hps), now_ms());
    }
    match st.mining.challenge_for(wallet, rig, now_ms()) {
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

#[flux_api_macros::api(GET, "/v1/dagknight/recent", summary = "Recent finalized blocks with v2 GHOSTDAG blue/red coloring, for the DagKnight visualization")]
pub async fn dagknight_recent(State(st): State<AppState>) -> Json<ApiResponse<dagknight::DagSnapshot>> {
    ApiResponse::ok(st.dagknight.get())
}

#[flux_api_macros::api(GET, "/v1/mining/miners", summary = "Live mining power and accept/reject counters")]
pub async fn mining_miners(
    State(st): State<AppState>,
    Query(q): Query<MinersQuery>,
) -> Json<ApiResponse<MinersResponse>> {
    let (net_hps, live_miners, blocks, shares, rejects) = st.mining.stats(now_ms());
    let wallet = q.wallet.as_deref().and_then(hex32);
    ApiResponse::ok(MinersResponse {
        height: st.mining.tip().map(|t| t.height),
        net_hps,
        live_miners,
        blocks_accepted: blocks,
        shares_accepted: shares,
        queued_solves: st.mining.queued_solves(),
        rejects,
        my_hps: st.mining.hps_for_wallet_total(wallet),
    })
}

/// Build the money router with tower middleware tuned for a money workload
/// (Quillon's stack: concurrency cap, timeout, permissive CORS, body limit).
pub fn router(state: AppState) -> Router {
    use tower_http::cors::CorsLayer;
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/search", get(search_handler))
        .route("/v1/balance", get(balance))
        .route("/v1/supply", get(supply))
        .route("/v1/transactions", post(submit_transaction))
        .route("/v1/transactions/:hash", get(tx_status))
        .route("/v1/send", post(send_handler))
        .route("/v1/shielded/register", post(shielded_register_handler))
        .route("/v1/shield", post(shield_handler))
        .route("/v1/shielded_send", post(shielded_send_handler))
        .route("/v1/unshield", post(unshield_handler))
        .route("/v1/shielded/anchor", get(shielded_anchor_handler))
        .route("/v1/shielded/leaves", get(shielded_leaves_handler))
        .route("/v1/shielded/address", get(shielded_address_handler))
        .route("/v1/eth/usdc", get(eth_usdc_handler))
        .route("/v1/mining/challenge", get(mining_challenge))
        .route("/v1/mining/submit", post(mining_submit))
        .route("/v1/mining/miners", get(mining_miners))
        .route("/v1/dagknight/recent", get(dagknight_recent))
        .route("/v1/bridge/lock", post(bridge_lock_handler))
        .route("/v1/bridge/locks", get(bridge_locks_handler))
        .route("/v1/bridge/unlock", post(bridge_unlock_handler))
        .route("/v1/bridge/pause", post(bridge_pause_handler))
        .route("/v1/bridge/rotate_relayer", post(bridge_rotate_relayer_handler))
        .route("/v1/bridge/status", get(bridge_status_handler))
        .route("/v1/pools", get(pools_handler))
        .route("/v1/swap", post(swap_handler))
        .route("/v1/add_liquidity", post(add_liquidity_handler))
        .route("/v1/remove_liquidity", post(remove_liquidity_handler))
        .route("/v1/usds/status", get(usds_status_handler))
        .route("/v1/usds/mint", post(usds_mint_handler))
        .route("/v1/usds/redeem", post(usds_redeem_handler))
        .route("/v1/usds_bridge/lock", post(usds_bridge_lock_handler))
        .route("/v1/usds_bridge/locks", get(usds_bridge_locks_handler))
        .route("/v1/usds_bridge/unlock", post(usds_bridge_unlock_handler))
        .route("/v1/usds_bridge/pause", post(usds_bridge_pause_handler))
        .route("/v1/usds_bridge/rotate_relayer", post(usds_bridge_rotate_relayer_handler))
        .route("/v1/usds_bridge/status", get(usds_bridge_status_handler))
        // Wire-compatible aliases: a rig pointed at the rpcd paths keeps working
        // when its URL moves to the braid node.
        .route("/api/v1/mining/challenge", get(mining_challenge))
        .route("/api/v1/mining/submit", post(mining_submit))
        .route("/api/v1/mining/miners", get(mining_miners))
        .route("/api/v1/dagknight/recent", get(dagknight_recent))
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
        // /api/v1/status + /api/v1/recent port is separate, larger follow-up
        // work, not done here. /api/v1/search WAS the other half of that
        // gap — it's done now (2026-08-20): real indexing via
        // sigil-node's search_index.rs, tailing ChainLog in the background.
        .route("/api/v1/search", get(search_handler))
        .route("/api/v1/balance", get(balance))
        .route("/api/v1/supply", get(supply))
        // The wallet's `doSend` posts here directly (gui/sigil-wallet-tron-
        // embedded.html) — this is the fix for the "HTTP 200 send feature
        // doesn't work" report: the route didn't exist before, so every send
        // 404'd, and serve.rs's proxy (see its own fix) was masking that 404
        // as a fake "200 OK" with an unparseable body.
        .route("/api/v1/send", post(send_handler))
        .route("/api/v1/eth/usdc", get(eth_usdc_handler))
        .route("/api/v1/pools", get(pools_handler))
        .route("/api/v1/swap", post(swap_handler))
        .route("/api/v1/add_liquidity", post(add_liquidity_handler))
        .route("/api/v1/remove_liquidity", post(remove_liquidity_handler))
        .route("/api/v1/usds/status", get(usds_status_handler))
        .route("/api/v1/usds/mint", post(usds_mint_handler))
        .route("/api/v1/usds/redeem", post(usds_redeem_handler))
        .route("/api/v1/usds_bridge/lock", post(usds_bridge_lock_handler))
        .route("/api/v1/usds_bridge/locks", get(usds_bridge_locks_handler))
        .route("/api/v1/usds_bridge/unlock", post(usds_bridge_unlock_handler))
        .route("/api/v1/usds_bridge/pause", post(usds_bridge_pause_handler))
        .route("/api/v1/usds_bridge/rotate_relayer", post(usds_bridge_rotate_relayer_handler))
        .route("/api/v1/usds_bridge/status", get(usds_bridge_status_handler))
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
