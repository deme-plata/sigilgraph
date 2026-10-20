//! mining_relay — a miner points at ITS OWN node (`http://127.0.0.1:18186`) instead of at
//! Epsilon, the way a Quillon miner points at its own synced node.
//!
//! Operator, 2026-10-02: "miners can point their miners to localhost and then it works
//! decentralized over p2p … so my node dont become centralized failing point".
//!
//! What this is, honestly:
//! * A local HTTP relay speaking exactly the miner protocol (`GET /api/v1/mining/challenge`,
//!   `POST /api/v1/mining/submit`, `/api/v1/mining/miners`; `/v1/...` aliases), so any rig
//!   and the TUI's own [M] miner work unchanged.
//! * It serves ONLY while this node has verified the chain itself — the 10 ms tip-proof fold
//!   (light) or a full sync. An unverified node answers 503 rather than vouching for a tip it
//!   has not checked.
//! * It balances across several upstream nodes: the local producer API when this process
//!   produces, `SIGIL_MINING_UPSTREAMS`, then the configured node. The healthiest wins by
//!   measured latency; three failures in a row put an upstream in a 30 s cool-down; a submit
//!   goes first to the upstream that issued its challenge (share targets are per-producer),
//!   then fails over.
//!
//! What it is NOT yet: a gossip relay. Shares still reach a producer over HTTP, and g2 has
//! one producer today, so block production stays as centralised as it was. What this
//! removes is the single ENTRY POINT: a miner no longer depends on one URL being reachable.
//! Relaying shares over the p2p mesh needs a producer-side intake — a change to the live
//! producer, staged on a throwaway node pair first (see the design note in the commit).

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const DEFAULT_PORT: u16 = 18186;
const COOLDOWN: Duration = Duration::from_secs(30);
const FAILS_TO_COOL: u32 = 3;

static UP: AtomicBool = AtomicBool::new(false);
/// Set by the UI loop each frame: this node has verified the chain (tip proof or full sync).
static NODE_VERIFIED: AtomicBool = AtomicBool::new(false);
/// The node's own verified height, for the status page.
static NODE_HEIGHT: AtomicU64 = AtomicU64::new(0);

pub fn set_node_verified(ok: bool, height: u64) {
    NODE_VERIFIED.store(ok, Ordering::Relaxed);
    NODE_HEIGHT.store(height, Ordering::Relaxed);
}
pub fn relay_is_up() -> bool { UP.load(Ordering::Relaxed) && NODE_VERIFIED.load(Ordering::Relaxed) }
pub fn port() -> u16 {
    std::env::var("SIGIL_MINING_RELAY_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PORT)
}
pub fn url() -> String { format!("http://127.0.0.1:{}", port()) }
/// Default ON (it only ever binds loopback); `SIGIL_MINING_RELAY=0` turns it off.
pub fn enabled() -> bool { !matches!(std::env::var("SIGIL_MINING_RELAY").as_deref(), Ok("0") | Ok("false") | Ok("off")) }

#[derive(Debug, Clone)]
pub struct Upstream {
    pub base: String,
    pub ewma_ms: f64,
    pub ok: u64,
    pub fail: u64,
    pub consecutive_fail: u32,
    pub cool_until: Option<Instant>,
}
impl Upstream {
    fn new(base: String) -> Self { Self { base, ewma_ms: 0.0, ok: 0, fail: 0, consecutive_fail: 0, cool_until: None } }
    fn cooling(&self, now: Instant) -> bool { self.cool_until.map(|t| now < t).unwrap_or(false) }
    fn record(&mut self, ok: bool, ms: f64, now: Instant) {
        if ok {
            self.ok += 1;
            self.consecutive_fail = 0;
            self.cool_until = None;
            self.ewma_ms = if self.ewma_ms == 0.0 { ms } else { 0.8 * self.ewma_ms + 0.2 * ms };
        } else {
            self.fail += 1;
            self.consecutive_fail += 1;
            if self.consecutive_fail >= FAILS_TO_COOL { self.cool_until = Some(now + COOLDOWN); self.consecutive_fail = 0; }
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct Stats { pub challenges: u64, pub submits: u64, pub accepted: u64, pub rejected: u64, pub failovers: u64, pub refused_unverified: u64 }

struct State {
    ups: Vec<Upstream>,
    stats: Stats,
    /// (wallet, height) → upstream index that issued the challenge. Bounded.
    sticky: HashMap<(String, u64), usize>,
    sticky_order: VecDeque<(String, u64)>,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State { ups: Vec::new(), stats: Stats::default(), sticky: HashMap::new(), sticky_order: VecDeque::new() }))
}

/// Pure: the order to try upstreams in — healthy ones by measured latency (unmeasured ones
/// first, so a new upstream gets probed), cooling ones last. `prefer` jumps the queue.
pub fn pick_order(ups: &[Upstream], prefer: Option<usize>, now: Instant) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..ups.len()).collect();
    idx.sort_by(|&a, &b| {
        let (ua, ub) = (&ups[a], &ups[b]);
        (ua.cooling(now), ua.ewma_ms).partial_cmp(&(ub.cooling(now), ub.ewma_ms)).unwrap_or(std::cmp::Ordering::Equal)
    });
    if let Some(p) = prefer.filter(|&p| p < ups.len() && !ups[p].cooling(now)) {
        idx.retain(|&i| i != p);
        idx.insert(0, p);
    }
    idx
}

