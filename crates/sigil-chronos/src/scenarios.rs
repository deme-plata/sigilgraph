//! CHRONOS-F — curated adversarial scenario library.
//!
//! A named catalogue of attacks staged against the REAL SIGIL chain through
//! flux-chronos, each asserting the safety outcome SIGIL must guarantee.
//! Where the property fuzzer (`property`) throws random chaos and the
//! multiverse (`multiverse`) forks labelled timelines, this module pins the
//! *specific, known attacks* a chain must survive — the ones you'd put in a
//! security review checklist:
//!
//! | scenario | attack | required outcome |
//! |---|---|---|
//! | `double_spend` | wallet spends more than it holds across 2 txs | 2nd tx dropped, no money created |
//! | `byzantine_tampered_block` | producer commits a block whose roots lie | follower flags Divergence (exit-78) |
//! | `tamper_each_of_four_roots` | corrupt each of the 4 state roots in turn | all 4 → Divergence (every root checked) |
//! | `replay_attack` | re-submit an already-applied block | 2nd application Rejected |
//! | `equivocation` | two different blocks at the same height | follower keeps one, Rejects the other |
//! | `partition_safety` | total network partition | follower falls behind, never diverges |
//! | `self_send_does_not_mint` | wallet sends `amount` to ITSELF | balance unchanged, no `amount` minted |
//! | `transfers_conserve_supply` | many transfers among wallets | Σ demo balances never grows (no inflation) |
//! | `stale_tip_cannot_roll_back` | replay a far-older block at a lower height | Rejected, tip never moves backward |
//!
//! Every scenario returns a [`ScenarioOutcome`]; [`run_library`] runs the
//! whole catalogue. The `tests` module asserts each passes — so this file is
//! simultaneously the library AND the regression suite for SIGIL's safety
//! properties.

use flux_chronos::{secs, NetEdge, NodeId, ScenarioSeed, Universe};

use crate::{demo_genesis, sign_dummy, ApplyOutcome, SigilSimNode};
use sigil_tx::{SigilTx, NATIVE};

/// Outcome of running one scenario.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScenarioOutcome {
    /// Scenario name.
    pub name: &'static str,
    /// Did the chain behave safely (i.e. the attack was caught / contained)?
    pub passed: bool,
    /// Human-readable detail of what was observed.
    pub detail: String,
}

impl ScenarioOutcome {
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self { name, passed: true, detail: detail.into() }
    }
    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self { name, passed: false, detail: detail.into() }
    }
}

fn w(t: u8) -> [u8; 32] {
    [t; 32]
}

/// A fresh producer pre-loaded with one Send, ready to `produce_one`.
fn producer_with_send(from: u8, to: u8, amount: u128) -> SigilSimNode {
    let g = demo_genesis();
    let mut n = SigilSimNode::new("producer", NodeId(0), vec![NodeId(1)], true, secs(1), &g);
    n.enqueue_tx(sign_dummy(SigilTx::Send {
        from: w(from),
        to: w(to),
        amount,
        token: NATIVE,
        fee: 1,
    }));
    n
}

/// A fresh follower at genesis.
fn fresh_follower() -> SigilSimNode {
    let g = demo_genesis();
    SigilSimNode::new("follower", NodeId(1), vec![], false, secs(1), &g)
}

// ── Scenarios ────────────────────────────────────────────────────────────

/// A wallet tries to spend more than it holds across two transactions. The
/// second must fail (insufficient balance) and produce no block — and total
/// supply must not increase (no money minted from thin air).
pub fn double_spend() -> ScenarioOutcome {
    const NAME: &str = "double_spend";
    let g = demo_genesis();
    let mut n = SigilSimNode::new("producer", NodeId(0), vec![NodeId(1)], true, secs(1), &g);
    // Wallet 1 holds 1_000_000. Two sends of 700_000 each → total 1_400_000.
    for _ in 0..2 {
        n.enqueue_tx(sign_dummy(SigilTx::Send {
            from: w(1), to: w(2), amount: 700_000, token: NATIVE, fee: 1,
        }));
    }
    // Step twice; only the first should mint a block.
    let b1 = n.produce_one();
    let b2 = n.produce_one();

    let supply: u128 = (1..=5u8).map(|i| n.balance_of(&w(i), &NATIVE)).sum();
    let initial = 5 * 1_000_000u128;

    if b1.is_some() && b2.is_none() && supply <= initial {
        ScenarioOutcome::pass("double_spend",
            format!("1st send applied, 2nd dropped (overspend); supply {supply} ≤ {initial} — no money created"))
    } else {
        ScenarioOutcome::fail("double_spend",
            format!("b1={:?} b2={:?} supply={} (expected b1=Some, b2=None, supply≤{})",
                b1.is_some(), b2.is_some(), supply, initial))
    }
    .with_name(NAME)
}

