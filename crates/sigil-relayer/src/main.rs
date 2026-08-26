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
//! minted, convert 8-decimal SIGIL base units to 18-decimal Polygon units
//! and call `mint(dest, amount, lockId)` as the Polygon operator key.
//!
//! **Burn -> Unlock**: poll Polygon for `BurnedTo` events since the last
//! processed block. For each one, convert 18-decimal Polygon units back to
//! 8-decimal SIGIL units (floor division; a nonzero remainder is logged
//! loudly, never silently dropped) and call `POST /v1/bridge/unlock` on
//! SIGIL L1, signed by the SIGIL relayer key. Double-unlock protection is
//! ALREADY enforced server-side here (`bridge.rs::submit_unlock`'s
//! `processed_burns` dedup on `polygon_burn_tx`), so this direction is
//! inherently safer against a crash/restart than the mint direction is —
//! which is exactly why the mint direction gets the extra on-chain check.
//!
//! # Persistence
//!
//! A small JSON state file (`SIGIL_RELAYER_STATE_FILE`) tracks the last
//! processed lock id and Polygon block, written durably after each
//! successful action — so a restart resumes roughly where it left off
//! without needing to re-scan from genesis. It is a resume optimization,
//! NOT the safety mechanism (see above: the real safety nets are the
//! on-chain OperatorMinted check and bridge.rs's processed_burns set).

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

/// SIGIL is 8 decimals (`sigil_state::SIGIL_DECIMALS`); the Polygon wrapped
/// token is a standard 18-decimal ERC20. This is the one conversion factor
/// the whole bridge's accounting depends on getting right in both directions.
const DECIMAL_SHIFT: u128 = 10u128.pow(10);

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
    /// Base units (SIGIL is 8dp — `sigil_state::SIGIL_DECIMALS`).
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

#[derive(Debug, Serialize, Deserialize, Clone)]
struct RelayerState {
    last_lock_id: u64,
    last_polygon_block: u64,
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
            poll_interval: Duration::from_secs(
                std::env::var("SIGIL_RELAYER_POLL_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(15),
            ),
            default_start_block: std::env::var("SIGIL_BRIDGE_DEPLOY_BLOCK").ok().and_then(|s| s.parse().ok()).unwrap_or(0),
        })
    }
}

