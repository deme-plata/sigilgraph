//! Producer-mode block minting — Phase 2 (see `producer/mod.rs`).
//!
//! 2026-08-23 (grogu-producer-unification): `build_genesis()` moved out of
//! `sigil-node/src/main.rs` into `sigil-node/src/genesis.rs` (dual-declared in both
//! `main.rs` and `lib.rs`, same pattern as `block`/`coinbase`/`chain_log`) and is
//! re-exported here — zero duplication, same reasoning as `super::coinbase`.
//! Verified: `genesis::tests::build_genesis_is_deterministic` passes in sigil-node's
//! own test suite (two independent calls produce a byte-identical block 0), and the
//! full sigil-node genesis test group (`cant_apply_two_genesis_blocks`,
//! `genesis_passes_precheck`, `genesis_sets_master_wallet`,
//! `genesis_roots_match_after_apply`) still passes after the move — this is the same
//! function the live producer mints from, not a hand-ported copy.
//!
//! `mint_next_block()` — moved out of `sigil-node/src/main.rs`'s `run_start()` event
//! loop into `sigil-node/src/mint.rs` and re-exported here, same reasoning. It always
//! took its state as explicit parameters (chain, merge_parents, txs, reward_override,
//! solve, topology_commitment) rather than closing over the event loop's locals, so
//! the move needed zero behavioral change. It still needs `dag`'s merge_parents/braid
//! wiring supplied by the CALLER to mean anything (a block can't mint without knowing
//! its DAG parents) — that caller-side wiring (the actual producer loop calling
//! `dag::dag_seed_braid` → `dag::dag_build_frontier` → `mint_next_block` →
//! `dag::dag_drain_apply` in sequence, on a real running sigil-top instance) is the
//! remaining Phase 3/5 work, gated behind the runtime flags in `producer/mod.rs`.

pub use sigil_node::genesis::{
    build_genesis, DEMO_INITIAL_BALANCE, DEMO_WALLET, GENESIS_AI_ENDOWMENT, GENESIS_AI_WALLETS,
    GENESIS_TIMESTAMP_MS, MASTER_WALLET_GENESIS,
};
pub use sigil_node::mint::mint_next_block;

#[cfg(test)]
mod tests {
    use super::*;

    /// The seam this module exists to prove: sigil-top's producer feature mints
    /// EXACTLY the network's real genesis, not a drifted copy. If sigil-node's
    /// genesis.rs ever changes, this re-export picks it up automatically — there is
    /// no second definition here to fall out of sync.
    #[test]
    fn producer_genesis_matches_sigil_node_genesis() {
        let a = build_genesis().expect("sigil-top producer genesis");
        let b = sigil_node::genesis::build_genesis().expect("sigil-node genesis");
        assert_eq!(a.hash(), b.hash(), "sigil-top must mint the SAME genesis sigil-node mints");
    }

    /// End-to-end proof this crate can drive the REAL mint path: genesis →
    /// mint_next_block(height 1, no DAG/no solve — the free-running empty-block
    /// dyno path) → chain.apply() through the full precheck/commit/root-match
    /// chokepoint. If sigil-top's `producer` feature can mint one block this way,
    /// it can mint any — this is the seam `dag`'s braid functions plug into once
    /// the caller-side producer loop (Phase 3/5) is wired.
    #[test]
    fn producer_can_mint_block_one_on_top_of_genesis() {
        let mut chain = sigil_node::chain::ChainTip::new();
        let genesis = build_genesis().expect("genesis");
        chain.apply(genesis).expect("apply genesis");
        assert_eq!(chain.height(), 1, "chain advances to height 1 after applying block 0");

        let (block, included) = mint_next_block(
            &chain,
            vec![],   // no DAG merge parents — linear mode
            &[],      // no user txs
            None,     // reward: height schedule default
            None,     // no verified mining solve — free-running dyno path
            None,     // no topology commitment — linear mode
            // 7th arg (added to `mint_next_block` 2026-08-26, Option C): the partial-share
            // pool, drained only for a self-minted block. `None` here — this test mints on
            // the free-running dyno path with no miner and no shares, so there is nothing
            // to pay out. This test compiled against the 6-arg signature until the
            // `producer` feature became default-on and the test build started covering it.
            None,
        ).expect("mint block 1");
        assert_eq!(block.header.height, 1);
        assert_eq!(block.header.parent_hash, chain.parent_hash(), "chains onto genesis");
        assert!(included.is_empty(), "no txs submitted, none included");

        chain.apply(block).expect("apply block 1 through the real chokepoint");
        assert_eq!(chain.height(), 2, "chain advances to height 2 after applying block 1");
    }
}
