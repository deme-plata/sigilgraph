//! sigil-relayer — the automated service that actually connects the SIGIL
//! L1 <-> Polygon bridge. Everything up to this crate (sigil-api's
//! bridge.rs routes, SigilBridgeWrapped.sol on Polygon) is plumbing that
//! sits inert without something watching both chains and acting on what it
//! sees. This is that something.
//!
//! # The two directions
//!
//! **Lock -> Mint**: poll `GET {SIGIL_API_URL}/v1/bridge/locks?since=N` for
//! new locks. For each one, independently check on-chain whether it's
//! already been minted (query past `OperatorMinted` logs filtered by that
//! exact `lockId` — NOT just trusting local state, because
//! `SigilBridgeWrapped.mint()` has no on-chain idempotency guard of its
//! own; a double-mint is a real, silent, unrecoverable bug if this crate
//! ever double-processes a lock after a crash/restart). If not already
//! minted, convert 10-decimal SIGIL base units (glyphs) to 18-decimal Polygon
//! units and call `mint(dest, amount, lockId)` as the Polygon operator key.
//!
//! **`lockId` IS the SIGIL lock transaction hash** (2026-08-27 operator decision, re-landed
//! 2026-09-02 after the first implementation was lost uncommitted). The node's lock ids are
//! an in-memory counter that restarts at 1 on every node restart — and the chain itself was
//! reset g0→g1→g2 — so a small sequential id is neither unique nor stable, and keying the
//! mint dedup on it stranded 1 SIGIL in the vault on 2026-08-27 ("lock 1 already minted,
//! skipping"). The 32-byte tx hash is content-derived: it cannot be reset, re-used or
//! collided, and the contract takes `lockId` as an opaque `uint256` with no on-chain
//! idempotency of its own, so nothing ever required it to be small.
//!
//! **Burn -> Unlock**: poll Polygon for `BurnedTo` events since the last
//! processed block. For each one, convert 18-decimal Polygon units back to
//! 10-decimal SIGIL units (floor division; a nonzero remainder is logged
//! loudly, never silently dropped) and call `POST /v1/bridge/unlock` on
//! SIGIL L1, signed by the SIGIL relayer key. Double-unlock protection is
//! ALREADY enforced server-side here (`bridge.rs::submit_unlock`'s
//! `processed_burns` dedup on `polygon_burn_tx`), so this direction is
//! inherently safer against a crash/restart than the mint direction is —
//! which is exactly why the mint direction gets the extra on-chain check.
//!
//! # Persistence
//!
//! A small JSON state file (`SIGIL_RELAYER_STATE_FILE`) tracks the SIGIL lock tx hashes
//! already minted (`minted_lock_txs` — authoritative, restart-proof, checked before any
//! RPC), an informational lock-id cursor, and the last Polygon block — the block
//! watermark is written after EVERY successfully processed log chunk, not once per pass,
//! so a provider 503 halfway through a long catch-up resumes instead of restarting the
//! crawl (measured 2026-08-27: six minutes of zero progress over a ~5,000-block gap).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::{
    network::EthereumWallet,
    primitives::{Address, FixedBytes, B256, U256},
    providers::{Provider, ProviderBuilder},
    rpc::types::Filter,
    signers::local::PrivateKeySigner,
    sol,
    sol_types::SolEvent,
};
use anyhow::{bail, Context, Result};
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};

sol! {
    #[sol(rpc)]
    interface ISigilBridgeWrapped {
        function mint(address to, uint256 amount, uint256 lockId) external;

        event OperatorMinted(address indexed to, uint256 amount, uint256 indexed lockId);
        event BurnedTo(address indexed from, uint256 amount, bytes32 indexed destSigilAddress);
    }
}

/// SIGIL is 10 decimals (`sigil_state::SIGIL_DECIMALS`); the Polygon wrapped
/// token is a standard 18-decimal ERC20. This is the one conversion factor
/// the whole bridge's accounting depends on getting right in both directions.
/// SIGIL decimals, mirrored here because this binary deliberately does not depend on
/// `sigil-state` (it talks to the chain over HTTP, not by linking it). Mirrored constants
/// rot, so the pair below is checked against the real one by
/// `sigil-relayer`'s `decimals_match_the_chain` test rather than by hope.
const SIGIL_DECIMALS_MIRROR: u32 = 10;
/// Polygon's wrapped token is a standard 18-decimal ERC20.
const WRAPPED_DECIMALS: u32 = 18;
const DECIMAL_SHIFT: u128 = 10u128.pow(WRAPPED_DECIMALS - SIGIL_DECIMALS_MIRROR);

