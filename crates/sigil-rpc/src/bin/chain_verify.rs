//! Decentralization ①: independently RE-VERIFY a producer's whole mining chain over
//! HTTP (verify-don't-trust). Walks block 0..tip from genesis, re-deriving each challenge
//! from the running verified tip and re-checking the dual-lane proof + reward + tip-fold —
//! so a peer can detect if the producer rewrote history. Also a tamper test (must reject).
//!   chain_verify 127.0.0.1:8193

use std::io::{Read, Write};
use std::net::TcpStream;
use flux_miner::client::{check_submission, Challenge, Submission};
use flux_vdf::ModSquaring;
use serde_json::Value;

fn http(addr: &str, path: &str) -> String {
    let mut s = TcpStream::connect(addr).expect("connect");
    let _ = s.write_all(format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes());
    let mut o = String::new(); let _ = s.read_to_string(&mut o);
    o.split("\r\n\r\n").nth(1).unwrap_or("").to_string()
}
fn b3(parts: &[&[u8]]) -> [u8; 32] { let mut h = blake3::Hasher::new(); for p in parts { h.update(p); } *h.finalize().as_bytes() }
fn genesis() -> [u8; 32] { *blake3::hash(b"sigil-g0/mining-genesis").as_bytes() }
fn mining_seed(tip: &[u8; 32], h: u64) -> [u8; 32] { b3(&[b"sigil-g0/mining-challenge/v1", tip, &h.to_le_bytes()]) }
fn tip_v2(tip: &[u8; 32], bh: u64, b4: u64, nonce: u64, vdf_json: &[u8]) -> [u8; 32] {
    let vc = blake3::hash(vdf_json);
    b3(&[b"sigil-g0/tip/v2", tip, &bh.to_le_bytes(), &b4.to_le_bytes(), &nonce.to_le_bytes(), vc.as_bytes()])
}
fn target_from_bits(bits: u32) -> u64 { u64::MAX >> bits.min(63) }
fn hx(s: &str) -> [u8; 32] { let mut o = [0u8; 32]; for i in 0..32 { o[i] = u8::from_str_radix(&s[i*2..i*2+2], 16).unwrap_or(0); } o }

/// Independently verify one block against `my_tip`. Returns `(new_tip, this_block_ts_us)` —
/// the ts is threaded into the next call as `prev_ts` for the time-based reward check.
/// `genesis_ts == 0` → legacy block-based chain (no time anchor).
fn verify_block(rec: &Value, my_tip: &[u8; 32], g: &ModSquaring, genesis_ts: u128, prev_ts: u128, carry_in: u128) -> Result<([u8; 32], u128, u128), String> {
    let bh = rec["height"].as_u64().ok_or("no height")?;
    if hx(rec["prev_tip"].as_str().unwrap_or("")) != *my_tip { return Err(format!("block {bh}: linkage break (prev_tip != my tip)")); }
    let bits = rec["bits"].as_u64().unwrap_or(0) as u32;
    let vdf_t = rec["vdf_t"].as_u64().unwrap_or(0);
    let reward: u128 = rec["reward"].as_str().unwrap_or("0").parse().unwrap_or(0);
    let rec_ts: u128 = rec["ts"].as_str().unwrap_or("0").parse().unwrap_or(0);
    let sub: Submission = serde_json::from_value(rec["submission"].clone()).map_err(|e| format!("block {bh}: bad submission ({e})"))?;
    let c = Challenge { height: bh, vdf_input: mining_seed(my_tip, bh), blake4_target: target_from_bits(bits), vdf_t };
    if !check_submission(g, &c, &sub) { return Err(format!("block {bh}: dual-lane verify FAILED (work/VDF/header)")); }
    // LANE-R: recompute the reward exactly as the producer/follower do — time-based from the
    // block's stored µs ts when genesis is anchored, else the legacy block-based schedule.
    let (expected, new_carry) = if genesis_ts == 0 {
        (sigil_emission::block_reward(bh), 0u128)
    } else {
        sigil_emission::block_reward_time(genesis_ts, prev_ts, rec_ts, carry_in)
    };
    if reward != expected { return Err(format!("block {bh}: reward {reward} != schedule {expected}")); }
    let computed = tip_v2(my_tip, bh, sub.block.blake4_hash, sub.block.nonce, &serde_json::to_vec(&sub.block.vdf).unwrap());
    if computed != hx(rec["tip"].as_str().unwrap_or("")) { return Err(format!("block {bh}: tip-fold mismatch")); }
    Ok((computed, rec_ts, new_carry))
}

fn main() {
    let addr = std::env::args().nth(1).unwrap_or_else(|| "127.0.0.1:8193".into());
    let g = ModSquaring::bench_2048();
    let tip_info: Value = serde_json::from_str(&http(&addr, "/api/v1/tip")).expect("tip");
    let head_bh = tip_info["block_height"].as_u64().unwrap_or(0);
    let head_tip = hx(tip_info["tip"].as_str().unwrap_or(""));
    println!("producer tip: block_height={head_bh}");

    // LANE-R: the time-based emission anchor (0 on a legacy block-based chain). prev_ts is
    // threaded block→block so each reward is the integral over [prev_ts, this_block_ts].
    let genesis_ts: u128 = tip_info["genesis_ts_us"].as_str().unwrap_or("0").parse().unwrap_or(0);
    let mut tip = genesis();
    let mut prev_ts = genesis_ts;
    let mut carry = 0u128; // EXACT CARRY threaded from genesis — must mirror produce/apply
    let mut verified = 0u64;
    for bh in 0..head_bh {
        let rec: Value = serde_json::from_str(&http(&addr, &format!("/api/v1/block?height={bh}"))).unwrap_or(Value::Null);
        match verify_block(&rec, &tip, &g, genesis_ts, prev_ts, carry) {
            Ok((t, ts, c)) => { tip = t; prev_ts = ts; carry = c; verified += 1; }
            Err(e) => { println!("[FAIL] {e}"); std::process::exit(1); }
        }
    }
    let chain_ok = head_bh == 0 || tip == head_tip;
    println!("[{}] independently verified {verified}/{head_bh} blocks; final tip matches producer head: {chain_ok}",
        if chain_ok && verified == head_bh { "PASS" } else { "FAIL" });

    // tamper test: lie about block 0's reward → the verifier MUST reject.
    let mut tamper_ok = true;
    if head_bh > 0 {
        let mut rec0: Value = serde_json::from_str(&http(&addr, "/api/v1/block?height=0")).unwrap();
        rec0["reward"] = Value::String("999999999".into());
        tamper_ok = verify_block(&rec0, &genesis(), &g, genesis_ts, genesis_ts, 0).is_err();
        println!("[{}] tampered block (wrong reward) is REJECTED: {tamper_ok}", if tamper_ok { "PASS" } else { "FAIL" });
    }

    let pass = chain_ok && verified == head_bh && tamper_ok;
    println!("\n{}", if pass { "ALL CHECKS PASSED — chain independently verifiable; producer can't rewrite history undetected ✓" } else { "SOME CHECKS FAILED ✗" });
    std::process::exit(if pass { 0 } else { 1 });
}
