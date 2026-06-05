//! sigil-spawn CLI — forge a SIGIL PoW chain from a name (+ ticker) or a JSON
//! spec, print its identity/genesis/emission, and optionally scaffold it.
//!
//! Usage:
//!   sigil-spawn <Name> <TICKER> [--max-supply N] [--reward N] [--out DIR]
//!   sigil-spawn --json spec.json [--out DIR]
//!   sigil-spawn --template            # print a default ChainSpec JSON to edit

use sigil_spawn::{emission_schedule, ChainSpec, SpawnedChain};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "-h" || args[0] == "--help" {
        eprintln!(
            "sigil-spawn — forge a SIGIL PoW chain\n\n\
             sigil-spawn <Name> <TICKER> [--max-supply N] [--reward N] [--out DIR]\n\
             sigil-spawn --json <spec.json> [--out DIR]\n\
             sigil-spawn --template      # emit a default spec JSON to edit"
        );
        std::process::exit(if args.is_empty() { 2 } else { 0 });
    }

    if args[0] == "--template" {
        println!("{}", ChainSpec::new("MyChain", "MYC").to_json());
        return;
    }

    let flag = |name: &str| -> Option<String> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
    };

    // Build the spec: from JSON file, or from positional name + ticker.
    let spec = if args[0] == "--json" {
        let path = args.get(1).unwrap_or_else(|| {
            eprintln!("--json needs a path");
            std::process::exit(2);
        });
        let body = std::fs::read_to_string(path).unwrap_or_else(|e| {
            eprintln!("read {path}: {e}");
            std::process::exit(1);
        });
        ChainSpec::from_json(&body).unwrap_or_else(|e| {
            eprintln!("parse {path}: {e}");
            std::process::exit(1);
        })
    } else {
        let name = &args[0];
        let ticker = args.get(1).cloned().unwrap_or_else(|| {
            eprintln!("need a TICKER (2-8 uppercase) as the second arg");
            std::process::exit(2);
        });
        let mut s = ChainSpec::new(name, &ticker);
        if let Some(v) = flag("--max-supply").and_then(|v| v.parse().ok()) {
            s.max_supply = v;
        }
        if let Some(v) = flag("--reward").and_then(|v| v.parse().ok()) {
            s.initial_block_reward = v;
        }
        s
    };

    let chain = SpawnedChain::spawn(spec).unwrap_or_else(|e| {
        eprintln!("✗ spawn rejected: {e}");
        std::process::exit(1);
    });

    println!("{}\n", chain.summary());
    println!("emission preview (first 5 eras):");
    for era in emission_schedule(&chain.spec).iter().take(5) {
        println!(
            "  era {:>2}  h>={:<10}  reward={:<14}  cumulative={}",
            era.era, era.start_height, era.reward, era.cumulative
        );
    }

    if let Some(out) = flag("--out") {
        match chain.render(&out) {
            Ok(files) => {
                println!("\n📁 scaffolded {} file(s) under {}/{}:", files.len(), out, chain.identity.network_id);
                for f in files {
                    println!("   {}", f.display());
                }
            }
            Err(e) => {
                eprintln!("render failed: {e}");
                std::process::exit(1);
            }
        }
    }
}
