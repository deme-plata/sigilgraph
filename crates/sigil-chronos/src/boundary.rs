//! `boundary.rs` — **total supply as a boundary integral across a checkpoint.**
//!
//! ## Where this comes from
//!
//! Meissner & Penrose, *"The Physics of Conformal Cyclic Cosmology"*
//! (arXiv:2503.24263 §III). In CCC, two cosmic aeons are glued across a spacelike
//! **crossover 3-surface** X. The conserved quantity across X is a **2-surface
//! integral** of the rescaled Weyl spinor,
//!
//! ```text
//!     M = ∮ ψ_ABCD x^(A E') t^B)E' ε_A'B' dx^a dx^b
//! ```
//!
//! and the conservation argument is structural, not numerical: `M` can change
//! *only* by local flux across the 2-surface S. Kill the flux term and `M` is
//! conserved, full stop.
//!
//! Penrose's crossover is conformally smooth **everywhere on X except at a
//! discrete set of points** — the **Hawking points**, where the entire
//! mass-energy of a prior-aeon galactic cluster concentrates into a singularity
//! and smoothness fails.
//!
//! A chain checkpoint has exactly this shape. Total supply is the boundary
//! integral; a block boundary is the crossover surface; and a site where value
//! appears without being declared is a Hawking point.
//!
//! ## What SIGIL enforced before this module
//!
//! [`sigil_state::commit_state_transition`] enforces a **cap**:
//! `post_state_supply > MAX_SUPPLY` → reject the block. That is a *bound*, not a
//! conservation law. [`sigil_state::StateMutation::SetBalance`] writes an
//! **absolute** amount, so at the chokepoint a legitimate 50,000-base-unit
//! coinbase and a phantom mint of the same size are *the same operation*. Both
//! commit cleanly as long as the total stays under 21M.
//!
//! The pre-existing conservation tests assert inequalities in the same spirit —
//! `total <= initial_supply`, `initial_supply - total <= n_sends`. Those catch
//! gross inflation. They cannot catch value entering or leaving *within* the
//! bound, which is the whole class this module exists to close.
//!
//! ## The law this module enforces
//!
//! ```text
//!     supply(h₂) − supply(h₁)  ==  Σ declared_mint  −  Σ declared_burn
//! ```
//!
//! Anything else is undeclared flux across the boundary — a Hawking point.
//!
//! Two independent checks, because they fail differently:
//!
//! 1. **Accumulator drift** — the O(1) incremental `native_supply()` must equal
//!    the O(state) [`sigil_state::SigilState::native_supply_recompute`]. They can
//!    legitimately diverge because `set_balance` maintains the counter with
//!    *saturating* arithmetic. Once they do, the cap check is no longer a
//!    statement about balances anyone actually holds.
//! 2. **Undeclared flux** — the recomputed supply delta must equal what the
//!    blocks themselves declared.
//!
//! The flux law is evaluated against the **recomputed** supply, never the
//! incremental counter. Checking a counter against itself is how the flux-db
//! suite passed 105/105 while destroying terabytes.
//!
//! ## Known gap, stated plainly
//!
//! A SIGIL block **declares its mint** — every coinbase emits
//! [`sigil_events::SigilEvent::MintReward`] — but it **does not declare its
//! burn**. Fees are debited from the sender in `apply_tx` and credited nowhere;
//! no event records the amount. So [`DeclaredFlux::from_block`] can read the
//! mint side out of block data alone, and the burn side has to be supplied by
//! the caller ([`DeclaredFlux::with_burn`]).
//!
//! That means the boundary integral **cannot currently be closed from block data
//! alone** — a verifier holding only the chain cannot tell "1,000 base units of
//! fees were burned" from "1,000 base units leaked." Closing it needs a
//! `FeeBurned` event (or a burn field on the header). Recorded here rather than
//! papered over; see the module tests, which pin the gap.

use sigil_events::SigilEvent;
use sigil_header::Root;
use sigil_state::SigilState;

use crate::{Block, SigilSimNode};

