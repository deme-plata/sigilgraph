//! client.rs — the chain-agnostic challenge/submit mining client (design P0).
//!
//! Mirrors Quillon's proven thin-miner loop (`GET challenge → solve → POST
//! submit`), but the work is the **dual-lane** block (BLAKE4 Φ + VDF Ω) and the
//! endpoint is configurable so the same miner drives Quillon, SIGIL, or any Flux
//! chain. The node-side check ([`check_submission`]) is shared by the miner's
//! test mock and a real node — one verification rule, both sides.

use crate::{mine_dual, verify_dual, DualLaneBlock};
use flux_vdf::VdfGroup;
use serde::{Deserialize, Serialize};
#[cfg(feature = "client")]
use std::time::{Duration, Instant};

/// A node-issued mining challenge.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Challenge {
    pub height: u64,
    /// Node-issued VDF seed material (binds the work to this height/tip).
    pub vdf_input: [u8; 32],
    /// Lane A difficulty: the BLAKE4 hash word must be `<= blake4_target`.
    pub blake4_target: u64,
    /// Lane B difficulty: number of sequential VDF squarings required.
    pub vdf_t: u64,
    /// v7.0.9-fix: TOTAL network hashrate the node measures by summing every active miner's
    /// self-reported rate (each miner sends `&hps=` on the challenge fetch). Defaults to 0 so a
    /// pre-fix node/client still parses. This is the ACCURATE total power — unlike a difficulty
    /// estimate, which undercounts when the difficulty is pinned + submissions are throttled.
    #[serde(default)]
    pub net_hps: f64,
    /// POOL-SHARES (v7.1.0): sub-difficulty share target — `blake4_target << ease`
    /// bits easier than the block target. 0 = node runs solo mode (pre-pool node,
    /// or pool disabled); a pool-aware miner then behaves exactly as before. When
    /// >0 the miner submits every solve `<= share_target`; the node credits shares
    /// proportionally when a full-difficulty solve lands the block. Defaults to 0
    /// so both directions stay wire-compatible with pre-7.1 peers.
    #[serde(default)]
    pub share_target: u64,
    /// 2026-08-20 (the VDF-bound hashrate-collapse fix): sequential VDF turns
    /// REQUIRED for a share-grade submission — deliberately much smaller than
    /// `vdf_t` (the block's depth). Before this field existed, a share was
    /// held to the SAME `vdf_t` as a full block (see `check_submission_at`'s
    /// doc) — fine for anti-forgery, but once VARDIFF pushes the hash target
    /// easy enough that a nonce is found in microseconds, the fixed, purely-
    /// sequential VDF cost (identical on every CPU/GPU regardless of raw hash
    /// power) becomes the entire cycle time. Every miner — 5 MH/s or 500
    /// MH/s — converges to the same VDF-bound share rate, and hashrate stops
    /// differentiating anyone. A separate, much smaller `share_vdf_t` still
    /// proves genuine sequential work per share (can't be forged with a t=0
    /// instant proof) without VDF dominating a cycle that's supposed to be
    /// dominated by the hash search. 0 when `share_target` is also 0 (solo
    /// mode; unused). Defaults 0 on deserialize so a pre-fix peer still
    /// parses this wire format.
    #[serde(default)]
    pub share_vdf_t: u64,
}

/// A solved share submitted back to the node.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Submission {
    pub height: u64,
    pub wallet: String,
    pub block: DualLaneBlock,
}

/// The node's verdict on a submitted share.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SubmitResult {
    pub accepted: bool,
    pub reason: Option<String>,
    /// POOL-SHARES: true when this accept was a sub-difficulty SHARE (credited at
    /// the next block's payout), false when it was the block itself. Defaults
    /// false so a pre-7.1 node's reply parses as "block" — the solo semantics.
    #[serde(default)]
    pub share: bool,
}

/// Chain-agnostic endpoint config. Defaults match Quillon's `/api/v1/mining/*`.
#[derive(Clone, Debug)]
pub struct Endpoints {
    pub base_url: String,
    pub challenge_path: String,
    pub submit_path: String,
}

impl Endpoints {
    /// Quillon-style endpoints (also the SIGIL default until it diverges).
    pub fn standard(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            challenge_path: "/api/v1/mining/challenge".into(),
            submit_path: "/api/v1/mining/submit".into(),
        }
    }
}

