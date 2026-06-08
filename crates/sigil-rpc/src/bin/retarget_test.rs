//! Verifies fix #2 (difficulty retarget) against a LIVE sigil-rpcd.
//! Mines one full RETARGET_WINDOW of blocks FAR faster than SIGIL_TARGET_BLOCK_MS,
//! then asserts the BLAKE4 target shrank (difficulty rose). Run a node with low
//! initial difficulty + fast VDF, e.g.:
//!   SIGIL_RPC_ADDR=127.0.0.1:8198 SIGIL_MINING_BLAKE4_BITS=8 SIGIL_MINING_VDF_T=20 \
//!     SIGIL_HISTORY_PATH=/tmp/rt-hist sigil-rpcd
//!   retarget_test 127.0.0.1:8198

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
    serde_json::from_str(&http(addr, "GET", &format!("/api/v1/mining/challenge?wallet={wallet}"), None)).expect("parse challenge")
}
fn submit(addr: &str, sub: &Submission) -> SubmitResult {
    serde_json::from_str(&http(addr, "POST", "/api/v1/mining/submit", Some(&serde_json::to_string(sub).unwrap())))
        .unwrap_or(SubmitResult { accepted: false, reason: Some("unparsed".into()) })
}

fn main() {
    let addr = std::env::args().nth(1).unwrap_or_else(|| "127.0.0.1:8198".into());
    let wallet = "a1b2c3d4".repeat(8);
    let g = ModSquaring::bench_2048();
    let t_start = get_challenge(&addr, &wallet).blake4_target;
    let mut accepted = 0;
    for _ in 0..18 {
        let c = get_challenge(&addr, &wallet);
        let block = mine_dual(&build_header(&c, &wallet), c.blake4_target, c.vdf_t, &g);
        let r = submit(&addr, &Submission { height: c.height, wallet: wallet.clone(), block });
        if r.accepted { accepted += 1; } else { println!("  reject h={}: {:?}", c.height, r.reason); }
    }
    let t_end = get_challenge(&addr, &wallet).blake4_target;
    let harder = t_end < t_start;
    println!("mined {accepted}/18 | target {:#018x} -> {:#018x} | difficulty rose: {}", t_start, t_end, harder);
    println!("{}", if harder && accepted >= 16 { "PASS — retarget raised difficulty when blocks came too fast ✓" } else { "FAIL ✗" });
    std::process::exit(if harder && accepted >= 16 { 0 } else { 1 });
}
