//! braid_sim — the sigil-dagknight VARFLOW sim gate binary
//! (`docs/SIGIL_DAGKNIGHT_LANE_v0.md` §4).
//!
//! Runs the six deterministic adversarial scenarios (S1–S6) against the
//! **deterministic braid linearization** core (NOT GHOSTDAG, NOT
//! DagKnight-the-paper — see the crate header), prints one line per scenario,
//! and exits non-zero if any scenario fails. Lane D (node wiring) may not
//! merge until this gate passes.

use sigil_dagknight::sim;

fn main() {
    let reports = [
        sim::run_dual_instance(0xDA61, 3, 10_000),
        sim::run_permutation_invariance(0xDA62, 16),
        sim::run_withheld_attacker(0xDA63, 32),
        sim::run_tamper_reject(0xDA64),
        sim::run_live_topology(0xDA65, 30, true),
        sim::run_window_bounds(0xDA66, 100_000),
        sim::run_ghostdag_dual_instance(0xDA67, 3, 10_000, 4),
        sim::run_restart_catchup(0xDA68, 10),
        sim::run_restart_catchup(0xDA69, 30),
        sim::run_restart_catchup(0xDA6A, 50),
        sim::run_restart_catchup(0xDA6B, 70),
        sim::run_restart_catchup(0xDA6C, 90),
    ];
    let mut all = true;
    for r in &reports {
        println!("{}", r.summary());
        all &= r.passed;
    }
    if all {
        println!("braid_sim gate: PASS ({}/{} scenarios)", reports.len(), reports.len());
    } else {
        println!("braid_sim gate: FAIL");
        std::process::exit(1);
    }
}
