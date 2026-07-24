//! ONE-CHAIN step 1 — money enters the braid.
//!
//! The braid producer historically minted EMPTY blocks (a throughput dyno).
//! To make the DAGKnight braid THE money chain, every minted block now carries
//! a **coinbase**: the producer credits itself the block reward through the ONE
//! money chokepoint (`commit_state_transition` — same 21M cap + conservation as
//! the rpcd path). The reward mutation lives in the block body, so every
//! follower re-applies the identical mutation and `ChainTip::apply`'s
//! root-match check passes — deterministic, no special-casing.
//!
//! Reward: block-based `sigil_emission::block_reward(height)` — a pure function
//! of height, so all nodes agree with zero shared clock state. (Time-based
//! wall-clock emission, as rpcd uses, is a follow-on before the live launch;
//! block-based is correct + deterministic for the dev proof and can't drift.)

use sigil_header::BlockHash;
use sigil_state::{SigilState, StateMutation, StateRoots, StateTransition, TokenId, WalletId};

/// Native SIGIL token id (matches sigil_state::NATIVE).
const NATIVE: TokenId = [0u8; 32];

/// Resolve the producer's payout wallet: `SIGIL_PRODUCER_WALLET` (64-hex) or,
/// unset, a deterministic dev wallet so a fresh node still mints coherently.
pub fn producer_wallet() -> WalletId {
    if let Ok(h) = std::env::var("SIGIL_PRODUCER_WALLET") {
        if let Some(w) = hex64(h.trim()) {
            return w;
        }
    }
    // Deterministic dev default (all-0xC1 — visibly "coinbase").
    [0xC1; 32]
}

fn hex64(s: &str) -> Option<WalletId> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Build the coinbase transition for a block at `height`: credit `producer`
/// with `block_reward(height)` on top of its current balance. Empty (no-op) at
/// heights where the reward is zero (post-tail), so late blocks stay valid.
pub fn coinbase_transition(state: &SigilState, height: u64, producer: WalletId) -> StateTransition {
    let reward = sigil_emission::block_reward(height);
    let mut mutations = Vec::new();
    if reward > 0 {
        let bal = state.balance_of(&producer, &NATIVE);
        mutations.push(StateMutation::SetBalance {
            wallet: producer,
            token: NATIVE,
            amount: bal.saturating_add(reward),
        });
    }
    StateTransition { at_height: height, mutations }
}

/// Compute the four state roots AFTER applying `transition` to a CLONE of the
/// live state — without mutating it. The producer stamps these into the header;
/// the real `ChainTip::apply` then re-applies to the true state and asserts the
/// roots match (verify-don't-trust holds across the coinbase).
pub fn roots_after(state: &SigilState, transition: &StateTransition, height: u64) -> StateRoots {
    let mut clone = state.clone();
    // A well-formed coinbase never trips the cap on a dev chain; if it ever did,
    // the producer must not mint that block — surface by returning parent roots
    // so apply() fails root-match loudly rather than minting an invalid block.
    match sigil_state::commit_state_transition(&mut clone, transition, height) {
        Ok(roots) => roots,
        Err(_) => state.roots(),
    }
}

/// Convenience: (transition, post-apply roots) for the current tip.
pub fn coinbase_for(
    state: &SigilState,
    height: u64,
    _parent: BlockHash,
) -> (StateTransition, StateRoots) {
    let producer = producer_wallet();
    let tr = coinbase_transition(state, height, producer);
    let roots = roots_after(state, &tr, height);
    (tr, roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coinbase_credits_reward_and_moves_root() {
        let mut st = SigilState::new();
        let root0 = st.roots().wallet_state_root;
        let prod = [0xC1; 32];
        let (tr, roots) = coinbase_for(&st, 1, [0u8; 32]);
        assert_eq!(tr.mutations.len(), 1, "height 1 pays a reward");
        // roots_after must equal a real apply
        let applied = sigil_state::commit_state_transition(&mut st, &tr, 1).unwrap();
        assert_eq!(roots.wallet_state_root, applied.wallet_state_root, "predicted roots == applied roots");
        assert_ne!(root0, applied.wallet_state_root, "coinbase moves the wallet root — money entered the graph");
        assert_eq!(st.balance_of(&prod, &NATIVE), sigil_emission::block_reward(1));
        assert_eq!(st.native_supply(), sigil_emission::block_reward(1), "supply grew by exactly the reward");
    }

    #[test]
    fn deterministic_across_nodes() {
        // two independent producers at the same height compute identical roots
        let a = SigilState::new();
        let b = SigilState::new();
        let (_, ra) = coinbase_for(&a, 5, [0u8; 32]);
        let (_, rb) = coinbase_for(&b, 5, [0u8; 32]);
        assert_eq!(ra.wallet_state_root, rb.wallet_state_root);
    }

    /// CHRONOS GATE (the flux-way money-on-braid proof): N independent nodes
    /// replay the SAME ordered sequence of coinbase blocks and must converge to
    /// a byte-identical wallet_state_root + native_supply — divergence == 0.
    /// This is what makes it SAFE to settle money over the braid's finalized
    /// order: the order determines the money, uniquely, on every node.
    #[test]
    fn chronos_coinbase_replay_divergence_zero() {
        const NODES: usize = 4;
        const BLOCKS: u64 = 200;

        // Producer builds the canonical order: coinbase transition per height.
        let mut producer = SigilState::new();
        let mut order: Vec<StateTransition> = Vec::new();
        for h in 1..=BLOCKS {
            let tr = coinbase_transition(&producer, h, producer_wallet());
            // apply on the producer to advance its own state (as a real node does)
            sigil_state::commit_state_transition(&mut producer, &tr, h).unwrap();
            order.push(tr);
        }

        // N followers replay the identical order from genesis.
        let mut roots = Vec::new();
        let mut supplies = Vec::new();
        for _ in 0..NODES {
            let mut node = SigilState::new();
            for (i, tr) in order.iter().enumerate() {
                let h = (i as u64) + 1;
                sigil_state::commit_state_transition(&mut node, tr, h).unwrap();
            }
            roots.push(node.roots().wallet_state_root);
            supplies.push(node.native_supply());
        }

        // Divergence must be zero: every node agrees with the producer + each other.
        let p_root = producer.roots().wallet_state_root;
        let p_supply = producer.native_supply();
        for i in 0..NODES {
            assert_eq!(roots[i], p_root, "node {i} wallet_root diverged from producer");
            assert_eq!(supplies[i], p_supply, "node {i} supply diverged from producer");
        }
        // and the emission is exactly the schedule sum (conservation)
        let expected: u128 = (1..=BLOCKS).map(sigil_emission::block_reward).sum();
        assert_eq!(p_supply, expected, "minted supply == Σ block_reward(h) — no phantom emission");
    }
}
