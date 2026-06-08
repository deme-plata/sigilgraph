//! Verifies fix #1 (dual-lane precompute defense) against a LIVE sigil-rpcd.
//!
//! Run a node first, e.g.:
//!   SIGIL_RPC_ADDR=127.0.0.1:8199 SIGIL_MINING_BLAKE4_BITS=10 SIGIL_MINING_VDF_T=50 \
//!     SIGIL_HISTORY_PATH=/tmp/pt-hist sigil-rpcd
//! then:  precompute_test 127.0.0.1:8199
//!
//! Asserts: (1) honest mine on the served tip is ACCEPTED; (2) a block mined for a
//! FUTURE height with an offline-guessed seed is REJECTED (the old attack); (3) a
//! valid block replayed after the tip advanced is REJECTED (stale height).

use std::io::{Read, Write};
use std::net::TcpStream;
use flux_miner::client::{build_header, Challenge, Submission, SubmitResult};
use flux_miner::mine_dual;
use flux_vdf::ModSquaring;

fn http(addr: &str, method: &str, path: &str, body: Option<&str>) -> String {
    let mut s = TcpStream::connect(addr).expect("connect");
    let req = match body {
        Some(b) => format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}", b.len()),
        None => format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"),
    };
    s.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out.split("\r\n\r\n").nth(1).unwrap_or("").to_string()
}

fn get_challenge(addr: &str, wallet: &str) -> Challenge {
    let body = http(addr, "GET", &format!("/api/v1/mining/challenge?wallet={wallet}"), None);
    serde_json::from_str(&body).expect("parse challenge")
}
fn submit(addr: &str, sub: &Submission) -> SubmitResult {
    let body = http(addr, "POST", "/api/v1/mining/submit", Some(&serde_json::to_string(sub).unwrap()));
    serde_json::from_str(&body).unwrap_or(SubmitResult { accepted: false, reason: Some(format!("unparsed: {body}")) })
}

fn main() {
    let addr = std::env::args().nth(1).unwrap_or_else(|| "127.0.0.1:8199".into());
    let wallet = "a1b2c3d4".repeat(8); // 64-hex test wallet
    let g = ModSquaring::bench_2048();
    let mut pass = true;
    let mut check = |name: &str, cond: bool, detail: String| {
        println!("[{}] {name} — {detail}", if cond { "PASS" } else { "FAIL" });
        pass &= cond;
    };

    // (1) HONEST: mine the served tip → must be ACCEPTED.
    let c = get_challenge(&addr, &wallet);
    let h0 = c.height;
    let honest_block = mine_dual(&build_header(&c, &wallet), c.blake4_target, c.vdf_t, &g);
    let honest_sub = Submission { height: h0, wallet: wallet.clone(), block: honest_block.clone() };
    let r = submit(&addr, &honest_sub);
    check("honest-mine-current-tip", r.accepted, format!("h={h0} accepted={} reason={:?}", r.accepted, r.reason));

    // (2) PRECOMPUTE FUTURE HEIGHT (the old attack): build a challenge for a future
    // height with the only thing an attacker can compute offline — a guessed seed
    // (here: the legacy height-only seed). Mine a fully valid dual-lane block for it.
    let future_h = h0 + 1; // node is now at h0+1 after the honest accept
    let mut guessed_seed = [0u8; 32];
    guessed_seed[..8].copy_from_slice(&future_h.to_le_bytes()); // legacy height-only derivation
    let forged = Challenge { height: future_h, vdf_input: guessed_seed, blake4_target: c.blake4_target, vdf_t: c.vdf_t };
    let pre_block = mine_dual(&build_header(&forged, &wallet), forged.blake4_target, forged.vdf_t, &g);
    let pre_sub = Submission { height: future_h, wallet: wallet.clone(), block: pre_block };
    let r = submit(&addr, &pre_sub);
    check("precompute-future-height-REJECTED", !r.accepted, format!("h={future_h} accepted={} reason={:?}", r.accepted, r.reason));

    // (3) REPLAY after tip advanced: resubmit the honest block (height h0) now that
    // the tip is past h0 → stale → REJECTED.
    let r = submit(&addr, &honest_sub);
    check("replay-stale-height-REJECTED", !r.accepted, format!("h={h0} accepted={} reason={:?}", r.accepted, r.reason));

    println!("\n{}", if pass { "ALL CHECKS PASSED — precompute attack closed ✓" } else { "SOME CHECKS FAILED ✗" });
    std::process::exit(if pass { 0 } else { 1 });
}