/// `amount` as it arrives on the wire.
///
/// **Why this exists.** `sigil-api`'s `bridge::LockRecord.amount` is a `u128` with
/// a plain `#[derive(Serialize)]`, so `/v1/bridge/locks` emits it as a JSON
/// **number** (`"amount":4947260948`). This side declared it as `String`, so every
/// poll died in serde before a single lock was ever looked at:
/// `locks response was not valid JSON: invalid type: integer 4947260948, expected
/// a string`. The two halves of the bridge disagreed about one field's type, and
/// because that is a whole-RESPONSE parse error the relayer could not even report
/// which lock it was choking on — it just retried the identical failure every 15s,
/// forever, minting nothing.
///
/// **Why a hand-written visitor and NOT `#[serde(untagged)]`.** The obvious
/// two-variant untagged enum compiles and then fails at runtime on the real
/// payload. `untagged` first buffers the input into serde's private `Content`
/// type, and `Content` has no 128-bit variant at all — a JSON integer lands in it
/// as `U64`, and the `u128` arm then refuses to match, giving the useless
/// `data did not match any variant of untagged enum` error. (Caught here by
/// `wire_compat_tests`, not in production, which is the entire reason those tests
/// pin the real captured body.) `deserialize_any` with an explicit visitor skips
/// the buffering entirely and sees the true number, so it is both simpler and
/// strictly more capable.
///
/// Accepting BOTH a number and a decimal string is deliberate rather than just
/// flipping the field to `u128`: a u128 amount genuinely can exceed IEEE-754's
/// exact-integer range (2^53), so a future `serialize_with`-to-string migration on
/// the API side is a real possibility, and a number-only relayer would then break
/// in the very same silent way in the opposite direction. This makes the wire
/// contract permissive in the one place these two crates have already proven they
/// can drift apart.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WireAmount(u128);

impl WireAmount {
    /// Base units, i.e. glyphs (SIGIL is 10dp — `sigil_state::SIGIL_DECIMALS`).
    fn to_base_units(&self) -> Result<u128> {
        Ok(self.0)
    }
}

impl std::fmt::Display for WireAmount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl<'de> Deserialize<'de> for WireAmount {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = WireAmount;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a non-negative integer amount, as a JSON number or a decimal string")
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                Ok(WireAmount(v as u128))
            }
            fn visit_u128<E: serde::de::Error>(self, v: u128) -> std::result::Result<Self::Value, E> {
                Ok(WireAmount(v))
            }
            /// A negative amount is not merely unrepresentable, it is nonsense for a
            /// lock — reject it loudly rather than wrapping into a huge u128 and
            /// minting an absurd quantity of wrapped SIGIL.
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                u128::try_from(v)
                    .map(WireAmount)
                    .map_err(|_| E::custom(format!("negative lock amount {v}")))
            }
            fn visit_i128<E: serde::de::Error>(self, v: i128) -> std::result::Result<Self::Value, E> {
                u128::try_from(v)
                    .map(WireAmount)
                    .map_err(|_| E::custom(format!("negative lock amount {v}")))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<Self::Value, E> {
                v.trim()
                    .parse::<u128>()
                    .map(WireAmount)
                    .map_err(|_| E::custom(format!("lock amount {v:?} is not a valid integer")))
            }
        }
        d.deserialize_any(V)
    }
}

#[derive(Debug, Clone, Deserialize)]
struct LockRecord {
    id: u64,
    from: String,
    amount: WireAmount,
    dest_polygon_address: String,
    tx_hash: String,
    #[allow(dead_code)]
    ts_ms: u64,
    /// Whether every one of this lock's on-chain parts has SETTLED.
    ///
    /// **Minting without checking this creates wrapped SIGIL backed by nothing.** A lock
    /// RECORD exists the moment a request is accepted; it is not evidence that any value
    /// moved. That distinction was not academic: for the whole life of the transparent-
    /// `Send` lock the records accumulated while every transaction was rejected at mint,
    /// the vault balance stayed `0`, and this relayer would have happily minted against
    /// them had it not been failing to parse the feed at the same time. Two bugs cancelling
    /// out is not a safety property.
    ///
    /// `#[serde(default)]` = `false`, so an older API that does not send the field is
    /// treated as UNSETTLED and mints nothing. Fail closed.
    #[serde(default)]
    settled: bool,
}