/// A producer commits a block whose header roots don't match the transition
/// it carries. The follower recomputes roots locally and MUST flag
/// Divergence — the exit-78 invariant.
pub fn byzantine_tampered_block() -> ScenarioOutcome {
    let mut producer = producer_with_send(1, 2, 100);
    let mut block = producer.produce_one().expect("producer mints block 1");
    // Tamper: claim a wallet root the transition didn't produce.
    block.header.wallet_state_root = [0xFFu8; 32];

    let mut follower = fresh_follower();
    let outcome = follower.apply_external_block(&block);
    if outcome == ApplyOutcome::Divergence {
        ScenarioOutcome::pass("byzantine_tampered_block",
            "follower recomputed roots, caught the lie, flagged Divergence (exit-78)")
    } else {
        ScenarioOutcome::fail("byzantine_tampered_block",
            format!("expected Divergence, got {outcome:?}"))
    }
}

/// Corrupt each of the four state roots independently — every one must be
/// caught. Proves the follower checks all four, not just the wallet root.
pub fn tamper_each_of_four_roots() -> ScenarioOutcome {
    let mut all_caught = true;
    let mut detail = Vec::new();
    for which in ["wallet", "dex", "event", "contract"] {
        let mut producer = producer_with_send(1, 2, 100);
        let mut block = producer.produce_one().expect("mint");
        match which {
            "wallet" => block.header.wallet_state_root = [0x11; 32],
            "dex" => block.header.dex_state_root = [0x22; 32],
            "event" => block.header.event_log_root = [0x33; 32],
            "contract" => block.header.contract_state_root = [0x44; 32],
            _ => unreachable!(),
        }
        let mut follower = fresh_follower();
        let outcome = follower.apply_external_block(&block);
        let caught = outcome == ApplyOutcome::Divergence;
        all_caught &= caught;
        detail.push(format!("{which}:{}", if caught { "caught" } else { "MISSED" }));
    }
    if all_caught {
        ScenarioOutcome::pass("tamper_each_of_four_roots",
            format!("all four roots checked — {}", detail.join(" ")))
    } else {
        ScenarioOutcome::fail("tamper_each_of_four_roots",
            format!("a root went unchecked — {}", detail.join(" ")))
    }
}

/// Re-submit an already-applied block. The first application succeeds; the
/// replay must be Rejected (the follower already advanced past that height).
pub fn replay_attack() -> ScenarioOutcome {
    let mut producer = producer_with_send(1, 2, 100);
    let block = producer.produce_one().expect("mint block 1");
    let mut follower = fresh_follower();
    let first = follower.apply_external_block(&block);
    let second = follower.apply_external_block(&block); // replay
    if first == ApplyOutcome::Ok && second == ApplyOutcome::Rejected {
        ScenarioOutcome::pass("replay_attack",
            "first apply Ok; replay of the same block Rejected (height already advanced)")
    } else {
        ScenarioOutcome::fail("replay_attack",
            format!("first={first:?} second={second:?} (expected Ok then Rejected)"))
    }
}

/// A producer equivocates: two DIFFERENT blocks at the same height. The
/// follower accepts the first it sees and Rejects the conflicting one (it
/// has already advanced past that height).
pub fn equivocation() -> ScenarioOutcome {
    // Two producers from identical genesis mint different block-1s (different
    // tx → different roots → different hash, same height 1).
    let mut p_a = producer_with_send(1, 2, 100);
    let mut p_b = producer_with_send(3, 4, 250); // different tx
    let block_a = p_a.produce_one().expect("A mints");
    let block_b = p_b.produce_one().expect("B mints");

    if block_a.hash() == block_b.hash() {
        return ScenarioOutcome::fail("equivocation", "the two blocks were identical — not an equivocation");
    }

    let mut follower = fresh_follower();
    let first = follower.apply_external_block(&block_a);
    let second = follower.apply_external_block(&block_b); // conflicting, same height
    if first == ApplyOutcome::Ok && second == ApplyOutcome::Rejected {
        ScenarioOutcome::pass("equivocation",
            "follower committed block A, Rejected the conflicting block B at the same height")
    } else {
        ScenarioOutcome::fail("equivocation",
            format!("first={first:?} second={second:?} (expected Ok then Rejected)"))
    }
}