fn load_state(path: &PathBuf, default_start_block: u64) -> RelayerState {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or(RelayerState { last_lock_id: 0, last_polygon_block: default_start_block }),
        Err(_) => RelayerState { last_lock_id: 0, last_polygon_block: default_start_block },
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

async fn get_logs_chunked(
    provider: &impl Provider,
    base_filter: &Filter,
    from_block: u64,
    to_block: u64,
) -> Result<Vec<alloy::rpc::types::Log>> {
    let mut all = Vec::new();
    let mut cur = from_block;
    while cur <= to_block {
        let chunk_end = (cur + LOG_CHUNK_BLOCKS - 1).min(to_block);
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

        all.extend(logs);
        cur = chunk_end + 1;
        if cur <= to_block {
            tokio::time::sleep(CHUNK_DELAY).await;
        }
    }
    Ok(all)
}

/// Bounded lookback for the resume-safety check (see `poll_locks_and_mint`
/// doc comment) — NOT a full contract-history scan. A double-mint can only
/// happen from a crash between "mint tx confirmed" and "state persisted",
/// which is a window of, at most, a few blocks around the crash moment;
/// 1000 blocks (~30 min on Polygon's ~2s blocks) is a generous margin over
/// that, not an attempt to search "since genesis" — which would cost
/// hundreds of chunked requests for no real safety benefit.
const MINT_CHECK_LOOKBACK_BLOCKS: u64 = 1000;

async fn already_minted(provider: &impl Provider, contract: Address, lock_id: u64) -> Result<bool> {
    let latest = provider.get_block_number().await.context("fetching latest block for the resume-safety check failed")?;
    let from = latest.saturating_sub(MINT_CHECK_LOOKBACK_BLOCKS);
    let lock_id_topic: B256 = B256::from(U256::from(lock_id));
    let base = Filter::new().address(contract).event_signature(ISigilBridgeWrapped::OperatorMinted::SIGNATURE_HASH).topic2(lock_id_topic);
    let logs = get_logs_chunked(provider, &base, from, latest).await?;
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
    let locks = fetch_locks_since(&cfg.sigil_api_url, state.last_lock_id)?;
    for lock in locks {
        // Locks are sequential and the watermark is a single id, so an unsettled lock must
        // STOP the pass rather than be skipped — skipping it would advance the watermark
        // past it and strand it forever once it does settle.
        if !lock.settled {
            eprintln!(
                "· lock {} not settled yet ({} SIGIL base units) — waiting, minting nothing",
                lock.id, lock.amount
            );
            break;
        }
        let amount_base: u128 = lock.amount.to_base_units()?;
        let dest: Address = lock.dest_polygon_address.parse()
            .with_context(|| format!("lock {} has an invalid dest_polygon_address {}", lock.id, lock.dest_polygon_address))?;
        let polygon_amount = U256::from(amount_base) * U256::from(DECIMAL_SHIFT);

        if !*resume_checked {
            if already_minted(polygon_provider, cfg.contract, lock.id).await? {
                eprintln!("- lock {} already minted on-chain (resumed after a restart) — skipping, advancing watermark", lock.id);
                state.last_lock_id = lock.id;
                save_state(&cfg.state_file, state)?;
                *resume_checked = true;
                continue;
            }
            *resume_checked = true;
        }

        eprintln!("+ lock {} from {} amount={} -> mint {polygon_amount} to {dest} (sigil tx {})", lock.id, lock.from, lock.amount, lock.tx_hash);
        let c = ISigilBridgeWrapped::new(cfg.contract, polygon_provider.clone());
        let pending = c.mint(dest, polygon_amount, U256::from(lock.id)).send().await
            .with_context(|| format!("mint tx failed to send for lock {}", lock.id))?;
        let receipt = pending.get_receipt().await.with_context(|| format!("mint tx failed to confirm for lock {}", lock.id))?;
        eprintln!("  minted: tx={:?} status={}", receipt.transaction_hash, receipt.status());

        state.last_lock_id = lock.id;
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
    let logs = get_logs_chunked(polygon_provider, &base, state.last_polygon_block, latest).await
        .context("querying BurnedTo logs failed")?;

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
            eprintln!("! burn {burn_tx}: amount {amount} is not a clean multiple of {DECIMAL_SHIFT} — {remainder} base units of dust cannot be unlocked, flooring to {sigil_amount}");
        }
        if sigil_amount == U256::ZERO {
            eprintln!("! burn {burn_tx}: rounds to 0 SIGIL after conversion — skipping, nothing to unlock");
            continue;
        }

        eprintln!("+ burn {burn_tx} amount={amount} -> unlock {sigil_amount} to {dest_hex}");
        let sigil_amount_u128: u128 = sigil_amount.try_into().context("unlock amount overflowed u128")?;
        submit_unlock(&cfg.sigil_api_url, sigil_signer, &dest_hex, sigil_amount_u128, &burn_tx)
            .with_context(|| format!("unlock failed for burn {burn_tx}"))?;
        eprintln!("  unlocked on SIGIL L1");
    }

    state.last_polygon_block = latest + 1;
    save_state(&cfg.state_file, state)?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Config::from_env()?;
    let mut state = load_state(&cfg.state_file, cfg.default_start_block);
    eprintln!("sigil-relayer starting — resuming from lock_id={} polygon_block={}", state.last_lock_id, state.last_polygon_block);

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

    /// The conversion the mint depends on: SIGIL is 8dp, wrapped SIGIL is 18dp.
    /// Getting this wrong mints 10^10x too much or too little.
    #[test]
    fn decimal_shift_maps_base_units_to_wrapped_units() {
        let base = WireAmount(4_947_260_948).to_base_units().unwrap();
        assert_eq!(base * DECIMAL_SHIFT, 49_472_609_480_000_000_000u128);
    }
}
