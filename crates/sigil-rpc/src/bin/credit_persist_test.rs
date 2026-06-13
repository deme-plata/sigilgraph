//! LANE-X ACCEPTANCE GATE — credit-position restart persistence.
//!
//! The lane's hard requirement: "et lån må ALDRIG forsvinde ved restart".
//! Today the rpcd's in-memory state reset the mining chain on restart; credit
//! positions must NOT share that fate. This test drives the REAL sigil-rpcd
//! binary (not a mock, not the lib):
//!
//!   1. boot rpcd on a fresh temp flux-db
//!   2. POST /credit/lock (OPERATOR locks 100_000 SIGIL, bronze tier)
//!   3. verify position + balances via HTTP
//!   4. kill -9 the daemon (no graceful shutdown — the brutal case)
//!   5. reboot it on the same state dir
//!   6. assert the position, CREDIT mint, vault balance, and wallet balance
//!      all survived byte-for-byte
//!
//! Run: `credit_persist_test` (expects sigil-rpcd next to it in target dir).
//! Exit 0 = gate passed; nonzero + loud stderr = gate FAILED.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const ADDR: &str = "127.0.0.1:18099";
const OPERATOR_HEX: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
const LOCK_AMOUNT: u128 = 100_000;
const EXPECT_CREDIT: u128 = 50_000; // 50% LTV

fn http(method: &str, path: &str, body: &str) -> Option<String> {
    let mut s = TcpStream::connect(ADDR).ok()?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {ADDR}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).ok()?;
    let mut out = String::new();
    s.read_to_string(&mut out).ok()?;
    out.split("\r\n\r\n").nth(1).map(|b| b.to_string())
}