/// Deterministic block header derived from a challenge — both the miner (to mine)
/// and the node (to verify) build the identical header, so a share can't claim a
/// different height/seed/wallet than it was issued for.
pub fn build_header(c: &Challenge, wallet: &str) -> Vec<u8> {
    let mut h = blake3::Hasher::new();
    h.update(b"flux-miner/header/v1");
    h.update(&c.height.to_le_bytes());
    h.update(&c.vdf_input);
    h.update(wallet.as_bytes());
    h.finalize().as_bytes().to_vec()
}

/// Do the WORK: solve a challenge into a dual-lane block (BLAKE4 nonce search to
/// `blake4_target`, then the VDF for `vdf_t` sequential turns).
pub fn solve<G: VdfGroup>(c: &Challenge, wallet: &str, g: &G) -> DualLaneBlock {
    let header = build_header(c, wallet);
    mine_dual(&header, c.blake4_target, c.vdf_t, g)
}

/// The consensus gate a node applies to a submitted share — height match, header
/// binding, then BOTH lanes verified. Shared by the mock node and a real node.
pub fn check_submission<G: VdfGroup>(g: &G, c: &Challenge, sub: &Submission) -> bool {
    check_submission_at(g, c, sub, c.blake4_target, c.vdf_t)
}

/// [`check_submission`] with an explicit Lane-A target AND the Lane-B depth
/// required for THAT target — the ONE verification rule at two difficulties:
/// the node calls this with `(blake4_target, vdf_t)` for the block gate and
/// with `(share_target, share_vdf_t)` for the POOL-SHARES sub-difficulty
/// gate, so a share is held to the identical height/header/VDF-BINDING as a
/// block, just at a shallower REQUIRED depth (2026-08-20: was always
/// `c.vdf_t` regardless of `target` — fine for the block gate, but it meant
/// every pool share paid the full block's fixed sequential VDF cost even
/// though the hash target was trivially easy, capping every miner's
/// effective rate at the same hardware-independent VDF-bound ceiling
/// regardless of raw hash power; see `Challenge::share_vdf_t`'s doc).
pub fn check_submission_at<G: VdfGroup>(
    g: &G,
    c: &Challenge,
    sub: &Submission,
    target: u64,
    required_vdf_t: u64,
) -> bool {
    if sub.height != c.height {
        return false;
    }
    if sub.block.header != build_header(c, &sub.wallet) {
        return false;
    }
    // Enforce the REQUIRED sequential work (Lane B time): without this, a t=1 (instant)
    // VDF proof would verify as "a valid VDF" and bypass the time lane entirely. The proof
    // must commit to exactly the REQUIRED depth for this target — the block's `vdf_t` when
    // checking against `blake4_target`, the (much smaller) `share_vdf_t` when checking
    // against `share_target`. A share cannot claim the block's depth to "upgrade" itself —
    // the caller decides which target/depth PAIR it's checking, never a mismatched pair.
    if sub.block.vdf.t != required_vdf_t {
        return false;
    }
    verify_dual(g, &sub.block, target)
}

/// Live mining stats (what `flux_miner_status` will surface).
#[derive(Clone, Debug, Default)]
pub struct MineStats {
    pub shares_accepted: u64,
    pub shares_rejected: u64,
    pub challenges_fetched: u64,
    pub fetch_errors: u64,
    pub last_solve_ms: f64,
    pub last_height: u64,
}

/// The HTTP mining client. Gated behind the `client` feature so a node that
/// only needs the verification gate ([`check_submission`]) can depend on
/// flux-miner without pulling in reqwest.
/// POOL-DIAG: the version string reported to the node on every challenge fetch
/// (`&v=`), so the pool's /mining/miners can tell which build a rig actually
/// runs (the HiveOS installer keeps stale binaries — the node needs to see it).
/// The embedding binary (sigil-top) sets this ONCE at startup to its own release
/// version; unset it falls back to this engine crate's version. std-only, so it
/// lives outside the `client` feature gate.
pub static CLIENT_VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// POOL / MULTI-RIG (2026-08-01): a stable per-PROCESS rig identity, reported to
/// the pool as `&rig=` on every challenge fetch.
///
/// ## Why this exists
///
/// The pool keys every per-RIG quantity — the reported hashrate, the issued
/// vardiff share ease, the client version, the per-height share cap — by WALLET.
/// But a farm points N machines at ONE payout address, so those N rigs share one
/// slot each and clobber one another. Measured live on `:8099` (2026-08-01, 14
/// samples x 15s): one wallet reported two different client builds, flipping 5
/// times; another swung its reported rate 3,249..7,233,438 h/s (2226x) between
/// samples. Both are two machines behind one address, not one noisy machine.
///
/// The damage is not cosmetic: the pool derives a vardiff ease from whichever rig
/// reported LAST, then verifies the OTHER rig's in-flight share against it. An
/// ease swing of 8 -> 1 is a 2^7 = 128x harder target, so honest work — work the
/// pool itself asked for — is rejected as `verify_mismatch`. A rig id lets the
/// pool hold that state per machine instead.
///
/// ## Compatibility
///
/// PURELY ADDITIVE. The node reads named query keys, so a pool that does not know
/// `rig` simply ignores it and the exchange is byte-identical to today's. That is
/// deliberate: rigs are real hardware pointed at a real URL, and a change that can
/// strand them is the one unacceptable outcome. This is safe to ship before the
/// server half exists.
///
/// ## Value
///
/// Precedence: `SIGIL_RIG_ID` (operator-set — authoritative, and the right knob
/// for a named farm) -> `<hostname>-<pid>` -> `rig-<pid>`. The pid keeps two
/// miners on ONE host distinct; the hostname keeps two hosts distinct. Sanitised
/// to the charset and 24-byte bound the pool already enforces on `&v=`, so a
/// hostile or malformed value cannot smuggle JSON or bloat the pool's map.
pub static RIG_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// This process's rig id — see [`RIG_ID`]. Resolved once, then stable for the
/// process lifetime (the pool needs it stable to key state on).
pub fn rig_id() -> &'static str {
    RIG_ID.get_or_init(default_rig_id)
}