/// The boundary integral evaluated at one checkpoint — the discrete analogue of
/// CCC's 2-surface integral `M`, plus the audit data needed to trust it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Boundary {
    /// Height at which this surface was taken.
    pub height: u64,
    /// The O(1) incrementally-maintained counter.
    pub supply_incremental: u128,
    /// The O(state) ground truth, summed from the wallet map.
    pub supply_recomputed: u128,
    /// Wallet state root at this surface — binds the integral to a committed state.
    pub wallet_root: Root,
}

impl Boundary {
    /// Evaluate the integral over a raw state at `height`.
    pub fn of_state(state: &SigilState, height: u64) -> Self {
        Self {
            height,
            supply_incremental: state.native_supply(),
            supply_recomputed: state.native_supply_recompute(),
            wallet_root: state.roots().wallet_state_root,
        }
    }

    /// Evaluate the integral over a live simulated node.
    pub fn of_node(node: &SigilSimNode) -> Self {
        // `SigilSimNode` is defined in the crate root, so its private fields are
        // reachable from this descendant module — no API widening needed.
        Self::of_state(&node.state, node.height())
    }

    /// Does the O(1) counter agree with the O(state) ground truth?
    pub fn is_consistent(&self) -> bool {
        self.supply_incremental == self.supply_recomputed
    }
}

/// Flux declared to cross a boundary: what the chain *says* it minted and burned.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeclaredFlux {
    /// Sum of `MintReward` amounts — read directly from block events.
    pub minted: u128,
    /// Sum of burned fees. **Not** recoverable from block data (see module docs);
    /// the caller supplies it.
    pub burned: u128,
}

impl DeclaredFlux {
    /// Read the declared mint out of a block's own event log. `burned` is left
    /// at zero because a SIGIL block does not declare its burn.
    pub fn from_block(block: &Block) -> Self {
        let minted = block
            .events
            .iter()
            .filter_map(|e| match e {
                SigilEvent::MintReward { amount, .. } => Some(*amount),
                _ => None,
            })
            .fold(0u128, |a, v| a.saturating_add(v));
        Self { minted, burned: 0 }
    }

    /// Sum the declared flux of every block in a span.
    pub fn from_blocks<'a>(blocks: impl IntoIterator<Item = &'a Block>) -> Self {
        blocks.into_iter().fold(Self::default(), |acc, b| {
            let f = Self::from_block(b);
            Self {
                minted: acc.minted.saturating_add(f.minted),
                burned: acc.burned.saturating_add(f.burned),
            }
        })
    }

    /// Attach the burn term the block could not declare.
    pub fn with_burn(self, burned: u128) -> Self {
        Self { burned, ..self }
    }

    /// Net declared flux across the surface, signed.
    pub fn net(&self) -> i128 {
        self.minted as i128 - self.burned as i128
    }
}

/// Outcome of testing one boundary crossing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// `Δsupply == declared mint − declared burn`. The flux term is zero.
    Conserved {
        /// The change in supply across the crossing.
        delta: i128,
    },
    /// The O(1) counter disagrees with the O(state) truth, so the cap check no
    /// longer describes real balances. Reported *before* the flux law, because
    /// a drifted counter makes the flux law meaningless.
    AccumulatorDrift {
        /// Height at which drift was observed.
        height: u64,
        /// The incremental counter's value.
        incremental: u128,
        /// The recomputed ground truth.
        recomputed: u128,
    },
    /// A **Hawking point**: value crossed the boundary that no block declared.
    UndeclaredFlux {
        /// What the supply actually did.
        actual: i128,
        /// What the chain said it would do.
        declared: i128,
        /// `actual − declared`. Positive = undeclared mint; negative = undeclared burn.
        excess: i128,
    },
}

impl Verdict {
    /// Did the boundary integral survive the crossing?
    pub fn is_conserved(&self) -> bool {
        matches!(self, Verdict::Conserved { .. })
    }
}

/// Test one crossing `before → after` against the flux the chain declared.
///
/// The flux law is evaluated on `supply_recomputed` — the O(state) ground truth —
/// never on the incremental counter. Drift is reported separately and first.
pub fn check_crossing(before: &Boundary, after: &Boundary, flux: DeclaredFlux) -> Verdict {
    for b in [before, after] {
        if !b.is_consistent() {
            return Verdict::AccumulatorDrift {
                height: b.height,
                incremental: b.supply_incremental,
                recomputed: b.supply_recomputed,
            };
        }
    }

    let actual = after.supply_recomputed as i128 - before.supply_recomputed as i128;
    let declared = flux.net();

    if actual == declared {
        Verdict::Conserved { delta: actual }
    } else {
        Verdict::UndeclaredFlux {
            actual,
            declared,
            excess: actual - declared,
        }
    }
}

