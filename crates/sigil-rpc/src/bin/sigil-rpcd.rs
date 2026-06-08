//! sigil-rpcd — std-only HTTP transport over the sigil-rpc money keystone.
//!
//! No axum/tokio: raw TcpListener + a tiny HTTP/1.1 handler (FLUXFOOD-light,
//! same std-only ethos as sigil-top). Wraps the verified keystone fns behind a
//! shared Arc<RwLock<Node>> with thread-per-connection (concurrent reads,
//! serialized writes — fixes the single-thread accept-loop throughput wall).
//! Every state change still goes through the 21M-cap
//! chokepoint (commit_state_transition) — the transport adds zero trust.
//!
//!   GET  /health
//!   GET  /balance?wallet=HEX64&token=HEX64        (token omitted = NATIVE)
//!   GET  /pools                                    (the bootstrapped pool)
//!   POST /swap   {"from":HEX,"pool":HEX,"dir":"AtoB|BtoA","amount_in":N,"min_out":N}
//!   POST /mine   {"miner":HEX,"header":"str","nonce":N,"difficulty":N,"reward":N}
//!   GET  /api/v1/mining/challenge?wallet=HEX64   → dual-lane Challenge (flux-miner)
//!   POST /api/v1/mining/submit  {Submission}     → SubmitResult (BLAKE4 Φ + VDF Ω)
//!   POST /credit {"operator_pool":HEX,"pool_amount":N,"verifiers":[HEX,...]}

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, RwLock};
use std::thread;

use flux_miner::client::{check_submission, Challenge, Submission, SubmitResult};
use flux_vdf::ModSquaring;
use sigil_dex::SwapDirection;
use sigil_rpc::nation;
use sigil_rpc::onboard::{self, VerifiedRegistry};
use sigil_rpc::{credit_light_verifiers, credit_share, execute_swap, submit_share};
use sigil_state::{
    commit_state_transition, PoolId, PoolState, SigilState, StateMutation, StateTransition,
    TokenId, WalletId, NATIVE,
};

/// The live SIGIL DEX economy. Beyond raw state, the daemon tracks the token +
/// pool registries (so the swarm + dashboard can enumerate them), the verified-
/// student registry (the earning gate), and the citizen roster.
struct Node {
    state: SigilState,
    height: u64,
    /// Student / verified-user registry — onboarded users that may earn.
    students: VerifiedRegistry,
    /// symbol → token-id (predefined + dynamically deployed via /deploy_token).
    tokens: Vec<(String, TokenId)>,
    /// label → pool-id (predefined + dynamically created).
    pools: Vec<(String, PoolId)>,
    /// attested nation citizens (wallet roster, for /nation/citizens).
    citizens: Vec<WalletId>,
    /// flux-native explorer index: every tx appended as a searchable HistoryEntry
    /// (flux-db persist + flux-search TF-IDF). None if the store couldn't open.
    history: Option<flux_history::HistoryStore>,
}

/// hex of a 32-byte id (searchable content + the `addr` tag).
fn hexs(b: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for x in b { s.push_str(&format!("{x:02x}")); }
    s
}
fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}
/// Append a tx to the flux-history index. `addrs` go into the full-text content +
/// an `addr` tag so a wallet-address search returns this tx.
fn ingest(node: &mut Node, kind: &str, title: String, addrs: &[[u8; 32]], extra: &str) {
    let height = node.height;
    let ts = now_ms();
    let Some(h) = node.history.as_mut() else { return };
    let addr_str: Vec<String> = addrs.iter().map(hexs).collect();
    let content = format!("block {height} {} {}", addr_str.join(" "), extra);
    let mut entry = flux_history::HistoryEntry::new(kind, addr_str.first().cloned().unwrap_or_default(), title, content, ts)
        .with_tag("height", &height.to_string());
    for a in &addr_str { entry = entry.with_tag("addr", a); }
    let _ = h.append(entry);
}

// ── Token ids (legible byte-fills; dynamically deployed tokens are hashed). ──
const USDS: TokenId = [0xAA; 32]; // stablecoin quote asset
const WQUG: TokenId = [0xBB; 32]; // wrapped Quillon QUG
const CLAI: TokenId = [0xC1; 32]; // Claude-Liaison inter-agent token
const PACI: TokenId = [0xCA; 32]; // Rocky's PACI
const SCAL: TokenId = [0x5C; 32]; // Codex's SCALPEL
const GPU_T: TokenId = [0x9B; 32]; // compute credit token

