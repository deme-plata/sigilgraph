//! mandatd — the MandatPilot credit daemon. Holds the SigilState in memory and exposes
//! the credit ledger over plain HTTP (std-only, sigil-rpcd style). The edge (flux-id-mcp)
//! calls it: top up after a Stripe payment, debit + cross-check on a CVR-Verify.
//!
//!   GET  /credits?sub=<mitid_sub>
//!   POST /topup    {"sub","usd_cents","event_id"}        (idempotent)
//!   POST /verify   {"sub","cvr","person_name","is_signatory","reg_cvr","reg_name","reg_active"}
//!
//! In-memory MVP (state resets on restart); persistence is the next step. The chain is
//! INVISIBLE to callers — they speak sub/credits/kr, never wallet/coin.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Mutex;

use std::collections::HashMap;

use flux_uint::Amount;
use sigil_mandat::{
    account_from_mitid, apply_payment, commit, credits_of, monitor_check, verify_business,
    watch_start, Change, CvrRecord, CvrSnapshot, MitidClaims, Payment, TREASURY,
};
use sigil_state::SigilState;

struct Ledger {
    s: SigilState,
    h: u64,
    seen: HashSet<String>,
    snaps: HashMap<String, CvrSnapshot>, // key = sub|cvr → last CVR snapshot (for monitoring)
}

fn main() {
    let port: u16 = std::env::var("MANDATD_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8791);
    let l = Mutex::new(Ledger { s: SigilState::new(), h: 0, seen: HashSet::new(), snaps: HashMap::new() });
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind");
    eprintln!("⚡ mandatd — MandatPilot credit ledger on SigilGraph :{port}");
    eprintln!("   GET /credits?sub=  ·  POST /topup  ·  POST /verify  (chain is invisible)");
    for stream in listener.incoming().flatten() {
        let mut tcp = stream;
        let mut buf = [0u8; 8192];
        let n = match tcp.read(&mut buf) { Ok(n) if n > 0 => n, _ => continue };
        let raw = String::from_utf8_lossy(&buf[..n]).to_string();
        let first = raw.lines().next().unwrap_or("");
        let mut it = first.split_whitespace();
        let method = it.next().unwrap_or("");
        let path = it.next().unwrap_or("");
        let body = raw.splitn(2, "\r\n\r\n").nth(1).unwrap_or("").to_string();
        let resp = route(&l, method, path, &body);
        let out = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\n\r\n{}",
            resp.len(), resp
        );
        let _ = tcp.write_all(out.as_bytes());
    }
}

fn j(v: &serde_json::Value, k: &str) -> String { v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string() }
fn jb(v: &serde_json::Value, k: &str) -> bool { v.get(k).and_then(|x| x.as_bool()).unwrap_or(false) }
fn ju(v: &serde_json::Value, k: &str) -> u64 { v.get(k).and_then(|x| x.as_u64()).unwrap_or(0) }
fn err(msg: &str) -> String { format!("{{\"ok\":false,\"error\":\"{msg}\"}}") }

