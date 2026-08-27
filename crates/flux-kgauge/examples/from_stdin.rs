//! Read an `Observables` JSON document on stdin, print an honest gauge report.
//!
//! This is the seam that keeps the crate chain-agnostic: whatever adapter you
//! write — a shell script over `curl`, a node-internal task, a replay of stored
//! blocks — only has to emit this one struct. The gauge never learns what a
//! SIGIL block or a Quillon block looks like.
//!
//! ```sh
//! ./scripts/sigil-probe.sh | cargo run --example from_stdin -- sigil
//! ```
//!
//! The optional argument picks a chain preset (`sigil` or `quillon`), which
//! only sets the expected block rate. Add `enhanced` to drive the phase
//! decision off Eq. 25 instead of Eq. 10.

use std::io::Read;

use flux_kgauge::{GaugeConfig, KGauge, Observables, PhaseDriver};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut cfg = if args.iter().any(|a| a == "quillon") {
        GaugeConfig::quillon()
    } else {
        GaugeConfig::sigil()
    };
    if args.iter().any(|a| a == "enhanced") {
        cfg = cfg.with_phase_driver(PhaseDriver::Enhanced);
    }

    let mut raw = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut raw) {
        eprintln!("could not read stdin: {e}");
        std::process::exit(2);
    }

    let obs: Observables = match serde_json::from_str(&raw) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("stdin is not a valid Observables document: {e}");
            std::process::exit(2);
        }
    };

    let mut gauge = KGauge::new(cfg);
    let report = gauge.observe(&obs);

    print!("{}", report.render());
    println!();
    println!("--- prometheus ---");
    print!("{}", report.prometheus("sigil_kgauge"));

    // Exit non-zero when the reading is not safe to act on, so a cron wrapper
    // can tell "healthy" from "I could not tell".
    if !report.is_actionable() {
        std::process::exit(1);
    }
}