// ── Pools (constant-product, 30 bps LP fee). USDS is the common quote asset. ─
const POOL: PoolId = [0xCC; 32]; // USDS/wQUG (kept for nation/back-compat)
const POOL_CLAI: PoolId = [0xC2; 32]; // CLAI/USDS
const POOL_PACI: PoolId = [0xC3; 32]; // PACI/USDS
const POOL_SCAL: PoolId = [0xC4; 32]; // SCAL/USDS
const POOL_SIGIL: PoolId = [0xC5; 32]; // SIGIL(NATIVE)/USDS

const OPERATOR: WalletId = [0xEE; 32];
const MASTER: WalletId = [0xFF; 32];
// flux-nation demo citizen (funded + attested at bootstrap so the page's pay works)
const CITIZEN: WalletId = [0x11; 32];
const POWER_CO: WalletId = [0x9E; 32];
const CPR: [u8; 32] = [0x42; 32]; // hash of a CPR (raw CPR never stored)

/// How many swarm trader wallets to fund at genesis.
const TRADERS: u8 = 8;
/// Deterministic swarm trader wallet `i` → `a0a0…a0<i>`.
fn trader(i: u8) -> WalletId {
    let mut w = [0xA0; 32];
    w[31] = i;
    w
}

// ── Dual-lane mining (BLAKE4 Φ + VDF Ω) params ───────────────────────────────
// These define the `/api/v1/mining/{challenge,submit}` work and MUST match the
// flux-miner client's group (ModSquaring::bench_2048) + the reference
// flux-mining-node defaults, so the same `flux-miner mine <url> <wallet>` binary
// drives sigil-rpcd unchanged. Operator-tunable via env.
const MINING_BLAKE4_BITS: u32 = 16; // BLAKE4 target = u64::MAX >> bits (Lane A)
const MINING_VDF_T: u64 = 600; // sequential VDF squarings per share (Lane B)
const MINING_REWARD: u128 = 50; // NATIVE coinbase per accepted share (cap-enforced)

fn mining_bits() -> u32 {
    std::env::var("SIGIL_MINING_BLAKE4_BITS").ok().and_then(|s| s.parse().ok()).unwrap_or(MINING_BLAKE4_BITS)
}
fn mining_vdf_t() -> u64 {
    std::env::var("SIGIL_MINING_VDF_T").ok().and_then(|s| s.parse().ok()).unwrap_or(MINING_VDF_T)
}
fn mining_reward() -> u128 {
    std::env::var("SIGIL_MINING_REWARD").ok().and_then(|s| s.parse().ok()).unwrap_or(MINING_REWARD)
}
fn mining_blake4_target() -> u64 {
    u64::MAX >> mining_bits()
}
/// Deterministic per-height VDF seed (binds a challenge to its height) — same
/// derivation as the reference flux-mining-node so one miner binary drives both.
fn mining_seed_for(height: u64) -> [u8; 32] {
    let mut s = [0u8; 32];
    s[..8].copy_from_slice(&height.to_le_bytes());
    s
}