fn wait_health(deadline_s: u64) -> bool {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(deadline_s) {
        if let Some(b) = http("GET", "/health", "") {
            if b.contains("\"ok\":true") {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    false
}

fn spawn_rpcd(bin: &std::path::Path, state: &str, hist: &str) -> Child {
    Command::new(bin)
        .env("SIGIL_RPC_ADDR", ADDR)
        .env("SIGIL_RPC_NO_AUTH", "1") // local gate test — auth has its own tests
        .env("SIGIL_STATE_PATH", state)
        .env("SIGIL_HISTORY_PATH", hist)
        .env("SIGIL_DAG_BLOCKS", "/nonexistent") // skip the backfill
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sigil-rpcd")
}

fn field<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let p = format!("\"{key}\":");
    let i = json.find(&p)? + p.len();
    let rest = json[i..].trim_start();
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        Some(&stripped[..end])
    } else {
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        Some(&rest[..end])
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("❌ LANE-X ACCEPTANCE GATE FAILED: {msg}");
    std::process::exit(1)
}

fn main() {
    // sigil-rpcd sits next to this test bin in the target dir.
    let bin = std::env::current_exe()
        .expect("current_exe")
        .parent()
        .expect("target dir")
        .join("sigil-rpcd");
    if !bin.exists() {
        fail(&format!("sigil-rpcd binary not found at {} — build it first", bin.display()));
    }
    let tmp = std::env::temp_dir().join(format!("sigil-credit-gate-{}", std::process::id()));
    let state = tmp.join("state");
    let hist = tmp.join("history");
    std::fs::create_dir_all(&state).expect("mk state dir");
    std::fs::create_dir_all(&hist).expect("mk hist dir");
    let state = state.to_string_lossy().to_string();
    let hist = hist.to_string_lossy().to_string();

    // ── boot #1: fresh genesis ──
    let mut child = spawn_rpcd(&bin, &state, &hist);
    if !wait_health(30) {
        let _ = child.kill();
        fail("daemon did not come healthy (boot #1)");
    }
    println!("✓ boot #1 healthy");

    // sanity: locking from an unfunded wallet must fail cleanly
    // (NOT 0x11×32 — that's the genesis-funded demo CITIZEN; 0x33×32 holds nothing)
    let r = http("POST", "/credit/lock",
        &format!("{{\"wallet\":\"{}\",\"amount\":10,\"tier\":\"bronze\"}}", "33".repeat(32)))
        .unwrap_or_default();
    if !r.contains("insufficient") {
        let _ = child.kill();
        fail(&format!("unfunded lock should fail with 'insufficient', got: {r}"));
    }
    println!("✓ unfunded lock rejected");

    // ── lock: OPERATOR locks 100k SIGIL bronze → 50k CREDIT ──
    let r = http("POST", "/credit/lock",
        &format!("{{\"wallet\":\"{OPERATOR_HEX}\",\"amount\":{LOCK_AMOUNT},\"tier\":\"bronze\"}}"))
        .unwrap_or_default();
    if !r.contains("\"ok\":true") {
        let _ = child.kill();
        fail(&format!("lock failed: {r}"));
    }
    if field(&r, "credit_minted") != Some(&EXPECT_CREDIT.to_string()) {
        let _ = child.kill();
        fail(&format!("expected credit_minted={EXPECT_CREDIT} (50% LTV), got: {r}"));
    }
    println!("✓ locked {LOCK_AMOUNT} SIGIL → {EXPECT_CREDIT} CREDIT");

    // unlock before expiry must refuse (bronze = 7 days)
    let r = http("POST", "/credit/unlock",
        &format!("{{\"wallet\":\"{OPERATOR_HEX}\",\"position_index\":0}}")).unwrap_or_default();
    if !r.contains("locked until") {
        let _ = child.kill();
        fail(&format!("early unlock should refuse with 'locked until', got: {r}"));
    }
    println!("✓ early unlock refused");

    // snapshot the pre-kill observable state
    let pos1 = http("GET", &format!("/credit/position?wallet={OPERATOR_HEX}"), "").unwrap_or_default();
    let stat1 = http("GET", "/credit/status", "").unwrap_or_default();
    let bal1 = http("GET", &format!("/balance?wallet={OPERATOR_HEX}"), "").unwrap_or_default();
    if field(&pos1, "collateral_locked") != Some(&LOCK_AMOUNT.to_string()) {
        let _ = child.kill();
        fail(&format!("position not visible before kill: {pos1}"));
    }

    // ── kill -9: the brutal restart ──
    child.kill().expect("SIGKILL rpcd");
    let _ = child.wait();
    println!("✓ daemon killed (SIGKILL)");
    std::thread::sleep(Duration::from_millis(500));

    // ── boot #2: same state dir — the loan must still be there ──
    let mut child = spawn_rpcd(&bin, &state, &hist);
    if !wait_health(30) {
        let _ = child.kill();
        fail("daemon did not come healthy (boot #2)");
    }
    println!("✓ boot #2 healthy (same flux-db)");

    let pos2 = http("GET", &format!("/credit/position?wallet={OPERATOR_HEX}"), "").unwrap_or_default();
    let stat2 = http("GET", "/credit/status", "").unwrap_or_default();
    let bal2 = http("GET", &format!("/balance?wallet={OPERATOR_HEX}"), "").unwrap_or_default();
    let _ = child.kill();
    let _ = child.wait();

    // position survived, field by field
    for key in ["collateral_locked", "credit_minted", "tier", "lock_timestamp", "unlock_timestamp", "credit_balance"] {
        let (a, b) = (field(&pos1, key), field(&pos2, key));
        if a.is_none() || a != b {
            fail(&format!("position field '{key}' did not survive restart: before={a:?} after={b:?}\npre: {pos1}\npost: {pos2}"));
        }
    }
    println!("✓ position survived restart byte-for-byte");

    // vault totals survived
    for key in ["total_collateral", "total_credit_supply", "vault_native_balance", "position_count"] {
        let (a, b) = (field(&stat1, key), field(&stat2, key));
        if a.is_none() || a != b {
            fail(&format!("vault field '{key}' did not survive restart: before={a:?} after={b:?}"));
        }
    }
    println!("✓ vault totals survived restart");

    // wallet NATIVE balance survived (collateral stayed deducted — no re-genesis)
    if field(&bal1, "balance").is_none() || field(&bal1, "balance") != field(&bal2, "balance") {
        fail(&format!("wallet balance changed across restart: before={bal1} after={bal2}"));
    }
    println!("✓ wallet balance survived restart");

    let _ = std::fs::remove_dir_all(&tmp);
    println!("\n✅ LANE-X ACCEPTANCE GATE PASSED — a loan does NOT vanish on restart (kill -9 included)");
}
