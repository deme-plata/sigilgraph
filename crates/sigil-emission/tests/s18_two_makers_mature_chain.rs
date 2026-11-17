//! S18 (lane sigil-r3-rehearsal-1114, 2026-11-14) — NOT for the main line, a measurement.
//!
//! Question: with SIGIL_EMISSION_CHAINWIDE=1, does a SECOND block-maker on a MATURE chain (live g3: ~25.75 days
//! old, supply on schedule) keep issuance at the time target?
//!
//! The node (sigil-node/src/lib.rs, 10.7.8 = ac1806fc) does, per block it mints itself:
//!   reward = c.chainwide_reward(now_secs, frontier.cap_supply(), c.smoothed_rate())   // lib.rs:2650
//!   ... the block is made, enters the braid ...
//!   c.add_block(h, now_secs, now_secs)                                                 // lib.rs:2902
//! `c` is THIS maker's own controller, and add_block is reached only from the maker's own mint path, so
//! `smoothed_rate()` is the maker's OWN MINT rate, not the chain's. (The unit test
//! `chainwide_reward_is_independent_of_maker_count` passes the chain's total rate, 8.0, to both makers.)
//!
//! Feed::OwnMint      = what 10.7.8 does.
//! Feed::ChainSettled = the proposed fix: every node's controller records every block that PAYS on the settled
//!                      chain (all makers' blocks, once each) and nothing else.
//!
//! Model: maker i mints every 1/r_i seconds; a fraction pay_i of ITS blocks lands on the settled spine and pays
//! (side blocks never pay on g3). Both makers read the same committed cap supply. Deterministic.

use sigil_emission::controller::EmissionController;

const GEN: u64 = 1_792_418_887; // live g3 SIGIL_GENESIS_TS
const ELAPSED0: u64 = 2_225_000; // ~25.75 days: live g3 age on 2026-11-14 09:3x CET
const WINDOW: u64 = 1_800; // RATE_WINDOW_SECS

#[derive(Clone, Copy, PartialEq)]
enum Feed {
    OwnMint,
    ChainSettled,
}

struct Maker {
    c: EmissionController,
    rate: f64,
    pay: f64,
    acc: f64,
    next_t: f64,
    h: u64,
    fed: Option<u64>,
}

fn prewarm(c: &mut EmissionController, rate: f64, start: f64) {
    let n = (WINDOW as f64 * rate) as u64;
    for k in 0..n {
        let t = (start - WINDOW as f64 + k as f64 / rate) as u64;
        c.add_block(k, t, t);
    }
}

/// `makers`: (mint rate, share of its own blocks that pay, warm rate window?). Returns (issuance ÷ target, cum error).
fn run(spec: &[(f64, f64, bool)], feed: Feed, secs: f64, chain_rate_before: f64) -> (f64, f64) {
    let t0 = (GEN + ELAPSED0) as f64;
    let mut makers: Vec<Maker> = spec
        .iter()
        .map(|&(rate, pay, warm)| {
            let mut c = EmissionController::new(GEN);
            if warm {
                // a maker that has been running: its window holds what its feed policy recorded before t0
                prewarm(&mut c, if feed == Feed::OwnMint { rate } else { chain_rate_before }, t0);
            }
            Maker { c, rate, pay, acc: 0.0, next_t: t0, h: 0, fed: None }
        })
        .collect();
    let ctl = EmissionController::new(GEN);
    let s0 = ctl.target_cumulative_at_time(ELAPSED0) * 9_998 / 10_000; // live: -0.02 % vs schedule
    let mut s = s0;
    let mut settled: u64 = 0; // settled height = paying blocks so far
    let t_end = t0 + secs;
    loop {
        let (i, t) = makers
            .iter()
            .enumerate()
            .map(|(i, m)| (i, m.next_t))
            .fold((usize::MAX, f64::INFINITY), |a, b| if b.1 < a.1 { b } else { a });
        if t >= t_end {
            break;
        }
        let now = t as u64;
        if feed == Feed::ChainSettled {
            // exactly what the fixed lib.rs does at each mint tick, through the same method
            let m = &mut makers[i];
            m.c.feed_settled_height(&mut m.fed, settled, now, now);
        }
        let r = makers[i].c.chainwide_reward(now, s, makers[i].c.smoothed_rate());
        let m = &mut makers[i];
        m.acc += m.pay;
        let pays = if m.acc >= 1.0 { m.acc -= 1.0; true } else { false };
        if pays {
            s = s.saturating_add(r);
            settled += 1;
        }
        m.h += 1;
        m.next_t += 1.0 / m.rate;
        match feed {
            Feed::OwnMint => {
                let h = makers[i].h;
                makers[i].c.add_block(h, now, now)
            }
            Feed::ChainSettled => {}
        }
    }
    let tgt0 = ctl.target_cumulative_at_time(ELAPSED0);
    let tgt1 = ctl.target_cumulative_at_time(ELAPSED0 + secs as u64);
    ((s - s0) as f64 / (tgt1 - tgt0) as f64, (s as f64 - tgt1 as f64) / tgt1 as f64)
}