/// Keep only what the pool's `&v=` validator already accepts (ASCII
/// alphanumeric, `.`, `-`, `+`), map everything else to `-`, and bound the
/// length. Returns `None` for a value with no usable characters so the caller
/// falls through to the next source rather than reporting an empty id.
fn sanitize_rig_id(raw: &str) -> Option<String> {
    let s: String = raw
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '+' { c } else { '-' })
        .take(24)
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// The host part of the default rig id, portable across the rigs we actually
/// ship to: Linux/HiveOS (`HOSTNAME`, else `/etc/hostname`) and Windows
/// (`COMPUTERNAME`, which is where sigil-top.exe runs).
fn host_label() -> Option<String> {
    for key in ["HOSTNAME", "COMPUTERNAME"] {
        if let Ok(h) = std::env::var(key) {
            if let Some(s) = sanitize_rig_id(&h) { return Some(s); }
        }
    }
    std::fs::read_to_string("/etc/hostname").ok().and_then(|h| sanitize_rig_id(&h))
}

fn default_rig_id() -> String {
    if let Ok(v) = std::env::var("SIGIL_RIG_ID") {
        if let Some(s) = sanitize_rig_id(&v) { return s; }
    }
    let pid = std::process::id().to_string();
    match host_label() {
        Some(h) => {
            // Budget the 24-byte bound so the PID ALWAYS survives. Truncating the
            // joined string instead cuts exactly the wrong end: this box is
            // `cs30067.seedhost.eu` (19 chars), so `<host>-<pid>` is 27 and naive
            // truncation yielded "cs30067.seedhost.eu-2238" — pid amputated. Two
            // miners on one host would then collide on ONE rig id, which is the
            // wallet-keyed bug this whole change exists to fix, moved down a level.
            // (Caught by `default_rig_id_distinguishes_processes_on_one_host`.)
            let room = 24usize.saturating_sub(pid.len() + 1);
            let host: String = h.chars().take(room).collect();
            let host = host.trim_matches('-');
            if host.is_empty() { format!("rig-{pid}") } else { format!("{host}-{pid}") }
        }
        None => format!("rig-{pid}"),
    }
}

#[cfg(feature = "client")]
pub struct MinerClient {
    pub endpoints: Endpoints,
    pub wallet: String,
    http: reqwest::blocking::Client,
}

