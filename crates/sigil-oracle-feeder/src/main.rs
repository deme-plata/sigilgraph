//! sigil-oracle-feeder — keeps the SIGIL/USD price on the chain fresh.
//!
//! USDS mints and redeems at the price committed in `sigil_oracle::PRICE_SLOT`,
//! and `sigil-usds` REFUSES to use a price older than `MAX_PRICE_AGE_BLOCKS`
//! (~1.7 h). So the stablecoin is only as live as this process: if it dies the
//! peg does not drift, it FREEZES — which is the right failure. This binary is
//! the thing that has to keep running.
//!
//! # Where the price comes from, honestly
//!
//! The only market SIGIL trades on is the wSIGIL3/USDC Uniswap V2 pair on
//! Polygon. On 2026-09-11 that pool held **$0.001 of USDC** — a number anyone
//! can move 10× for cents. Feeding that spot straight into a mint would let
//! someone lock SIGIL at a manufactured price and walk away with USDS worth
//! more than the collateral. So the feeder does NOT trust the pool below a
//! liquidity floor (`SIGIL_ORACLE_MIN_QUOTE_USD`, default $50 of USDC reserve):
//! under the floor it pushes the operator-set **reference price**
//! (`SIGIL_ORACLE_REFERENCE_USD_E8`) and says so on every push. Above the
//! floor it uses the pool spot, but bounded: at most `SIGIL_ORACLE_MAX_STEP_BPS`
//! (default 5 %) away from the last COMMITTED price per push, so a flash move
//! becomes a slow walk that an operator can see and stop.
//!
//! Neither of those is a decentralised oracle. They are the honest version of
//! "one operator-controlled feed with a speed limit", which is what a chain
//! with one $0.001 pool actually has. Say that out loud when you describe it.
//!
//! # Push policy
//!
//! Every `SIGIL_ORACLE_INTERVAL_SECS` (default 600) the feeder computes a
//! candidate and pushes when (a) no price is committed yet, (b) the candidate
//! differs from the committed price by ≥ `SIGIL_ORACLE_MOVE_BPS` (0.5 %), or
//! (c) the committed stamp is older than `SIGIL_ORACLE_REFRESH_BLOCKS`
//! (default 20,000 = half the staleness window), so a flat market still gets a
//! heartbeat push well inside the window.
//!
//! It only pushes once the chain reports USDS live AND names this wallet as the
//! delegated feeder (`/v1/usds/status`.feeder). Until the master signs that
//! delegation it logs its own address and waits — it cannot and should not
//! push as anyone else.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::{
    primitives::{Address, U256},
    providers::{Provider, ProviderBuilder},
    sol,
};
use anyhow::{bail, Context, Result};
use ed25519_dalek::{Signer, SigningKey};
use serde::Deserialize;

sol! {
    #[sol(rpc)]
    interface IUniswapV2Pair {
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
        function token0() external view returns (address);
        function token1() external view returns (address);
    }
}

/// Polygon native USDC (6 dp) — the quote asset of the wSIGIL3 pair.
const USDC_POLYGON: &str = "0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359";
/// The wSIGIL3/USDC Uniswap V2 pair (deployed 2026-09-06).
const DEFAULT_PAIR: &str = "0x7e9C5d73104Ed2fC2bc55139ac0999036EAf41d7";

/// `sigil_oracle::PRICE_SCALE` mirrored (this binary does not link the chain).
const PRICE_SCALE: u128 = 100_000_000;
const USDC_DECIMALS: u32 = 6;
const WSIGIL_DECIMALS: u32 = 18;

struct Config {
    sigil_api_url: String,
    polygon_rpc_url: String,
    pair: Address,
    keyfile: PathBuf,
    interval: Duration,
    /// Below this many USDC (whole dollars) in the pool, do not trust it.
    min_quote_usd: f64,
    /// Operator-set price used while the pool is under the liquidity floor.
    reference_usd_e8: u128,
    max_step_bps: u128,
    move_bps: u128,
    refresh_blocks: u64,
    once: bool,
    dry_run: bool,
}