fn route(l: &Mutex<Ledger>, method: &str, path: &str, body: &str) -> String {
    let (p, query) = match path.split_once('?') { Some((a, b)) => (a, b), None => (path, "") };
    match (method, p) {
        ("GET", "/credits") => {
            let sub = query.split('&').find_map(|kv| kv.strip_prefix("sub=")).unwrap_or("");
            let g = l.lock().unwrap();
            let c = credits_of(&g.s, &account_from_mitid(sub)).as_ore();
            format!("{{\"ok\":true,\"sub\":\"{sub}\",\"credits\":{c}}}")
        }
        ("POST", "/topup") => {
            let v: serde_json::Value = match serde_json::from_str(body) { Ok(v) => v, Err(_) => return err("bad json") };
            let sub = j(&v, "sub");
            let acct = account_from_mitid(&sub);
            let pay = Payment { id: j(&v, "event_id"), usd_cents: ju(&v, "usd_cents") };
            let mut guard = l.lock().unwrap();
            let g = &mut *guard; // deref once → disjoint field borrows of g.s / g.seen are allowed
            let h = g.h + 1;
            match apply_payment(&g.s, &acct, &pay, h, &mut g.seen) {
                None => { let c = credits_of(&g.s, &acct).as_ore(); format!("{{\"ok\":true,\"applied\":false,\"replay\":true,\"credits\":{c}}}") }
                Some(Ok(t)) => { g.h = h; commit(&mut g.s, &t, h).ok(); let c = credits_of(&g.s, &acct).as_ore(); format!("{{\"ok\":true,\"applied\":true,\"credits\":{c}}}") }
                Some(Err(e)) => err(&format!("{e}")),
            }
        }
        ("POST", "/verify") => {
            let v: serde_json::Value = match serde_json::from_str(body) { Ok(v) => v, Err(_) => return err("bad json") };
            let sub = j(&v, "sub");
            let acct = account_from_mitid(&sub);
            let claims = MitidClaims { cvr: j(&v, "cvr"), person_name: j(&v, "person_name"), is_signatory: jb(&v, "is_signatory") };
            let reg = CvrRecord {
                cvr: j(&v, "reg_cvr"),
                company_name: j(&v, "reg_name"),
                active: jb(&v, "reg_active"),
                bankrupt: jb(&v, "reg_bankrupt"),
                employees: ju(&v, "reg_employees") as u32,
                industry: j(&v, "reg_industry"),
            };
            let mut g = l.lock().unwrap();
            let h = g.h + 1;
            match verify_business(&g.s, &acct, &claims, &reg, h) {
                Ok((t, res)) => {
                    g.h = h; commit(&mut g.s, &t, h).ok();
                    let c = credits_of(&g.s, &acct).as_ore();
                    let rev = credits_of(&g.s, &TREASURY).as_ore();
                    format!(
                        "{{\"ok\":true,\"verified\":{},\"cvr\":\"{}\",\"company_name\":\"{}\",\"signatory\":{},\"company_active\":{},\"bankrupt\":{},\"employees\":{},\"industry\":\"{}\",\"credits_left\":{c},\"charged\":10,\"treasury\":{rev}}}",
                        res.verified, res.cvr, res.company_name, res.signatory, res.company_active,
                        res.bankrupt, res.employees, res.industry
                    )
                }
                Err(e) => err(&format!("{e}")),
            }
        }
        ("POST", "/watch") => {
            let v: serde_json::Value = match serde_json::from_str(body) { Ok(v) => v, Err(_) => return err("bad json") };
            let sub = j(&v, "sub");
            let acct = account_from_mitid(&sub);
            let snap = snap_from(&v);
            let key = format!("{sub}|{}", snap.cvr);
            let g = &mut *l.lock().unwrap();
            let h = g.h + 1;
            match watch_start(&g.s, &acct, h) {
                Ok(t) => {
                    g.h = h; commit(&mut g.s, &t, h).ok();
                    g.snaps.insert(key, snap);
                    let c = credits_of(&g.s, &acct).as_ore();
                    format!("{{\"ok\":true,\"watching\":true,\"credits_left\":{c}}}")
                }
                Err(e) => err(&format!("{e}")),
            }
        }
        ("POST", "/check") => {
            let v: serde_json::Value = match serde_json::from_str(body) { Ok(v) => v, Err(_) => return err("bad json") };
            let sub = j(&v, "sub");
            let acct = account_from_mitid(&sub);
            let now = snap_from(&v);
            let key = format!("{sub}|{}", now.cvr);
            let g = &mut *l.lock().unwrap();
            let prev = g.snaps.get(&key).cloned().unwrap_or_else(|| now.clone());
            let h = g.h + 1;
            match monitor_check(&g.s, &acct, &prev, &now, h) {
                Ok((t, changes)) => {
                    g.h = h; commit(&mut g.s, &t, h).ok();
                    g.snaps.insert(key, now);
                    let c = credits_of(&g.s, &acct).as_ore();
                    format!("{{\"ok\":true,\"changed\":{},\"changes\":{},\"credits_left\":{c},\"charged\":2}}",
                        !changes.is_empty(), changes_json(&changes))
                }
                Err(e) => err(&format!("{e}")),
            }
        }
        _ => err("not found"),
    }
}

fn snap_from(v: &serde_json::Value) -> CvrSnapshot {
    CvrSnapshot {
        cvr: j(v, "cvr"),
        company_name: j(v, "company_name"),
        active: v.get("active").and_then(|x| x.as_bool()).unwrap_or(true),
        bankrupt: jb(v, "bankrupt"),
        employees: ju(v, "employees") as u32,
    }
}
fn changes_json(cs: &[Change]) -> String {
    let items: Vec<String> = cs.iter().map(|c| match c {
        Change::NameChanged { from, to } => format!("{{\"type\":\"name\",\"from\":\"{from}\",\"to\":\"{to}\"}}"),
        Change::StatusChanged { now_active } => format!("{{\"type\":\"status\",\"now_active\":{now_active}}}"),
        Change::BankruptcyChanged { now_bankrupt } => format!("{{\"type\":\"konkurs\",\"now_bankrupt\":{now_bankrupt}}}"),
        Change::EmployeesChanged { from, to } => format!("{{\"type\":\"employees\",\"from\":{from},\"to\":{to}}}"),
    }).collect();
    format!("[{}]", items.join(","))
}