/// Pure: the upstream list from configuration, de-duplicated, local producer first.
pub fn upstream_list(local_producer: Option<String>, env_list: Option<&str>, configured: &str) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    let mut push = |s: &str| {
        let s = s.trim().trim_end_matches('/').to_string();
        if !s.is_empty() && !v.contains(&s) && s != url() { v.push(s); }
    };
    if let Some(l) = local_producer { push(&l); }
    if let Some(e) = env_list { for s in e.split(',') { push(s); } }
    push(configured);
    v
}

fn http_client() -> &'static reqwest::blocking::Client {
    static C: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    C.get_or_init(|| reqwest::blocking::Client::builder().timeout(Duration::from_secs(10))
        .user_agent(concat!("sigil-top-mining-relay/", env!("CARGO_PKG_VERSION"))).build().expect("http client"))
}

/// Start the relay once (idempotent). `configured` = the node API base sigil-top already uses.
pub fn start(configured: String) {
    if !enabled() || UP.load(Ordering::Relaxed) { return; }
    // 10.6.0: your own node follows the chain; shares credit at the network's producer, so there is no local
    // producer endpoint to prefer (the second node implementation that served one is gone).
    let local_producer: Option<String> = None;
    let env = std::env::var("SIGIL_MINING_UPSTREAMS").ok();
    let list = upstream_list(local_producer, env.as_deref(), &configured);
    if let Ok(mut s) = state().lock() { s.ups = list.into_iter().map(Upstream::new).collect(); }
    let addr = format!("127.0.0.1:{}", port());
    let Ok(listener) = TcpListener::bind(&addr) else { return };   // port taken = another relay already serves
    UP.store(true, Ordering::Relaxed);
    let _ = std::thread::Builder::new().name("mining-relay".into()).spawn(move || {
        for conn in listener.incoming().flatten() {
            let _ = std::thread::Builder::new().name("mining-relay-conn".into()).spawn(move || { let _ = handle(conn); });
        }
    });
}

pub fn snapshot() -> (Stats, Vec<Upstream>) {
    state().lock().map(|s| (s.stats.clone(), s.ups.clone())).unwrap_or_default()
}