/// The **order-invariant projection** of a node snapshot — "who is owed what",
/// with the order-recording fields stripped out.
///
/// Needed because a whole-snapshot byte-diff is the *wrong* invariant for a
/// mempool-reordering test. Two nodes that applied the same commutative tx set
/// in different orders legitimately hold a different `parent_hash` — block
/// contents encode which tx landed in which block, so the hash chain differs.
/// That is not a consensus fault. The settlement view is the part that genuinely
/// must not move, and it is exactly where the boundary integral lives.
///
/// MEASURED (`divergence_profile_is_measured_not_assumed`, 2026-07-29): across
/// four injection orders, **`parent_hash` is the only field that moves** — 32 of
/// 200 snapshot bytes. `event_log_root` is invariant, but for an unhelpful
/// reason: it roots the *currently-open* block's events, which every commit
/// clears, so a between-blocks snapshot always sees the empty-log root. Its
/// agreement carries no information about event history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettlementView {
    /// Chain height.
    pub height: u64,
    /// Root over wallet balances.
    pub wallet_root: Root,
    /// Root over DEX pools.
    pub dex_root: Root,
    /// Root over VM contract storage.
    pub contract_root: Root,
    /// Total native supply — the boundary integral itself.
    pub native_supply: u128,
}

/// Project a [`crate::SigilSimNode`] snapshot down to its order-invariant part.
/// Returns `None` if the snapshot is not the expected length.
pub fn settlement_view(snapshot: &[u8]) -> Option<SettlementView> {
    use crate::snap;
    if snapshot.len() != snap::LEN {
        return None;
    }
    let root = |r: std::ops::Range<usize>| -> Root {
        let mut out = [0u8; 32];
        out.copy_from_slice(&snapshot[r]);
        out
    };
    Some(SettlementView {
        height: u64::from_le_bytes(snapshot[snap::HEIGHT].try_into().ok()?),
        wallet_root: root(snap::WALLET_ROOT),
        dex_root: root(snap::DEX_ROOT),
        contract_root: root(snap::CONTRACT_ROOT),
        native_supply: u128::from_le_bytes(snapshot[snap::NATIVE_SUPPLY].try_into().ok()?),
    })
}