/// Integer sqrt (initial LP shares = √(a·b), Uniswap-V2 style).
fn isqrt_u128(n: u128) -> u128 {
    if n == 0 {
        return 0;
    }
    let (mut x, mut y) = (n, (n + 1) / 2);
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

/// Deterministic non-crypto token id from a symbol (for /deploy_token). FNV-1a
/// then a bit-mixer folded across 32 bytes; first byte tagged `0x70` so dynamic
/// tokens are visually distinct from the predefined byte-fill ids.
fn token_id_for(sym: &str) -> TokenId {
    let mut out = [0u8; 32];
    let mut h: u64 = 0xcbf29ce484222325;
    for b in sym.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    for o in out.iter_mut() {
        h ^= h >> 33;
        h = h.wrapping_mul(0xff51afd7ed558ccd);
        *o = h as u8;
    }
    out[0] = 0x70;
    out
}

fn bootstrap() -> Node {
    let mut state = SigilState::new();
    let mut m: Vec<StateMutation> = vec![StateMutation::SetMasterWallet { wallet: MASTER }];

    // Operator + demo citizen seed funding (NATIVE well under the 21M cap).
    m.push(StateMutation::SetBalance { wallet: OPERATOR, token: NATIVE, amount: 2_000_000 });
    m.push(StateMutation::SetBalance { wallet: OPERATOR, token: USDS, amount: 5_000_000 });
    m.push(StateMutation::SetBalance { wallet: CITIZEN, token: NATIVE, amount: 1_000 });

    // Fund the swarm traders: a stack of every token so they can hit any pool.
    for i in 0..TRADERS {
        let w = trader(i);
        m.push(StateMutation::SetBalance { wallet: w, token: NATIVE, amount: 2_000 });
        m.push(StateMutation::SetBalance { wallet: w, token: USDS, amount: 100_000 });
        m.push(StateMutation::SetBalance { wallet: w, token: WQUG, amount: 50_000 });
        m.push(StateMutation::SetBalance { wallet: w, token: CLAI, amount: 20_000 });
        m.push(StateMutation::SetBalance { wallet: w, token: PACI, amount: 40_000 });
        m.push(StateMutation::SetBalance { wallet: w, token: SCAL, amount: 200_000 });
        m.push(StateMutation::SetBalance { wallet: w, token: GPU_T, amount: 10_000 });
    }

    // Seed the LP pools with real reserves (lp_shares = √(a·b)).
    let seed = |pool: PoolId, ta: TokenId, tb: TokenId, ra: u128, rb: u128| StateMutation::SetPool {
        pool,
        state: PoolState {
            token_a: ta, token_b: tb,
            reserve_a: ra, reserve_b: rb,
            lp_shares: isqrt_u128(ra.saturating_mul(rb)),
            fee_bps: 30, accrued_fees: 0,
        },
    };
    m.push(seed(POOL, USDS, WQUG, 100_000, 100_000));
    m.push(seed(POOL_CLAI, CLAI, USDS, 50_000, 100_000));
    m.push(seed(POOL_PACI, PACI, USDS, 200_000, 100_000));
    m.push(seed(POOL_SCAL, SCAL, USDS, 1_000_000, 50_000));
    m.push(seed(POOL_SIGIL, NATIVE, USDS, 100_000, 100_000));

    commit_state_transition(&mut state, &StateTransition { at_height: 0, mutations: m }, 0)
        .expect("genesis");

    // flux-nation: attest the demo citizen so /nation/pay works against real state.
    nation::attest_citizen(&mut state, 1, nation::BORGER_AUTHORITY, CITIZEN, CPR)
        .expect("attest demo citizen");

    let tokens = vec![
        ("SIGIL".into(), NATIVE), ("USDS".into(), USDS), ("wQUG".into(), WQUG),
        ("CLAI".into(), CLAI), ("PACI".into(), PACI), ("SCAL".into(), SCAL),
        ("GPU".into(), GPU_T),
    ];
    let pools = vec![
        ("USDS/wQUG".into(), POOL), ("CLAI/USDS".into(), POOL_CLAI),
        ("PACI/USDS".into(), POOL_PACI), ("SCAL/USDS".into(), POOL_SCAL),
        ("SIGIL/USDS".into(), POOL_SIGIL),
    ];
    // flux-history index (flux-db + flux-search) — persistent across restarts.
    let hist_path = std::env::var("SIGIL_HISTORY_PATH").unwrap_or_else(|_| "/home/orobit/sigil-data/history".into());
    let history = match flux_history::HistoryStore::open(&hist_path) {
        Ok(s) => { eprintln!("flux-history: {} entries @ {hist_path}", s.len()); Some(s) }
        Err(e) => { eprintln!("flux-history: disabled ({e})"); None }
    };
    Node { state, height: 2, students: VerifiedRegistry::new(), tokens, pools, citizens: vec![CITIZEN], history }
}

/// Symbol for a token id given the live registry (falls back to a hex stub).
fn sym_of(node: &Node, t: &TokenId) -> String {
    node.tokens.iter().find(|(_, id)| id == t).map(|(s, _)| s.clone())
        .unwrap_or_else(|| format!("0x{}", &to_hex(t)[..6]))
}

fn hex32(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 { return None; }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}
fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// minimal JSON field pluckers (the bodies are small + flat)
fn jnum(body: &str, key: &str) -> Option<u128> {
    let p = format!("\"{key}\"");
    let i = body.find(&p)? + p.len();
    let rest = &body[i..];
    let c = rest.find(':')? + 1;
    let tail = rest[c..].trim_start();
    let end = tail.find(|ch: char| !ch.is_ascii_digit()).unwrap_or(tail.len());
    tail[..end].parse().ok()
}
fn jstr<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let p = format!("\"{key}\"");
    let i = body.find(&p)? + p.len();
    let rest = &body[i..];
    let q1 = rest[rest.find(':')? + 1..].find('"')? + rest.find(':')? + 2;
    let q2 = rest[q1..].find('"')? + q1;
    Some(&rest[q1..q2])
}

