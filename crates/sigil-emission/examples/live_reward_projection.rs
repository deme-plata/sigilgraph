//! Projects what the adaptive emission controller would actually pay RIGHT NOW,
//! using today's real live chain numbers (2026-08-16) — not synthetic test data.
//!
//! Two scenarios, side by side:
//!   A) NAIVE activation — the exact bug in `sigil-node`'s current `load_controller`:
//!      with no persisted `emission-controller.json`, it cold-starts
//!      `EmissionController::new(genesis_ts)` with `total_cumulative_emission = 0`,
//!      even though this chain has ALREADY minted real supply since the reset.
//!   B) SEEDED activation — the controller pre-loaded with the chain's ACTUAL
//!      current supply and era-progress before the switch is flipped.
//!
//! Run: `fluxc run --example live_reward_projection -p sigil-emission`

use sigil_emission::controller::EmissionController;
use sigil_state::MAX_SUPPLY;

// ── real inputs, captured live 2026-08-16 ~14:40 UTC ────────────────────────
/// The reset window was observed on Epsilon between the pre-reset snapshot
/// backup (Aug 14 16:37) and the fresh `aether/` dir (Aug 15 03:47) — no
/// persisted genesis timestamp exists to read exactly, so this anchors to
/// 2026-08-15 00:00 UTC as a reasonable within-hours estimate. Era/annual math
/// is insensitive to hour-level error (eras are 4 YEARS); only the elapsed-time
/// input to the PID setpoint moves by the same margin, which the PID's own
/// smoothing absorbs in the first correction cycle either way.
const GENESIS_TS: u64 = 1_786_752_000; // 2026-08-15 00:00:00 UTC
const NOW_TS: u64 = 1_786_891_200; // 2026-08-16 14:40:00 UTC (this session)
/// Live `/api/v1/supply` reading, `native_supply` field, raw base units.
const CURRENT_SUPPLY: u128 = 389_225_501_400_000;
/// Live `/api/v1/mining/miners` `net_hps`-implied production: height ~779K
/// blocks minted over ~38.7h elapsed ≈ 5.6 blk/s time-averaged. The adaptive
/// RATE GOVERNOR (SIGIL_RATE_MIN=8, SIGIL_RATE_MAX=60) means the true
/// instantaneous rate swings inside that band — three representative points.
const RATES_TO_TRY: [f64; 3] = [8.0, 25.0, 60.0];

fn seeded_controller() -> EmissionController {
    let mut c = EmissionController::new(GENESIS_TS);
    let elapsed = NOW_TS.saturating_sub(GENESIS_TS);
    c.current_era = c.era_at_time(elapsed);
    c.total_cumulative_emission = CURRENT_SUPPLY;
    // This early (era 0, ~38.7h into a 4-year era), ALL minted supply is
    // within era 0 — no era boundary has been crossed yet.
    c.total_emitted_this_era = CURRENT_SUPPLY;
    c
}

fn naive_controller() -> EmissionController {
    // Exactly what sigil-node's load_controller() does today on first
    // activation: EmissionController::new(genesis_ts_secs), full stop.
    EmissionController::new(GENESIS_TS)
}

fn on_pace_controller() -> EmissionController {
    // A THIRD option, not "seed with truth" but "start clean from here":
    // set the watermark to exactly what the time-ideal schedule expects at
    // this instant, so correction_factor starts at neutral (1.0) and the
    // annual-constant design governs everything from NOW forward, without
    // trying to claw back the historical over-mint from the flat schedule
    // that ran before the adaptive controller ever existed. The real
    // SigilState total_supply (used for the hard 21M cap check in
    // adaptive_reward) is untouched by this — the absolute safety ceiling
    // is unaffected either way; only the PID's soft annual-pacing baseline
    // is what "starts fresh" here.
    let mut c = EmissionController::new(GENESIS_TS);
    let elapsed = NOW_TS.saturating_sub(GENESIS_TS);
    c.current_era = c.era_at_time(elapsed);
    let on_pace = c.target_cumulative_at_time(elapsed);
    c.total_cumulative_emission = on_pace;
    c.total_emitted_this_era = on_pace;
    c
}

