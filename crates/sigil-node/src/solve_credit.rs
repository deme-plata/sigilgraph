//! Which queued mining solve (if any) a producer tick credits when it mints its next
//! block. Extracted out of `main.rs`'s event loop (2026-08-25, local-mining-API work)
//! into its own module and dual-declared into `lib.rs` — same pattern already used for
//! `coinbase`/`dag`/`mint`/`genesis`/`snapshot` (see `lib.rs`'s doc comments on each):
//! zero `crate::`-relative deps of its own (only `sigil_api`/`sigil_header`/`flux_vdf`),
//! so exposing it here is safe by the same test those already passed.
//!
//! Why this needed its own module rather than staying main.rs-private: sigil-top's
//! `producer` feature (see its own `producer/run.rs`) mints real blocks too, and its
//! tick loop needs to make the EXACT same "which solve gets credited" decision this
//! node's live producer already makes — this logic was tuned live against a real,
//! reproduced money-loss bug (see the doc comments below), so a hand-ported second copy
//! in sigil-top risked silently drifting from whatever gets fixed here next. Sharing the
//! one real implementation is the same reasoning `mint_next_block`/`dag_drain_apply`
//! already established.

use sigil_header::BlockHash;

/// A near-miss solve (verified against a HISTORICAL frontier, not the current one) still
/// credits the real miner's wallet, but its nonce/blake4_hash/vdf must NOT be embedded in
/// the block header — they were computed against a DIFFERENT parent than the one this
/// block actually carries, and embedding them would make the header's claimed PoW fail
/// re-verification for every follower. Zeroed exactly like the "no solve" path — the
/// block's PoW fields end up identical either way; only WHO gets paid changes.
pub fn near_miss_credit(s: sigil_api::mining::AcceptedSolve) -> sigil_api::mining::AcceptedSolve {
    sigil_api::mining::AcceptedSolve {
        nonce: 0,
        blake4_hash: 0,
        vdf: flux_vdf::VdfProof { y: vec![], pi: vec![], t: 0 },
        ..s
    }
}

/// How many entries `take_creditable_solve` will pop-and-check in a single tick before
/// giving up. See that function's doc for why this exists.
///
/// 2026-08-23 (grogu-mining-balance-instant): raised 64 → 512 (matching `SOLVE_QUEUE_CAP`,
/// so a fully-saturated queue can be entirely drained in one pass). Live-measured on
/// Epsilon: `queued_solves` was PERMANENTLY PINNED at exactly 512 (the queue cap) for the
/// whole observation window while `shares_accepted` climbed at ~45-50/s from 4 live
/// miners — arrival rate vastly exceeded the old 64-per-tick drain rate, so the queue
/// stayed full and every new accepted share silently evicted the oldest still-unpaid one
/// (FIFO overflow). That's a real, ongoing loss of already-verified mining credit, not a
/// display bug — confirmed live by an operator whose real, funded wallet balance never
/// rose despite active mining. Scanning is cheap (no I/O, just VecDeque pops + integer
/// compares), so 512 costs negligible tick time; it does NOT change any verification or
/// acceptance rule, only how many already-accepted entries get checked for payout per
/// round.
pub const SOLVE_SCAN_MAX: u32 = 512;

/// v7.1.41 (grogu-sync-perf, 2026-08-19, operator-directed — "all mining rewards should
/// go to miners"): `MiningBridge::take_solve()` pops exactly ONE FIFO entry per call, with
/// no peek. Under real multi-miner load, valid solves arrive faster than one-per-tick can
/// examine them, so a single stale pop used to discard the WHOLE tick's chance to credit
/// anyone — fresher solves sitting right behind it in the queue just kept aging while they
/// waited their turn, and by the time their turn came they'd often gone stale too.
///
/// Measured live this session: a fresh wallet mined 2 dual-lane-verified, API-"ACCEPTED"
/// solves from a real remote miner; `queued_solves` grew steadily (5→7→8+) over the
/// following minute while total network supply kept climbing (proving other mints WERE
/// happening) and the wallet's own balance stayed at exactly 0 the whole time — the same
/// symptom already diagnosed once before (see `sigil_api::mining::credit_window`'s doc:
/// "93.8% of supply had gone to the producer-wallet fallback"). Widening `credit_window`
/// alone doesn't fix this half of the problem: with arrival-rate > one-per-tick
/// drain-rate and a strict FIFO, sufficient backlog age-out happens regardless of how wide
/// the window is.
///
/// Fix: scan up to [`SOLVE_SCAN_MAX`] entries in ONE tick instead of giving up after the
/// first stale pop — lets a tick catch up through a backlog instead of decaying it one
/// entry at a time. Bounded (not unbounded) so a pathological backlog still can't stall
/// block production for an unbounded time; anything scanned past and found stale is
/// discarded exactly as before (no requeue — same accepted tradeoff as pre-fix, just now
/// applied to up to [`SOLVE_SCAN_MAX`] candidates per tick instead of only 1).
pub fn take_creditable_solve(
    mining_bridge: &sigil_api::mining::MiningBridge,
    parent_hash: BlockHash,
    height: u64,
) -> Option<sigil_api::mining::AcceptedSolve> {
    for _ in 0..SOLVE_SCAN_MAX {
        match mining_bridge.take_solve() {
            Some(s) if s.parent_hash == parent_hash && s.height == height => return Some(s),
            Some(s) if height.saturating_sub(s.height) <= sigil_api::mining::credit_window() => {
                return Some(near_miss_credit(s));
            }
            Some(_) => continue, // stale — drop it, keep scanning THIS tick
            None => return None, // queue empty
        }
    }
    None // scanned the cap without finding a creditable solve
}
