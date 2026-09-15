//! sigil-earth — CLI. See lib.rs for the four jobs.

use anyhow::{anyhow, Result};
use std::path::PathBuf;

const OUT_DIR: &str = "/home/orobit/sigilgraph-org-site/eop";
const CACHE_DIR: &str = "/home/storage/sigil-earth/cache";
const STATE_DIR: &str = "/home/storage/sigil-earth/state";
const BIND: &str = "127.0.0.1:8460";

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}
fn opt(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn usage() -> ! {
    eprintln!(
        "sigil-earth {}\n\
         usage:\n  sigil-earth fetch   [--out DIR] [--cache DIR] [--state DIR] [--attest-key FILE] [--dry-run] [--inject-k X] [--inject-k-bio X] [--webhook] [--buzz]\n  \
         sigil-earth serve   [--bind HOST:PORT] [--data DIR]\n  \
         sigil-earth alert   --inject-k X | --inject-k-bio X [--out DIR] [--dry-run] [--webhook] [--buzz]\n  \
         sigil-earth verify  [--out DIR] | verify --remote https://sigilgraph.org [--viewing-key HEX]\n  \
         sigil-earth attest  --seed-file FILE [--out DIR] [--fluxc PATH] [--amount N] [--dry-run]\n  \
         sigil-earth buzz    --text \"...\" [--channel sigil]\n  \
         sigil-earth publish [--data DIR] [--nodes URL,URL] [--aether-store DIR] [--db DIR] [--search-index FILE] [--state FILE] [--dry-run] [--allow-unattested]",
        sigil_earth::VERSION
    );
    std::process::exit(2)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().map(|s| s.as_str()) else { usage() };
    let out_dir = PathBuf::from(opt(&args, "--out").or_else(|| opt(&args, "--data")).unwrap_or_else(|| OUT_DIR.into()));
    match cmd {
        "fetch" | "alert" => {
            let inject_k = opt(&args, "--inject-k").map(|s| s.parse::<f64>()).transpose().map_err(|e| anyhow!("--inject-k: {e}"))?;
            let inject_k_bio = opt(&args, "--inject-k-bio").map(|s| s.parse::<f64>()).transpose().map_err(|e| anyhow!("--inject-k-bio: {e}"))?;
            if cmd == "alert" && inject_k.is_none() && inject_k_bio.is_none() {
                return Err(anyhow!("alert needs --inject-k X and/or --inject-k-bio X"));
            }
            let o = sigil_earth::out::Opts {
                out_dir,
                cache_dir: PathBuf::from(opt(&args, "--cache").unwrap_or_else(|| CACHE_DIR.into())),
                state_dir: PathBuf::from(opt(&args, "--state").unwrap_or_else(|| STATE_DIR.into())),
                attest_key: PathBuf::from(opt(&args, "--attest-key").unwrap_or_else(|| sigil_earth::attest::DEFAULT_KEY.into())),
                dry_run: flag(&args, "--dry-run"),
                dispatch: sigil_earth::alert::Dispatch { webhook: flag(&args, "--webhook"), buzz: flag(&args, "--buzz") },
                inject_k,
                inject_k_bio,
            };
            let summary = sigil_earth::out::run(&o)?;
            println!("{}", serde_json::to_string(&summary)?);
            Ok(())
        }
        "serve" => {
            let bind = opt(&args, "--bind").unwrap_or_else(|| BIND.into());
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
            rt.block_on(sigil_earth::serve::run(&bind, out_dir))
        }
        "verify" => {
            if let Some(base) = opt(&args, "--remote") {
                let rep = sigil_earth::remote::verify_remote(&base, opt(&args, "--viewing-key"))?;
                println!("{}", serde_json::to_string_pretty(&rep)?);
                return if rep["ok"] == true { Ok(()) } else { Err(anyhow!("remote verification failed")) };
            }
            let v = sigil_earth::attest::verify(&sigil_earth::attest::chain_path(&out_dir), Some(&out_dir.join("latest.json")));
            println!("{}", serde_json::to_string_pretty(&v)?);
            if v.chain_intact && v.sigs_bad.is_empty() && v.live_match != Some(false) {
                Ok(())
            } else {
                Err(anyhow!("attestation chain does not verify"))
            }
        }
        "attest" => {
            let seed_file = opt(&args, "--seed-file").unwrap_or_else(|| "/root/.config/sigil/earth-anchor.seed".into());
            let fluxc = opt(&args, "--fluxc").unwrap_or_else(|| "/home/orobit/flux-sigil-attest/fluxc".into());
            let amount = opt(&args, "--amount").unwrap_or_else(|| "1000".into());
            let dry = flag(&args, "--dry-run");
            let chain = sigil_earth::attest::chain_path(&out_dir);
            let rows = sigil_earth::attest::read_chain(&chain);
            let last = rows.iter().rev().find(|r| r.kind == "attest").ok_or_else(|| anyhow!("no attest row to anchor"))?;
            if rows.iter().any(|r| r.kind == "anchor" && r.n == last.n && r.anchor["executed"] == true) {
                println!("{{\"ok\":true,\"already_anchored\":{}}}", last.n);
                return Ok(());
            }
            let memo = last.anchor["memo"].as_str().unwrap_or_default().to_string();
            let mut cmd = std::process::Command::new(&fluxc);
            cmd.args(["sigil-attest", "--seed-file", &seed_file, "--memo", &memo, "--amount", &amount]);
            if dry {
                cmd.arg("--dry-run");
            }
            let outp = cmd.output().map_err(|e| anyhow!("run {fluxc}: {e}"))?;
            let stdout = String::from_utf8_lossy(&outp.stdout).to_string();
            let resp: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|_| serde_json::json!({"raw": stdout.trim(), "stderr": String::from_utf8_lossy(&outp.stderr).trim()}));
            let txid = resp.pointer("/response/txid").or_else(|| resp.get("txid")).and_then(|v| v.as_str()).map(|s| s.to_string());
            let executed = !dry && txid.is_some();
            let row = sigil_earth::attest::AttestRow {
                kind: "anchor".into(),
                v: 1,
                n: last.n,
                date: last.date.clone(),
                ts: sigil_earth::time::iso_now(),
                subject: "eop/latest.json".into(),
                blake3: last.blake3.clone(),
                sha256: last.sha256.clone(),
                prev: None,
                msg: String::new(),
                pubkey: String::new(),
                sig: String::new(),
                anchor: serde_json::json!({"memo": memo, "executed": executed, "dry_run": dry, "tx_hash": txid, "amount": amount, "fee": 100000,
                                           "tool": "fluxc sigil-attest", "ts": sigil_earth::time::iso_now(),
                                           "wallet": resp.get("address").or_else(|| resp.pointer("/plan/from")).cloned(),
                                           "error": if executed || dry { serde_json::Value::Null } else { serde_json::json!(resp) }}),
            };
            if !dry {
                sigil_earth::attest::append(&chain, &row)?;
            }
            println!("{}", serde_json::to_string(&row)?);
            if executed || dry {
                Ok(())
            } else {
                Err(anyhow!("anchor not executed"))
            }
        }
        "publish" => {
            let o = sigil_earth::publish::Opts {
                data: out_dir,
                nodes: opt(&args, "--nodes").unwrap_or_else(|| sigil_earth::publish::DEFAULT_NODES.into())
                    .split(',').map(|s| s.trim().trim_end_matches('/').to_string()).filter(|s| !s.is_empty()).collect(),
                aether_store: PathBuf::from(opt(&args, "--aether-store").or_else(|| std::env::var("FLUX_AETHER_STORE").ok()).unwrap_or_else(|| sigil_earth::publish::DEFAULT_AETHER_STORE.into())),
                db: PathBuf::from(opt(&args, "--db").unwrap_or_else(|| sigil_earth::publish::DEFAULT_DB.into())),
                search_index: PathBuf::from(opt(&args, "--search-index").unwrap_or_else(|| sigil_earth::publish::DEFAULT_SEARCH_INDEX.into())),
                state: PathBuf::from(opt(&args, "--state").unwrap_or_else(|| format!("{STATE_DIR}/publish.json"))),
                dry_run: flag(&args, "--dry-run"),
                allow_unattested: flag(&args, "--allow-unattested"),
            };
            let rep = sigil_earth::publish::run(&o)?;
            println!("{}", serde_json::to_string_pretty(&rep)?);
            if rep["ok"] == true { Ok(()) } else { Err(anyhow!("publish: at least one node refused — see report")) }
        }
        "buzz" => {
            let text = opt(&args, "--text").ok_or_else(|| anyhow!("buzz needs --text"))?;
            let channel = opt(&args, "--channel").unwrap_or_else(|| "sigil".into());
            println!("{}", sigil_earth::buzz::post(&text, &channel)?);
            Ok(())
        }
        _ => usage(),
    }
}
