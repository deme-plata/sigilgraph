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
    if sub.height != c.height {
        return false;
    }
    if sub.block.header != build_header(c, &sub.wallet) {
        return false;
    }
    verify_dual(g, &sub.block, c.blake4_target)
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
    pub fn fetch_challenge(&self) -> anyhow::Result<Challenge> {
        let url = format!("{}{}?wallet={}", self.endpoints.base_url, self.endpoints.challenge_path, self.wallet);
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
        let c = self.fetch_challenge()?;
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
                    let r = SubmitResult { accepted: verdict, reason: None };
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
        let c = Challenge { height: 42, vdf_input: [9u8; 32], blake4_target: 1, vdf_t: 1 };
        assert_eq!(build_header(&c, "alice"), build_header(&c, "alice"));
        assert_ne!(build_header(&c, "alice"), build_header(&c, "bob"));
        let mut c2 = c.clone();
        c2.height = 43;
        assert_ne!(build_header(&c, "alice"), build_header(&c2, "alice"));
    }
}
