//! flux-sentinel CLI — the defensive scanner as a one-shot command.
//!   flux-sentinel scan <path> [path...]   scan files, print verdicts, quarantine non-clean
//!   flux-sentinel version                 print the genesis-stamped build line
use std::env;
use std::fs;
use flux_sentinel::{Sentinel, Level};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        println!("{}", flux_sentinel::stamp().line());
        println!("usage: flux-sentinel scan <path> [path...]  |  flux-sentinel version");
        return;
    }
    match args[1].as_str() {
        "version" | "--version" | "-V" => println!("{}", flux_sentinel::stamp().line()),
        "scan" => {
            let mut s = Sentinel::new();
            let (mut clean, mut sus, mut mal) = (0u32, 0u32, 0u32);
            if args.len() < 3 { println!("nothing to scan. usage: flux-sentinel scan <path>"); return; }
            for path in &args[2..] {
                match fs::read(path) {
                    Ok(data) => {
                        let v = s.scan_and_quarantine(path, &data);
                        let tag = match v.level { Level::Malicious => "MALICIOUS", Level::Suspicious => "SUSPECT", Level::Clean => "clean" };
                        match v.level { Level::Malicious => mal += 1, Level::Suspicious => sus += 1, Level::Clean => clean += 1 }
                        let why = if v.reasons.is_empty() { String::new() } else { format!("  · {}", v.reasons.join(", ")) };
                        println!("[{:^9}] {:.2}  {}{}", tag, v.score, path, why);
                    }
                    Err(e) => println!("[  error  ] --  {}: {}", path, e),
                }
            }
            println!("── {} clean · {} suspect · {} malicious · {} quarantined", clean, sus, mal, s.quarantine_log().len());
            if mal > 0 { std::process::exit(1); }
        }
        _ => println!("unknown command. usage: flux-sentinel scan <path> | flux-sentinel version"),
    }
}
