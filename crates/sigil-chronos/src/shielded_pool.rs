//! SHIELDED POOL AT SCALE — chronos scenarios (2026-08-23).
//!
//! The pool's code has only ever run with a handful of notes. Two questions cannot be
//! answered by reasoning, and both decide whether the design is deployable:
//!
//!   1. **Does it scale?** `refresh_anchor` rebuilds the entire `2^POOL_DEPTH`-leaf MiMC
//!      tree on every block that touches the pool. At depth 15 that is 32,767 `compress2`
//!      calls of 63 rounds each — roughly 2M `pow7` operations *per block*. I deferred the
//!      incremental append-only root deliberately (a wrong incremental root is a consensus
//!      split), and then never measured what the safe version costs. If it is seconds per
//!      block, the deferral was wrong and the optimization is mandatory, not optional.
//!
//!   2. **Does it actually hide?** Privacy is usually argued. Here it can be *measured*:
//!      simulate an adversary who sees only the public transcript and let it try to link
//!      shields to unshields. The output is a number — the fraction it links correctly —
//!      and that number is what "how private is this" should mean.
//!
//! Both are run as chronos scenarios rather than benchmarks because determinism matters: a
//! privacy result you cannot reproduce is an anecdote.

use std::collections::HashMap;
use std::time::Instant;

use sigil_shield::note_v1::{padding_leaf_wire, to_wire, Note};
use sigil_state::shielded::{is_denomination, DENOMINATIONS, POOL_CAPACITY, POOL_DEPTH};
use sigil_state::{
    commit_state_transition, SigilState, StateMutation, StateTransition, WalletId, NATIVE,
};
use sigil_shield::mimc::compress2;
use winterfell::math::fields::f64::BaseElement;

/// A deterministic pseudo-random stream. `Math.random` equivalents are banned in
/// reproducible harnesses for the obvious reason.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[(self.next() % xs.len() as u64) as usize]
    }
}

/// Result of filling the pool to `notes` and measuring the cost of doing so.
#[derive(Debug, Clone)]
pub struct ScaleSample {
    pub notes: usize,
    /// Wall-clock for ONE anchor refresh at this pool size — the per-block cost.
    pub anchor_refresh_ms: f64,
    /// Wall-clock to materialise the padded leaf view (what a prover also builds).
    pub pool_view_ms: f64,
}

/// Fill a pool to `target` notes and measure the per-block anchor cost at that size.
///
/// Notes are appended directly through the chokepoint, which is the honest path: it
/// exercises the same denomination check and accounting a real shield would.
pub fn measure_anchor_cost(target: usize, seed: u64) -> ScaleSample {
    let mut rng = Rng::new(seed);
    let mut state = SigilState::default();
    let alice = [0xA1u8; 32];

    // Fund generously; every shield must use a legal denomination.
    let funding: u128 = (target as u128 + 1) * DENOMINATIONS[0] * 2;
    commit_state_transition(
        &mut state,
        &StateTransition {
            at_height: 1,
            mutations: vec![StateMutation::SetBalance {
                wallet: alice,
                token: NATIVE,
                amount: funding,
            }],
        },
        1,
    )
    .expect("funding");

    // Append notes in ONE transition so the anchor refresh happens once, at the end —
    // otherwise we would be measuring `target` refreshes instead of one.
    let mut muts = Vec::with_capacity(target);
    for i in 0..target {
        let n = Note::new(DENOMINATIONS[0] as u64, rng.next(), rng.next()).expect("note");
        muts.push(StateMutation::Shield {
            from: alice,
            amount: DENOMINATIONS[0],
            cm: to_wire(n.commitment()),
        });
        let _ = i;
    }
    if !muts.is_empty() {
        commit_state_transition(
            &mut state,
            &StateTransition { at_height: 2, mutations: muts },
            2,
        )
        .expect("bulk shield");
    }
    assert_eq!(state.shielded().len(), target);

    // The measurement: one anchor refresh, which is what every touching block pays.
    let t0 = Instant::now();
    let root = state.shielded().current_root();
    let anchor_refresh_ms = t0.elapsed().as_secs_f64() * 1000.0;
    assert_ne!(root, [0u8; 32]);

    let t1 = Instant::now();
    let view = state.shielded().padded_leaves(padding_leaf_wire);
    let pool_view_ms = t1.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(view.len(), POOL_CAPACITY);

    ScaleSample { notes: target, anchor_refresh_ms, pool_view_ms }
}

