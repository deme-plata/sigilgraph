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
                .map(|(w, pk)| StateMutation::RegisterShieldedAddress { wallet: *w, pk_shield: *pk, pk_encrypt: None })
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

/// One miner in a realistic mining-power distribution: a few large operators and a long
/// tail of small ones, the shape real hashpower actually takes — not N equal miners, which
/// would hide exactly the question that matters here (does a WHALE's shielded reward look
/// any different from a MINNOW's once it's in the pool?).
#[derive(Clone, Copy)]
pub struct WeightedMiner {
    pub wallet: WalletId,
    /// Relative mining-power weight (not a probability; normalised at selection time).
    pub weight: u64,
    /// Registered for shielded rewards via `RegisterShieldedAddress`. An UNREGISTERED
    /// miner's reward stays fully transparent — that's the honest contrast this scenario
    /// exists to show, not a gap to route around.
    pub registered: bool,
    /// Only meaningful when `registered`; the key the coinbase note binds to.
    pub pk_shield: [u8; 32],
}

/// Build `n` miners on a Zipf-like power-law weight curve (`weight ∝ 1/rank`), the standard
/// stand-in for real hashpower concentration, alternating registered/unregistered by index
/// so BOTH classes contain both large and small operators — otherwise "registered" and
/// "large" would be confounded and any measurement below would be meaningless.
pub fn realistic_miner_set(n: usize) -> Vec<WeightedMiner> {
    (0..n)
        .map(|i| {
            let wallet = [i as u8 + 1; 32];
            let weight = (10_000 / (i as u64 + 1)).max(1); // 10000, 5000, 3333, 2500, ...
            let registered = i % 2 == 0;
            let sk = BaseElement::new(0xB000 + i as u64);
            let pk_shield =
                to_wire(compress2(sk, BaseElement::new(sigil_shield::spend_full_v4::PK_DOMAIN)));
            WeightedMiner { wallet, weight, registered, pk_shield }
        })
        .collect()
}

/// Pick a miner index proportional to weight — the deterministic weighted lottery every
/// PoW/PoS block-winner selection reduces to.
fn weighted_pick(miners: &[WeightedMiner], rng: &mut Rng) -> usize {
    let total: u64 = miners.iter().map(|m| m.weight).sum();
    let mut r = rng.next() % total.max(1);
    for (i, m) in miners.iter().enumerate() {
        if r < m.weight {
            return i;
        }
        r -= m.weight;
    }
    miners.len() - 1
}

/// What a realistic, mixed registered/unregistered mining population actually produces.
#[derive(Debug, Clone)]
pub struct RealisticPoolResult {
    pub blocks: usize,
    pub miners: usize,
    /// Share of TOTAL mining weight that belongs to registered (shielded) miners.
    pub registered_weight_share: f64,
    /// Share of blocks ACTUALLY won by registered miners — should track
    /// `registered_weight_share` if the weighted lottery is fair.
    pub registered_blocks_share: f64,
    pub shielded_value: u128,
    pub transparent_value: u128,
    pub distinct_shielded_owners: usize,
    /// A mint-time adversary who knows only the public transcript (registered pks, block
    /// heights, block reward amounts — all public) tries to attribute each pool leaf to the
    /// registered miner who won it, by recomputing `coinbase_commitment_wire(height, pk,
    /// amount)` for every candidate pk and matching against the real leaves.
    pub mint_time_correctly_attributed: usize,
    pub mint_time_total_shielded_notes: usize,
}