#[cfg(feature = "client")]
impl MinerClient {
    pub fn new(endpoints: Endpoints, wallet: impl Into<String>) -> anyhow::Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent(concat!("flux-miner/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { endpoints, wallet: wallet.into(), http })
    }

    /// `GET {base}{challenge_path}?wallet=<wallet>` → [`Challenge`].
    ///
    /// `kind` is `"cpu"` or `"gpu"` — the caller always knows unambiguously
    /// which lane it is (a process runs either `mining_loop` or
    /// `gpu_mining_loop`, never both), so this is a plain string, not a
    /// hardware probe. Additive: a pool that doesn't read `&kind=` is
    /// unaffected, and the server treats a missing/unrecognized value as
    /// `MinerKind::Unknown` — same backward-compat shape as `&rig=`.
    pub fn fetch_challenge(&self, hps: f64, kind: &str) -> anyhow::Result<Challenge> {
        // v7.0.9-fix: report our measured hashrate so the node can SUM active miners into the
        // true total network power (returned as Challenge.net_hps). `hps=0` on the first fetch.
        let v = CLIENT_VERSION.get().map(String::as_str).unwrap_or(env!("CARGO_PKG_VERSION"));
        // MULTI-RIG: identify THIS machine, not just the payout wallet — see [`RIG_ID`].
        // Additive; a pool that does not read `rig` is unaffected.
        let url = format!(
            "{}{}?wallet={}&hps={:.0}&v={}&rig={}&kind={}",
            self.endpoints.base_url, self.endpoints.challenge_path,
            self.wallet, hps.max(0.0), v, rig_id(), kind
        );
        let c = self.http.get(&url).send()?.error_for_status()?.json::<Challenge>()?;
        Ok(c)
    }

    /// `POST {base}{submit_path}` with the [`Submission`] JSON → [`SubmitResult`].
    pub fn submit(&self, sub: &Submission) -> anyhow::Result<SubmitResult> {
        let url = format!("{}{}", self.endpoints.base_url, self.endpoints.submit_path);
        let r = self.http.post(&url).json(sub).send()?.error_for_status()?.json::<SubmitResult>()?;
        Ok(r)
    }

    /// One full iteration: fetch → solve → submit. Returns the node's verdict.
    pub fn mine_one<G: VdfGroup>(&self, g: &G, stats: &mut MineStats) -> anyhow::Result<SubmitResult> {
        // Generic single-shot helper, not the production CPU/GPU loops
        // (`engine.rs`'s `mining_loop`/`gpu_mining_loop`) — genuinely doesn't
        // know which hardware it's running on, so reports honestly as such.
        let c = self.fetch_challenge(0.0, "unknown")?;
        stats.challenges_fetched += 1;
        stats.last_height = c.height;
        let t = Instant::now();
        let block = solve(&c, &self.wallet, g);
        stats.last_solve_ms = t.elapsed().as_secs_f64() * 1000.0;
        let sub = Submission { height: c.height, wallet: self.wallet.clone(), block };
        let r = self.submit(&sub)?;
        if r.accepted {
            stats.shares_accepted += 1;
        } else {
            stats.shares_rejected += 1;
        }
        Ok(r)
    }

    /// Mine until `max_blocks` shares are processed (None = forever). `poll`
    /// throttles between iterations; fetch errors back off, don't crash.
    pub fn mine_loop<G: VdfGroup>(&self, g: &G, max_blocks: Option<u64>, poll: Duration, stats: &mut MineStats) {
        loop {
            match self.mine_one(g, stats) {
                Ok(_) => {}
                Err(_) => {
                    stats.fetch_errors += 1;
                    std::thread::sleep(poll.max(Duration::from_secs(1)));
                }
            }
            if let Some(m) = max_blocks {
                if stats.shares_accepted + stats.shares_rejected >= m {
                    break;
                }
            }
            std::thread::sleep(poll);
        }
    }
}

// The roundtrip test spins a tiny_http mock node AND the reqwest MinerClient, so
// it needs both features. The pure header test lives in `core_tests` below.
#[cfg(all(test, feature = "client", feature = "node"))]
mod tests {
    use super::*;
    use flux_vdf::ModSquaring;
    use std::io::Read;
    use tiny_http::{Response, Server};