impl Config {
    fn from_env(once: bool, dry_run: bool) -> Result<Self> {
        let env_u = |k: &str, d: u128| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
        Ok(Self {
            sigil_api_url: std::env::var("SIGIL_API_URL").unwrap_or_else(|_| "http://127.0.0.1:18181".into()),
            polygon_rpc_url: std::env::var("POLYGON_RPC_URL").unwrap_or_else(|_| "https://polygon-bor-rpc.publicnode.com".into()),
            pair: std::env::var("SIGIL_ORACLE_PAIR").unwrap_or_else(|_| DEFAULT_PAIR.into()).parse().context("SIGIL_ORACLE_PAIR is not an address")?,
            keyfile: std::env::var("SIGIL_ORACLE_FEEDER_KEYFILE").unwrap_or_else(|_| "/root/.config/sigil/oracle-feeder.seed".into()).into(),
            interval: Duration::from_secs(env_u("SIGIL_ORACLE_INTERVAL_SECS", 600) as u64),
            min_quote_usd: std::env::var("SIGIL_ORACLE_MIN_QUOTE_USD").ok().and_then(|v| v.parse().ok()).unwrap_or(50.0),
            reference_usd_e8: env_u("SIGIL_ORACLE_REFERENCE_USD_E8", 100_000), // $0.001
            max_step_bps: env_u("SIGIL_ORACLE_MAX_STEP_BPS", 500),
            move_bps: env_u("SIGIL_ORACLE_MOVE_BPS", 50),
            refresh_blocks: env_u("SIGIL_ORACLE_REFRESH_BLOCKS", 20_000) as u64,
            once,
            dry_run,
        })
    }
}

#[derive(Debug, Deserialize)]
struct UsdsStatus {
    price: String,
    price_height: u64,
    #[allow(dead_code)]
    price_fresh: bool,
    feeder: String,
    height: u64,
    live: bool,
    live_height: u64,
}

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    ok: bool,
    data: Option<T>,
    error: Option<String>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Read or create the feeder seed. A fresh seed is written 0600 on first run so