/// Simulate `blocks` of REAL block production across a realistic, mixed miner population:
/// registered miners' rewards mint as shielded notes via the real `ShieldedCoinbase`
/// mutation (exactly the production path); unregistered miners' rewards credit a plain
/// transparent balance (the real chain does this outside `sigil-state`, in the producer;
/// modelled here as a direct balance credit, which is the honest state-layer equivalent —
/// noted as a simplification, not claimed as byte-identical to the producer's own code).
pub fn simulate_realistic_mining_pool(
    blocks: usize,
    miners: &[WeightedMiner],
    seed: u64,
) -> RealisticPoolResult {
    let mut rng = Rng::new(seed);
    let mut state = SigilState::default();

    let registrations: Vec<StateMutation> = miners
        .iter()
        .filter(|m| m.registered)
        .map(|m| StateMutation::RegisterShieldedAddress {
            wallet: m.wallet,
            pk_shield: m.pk_shield,
            pk_encrypt: None,
        })
        .collect();
    if !registrations.is_empty() {
        commit_state_transition(
            &mut state,
            &StateTransition { at_height: 1, mutations: registrations },
            1,
        )
        .expect("registration");
    }

    let total_weight: u64 = miners.iter().map(|m| m.weight).sum();
    let registered_weight: u64 = miners.iter().filter(|m| m.registered).map(|m| m.weight).sum();

    let mut owners: std::collections::BTreeSet<[u8; 32]> = Default::default();
    let mut registered_wins = 0usize;
    // (height, pk, amount) for every shielded coinbase this run mints — what the mint-time
    // adversary below gets to see (all three fields are public on any real chain).
    let mut shielded_mints: Vec<(u64, [u8; 32], u128)> = Vec::with_capacity(blocks);

    for b in 0..blocks {
        let h = 2 + b as u64;
        let idx = weighted_pick(miners, &mut rng);
        let m = miners[idx];
        let reward = 200_000_000u128 + (rng.next() % 5_000_000) as u128;

        if m.registered {
            registered_wins += 1;
            let cm = sigil_shield::note_v1::coinbase_commitment_wire(h, &m.pk_shield, reward)
                .expect("reward within range");
            commit_state_transition(
                &mut state,
                &StateTransition {
                    at_height: h,
                    mutations: vec![StateMutation::ShieldedCoinbase {
                        pk_shield: m.pk_shield,
                        amount: reward,
                        cm,
                    }],
                },
                h,
            )
            .expect("shielded coinbase must commit");
            owners.insert(m.pk_shield);
            shielded_mints.push((h, m.pk_shield, reward));
        } else {
            let prior = state.balance_of(&m.wallet, &NATIVE);
            commit_state_transition(
                &mut state,
                &StateTransition {
                    at_height: h,
                    mutations: vec![StateMutation::SetBalance {
                        wallet: m.wallet,
                        token: NATIVE,
                        amount: prior + reward,
                    }],
                },
                h,
            )
            .expect("transparent reward must commit");
        }
    }

    let transparent_value: u128 = miners
        .iter()
        .filter(|m| !m.registered)
        .map(|m| state.balance_of(&m.wallet, &NATIVE))
        .sum();

    // ── the mint-time linkage adversary ───────────────────────────────────────────────
    // Every leaf really in the pool, in the exact order minted.
    let pool_leaves = state.shielded().notes().to_vec();
    let registered_pks: Vec<[u8; 32]> =
        miners.iter().filter(|m| m.registered).map(|m| m.pk_shield).collect();
    let mut correctly_attributed = 0usize;
    for (h, true_pk, amount) in &shielded_mints {
        let mut match_count = 0usize;
        let mut matched_pk = None;
        for cand in &registered_pks {
            if let Some(cm) = sigil_shield::note_v1::coinbase_commitment_wire(*h, cand, *amount) {
                if pool_leaves.contains(&cm) {
                    match_count += 1;
                    matched_pk = Some(*cand);
                }
            }
        }
        if match_count == 1 && matched_pk == Some(*true_pk) {
            correctly_attributed += 1;
        }
    }

    RealisticPoolResult {
        blocks,
        miners: miners.len(),
        registered_weight_share: registered_weight as f64 / total_weight.max(1) as f64,
        registered_blocks_share: registered_wins as f64 / blocks.max(1) as f64,
        shielded_value: state.shielded().value_locked(),
        transparent_value,
        distinct_shielded_owners: owners.len(),
        mint_time_correctly_attributed: correctly_attributed,
        mint_time_total_shielded_notes: shielded_mints.len(),
    }
}

#[cfg(test)]
mod realistic_mining_tests {
    use super::*;