#[derive(Debug, Deserialize)]
struct LocksResponse {
    ok: bool,
    data: Option<Vec<LockRecord>>,
    error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
struct RelayerState {
    /// Informational only since dedup moved to tx hashes: the highest lock id seen. Never
    /// used to skip anything — see `poll_locks_and_mint`.
    last_lock_id: u64,
    /// Next Polygon block to scan for `BurnedTo`. Advanced per successful CHUNK.
    last_polygon_block: u64,
    /// SIGIL lock tx hashes (hex) whose mint is CONFIRMED on Polygon. The authority for
    /// "already minted"; the on-chain `OperatorMinted` lookback is only the backstop for the
    /// crash window between "receipt received" and "state saved".
    #[serde(default)]
    minted_lock_txs: BTreeSet<String>,
}

/// The `lockId` passed to `mint(to, amount, lockId)`: the SIGIL lock tx hash as a big-endian
/// `uint256`. Same bytes as the `OperatorMinted.lockId` indexed topic, so the on-chain
/// lookback can filter on it directly.
fn lock_key(tx_hash_hex: &str) -> Result<U256> {
    let raw = hex::decode(tx_hash_hex.trim().trim_start_matches("0x"))
        .with_context(|| format!("lock tx hash {tx_hash_hex:?} is not hex"))?;
    let arr: [u8; 32] = raw
        .try_into()
        .map_err(|_| anyhow::anyhow!("lock tx hash {tx_hash_hex:?} is not 32 bytes"))?;
    Ok(U256::from_be_bytes(arr))
}

struct Config {
    sigil_api_url: String,
    polygon_rpc_url: String,
    contract: Address,
    sigil_relayer_keyfile: PathBuf,
    polygon_relayer_keyfile: PathBuf,
    state_file: PathBuf,
    poll_interval: Duration,
    /// Used only when no state file exists yet — avoids scanning the whole
    /// chain for a contract that has no history before this block.
    default_start_block: u64,
    /// `eth_getLogs` range per request. Alchemy's free tier allows 10; a provider without
    /// that cap (publicnode, a self-hosted bor) can take thousands, which turns a
    /// multi-day catch-up from an hour of chunked requests into seconds.
    /// `SIGIL_RELAYER_LOG_CHUNK`, default `LOG_CHUNK_BLOCKS`.
    log_chunk_blocks: u64,
}

impl Config {
    fn from_env() -> Result<Self> {
        Ok(Self {
            sigil_api_url: std::env::var("SIGIL_API_URL").unwrap_or_else(|_| "http://127.0.0.1:18181".into()),
            polygon_rpc_url: std::env::var("POLYGON_RPC_URL").context("POLYGON_RPC_URL must be set")?,
            contract: std::env::var("SIGIL_BRIDGE_CONTRACT").context("SIGIL_BRIDGE_CONTRACT must be set")?.parse()
                .context("SIGIL_BRIDGE_CONTRACT is not a valid address")?,
            sigil_relayer_keyfile: std::env::var("SIGIL_RELAYER_KEYFILE").context("SIGIL_RELAYER_KEYFILE must be set")?.into(),
            polygon_relayer_keyfile: std::env::var("POLYGON_RELAYER_KEYFILE").context("POLYGON_RELAYER_KEYFILE must be set")?.into(),
            state_file: std::env::var("SIGIL_RELAYER_STATE_FILE").unwrap_or_else(|_| "/home/orobit/sigil-bridge-relayer/state.json".into()).into(),
            log_chunk_blocks: std::env::var("SIGIL_RELAYER_LOG_CHUNK")
                .ok()
                .and_then(|s| s.parse().ok())
                .filter(|n: &u64| *n >= 1)
                .unwrap_or(LOG_CHUNK_BLOCKS),
            poll_interval: Duration::from_secs(
                std::env::var("SIGIL_RELAYER_POLL_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(15),
            ),
            default_start_block: std::env::var("SIGIL_BRIDGE_DEPLOY_BLOCK").ok().and_then(|s| s.parse().ok()).unwrap_or(0),
        })
    }
}

fn load_state(path: &PathBuf, default_start_block: u64) -> RelayerState {
    let fresh = || RelayerState { last_polygon_block: default_start_block, ..Default::default() };
    match std::fs::read_to_string(path) {
        // A state file that exists but does not parse is NOT silently replaced: that would
        // forget every minted lock and re-mint all of them on the next pass.
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            panic!("refusing to start: {} exists but does not parse ({e}) — fix or move it", path.display())
        }),
        Err(_) => fresh(),
    }
}

fn save_state(path: &PathBuf, state: &RelayerState) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(state)?)?;
    std::fs::rename(&tmp, path)?; // atomic on the same filesystem — no torn writes
    Ok(())
}

fn read_sigil_key(path: &PathBuf) -> Result<SigningKey> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let hex_str = raw.trim().strip_prefix("0x").unwrap_or(raw.trim());
    let bytes = hex::decode(hex_str).context("SIGIL relayer key is not valid hex")?;
    let arr: [u8; 32] = bytes.try_into().map_err(|_| anyhow::anyhow!("SIGIL relayer key must be 32 bytes"))?;
    Ok(SigningKey::from_bytes(&arr))
}