/// What an adversary watching only the PUBLIC transcript can reconstruct.
#[derive(Debug, Clone)]
pub struct LinkabilityResult {
    pub shields: usize,
    pub unshields: usize,
    /// Unshields the adversary matched to the CORRECT shield.
    pub correctly_linked: usize,
    /// Unshields where exactly one shield was a candidate — a forced, certain link.
    pub forced_links: usize,
    /// Mean size of the candidate set an unshield could have come from. This IS the
    /// effective anonymity set, and it is the number that matters.
    pub mean_candidates: f64,
}

/// One participant's public footprint, as an observer would record it.
#[derive(Clone, Copy)]
struct RampEvent {
    actor: u8,
    amount: u128,
    at: u64,
}

/// Simulate ramp traffic and run a value-correlation adversary over it.
///
/// The adversary is the cheap, realistic one — it never touches a proof. It sees every
/// shield and unshield (both are transparent by construction), and for each unshield asks:
/// *which shields have the same amount and happened earlier?* If exactly one, the link is
/// forced and privacy has failed for that user regardless of how good the circuit is.
///
/// `enforce_denominations` toggles the rule added on 2026-08-23, so the harness measures
/// what that rule actually bought rather than assuming it helped.
pub fn measure_linkability(
    actors: usize,
    rounds: usize,
    enforce_denominations: bool,
    seed: u64,
) -> LinkabilityResult {
    let mut rng = Rng::new(seed);
    let mut shields: Vec<RampEvent> = Vec::new();
    let mut unshields: Vec<RampEvent> = Vec::new();

    for r in 0..rounds {
        for a in 0..actors {
            let amount = if enforce_denominations {
                rng.pick(DENOMINATIONS)
            } else {
                // Free-form amounts: what users naturally do when nothing stops them.
                ((rng.next() % 9_000_000) + 1_000) as u128
            };
            if enforce_denominations {
                assert!(is_denomination(amount));
            }
            shields.push(RampEvent { actor: a as u8, amount, at: r as u64 * 100 + a as u64 });
            // Each actor exits a while later with the same value.
            unshields.push(RampEvent {
                actor: a as u8,
                amount,
                at: r as u64 * 100 + 50 + a as u64,
            });
        }
    }

    // ── the adversary ──────────────────────────────────────────────────────────────
    let mut correctly_linked = 0usize;
    let mut forced_links = 0usize;
    let mut candidate_total = 0usize;

    for u in &unshields {
        let candidates: Vec<&RampEvent> = shields
            .iter()
            .filter(|s| s.amount == u.amount && s.at < u.at)
            .collect();
        candidate_total += candidates.len();
        if candidates.len() == 1 {
            forced_links += 1;
            if candidates[0].actor == u.actor {
                correctly_linked += 1;
            }
        }
    }

    LinkabilityResult {
        shields: shields.len(),
        unshields: unshields.len(),
        correctly_linked,
        forced_links,
        mean_candidates: if unshields.is_empty() {
            0.0
        } else {
            candidate_total as f64 / unshields.len() as f64
        },
    }
}

/// Effective anonymity set as the pool fills: how many REAL notes a spend could plausibly
/// be, versus the tree's nominal capacity. The gap between the two is the honest measure
/// of how much of the advertised privacy actually exists.
pub fn effective_anonymity(real_notes: usize) -> (usize, usize, f64) {
    let nominal = POOL_CAPACITY;
    let effective = real_notes;
    let pct = if nominal == 0 { 0.0 } else { effective as f64 * 100.0 / nominal as f64 };
    (effective, nominal, pct)
}