/// Total network partition: the producer keeps minting but nothing reaches
/// the follower. The follower must fall behind (liveness loss) while NEVER
/// diverging (safety intact). Run over the real in-memory mesh with a
/// partitioned edge.
pub fn partition_safety() -> ScenarioOutcome {
    let g = demo_genesis();
    let mut u = Universe::new(ScenarioSeed::from(1));
    let d = u.spawn_node(Box::new(SigilSimNode::new("delta", NodeId(0), vec![NodeId(1)], true, secs(1), &g)));
    let e = u.spawn_node(Box::new(SigilSimNode::new("epsilon", NodeId(1), vec![], false, secs(1), &g)));
    u.connect(d, e, NetEdge { latency_micros: 50_000, drop_prob: 0.0, partitioned: true });

    for n in 0..20u32 {
        let tx = sign_dummy(SigilTx::Send {
            from: w((n % 5 + 1) as u8), to: w(((n + 1) % 5 + 1) as u8),
            amount: 10, token: NATIVE, fee: 1,
        });
        let mut p = vec![0u8]; // TAG_TX
        p.extend_from_slice(&serde_json::to_vec(&tx).unwrap());
        u.inject(d, p);
    }
    u.advance(secs(60));

    let log = u.event_log();
    let produced = log.iter().filter(|(_, _, s)| s.contains("produced H=")).count();
    let applied = log.iter().filter(|(_, _, s)| s.contains("apply H=") && s.contains("Ok")).count();
    let diverged = log.iter().filter(|(_, _, s)| s.contains("Divergence")).count();

    if diverged == 0 && applied == 0 && produced > 0 {
        ScenarioOutcome::pass("partition_safety",
            format!("producer minted {produced} blocks; follower applied 0 (partitioned) but 0 divergence — safety held, liveness lost"))
    } else {
        ScenarioOutcome::fail("partition_safety",
            format!("produced={produced} applied={applied} diverged={diverged} (expected applied=0, diverged=0)"))
    }
}

/// UPGRADE-COMPAT: a node running the CURRENT rules receives a block built
/// under NEWER consensus (a bumped header `version`). It must SAFELY reject —
/// never silently apply a block from rules it doesn't understand. This is the
/// guarantee that a consensus/block-format change can't corrupt nodes that
/// haven't upgraded: they halt-reject, they don't mis-sync.
pub fn old_node_rejects_newer_block() -> ScenarioOutcome {
    let mut producer = producer_with_send(1, 2, 100);
    let mut block = producer.produce_one().expect("mint block 1");
    // Simulate a future consensus version the old node doesn't know.
    block.header.version = sigil_header::HEADER_VERSION + 1;

    let mut follower = fresh_follower();
    let outcome = follower.apply_external_block(&block);
    if outcome == ApplyOutcome::Rejected {
        ScenarioOutcome::pass("old_node_rejects_newer_block",
            "old-rules node rejected a newer-version block (precheck) — no silent mis-apply across an upgrade")
    } else {
        ScenarioOutcome::fail("old_node_rejects_newer_block",
            format!("expected Rejected, got {outcome:?} — old node accepted a newer-consensus block!"))
    }
}

/// UPGRADE-COMPAT / replay-protection: a block stamped with a DIFFERENT
/// network_id (e.g. a cross-network replay, or a fork's blocks) must be
/// rejected. Proves the network_id in the header is load-bearing — blocks
/// from another SIGIL network (or Quillon) can never be applied here.
pub fn wrong_network_block_rejected() -> ScenarioOutcome {
    let mut producer = producer_with_send(1, 2, 100);
    let mut block = producer.produce_one().expect("mint block 1");
    block.header.network_id = *b"OTHERNET"; // not sigil-g0

    let mut follower = fresh_follower();
    let outcome = follower.apply_external_block(&block);
    if outcome == ApplyOutcome::Rejected {
        ScenarioOutcome::pass("wrong_network_block_rejected",
            "block from a different network_id rejected — cross-network replay impossible")
    } else {
        ScenarioOutcome::fail("wrong_network_block_rejected",
            format!("expected Rejected, got {outcome:?} — accepted a foreign-network block!"))
    }
}