fn read_polygon_signer(path: &PathBuf) -> Result<PrivateKeySigner> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let hex_str = raw.trim().strip_prefix("0x").unwrap_or(raw.trim());
    hex_str.parse().context("Polygon relayer key is not a valid private key")
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Every signed action against sigil-api uses this exact scheme
/// (`bridge.rs::verify_sig` / `send.rs`): the WalletId IS the raw Ed25519
/// pubkey, and req_nonce must be strictly greater than the last accepted
/// one for that wallet — Date.now()-style monotonic milliseconds, bumped
/// if two calls land in the same millisecond.
struct SigilSigner {
    key: SigningKey,
    address_hex: String,
    last_nonce: u64,
}

impl SigilSigner {
    fn new(key: SigningKey) -> Self {
        let address_hex = hex::encode(key.verifying_key().to_bytes());
        Self { key, address_hex, last_nonce: 0 }
    }

    fn next_nonce(&mut self) -> u64 {
        let n = now_ms().max(self.last_nonce + 1);
        self.last_nonce = n;
        n
    }

    fn sign(&self, msg: &str) -> String {
        hex::encode(self.key.sign(msg.as_bytes()).to_bytes())
    }
}

fn submit_unlock(
    sigil_api_url: &str,
    signer: &mut SigilSigner,
    to_hex: &str,
    amount: u128,
    polygon_burn_tx: &str,
) -> Result<()> {
    let nonce = signer.next_nonce();
    let msg = format!("sigil-rpc/v1|bridge_unlock|{to_hex}|{amount}|{polygon_burn_tx}|nonce={nonce}");
    let sig = signer.sign(&msg);
    let body = serde_json::json!({
        "relayer": signer.address_hex,
        "to": to_hex,
        "amount": amount,
        "polygon_burn_tx": polygon_burn_tx,
        "sig": sig,
        "req_nonce": nonce,
    });
    let resp: serde_json::Value = ureq::post(&format!("{sigil_api_url}/v1/bridge/unlock"))
        .send_json(body)
        .context("POST /v1/bridge/unlock failed")?
        .into_json()
        .context("unlock response was not valid JSON")?;
    if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        bail!("unlock rejected: {}", resp.get("error").and_then(|v| v.as_str()).unwrap_or("unknown"));
    }
    Ok(())
}

fn fetch_locks_since(sigil_api_url: &str, since: u64) -> Result<Vec<LockRecord>> {
    let resp: LocksResponse = ureq::get(&format!("{sigil_api_url}/v1/bridge/locks?since={since}"))
        .call()
        .context("GET /v1/bridge/locks failed")?
        .into_json()
        .context("locks response was not valid JSON")?;
    if !resp.ok {
        bail!("locks fetch rejected: {}", resp.error.unwrap_or_default());
    }
    Ok(resp.data.unwrap_or_default())
}

/// Alchemy's free tier caps `eth_getLogs` to a 10-block range per call
/// (learned the hard way, live, first run — see the commit that added
/// this). Every ranged log query in this crate MUST go through this
/// chunker; a raw `provider.get_logs()` call with a wide range will get a
/// hard 400 from the RPC, not a slow-but-working response.
const LOG_CHUNK_BLOCKS: u64 = 10;
/// A short pause between chunk requests so a wide catch-up range (e.g. a
/// cold start, or resuming after the service was down a while) doesn't
/// also trip a requests-per-second limit on top of the range cap.
const CHUNK_DELAY: Duration = Duration::from_millis(150);

/// How many times one chunk is retried before the whole range gives up.
///
/// **Why this exists.** The provider returns intermittent `503 -32001 "Unable to complete
/// request at this time"` under normal operation. Without a retry, ONE such response
/// anywhere in the range aborts the entire pass — and because the caller only advances its
/// watermark on full success, the next pass restarts the same crawl from the beginning.
/// Over a wide catch-up range that never converges: measured live on 2026-08-27, six
/// minutes of consecutive failed passes with zero progress across a ~5,000-block gap,
/// which blocked both a mint and an unlock until the watermark was moved by hand.
///
/// Retrying the ONE flaky chunk turns a fatal range failure into a pause of a few hundred
/// milliseconds. It is not a substitute for per-chunk checkpointing (a long enough outage
/// still loses the pass), but it removes the failure that actually happens.
const CHUNK_RETRIES: u32 = 4;

/// The `[from, to]` sub-ranges a chunked crawl visits, in order. Pure, so the arithmetic
/// that decides which blocks get scanned is testable without a provider.
fn chunk_bounds(from_block: u64, to_block: u64, chunk: u64) -> Vec<(u64, u64)> {
    let chunk = chunk.max(1);
    let mut out = Vec::new();
    let mut cur = from_block;
    while cur <= to_block {
        let end = cur.saturating_add(chunk - 1).min(to_block);
        out.push((cur, end));
        if end == u64::MAX {
            break;
        }
        cur = end + 1;
    }
    out
}