/// the operator can read the address off the log and delegate to it.
fn load_or_create_key(path: &PathBuf) -> Result<SigningKey> {
    if let Ok(raw) = std::fs::read_to_string(path) {
        let h = raw.trim().strip_prefix("0x").unwrap_or(raw.trim());
        let b = hex::decode(h).context("feeder seed is not hex")?;
        let arr: [u8; 32] = b.try_into().map_err(|_| anyhow::anyhow!("feeder seed must be 32 bytes"))?;
        return Ok(SigningKey::from_bytes(&arr));
    }
    let sk = SigningKey::generate(&mut rand::rngs::OsRng);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, hex::encode(sk.to_bytes()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    eprintln!("🔑 generated a NEW feeder seed at {} (chmod 600)", path.display());
    Ok(sk)
}

fn fetch_status(api: &str) -> Result<UsdsStatus> {
    let env: Envelope<UsdsStatus> = ureq::get(&format!("{api}/v1/usds/status"))
        .call()
        .context("GET /v1/usds/status failed")?
        .into_json()
        .context("usds status was not valid JSON")?;
    if !env.ok {
        bail!("usds status rejected: {}", env.error.unwrap_or_default());
    }
    env.data.context("usds status had no data")
}

/// Spot price from the pair reserves as USD×1e8 per whole SIGIL, plus the
/// USDC reserve in whole dollars. Handles either token order — the
/// USDC-vs-wSIGIL reversal is a 10^27 error if guessed (memory 2026-08-28).
fn spot_from_reserves(reserve_usdc: U256, reserve_wsigil: U256) -> Option<u128> {
    if reserve_wsigil.is_zero() {
        return None;
    }
    // price_e8 = (usdc / 1e6) / (wsigil / 1e18) × 1e8 = usdc × 1e20 / wsigil
    let scale = U256::from(10u128.pow(WSIGIL_DECIMALS - USDC_DECIMALS)) * U256::from(PRICE_SCALE);
    let p = reserve_usdc.checked_mul(scale)? / reserve_wsigil;
    p.try_into().ok()
}

fn usdc_reserve_usd(reserve_usdc: U256) -> f64 {
    let v: u128 = reserve_usdc.try_into().unwrap_or(u128::MAX);
    v as f64 / 10f64.powi(USDC_DECIMALS as i32)
}

/// Bound `candidate` to ±max_step_bps of `last` (no bound when nothing is
/// committed yet). Returns the bounded price and whether it was clamped.
fn clamp_step(candidate: u128, last: u128, max_step_bps: u128) -> (u128, bool) {
    if last == 0 {
        return (candidate, false);
    }
    let up = last + last * max_step_bps / 10_000;
    // The floor is never below 1: a zero price is "no oracle" on the chain
    // (OraclePush refuses it), so a walk down can approach zero but not push it.
    let down = (last - last * max_step_bps / 10_000).max(1);
    if candidate > up {
        (up, true)
    } else if candidate < down {
        (down, true)
    } else {
        (candidate, false)
    }
}

/// Basis-point distance between two prices, relative to `last`.
fn move_bps(candidate: u128, last: u128) -> u128 {
    if last == 0 {
        return u128::MAX;
    }
    let diff = candidate.abs_diff(last);
    diff * 10_000 / last
}

struct Decision {
    price: u128,
    source: &'static str,
    clamped: bool,
    push: bool,
    reason: String,
}

/// The pure push policy — testable without a chain or an RPC.
fn decide(cfg: &Config, st: &UsdsStatus, pool: Option<(u128, f64)>) -> Decision {
    let last: u128 = st.price.parse().unwrap_or(0);
    let (raw, source) = match pool {
        Some((spot, usd)) if usd >= cfg.min_quote_usd => (spot, "pool"),
        Some((_, usd)) => {
            eprintln!(
                "⚠ pool has only ${usd:.4} of USDC (< ${:.2} floor) — a number anyone can move for cents. Using the operator reference ${:.6}/SIGIL instead.",
                cfg.min_quote_usd,
                cfg.reference_usd_e8 as f64 / PRICE_SCALE as f64
            );
            (cfg.reference_usd_e8, "reference(thin pool)")
        }
        None => (cfg.reference_usd_e8, "reference(no pool read)"),
    };
    let (price, clamped) = clamp_step(raw, last, cfg.max_step_bps);
    let age = st.height.saturating_sub(st.price_height);
    let moved = move_bps(price, last);
    let (push, reason) = if last == 0 {
        (true, "no committed price".to_string())
    } else if moved >= cfg.move_bps {
        (true, format!("moved {moved} bps ≥ {} bps", cfg.move_bps))
    } else if st.price_height == 0 || age >= cfg.refresh_blocks {
        (true, format!("stamp age {age} blocks ≥ {} refresh", cfg.refresh_blocks))
    } else {
        (false, format!("unchanged ({moved} bps), stamp age {age} blocks"))
    };
    Decision { price, source, clamped, push, reason }
}

fn push_price(cfg: &Config, sk: &SigningKey, feeder_hex: &str, price: u128, nonce: u64) -> Result<String> {
    let msg = format!("sigil-rpc/v1|oracle_push|{feeder_hex}|{price}|0|nonce={nonce}");
    let sig = hex::encode(sk.sign(msg.as_bytes()).to_bytes());
    let body = serde_json::json!({
        "authority": feeder_hex,
        "price_usd_e8": price,
        "fee": 0,
        "sig": sig,
        "req_nonce": nonce,
    });
    let resp: serde_json::Value = ureq::post(&format!("{}/v1/nation/oracle/push_wallet", cfg.sigil_api_url))
        .send_json(body)
        .context("POST /v1/nation/oracle/push_wallet failed")?
        .into_json()
        .context("push response was not valid JSON")?;
    if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        bail!("push rejected: {}", resp.get("error").and_then(|v| v.as_str()).unwrap_or("unknown"));
    }
    Ok(resp.get("txid").and_then(|v| v.as_str()).unwrap_or("").to_string())
}

