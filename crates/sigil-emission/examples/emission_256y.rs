//! Does SIGIL actually emit for 256 years, and does the answer depend on block rate?
//!
//! `NUM_ERAS = 64` and `SECONDS_PER_HALVING = 4 Julian years` say 256 years on paper. But
//! the live reward comes from `EmissionController::adaptive_reward`, which corrects for the
//! observed block rate — so the schedule on paper and the schedule in practice are two
//! different claims. This drives the REAL controller (not a reimplementation, which would
//! only test my reading of it) and reports what actually comes out.
//!
//! Run:
//!   fluxc run --release --package sigil-emission --example emission_256y
//!
//! Block-by-block over 256 years at the live rate would be ~21 billion iterations, so each
//! run uses a coarse block interval. That is not a fudge: `adaptive_reward` divides an
//! ANNUAL budget by the expected block count, so per-block reward scales inversely with rate
//! and annual emission is meant to be rate-independent. Running two very different intervals
//! and comparing totals is precisely the test of that claim.

use sigil_emission::controller::EmissionController;
use sigil_state::MAX_SUPPLY;

const SECS_PER_JULIAN_YEAR: u64 = 31_557_600; // 365.25 d
const DP: f64 = 1e8;

fn run(label: &str, block_secs: u64, years: u64, verbose: bool) -> (u128, u64) {
    let genesis = 0u64;
    let mut c = EmissionController::new(genesis);
    let mut supply: u128 = 0;
    let mut t = genesis;
    let end = genesis + years * SECS_PER_JULIAN_YEAR;
    let mut last_year_reported = 0u64;
    let mut last_nonzero_year = 0u64;
    let mut height = 0u64;

    while t < end {
        // FEED THE RATE WINDOW. `smoothed_rate()` returns a default 1.0 blk/s until
        // `rate_samples` has two live entries, and only `add_block` fills it — which the
        // live node does at sigil-node/src/main.rs:1607. A simulation that omits this
        // pins the rate at 1.0 and makes emission look proportional to block COUNT rather
        // than to wall-clock time. That is exactly the false alarm this line removes.
        height += 1;
        c.add_block(height, t, t);
        let r = c.calculate_block_reward(t, supply);
        if r > 0 {
            supply = supply.saturating_add(r);
            c.record_emission(r);
            last_nonzero_year = (t - genesis) / SECS_PER_JULIAN_YEAR;
        }
        t += block_secs;

        let year = (t - genesis) / SECS_PER_JULIAN_YEAR;
        if verbose && year != last_year_reported && (year <= 8 || year % 32 == 0) {
            println!(
                "    year {year:>3}  supply {:>14.2} SIGIL  ({:>5.2}% of cap)",
                supply as f64 / DP,
                supply as f64 / MAX_SUPPLY as f64 * 100.0
            );
            last_year_reported = year;
        } else if year != last_year_reported {
            last_year_reported = year;
        }
    }
    println!(
        "  {label:<26} -> {:>14.2} SIGIL  ({:>6.2}% of cap), last emission in year {last_nonzero_year}",
        supply as f64 / DP,
        supply as f64 / MAX_SUPPLY as f64 * 100.0
    );
    (supply, last_nonzero_year)
}

/// Run at the LIVE block rate until either the cap is reached or 256 years elapse, and say
/// which happened. Milliseconds per block, because 2.66 blk/s is 376 ms.
fn run_live(label: &str, block_ms: u64, max_years: u64) {
    let mut c = EmissionController::new(0);
    let mut supply: u128 = 0;
    let mut t_ms: u128 = 0;
    let end_ms = max_years as u128 * SECS_PER_JULIAN_YEAR as u128 * 1000;
    let mut iters: u64 = 0;
    let mut h = 0u64;
    while t_ms < end_ms && supply < MAX_SUPPLY {
        h += 1;
        c.add_block(h, (t_ms / 1000) as u64, (t_ms / 1000) as u64);
        let r = c.calculate_block_reward((t_ms / 1000) as u64, supply);
        if r > 0 {
            supply = supply.saturating_add(r);
            c.record_emission(r);
        }
        t_ms += block_ms as u128;
        iters += 1;
        if iters > 4_000_000_000 {
            println!("  {label:<26} -> aborted at 4e9 blocks");
            return;
        }
    }
    let years = t_ms as f64 / 1000.0 / SECS_PER_JULIAN_YEAR as f64;
    let hit_cap = supply >= MAX_SUPPLY;
    println!(
        "  {label:<26} -> {:>12.0} SIGIL ({:>6.2}% of cap) after {years:>7.2} years  [{}]",
        supply as f64 / DP,
        supply as f64 / MAX_SUPPLY as f64 * 100.0,
        if hit_cap { "CAP REACHED — mining ends here" } else { "reached 256y without exhausting" }
    );
}

fn main() {
    println!("MAX_SUPPLY = {:.0} SIGIL\n", MAX_SUPPLY as f64 / DP);

    println!("Schedule over 256 years (1 block/hour):");
    let (a, last_a) = run("1 block / hour", 3600, 256, true);

    println!("\nSame 256 years at other block intervals — annual emission is supposed to be");
    println!("rate-INDEPENDENT, so these totals should agree:");
    let (b, _) = run("1 block / 10 min", 600, 256, false);
    let (c_, _) = run("1 block / 6 min", 360, 256, false);

    println!("\nAt realistic block rates — the question that actually matters:");
    run_live("2.66 blk/s (live today)", 376, 256);
    run_live("1 blk/s", 1000, 256);
    run_live("10 blk/s", 100, 256);

    println!("\nVERDICT");
    let spread = {
        let hi = a.max(b).max(c_) as f64;
        let lo = a.min(b).min(c_) as f64;
        if lo > 0.0 { (hi - lo) / lo * 100.0 } else { f64::INFINITY }
    };
    println!("  rate-independence : totals differ by {spread:.4}%");
    println!("  mining lifetime   : last emission in year {last_a} of 256");
    println!(
        "  cap respected     : {} ({:.4}% of 21M)",
        if a <= MAX_SUPPLY { "yes" } else { "NO — OVERSHOOT" },
        a as f64 / MAX_SUPPLY as f64 * 100.0
    );
}