/// Collecting form of [`get_logs_chunked_with`] for callers that want the whole range.
async fn get_logs_chunked(
    provider: &impl Provider,
    base_filter: &Filter,
    from_block: u64,
    to_block: u64,
    chunk: u64,
) -> Result<Vec<alloy::rpc::types::Log>> {
    let mut all = Vec::new();
    get_logs_chunked_with(provider, base_filter, from_block, to_block, chunk, |_, logs| {
        all.extend(logs);
        Ok(())
    })
    .await?;
    Ok(all)
}

/// Crawl `[from_block, to_block]` in `chunk`-sized `eth_getLogs` calls, handing each chunk's
/// logs to `on_chunk(chunk_end, logs)` as soon as it arrives. The callback is where a caller
/// CHECKPOINTS: if it persists its watermark at `chunk_end + 1`, a failure in a later chunk
/// costs one chunk, not the whole pass.
async fn get_logs_chunked_with(
    provider: &impl Provider,
    base_filter: &Filter,
    from_block: u64,
    to_block: u64,
    chunk: u64,
    mut on_chunk: impl FnMut(u64, Vec<alloy::rpc::types::Log>) -> Result<()>,
) -> Result<()> {
    for (cur, chunk_end) in chunk_bounds(from_block, to_block, chunk) {
        let filter = base_filter.clone().from_block(cur).to_block(chunk_end);

        let mut attempt = 0;
        let logs = loop {
            match provider.get_logs(&filter).await {
                Ok(logs) => break logs,
                Err(e) if attempt < CHUNK_RETRIES => {
                    // Exponential backoff: 250ms, 500ms, 1s, 2s. Long enough to ride out a
                    // rate-limit or a momentary provider hiccup, short enough that a real
                    // outage still surfaces within a few seconds instead of hanging.
                    let backoff = CHUNK_DELAY * 2u32.pow(attempt).saturating_mul(2);
                    eprintln!(
                        "  get_logs [{cur}, {chunk_end}] attempt {} failed ({e}) — retrying in {:?}",
                        attempt + 1,
                        backoff
                    );
                    tokio::time::sleep(backoff).await;
                    attempt += 1;
                }
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!(
                            "get_logs failed for range [{cur}, {chunk_end}] after                              {} attempts",
                            CHUNK_RETRIES + 1
                        )
                    })
                }
            }
        };

        on_chunk(chunk_end, logs)?;
        if chunk_end < to_block {
            tokio::time::sleep(CHUNK_DELAY).await;
        }
    }
    Ok(())
}

/// Bounded lookback for the resume-safety check (see `poll_locks_and_mint`
/// doc comment) — NOT a full contract-history scan. A double-mint can only
/// happen from a crash between "mint tx confirmed" and "state persisted",
/// which is a window of, at most, a few blocks around the crash moment;
/// 1000 blocks (~30 min on Polygon's ~2s blocks) is a generous margin over
/// that, not an attempt to search "since genesis" — which would cost
/// hundreds of chunked requests for no real safety benefit.
const MINT_CHECK_LOOKBACK_BLOCKS: u64 = 1000;

async fn already_minted(provider: &impl Provider, contract: Address, lock_id: U256, chunk: u64) -> Result<bool> {
    let latest = provider.get_block_number().await.context("fetching latest block for the resume-safety check failed")?;
    let from = latest.saturating_sub(MINT_CHECK_LOOKBACK_BLOCKS);
    let lock_id_topic: B256 = B256::from(lock_id);
    let base = Filter::new().address(contract).event_signature(ISigilBridgeWrapped::OperatorMinted::SIGNATURE_HASH).topic2(lock_id_topic);
    let logs = get_logs_chunked(provider, &base, from, latest, chunk).await?;
    Ok(!logs.is_empty())
}