fn fmt_sigil(raw: u128) -> String {
    // 8-decimal SIGIL, matching the wallet UI's display convention.
    format!("{}.{:08}", raw / 100_000_000, raw % 100_000_000)
}

fn main() {
    let elapsed = NOW_TS.saturating_sub(GENESIS_TS);
    println!("== live inputs ==");
    println!("genesis_ts        : {GENESIS_TS} (2026-08-15 00:00 UTC, estimated)");
    println!("now_ts             : {NOW_TS} (this session)");
    println!("elapsed             : {elapsed}s ({:.2}h)", elapsed as f64 / 3600.0);
    println!("current_supply     : {CURRENT_SUPPLY} raw = {} SIGIL", fmt_sigil(CURRENT_SUPPLY));
    println!(
        "max_supply          : {} raw = {} SIGIL ({:.4}% minted)",
        MAX_SUPPLY,
        fmt_sigil(MAX_SUPPLY),
        CURRENT_SUPPLY as f64 / MAX_SUPPLY as f64 * 100.0
    );

    println!();
    println!("== the CURRENT, live, pure height-halving schedule (what's actually running) ==");
    for h in [700_000u64, 750_000, 779_000] {
        println!("  block_reward(h={h}) = {} raw = {} SIGIL", sigil_emission::block_reward(h), fmt_sigil(sigil_emission::block_reward(h)));
    }
    println!("  -> FLAT regardless of block rate. At today's ~5.6 blk/s average this schedule");
    println!("     is already implicitly emitting far faster per YEAR than a 256-year/64-era");
    println!("     cap was designed for if the rate ever climbs toward the 60 blk/s ceiling.");

    for (label, ctrl) in [("A) NAIVE cold-start (today's actual load_controller bug)", naive_controller()),
                           ("B) SEEDED with the REAL current supply (claws back historical over-mint)", seeded_controller()),
                           ("C) ON-PACE reset (starts the annual-constant design fresh from NOW)", on_pace_controller())] {
        println!();
        println!("== {label} ==");
        println!("  starting total_cumulative_emission: {} raw", ctrl.total_cumulative_emission);
        let target_now = ctrl.target_cumulative_at_time(elapsed);
        println!("  time-ideal target at elapsed={elapsed}s: {target_now} raw = {} SIGIL", fmt_sigil(target_now));
        let error = if target_now > 0 {
            (ctrl.total_cumulative_emission as f64 - target_now as f64) / target_now as f64 * 100.0
        } else { 0.0 };
        println!("  => drift from target: {error:+.2}%  (negative = under-minted => PID GROWS reward; positive = over-minted => PID SHRINKS it)");
        // adaptive_reward() is the pure function — takes rate explicitly, no
        // stateful bookkeeping — the right tool for "what WOULD it pay at rate
        // X", independent of any other trial in this loop.
        for rate in RATES_TO_TRY {
            let reward = ctrl.adaptive_reward(elapsed, rate, CURRENT_SUPPLY);
            let correction = ctrl.correction_factor_at(elapsed);
            println!(
                "  at rate {rate:>5.1} blk/s -> reward = {reward} raw = {} SIGIL  (correction_factor={correction:.4}, implied annual = {} SIGIL/yr)",
                fmt_sigil(reward),
                fmt_sigil(reward.saturating_mul((rate * 31_557_600.0) as u128))
            );
        }
    }

    println!();
    println!("== verdict ==");
    println!("If scenario A's reward is dramatically higher than B's, flipping the switch WITHOUT");
    println!("seeding the watermark would mint noticeably more than intended in the first stretch");
    println!("after activation, because the controller thinks it's starting from zero and tries to");
    println!("'catch up' to a target that doesn't know ~{:.1}% of the cap is already minted.", CURRENT_SUPPLY as f64 / MAX_SUPPLY as f64 * 100.0);
}