/// A wallet sends `amount` to ITSELF. The aliasing-safe Send handler must NOT
/// mint: with naive debit-then-credit, the recipient credit (reading pre-state)
/// overwrites the sender debit on the same (wallet, NATIVE) slot via
/// last-writer-wins, leaving `balance = before + amount` — `amount` minted from
/// nothing. This drives a self-send through the REAL produce → chokepoint path
/// and asserts the balance is unchanged (fee 0 → exactly equal). Regression for
/// the documented self-transfer anti-mint guard in `sigil-tx::apply_tx`.
pub fn self_send_does_not_mint() -> ScenarioOutcome {
    const NAME: &str = "self_send_does_not_mint";
    let g = demo_genesis();
    let mut n = SigilSimNode::new("producer", NodeId(0), vec![NodeId(1)], true, secs(1), &g);
    let before = n.balance_of(&w(1), &NATIVE);
    // Wallet 1 sends half its holdings to ITSELF, fee 0 → net change must be 0.
    n.enqueue_tx(sign_dummy(SigilTx::Send {
        from: w(1), to: w(1), amount: before / 2, token: NATIVE, fee: 0,
    }));
    let block = n.produce_one();
    let after = n.balance_of(&w(1), &NATIVE);
    if block.is_some() && after == before {
        ScenarioOutcome::pass(NAME,
            format!("self-send of {} left balance at {after} (== {before}) — no money minted", before / 2))
    } else {
        ScenarioOutcome::fail(NAME,
            format!("self-send minted: before={before} after={after} block={:?} (expected unchanged)", block.is_some()))
    }
}

/// Many ordinary transfers among the demo wallets. With fee 0 the total native
/// holdings of the demo wallets MUST be exactly conserved — a transfer moves
/// value, it never creates or destroys it. Coinbase mints to DISJOINT wallets
/// (`PRODUCER_WALLET` et al.), so the demo-wallet sum is a clean conservation
/// gauge: it may never rise (that would be inflation into user wallets) and at
/// fee 0 may never fall either. Drives every send through the real chokepoint.
pub fn transfers_conserve_supply() -> ScenarioOutcome {
    const NAME: &str = "transfers_conserve_supply";
    let g = demo_genesis();
    let mut n = SigilSimNode::new("producer", NodeId(0), vec![NodeId(1)], true, secs(1), &g);
    let demo_sum = |n: &SigilSimNode| -> u128 { (1..=5u8).map(|i| n.balance_of(&w(i), &NATIVE)).sum() };
    let before = demo_sum(&n);
    let start_height = n.height();

    // 12 transfers in a ring, fee 0, amounts that always fit (max held is the
    // genesis 1_000_000 each). Each produces one block (one tx per block).
    let mut produced = 0u64;
    for k in 0..12u32 {
        let from = (k % 5 + 1) as u8;
        let to = ((k + 2) % 5 + 1) as u8; // never equal to `from`
        n.enqueue_tx(sign_dummy(SigilTx::Send {
            from: w(from), to: w(to), amount: 1_000, token: NATIVE, fee: 0,
        }));
        if n.produce_one().is_some() {
            produced += 1;
        }
    }

    let after = demo_sum(&n);
    let advanced = n.height() - start_height;
    if after == before && produced == 12 && advanced == 12 {
        ScenarioOutcome::pass(NAME,
            format!("12 transfers, demo-supply conserved at {after} (== {before}); {advanced} coinbase blocks minted to disjoint wallets"))
    } else {
        ScenarioOutcome::fail(NAME,
            format!("conservation broken: before={before} after={after} produced={produced} advanced={advanced} (expected equal, 12, 12)"))
    }
}

