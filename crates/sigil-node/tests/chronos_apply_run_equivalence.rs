//! 2026-10-22 (speed stage 1, Viktor: "i want at least 10x that"): `ChainTip::apply_run` applies a contiguous run
//! of blocks on ONE state copy instead of one copy per block. This pins that it ends EXACTLY where one-at-a-time
//! `apply` ends — for runs of uneven sizes, and when a block in the middle of a run fails AFTER its transition was
//! committed into the run's copy (the 2026-10-09 M0 shape: a body that does not match its header's roots). The
//! refused block must leave no trace, and the honest block of the same height must still apply after it.

mod common;
use common::*;
use sigil_node::chain::ChainTip;

fn fresh() -> ChainTip {
    let mut c = ChainTip::new();
    c.apply(sigil_node::genesis::build_genesis().expect("genesis")).expect("apply genesis");
    c
}

fn same(a: &ChainTip, b: &ChainTip) -> bool {
    a.height() == b.height()
        && a.parent_hash() == b.parent_hash()
        && a.state().roots() == b.state().roots()
        && a.state().shielded_pool_root() == b.state().shielded_pool_root()
}

#[test]
fn apply_run_equals_one_at_a_time_and_a_refused_block_leaves_no_trace() {
    std::env::set_var("SIGIL_DAG_FINAL_DEPTH", FINAL_DEPTH);
    let mut h = Harness::new(OfferPolicy::Fixed);
    for _ in 0..80 {
        h.tick();
    }
    let blocks = h.applied_blocks.clone();
    assert!(blocks.len() >= 40, "the harness settled only {} blocks", blocks.len());

    let mut seq = fresh();
    for b in &blocks {
        seq.apply(b.clone()).expect("one-at-a-time apply");
    }

    // Runs of uneven sizes, a run of one among them.
    let mut run = fresh();
    let mut i = 0;
    for size in [1usize, 7, 16, 3].iter().cycle() {
        if i >= blocks.len() {
            break;
        }
        let end = (i + size).min(blocks.len());
        let out = run.apply_run(&blocks[i..end]);
        assert!(out.error.is_none(), "run [{i}..{end}) failed: {:?}", out.error.map(|e| e.1.to_string()));
        assert_eq!(out.applied, end - i);
        for (k, b) in blocks[i..end].iter().enumerate() {
            assert_eq!(out.hashes[k], b.hash(), "the returned hash is the block's own");
        }
        i = end;
    }
    assert!(same(&seq, &run), "a run must end exactly where one-at-a-time application ends");

    // A block whose header roots do not match its body: its transition COMMITS into the run's copy, then the root
    // check refuses it.
    let k = blocks.len() / 2;
    let mut bad = blocks.clone();
    bad[k].header.wallet_state_root[0] ^= 0xFF;
    let mut prefix = fresh();
    for b in &blocks[..k] {
        prefix.apply(b.clone()).expect("prefix");
    }
    let mut tried = fresh();
    let out = tried.apply_run(&bad);
    let (at, e) = out.error.expect("the tampered block must be refused");
    assert_eq!(at, k, "refused at the tampered block");
    assert_eq!(out.applied, k, "everything before it applied");
    assert!(e.to_string().contains("STATE DIVERGENCE"), "{e}");
    assert!(same(&prefix, &tried), "the refused block leaves no trace: the state is the good prefix, applied one by one");
    tried.apply(blocks[k].clone()).expect("the honest block of the same height applies after the refusal");
    std::env::remove_var("SIGIL_DAG_FINAL_DEPTH");
}