async fn read_pool(provider: &impl Provider, pair: Address) -> Result<(u128, f64)> {
    let c = IUniswapV2Pair::new(pair, provider);
    let t0 = c.token0().call().await.context("token0() failed")?;
    let usdc: Address = USDC_POLYGON.parse().unwrap();
    let r = c.getReserves().call().await.context("getReserves() failed")?;
    let (r_usdc, r_ws) = if t0 == usdc {
        (U256::from(r.reserve0), U256::from(r.reserve1))
    } else {
        let t1 = c.token1().call().await.context("token1() failed")?;
        if t1 != usdc {
            bail!("pair {pair} has no USDC side (token0={t0}, token1={t1})");
        }
        (U256::from(r.reserve1), U256::from(r.reserve0))
    };
    let spot = spot_from_reserves(r_usdc, r_ws).context("pool has no wSIGIL reserve")?;
    Ok((spot, usdc_reserve_usd(r_usdc)))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let once = args.iter().any(|a| a == "--once");
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let cfg = Config::from_env(once, dry_run)?;
    let sk = load_or_create_key(&cfg.keyfile)?;
    let feeder_hex = hex::encode(sk.verifying_key().to_bytes());

    if args.iter().any(|a| a == "address") {
        println!("{feeder_hex}");
        return Ok(());
    }
    if args.iter().any(|a| a == "pool") {
        // Read the pair once and print what the policy would do with it — no chain, no push.
        let provider = ProviderBuilder::new().connect_http(cfg.polygon_rpc_url.parse()?);
        let (spot, usd) = read_pool(&provider, cfg.pair).await?;
        println!("pair {} spot ${:.6}/SIGIL ({} e8) usdc_reserve ${:.6} trusted={}",
            cfg.pair, spot as f64 / PRICE_SCALE as f64, spot, usd, usd >= cfg.min_quote_usd);
        return Ok(());
    }

    eprintln!(
        "sigil-oracle-feeder — feeder wallet {feeder_hex}\n  api={} pair={} floor=${:.2} reference=${:.6} max_step={}bps move={}bps refresh={}blk interval={:?}{}{}",
        cfg.sigil_api_url, cfg.pair, cfg.min_quote_usd, cfg.reference_usd_e8 as f64 / PRICE_SCALE as f64,
        cfg.max_step_bps, cfg.move_bps, cfg.refresh_blocks, cfg.interval,
        if cfg.once { " [once]" } else { "" }, if cfg.dry_run { " [dry-run]" } else { "" }
    );

    let provider = ProviderBuilder::new().connect_http(cfg.polygon_rpc_url.parse()?);
    let mut last_nonce: u64 = 0;

    loop {
        match fetch_status(&cfg.sigil_api_url) {
            Err(e) => eprintln!("! status: {e:#}"),
            Ok(st) => {
                if !st.live {
                    eprintln!("· USDS not live yet: height {} < activation {} — waiting", st.height, st.live_height);
                } else if st.feeder != feeder_hex {
                    eprintln!(
                        "· not delegated: chain feeder is {:?}, I am {feeder_hex}. The master must sign OracleDelegate (POST /v1/nation/oracle/delegate_wallet) — waiting",
                        if st.feeder.is_empty() { "<none>".to_string() } else { st.feeder.clone() }
                    );
                } else {
                    let pool = match read_pool(&provider, cfg.pair).await {
                        Ok(p) => Some(p),
                        Err(e) => {
                            eprintln!("! pool read failed: {e:#}");
                            None
                        }
                    };
                    let d = decide(&cfg, &st, pool);
                    eprintln!(
                        "· h={} committed={} @h{} candidate={} (${:.6}, {}{}) → {}: {}",
                        st.height, st.price, st.price_height, d.price, d.price as f64 / PRICE_SCALE as f64,
                        d.source, if d.clamped { ", CLAMPED" } else { "" },
                        if d.push { "PUSH" } else { "hold" }, d.reason
                    );
                    if d.push && !cfg.dry_run {
                        let nonce = now_ms().max(last_nonce + 1);
                        last_nonce = nonce;
                        match push_price(&cfg, &sk, &feeder_hex, d.price, nonce) {
                            Ok(txid) => eprintln!("  ✓ pushed {} (${:.6}) txid={txid}", d.price, d.price as f64 / PRICE_SCALE as f64),
                            Err(e) => eprintln!("  ✗ push failed: {e:#}"),
                        }
                    }
                }
            }
        }
        if cfg.once {
            return Ok(());
        }
        tokio::time::sleep(cfg.interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config {
            sigil_api_url: String::new(),
            polygon_rpc_url: String::new(),
            pair: Address::ZERO,
            keyfile: PathBuf::new(),
            interval: Duration::from_secs(1),
            min_quote_usd: 50.0,
            reference_usd_e8: 100_000,
            max_step_bps: 500,
            move_bps: 50,
            refresh_blocks: 20_000,
            once: true,
            dry_run: true,
        }
    }

    fn status(price: u128, price_height: u64, height: u64) -> UsdsStatus {
        UsdsStatus { price: price.to_string(), price_height, price_fresh: true, feeder: String::new(), height, live: true, live_height: 0 }
    }

    /// The live pool on 2026-09-06: 0.001 USDC (1000 base) vs 1 wSIGIL3 (1e18 wei) → $0.001.
    #[test]
    fn spot_matches_the_live_pool_figures() {
        let p = spot_from_reserves(U256::from(1_000u64), U256::from(1_000_000_000_000_000_000u128)).unwrap();
        assert_eq!(p, 100_000, "$0.001000 × 1e8");
        // 1 USDC vs 500 wSIGIL → $0.002
        let p = spot_from_reserves(U256::from(1_000_000u64), U256::from(500u128 * 10u128.pow(18))).unwrap();
        assert_eq!(p, 200_000);
        assert_eq!(spot_from_reserves(U256::from(1u64), U256::ZERO), None);
        assert!((usdc_reserve_usd(U256::from(1_000u64)) - 0.001).abs() < 1e-12);
    }

    #[test]
    fn a_thin_pool_is_not_trusted_and_the_reference_is_used() {
        let d = decide(&cfg(), &status(0, 0, 100), Some((5_000_000, 0.001)));
        assert_eq!(d.price, 100_000, "reference, not the $0.05 the thin pool claims");
        assert!(d.source.starts_with("reference"));
        assert!(d.push, "nothing committed → push");
    }

    #[test]
    fn a_deep_pool_is_trusted_but_speed_limited() {
        // Committed $0.001; pool says $0.01 (10×) with $500 behind it → walk at most 5 %.
        let d = decide(&cfg(), &status(100_000, 90, 100), Some((1_000_000, 500.0)));
        assert_eq!(d.source, "pool");
        assert!(d.clamped);
        assert_eq!(d.price, 105_000);
        assert!(d.push, "5 % ≥ 0.5 % move");
        // Down the same way, and never to zero.
        let d = decide(&cfg(), &status(100_000, 90, 100), Some((1, 500.0)));
        assert_eq!(d.price, 95_000);
        let (p, _) = clamp_step(0, 10, 10_000);
        assert_eq!(p, 1);
    }

    #[test]
    fn a_flat_market_holds_until_the_refresh_heartbeat() {
        // Same price as committed, stamp 100 blocks old → hold.
        let d = decide(&cfg(), &status(100_000, 1_000, 1_100), Some((100_000, 500.0)));
        assert!(!d.push, "{}", d.reason);
        // 0.4 % move → still hold (below 0.5 %).
        let d = decide(&cfg(), &status(100_000, 1_000, 1_100), Some((100_400, 500.0)));
        assert!(!d.push);
        // Stamp 20,000 blocks old → heartbeat push even though unchanged.
        let d = decide(&cfg(), &status(100_000, 1_000, 21_000), Some((100_000, 500.0)));
        assert!(d.push);
        assert!(d.reason.contains("stamp age"));
        // A price with NO stamp (pre-activation push) is re-pushed immediately.
        let d = decide(&cfg(), &status(100_000, 0, 5), Some((100_000, 500.0)));
        assert!(d.push);
    }

    #[test]
    fn move_bps_is_relative_to_the_committed_price() {
        assert_eq!(move_bps(105_000, 100_000), 500);
        assert_eq!(move_bps(95_000, 100_000), 500);
        assert_eq!(move_bps(100_000, 100_000), 0);
        assert_eq!(move_bps(1, 0), u128::MAX);
    }
}