/// `resume_checked` lives in `main()`'s stack, NOT in the persisted state
/// file — it is reset to `false` every process start on purpose. The
/// on-chain `already_minted` check only matters for the FIRST lock this
/// process attempts after starting (the only one that could have been
/// double-attempted by a PRIOR run that crashed between mint-confirm and
/// state-persist); every lock after that, within the same run, has
/// definitely never been attempted before, so re-checking it on-chain
/// every single time would just be hundreds of wasted chunked RPC calls.
async fn poll_locks_and_mint(
    cfg: &Config,
    state: &mut RelayerState,
    resume_checked: &mut bool,
    polygon_provider: &(impl Provider + Clone),
) -> Result<()> {
    // ALWAYS from 0. The node's lock list is small and in-memory, and its ids restart at 1
    // on every node restart, so a `since=<cursor>` fetch would hide every lock made after a
    // restart behind a cursor from before it — the exact way 1 SIGIL got stranded on
    // 2026-08-27. Dedup is on the tx hash (below); the cursor is only reported.
    let mut locks = fetch_locks_since(&cfg.sigil_api_url, 0)?;
    locks.sort_by_key(|l| l.id);
    for lock in locks {
        state.last_lock_id = state.last_lock_id.max(lock.id);
        let tx = lock.tx_hash.trim().trim_start_matches("0x").to_ascii_lowercase();
        if state.minted_lock_txs.contains(&tx) {
            continue;
        }
        // No watermark to strand any more, so an unsettled lock is simply skipped this
        // pass and looked at again next pass, while later settled locks still mint.
        if !lock.settled {
            eprintln!(
                "· lock {} (sigil tx {}) not settled yet ({} glyphs) — waiting, minting nothing",
                lock.id, &tx[..tx.len().min(12)], lock.amount
            );
            continue;
        }
        let amount_base: u128 = lock.amount.to_base_units()?;
        let dest: Address = lock.dest_polygon_address.parse()
            .with_context(|| format!("lock {} has an invalid dest_polygon_address {}", lock.id, lock.dest_polygon_address))?;
        let polygon_amount = U256::from(amount_base) * U256::from(DECIMAL_SHIFT);
        let key = lock_key(&tx)?;

        // Backstop for the crash window between "mint receipt" and "state saved": the first
        // candidate after a process start is checked on-chain. Everything after it within
        // this run has provably never been attempted.
        if !*resume_checked {
            *resume_checked = true;
            if already_minted(polygon_provider, cfg.contract, key, cfg.log_chunk_blocks).await? {
                eprintln!("- lock {} (sigil tx {}) already minted on-chain (resumed after a restart) — recording, skipping", lock.id, &tx[..12]);
                state.minted_lock_txs.insert(tx.clone());
                save_state(&cfg.state_file, state)?;
                continue;
            }
        }

        eprintln!("+ lock {} from {} amount={} glyphs -> mint {polygon_amount} to {dest} (lockId = sigil tx {})", lock.id, lock.from, lock.amount, tx);
        let c = ISigilBridgeWrapped::new(cfg.contract, polygon_provider.clone());
        let pending = c.mint(dest, polygon_amount, key).send().await
            .with_context(|| format!("mint tx failed to send for lock {} (sigil tx {tx})", lock.id))?;
        let receipt = pending.get_receipt().await.with_context(|| format!("mint tx failed to confirm for lock {} (sigil tx {tx})", lock.id))?;
        eprintln!("  minted: tx={:?} status={}", receipt.transaction_hash, receipt.status());
        if !receipt.status() {
            // A reverted mint is NOT minted: leave the hash out of the set so it is retried,
            // and stop the pass so the operator sees the failure at the top of the log.
            bail!("mint for lock {} (sigil tx {tx}) REVERTED on Polygon: {:?}", lock.id, receipt.transaction_hash);
        }

        state.minted_lock_txs.insert(tx);
        save_state(&cfg.state_file, state)?;
    }
    Ok(())
}