fn respond(mut c: &TcpStream, code: u16, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    let reason = match code { 200 => "OK", 400 => "Bad Request", 404 => "Not Found", 502 => "Bad Gateway", 503 => "Service Unavailable", _ => "OK" };
    write!(c, "HTTP/1.1 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())?;
    c.write_all(body)
}

fn handle(conn: TcpStream) -> std::io::Result<()> {
    conn.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut rd = BufReader::new(&conn);
    let mut line = String::new();
    rd.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("/").to_string());
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        if rd.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" { break; }
        if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") { len = v.trim().parse().unwrap_or(0); }
    }
    let mut body = vec![0u8; len.min(1 << 20)];
    rd.read_exact(&mut body)?;
    let path = target.split('?').next().unwrap_or("").replacen("/api/v1/", "/v1/", 1);

    if path == "/relay/status" {
        let (st, ups) = snapshot();
        let j = serde_json::json!({
            "ok": true, "relay": url(), "node_verified": NODE_VERIFIED.load(Ordering::Relaxed),
            "node_height": NODE_HEIGHT.load(Ordering::Relaxed),
            "stats": {"challenges": st.challenges, "submits": st.submits, "accepted": st.accepted, "rejected": st.rejected,
                      "failovers": st.failovers, "refused_unverified": st.refused_unverified},
            "upstreams": ups.iter().map(|u| serde_json::json!({"base": u.base, "ewma_ms": (u.ewma_ms*10.0).round()/10.0,
                "ok": u.ok, "fail": u.fail, "cooling": u.cooling(Instant::now())})).collect::<Vec<_>>(),
        });
        return respond(&conn, 200, "application/json", j.to_string().as_bytes());
    }
    let is_challenge = method == "GET" && path == "/v1/mining/challenge";
    let is_submit = method == "POST" && path == "/v1/mining/submit";
    let is_miners = method == "GET" && path == "/v1/mining/miners";
    if !(is_challenge || is_submit || is_miners) {
        return respond(&conn, 404, "application/json", br#"{"ok":false,"error":"mining relay: only /api/v1/mining/{challenge,submit,miners} and /relay/status"}"#);
    }
    if !NODE_VERIFIED.load(Ordering::Relaxed) {
        if let Ok(mut s) = state().lock() { s.stats.refused_unverified += 1; }
        return respond(&conn, 503, "application/json", br#"{"ok":false,"error":"this node has not verified the chain yet (tip proof / sync) - try again in a moment"}"#);
    }
    // wallet + height keys for stickiness
    let query = target.splitn(2, '?').nth(1).unwrap_or("").to_string();
    let wallet = query.split('&').find_map(|kv| kv.strip_prefix("wallet=")).unwrap_or("").to_string();
    let sub_key = if is_submit {
        serde_json::from_slice::<serde_json::Value>(&body).ok().map(|v| (
            v.get("wallet").and_then(|w| w.as_str()).unwrap_or("").to_string(),
            v.get("height").and_then(|h| h.as_u64()).unwrap_or(0)))
    } else { None };
    let (order, ups) = {
        let s = state().lock().map_err(|_| std::io::Error::other("poisoned"))?;
        let prefer = sub_key.as_ref().and_then(|k| s.sticky.get(k).copied());
        (pick_order(&s.ups, prefer, Instant::now()), s.ups.iter().map(|u| u.base.clone()).collect::<Vec<_>>())
    };
    let tail = format!("{}{}", path.trim_start_matches("/v1/mining/"), if query.is_empty() { String::new() } else { format!("?{query}") });
    let mut tried = 0usize;
    for i in order {
        tried += 1;
        let t0 = Instant::now();
        // sigil-api's canonical prefix is /v1/; some nodes only route /api/v1/ (sigil-top's
        // embedded serve, older proxies). Try /v1/ first, /api/v1/ if that is not JSON.
        let send = |prefix: &str| {
            let u = format!("{}{prefix}/mining/{tail}", ups[i]);
            let req = if is_submit { http_client().post(u).header("content-type", "application/json").body(body.clone()) }
                      else { http_client().get(u) };
            req.send().ok().filter(|r| r.headers().get("content-type").and_then(|v| v.to_str().ok()).map(|c| c.contains("json")).unwrap_or(false))
        };
        let res = send("/v1").or_else(|| send("/api/v1")).ok_or(());
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        match res {
            Ok(r) if r.status().is_success() || r.status().is_client_error() => {
                let code = r.status().as_u16();
                let ct = r.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("application/json").to_string();
                let bytes = r.bytes().unwrap_or_default();
                let json_ok = ct.contains("json");
                if let Ok(mut s) = state().lock() {
                    s.ups[i].record(json_ok, ms, Instant::now());
                    if !json_ok { continue; }
                    if tried > 1 { s.stats.failovers += 1; }
                    if is_challenge {
                        s.stats.challenges += 1;
                        if let Some(h) = serde_json::from_slice::<serde_json::Value>(&bytes).ok().and_then(|v| v.get("height").and_then(|h| h.as_u64())) {
                            let k = (wallet.clone(), h);
                            if s.sticky.insert(k.clone(), i).is_none() { s.sticky_order.push_back(k); }
                            while s.sticky_order.len() > 256 { if let Some(old) = s.sticky_order.pop_front() { s.sticky.remove(&old); } }
                        }
                    }
                    if is_submit {
                        s.stats.submits += 1;
                        let txt = String::from_utf8_lossy(&bytes);
                        if code < 300 && (txt.contains("\"accepted\":true") || txt.contains("\"ok\":true")) { s.stats.accepted += 1 } else { s.stats.rejected += 1 }
                    }
                }
                if !json_ok { continue; }   // an HTML 200 (SPA fallback) is a missing route, not an answer
                return respond(&conn, code, &ct, &bytes);
            }
            _ => { if let Ok(mut s) = state().lock() { s.ups[i].record(false, ms, Instant::now()); } }
        }
    }
    respond(&conn, 502, "application/json", br#"{"ok":false,"error":"mining relay: no upstream node answered"}"#)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fastest_healthy_upstream_first_cooling_last_and_sticky_wins() {
        let now = Instant::now();
        let mut ups = vec![Upstream::new("a".into()), Upstream::new("b".into()), Upstream::new("c".into())];
        ups[0].ewma_ms = 120.0; ups[1].ewma_ms = 30.0; ups[2].ewma_ms = 10.0;
        ups[2].cool_until = Some(now + COOLDOWN);
        assert_eq!(pick_order(&ups, None, now), vec![1, 0, 2]);
        assert_eq!(pick_order(&ups, Some(0), now), vec![0, 1, 2]);
        assert_eq!(pick_order(&ups, Some(2), now)[0], 1, "a cooling upstream never jumps the queue");
    }

    #[test]
    fn three_failures_cool_an_upstream_and_one_success_heals_it() {
        let now = Instant::now();
        let mut u = Upstream::new("x".into());
        for _ in 0..3 { u.record(false, 0.0, now); }
        assert!(u.cooling(now));
        u.record(true, 50.0, now);
        assert!(!u.cooling(now) && u.ewma_ms == 50.0);
    }

    #[test]
    fn upstream_list_dedups_and_puts_the_local_producer_first() {
        let v = upstream_list(Some("http://127.0.0.1:18185".into()), Some("https://n1.example/ , https://sigilgraph.org"), "https://sigilgraph.org/");
        assert_eq!(v, vec!["http://127.0.0.1:18185", "https://n1.example", "https://sigilgraph.org"]);
    }
}