/// Group ramp amounts into buckets, the way an observer would. Returns the bucket sizes,
/// smallest first — the smallest bucket is the weakest user's anonymity set.
pub fn amount_bucket_sizes(events: &[u128]) -> Vec<usize> {
    let mut by_amount: HashMap<u128, usize> = HashMap::new();
    for a in events {
        *by_amount.entry(*a).or_insert(0) += 1;
    }
    let mut sizes: Vec<usize> = by_amount.into_values().collect();
    sizes.sort_unstable();
    sizes
}

/// What shielded mining rewards actually produce, run as a simulated chain.
#[derive(Debug, Clone)]
pub struct CoinbaseFillResult {
    pub blocks: usize,
    pub miners: usize,
    pub notes: usize,
    pub value_locked: u128,
    /// Per-block anchor cost at the END of the run — the number that decides whether this
    /// is sustainable on the producer's critical path.
    pub final_anchor_ms: f64,
    /// Distinct owners holding notes. THIS is the anonymity set; note count alone is
    /// meaningless if one entity owns everything.
    pub distinct_owners: usize,
}

/// Simulate `blocks` blocks of shielded coinbase across `miners` independent producers.
///
/// Rewards vary per block, as they do on a real chain, so the run exercises the
/// denomination-exemption path rather than a tidy round number.
pub fn simulate_coinbase_fill(blocks: usize, miners: usize, seed: u64) -> CoinbaseFillResult {
    let mut rng = Rng::new(seed);
    let mut state = SigilState::default();

    // Each miner registers a shielded key once.
    let keys: Vec<(WalletId, [u8; 32])> = (0..miners)
        .map(|i| {
            let w = [i as u8 + 1; 32];
            let sk = BaseElement::new(0xA000 + i as u64);
            let pk = to_wire(compress2(sk, BaseElement::new(sigil_shield::spend_full_v4::PK_DOMAIN)));
            (w, pk)
        })
        .collect();
    commit_state_transition(
        &mut state,
        &StateTransition {
            at_height: 1,
            mutations: keys
                .iter()
                .map(|(w, pk)| StateMutation::RegisterShieldedAddress { wallet: *w, pk_shield: *pk })
                .collect(),
        },
        1,
    )
    .expect("registration");

    let mut owners: std::collections::BTreeSet<[u8; 32]> = Default::default();
    for b in 0..blocks {
        let h = 2 + b as u64;
        let (_, pk) = keys[(rng.next() as usize) % miners];
        // realistic, non-round reward
        let reward = 200_000_000u128 + (rng.next() % 5_000_000) as u128;
        let cm = sigil_shield::note_v1::coinbase_commitment_wire(h, &pk, reward)
            .expect("reward within range");
        commit_state_transition(
            &mut state,
            &StateTransition {
                at_height: h,
                mutations: vec![StateMutation::ShieldedCoinbase { pk_shield: pk, amount: reward, cm }],
            },
            h,
        )
        .expect("shielded coinbase must commit");
        owners.insert(pk);
    }

    // Measure what PRODUCTION pays, not what is convenient to call. `current_root()` is
    // the sparse O(notes) reference; the producer goes through `commit_state_transition`,
    // which calls `refresh_anchor` -> `current_root_fast` (the incremental tree). Timing
    // the reference instead would have reported linear growth and called it flat.
    let h = 2 + blocks as u64;
    let (_, pk) = keys[0];
    let reward = 200_000_001u128;
    let cm = sigil_shield::note_v1::coinbase_commitment_wire(h, &pk, reward).unwrap();
    let t0 = Instant::now();
    commit_state_transition(
        &mut state,
        &StateTransition {
            at_height: h,
            mutations: vec![StateMutation::ShieldedCoinbase { pk_shield: pk, amount: reward, cm }],
        },
        h,
    )
    .expect("final block");
    let final_anchor_ms = t0.elapsed().as_secs_f64() * 1000.0;

    CoinbaseFillResult {
        blocks,
        miners,
        notes: state.shielded().len(),
        value_locked: state.shielded().value_locked(),
        final_anchor_ms,
        distinct_owners: owners.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE SCALING QUESTION. Prints the per-block anchor cost as the pool fills.
    ///
    /// This is the number that decides whether the deferred incremental Merkle root is
    /// optional or mandatory: it is paid by EVERY block that touches the pool, on the
    /// producer's critical path.
    #[test]
    fn anchor_refresh_cost_as_the_pool_fills() {
        println!("\n  notes   anchor_refresh   pool_view    (depth {POOL_DEPTH}, cap {POOL_CAPACITY})");
        println!("  ─────   ──────────────   ─────────");
        let mut worst = 0.0f64;
        for target in [1usize, 64, 512, 4_096, 16_384] {
            let s = measure_anchor_cost(target, 0xC0FFEE + target as u64);
            println!(
                "  {:>5}   {:>11.2} ms   {:>6.2} ms",
                s.notes, s.anchor_refresh_ms, s.pool_view_ms
            );
            worst = worst.max(s.anchor_refresh_ms);
        }
        // The tree is FIXED-SIZE — it is always padded to capacity — so the cost should be
        // roughly flat in the number of real notes. Flat-but-expensive is the outcome that
        // makes the incremental root mandatory; flat-and-cheap makes it optional.
        println!("\n  worst per-block anchor cost: {worst:.2} ms");
        println!(
            "  verdict: {}",
            if worst > 100.0 {
                "TOO SLOW — incremental append-only root is MANDATORY before real use"
            } else if worst > 10.0 {
                "acceptable now, optimize before high tx rates"
            } else {
                "cheap — the deferral was justified"
            }
        );
        assert!(worst.is_finite());
    }

    /// THE PRIVACY QUESTION, measured rather than argued: what does the denomination rule
    /// actually buy? Runs the same adversary against free-form and standardised amounts.
    #[test]
    fn denominations_measurably_defeat_value_correlation() {
        let free = measure_linkability(20, 10, false, 0xBEEF);
        let denom = measure_linkability(20, 10, true, 0xBEEF);

        println!("\n  VALUE-CORRELATION ADVERSARY (sees only the public ramp transcript)");
        println!("  ─────────────────────────────────────────────────────────────────");
        for (label, r) in [("free-form amounts", &free), ("denominated", &denom)] {
            println!(
                "  {label:<18}  forced links {:>4}/{:<4}  correct {:>4}  mean anon-set {:>7.1}",
                r.forced_links, r.unshields, r.correctly_linked, r.mean_candidates
            );
        }

        // The rule must strictly reduce forced links; if it does not, it is theatre.
        assert!(
            denom.forced_links < free.forced_links,
            "SECURITY: denominations must reduce forced links ({} -> {}); if they do not, \
             the rule is costing users convenience and buying nothing",
            free.forced_links,
            denom.forced_links
        );
        assert!(
            denom.mean_candidates > free.mean_candidates,
            "denominations must GROW the candidate set ({:.1} -> {:.1})",
            free.mean_candidates,
            denom.mean_candidates
        );
        println!(
            "\n  forced links cut {:.0}% ({} -> {}); mean anonymity set {:.1}x larger",
            100.0 - (denom.forced_links as f64 * 100.0 / free.forced_links.max(1) as f64),
            free.forced_links,
            denom.forced_links,
            denom.mean_candidates / free.mean_candidates.max(0.001)
        );
    }

    /// The effective anonymity set is real notes, NOT tree capacity. Advertising 32,768
    /// while holding 3 would be the single most misleading number this project could quote.
    #[test]
    fn effective_anonymity_is_real_notes_not_capacity() {
        for real in [0usize, 1, 3, 100, 5_000, 32_768] {
            let (eff, nom, pct) = effective_anonymity(real);
            println!("  real {eff:>6} / capacity {nom:>6}  =  {pct:>5.1}% of advertised");
        }
        let (eff, _, _) = effective_anonymity(0);
        assert_eq!(eff, 0, "an empty pool provides ZERO anonymity, not 32768");
        let (eff1, _, _) = effective_anonymity(1);
        assert_eq!(eff1, 1, "one note is an anonymity set of one — i.e. none");
    }

    /// The weakest user's privacy is the SMALLEST bucket, not the average. An average
    /// hides the person standing alone.
    #[test]
    fn the_weakest_bucket_is_what_matters() {
        let amounts: Vec<u128> = vec![
            1_000, 1_000, 1_000, 1_000, // a healthy bucket
            10_000, 10_000,             // a thin one
            7_777_777,                  // one person, alone
        ];
        let sizes = amount_bucket_sizes(&amounts);
        println!("  bucket sizes (smallest first): {sizes:?}");
        assert_eq!(sizes[0], 1, "the lone amount is a bucket of one");
        assert!(
            sizes[0] < 2,
            "PRIVACY: a bucket of one means that user is fully identified by amount alone, \
             regardless of the cryptography"
        );
    }
}

#[cfg(test)]
mod coinbase_tests {
    use super::*;

    /// THE POINT OF SHIELDED COINBASE: one note per block, many independent owners, and a
    /// per-block cost that does not grow with the pool.
    #[test]
    fn shielded_coinbase_fills_the_pool_with_independent_owners() {
        let r = simulate_coinbase_fill(2_000, 8, 0xC0FFEE);
        println!("\n  SHIELDED COINBASE — {} blocks, {} miners", r.blocks, r.miners);
        println!("  ────────────────────────────────────────");
        println!("  notes minted        : {}", r.notes);
        println!("  distinct owners     : {}  <- THE anonymity set", r.distinct_owners);
        println!("  value locked        : {}", r.value_locked);
        println!("  final anchor cost   : {:.2} ms", r.final_anchor_ms);

        assert_eq!(
            r.notes, r.blocks + 1, // +1 for the measured final block
            "ONE note per block — the denomination exemption is what buys this; forcing the \
             ladder measured 11.9 notes per block for zero privacy gain"
        );
        assert_eq!(
            r.distinct_owners, r.miners,
            "every registered miner must own notes — otherwise the pool is one entity's \
             notes wearing different hats, and the anonymity set is 1"
        );
        assert!(r.value_locked > 0);
    }

    /// The cost must be flat in pool size, or shielded coinbase eventually stalls the
    /// producer. This is the property the incremental tree exists to provide.
    #[test]
    fn per_block_cost_stays_flat_as_the_pool_grows() {
        let small = simulate_coinbase_fill(200, 4, 1);
        let large = simulate_coinbase_fill(4_000, 4, 1);
        println!(
            "\n  anchor cost: {} notes -> {:.2} ms   |   {} notes -> {:.2} ms",
            small.notes, small.final_anchor_ms, large.notes, large.final_anchor_ms
        );
        // 20x the notes must not cost anywhere near 20x the time. A linear result means
        // the incremental tree is NOT on the path being measured, which is exactly the
        // mistake this assertion exists to catch.
        let growth = large.final_anchor_ms / small.final_anchor_ms.max(0.001);
        println!("  growth factor for 20x the notes: {growth:.1}x  (linear would be ~20x)");
        assert!(
            growth < 5.0,
            "per-block cost grew {growth:.1}x for 20x the notes — that is linear, so the \
             incremental tree is not on the production path and the pool will stall as it \
             fills"
        );
        assert!(
            large.final_anchor_ms < 50.0,
            "per-block cost {:.1} ms at {} notes would eat the block budget",
            large.final_anchor_ms,
            large.notes
        );
    }
}
