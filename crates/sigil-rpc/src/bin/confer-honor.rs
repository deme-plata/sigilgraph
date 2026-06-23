//! confer-honor — operator CLI to confer a SIGIL Order (Ridderkorset / Elefantordenen) on a
//! citizen, signing the request so sigil-rpcd's `/api/v1/honor/confer` authorizes it (honor
//! over power: conferral is signed, never anonymous). The honour lands immutably on-chain and
//! the SIGIL honor Discord bot then shows the badge in "vis medlemmer".
//!
//! Usage:
//!   confer-honor --to <hex64> --order Ridderkorset --rank Storkors \
//!                --citation "shipped the v3 sync sprint" [--url 127.0.0.1:8099]
//!   confer-honor --to <hex64> --order Elefantordenen --citation "saved the chain"
//!
//! Signing key: --seed <hex32> or env SIGIL_CONFERRER_SEED (the conferrer's ed25519 seed).
//! The conferrer's pubkey is recorded as `conferred_by`. (Daemons run with SIGIL_RPC_NO_AUTH
//! skip the signature check — but the CLI always signs so it works in both modes.)

use sigil_oauth::Keypair;
use std::io::{Read, Write};
use std::time::Duration;

fn arg(flag: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == flag).and_then(|i| a.get(i + 1)).cloned()
}

fn hex32(s: &str) -> Option<[u8; 32]> {
    let b = hex::decode(s.trim()).ok()?;
    <[u8; 32]>::try_from(b.as_slice()).ok()
}

fn http_post(host_port: &str, path: &str, body: &str) -> std::io::Result<String> {
    let mut s = std::net::TcpStream::connect(host_port)?;
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes())?;
    let mut o = String::new();
    s.read_to_string(&mut o)?;
    Ok(o.split("\r\n\r\n").nth(1).unwrap_or(&o).to_string())
}

fn main() {
    let usage = "usage: confer-honor --to <hex64> --order <Ridderkorset|Elefantordenen> \
                 [--rank <Ridder|Kommandør|Storkors>] --citation <text> [--url host:port] [--seed <hex32>]";

    let to = match arg("--to").and_then(|s| hex32(&s)) {
        Some(t) => t,
        None => { eprintln!("error: --to <hex64 recipient wallet>\n{usage}"); std::process::exit(2); }
    };
    let order = arg("--order").unwrap_or_default();
    if order != "Ridderkorset" && order != "Elefantordenen" {
        eprintln!("error: --order must be Ridderkorset or Elefantordenen\n{usage}");
        std::process::exit(2);
    }
    let rank = arg("--rank").unwrap_or_default();
    let rank_ok = match order.as_str() {
        "Ridderkorset" => matches!(rank.as_str(), "Ridder" | "Kommandør" | "Storkors"),
        "Elefantordenen" => rank.is_empty(),
        _ => false,
    };
    if !rank_ok {
        eprintln!("error: rank invalid for {order} (Ridderkorset: Ridder|Kommandør|Storkors; Elefantordenen: none)");
        std::process::exit(2);
    }
    let citation = arg("--citation").unwrap_or_else(|| "conferred by deed".into());
    let url = arg("--url").unwrap_or_else(|| "127.0.0.1:8099".into());

    let seed_hex = arg("--seed").or_else(|| std::env::var("SIGIL_CONFERRER_SEED").ok());
    let seed = match seed_hex.as_deref().and_then(hex32) {
        Some(s) => s,
        None => { eprintln!("error: signing key required — --seed <hex32> or SIGIL_CONFERRER_SEED"); std::process::exit(2); }
    };
    let kp = Keypair::from_seed(&seed);
    let conferrer_hex = kp.pubkey_hex();
    let recipient_hex = hex::encode(to);

    // Sign the canonical auth message — fields MUST match the server's order: [order, rank, recipient_hex].
    let req_nonce: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let fields = [order.as_str(), rank.as_str(), recipient_hex.as_str()];
    let msg = sigil_rpc::auth::auth_message("honor_confer", &fields, req_nonce);
    let sig_hex = hex::encode(kp.sign(&msg));

    let body = format!(
        r#"{{"recipient":"{recipient_hex}","conferrer":"{conferrer_hex}","order":"{order}","rank":"{rank}","citation":{citation},"sig":"{sig_hex}","req_nonce":{req_nonce}}}"#,
        citation = serde_json::to_string(&citation).unwrap_or_else(|_| "\"\"".into()),
    );

    match http_post(&url, "/api/v1/honor/confer", &body) {
        Ok(resp) => {
            println!("⚔️  conferred {order} {rank} → {}", &recipient_hex[..16]);
            println!("    by {} · {}", &conferrer_hex[..16], citation);
            println!("    {}", resp.trim());
        }
        Err(e) => { eprintln!("POST {url} failed: {e}"); std::process::exit(1); }
    }
}