    /// THE HEADLINE SCENARIO: a realistic whale-and-minnow mining population, some
    /// registered for privacy and some not, over a real (chronos-scale) number of blocks.
    #[test]
    fn realistic_mixed_population_grows_the_pool_proportionally_to_registered_weight() {
        let miners = realistic_miner_set(12);
        let r = simulate_realistic_mining_pool(3_000, &miners, 0x5161);

        println!("\n  REALISTIC MIXED MINING POPULATION — {} blocks, {} miners", r.blocks, r.miners);
        println!("  ────────────────────────────────────────────────────────────");
        println!("  registered weight share   : {:.1}%", r.registered_weight_share * 100.0);
        println!("  registered blocks won     : {:.1}%", r.registered_blocks_share * 100.0);
        println!("  shielded value locked     : {}", r.shielded_value);
        println!("  transparent value credited: {}", r.transparent_value);
        println!("  distinct shielded owners  : {}", r.distinct_shielded_owners);
        println!(
            "  shielded / total emission : {:.1}%",
            100.0 * r.shielded_value as f64
                / (r.shielded_value as f64 + r.transparent_value as f64).max(1.0)
        );

        // The weighted lottery must actually respect weight, not just claim to: registered
        // share of BLOCKS WON should track registered share of WEIGHT within sampling noise
        // over 3,000 draws.
        let drift = (r.registered_blocks_share - r.registered_weight_share).abs();
        assert!(
            drift < 0.05,
            "registered blocks-won share ({:.3}) drifted too far from registered weight \
             share ({:.3}) — the weighted selection is not actually weighted",
            r.registered_blocks_share,
            r.registered_weight_share
        );

        // Every registered miner that won at least one block must show up as a distinct
        // owner — a whale and a minnow's notes must be indistinguishable in the pool, not
        // merged into one entry.
        assert!(r.distinct_shielded_owners >= 2, "need multiple registered owners represented");
        assert!(r.shielded_value > 0 && r.transparent_value > 0, "both classes must be exercised");
    }

    /// THE PRIVACY FINDING, measured rather than assumed: mint-time attribution is FULLY
    /// public by design (blinding derives deterministically from height+pk, both public),
    /// so anyone can compute which registered miner won which block. That is NOT a bug —
    /// `sigil_shield::note_v1::coinbase_blinding`'s own doc comment states privacy for a
    /// coinbase note "arrives at SPEND time, not mint time" — but it is a real, easy-to-miss
    /// property worth pinning with a number rather than a comment: an observer who does
    /// nothing but public arithmetic gets a FORCED, 100% correct link from note to miner at
    /// mint. If this ever drops below 100%, either the design changed or something is
    /// broken; if anyone reads this pool as "private the moment a reward lands," this test
    /// is the correction.
    #[test]
    fn coinbase_notes_are_publicly_attributable_to_their_miner_at_mint_time() {
        let miners = realistic_miner_set(8);
        let r = simulate_realistic_mining_pool(1_000, &miners, 0xC0FFEE);

        println!("\n  MINT-TIME LINKAGE ADVERSARY (public arithmetic only, no proof, no chain scan)");
        println!("  ───────────────────────────────────────────────────────────────────────────");
        println!(
            "  correctly attributed: {} / {}",
            r.mint_time_correctly_attributed, r.mint_time_total_shielded_notes
        );

        assert_eq!(
            r.mint_time_correctly_attributed, r.mint_time_total_shielded_notes,
            "EXPECTED BY DESIGN, worth failing loudly if it ever changes: every shielded \
             coinbase note must be attributable to its miner at mint time by public \
             arithmetic alone — anonymity for these notes is a SPEND-time property, not a \
             mint-time one. A number below 100% here means the deterministic-blinding \
             design changed without this test being updated, not that privacy improved."
        );
    }

    /// The contrast this whole scenario exists to show: an UNREGISTERED miner's reward
    /// never touches the shielded pool at all, no matter how much weight it has.
    #[test]
    fn unregistered_miners_never_contribute_to_the_pool() {
        let miners = realistic_miner_set(6);
        let r = simulate_realistic_mining_pool(600, &miners, 7);
        let unregistered_wallets: Vec<WalletId> =
            miners.iter().filter(|m| !m.registered).map(|m| m.wallet).collect();

        // None of the pool's owner set may be an unregistered miner's key — there is no
        // key for one anyway (only `pk_shield` ever enters the pool), but the stronger,
        // directly-checkable claim is that every unregistered miner actually accumulated a
        // transparent balance, proving their rewards landed somewhere real rather than
        // being silently dropped or misrouted into the pool.
        assert!(!unregistered_wallets.is_empty());
        assert!(
            r.transparent_value > 0,
            "unregistered miners must have received real transparent balance"
        );
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