/// Walk a producer forward `n` blocks, testing the boundary law at **every**
/// block boundary rather than only at the endpoints.
///
/// Returns the per-block verdicts in height order. A leak that is later undone
/// nets to zero over a long span, so endpoint-only checking hides it — the
/// crossing must be tested at each surface.
pub fn audit_block_by_block(node: &mut SigilSimNode, n: u64, burn_per_block: u128) -> Vec<Verdict> {
    let mut verdicts = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let before = Boundary::of_node(node);
        let Some(block) = node.produce_one() else { break };
        let after = Boundary::of_node(node);
        let flux = DeclaredFlux::from_block(&block).with_burn(burn_per_block);
        verdicts.push(check_crossing(&before, &after, flux));
    }
    verdicts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{demo_genesis, sign_dummy, SigilSimNode, TAG_TX, BLOCK_REWARD};
    use flux_chronos::{
        secs, tourbillon, Injection, NetEdge, NodeId, ScenarioSeed, SimNode, Universe,
    };
    use sigil_state::{commit_state_transition, StateMutation, StateTransition};
    use sigil_tx::{SigilTx, SignedTx, NATIVE};

    fn wallet(tag: u8) -> [u8; 32] {
        [tag; 32]
    }

    /// Sends among funded wallets, fee 0 — so the burn term is exactly zero and
    /// the law reduces to `Δsupply == Σ MintReward`, closable from block data alone.
    fn free_send(n: u32) -> SignedTx {
        let from = wallet((n % 5 + 1) as u8);
        let to = wallet(((n + 1) % 5 + 1) as u8);
        sign_dummy(SigilTx::Send { from, to, amount: 100, token: NATIVE, fee: 0 })
    }

    /// The happy path: every block boundary conserves, and the only flux is the
    /// coinbase the block itself declared.
    #[test]
    fn every_block_boundary_conserves() {
        let g = demo_genesis();
        let mut node = SigilSimNode::new("producer", NodeId(0), vec![], true, secs(1), &g);
        for n in 0..40u32 {
            node.enqueue_tx(free_send(n));
        }
        let verdicts = audit_block_by_block(&mut node, 40, 0);
        assert_eq!(verdicts.len(), 40, "producer stalled early");
        for (i, v) in verdicts.iter().enumerate() {
            assert!(v.is_conserved(), "boundary {i} not conserved: {v:?}");
        }
        // Each block's only declared flux is one BLOCK_REWARD coinbase.
        assert_eq!(
            verdicts[0],
            Verdict::Conserved { delta: BLOCK_REWARD as i128 },
            "a fee-free block must move supply by exactly the coinbase"
        );
    }

    /// The detector must actually fire. Commit a `SetBalance` that raises a
    /// wallet **without** emitting a `MintReward` — a phantom mint that the
    /// existing cap check waves through because the total stays far under 21M.
    #[test]
    fn phantom_mint_is_detected_as_a_hawking_point() {
        let g = demo_genesis();
        let mut state = SigilState::new();
        commit_state_transition(&mut state, &g.build_block().transition, 0).unwrap();

        let before = Boundary::of_state(&state, 0);
        let victim = wallet(1);
        let inflated = state.balance_of(&victim, &NATIVE) + 777;
        let transition = StateTransition {
            at_height: 1,
            mutations: vec![StateMutation::SetBalance {
                wallet: victim,
                token: NATIVE,
                amount: inflated,
            }],
        };
        // The cap check accepts this — that is the point.
        commit_state_transition(&mut state, &transition, 1)
            .expect("cap check accepts an undeclared mint; only the boundary law catches it");
        let after = Boundary::of_state(&state, 1);

        let verdict = check_crossing(&before, &after, DeclaredFlux::default());
        assert_eq!(
            verdict,
            Verdict::UndeclaredFlux { actual: 777, declared: 0, excess: 777 },
            "777 base units crossed the boundary undeclared and went unnoticed"
        );
    }

    /// A burn is undeclared flux too — value leaving without a record is as much
    /// a boundary violation as value arriving.
    #[test]
    fn undeclared_burn_is_detected() {
        let g = demo_genesis();
        let mut state = SigilState::new();
        commit_state_transition(&mut state, &g.build_block().transition, 0).unwrap();
        let before = Boundary::of_state(&state, 0);

        let victim = wallet(2);
        let drained = state.balance_of(&victim, &NATIVE) - 500;
        let transition = StateTransition {
            at_height: 1,
            mutations: vec![StateMutation::SetBalance { wallet: victim, token: NATIVE, amount: drained }],
        };
        commit_state_transition(&mut state, &transition, 1).unwrap();
        let after = Boundary::of_state(&state, 1);

        match check_crossing(&before, &after, DeclaredFlux::default()) {
            Verdict::UndeclaredFlux { excess, .. } => assert_eq!(excess, -500),
            other => panic!("undeclared burn not caught: {other:?}"),
        }
    }

    /// Pins the known gap: the burn side is NOT recoverable from block data.
    /// A block carrying real fee burn declares mint only, so reading flux from
    /// the block alone mis-reports the crossing. When a `FeeBurned` event lands,
    /// this test should start failing — that is its job.
    #[test]
    fn burn_is_not_declared_by_the_block() {
        let g = demo_genesis();
        let mut node = SigilSimNode::new("producer", NodeId(0), vec![], true, secs(1), &g);
        let fee = 7u128;
        node.enqueue_tx(sign_dummy(SigilTx::Send {
            from: wallet(1), to: wallet(2), amount: 100, token: NATIVE, fee,
        }));

        let before = Boundary::of_node(&node);
        let block = node.produce_one().expect("block produced");
        let after = Boundary::of_node(&node);

        // Read flux from the block alone — the burn is invisible.
        let from_block_only = DeclaredFlux::from_block(&block);
        assert_eq!(from_block_only.burned, 0, "block declares no burn");
        let verdict = check_crossing(&before, &after, from_block_only);
        assert_eq!(
            verdict,
            Verdict::UndeclaredFlux {
                actual: BLOCK_REWARD as i128 - fee as i128,
                declared: BLOCK_REWARD as i128,
                excess: -(fee as i128),
            },
            "a block-only verifier cannot close the integral; it sees the fee as a leak"
        );

        // Supplying the burn out-of-band closes it.
        assert!(check_crossing(&before, &after, from_block_only.with_burn(fee)).is_conserved());
    }

    /// **The lane's headline.** Run `flux_chronos::tourbillon` over the REAL
    /// `SigilSimNode` — every permutation of the injection order, byte-diffed.
    ///
    /// This is the discrete form of Penrose's argument: `M` must not depend on
    /// which cross-section you evaluate it over. Here: the committed state must
    /// not depend on the order the txs arrived in. `divergence_pairs` is the set
    /// of Hawking points.
    ///
    /// Only meaningful because `SigilSimNode::snapshot()` now commits the four
    /// state roots + native supply. Under the old counters-only snapshot this
    /// test would pass on a genuinely diverged chain.
    #[test]
    fn tourbillon_finds_no_ordering_divergence() {
        let g = demo_genesis();
        // 4 txs → 4! = 24 permutations, all commutative (every wallet is funded
        // far beyond the amounts, so no order can overdraft).
        let injections: Vec<Injection> = (0..4u32)
            .map(|n| {
                let mut payload = vec![TAG_TX];
                payload.extend_from_slice(&serde_json::to_vec(&free_send(n)).unwrap());
                Injection { target: NodeId(0), payload }
            })
            .collect();

        let report = tourbillon::run(
            ScenarioSeed::from(24263),
            &injections,
            secs(60),
            None,
            |seed| {
                let mut u = Universe::new(seed);
                let producer = Box::new(SigilSimNode::new(
                    "producer", NodeId(0), vec![NodeId(1)], true, secs(1), &g,
                ));
                let follower = Box::new(SigilSimNode::new(
                    "follower", NodeId(1), vec![], false, secs(1), &g,
                ));
                let p = u.spawn_node(producer);
                let f = u.spawn_node(follower);
                u.connect(p, f, NetEdge { latency_micros: 50_000, ..Default::default() });
                u
            },
        );

        assert_eq!(report.outcomes.len(), 24, "all 4! orders must run");

        // Guard against the false-green this test was almost written into: prove
        // the snapshot actually commits to state, not just progress counters.
        let snap0 = &report.outcomes[0].snapshots[&NodeId(0)];
        assert_eq!(snap0.len(), crate::snap::LEN, "snapshot layout drifted from `snap`");

        // MEASURED 2026-07-29: `report.converged` is FALSE — all C(24,2)=276
        // pairs differ. That is NOT a consensus fault, and asserting on it would
        // have been the wrong test. Whole-snapshot equality is too strong for a
        // mempool-reordering sweep: `parent_hash` encodes WHICH tx landed in
        // WHICH block, so the hash chain must differ when the order differs.
        //
        // The falsifiable claim is narrower and stronger: divergence is confined
        // to the hash chain and never reaches settlement. If it ever leaks into
        // the wallet/dex/contract roots or the supply, this fails and names the
        // field. (`divergence_profile_is_measured_not_assumed` pins that
        // `parent_hash` is in fact the ONLY field that moves.)
        for (a, b) in &report.divergence_pairs {
            let sa = &report.outcomes[*a].snapshots[&NodeId(0)];
            let sb = &report.outcomes[*b].snapshots[&NodeId(0)];
            for (field, range) in [
                ("height", crate::snap::HEIGHT),
                ("blocks_applied", crate::snap::BLOCKS_APPLIED),
                ("divergence_count", crate::snap::DIVERGENCE_COUNT),
                ("wallet_root", crate::snap::WALLET_ROOT),
                ("dex_root", crate::snap::DEX_ROOT),
                ("contract_root", crate::snap::CONTRACT_ROOT),
                ("native_supply", crate::snap::NATIVE_SUPPLY),
            ] {
                assert_eq!(
                    sa[range.clone()], sb[range],
                    "perm {a} vs {b}: `{field}` is order-DEPENDENT — a real Hawking point"
                );
            }
        }

        // The settlement projection — who is owed what — must be byte-identical
        // across every one of the 24 orders. This is the discrete form of
        // "M must not depend on which cross-section you evaluate it over."
        let views: Vec<_> = report
            .outcomes
            .iter()
            .map(|o| settlement_view(&o.snapshots[&NodeId(0)]).expect("well-formed snapshot"))
            .collect();
        assert!(
            views.windows(2).all(|w| w[0] == w[1]),
            "settlement diverges under reordering — Hawking points found: {views:?}"
        );

        // And the follower must have converged to the same settlement as the producer.
        let f_views: Vec<_> = report
            .outcomes
            .iter()
            .map(|o| settlement_view(&o.snapshots[&NodeId(1)]).expect("well-formed snapshot"))
            .collect();
        assert!(
            f_views.windows(2).all(|w| w[0] == w[1]),
            "follower settlement diverges under reordering: {f_views:?}"
        );
    }

    /// Measures WHICH snapshot fields actually move under reordering, rather
    /// than only proving the settlement ones don't. The sibling test establishes
    /// *containment* (divergence ⊆ {parent_hash, event_log_root}); this one
    /// pins the exact profile so the docs can't drift from reality.
    ///
    /// MEASURED 2026-07-29 — printed by `--nocapture`.
    #[test]
    fn divergence_profile_is_measured_not_assumed() {
        let g = demo_genesis();
        let mut snaps = Vec::new();
        for order in [[0u32, 1, 2, 3], [3, 2, 1, 0], [1, 3, 0, 2], [2, 0, 3, 1]] {
            let mut node = SigilSimNode::new("p", NodeId(0), vec![], true, secs(1), &g);
            for n in order {
                node.enqueue_tx(free_send(n));
            }
            for _ in 0..4 {
                node.produce_one().expect("block");
            }
            snaps.push(SimNode::snapshot(&node));
        }

        let fields: [(&str, std::ops::Range<usize>); 9] = [
            ("height", crate::snap::HEIGHT),
            ("parent_hash", crate::snap::PARENT_HASH),
            ("blocks_applied", crate::snap::BLOCKS_APPLIED),
            ("divergence_count", crate::snap::DIVERGENCE_COUNT),
            ("wallet_root", crate::snap::WALLET_ROOT),
            ("dex_root", crate::snap::DEX_ROOT),
            ("event_log_root", crate::snap::EVENT_ROOT),
            ("contract_root", crate::snap::CONTRACT_ROOT),
            ("native_supply", crate::snap::NATIVE_SUPPLY),
        ];
        let mut moved = Vec::new();
        for (name, range) in fields {
            let differs = snaps
                .windows(2)
                .any(|w| w[0][range.clone()] != w[1][range.clone()]);
            println!(
                "  {name:<18} {:>3} bytes  {}",
                range.len(),
                if differs { "MOVES with order" } else { "invariant" }
            );
            if differs {
                moved.push(name);
            }
        }
        println!("  => order-dependent fields: {moved:?}");

        assert_eq!(
            moved,
            vec!["parent_hash"],
            "divergence profile changed — update the module docs to match"
        );
    }

    /// Order-invariance of the boundary integral itself, checked directly rather
    /// than through snapshot bytes: drive a cloned node through several orders
    /// and require the same final `Boundary`.
    #[test]
    fn boundary_integral_is_order_invariant() {
        let g = demo_genesis();
        let orders: [[u32; 4]; 3] = [[0, 1, 2, 3], [3, 2, 1, 0], [2, 0, 3, 1]];
        let mut finals = Vec::new();
        for order in orders {
            let mut node = SigilSimNode::new("p", NodeId(0), vec![], true, secs(1), &g);
            for n in order {
                node.enqueue_tx(free_send(n));
            }
            let verdicts = audit_block_by_block(&mut node, 4, 0);
            assert!(verdicts.iter().all(|v| v.is_conserved()), "{order:?} leaked: {verdicts:?}");
            finals.push(Boundary::of_node(&node));
        }
        assert!(
            finals.windows(2).all(|w| w[0] == w[1]),
            "boundary integral depends on tx order — it must not: {finals:?}"
        );
    }
}