async fn poll_burns_and_unlock(
    cfg: &Config,
    state: &mut RelayerState,
    sigil_signer: &mut SigilSigner,
    polygon_provider: &impl Provider,
) -> Result<()> {
    let latest = polygon_provider.get_block_number().await.context("fetching latest Polygon block failed")?;
    if latest < state.last_polygon_block {
        return Ok(()); // reorg-ish edge case; just wait for the chain to move forward again
    }
    let base = Filter::new().address(cfg.contract).event_signature(ISigilBridgeWrapped::BurnedTo::SIGNATURE_HASH);
    let from = state.last_polygon_block;
    let state_file = cfg.state_file.clone();
    let api = cfg.sigil_api_url.clone();
    // PER-CHUNK CHECKPOINT. Each chunk's burns are unlocked and THEN the watermark moves past
    // that chunk and is persisted. A 503 in chunk N+1 costs chunk N+1, not the crawl since
    // the last pass. Re-running a chunk after a mid-chunk failure is safe: the node dedups
    // unlocks on `polygon_burn_tx` (`bridge.rs::submit_unlock`), so an already-unlocked burn
    // is refused there, not paid twice.
    get_logs_chunked_with(polygon_provider, &base, from, latest, cfg.log_chunk_blocks, |chunk_end, logs| {
        for log in logs {
            let burn_tx = match log.transaction_hash {
                Some(h) => format!("{h:#x}"),
                None => continue,
            };
            let decoded = log.log_decode::<ISigilBridgeWrapped::BurnedTo>().context("failed to decode BurnedTo log")?;
            let ev = decoded.inner.data;
            let amount = ev.amount;
            let dest: FixedBytes<32> = ev.destSigilAddress;
            let dest_hex = hex::encode(dest.0);

            let sigil_amount = amount / U256::from(DECIMAL_SHIFT);
            let remainder = amount % U256::from(DECIMAL_SHIFT);
            if remainder != U256::ZERO {
                eprintln!("! burn {burn_tx}: amount {amount} is not a clean multiple of {DECIMAL_SHIFT} — {remainder} base units of dust cannot be unlocked, floor applied");
            }
            if sigil_amount == U256::ZERO {
                eprintln!("! burn {burn_tx}: rounds to 0 SIGIL after conversion — skipping, nothing to unlock");
                continue;
            }

            eprintln!("+ burn {burn_tx} amount={amount} -> unlock {sigil_amount} glyphs to {dest_hex}");
            let sigil_amount_u128: u128 = sigil_amount.try_into().context("unlock amount overflowed u128")?;
            match submit_unlock(&api, sigil_signer, &dest_hex, sigil_amount_u128, &burn_tx) {
                Ok(()) => eprintln!("  unlocked on SIGIL L1"),
                // The node's own dedup: this burn was paid in an earlier (checkpoint-lost) run.
                Err(e) if format!("{e:#}").contains("already") => {
                    eprintln!("  already unlocked on SIGIL L1 (node dedup) — continuing");
                }
                Err(e) => return Err(e).with_context(|| format!("unlock failed for burn {burn_tx}")),
            }
        }
        state.last_polygon_block = chunk_end + 1;
        save_state(&state_file, state)
    })
    .await
    .context("querying BurnedTo logs failed")?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Config::from_env()?;
    let mut state = load_state(&cfg.state_file, cfg.default_start_block);
    eprintln!(
        "sigil-relayer starting — {} lock tx(s) already minted, polygon_block={}, log chunk={} blocks, {} glyphs = 1 wSIGIL unit shift 10^{}",
        state.minted_lock_txs.len(), state.last_polygon_block, cfg.log_chunk_blocks, DECIMAL_SHIFT, WRAPPED_DECIMALS - SIGIL_DECIMALS_MIRROR
    );

    let sigil_key = read_sigil_key(&cfg.sigil_relayer_keyfile)?;
    let mut sigil_signer = SigilSigner::new(sigil_key);
    eprintln!("  SIGIL relayer address: {}", sigil_signer.address_hex);

    let polygon_signer = read_polygon_signer(&cfg.polygon_relayer_keyfile)?;
    let polygon_address = polygon_signer.address();
    eprintln!("  Polygon relayer address: {polygon_address}");

    let wallet = EthereumWallet::from(polygon_signer);
    let polygon_provider = ProviderBuilder::new().wallet(wallet).connect_http(cfg.polygon_rpc_url.parse()?);
    let mut resume_checked = false;

    loop {
        if let Err(e) = poll_locks_and_mint(&cfg, &mut state, &mut resume_checked, &polygon_provider).await {
            eprintln!("! lock->mint pass failed: {e:#}");
        }
        if let Err(e) = poll_burns_and_unlock(&cfg, &mut state, &mut sigil_signer, &polygon_provider).await {
            eprintln!("! burn->unlock pass failed: {e:#}");
        }
        tokio::time::sleep(cfg.poll_interval).await;
    }
}

#[cfg(test)]
mod wire_compat_tests {
    use super::*;

    /// The EXACT body `/v1/bridge/locks` served on 2026-08-26 for the first real
    /// user lock, captured live from `http://127.0.0.1:18181`. Pinning the real
    /// bytes (not a hand-written approximation) is the whole point: the outage
    /// this test exists for was a disagreement about one field's wire type, and
    /// only the genuine payload can prove the two halves agree.
    const LIVE_LOCKS_BODY: &str = r#"{"ok":true,"data":[{"id":1,"from":"7cdd2a7df916518b3ca3bb71497daec7ac17642bc7d35ea16af2daf006f665aa","amount":4947260948,"dest_polygon_address":"0xd7cab8075188df9a50dc494e9bb827f96df93936","tx_hash":"806884e5f464a8d7c631dcc39558c5a4d99d1ce3661c367d15a7f1d9f8d02793","ts_ms":1787772707778}],"ts":1787772972509}"#;