fn ok(body: String) -> String {
    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\n\r\n{}", body.len(), body)
}
fn bad(msg: &str) -> String {
    let b = format!("{{\"ok\":false,\"error\":\"{}\"}}", msg.replace('"', "'"));
    format!("HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\n\r\n{}", b.len(), b)
}

fn route(node: &RwLock<Node>, method: &str, path: &str, query: &str, body: &str) -> String {
    // The SIGIL wallet (a q-api fork) calls the `/api/v1/*` shape — map it onto
    // our flat routes so the wallet works unchanged against sigil-rpcd.
    let path = path.strip_prefix("/api/v1").unwrap_or(path);
    // CORS preflight (harmless when served same-origin via the q-flux vhost).
    if method == "OPTIONS" {
        return ok("{}".into());
    }
    match (method, path) {
        ("GET", "/health") => ok("{\"ok\":true,\"service\":\"sigil-rpcd\"}".into()),
        ("GET", "/status") => {
            let n = node.read().unwrap();
            ok(format!(
                "{{\"ok\":true,\"service\":\"sigil-rpcd\",\"network\":\"sigil-g0\",\"height\":{},\"version\":\"0.0.7\",\"peers\":1}}",
                n.height
            ))
        }
        // flux-search over the tx history (flux-db + flux-search). Search a wallet
        // address → that address's transactions; or a block hash/height/keyword.
        ("GET", "/search") => {
            let q = query_get(query, "q").unwrap_or("").trim().to_string();
            if q.len() < 2 { return ok("{\"ok\":true,\"q\":\"\",\"count\":0,\"results\":[]}".into()); }
            let js = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
            // a 64-hex query is a wallet ADDRESS → EXACT tag lookup (TF-IDF would
            // miss it, e.g. IDF=0 when a term is in every doc). Else full-text.
            let is_addr = q.len() == 64 && q.chars().all(|c| c.is_ascii_hexdigit());
            let mut n = node.write().unwrap();
            let items: Vec<String> = match n.history.as_mut() {
                None => vec![],
                Some(h) if is_addr => h.by_tag("addr", &q.to_lowercase()).unwrap_or_default().iter().map(|e| format!(
                    "{{\"title\":{},\"snippet\":{},\"id\":{},\"kind\":{},\"ts\":{},\"score\":1.0}}",
                    js(&e.title), js(&e.content), js(&e.id), js(&e.kind), e.ts_ms)).collect(),
                Some(h) => h.search(&q, 25).iter().map(|r| format!(
                    "{{\"title\":{},\"snippet\":{},\"id\":{},\"score\":{:.3}}}",
                    js(&r.title), js(&r.snippet), js(&r.url), r.score)).collect(),
            };
            ok(format!("{{\"ok\":true,\"q\":{},\"count\":{},\"results\":[{}]}}", js(&q), items.len(), items.join(",")))
        }
        ("GET", "/balance") => {
            let w = query_get(query, "wallet").and_then(hex32);
            let t = query_get(query, "token").and_then(hex32).unwrap_or(NATIVE);
            match w {
                Some(w) => {
                    let n = node.read().unwrap();
                    ok(format!("{{\"ok\":true,\"balance\":{}}}", n.state.balance_of(&w, &t)))
                }
                None => bad("wallet must be 64-hex"),
            }
        }
        ("GET", "/pools") => {
            let n = node.read().unwrap();
            let mut items = Vec::new();
            for (label, pid) in &n.pools {
                if let Some(p) = n.state.pool(pid) {
                    items.push(format!(
                        "{{\"id\":\"{}\",\"label\":\"{}\",\"token_a\":\"{}\",\"token_b\":\"{}\",\"sym_a\":\"{}\",\"sym_b\":\"{}\",\"reserve_a\":{},\"reserve_b\":{},\"lp_shares\":{},\"fee_bps\":{},\"accrued_fees\":{}}}",
                        to_hex(pid), label, to_hex(&p.token_a), to_hex(&p.token_b),
                        sym_of(&n, &p.token_a), sym_of(&n, &p.token_b),
                        p.reserve_a, p.reserve_b, p.lp_shares, p.fee_bps, p.accrued_fees));
                }
            }
            ok(format!("{{\"ok\":true,\"pools\":[{}]}}", items.join(",")))
        }
        ("GET", "/tokens") => {
            let n = node.read().unwrap();
            let items: Vec<String> = n.tokens.iter()
                .map(|(s, id)| format!("{{\"symbol\":\"{}\",\"id\":\"{}\"}}", s, to_hex(id)))
                .collect();
            ok(format!("{{\"ok\":true,\"tokens\":[{}]}}", items.join(",")))
        }
        ("GET", "/wallets") => {
            // The funded swarm trader wallets, with per-token balances.
            let n = node.read().unwrap();
            let mut items = Vec::new();
            for i in 0..TRADERS {
                let w = trader(i);
                let bals: Vec<String> = n.tokens.iter()
                    .map(|(s, id)| format!("\"{}\":{}", s, n.state.balance_of(&w, id)))
                    .collect();
                items.push(format!("{{\"i\":{},\"wallet\":\"{}\",\"balances\":{{{}}}}}", i, to_hex(&w), bals.join(",")));
            }
            ok(format!("{{\"ok\":true,\"traders\":[{}]}}", items.join(",")))
        }
        ("GET", "/economy") => {
            let n = node.read().unwrap();
            ok(format!(
                "{{\"ok\":true,\"network\":\"sigil-g0\",\"height\":{},\"tokens\":{},\"pools\":{},\"traders\":{},\"students\":{},\"citizens\":{},\"treasury_usds\":{}}}",
                n.height, n.tokens.len(), n.pools.len(), TRADERS, n.students.len(), n.citizens.len(),
                n.state.balance_of(&MASTER, &USDS)))
        }
        ("POST", "/deploy_token") => {
            let symbol = match jstr(body, "symbol") { Some(s) if !s.is_empty() => s.to_string(), _ => return bad("symbol required") };
            let supply = jnum(body, "supply").unwrap_or(1_000_000);
            let to = jstr(body, "to").and_then(hex32).unwrap_or(OPERATOR);
            let id = token_id_for(&symbol);
            let mut n = node.write().unwrap();
            if n.tokens.iter().any(|(s, _)| s.eq_ignore_ascii_case(&symbol)) { return bad("token symbol already deployed"); }
            let h = n.height;
            let cur = n.state.balance_of(&to, &id);
            match commit_state_transition(&mut n.state, &StateTransition { at_height: h, mutations: vec![StateMutation::SetBalance { wallet: to, token: id, amount: cur.saturating_add(supply) }] }, h) {
                Ok(_) => { n.height += 1; n.tokens.push((symbol.clone(), id)); ok(format!("{{\"ok\":true,\"symbol\":\"{}\",\"id\":\"{}\",\"supply\":{},\"to\":\"{}\"}}", symbol, to_hex(&id), supply, to_hex(&to))) }
                Err(e) => bad(&e.to_string()),
            }
        }
        ("POST", "/add_liquidity") => {
            let from = jstr(body, "from").and_then(hex32);
            let pool = jstr(body, "pool").and_then(hex32);
            let amt_a = jnum(body, "amount_a");
            let amt_b = jnum(body, "amount_b");
            match (from, pool, amt_a, amt_b) {
                (Some(from), Some(pool), Some(amt_a), Some(amt_b)) if amt_a > 0 && amt_b > 0 => {
                    let mut n = node.write().unwrap();
                    let prev = match n.state.pool(&pool) { Some(p) => p.clone(), None => return bad("unknown pool") };
                    // proportional shares vs the limiting side (Uniswap-V2 model).
                    let s_a = amt_a.saturating_mul(prev.lp_shares) / prev.reserve_a.max(1);
                    let s_b = amt_b.saturating_mul(prev.lp_shares) / prev.reserve_b.max(1);
                    let shares = s_a.min(s_b);
                    if shares == 0 { return bad("deposit too small to mint shares"); }
                    let pool_after = PoolState {
                        token_a: prev.token_a, token_b: prev.token_b,
                        reserve_a: prev.reserve_a + amt_a, reserve_b: prev.reserve_b + amt_b,
                        lp_shares: prev.lp_shares + shares, fee_bps: prev.fee_bps, accrued_fees: prev.accrued_fees,
                    };
                    let h = n.height;
                    match commit_state_transition(&mut n.state, &StateTransition { at_height: h, mutations: vec![StateMutation::LpDelta { from, pool, amt_a, amt_b, shares_minted: shares, fee: 0, pool_after }] }, h) {
                        Ok(_) => { n.height += 1; ok(format!("{{\"ok\":true,\"shares_minted\":{},\"pool\":\"{}\"}}", shares, to_hex(&pool))) }
                        Err(e) => bad(&e.to_string()),
                    }
                }
                _ => bad("need from,pool (64-hex) + amount_a,amount_b > 0"),
            }
        }
        ("POST", "/onboard") => {
            // seed: explicit 64-hex, else derive from time entropy (xorshift).
            let seed: [u8; 32] = jstr(body, "seed").and_then(hex32).unwrap_or_else(|| {
                let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(1);
                let mut s = [0u8; 32];
                let mut x = nanos as u64 ^ 0x9E3779B97F4A7C15;
                for o in s.iter_mut() { x ^= x >> 12; x ^= x << 25; x ^= x >> 27; *o = (x.wrapping_mul(0x2545F4914F6CDD1D) >> 32) as u8; }
                s
            });
            let mut n = node.write().unwrap();
            let nonce = n.height;
            let (kp, verification) = onboard::onboard_user(&seed, nonce);
            let wallet = kp.pubkey();
            // student: register the self-signed verification (the earning gate).
            if let Err(e) = n.students.register(verification) { return bad(&format!("student: {:?}", e)); }
            // nation: attest as citizen with a derived (non-zero) cpr_hash.
            let mut cpr = [0u8; 32];
            for (c, w) in cpr.iter_mut().zip(wallet.iter()) { *c = w ^ 0x5a; }
            cpr[0] |= 1;
            let h = n.height;
            if let Err(e) = nation::attest_citizen(&mut n.state, h, nation::BORGER_AUTHORITY, wallet, cpr) { return bad(&format!("nation: {:?}", e)); }
            n.height += 1;
            // starter funding so the new user can immediately trade.
            let h2 = n.height;
            let nat = n.state.balance_of(&wallet, &NATIVE).saturating_add(100);
            let usd = n.state.balance_of(&wallet, &USDS).saturating_add(1_000);
            let _ = commit_state_transition(&mut n.state, &StateTransition { at_height: h2, mutations: vec![
                StateMutation::SetBalance { wallet, token: NATIVE, amount: nat },
                StateMutation::SetBalance { wallet, token: USDS, amount: usd },
            ] }, h2);
            n.height += 1;
            n.citizens.push(wallet);
            ok(format!("{{\"ok\":true,\"wallet\":\"{}\",\"seed\":\"{}\",\"student\":true,\"citizen\":true,\"native\":{},\"usds\":{}}}",
                to_hex(&wallet), to_hex(&seed), nat, usd))
        }
        ("GET", "/nation/citizens") => {
            let n = node.read().unwrap();
            let items: Vec<String> = n.citizens.iter().map(|w| format!("\"{}\"", to_hex(w))).collect();
            ok(format!("{{\"ok\":true,\"count\":{},\"citizens\":[{}]}}", n.citizens.len(), items.join(",")))
        }
        ("POST", "/swap") => {
            let from = jstr(body, "from").and_then(hex32);
            let pool = jstr(body, "pool").and_then(hex32);
            let dir = match jstr(body, "dir") { Some("AtoB") => SwapDirection::AtoB, Some("BtoA") => SwapDirection::BtoA, _ => return bad("dir must be AtoB|BtoA") };
            let amount_in = jnum(body, "amount_in");
            let min_out = jnum(body, "min_out").unwrap_or(1);
            match (from, pool, amount_in) {
                (Some(from), Some(pool), Some(amount_in)) => {
                    let mut n = node.write().unwrap();
                    let h = n.height;
                    match execute_swap(&mut n.state, h, from, pool, dir, amount_in, min_out) {
                        Ok(r) => { n.height += 1; ingest(&mut n, "swap", format!("swap {amount_in} in → {} out", r.amount_out), &[from], "swap dex"); ok(format!("{{\"ok\":true,\"amount_out\":{},\"lp_fee\":{},\"protocol_fee\":{}}}", r.amount_out, r.fee, r.protocol_fee)) }
                        Err(e) => bad(&e.to_string()),
                    }
                }
                _ => bad("need from,pool (64-hex) + amount_in"),
            }
        }
        // Diagnostic collector — miners POST GPU init/search errors here so they
        // can be read server-side (no file-pasting). Appends the raw body to a log.
        ("POST", "/diag") => {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("/home/orobit/sigil-miner-diag.log")
            {
                let _ = writeln!(f, "{}", body);
            }
            ok("{\"ok\":true}".into())
        }
        ("POST", "/mine") => {
            let miner = jstr(body, "miner").and_then(hex32);
            let header = jstr(body, "header").unwrap_or("sigil-block").to_string();
            let nonce = jnum(body, "nonce").unwrap_or(0) as u64;
            let difficulty = jnum(body, "difficulty").unwrap_or(0) as u32;
            let reward = jnum(body, "reward").unwrap_or(50);
            match miner {
                Some(miner) => {
                    let mut n = node.write().unwrap();
                    let h = n.height;
                    match submit_share(&mut n.state, h, miner, header.as_bytes(), nonce, difficulty, reward) {
                        Ok(bal) => { n.height += 1; ingest(&mut n, "mine", format!("mine reward {reward} SIGIL"), &[miner], "mining block reward"); ok(format!("{{\"ok\":true,\"new_balance\":{}}}", bal)) }
                        Err(e) => bad(&e.to_string()),
                    }
                }
                None => bad("miner must be 64-hex"),
            }
        }
        // ── Dual-lane miner (flux-miner): the BLAKE4 Φ + VDF Ω work surface. ──
        // GET issues a height-bound challenge; POST verifies the solved block via
        // flux-miner's shared `check_submission` gate, then credits through the
        // cap-enforced money chokepoint. The wallet is bound into the header at
        // solve time, so a share can't be re-pointed to a different miner.
        ("GET", "/mining/challenge") => {
            let n = node.read().unwrap();
            let h = n.height;
            let c = Challenge {
                height: h,
                vdf_input: mining_seed_for(h),
                blake4_target: mining_blake4_target(),
                vdf_t: mining_vdf_t(),
            };
            ok(serde_json::to_string(&c).unwrap_or_else(|_| "{}".into()))
        }
        ("POST", "/mining/submit") => {
            let sub: Submission = match serde_json::from_str(body) {
                Ok(s) => s,
                Err(e) => return bad(&format!("bad submission json: {e}")),
            };
            let miner = match hex32(&sub.wallet) {
                Some(w) => w,
                None => return bad("wallet must be 64-hex"),
            };
            // Reconstruct the challenge this share's height was issued under
            // (target + vdf_t fixed, seed height-derived), then run the dual-lane
            // consensus gate (Lane A BLAKE4 ≤ target AND Lane B VDF verify).
            let c = Challenge {
                height: sub.height,
                vdf_input: mining_seed_for(sub.height),
                blake4_target: mining_blake4_target(),
                vdf_t: mining_vdf_t(),
            };
            let g = ModSquaring::bench_2048();
            if !check_submission(&g, &c, &sub) {
                let r = SubmitResult {
                    accepted: false,
                    reason: Some("dual-lane verify / height / header mismatch".into()),
                };
                return ok(serde_json::to_string(&r).unwrap_or_else(|_| "{}".into()));
            }
            // Verified — credit through the SAME cap-enforced chokepoint as /mine.
            let mut n = node.write().unwrap();
            let h = n.height;
            match credit_share(&mut n.state, h, miner, mining_reward()) {
                Ok(bal) => {
                    n.height += 1;
                    ok(format!("{{\"accepted\":true,\"reason\":null,\"new_balance\":{}}}", bal))
                }
                // Cap hit / overflow → NOT credited; report as a rejected share.
                Err(e) => {
                    let r = SubmitResult { accepted: false, reason: Some(e.to_string()) };
                    ok(serde_json::to_string(&r).unwrap_or_else(|_| "{}".into()))
                }
            }
        }
        ("POST", "/credit") => {
            let opw = jstr(body, "operator_pool").and_then(hex32);
            let pool_amount = jnum(body, "pool_amount").unwrap_or(0);
            // verifiers: comma-joined 64-hex inside the array, parsed loosely
            let verifiers: Vec<WalletId> = body
                .split('"').filter_map(hex32).filter(|w| *w != opw.unwrap_or([0u8;32])).collect();
            match opw {
                Some(opw) => {
                    let mut n = node.write().unwrap();
                    let h = n.height;
                    match credit_light_verifiers(&mut n.state, h, opw, pool_amount, &verifiers) {
                        Ok(r) => { n.height += 1; ok(format!("{{\"ok\":true,\"per_verifier\":{},\"credited\":{},\"num_verifiers\":{}}}", r.per_verifier, r.credited, r.num_verifiers)) }
                        Err(e) => bad(&e.to_string()),
                    }
                }
                None => bad("operator_pool must be 64-hex"),
            }
        }
        ("OPTIONS", _) => ok("{}".into()),
        ("GET", "/nation/state") => {
            let n = node.read().unwrap();
            ok(format!("{{\"ok\":true,\"citizen\":\"{}\",\"is_citizen\":{},\"citizen_native\":{},\"provider_native\":{}}}",
                to_hex(&CITIZEN), nation::is_citizen(&n.state, &CITIZEN),
                n.state.balance_of(&CITIZEN, &NATIVE), n.state.balance_of(&POWER_CO, &NATIVE)))
        }
        ("POST", "/nation/pay") => {
            let amount = jnum(body, "amount").unwrap_or(0);
            if amount == 0 { bad("amount required (positive)") }
            else {
                let mut n = node.write().unwrap();
                let h = n.height;
                match nation::pay_utility_bill(&mut n.state, h, CITIZEN, POWER_CO, amount) {
                    Ok(left) => { n.height += 1; ok(format!("{{\"ok\":true,\"paid\":{},\"citizen_native\":{},\"provider_native\":{}}}",
                        amount, left, n.state.balance_of(&POWER_CO, &NATIVE))) }
                    Err(e) => bad(&format!("nation: {:?}", e)),
                }
            }
        }
        ("POST", "/nation/eboks") => {
            match jstr(body, "doc").and_then(hex32) {
                Some(doc) => {
                    let mut n = node.write().unwrap();
                    let h = n.height;
                    match nation::issue_eboks_receipt(&mut n.state, h, CITIZEN, doc) {
                        Ok(()) => { n.height += 1; ok(format!("{{\"ok\":true,\"doc\":\"{}\"}}", to_hex(&doc))) }
                        Err(e) => bad(&format!("nation: {:?}", e)),
                    }
                }
                None => bad("doc must be 64-hex"),
            }
        }
        ("GET", "/nation/verify") => {
            match query_get(query, "doc").and_then(hex32) {
                Some(doc) => {
                    let n = node.read().unwrap();
                    ok(format!("{{\"ok\":true,\"verified\":{}}}", nation::verify_eboks_receipt(&n.state, &CITIZEN, &doc)))
                }
                None => bad("doc must be 64-hex"),
            }
        }
        _ => bad("unknown route"),
    }
}