fn row(name: &str, spec: &[(f64, f64, bool)], feed: Feed) -> Vec<f64> {
    // the chain's paying-block rate before t0 = today's g3: one maker at 8/s
    let mut line = format!("{name:<58}");
    let mut out = vec![];
    for secs in [600.0, 3_600.0, 86_400.0, 7.0 * 86_400.0] {
        let (ratio, err) = run(spec, feed, secs, 8.0);
        out.push(ratio);
        line += &format!(" {ratio:>6.3}x");
        if secs == 7.0 * 86_400.0 {
            line += &format!("  {:+6.2} %", err * 100.0);
        }
    }
    println!("{line}");
    out
}

const SC: &[(&str, &[(f64, f64, bool)])] = &[
    ("one maker, 8/s (today's g3)", &[(8.0, 1.0, true)]),
    ("50/50 interleaved: A 8/s + B 8/s, every block pays", &[(8.0, 1.0, true), (8.0, 1.0, false)]),
    ("50/50 competing:   A 8/s + B 8/s, half of each pays", &[(8.0, 1.0, true), (8.0, 1.0, false)]),
    ("70/30 interleaved: A 8/s + B 3.43/s, every block pays", &[(8.0, 1.0, true), (3.43, 1.0, false)]),
    ("70/30 competing:   A 8/s + B 8/s, A 70 % / B 30 % pay", &[(8.0, 0.7, true), (8.0, 0.3, false)]),
];

fn table(feed: Feed) -> Vec<Vec<f64>> {
    let mut v = vec![];
    for (i, (name, spec)) in SC.iter().enumerate() {
        // row 2 is the competing 50/50 case: each maker's pay share is 0.5
        let spec: Vec<(f64, f64, bool)> =
            if i == 2 { spec.iter().map(|&(r, _, w)| (r, 0.5, w)).collect() } else { spec.to_vec() };
        v.push(row(name, &spec, feed));
    }
    v
}

#[test]
fn s18_issuance_with_a_second_maker_on_a_mature_chain() {
    println!("\nissuance ÷ time target over                                10 min     1 h    24 h     7 d   supply vs schedule after 7 d");
    println!("— 10.7.8 as it is (each controller fed by its OWN mints, lib.rs:2902) —");
    let now = table(Feed::OwnMint);
    println!("— proposed fix (every controller fed by every block that PAYS on the settled chain) —");
    let fixed = table(Feed::ChainSettled);
    // the single-maker control must be on schedule under both feeds, or the model is wrong
    for r in now[0].iter().chain(fixed[0].iter()) {
        assert!((r - 1.0).abs() < 0.02, "single-maker control off schedule: {r}");
    }
    // the fix keeps every two-maker case on schedule over a day and a week (the property 10.7.8 lacks). The first
    // hour overshoots while the 30-min rate window (RATE_WINDOW_SECS) learns the new chain rate: measured 1.36x over
    // 10 min and 1.10x over 1 h when a second interleaving maker joins — about 29 SIGIL once per change in makers.
    for (i, rs) in fixed.iter().enumerate() {
        for r in &rs[2..] {
            assert!((r - 1.0).abs() < 0.01, "fixed feed, scenario {i}: {r}");
        }
    }
    // and 10.7.8 does NOT hold it for interleaved makers (rows 1 and 3): the bug this lane found
    assert!(now[1][2] > 1.5 && now[3][2] > 1.5, "10.7.8 feed was expected to over-issue: {:?} {:?}", now[1], now[3]);
}

/// The regression guard for the 10.7.8 behaviour: two makers that interleave each pay ~2x. This test PASSES while
/// the bug exists (it documents it); flip the assert when lib.rs feeds the chain's rate.
#[test]
fn s18_own_mint_feed_doubles_issuance_with_two_interleaved_makers() {
    let (ratio, _) = run(&[(8.0, 1.0, true), (8.0, 1.0, false)], Feed::OwnMint, 3_600.0, 8.0);
    assert!(ratio > 1.9, "expected the 10.7.8 feed to double issuance with two interleaved makers, got {ratio}");
}