/// SYNC-DOWN PROTECTION (Inv 3, SIGIL granularity). The catastrophic-loss rule
/// is "never let a peer announcing a LOWER tip move you backward / truncate the
/// chain." SIGIL is structurally immune: the chain log is append-only and
/// `apply_block` accepts ONLY a block at exactly `next_height`. This scenario
/// advances a follower several blocks, then feeds it a far-older block at a much
/// lower height — it must be Rejected AND leave the tip and applied-count
/// untouched (no rollback, no truncation). The explicit "height unchanged after
/// rejection" assertion is what distinguishes this from `replay_attack`.
pub fn stale_tip_cannot_roll_back() -> ScenarioOutcome {
    const NAME: &str = "stale_tip_cannot_roll_back";
    // Producer mints three blocks; follower applies all three (tip → height 4).
    let mut producer = producer_with_send(1, 2, 100);
    let b1 = producer.produce_one().expect("mint block 1");
    // Keep producing so the producer has more in flight; re-seed mempool each time.
    producer.enqueue_tx(sign_dummy(SigilTx::Send { from: w(1), to: w(3), amount: 100, token: NATIVE, fee: 0 }));
    let b2 = producer.produce_one().expect("mint block 2");
    producer.enqueue_tx(sign_dummy(SigilTx::Send { from: w(1), to: w(4), amount: 100, token: NATIVE, fee: 0 }));
    let b3 = producer.produce_one().expect("mint block 3");

    let mut follower = fresh_follower();
    let o1 = follower.apply_external_block(&b1);
    let o2 = follower.apply_external_block(&b2);
    let o3 = follower.apply_external_block(&b3);
    let tip_before = follower.height();
    let applied_before = follower.blocks_applied;

    // Now a peer serves the OLD block 1 (height 1) while we sit at height 4 — a
    // sync-down attempt. Must be rejected with zero state movement.
    let stale = follower.apply_external_block(&b1);
    let tip_after = follower.height();
    let applied_after = follower.blocks_applied;

    let advanced_ok = o1 == ApplyOutcome::Ok && o2 == ApplyOutcome::Ok && o3 == ApplyOutcome::Ok;
    if advanced_ok
        && stale == ApplyOutcome::Rejected
        && tip_after == tip_before
        && applied_after == applied_before
    {
        ScenarioOutcome::pass(NAME,
            format!("tip held at {tip_after} after a stale height-1 block was served at tip {tip_before} — Rejected, no rollback"))
    } else {
        ScenarioOutcome::fail(NAME,
            format!("sync-down not contained: o1/2/3={o1:?}/{o2:?}/{o3:?} stale={stale:?} tip {tip_before}->{tip_after} applied {applied_before}->{applied_after}"))
    }
}

/// Run the entire scenario library. Returns every outcome.
pub fn run_library() -> Vec<ScenarioOutcome> {
    vec![
        double_spend(),
        byzantine_tampered_block(),
        tamper_each_of_four_roots(),
        replay_attack(),
        equivocation(),
        partition_safety(),
        old_node_rejects_newer_block(),
        wrong_network_block_rejected(),
        self_send_does_not_mint(),
        transfers_conserve_supply(),
        stale_tip_cannot_roll_back(),
    ]
}

// tiny helper to override the name field on the double_spend builder above
trait WithName {
    fn with_name(self, name: &'static str) -> Self;
}
impl WithName for ScenarioOutcome {
    fn with_name(mut self, name: &'static str) -> Self {
        self.name = name;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_spend_is_contained() {
        assert!(double_spend().passed);
    }

    #[test]
    fn byzantine_block_is_caught() {
        assert!(byzantine_tampered_block().passed);
    }

    #[test]
    fn all_four_roots_are_checked() {
        assert!(tamper_each_of_four_roots().passed);
    }

    #[test]
    fn replay_is_rejected() {
        assert!(replay_attack().passed);
    }

    #[test]
    fn equivocation_is_rejected() {
        assert!(equivocation().passed);
    }

    #[test]
    fn partition_keeps_safety() {
        assert!(partition_safety().passed);
    }

    #[test]
    fn old_node_rejects_newer_consensus() {
        assert!(old_node_rejects_newer_block().passed);
    }

    #[test]
    fn foreign_network_block_rejected() {
        assert!(wrong_network_block_rejected().passed);
    }

    #[test]
    fn self_send_mints_nothing() {
        let o = self_send_does_not_mint();
        assert!(o.passed, "{}", o.detail);
    }

    #[test]
    fn transfers_are_conservative() {
        let o = transfers_conserve_supply();
        assert!(o.passed, "{}", o.detail);
    }

    #[test]
    fn sync_down_is_contained() {
        let o = stale_tip_cannot_roll_back();
        assert!(o.passed, "{}", o.detail);
    }

    #[test]
    fn whole_library_passes() {
        let results = run_library();
        let failed: Vec<_> = results.iter().filter(|r| !r.passed).collect();
        assert!(
            failed.is_empty(),
            "scenario(s) failed: {:?}",
            failed.iter().map(|r| (r.name, &r.detail)).collect::<Vec<_>>()
        );
        assert_eq!(results.len(), 11, "library should have 11 scenarios");
    }
}