fn query_get<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    for kv in query.split('&') {
        let mut it = kv.splitn(2, '=');
        if it.next() == Some(key) { return it.next(); }
    }
    None
}

fn handle(mut stream: TcpStream, node: &RwLock<Node>) {
    // Read until headers are complete, then until the full Content-Length body
    // has arrived (a single read() can miss a body in a later TCP segment).
    let mut data: Vec<u8> = Vec::with_capacity(2048);
    let mut tmp = [0u8; 8192];
    let mut header_end;
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => return,
            Ok(k) => data.extend_from_slice(&tmp[..k]),
            Err(_) => return,
        }
        if let Some(pos) = find_sub(&data, b"\r\n\r\n") { header_end = pos + 4; break; }
        if data.len() > 1 << 20 { return; }
    }
    let head = String::from_utf8_lossy(&data[..header_end]).to_string();
    let content_len: usize = head
        .lines()
        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
        .unwrap_or(0);
    while data.len() < header_end + content_len {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(k) => data.extend_from_slice(&tmp[..k]),
            Err(_) => break,
        }
    }
    let first = head.lines().next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("");
    let full_path = parts.next().unwrap_or("/");
    let (path, query) = match full_path.split_once('?') { Some((p, q)) => (p, q), None => (full_path, "") };
    let body = String::from_utf8_lossy(&data[header_end..]).to_string();
    let resp = route(node, method, path, query, &body);
    let _ = stream.write_all(resp.as_bytes());
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn main() {
    let addr = std::env::var("SIGIL_RPC_ADDR").unwrap_or_else(|_| "127.0.0.1:8099".into());
    let node = Arc::new(RwLock::new(bootstrap()));
    let listener = TcpListener::bind(&addr).expect("bind");
    eprintln!("sigil-rpcd listening on {addr} — thread-per-conn + RwLock (concurrent reads, serialized writes); pool USDS/wQUG, trader+operator funded");
    for stream in listener.incoming().flatten() {
        let n = Arc::clone(&node);
        thread::spawn(move || handle(stream, &n));
    }
}