    /// Guards the regression directly: for 5 months this parse failed, so the
    /// relayer never saw a single lock and no wrapped SIGIL was ever minted.
    ///
    /// This test already earned its keep once: the first attempt at the fix used
    /// `#[serde(untagged)]`, which compiles fine and then fails on exactly this
    /// payload (serde's buffering `Content` type has no 128-bit variant, so the
    /// integer arrives as `U64` and the `u128` arm never matches). That would have
    /// shipped one silent-blindness bug in place of another. It also guards the
    /// `serde_json/arbitrary_precision` hazard: the sigil WORKSPACE enables that
    /// feature and `sigil-relayer` escapes it only by declaring its own plain
    /// `serde_json = "1"`. If either ever changes, this fails at build time rather
    /// than in production.
    #[test]
    fn parses_the_json_number_amount_the_api_actually_sends() {
        let resp: LocksResponse =
            serde_json::from_str(LIVE_LOCKS_BODY).expect("live /v1/bridge/locks body must parse");
        assert!(resp.ok);
        let locks = resp.data.expect("data present");
        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].id, 1);
        assert_eq!(
            locks[0].amount.to_base_units().unwrap(),
            4_947_260_948u128,
            "49.47260948 SIGIL at 8dp"
        );
    }

    /// Forward-compat: a string-serialized u128 (what a future `serialize_with`
    /// migration on the API side would emit, and the shape this crate WRONGLY
    /// assumed was already in use) must parse to the identical value.
    #[test]
    fn also_parses_a_string_amount_so_a_future_api_migration_cannot_break_it() {
        let body = LIVE_LOCKS_BODY.replace(
            r#""amount":4947260948"#,
            r#""amount":"4947260948""#,
        );
        let resp: LocksResponse = serde_json::from_str(&body).expect("string form must parse");
        let locks = resp.data.expect("data present");
        assert_eq!(locks[0].amount.to_base_units().unwrap(), 4_947_260_948u128);
    }

    /// The conversion the mint depends on: SIGIL is 10dp (g2), wrapped SIGIL is 18dp, so
    /// the shift is 10^8. Getting this wrong mints 100x too much or too little — the
    /// pre-g2 binary (built for 8dp) would have minted 100 wSIGIL for a 1 SIGIL lock.
    #[test]
    fn decimal_shift_maps_base_units_to_wrapped_units() {
        assert_eq!(DECIMAL_SHIFT, 100_000_000);
        let one_sigil_glyphs: u128 = 10_000_000_000; // 1 SIGIL at 10dp
        assert_eq!(one_sigil_glyphs * DECIMAL_SHIFT, 1_000_000_000_000_000_000u128, "1 SIGIL -> 1e18 wei");
    }

    /// The lock id IS the tx hash: same bytes as the `OperatorMinted.lockId` topic, so a
    /// resume check can filter on it; and a hash that is not 32 bytes is refused, never
    /// truncated into a colliding id.
    #[test]
    fn lock_key_is_the_tx_hash_and_refuses_junk() {
        let h = "9e9889972b1552b89f43d8b2c1b4adf0a21d861a5a877a524225c1df13995b34";
        let k = lock_key(h).unwrap();
        assert_eq!(B256::from(k), B256::from_slice(&hex::decode(h).unwrap()));
        assert_eq!(lock_key(&format!("0x{h}")).unwrap(), k, "0x prefix tolerated");
        assert!(lock_key(&h[..60]).is_err());
        assert!(lock_key("zz").is_err());
    }

    /// The live state file as deployed on 2026-08-27 (`minted_lock_txs` present) AND the
    /// older two-field shape must both load — forgetting the set would re-mint every lock.
    #[test]
    fn state_file_round_trips_with_and_without_the_minted_set() {
        let live = r#"{"last_lock_id": 2, "last_polygon_block": 92719557, "minted_lock_txs": ["9e9889972b1552b89f43d8b2c1b4adf0a21d861a5a877a524225c1df13995b34", "2f4b0a9edc113a94535e589495e792603e53c8e0490c3dd568a3570dc59bc05a"]}"#;
        let st: RelayerState = serde_json::from_str(live).unwrap();
        assert_eq!(st.minted_lock_txs.len(), 2);
        assert!(st.minted_lock_txs.contains("9e9889972b1552b89f43d8b2c1b4adf0a21d861a5a877a524225c1df13995b34"));
        let old: RelayerState = serde_json::from_str(r#"{"last_lock_id":0,"last_polygon_block":1}"#).unwrap();
        assert!(old.minted_lock_txs.is_empty());
        let back: RelayerState = serde_json::from_str(&serde_json::to_string(&st).unwrap()).unwrap();
        assert_eq!(back.minted_lock_txs, st.minted_lock_txs);
    }

    /// Chunk arithmetic: contiguous, inclusive, never past `to`, never a zero-width loop —
    /// the per-chunk checkpoint (`chunk_end + 1`) relies on exactly this.
    #[test]
    fn chunk_bounds_cover_the_range_exactly_once() {
        assert_eq!(chunk_bounds(100, 125, 10), vec![(100, 109), (110, 119), (120, 125)]);
        assert_eq!(chunk_bounds(5, 5, 10), vec![(5, 5)]);
        assert!(chunk_bounds(6, 5, 10).is_empty());
        assert_eq!(chunk_bounds(0, 2, 0), vec![(0, 0), (1, 1), (2, 2)], "chunk 0 is treated as 1");
        let b = chunk_bounds(92_719_557, 92_719_557 + 25_000, 2_000);
        assert_eq!(b.len(), 13);
        assert_eq!(b.last().unwrap().1, 92_719_557 + 25_000);
    }
}