    /// End-to-end: a tiny_http MOCK NODE issues a challenge, the MinerClient
    /// fetches it, solves the dual-lane block, submits it, and the node verifies
    /// it with the SHARED `check_submission` gate → accepted. Proves the whole
    /// challenge/solve/submit loop with no external network.
    #[test]
    fn challenge_solve_submit_roundtrip() {
        let server = Server::http("127.0.0.1:0").expect("mock node");
        let addr = server.server_addr().to_ip().expect("ip addr");
        let base = format!("http://{addr}");

        let challenge = Challenge {
            height: 7,
            vdf_input: [3u8; 32],
            blake4_target: u64::MAX >> 12, // easy so the test is fast
            vdf_t: 800,
            net_hps: 0.0,
            share_target: 0,
            share_vdf_t: 0, // solo mode (share_target == 0); unused
        };
        let node_challenge = challenge.clone();

        // Mock node: serve one challenge, verify one submit, then stop.
        let node = std::thread::spawn(move || {
            let g = ModSquaring::bench_2048();
            let mut verdict = false;
            for mut req in server.incoming_requests() {
                let url = req.url().to_string();
                if url.starts_with("/api/v1/mining/challenge") {
                    let body = serde_json::to_string(&node_challenge).unwrap();
                    let _ = req.respond(Response::from_string(body));
                } else if url.starts_with("/api/v1/mining/submit") {
                    let mut s = String::new();
                    let _ = req.as_reader().read_to_string(&mut s);
                    let sub: Submission = serde_json::from_str(&s).unwrap();
                    verdict = check_submission(&g, &node_challenge, &sub);
                    let r = SubmitResult { accepted: verdict, reason: None, ..Default::default() };
                    let _ = req.respond(Response::from_string(serde_json::to_string(&r).unwrap()));
                    break;
                }
            }
            verdict
        });

        let g = ModSquaring::bench_2048();
        let client = MinerClient::new(Endpoints::standard(base), "qnk_test_miner").unwrap();
        let mut stats = MineStats::default();
        let result = client.mine_one(&g, &mut stats).expect("mine_one");

        assert!(result.accepted, "node must accept a correctly solved dual-lane share");
        assert_eq!(stats.shares_accepted, 1);
        assert_eq!(stats.last_height, 7);
        assert!(node.join().unwrap(), "node-side verify must pass");
    }
}

// The header-binding test is pure (no HTTP, no mock node) — always compiled so
// the reqwest-free core stays covered even under default-features = false.
#[cfg(test)]
mod core_tests {
    use super::*;

    #[test]
    fn header_is_deterministic_and_binding() {
        let c = Challenge { height: 42, vdf_input: [9u8; 32], blake4_target: 1, vdf_t: 1, net_hps: 0.0, share_target: 0, share_vdf_t: 0 };
        assert_eq!(build_header(&c, "alice"), build_header(&c, "alice"));
        assert_ne!(build_header(&c, "alice"), build_header(&c, "bob"));
        let mut c2 = c.clone();
        c2.height = 43;
        assert_ne!(build_header(&c, "alice"), build_header(&c2, "alice"));
    }

    // ── MULTI-RIG rig-id (see [`RIG_ID`]) ────────────────────────────────────
    // These are deliberately PURE (no `set_var`): the resolution order reads
    // process-global env, and mutating it from a test races every other test in
    // the binary. The sanitiser is where the safety properties live, so that is
    // what is pinned; `rig_id()` itself is only checked for the invariants that
    // hold regardless of environment.

    #[test]
    fn rig_id_sanitiser_enforces_the_pools_charset_and_bound() {
        // The pool accepts [A-Za-z0-9.-+] and bounds length at 24; anything else
        // must be neutralised rather than passed through.
        assert_eq!(sanitize_rig_id("hive-rig-01"), Some("hive-rig-01".into()));
        assert_eq!(sanitize_rig_id("  spaced  "), Some("spaced".into()));
        // Quotes/braces are the smuggling risk — /mining/miners emits hand-rolled
        // JSON, so a raw `"` in a rig id would break the document.
        let dirty = sanitize_rig_id("a\"b{}c").expect("has usable chars");
        assert!(!dirty.contains('"') && !dirty.contains('{') && !dirty.contains('}'));
        // Bound holds for a long hostname.
        let long = sanitize_rig_id(&"x".repeat(100)).expect("has usable chars");
        assert!(long.len() <= 24, "rig id must respect the 24-byte bound, got {}", long.len());
        // No usable characters -> None, so the caller falls through to the next
        // source instead of reporting an empty id.
        assert_eq!(sanitize_rig_id("   "), None);
        assert_eq!(sanitize_rig_id("---"), None);
    }

    #[test]
    fn rig_id_is_nonempty_stable_and_url_safe() {
        let a = rig_id();
        let b = rig_id();
        // Stable: the pool keys per-rig state on this, so it must not move.
        assert_eq!(a, b, "rig id must be stable for the process lifetime");
        assert!(!a.is_empty(), "rig id must never be empty");
        assert!(a.len() <= 24, "rig id must respect the 24-byte bound");
        assert!(
            a.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '+'),
            "rig id must be URL-safe without escaping, got {a:?}"
        );
    }

    #[test]
    fn default_rig_id_distinguishes_processes_on_one_host() {
        // The whole point of the pid suffix: two miners on ONE machine must not
        // collapse into one pool identity (that is the wallet-keyed bug, moved
        // down a level). Same host => id still carries this process's pid.
        let id = default_rig_id();
        assert!(
            id.contains(&std::process::id().to_string()) || std::env::var("SIGIL_RIG_ID").is_ok(),
            "default rig id must carry the pid unless explicitly overridden, got {id:?}"
        );
    }
}
