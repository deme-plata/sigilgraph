//! Verifies fix #3 (VDF-chaining) against a LIVE, FRESH sigil-rpcd.
//! Tracks the chain tip from genesis, folding each block's VDF output in exactly as
//! the node does, and proves:
//!   (A) the VDF-inclusive tip predicts EVERY served challenge seed (tip tracking),
//!   (B) a tip computed WITHOUT the VDF output mispredicts the next challenge
//!       → the VDF output is load-bearing: you can't know height H+1's challenge
//!         until you've run height H's VDF.  → the VDFs form one sequential chain.
//! Run a FRESH node (height 2, genesis tip):
//!   SIGIL_RPC_ADDR=127.0.0.1:8197 SIGIL_MINING_BLAKE4_BITS=8 SIGIL_MINING_VDF_T=20 \
//!     SIGIL_HISTORY_PATH=/tmp/vc-hist sigil-rpcd
//!   vdfchain_test 127.0.0.1:8197

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
    let mut o = String::new(); s.read_to_string(&mut o).unwrap();
    o.split("\r\n\r\n").nth(1).unwrap_or("").to_string()
}
fn challenge(addr: &str, w: &str) -> Challenge { serde_json::from_str(&http(addr, "GET", &format!("/api/v1/mining/challenge?wallet={w}"), None)).expect("challenge") }
fn submit(addr: &str, s: &Submission) -> SubmitResult { serde_json::from_str(&http(addr, "POST", "/api/v1/mining/submit", Some(&serde_json::to_string(s).unwrap()))).unwrap_or_default() }

// Exact replicas of the node's derivations (sigil-rpcd.rs).
fn b3(parts: &[&[u8]]) -> [u8; 32] { let mut h = blake3::Hasher::new(); for p in parts { h.update(p); } *h.finalize().as_bytes() }
fn genesis() -> [u8; 32] { *blake3::hash(b"sigil-g0/mining-genesis").as_bytes() }
fn mining_seed(tip: &[u8; 32], h: u64) -> [u8; 32] { b3(&[b"sigil-g0/mining-challenge/v1", tip, &h.to_le_bytes()]) }
fn tip_v2(tip: &[u8; 32], h: u64, b4: u64, nonce: u64, vdf_json: &[u8]) -> [u8; 32] {
    let vc = blake3::hash(vdf_json);
    b3(&[b"sigil-g0/tip/v2", tip, &h.to_le_bytes(), &b4.to_le_bytes(), &nonce.to_le_bytes(), vc.as_bytes()])
}
fn tip_no_vdf(tip: &[u8; 32], h: u64, b4: u64, nonce: u64) -> [u8; 32] {
    b3(&[b"sigil-g0/tip/v2", tip, &h.to_le_bytes(), &b4.to_le_bytes(), &nonce.to_le_bytes()]) // omits the VDF commit
}

fn main() {
    let addr = std::env::args().nth(1).unwrap_or_else(|| "127.0.0.1:8197".into());
    let w = "a1b2c3d4".repeat(8);
    let g = ModSquaring::bench_2048();
    let mut tip = genesis();
    let (mut tip_before, mut lh, mut lb4, mut ln) = (tip, 0u64, 0u64, 0u64);
    let mut all_match = true; let mut accepted = 0;
    for _ in 0..8 {
        let c = challenge(&addr, &w);
        if c.vdf_input != mining_seed(&tip, c.height) { all_match = false; println!("  seed mismatch h={}", c.height); }
        let block = mine_dual(&build_header(&c, &w), c.blake4_target, c.vdf_t, &g);
        let r = submit(&addr, &Submission { height: c.height, wallet: w.clone(), block: block.clone() });
        if !r.accepted { println!("  reject h={}: {:?}", c.height, r.reason); continue; }
        accepted += 1;
        tip_before = tip; lh = c.height; lb4 = block.blake4_hash; ln = block.nonce;
        tip = tip_v2(&tip, c.height, block.blake4_hash, block.nonce, &serde_json::to_vec(&block.vdf).unwrap());
    }
    // (A) the VDF-inclusive tip predicted every served challenge
    let a = all_match && accepted >= 6;
    println!("[{}] (A) VDF-inclusive tip predicts every challenge — {accepted}/8 mined, all_match={all_match}", if a { "PASS" } else { "FAIL" });

    // (B) the VDF output is load-bearing: omitting it mispredicts the next challenge
    let actual_next = challenge(&addr, &w).vdf_input; // for height lh+1
    let with_vdf = mining_seed(&tip, lh + 1);
    let no_vdf = mining_seed(&tip_no_vdf(&tip_before, lh, lb4, ln), lh + 1);
    let b = actual_next == with_vdf && actual_next != no_vdf;
    println!("[{}] (B) VDF output is load-bearing — actual==with_vdf:{} actual!=no_vdf:{}",
        if b { "PASS" } else { "FAIL" }, actual_next == with_vdf, actual_next != no_vdf);

    println!("\n{}", if a && b { "ALL CHECKS PASSED — VDFs chain into one sequential timeline ✓" } else { "SOME CHECKS FAILED ✗" });
    std::process::exit(if a && b { 0 } else { 1 });
}
