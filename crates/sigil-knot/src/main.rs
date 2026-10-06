//! sigil-knot — read a SIGIL node's recent DAG as a braid and say what its knot means.
//!
//!   curl -s https://sigilgraph.org/v1/dagknight/recent | sigil-knot [--window N|--all] [--expect N] [--json]
//!   sigil-knot --demo        # the four shapes on synthetic DAGs
//!
//! Analysis only: it reads JSON on stdin and writes a report. It never talks to a node.

use std::io::Read;

fn usage() -> ! {
    eprintln!(
        "usage: curl -s https://sigilgraph.org/v1/dagknight/recent | sigil-knot [--window N | --all] [--expect N] [--json]\n       sigil-knot --demo [--json]"
    );
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut window = Some(sigil_knot::COMMITMENT_WINDOW);
    let mut expect = None;
    let (mut json, mut demo) = (false, false);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => json = true,
            "--demo" => demo = true,
            "--all" => window = None,
            "--window" => {
                i += 1;
                window = Some(args.get(i).and_then(|s| s.parse().ok()).filter(|n: &u64| *n > 0).unwrap_or_else(|| usage()));
            }
            "--expect" => {
                i += 1;
                expect = Some(args.get(i).and_then(|s| s.parse().ok()).unwrap_or_else(|| usage()));
            }
            "-h" | "--help" => usage(),
            _ => usage(),
        }
        i += 1;
    }
    // An i128 overflow inside the Alexander computation is reported, not printed as a panic.
    std::panic::set_hook(Box::new(|_| {}));

    if demo {
        for (name, b, from, to) in sigil_knot::demo_scenarios() {
            let r = sigil_knot::analyze_braid(&b, from, to);
            if json {
                println!("{}", serde_json::json!({ "scenario": name, "report": r }));
            } else {
                println!("── {name} ──\n{}", sigil_knot::render(&r, None));
            }
        }
        return;
    }

    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() || input.trim().is_empty() {
        usage();
    }
    let parsed: serde_json::Value = serde_json::from_str(&input).unwrap_or_else(|e| {
        eprintln!("sigil-knot: stdin is not JSON: {e}");
        std::process::exit(1);
    });
    let views = sigil_knot::views_from_snapshot(&parsed).unwrap_or_else(|e| {
        eprintln!("sigil-knot: {e}");
        std::process::exit(1);
    });
    let (b, lo, hi) = sigil_knot::braid_from_views(&views).unwrap_or_else(|e| {
        eprintln!("sigil-knot: {e}");
        std::process::exit(1);
    });
    let from = match window {
        Some(w) => hi.saturating_sub(w - 1).max(lo),
        None => lo,
    };
    let r = sigil_knot::analyze_braid(&b, from, hi);
    if json {
        println!("{}", serde_json::to_string_pretty(&r).unwrap());
    } else {
        print!("{}", sigil_knot::render(&r, expect));
    }
}
