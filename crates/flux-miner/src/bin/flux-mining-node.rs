//! flux-mining-node — a REFERENCE mining node for flux-miner.
//!
//! Issues dual-lane challenges and verifies submitted shares with the crate's
//! OWN [`check_submission`] gate — the same rule the miner's test uses. This is
//! what lets `flux-miner mine <url> <wallet>` run end-to-end on REAL binaries
//! today, before a full chain (SIGIL VDF block production — design P4) exposes
//! `/mining/*`. Promotes the unit-test mock into a runnable node.
//!
//!   flux-mining-node [listen_addr] [blake4_bits] [vdf_t]
//!   defaults:        127.0.0.1:8645  16           600

use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};

use flux_miner::client::{check_submission, Challenge, Submission, SubmitResult};
use flux_vdf::ModSquaring;
use tiny_http::{Header, Method, Response, Server};

/// Deterministic per-height VDF seed (binds a challenge to its height).
fn seed_for(height: u64) -> [u8; 32] {
    let mut s = [0u8; 32];
    s[..8].copy_from_slice(&height.to_le_bytes());
    s
}

fn json(body: String) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body).with_header("Content-Type: application/json".parse::<Header>().unwrap())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let addr = args.get(1).cloned().unwrap_or_else(|| "127.0.0.1:8645".into());
    let bits: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(16);
    let vdf_t: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(600);
    let blake4_target = u64::MAX >> bits;

    let g = ModSquaring::bench_2048(); // must match the miner's group
    let height = AtomicU64::new(1);
    let server = Server::http(addr.as_str()).expect("bind");
    println!("flux-mining-node on http://{addr}  (BLAKE4 {bits}-bit target · vdf_t={vdf_t})");
    println!("  GET  /api/v1/mining/challenge?wallet=X");
    println!("  POST /api/v1/mining/submit");

    for mut req in server.incoming_requests() {
        let url = req.url().to_string();
        let is_get = req.method() == &Method::Get;
        let is_post = req.method() == &Method::Post;

        if is_get && url.starts_with("/api/v1/mining/challenge") {
            let h = height.load(Ordering::SeqCst);
            let c = Challenge { height: h, vdf_input: seed_for(h), blake4_target, vdf_t };
            let _ = req.respond(json(serde_json::to_string(&c).unwrap()));
        } else if is_post && url.starts_with("/api/v1/mining/submit") {
            let mut body = String::new();
            let _ = req.as_reader().read_to_string(&mut body);
            let result = match serde_json::from_str::<Submission>(&body) {
                Ok(sub) => {
                    // Reconstruct the challenge this share's height was issued under
                    // (target + vdf_t are fixed; seed is height-derived), then verify.
                    let c = Challenge { height: sub.height, vdf_input: seed_for(sub.height), blake4_target, vdf_t };
                    if check_submission(&g, &c, &sub) {
                        let now = height.fetch_add(1, Ordering::SeqCst) + 1;
                        println!("  ✓ accepted h={} from {} → height now {now}", sub.height, sub.wallet);
                        SubmitResult { accepted: true, reason: None }
                    } else {
                        println!("  ✗ rejected h={} from {}", sub.height, sub.wallet);
                        SubmitResult { accepted: false, reason: Some("dual-lane verify / height / header mismatch".into()) }
                    }
                }
                Err(e) => SubmitResult { accepted: false, reason: Some(format!("bad submission json: {e}")) },
            };
            let _ = req.respond(json(serde_json::to_string(&result).unwrap()));
        } else {
            let _ = req.respond(Response::from_string(
                "flux-mining-node: GET /api/v1/mining/challenge?wallet=X | POST /api/v1/mining/submit\n",
            ));
        }
    }
}
