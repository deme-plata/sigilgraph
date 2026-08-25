//! mining.rs — the dual-lane mining surface **on the braid**.
//!
//! `sigil-rpcd` grew a second, linear chain because mining needed a settled tip
//! to bind work to, and the braid only settles after `final_depth`. This module
//! removes that reason: work binds to the braid's **frontier** (the settled chain
//! plus the selected-spine suffix the producer is about to extend), which is
//! exactly the parent the next braid block will carry. Same `flux-miner`
//! verification gate as rpcd, so every existing miner speaks to the braid
//! unchanged — only the URL moves.
//!
//! ## The property that makes this real: headers self-certify their work
//!
//! A challenge is fully determined by `(parent_hash, height)`, and the solved
//! header additionally binds the miner's wallet. All three live in
//! `SigilBlockHeaderV0` (`parent_hash`, `height`, `producer`), and the winning
//! `(nonce, blake4_hash)` ride in the 292-byte `nonce_sqisign` carrier with the
//! Wesolowski proof in `vdf_proof`. So **any follower can reconstruct the
//! challenge and re-verify both lanes from the header alone** —
//! [`verify_header_pow`]. No trust in the producer, no side-channel, no stored
//! block record required. That is what the empty braid never had.
//!
//! ## Canonical wallet encoding (a real tightening over rpcd)
//!
//! `build_header` hashes the wallet as the *string the miner sent*. rpcd accepts
//! any casing, which means a follower holding only the 32 header bytes cannot
//! always rebuild that string. The braid therefore requires the submitted wallet
//! to be exactly `hex::encode(bytes)` — 64 lowercase hex, no `0x`. Non-canonical
//! submissions are rejected with a reason that says how to fix it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Mutex, RwLock};

use flux_miner::client::{build_header, check_submission_at, Challenge, Submission};
use flux_miner::{blake4, verify_dual, DualLaneBlock};
use flux_vdf::{ModSquaring, VdfProof};
use sigil_state::WalletId;

/// Domain-separated per-height challenge seed, bound to the frontier parent:
/// `BLAKE3(domain ‖ parent ‖ height)`. Because `parent` is the hash of the block
/// this work will extend, the seed for the next height is unknowable until that
/// block exists — the same precompute defence rpcd's tip-fold provides.
pub fn mining_seed(parent: &[u8; 32], height: u64) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-g0/mining-challenge/v1");
    h.update(parent);
    h.update(&height.to_le_bytes());
    *h.finalize().as_bytes()
}

/// Lane-A target for difficulty `bits`: `u64::MAX >> bits`.
pub fn target_from_bits(bits: u32) -> u64 {
    u64::MAX >> bits.min(63)
}

/// Lane-A difficulty (`SIGIL_MINING_BLAKE4_BITS`, default 16 — matches rpcd, so a
/// miner pointed at the braid does the same amount of work it does today).
///
/// This is the MANUAL OVERRIDE / cold-start seed, not the live value once
/// auto-retargeting is active — see [`MiningBridge::dynamic_bits`]. Setting
/// the env var explicitly pins `bits` to exactly this value forever (auto-
/// retargeting is skipped whenever it's set), matching today's behavior
/// unchanged for anyone relying on a fixed difficulty.
pub fn blake4_bits() -> u32 {
    std::env::var("SIGIL_MINING_BLAKE4_BITS").ok().and_then(|s| s.parse().ok()).unwrap_or(16)
}

/// Is the operator pinning `bits` by hand? If so, [`MiningBridge::dynamic_bits`]
/// must not touch it — an explicit env var is a deliberate choice (e.g. testing
/// a specific difficulty) and auto-retargeting overriding it would be a
/// surprise, not a fix.
fn blake4_bits_is_pinned() -> bool {
    std::env::var("SIGIL_MINING_BLAKE4_BITS").is_ok()
}

/// How often a real, credited win should land, once auto-retargeting has real
/// data to work from (`SIGIL_MINING_TARGET_WIN_SECS`, default 120 = 2 minutes).
///
/// Picked as a middle ground: frequent enough that mining a reward feels real
/// rather than theoretical (today's actual rate — zero wins across 15,000+
/// blocks — is the failure mode this whole mechanism exists to prevent), rare
/// enough that a single win still means something rather than firing on
/// nearly every block. Not derived from any deeper constraint; a reasonable
/// operator default meant to be tuned via the env var if 2 minutes turns out
/// to feel too fast or too slow in practice.
pub fn target_win_secs() -> f64 {
    std::env::var("SIGIL_MINING_TARGET_WIN_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(120.0)
}

/// Hard bounds `bits` can never leave, however the live math wants to move it
/// (`SIGIL_MIN_BLAKE4_BITS`/`SIGIL_MAX_BLAKE4_BITS`, default 8/48). Prevents
/// two failure modes symmetric to today's incident: retargeting itself into
/// "too easy to mean anything" (bits near 0, every hash wins) or "impossible"
/// (bits near 64, exactly what just happened by hand this morning).
fn min_bits() -> u32 {
    std::env::var("SIGIL_MIN_BLAKE4_BITS").ok().and_then(|s| s.parse().ok()).unwrap_or(8)
}
fn max_bits() -> u32 {
    std::env::var("SIGIL_MAX_BLAKE4_BITS").ok().and_then(|s| s.parse().ok()).unwrap_or(48)
}

/// Largest single step (in bits) auto-retargeting may take per evaluation,
/// however far the analytical target is from the current value. A noisy or
/// momentarily-spiky `net_hps` sample (a miner reconnecting, a burst report)
/// must not be able to swing live difficulty by double digits of bits in one
/// step — that IS today's incident, just automated instead of manual. Chosen
/// so a genuine, sustained hashrate move (the drop-then-recovery Viktor
/// described) still converges in a handful of evaluations, not one shock.
const MAX_STEP_BITS: i64 = 2;

/// Minimum wall-clock gap between retarget evaluations
/// (`SIGIL_MINING_RETARGET_INTERVAL_SECS`, default 20s). `publish_tip` runs on
/// every producer tick — measured elsewhere in this file at as fast as
/// 16-125ms per tick — so evaluating the retarget math on every call would
/// react to noise, not signal, and burn CPU on the producer's hot path for no
/// benefit. 20s is short enough to track a real hashrate swing within a few
/// evaluations, long enough that `net_hps` (itself pruned on a 30s idle
/// window, [`HPS_IDLE_MS`]) has had a chance to reflect who's actually mining.
fn retarget_interval_secs() -> f64 {
    std::env::var("SIGIL_MINING_RETARGET_INTERVAL_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(20.0)
}

fn now_ms_mining() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The analytically-ideal `bits` for `net_hps` hashes/sec to expect a win every
/// `target_secs` seconds.
///
/// Not a fitted curve or a guess: `target_from_bits(bits)` makes a random
/// 64-bit hash word a winner with probability `~2^-bits` (`u64::MAX >> bits`
/// is that fraction of the space), so the EXPECTED number of attempts before a
/// win is `2^bits`, and at `net_hps` attempts/sec the expected wall-clock time
/// to a win is `2^bits / net_hps` seconds. Solving `2^bits / net_hps =
/// target_secs` for `bits` gives this directly — the same reasoning real PoW
/// chains use for difficulty retargeting, just solved in closed form instead
/// of iterated, because `net_hps` is already a live, continuously-updated
/// number here (no need to wait N blocks to estimate it after the fact).
fn ideal_bits_for(net_hps: f64, target_secs: f64) -> Option<u32> {
    if !(net_hps > 0.0) || !(target_secs > 0.0) {
        return None;
    }
    let ideal = (net_hps * target_secs).log2();
    if !ideal.is_finite() {
        return None;
    }
    Some(ideal.round().clamp(0.0, 63.0) as u32)
}

/// Lane-B sequential work (`SIGIL_MINING_VDF_T`, default 600 squarings).
pub fn vdf_t() -> u64 {
    std::env::var("SIGIL_MINING_VDF_T").ok().and_then(|s| s.parse().ok()).unwrap_or(600)
}

/// Lane-B sequential work REQUIRED for a POOL SHARE (`SIGIL_SHARE_VDF_T`,
/// default 8 squarings) — deliberately far smaller than [`vdf_t`] (600).
///
/// 2026-08-20: before this existed, every share was held to the full block
/// `vdf_t`, same as `check_submission_at`'s doc describes for the anti-forgery
/// binding. That's correct for the ANTI-FORGERY property (a share still can't
/// claim a t=0 instant proof), but VDF turns are strictly sequential and cost
/// the SAME wall-clock time regardless of the miner's raw hash rate — CPU or
/// GPU, 5 MH/s or 500 MH/s. Once VARDIFF pushes the hash target easy enough
/// that a nonce is found in microseconds (the whole point of vardiff), the
/// fixed VDF cost becomes the ENTIRE cycle time, and every miner converges to
/// the same VDF-bound share rate no matter how much faster its hardware is —
/// confirmed live 2026-08-20: an RTX 2080 (previously ~500 MH/s) and a CPU
/// (previously ~50 MH/s) both collapsed to within the same order of
/// magnitude once vardiff drove difficulty down to the point where VDF, not
/// hashing, dominated. 8 squarings still proves genuine sequential work (not
/// instant, not free) while being cheap enough that hash power differentiates
/// miners again instead of everyone flatlining at the VDF floor.
pub fn share_vdf_t() -> u64 {
    std::env::var("SIGIL_SHARE_VDF_T").ok().and_then(|s| s.parse().ok()).unwrap_or(8)
}

/// Sub-difficulty share ease in bits (`SIGIL_SHARE_EASE_BITS`). Default 0 =
/// **solo semantics**: only full-difficulty solves are accepted, exactly the
/// pre-pool wire behaviour. Set >0 to run the braid as a pool. This is the
/// per-wallet CEILING vardiff is allowed to issue — see [`share_target_for`].
pub fn share_ease_bits() -> u32 {
    std::env::var("SIGIL_SHARE_EASE_BITS").ok().and_then(|s| s.parse().ok()).unwrap_or(0)
}

/// Target shares/sec each miner should land once vardiff is active
/// (`SIGIL_VARDIFF_RATE`, default 0.5 — same convention `sigil_rpc` already
/// uses for its pool, so an operator tuning one tunes both the same way).
pub fn vardiff_rate() -> f64 {
    std::env::var("SIGIL_VARDIFF_RATE").ok().and_then(|s| s.parse().ok()).unwrap_or(0.5)
}

/// Per-wallet share target from a wallet's self-reported Φ (Lane-A) rate,
/// aiming for ~[`vardiff_rate`] shares/sec instead of handing every wallet the
/// same flat ceiling regardless of hashrate.
///
/// This used to be a bits-*relative-to-the-block-target* "ease" (ported from
/// `sigil_rpc::vardiff_ease_for`, commit f34d06c): `share_target =
/// target_from_bits(bits - ease)`, which structurally can only ever make the
/// share target EASIER than the block target — `ease` is subtracted, never
/// added, and is clamped at a floor of 1. That is fine as long as the
/// operator's `bits` tracks real network hashrate, but at a low/static
/// `blake4_bits` (Epsilon runs the default 16) any wallet reporting more than
/// ~33 kH/s already wants a share difficulty *harder* than the block itself
/// to land only [`vardiff_rate`] hits/sec — impossible to express as a
/// subtraction from `bits`, so the old code saturated to the hardest value it
/// COULD express (`ease = 1`) for every hps above that point. A 33 kH/s CPU
/// and a 5 MH/s+ GPU/CPU therefore got the *identical* share target, so the
/// faster one landed shares ~150x too often instead of converging on the
/// 0.5/sec design rate, drowning the pool in submissions relative to what
/// blocks could actually absorb.
///
/// Fixed 2026-08-21 (operator-reported: "excellent mining stats" regressed to
/// "totally wasted" — CPU effective rate down to ~5 kH/s against a previously
/// measured ~50 MH/s raw baseline). Confirmed live before this fix: a wallet
/// reporting 5,000,000 H/s to Epsilon was issued `ease=1` (bits=15, i.e.
/// barely easier than the 16-bit block target) — this function now returns
/// the share TARGET directly (not a bits-relative ease), computed from
/// `hps`/`rate` as an absolute difficulty.
///
/// ⚠️ CORRECTED same day, hours later (operator-reported: hashrate got WORSE
/// after this fix shipped, down to ~3.68 kH/s): the first version of this fix
/// let the computed target go all the way up to (and past) `bits` — i.e. as
/// hard as, or harder than, the real block target — for any wallet doing
/// more than ~33 kH/s. That broke a load-bearing invariant the CLIENT depends
/// on: `flux_miner::engine::mining_loop` decides pool-vs-solo mode purely
/// from `share_target > blake4_target` (see `engine.rs`'s `let pool = ...`
/// line). Once a share target stops being strictly easier than the block
/// target, the client silently falls back to SOLO discipline — find one
/// block-level hit, submit it, then sleep/poll until the tip advances — which
/// is fine at sub-second block cadence but catastrophic at the ~27s cadence
/// this braid had at the time (itself inflated by an unrelated concurrent
/// incident), leaving the miner almost completely idle between submissions.
/// So the share target must NEVER reach `bits` — capped at `bits - 1`, the
/// hardest difficulty that still keeps the client in pool mode. Above the
/// crossover this makes `share_target_for` numerically equivalent to the
/// original ported `vardiff_ease_for` (ease=1, i.e. bits-1) — that saturation
/// was never actually the bug; it's the correct, unavoidable answer given
/// this invariant. The real, still-standing win from this function over the
/// original is only in the band between the operator ceiling and `bits-1`,
/// where it derives the target from `hps`/`rate` directly instead of via an
/// integer "ease" delta.
///
/// `hps<=1.0` (unknown/idle wallet — nothing reported yet) still gets the
/// flat, easy ceiling (`bits - share_ease`, floored at 1 bit): safe default
/// for a rig that hasn't spoken yet.
///
/// **What this does NOT yet fix:** crediting still weighs every accepted share
/// as `1` regardless of the target it was issued at (unchanged below in
/// `submit()`) — porting `sigil_rpc::achieved_ease`/`share_weight` (grade the
/// share by what its hash actually achieved, not what the pool guessed at
/// issue-time) needs the same live-measurement loop that fix took multiple
/// correction rounds to get right on rpcd (see swarm bus msgs #20/#23/#24,
/// 2026-08-01) — deliberately left as a follow-up rather than ported blind
/// with no compiler and no live pool to test against.
fn share_target_for(hps: f64, rate: f64, bits: u32, share_ease: u32) -> u64 {
    if share_ease == 0 {
        return 0; // solo semantics: pool disabled
    }
    let ceiling_bits = bits.saturating_sub(share_ease).max(1); // easiest allowed
    // Hardest a share may EVER be: strictly easier than the block target, or
    // the client's own `share_target > blake4_target` pool-detection breaks.
    let hardest_bits = bits.saturating_sub(1).max(1);
    let share_bits = if hps > 1.0 {
        let wanted_bits = (hps / rate).log2().ceil().max(1.0) as u32;
        // Never easier than the operator ceiling, and NEVER as hard as (or
        // harder than) the block target — clamp at `hardest_bits`.
        wanted_bits.max(ceiling_bits).min(hardest_bits)
    } else {
        ceiling_bits
    };
    target_from_bits(share_bits.min(63))
}

/// Lane-A share target for `bits` eased by `ease`: `0` (accept only full
/// difficulty) when `ease` is `0`, else the target at `bits - ease` (floored
/// at 1 bit so an aggressive ease can never hand out a target that accepts
/// everything). Ported from `sigil_rpc::share_target_from`.
fn share_target_from(bits: u32, ease: u32) -> u64 {
    if ease == 0 {
        return 0;
    }
    target_from_bits(bits.saturating_sub(ease).max(1))
}

/// The 292-byte `nonce_sqisign` carrier layout shared with
/// `sigil_rpc::build_ledger_header`: `nonce` LE ‖ `blake4_hash` LE ‖ zeros.
/// The claimed Lane-A word must be the word the header+nonce actually hash to.
///
/// `verify_dual` bounds the claimed `blake4_hash` by the target and separately
/// re-hashes the nonce — but it never ties the two together, so a valid nonce
/// paired with ANY under-target word (`0` being the easy one) passes. rpcd can
/// live with that because it only credits a share. The braid cannot: this word
/// is packed into the header's nonce carrier and is what the chain permanently
/// records as the block's proof-of-work, so a forged word would write a lie
/// into a header that still verified. One extra BLAKE4 buys the self-certifying
/// property this module claims. Honest miners are unaffected — `mine_dual`
/// always reports the exact recomputed word.
fn lane_a_word_is_honest(header: &[u8], nonce: u64, claimed: u64) -> bool {
    blake4(header, nonce) == claimed
}

pub fn pack_nonce_carrier(nonce: u64, blake4_hash: u64) -> [u8; 292] {
    let mut c = [0u8; 292];
    c[..8].copy_from_slice(&nonce.to_le_bytes());
    c[8..16].copy_from_slice(&blake4_hash.to_le_bytes());
    c
}

/// Inverse of [`pack_nonce_carrier`] — `(nonce, blake4_hash)`.
pub fn unpack_nonce_carrier(carrier: &[u8]) -> Option<(u64, u64)> {
    if carrier.len() < 16 {
        return None;
    }
    let mut n = [0u8; 8];
    let mut b = [0u8; 8];
    n.copy_from_slice(&carrier[..8]);
    b.copy_from_slice(&carrier[8..16]);
    Some((u64::from_le_bytes(n), u64::from_le_bytes(b)))
}

/// **The follower's verification of a mined braid block.** Rebuilds the exact
/// challenge the miner solved from header material only, then verifies Lane A
/// (BLAKE4 ≤ target) and Lane B (the Wesolowski proof over the required number
/// of sequential squarings). Every node runs this on every block — the work is
/// checked, never trusted.
pub fn verify_header_pow(
    parent_hash: &[u8; 32],
    height: u64,
    producer: &WalletId,
    nonce_carrier: &[u8],
    vdf: &VdfProof,
    bits: u32,
    required_vdf_t: u64,
) -> bool {
    let Some((nonce, blake4_hash)) = unpack_nonce_carrier(nonce_carrier) else {
        return false;
    };
    if vdf.t != required_vdf_t {
        return false;
    }
    let c = Challenge {
        height,
        vdf_input: mining_seed(parent_hash, height),
        blake4_target: target_from_bits(bits),
        vdf_t: required_vdf_t,
        net_hps: 0.0,
        share_target: 0,
        share_vdf_t: 0, // full-block verification only; no pool share involved here
    };
    let block = DualLaneBlock {
        header: build_header(&c, &hex::encode(producer)),
        nonce,
        blake4_hash,
        vdf: vdf.clone(),
    };
    if !lane_a_word_is_honest(&block.header, nonce, blake4_hash) {
        return false;
    }
    verify_dual(&ModSquaring::bench_2048(), &block, c.blake4_target)
}

/// The frontier a miner is working against: the parent the next braid block will
/// carry, plus the difficulty parameters in force for it.
#[derive(Clone, Debug, PartialEq)]
pub struct MiningTip {
    pub height: u64,
    pub parent_hash: [u8; 32],
    pub bits: u32,
    pub vdf_t: u64,
}

/// A verified full-difficulty solve, queued for the producer to mint into a
/// braid block. Carries everything the header needs — no re-derivation, no
/// second lookup.
#[derive(Clone, Debug)]
pub struct AcceptedSolve {
    pub wallet: WalletId,
    pub height: u64,
    pub parent_hash: [u8; 32],
    pub nonce: u64,
    pub blake4_hash: u64,
    pub vdf: VdfProof,
    pub bits: u32,
    /// Share weights accumulated for this height, for proportional payout. The
    /// winner's own weight is already folded in.
    pub shares: HashMap<WalletId, u64>,
}

/// Why a submission was turned away — the categories the pool diagnostics count.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RejectKind {
    StaleHeight,
    NonCanonicalWallet,
    Duplicate,
    VerifyMismatch,
    NoTip,
}

impl RejectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RejectKind::StaleHeight => "stale_height",
            RejectKind::NonCanonicalWallet => "non_canonical_wallet",
            RejectKind::Duplicate => "duplicate",
            RejectKind::VerifyMismatch => "verify_mismatch",
            RejectKind::NoTip => "no_tip",
        }
    }
}

/// Outcome of a submission, mapped to the wire `SubmitResult` by the handler.
#[derive(Clone, Debug, PartialEq)]
pub enum SubmitOutcome {
    /// Full-difficulty solve — queued; the producer will mint it.
    Block { height: u64 },
    /// Sub-difficulty share — credited into this height's payout window.
    Share { height: u64, weight: u64 },
    Rejected { kind: RejectKind, detail: String },
}

/// The seam between the HTTP surface (many threads) and the producer loop (one
/// thread). The producer publishes the frontier it is about to extend; miners
/// pull challenges from it and push solves back; the producer pops a solve when
/// it mints. Nothing else crosses.
#[derive(Default)]
pub struct MiningBridge {
    tip: RwLock<Option<MiningTip>>,
    /// Bounded history of recently-published tips, newest last. Lets `submit()`
    /// verify a submission that arrived after the frontier already moved on
    /// against the tip it was ACTUALLY solved for, instead of only the current
    /// one. See [`credit_window`] for why this exists.
    recent_tips: Mutex<VecDeque<MiningTip>>,
    solved: Mutex<VecDeque<AcceptedSolve>>,
    /// Share weights for the height currently being worked.
    shares: Mutex<HashMap<WalletId, u64>>,
    /// `(wallet, nonce)` already credited, indexed BY HEIGHT — replay guard.
    /// Keyed by height (not a flat `(wallet, nonce, height)` set) since
    /// [`credit_window`] means "already credited" now spans multiple
    /// heights, not just the current one; pruned by height on every tip
    /// advance instead of wiped wholesale, or a submission for an
    /// older-but-still-in-window height could be replayed for a second
    /// credit once the current height's entries clear.
    ///
    /// 2026-08-21/22 (the "production crawled to ~1 block/30s" incident):
    /// was a flat `HashSet<(WalletId, u64, u64)>`, pruned via
    /// `.retain(|&(_,_,h)| h >= floor)` on every advance — an O(total
    /// entries) scan REGARDLESS of how many actually aged out. Under real
    /// submission volume this became the dominant cost (measured live via
    /// `perf`: 32%+ of all CPU in raw `memcmp`), and — because
    /// `publish_tip()` runs inline in the SAME producer tick that's trying
    /// to advance the frontier — a slow retain() directly stalled block
    /// production itself, which widened the real-time span of the (height-
    /// bounded) credit window, which grew the set further: a genuine
    /// vicious cycle. Indexing by height first makes eviction O(number of
    /// height KEYS being dropped) — bounded by `credit_window` (default 20)
    /// regardless of how many submissions are nested under each height —
    /// instead of O(everything currently in the window).
    seen: Mutex<HashMap<u64, HashSet<(WalletId, u64)>>>,
    /// Self-reported Lane-A rate per (wallet, rig): `(hashes/s, last_report_ms)`.
    ///
    /// 2026-08-24 (MULTI-RIG fix): was keyed by `WalletId` alone. Two physical
    /// rigs mining to the same payout wallet each called `report_hps` with
    /// their own rate, and the second call's `m.insert(w, ...)` silently
    /// overwrote the first — `net_hps` (the network-total display) only ever
    /// reflected whichever rig reported most recently, not the sum. Confirmed
    /// live: operator's two rigs measured 1 GH/s+ combined, the pool's
    /// `/mining/miners` showed ~453 MH/s. `flux_miner::client::rig_id()`
    /// already exists client-side (this same session, uncommitted) and sends
    /// `&rig=` on every challenge fetch specifically to fix this — this map
    /// is the server half that was still missing to actually use it. Keying
    /// by `(WalletId, String)` with the rig id defaulting to `""` when a
    /// client doesn't send one preserves today's (degraded but not broken)
    /// clobbering behavior for pre-fix miners — purely additive, no client is
    /// worse off than before.
    hps: Mutex<HashMap<(WalletId, String), (f64, u64)>>,
    rejects: Mutex<HashMap<&'static str, u64>>,
    accepted_blocks: Mutex<u64>,
    accepted_shares: Mutex<u64>,
    /// Auto-retargeted `bits` + when it was last evaluated. `None` until the
    /// first evaluation — see [`dynamic_bits`](MiningBridge::dynamic_bits) for
    /// the cold-start rule. Separate from [`blake4_bits`]'s env var: that
    /// function is now only the manual-override / cold-start seed value.
    auto_bits: Mutex<Option<(u32, u64)>>,
}

/// Depth of the solve queue.
///
/// **Was `8`, raised 2026-08-18.** The original doc comment here worried that
/// a deep queue would "let a burst of solves mint a burst of blocks against a
/// stale parent" — but that isn't how the consumer (`sigil-node`'s producer
/// loop, `main.rs`) actually works: it calls [`MiningBridge::take_solve`]
/// **at most once per tick** and checks the popped solve against the CURRENT
/// mint target before using it (exact match mints with real PoW, a near-miss
/// within [`credit_window`] still credits via `near_miss_credit`, anything
/// older just `continue`s/`None`s and is discarded). So queue depth can never
/// cause more than one block to mint per tick, and a stale entry is safely
/// dropped the moment it's popped — a deeper queue costs a little memory
/// (`AcceptedSolve` is small) and nothing else.
///
/// The `8` cap turned out to actively hurt payouts: under real host
/// contention (this box also runs Quillon production + an Ethereum node, all
/// competing for the same cores the sequential VDF step needs), block
/// production can stall for tens of seconds while a fast miner finds several
/// solves per second — FIFO eviction was silently dropping already-verified,
/// still-within-`credit_window` solves before the producer ever got to them,
/// so miners went uncredited despite doing real, accepted work. Confirmed
/// live 2026-08-17: `queued_solves` pinned at exactly `8` for ~15 minutes
/// while `blocks_accepted` kept climbing and `shares_accepted` stayed at `0`.
/// Sized generously against that failure mode instead of tightly against a
/// risk the consumer already closes.
const SOLVE_QUEUE_CAP: usize = 512;

/// Miners are pruned from the rate table after this long without a challenge
/// fetch, so `net_hps` reflects live power only.
const HPS_IDLE_MS: u64 = 30_000;

/// How many blocks behind the current frontier a submission may still be
/// credited (`SIGIL_MINING_CREDIT_WINDOW`, default 20). Operator-directed
/// 2026-08-16: with this braid producing a block every ~16-125ms, a solve
/// that traveled a real internet round-trip (network latency + HTTP overhead)
/// is very often "stale" by the exact-match standard before it even arrives —
/// measured live: 93.8% of the entire supply had gone to the node's
/// placeholder producer wallet because real miner submissions almost never
/// won the exact-height race. 20 blocks still isn't much wall-clock at this
/// cadence (roughly 320ms-2.5s depending on the adaptive rate), which is why
/// the default leans toward "enough to matter" over "as wide as possible" —
/// tune via env if that turns out to be too tight or too loose in practice.
pub fn credit_window() -> u64 {
    std::env::var("SIGIL_MINING_CREDIT_WINDOW").ok().and_then(|s| s.parse().ok()).unwrap_or(20)
}

/// Capacity of the recent-tips ring — must comfortably exceed [`credit_window`]
/// or a submission within the intended window could already have aged out of
/// the history that would let it verify. Some slack on top of the configured
/// window, floored so a pathologically small window still gets a sane buffer.
fn recent_tips_capacity() -> usize {
    (credit_window() as usize).saturating_add(16).max(32)
}

impl MiningBridge {
    pub fn new() -> Self {
        Self::default()
    }

    /// Producer: publish the frontier the next block will extend. Called every
    /// produce tick; when the height advances, the previous height's share
    /// window and replay set are cleared (they belong to work that is done).
    /// The live `bits` value: auto-retargeted from real observed `net_hps`
    /// toward [`target_win_secs`], unless the operator has pinned
    /// [`blake4_bits`] by hand ([`blake4_bits_is_pinned`]) — in which case this
    /// returns exactly that pinned value, unchanged, forever (today's
    /// behavior, preserved).
    ///
    /// THE INCIDENT THIS EXISTS TO PREVENT: 2026-08-24, a manual bits change
    /// (16→40, to fix an unrelated vardiff display bug) made full-block wins
    /// go from "rare" (41 in the chain's history) to "zero across 15,000+
    /// blocks" — because nothing was watching whether the new value still
    /// matched real network hashrate. This closes that gap by continuously
    /// deriving `bits` FROM real hashrate instead of trusting a human to keep
    /// a static number calibrated.
    ///
    /// Cold start: with no prior evaluation and no live `net_hps` yet (e.g.
    /// right after a restart, before any miner has reported), there is
    /// nothing real to compute from — fall back to [`blake4_bits`]'s default
    /// (16) rather than guessing. The first real evaluation happens once a
    /// miner has reported AND [`retarget_interval_secs`] has passed.
    pub fn dynamic_bits(&self) -> u32 {
        if blake4_bits_is_pinned() {
            return blake4_bits();
        }
        let now = now_ms_mining();
        let mut guard = self.auto_bits.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((bits, last_eval)) = *guard {
            let elapsed_secs = now.saturating_sub(last_eval) as f64 / 1000.0;
            if elapsed_secs < retarget_interval_secs() {
                return bits; // too soon — hold the last evaluated value
            }
            let net_hps = self.report_hps(None, None, None, now);
            let next = match ideal_bits_for(net_hps, target_win_secs()) {
                Some(ideal) => {
                    let step = (ideal as i64 - bits as i64).clamp(-MAX_STEP_BITS, MAX_STEP_BITS);
                    ((bits as i64 + step).clamp(min_bits() as i64, max_bits() as i64)) as u32
                }
                // No live hashrate signal right now (every miner idled out) —
                // hold rather than drift toward an arbitrary value with no data.
                None => bits,
            };
            *guard = Some((next, now));
            next
        } else {
            // First-ever evaluation: seed from the configured default so the
            // very first published tip is never a guess, then let the next
            // evaluation (>= retarget_interval_secs later) start real-adjusting.
            let seed = blake4_bits();
            *guard = Some((seed, now));
            seed
        }
    }

    pub fn publish_tip(&self, height: u64, parent_hash: [u8; 32]) {
        let new = MiningTip { height, parent_hash, bits: self.dynamic_bits(), vdf_t: vdf_t() };
        let advanced = {
            let cur = self.tip.read().ok();
            match cur.as_deref() {
                Some(Some(t)) => t.height != new.height || t.parent_hash != new.parent_hash,
                _ => true,
            }
        };
        if let Ok(mut w) = self.tip.write() {
            *w = Some(new.clone());
        }
        if advanced {
            // Prune replay entries whose height has aged OUT of the credit
            // window (they can never be submitted again anyway — submit()
            // itself would reject them as stale), instead of wiping the whole
            // set. A blanket clear on every advance would forget an
            // in-window height's entries the moment a NEWER block lands,
            // reopening the replay hole the widened credit window exists to
            // close.
            let floor = height.saturating_sub(credit_window());
            if let Ok(mut s) = self.seen.lock() {
                // O(height keys dropped), not O(total submissions in the
                // window) — see the field's doc comment.
                s.retain(|&h, _| h >= floor);
            }
            if let Ok(mut h) = self.recent_tips.lock() {
                h.push_back(new);
                let cap = recent_tips_capacity();
                while h.len() > cap {
                    h.pop_front();
                }
            }
        }
    }

    pub fn tip(&self) -> Option<MiningTip> {
        self.tip.read().ok().and_then(|t| t.clone())
    }

    /// The tip that was live at `height`, if it's still within the recent-tips
    /// history — the challenge a submission for that height was actually
    /// solved against. `None` means either the height is ahead of the current
    /// frontier (not a real challenge yet) or old enough to have aged out of
    /// the bounded history (genuinely too stale, not just "a couple behind").
    fn tip_at(&self, height: u64) -> Option<MiningTip> {
        // 2026-08-25 (verify_mismatch root cause): `recent_tips` can hold MORE THAN
        // ONE entry for the same height -- `publish_tip` appends whenever the
        // frontier's tentative parent at that height changes, which happens
        // routinely while a height is still inside the final_depth probation
        // window (the braid re-electing its next candidate before committing to
        // it). Searching front-to-back (oldest first) returned whichever candidate
        // was proposed FIRST, which is exactly the one most likely to have been
        // abandoned once something else won. The entry that matters is the LAST
        // one published at that height -- the one live at the moment the frontier
        // actually advanced past it, which is definitionally the one every later
        // height's parent_hash chains back to. Searching newest-first (`.rev()`)
        // fixes this: proven by `tip_at_returns_the_abandoned_candidate_not_the_one_that_won`,
        // which failed before this change (valid work against the winning candidate
        // was rejected as verify_mismatch) and passes after it.
        self.recent_tips.lock().ok()?.iter().rev().find(|t| t.height == height).cloned()
    }

    /// Record one (wallet, rig)'s self-reported Lane-A rate and return the
    /// live network total (sum over every rig that fetched a challenge in
    /// the last 30s, across every wallet). `rig` defaults to `""` when the
    /// caller (an old client, or a bare wallet-only request) doesn't supply
    /// one — see the [`Self::hps`] field doc for why that's a safe default.
    pub fn report_hps(&self, wallet: Option<WalletId>, rig: Option<String>, hps: Option<f64>, now_ms: u64) -> f64 {
        let Ok(mut m) = self.hps.lock() else { return 0.0 };
        if let (Some(w), Some(r)) = (wallet, hps) {
            if r.is_finite() && r >= 0.0 {
                m.insert((w, rig.unwrap_or_default()), (r, now_ms));
            }
        }
        m.retain(|_, (_, t)| now_ms.saturating_sub(*t) <= HPS_IDLE_MS);
        m.values().map(|(r, _)| *r).sum()
    }

    /// This ONE (wallet, rig)'s own last-reported Lane-A rate, or `0.0` if it
    /// has never reported (or aged out). This is what vardiff must use to
    /// size a share target for the rig that's actually asking — summing
    /// across a wallet's OTHER rigs here would issue every rig a target
    /// calibrated for the wallet's combined rate, so a wallet running two
    /// identical rigs would have each one individually undershoot the
    /// intended shares/sec by ~2x. See [`Self::hps_for_wallet_total`] for the
    /// (deliberately different) summed view a human dashboard wants.
    fn hps_for_rig(&self, wallet: Option<WalletId>, rig: Option<&str>) -> f64 {
        let Some(w) = wallet else { return 0.0 };
        let key = (w, rig.unwrap_or("").to_string());
        self.hps.lock().ok().and_then(|m| m.get(&key).map(|(r, _)| *r)).unwrap_or(0.0)
    }

    /// This wallet's TOTAL self-reported rate, summed across every rig
    /// mining to it. `0.0` if the wallet has no live (unexpired) report from
    /// any rig.
    ///
    /// `pub` since 2026-08-23: the `/mining/miners` endpoint needs this for
    /// its optional `?wallet=` readback (the wallet UI's "my hashrate"
    /// pill) — see [`super::mining_miners`]. Deliberately SUMMED across rigs
    /// (unlike [`Self::hps_for_rig`]): a human checking their own dashboard
    /// wants their whole farm's contribution, not one arbitrary rig's.
    pub fn hps_for_wallet_total(&self, wallet: Option<WalletId>) -> f64 {
        let Some(w) = wallet else { return 0.0 };
        self.hps.lock().ok().map(|m| {
            m.iter().filter(|((mw, _), _)| *mw == w).map(|(_, (r, _))| *r).sum()
        }).unwrap_or(0.0)
    }

    /// Build the challenge for `wallet` against the current frontier. `None`
    /// when the producer has not published a frontier yet (node still booting,
    /// or not a producer).
    ///
    /// The issued `share_target` is per-wallet vardiff, not a flat global
    /// ease: a wallet that has told us its own rate gets a target aimed at
    /// [`vardiff_rate`] shares/sec for THAT rate, capped by the operator's
    /// [`share_ease_bits`] ceiling. Before this, every wallet — a phone doing
    /// a few KH/s and a desktop GPU doing tens of MH/s alike — was issued the
    /// exact same target, so whichever end of that range the flat ease didn't
    /// suit went long stretches with nothing accepted at all.
    pub fn challenge_for(&self, wallet: Option<WalletId>, rig: Option<String>, now_ms: u64) -> Option<Challenge> {
        let tip = self.tip()?;
        let net_hps = self.report_hps(wallet, rig.clone(), None, now_ms);
        // THIS rig's own rate, not the wallet's combined total — see
        // `hps_for_rig`'s doc for why vardiff must not sum across rigs here.
        let my_hps = self.hps_for_rig(wallet, rig.as_deref());
        let share_target = share_target_for(my_hps, vardiff_rate(), tip.bits, share_ease_bits());
        Some(Challenge {
            height: tip.height,
            vdf_input: mining_seed(&tip.parent_hash, tip.height),
            blake4_target: target_from_bits(tip.bits),
            vdf_t: tip.vdf_t,
            net_hps,
            share_target,
            share_vdf_t: share_vdf_t(),
        })
    }

    fn count_reject(&self, kind: RejectKind) {
        if let Ok(mut r) = self.rejects.lock() {
            *r.entry(kind.as_str()).or_insert(0) += 1;
        }
    }

    /// Verify a submission against the current frontier and, if it is work,
    /// bank it: a full-difficulty solve is queued for minting, a sub-difficulty
    /// solve is credited into this height's share window.
    pub fn submit(&self, sub: &Submission) -> SubmitOutcome {
        let Some(tip) = self.tip() else {
            self.count_reject(RejectKind::NoTip);
            return SubmitOutcome::Rejected {
                kind: RejectKind::NoTip,
                detail: "this node is not producing — no frontier to mine on".into(),
            };
        };

        // Canonical wallet: the header commits to the wallet STRING, so it must
        // be reproducible from the 32 bytes a follower reads out of the header.
        let Some(wallet) = crate::hex32(&sub.wallet) else {
            self.count_reject(RejectKind::NonCanonicalWallet);
            return SubmitOutcome::Rejected {
                kind: RejectKind::NonCanonicalWallet,
                detail: "wallet must be 64-hex".into(),
            };
        };
        if sub.wallet != hex::encode(wallet) {
            self.count_reject(RejectKind::NonCanonicalWallet);
            return SubmitOutcome::Rejected {
                kind: RejectKind::NonCanonicalWallet,
                detail: "wallet must be canonical lowercase 64-hex with no 0x prefix — \
                         the block header commits to this exact string so every follower \
                         can rebuild and re-verify the work".into(),
            };
        }

        // vtip: the tip this submission's work was ACTUALLY solved against —
        // the current frontier for an exact-height submission, or a recent
        // one from history for a submission that arrived a few blocks late.
        // `historical` marks the latter so the branches below know to skip
        // the partial-share pool (which belongs to the CURRENT height only —
        // mixing a stale win into it would credit the wrong height's pool)
        // and to build a solo shares map instead, same shape
        // `mint_next_block`'s producer-wallet fallback already uses.
        //
        // ⚠️ WIDENED (2026-08-16, operator-directed): this used to require
        // sub.height == tip.height EXACTLY. At this braid's block cadence
        // (~16-125ms/block) that meant almost no real internet-connected
        // miner could ever land a submission before the frontier moved past
        // it — measured live: 93.8% of the entire supply had gone to the
        // node's placeholder wallet, not to any real miner. See
        // [`credit_window`] for the tunable and its reasoning.
        let (vtip, historical) = if sub.height == tip.height {
            (tip.clone(), false)
        } else if sub.height < tip.height && tip.height - sub.height <= credit_window() {
            match self.tip_at(sub.height) {
                Some(t) => (t, true),
                None => {
                    self.count_reject(RejectKind::StaleHeight);
                    return SubmitOutcome::Rejected {
                        kind: RejectKind::StaleHeight,
                        detail: format!(
                            "height {} is within the credit window but its challenge \
                             already aged out of history — genuinely too stale",
                            sub.height
                        ),
                    };
                }
            }
        } else {
            self.count_reject(RejectKind::StaleHeight);
            return SubmitOutcome::Rejected {
                kind: RejectKind::StaleHeight,
                detail: format!(
                    "stale height: submitted {} but the mineable frontier is {} \
                     (outside the {}-block credit window)",
                    sub.height, tip.height, credit_window()
                ),
            };
        };

        // Replay guard before any verification work is banked. Keyed by
        // height now (not just wallet+nonce) — see the `seen` field doc for
        // why: "already credited" now spans the whole credit window.
        let key = (wallet, sub.block.nonce);
        {
            let Ok(mut seen) = self.seen.lock() else {
                return SubmitOutcome::Rejected {
                    kind: RejectKind::VerifyMismatch,
                    detail: "replay set unavailable".into(),
                };
            };
            if !seen.entry(sub.height).or_default().insert(key) {
                self.count_reject(RejectKind::Duplicate);
                return SubmitOutcome::Rejected {
                    kind: RejectKind::Duplicate,
                    detail: "this (wallet, nonce) already credited at this height".into(),
                };
            }
        }

        // Verify against the GLOBAL ceiling, not this wallet's specific
        // per-wallet-issued ease: hps can have moved between when this wallet
        // fetched its challenge and when it submits, and re-deriving the
        // narrower per-wallet target here would reject an honest submission
        // over a race the miner didn't cause. The global ceiling is the
        // widest target any wallet could legitimately have been issued, so
        // verifying against it is the lenient, race-free bound — same
        // "verify leniently" principle `sigil_rpc::achieved_ease` documents
        // (see the note on `vardiff_ease_for` above for why the other half of
        // that fix, crediting by achieved ease, is not ported yet).
        //
        // Built from `vtip` — the tip this submission was ACTUALLY solved
        // against (current, or a recent one from history) — not always the
        // live `tip`, or a historical submission would verify against a
        // challenge it was never solved for and always fail.
        let ease = share_ease_bits();
        let c = Challenge {
            height: vtip.height,
            vdf_input: mining_seed(&vtip.parent_hash, vtip.height),
            blake4_target: target_from_bits(vtip.bits),
            vdf_t: vtip.vdf_t,
            net_hps: 0.0,
            share_target: share_target_from(vtip.bits, ease),
            share_vdf_t: share_vdf_t(),
        };
        let g = ModSquaring::bench_2048();

        // A dishonest Lane-A word falls through to the tail, which releases the
        // replay slot — so an honest retry on the same nonce is not locked out.
        let honest_word =
            lane_a_word_is_honest(&sub.block.header, sub.block.nonce, sub.block.blake4_hash);

        if honest_word && check_submission_at(&g, &c, sub, c.blake4_target, c.vdf_t) {
            // The current height's partial-share pool belongs to the CURRENT
            // height only — a historical win must not fold into it (that
            // pool already moved on) or be cleared out from under whoever's
            // still working the live height. Solo credit instead, same shape
            // `mint_next_block`'s producer-wallet fallback already uses.
            let weight: u64 = 1u64 << ease.min(32);
            let shares = if historical {
                HashMap::from([(wallet, weight)])
            } else {
                let mut shares = self.shares.lock().map(|m| m.clone()).unwrap_or_default();
                *shares.entry(wallet).or_insert(0) += weight;
                shares
            };
            let solve = AcceptedSolve {
                wallet,
                height: vtip.height,
                parent_hash: vtip.parent_hash,
                nonce: sub.block.nonce,
                blake4_hash: sub.block.blake4_hash,
                vdf: sub.block.vdf.clone(),
                bits: vtip.bits,
                shares,
            };
            if let Ok(mut q) = self.solved.lock() {
                if q.len() >= SOLVE_QUEUE_CAP {
                    q.pop_front();
                }
                q.push_back(solve);
            }
            if !historical {
                if let Ok(mut m) = self.shares.lock() {
                    m.clear();
                }
            }
            if let Ok(mut n) = self.accepted_blocks.lock() {
                *n += 1;
            }
            return SubmitOutcome::Block { height: vtip.height };
        }

        // 2026-08-25 (verify_mismatch investigation, part 2): this used to be
        // gated `if !historical && ...`, on the theory that `self.shares` is
        // "the CURRENT height's pool" and a late share shouldn't fold into a
        // pool that's "already moved on". That premise doesn't hold: `self.shares`
        // is never reset on a height advance (only `publish_tip`'s neighboring
        // `seen`-pruning is height-aware) — it is ONE ongoing accumulator that
        // only ever clears when a NON-historical block win pays it out. A
        // historical share is exactly as real as a current one and belongs in
        // the exact same pool; gating it out here didn't protect anything, it
        // just discarded genuine work as `verify_mismatch`. Measured live: at
        // this chain's real block cadence (~100-150ms/block), most honest
        // submissions arrive at least one height late purely from real network
        // + VDF compute latency, making this the common case, not an edge case
        // — a sustained ~30-40% live rejection rate traced directly to this gate.
        // Proven by `a_late_share_is_wrongly_rejected_instead_of_credited`
        // (failed before this change, passes after).
        if honest_word && c.share_target > 0 && check_submission_at(&g, &c, sub, c.share_target, c.share_vdf_t) {
            let weight = 1u64;
            if let Ok(mut m) = self.shares.lock() {
                *m.entry(wallet).or_insert(0) += weight;
            }
            if let Ok(mut n) = self.accepted_shares.lock() {
                *n += 1;
            }
            return SubmitOutcome::Share { height: vtip.height, weight };
        }

        // Not work: release the replay slot so an honest retry isn't locked out.
        if let Ok(mut seen) = self.seen.lock() {
            if let Some(at_height) = seen.get_mut(&sub.height) {
                at_height.remove(&key);
            }
        }
        self.count_reject(RejectKind::VerifyMismatch);
        SubmitOutcome::Rejected {
            kind: RejectKind::VerifyMismatch,
            detail: "dual-lane verify failed (wrong frontier, target, VDF, or a \
                     blake4_hash that is not what this header+nonce hash to)".into(),
        }
    }

    /// Producer: take the next verified solve to mint, if one is waiting.
    pub fn take_solve(&self) -> Option<AcceptedSolve> {
        self.solved.lock().ok().and_then(|mut q| q.pop_front())
    }

    pub fn queued_solves(&self) -> usize {
        self.solved.lock().map(|q| q.len()).unwrap_or(0)
    }

    /// Snapshot for the miners endpoint: `(net_hps, live_miners, blocks, shares, rejects)`.
    ///
    /// `live_miners` counts live (wallet, rig) entries — i.e. live RIGS, not
    /// distinct wallets. Two rigs on one wallet now correctly count as 2,
    /// which is what actually answers "how many miners are hashing right
    /// now" (the multi-rig fix's whole point); a pre-fix client with no rig
    /// id still counts as 1 per wallet, same as before.
    pub fn stats(&self, now_ms: u64) -> (f64, usize, u64, u64, Vec<(String, u64)>) {
        let (net_hps, live) = self
            .hps
            .lock()
            .map(|m| {
                let live: Vec<_> =
                    m.values().filter(|(_, t)| now_ms.saturating_sub(*t) <= HPS_IDLE_MS).collect();
                (live.iter().map(|(r, _)| *r).sum::<f64>(), live.len())
            })
            .unwrap_or((0.0, 0));
        let blocks = self.accepted_blocks.lock().map(|n| *n).unwrap_or(0);
        let shares = self.accepted_shares.lock().map(|n| *n).unwrap_or(0);
        let mut rejects: Vec<(String, u64)> = self
            .rejects
            .lock()
            .map(|r| r.iter().map(|(k, v)| ((*k).to_string(), *v)).collect())
            .unwrap_or_default();
        rejects.sort();
        (net_hps, live, blocks, shares, rejects)
    }
}

/// `bridge_at` (and every test module's other env-touching tests) mutate
/// PROCESS-WIDE `SIGIL_*` env vars — but `cargo test` runs tests in parallel
/// threads by default, so without serialization two tests can interleave:
/// one resets `SIGIL_SHARE_EASE_BITS` to `0` while another is mid-assertion
/// expecting it to still be `8`. Every test that touches these vars
/// (directly or via `bridge_at`) must hold this lock for the duration.
/// Caught 2026-08-16 by
/// `a_reporting_gpu_gets_an_easier_target_than_an_unknown_wallet` flaking
/// under `cargo test`'s default parallelism — not a logic bug, a missing
/// guard around genuinely shared mutable state.
///
/// 2026-08-25: hoisted from inside `mod tests` to file scope so `mod
/// dynamic_bits_tests` (a later, separate test module that also mutates
/// `SIGIL_MINING_BLAKE4_BITS`) can share the SAME lock. Two different
/// `Mutex`es each guarding "the process environment" would not have
/// serialized anything — the whole point is one lock per genuinely-shared
/// resource. `dynamic_bits_tests` had exactly this bug: it mutated the env
/// var with no lock at all, and flaked under the full suite's parallelism
/// while passing every time it happened to run alone.
#[cfg(test)]
static ENV_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use flux_miner::client::solve;

    fn tiny_bits() -> u32 {
        4
    }

    fn bridge_at(height: u64, parent: [u8; 32]) -> MiningBridge {
        // Keep the test's work trivial and deterministic.
        std::env::set_var("SIGIL_MINING_BLAKE4_BITS", tiny_bits().to_string());
        std::env::set_var("SIGIL_MINING_VDF_T", "8");
        std::env::set_var("SIGIL_SHARE_EASE_BITS", "0");
        let b = MiningBridge::new();
        b.publish_tip(height, parent);
        b
    }

    fn solve_for(c: &Challenge, wallet: &WalletId) -> Submission {
        let g = ModSquaring::bench_2048();
        let block = solve(c, &hex::encode(wallet), &g);
        Submission { height: c.height, wallet: hex::encode(wallet), block }
    }

    #[test]
    fn challenge_binds_to_the_frontier_parent() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let parent_a = [0x11u8; 32];
        let parent_b = [0x22u8; 32];
        let b = bridge_at(7, parent_a);
        let c1 = b.challenge_for(None, None, 0).unwrap();
        assert_eq!(c1.height, 7);
        assert_eq!(c1.vdf_input, mining_seed(&parent_a, 7));

        // A different frontier parent at the same height is different work —
        // this is what stops precompute against a fork that never wins.
        b.publish_tip(7, parent_b);
        let c2 = b.challenge_for(None, None, 0).unwrap();
        assert_ne!(c1.vdf_input, c2.vdf_input);
    }

    /// PROOF OF MECHANISM (2026-08-25, verify_mismatch investigation): `recent_tips`
    /// can receive MORE THAN ONE entry for the same height -- `publish_tip` appends
    /// whenever `advanced` is true, which fires on ANY parent_hash change even if the
    /// height doesn't move (the frontier re-electing its tentative next candidate
    /// before it's finalized, which the braid does constantly -- see the 512-block
    /// `final_depth` probation window). `tip_at()` then does `.find()`, returning the
    /// FIRST (oldest) matching entry -- the ABANDONED candidate, not whichever one
    /// actually got built on and became real history. A miner who solved against the
    /// candidate that WON gets verified against the one that LOST, and fails for a
    /// reason that has nothing to do with their work being wrong.
    #[test]
    fn tip_at_returns_the_abandoned_candidate_not_the_one_that_won() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let wallet: WalletId = [0xCDu8; 32];
        let parent_abandoned = [0x11u8; 32];
        let parent_won = [0x22u8; 32];

        let b = bridge_at(7, parent_abandoned);
        // The frontier re-picks its height-7 candidate before moving on -- same
        // height, different parent. This is the "advanced" branch firing on a
        // parent_hash change alone (height unchanged).
        b.publish_tip(7, parent_won);
        // Miner solves against whichever challenge is live AT THE TIME -- the one
        // that just won the re-pick.
        let c = b.challenge_for(Some(wallet), None, 0).unwrap();
        assert_eq!(c.vdf_input, mining_seed(&parent_won, 7), "sanity: challenge_for tracks the live tip");
        let sub = solve_for(&c, &wallet);

        // Chain advances past height 7 (the height-7 submission is now "behind
        // the frontier" and must go through tip_at(), not the exact-match path).
        b.publish_tip(8, parent_won);

        // The work was valid -- solved against the parent that actually won and
        // that height 8 actually extends. It must be accepted.
        let outcome = b.submit(&sub);
        assert_eq!(
            outcome,
            SubmitOutcome::Block { height: 7 },
            "SECURITY/CORRECTNESS: valid work against the winning candidate must not be \
             rejected as verify_mismatch just because a since-abandoned candidate was \
             published at the same height first -- got {outcome:?}"
        );
    }

    /// PROOF OF MECHANISM #2 (2026-08-25, continued verify_mismatch investigation):
    /// even with `tip_at` fixed, a genuinely valid SHARE-level submission that
    /// arrives one height late (`historical == true`) is STILL always rejected as
    /// verify_mismatch. The share-credit branch in `submit()` is gated
    /// `if !historical && ...` -- so a late share is checked ONLY against the much
    /// harder full-block target (which it was never meant to clear), fails that,
    /// and then the share path is skipped entirely rather than attempted. This is
    /// not a "which tip" bug like the first one -- it's a real code path that
    /// never even tries to credit a late share, no matter how valid the work is.
    /// Live evidence: on the real chain right now, height advances roughly every
    /// 100-150ms, so ANY submission with real network/compute latency routinely
    /// arrives at least one height behind -- making this the common case, not an
    /// edge case, which matches the sustained ~30-40% live rejection rate.
    #[test]
    fn a_late_share_is_wrongly_rejected_instead_of_credited() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Real pool config shape: a hard block target (bits) with share ease
        // widening the share target well beyond it, same as production.
        std::env::set_var("SIGIL_MINING_BLAKE4_BITS", "8");
        std::env::set_var("SIGIL_MINING_VDF_T", "8");
        std::env::set_var("SIGIL_SHARE_VDF_T", "2");
        std::env::set_var("SIGIL_SHARE_EASE_BITS", "20");
        let wallet: WalletId = [0xEEu8; 32];
        let parent = [0x33u8; 32];
        let b = MiningBridge::new();
        b.publish_tip(9, parent);

        let c = b.challenge_for(Some(wallet), None, 0).unwrap();
        assert!(c.share_target > c.blake4_target, "sanity: share target must be easier");

        // Mine to the SHARE target specifically (not the block target) -- real
        // honest share-grade work, exactly what a pool miner submits far more
        // often than a full block win.
        let g = ModSquaring::bench_2048();
        let wallet_hex = hex::encode(wallet);
        let header = flux_miner::client::build_header(&c, &wallet_hex);
        let block = flux_miner::mine_dual(&header, c.share_target, c.share_vdf_t, &g);
        assert!(
            block.blake4_hash > c.blake4_target,
            "sanity: this must be share-grade work, not an accidental block win"
        );
        let sub = Submission { height: c.height, wallet: wallet_hex, block };

        // The frontier advances past height 9 before this share is submitted --
        // completely normal under real network/compute latency, not an edge case.
        b.publish_tip(10, parent);

        let outcome = b.submit(&sub);
        assert!(
            matches!(outcome, SubmitOutcome::Share { height: 9, .. }),
            "CORRECTNESS: genuinely valid share-grade work must be credited even one \
             height late -- got {outcome:?} instead of a Share outcome"
        );
    }

    #[test]
    fn valid_solve_is_accepted_and_queued_for_the_producer() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let wallet: WalletId = [0xABu8; 32];
        let b = bridge_at(3, [0x99u8; 32]);
        let c = b.challenge_for(Some(wallet), None, 0).unwrap();
        let sub = solve_for(&c, &wallet);

        assert_eq!(b.submit(&sub), SubmitOutcome::Block { height: 3 });
        let got = b.take_solve().expect("the producer finds a solve waiting");
        assert_eq!(got.wallet, wallet);
        assert_eq!(got.height, 3);
        assert_eq!(got.nonce, sub.block.nonce);
        assert!(b.take_solve().is_none(), "the queue drains");
    }

    #[test]
    fn a_mined_header_is_reverifiable_by_a_follower() {
        // The whole point: a follower holding ONLY the header can rebuild the
        // challenge and check both lanes.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let wallet: WalletId = [0x5Au8; 32];
        let parent = [0x77u8; 32];
        let b = bridge_at(11, parent);
        let c = b.challenge_for(Some(wallet), None, 0).unwrap();
        let sub = solve_for(&c, &wallet);
        assert!(matches!(b.submit(&sub), SubmitOutcome::Block { .. }));
        let s = b.take_solve().unwrap();

        let carrier = pack_nonce_carrier(s.nonce, s.blake4_hash);
        assert!(
            verify_header_pow(&parent, 11, &wallet, &carrier, &s.vdf, s.bits, c.vdf_t),
            "header material alone must re-verify the work"
        );

        // Tampering with any bound field breaks it.
        assert!(!verify_header_pow(&[0x00u8; 32], 11, &wallet, &carrier, &s.vdf, s.bits, c.vdf_t),
            "a different parent is different work");
        assert!(!verify_header_pow(&parent, 12, &wallet, &carrier, &s.vdf, s.bits, c.vdf_t),
            "a different height is different work");
        assert!(!verify_header_pow(&parent, 11, &[0x01u8; 32], &carrier, &s.vdf, s.bits, c.vdf_t),
            "a share cannot be re-pointed at another miner");
        let bad = pack_nonce_carrier(s.nonce.wrapping_add(1), s.blake4_hash);
        assert!(!verify_header_pow(&parent, 11, &wallet, &bad, &s.vdf, s.bits, c.vdf_t),
            "a forged nonce fails Lane A");
    }

    #[test]
    fn stale_height_and_replay_are_rejected() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let wallet: WalletId = [0x0Fu8; 32];
        let b = bridge_at(5, [0x42u8; 32]);
        let c = b.challenge_for(Some(wallet), None, 0).unwrap();
        let sub = solve_for(&c, &wallet);

        // replay of an accepted solve — rejected regardless of the credit
        // window, at the SAME height or any later one (seen is keyed by
        // height now, but this exact (wallet, nonce, height) triple was
        // already inserted and is still within the window's retention).
        assert!(matches!(b.submit(&sub), SubmitOutcome::Block { .. }));
        assert!(matches!(
            b.submit(&sub),
            SubmitOutcome::Rejected { kind: RejectKind::Duplicate, .. }
        ));
        b.publish_tip(6, [0x43u8; 32]);
        assert!(
            matches!(b.submit(&sub), SubmitOutcome::Rejected { kind: RejectKind::Duplicate, .. }),
            "the SAME solve replayed after the frontier moves must still be a duplicate, not credited twice"
        );
    }

    #[test]
    fn seen_is_pruned_by_height_key_not_left_to_grow_unbounded() {
        // 2026-08-21/22 (the "production crawled to ~1 block/30s" incident):
        // `seen` used to be pruned with a flat retain() that had to visit
        // every entry regardless of how many actually aged out — an O(total
        // submissions in the window) cost on every single tip advance, which
        // under real load became the dominant CPU cost (measured live via
        // perf: 32%+ in raw memcmp) and directly stalled block production
        // (publish_tip runs inline in the producer's own tick). Now indexed
        // by height first, so eviction only ever touches the height KEYS
        // being dropped. This test proves the height-keyed map actually
        // shrinks on prune — not just that duplicate-detection still works
        // (already covered by stale_height_and_replay_are_rejected above).
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let b = bridge_at(1, [0x00u8; 32]);
        // Submit one real solve at each of several early heights so `seen`
        // accumulates real height-keyed entries to prune.
        for h in 1..=5u64 {
            let wallet: WalletId = [h as u8; 32];
            let c = b.challenge_for(Some(wallet), None, 0).unwrap();
            let sub = solve_for(&c, &wallet);
            assert!(matches!(b.submit(&sub), SubmitOutcome::Block { .. }));
            b.publish_tip(h + 1, [(h + 1) as u8; 32]);
        }
        let before = b.seen.lock().unwrap().len();
        assert!(before >= 5, "expected at least the 5 submitted heights still tracked, got {before}");

        // Advance far past credit_window() (default 20) so every one of
        // those early heights ages fully out.
        b.publish_tip(1 + credit_window() + 50, [0xEEu8; 32]);
        let after = b.seen.lock().unwrap().len();
        assert!(
            after < before,
            "seen must actually shrink once heights age out of the credit window \
             (before={before}, after={after}) — a map that never shrinks is the \
             exact unbounded-growth bug this fix closes"
        );
    }

    #[test]
    fn a_fresh_near_miss_within_the_credit_window_still_wins() {
        // The actual point of the 2026-08-16 widening: a DIFFERENT (fresh
        // nonce) solve for a height a few blocks behind the current frontier
        // must still be credited, not discarded — this is what "93.8% of
        // supply went to the placeholder wallet" was actually fixing.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let wallet: WalletId = [0x11u8; 32];
        let b = bridge_at(5, [0x42u8; 32]);
        let c = b.challenge_for(Some(wallet), None, 0).unwrap();
        let sub = solve_for(&c, &wallet); // solved against height 5's real challenge

        // Frontier moves on 3 times before this submission arrives — well
        // within the default 20-block credit_window().
        b.publish_tip(6, [0x43u8; 32]);
        b.publish_tip(7, [0x44u8; 32]);
        b.publish_tip(8, [0x45u8; 32]);

        match b.submit(&sub) {
            SubmitOutcome::Block { height } => assert_eq!(height, 5, "credited AT the height it was solved for"),
            other => panic!("a near-miss within the credit window must still win, got {other:?}"),
        }
        let got = b.take_solve().expect("the producer finds the near-miss solve waiting");
        assert_eq!(got.wallet, wallet, "the real miner is credited, not the producer-wallet fallback");
    }

    #[test]
    fn a_solve_outside_the_credit_window_is_genuinely_stale() {
        // The widening has a real edge, not an unbounded amnesty: once a
        // height falls out of BOTH the window and the retained history, it
        // must still reject — this is what stops the replay/DoS surface a
        // truly unlimited window would open.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SIGIL_MINING_CREDIT_WINDOW", "2");
        let wallet: WalletId = [0x22u8; 32];
        let b = bridge_at(5, [0x42u8; 32]);
        let c = b.challenge_for(Some(wallet), None, 0).unwrap();
        let sub = solve_for(&c, &wallet);

        // 4 blocks later, window is 2 -> outside it.
        for (h, p) in [(6, 0x43u8), (7, 0x44), (8, 0x45), (9, 0x46)] {
            b.publish_tip(h, [p; 32]);
        }
        assert!(
            matches!(b.submit(&sub), SubmitOutcome::Rejected { kind: RejectKind::StaleHeight, .. }),
            "4 blocks behind must reject when the configured window is only 2"
        );

        std::env::remove_var("SIGIL_MINING_CREDIT_WINDOW");
    }

    #[test]
    fn non_canonical_wallet_is_rejected_with_a_fixable_reason() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let wallet: WalletId = [0xC3u8; 32];
        let b = bridge_at(2, [0x01u8; 32]);
        let c = b.challenge_for(Some(wallet), None, 0).unwrap();
        let mut sub = solve_for(&c, &wallet);
        sub.wallet = hex::encode(wallet).to_uppercase();

        match b.submit(&sub) {
            SubmitOutcome::Rejected { kind, detail } => {
                assert_eq!(kind, RejectKind::NonCanonicalWallet);
                assert!(detail.contains("lowercase"), "the reason says how to fix it");
            }
            other => panic!("uppercase wallet must be rejected, got {other:?}"),
        }
    }

    #[test]
    fn garbage_is_rejected_without_consuming_the_replay_slot() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let wallet: WalletId = [0x7Eu8; 32];
        let b = bridge_at(4, [0x55u8; 32]);
        let c = b.challenge_for(Some(wallet), None, 0).unwrap();
        let good = solve_for(&c, &wallet);

        // same nonce, broken proof
        let mut forged = good.clone();
        forged.block.blake4_hash = 0;
        assert!(matches!(
            b.submit(&forged),
            SubmitOutcome::Rejected { kind: RejectKind::VerifyMismatch, .. }
        ));
        // the honest solve carrying that nonce still lands
        assert!(matches!(b.submit(&good), SubmitOutcome::Block { .. }));
    }

    #[test]
    fn no_frontier_means_no_mining() {
        let b = MiningBridge::new();
        assert!(b.challenge_for(None, None, 0).is_none());
        let sub = Submission {
            height: 0,
            wallet: hex::encode([0u8; 32]),
            block: DualLaneBlock {
                header: vec![],
                nonce: 0,
                blake4_hash: 0,
                vdf: VdfProof { y: vec![], pi: vec![], t: 0 },
            },
        };
        assert!(matches!(
            b.submit(&sub),
            SubmitOutcome::Rejected { kind: RejectKind::NoTip, .. }
        ));
    }

    #[test]
    fn net_hps_sums_live_miners_and_prunes_idle_ones() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let b = bridge_at(1, [0u8; 32]);
        b.report_hps(Some([1u8; 32]), None, Some(1000.0), 0);
        b.report_hps(Some([2u8; 32]), None, Some(500.0), 0);
        assert_eq!(b.challenge_for(None, None, 0).unwrap().net_hps, 1500.0);

        // one keeps reporting, the other goes silent past the idle window
        let later = HPS_IDLE_MS + 1;
        b.report_hps(Some([1u8; 32]), None, Some(1000.0), later);
        assert_eq!(b.challenge_for(None, None, later).unwrap().net_hps, 1000.0);
    }

    /// THE MULTI-RIG GATE (2026-08-24). Two DIFFERENT rigs mining to the SAME
    /// wallet must both count toward `net_hps` — this is the exact bug the
    /// operator hit live (two rigs at 1 GH/s+ combined, pool showed ~450
    /// MH/s). The second half of this test (same rig id reporting twice)
    /// demonstrates the OLD, still-intentional clobbering behavior a repeat
    /// report from the SAME rig must have (a rig re-polling isn't a second
    /// rig) — proving the fix is keyed on rig identity, not just "never
    /// overwrite anything".
    #[test]
    fn two_rigs_on_one_wallet_both_count_toward_net_hps() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let b = bridge_at(1, [0u8; 32]);
        let wallet = [7u8; 32];

        // Two distinct rigs, same wallet, same instant.
        b.report_hps(Some(wallet), Some("rig-a".into()), Some(600_000_000.0), 0);
        b.report_hps(Some(wallet), Some("rig-b".into()), Some(500_000_000.0), 0);
        assert_eq!(
            b.challenge_for(Some(wallet), None, 0).unwrap().net_hps,
            1_100_000_000.0,
            "SECURITY/CORRECTNESS: two distinct rigs on one wallet must SUM, not clobber \
             each other — this is the exact live-reported bug (1 GH/s+ real, ~450 MH/s shown)"
        );

        // The wallet's OWN total (the human-facing 'my hashrate' readback) must also
        // reflect both rigs, not just whichever reported last.
        assert_eq!(b.hps_for_wallet_total(Some(wallet)), 1_100_000_000.0);

        // But vardiff issued to ONE rig must be sized to THAT rig's own rate, not the
        // wallet's combined total — else each rig is calibrated for 2x its real speed
        // and undershoots the intended shares/sec. Check this directly against the
        // per-rig/per-wallet accessors (not via `share_target`, which saturates at
        // this fixture's tiny test difficulty and would hide the distinction).
        assert_eq!(
            b.hps_for_rig(Some(wallet), Some("rig-a")), 600_000_000.0,
            "rig-a's OWN rate must read back as its own 600 MH/s report"
        );
        assert_eq!(
            b.hps_for_rig(Some(wallet), Some("rig-b")), 500_000_000.0,
            "rig-b's OWN rate must read back as its own 500 MH/s report, independent of rig-a"
        );
        assert_ne!(
            b.hps_for_rig(Some(wallet), Some("rig-a")),
            b.hps_for_wallet_total(Some(wallet)),
            "SECURITY/CORRECTNESS: a single rig's own rate must not silently equal the \
             wallet's combined total, or vardiff calibration for that rig regresses to \
             double its real speed"
        );

        // A REPEAT report from the SAME rig id is a re-poll, not a new rig — it must
        // still replace (not add to) that rig's own entry.
        b.report_hps(Some(wallet), Some("rig-a".into()), Some(650_000_000.0), 1);
        assert_eq!(
            b.challenge_for(Some(wallet), None, 1).unwrap().net_hps,
            1_150_000_000.0, // 650 (updated rig-a) + 500 (rig-b, still live)
            "a repeat report from the SAME rig id must UPDATE that rig's entry, not add a third"
        );
    }

    // ── vardiff (2026-08-16 port from sigil_rpc::vardiff_ease_for; 2026-08-21
    //    two-pass fix — see `share_target_for`'s doc comment for the full
    //    story, including the SECOND correction after the first pass shipped
    //    a live regression) ──────────────────────────────────────────────
    //
    // Cases below the operator ceiling / below `bits-1` match the original
    // ported formula's vectors exactly. Above that, the target is now
    // deliberately clamped at `bits-1` (numerically identical to the
    // original ease=1 saturation) — proven load-bearing, not a bug: the
    // client's pool-vs-solo detection is `share_target > blake4_target`,
    // so anything at or past `bits` silently reverts the miner to
    // solo-mode discipline.

    #[test]
    fn share_target_formula_matches_unsaturated_region_of_the_proven_vectors() {
        // Unaffected: same target either side of the fix.
        assert_eq!(share_target_for(0.0, 0.5, 24, 8), target_from_bits(16)); // unknown -> ceiling
        assert_eq!(share_target_for(1.0, 0.5, 24, 8), target_from_bits(16)); // <=1 H/s -> ceiling
        assert_eq!(share_target_for(3e9, 0.5, 24, 0), 0); // ceiling 0 -> always 0 (solo semantics)
        assert_eq!(share_target_for(100e3, 0.5, 24, 8), target_from_bits(18)); // below crossover
        assert_eq!(share_target_for(3e9, 0.5, 35, 8), target_from_bits(33)); // below crossover
    }

    #[test]
    fn share_target_never_reaches_the_block_target_no_matter_how_high_hps_goes() {
        // The load-bearing invariant: `flux_miner::engine::mining_loop`
        // decides pool-vs-solo purely from `share_target > blake4_target`.
        // A first version of this fix let `share_bits` grow past `bits`
        // for any hps above ~33 kH/s at bits=16, which made share_target
        // <= blake4_target and silently dropped real miners into solo
        // mode — catastrophic at slow block cadence (operator-reported:
        // hashrate fell further, to ~3.68 kH/s, AFTER that "fix" shipped).
        // This must hold for every hps, however absurd.
        let bits = 16;
        let blake4_target = target_from_bits(bits);
        for hps in [10e3, 33e3, 100e3, 10e6, 3e9, 1e15] {
            let t = share_target_for(hps, 0.5, bits, 8);
            assert!(
                t > blake4_target,
                "hps={hps} produced share_target={t} <= blake4_target={blake4_target} — \
                 this flips the client into solo mode"
            );
        }
        // And it must saturate at the SAME hardest value for every hps past
        // the crossover (bits-1) — proven correct, not a regression: there
        // is no share target harder than bits-1 that still satisfies the
        // invariant above, so every sufficiently fast miner necessarily
        // converges on the same floor.
        let t_10e6 = share_target_for(10e6, 0.5, bits, 8);
        let t_3e9 = share_target_for(3e9, 0.5, bits, 8);
        assert_eq!(t_10e6, target_from_bits(bits - 1));
        assert_eq!(t_3e9, target_from_bits(bits - 1));
        assert_eq!(t_10e6, t_3e9);
    }

    #[test]
    fn share_target_never_falls_below_the_operator_ceiling() {
        // No hps, however small, should be issued something EASIER than the
        // flat ceiling — that direction of the formula was never broken and
        // must stay pinned.
        for hps in [2.0, 10.0, 1_000.0, 32_000.0] {
            let t = share_target_for(hps, 0.5, 16, 8);
            assert!(t <= target_from_bits(8), "hps={hps} got an easier-than-ceiling target");
        }
    }

    /// THE 2026-08-24 FIX, proven before deploying it as a config change.
    ///
    /// `share_target_never_reaches_the_block_target_no_matter_how_high_hps_goes`
    /// (above) already proves — and pins as CORRECT, not a bug — that at
    /// `bits=16` every wallet above ~33 kH/s saturates at the SAME `bits-1`
    /// floor. That saturation is unavoidable at bits=16 given the pool/solo
    /// invariant; it isn't wrong, the INPUT was just too small for today's
    /// real hashrates (an RTX 2080-class GPU is comfortably 100-1000x past
    /// the crossover). This test proves the actual fix — raising BOTH
    /// `blake4_bits` (for headroom at the hard end) AND `share_ease_bits` by
    /// the SAME amount (so the easy floor for a brand-new/slow/idle miner
    /// stays at the identical absolute difficulty it is today) — restores
    /// real differentiation across the whole realistic range, from a slow
    /// CPU up through a generously-estimated multi-GPU rig, with nobody
    /// clamped to the same floor as everyone else.
    #[test]
    fn raising_bits_and_ease_together_restores_headroom_without_moving_the_easy_floor() {
        const NEW_BITS: u32 = 40;
        const NEW_EASE: u32 = 32; // bits - ease = 8, unchanged from today's live 16-8=8
        let rate = 0.5; // SIGIL_VARDIFF_RATE default, unchanged by this fix

        // The easy floor for an idle/brand-new wallet is BYTE-IDENTICAL to
        // today's live config (16 bits, ease 8) — nobody who could barely
        // land a share before gets locked out by this change.
        assert_eq!(
            share_target_for(0.0, rate, NEW_BITS, NEW_EASE),
            share_target_for(0.0, rate, 16, 8),
            "the easy floor for an unreporting wallet must not move"
        );

        // The realistic range: Viktor's own CURRENTLY-DEFLATED self-report
        // (4.77 MH/s, itself an artifact of the bits=16 clamp — see the
        // module-level incident note) through a deliberately generous
        // multi-GPU estimate (100 GH/s), none of it may hit the bits-1=39
        // ceiling — hitting it would mean this fix didn't move the crossover
        // far enough for real hardware.
        let hardest = target_from_bits(NEW_BITS - 1);
        for hps in [4.77e6, 70e6, 563e6, 5e9, 100e9] {
            let t = share_target_for(hps, rate, NEW_BITS, NEW_EASE);
            assert_ne!(
                t, hardest,
                "hps={hps} still saturates at the new ceiling — {NEW_BITS} needs to go higher"
            );
        }

        // And the invariant the OTHER two tests exist to guard is still
        // upheld at the new bits value: share_target must stay strictly
        // easier than the real block target for every hps, however absurd,
        // or the client silently falls back to solo mode.
        let blake4_target = target_from_bits(NEW_BITS);
        for hps in [10e3, 33e3, 100e3, 10e6, 3e9, 1e15] {
            let t = share_target_for(hps, rate, NEW_BITS, NEW_EASE);
            assert!(t > blake4_target, "hps={hps} produced share_target={t} <= blake4_target={blake4_target}");
        }
    }

    #[test]
    fn a_reporting_gpu_gets_an_easier_target_than_an_unknown_wallet() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // bridge_at() itself pins SIGIL_SHARE_EASE_BITS=0 (its "trivial and
        // deterministic" default) — override AFTER constructing the bridge,
        // not before, or bridge_at's own set_var clobbers ours right back to 0.
        let b = bridge_at(1, [0u8; 32]);
        std::env::set_var("SIGIL_SHARE_EASE_BITS", "8");
        std::env::set_var("SIGIL_VARDIFF_RATE", "0.5");
        let gpu: WalletId = [0xAAu8; 32];
        let unknown: WalletId = [0xBBu8; 32];

        // GPU has told us it does 100 MH/s; the unknown wallet has never
        // reported anything.
        b.report_hps(Some(gpu), None, Some(100_000_000.0), 0);

        let gpu_target = b.challenge_for(Some(gpu), None, 0).unwrap().share_target;
        let unknown_target = b.challenge_for(Some(unknown), None, 0).unwrap().share_target;

        // A SMALLER target is a HARDER target (fewer hashes clear the bar).
        // Before this fix both were issued the exact same flat share_target
        // regardless of hps — this assertion is what "per-wallet vardiff"
        // means and would fail against the old flat-ease code.
        assert!(
            gpu_target < unknown_target,
            "a rig reporting real hashrate should get a target scaled to it \
             (harder / smaller), not the same flat ceiling as an unknown wallet: \
             gpu_target={gpu_target} unknown_target={unknown_target}"
        );

        std::env::remove_var("SIGIL_SHARE_EASE_BITS");
        std::env::remove_var("SIGIL_VARDIFF_RATE");
    }

    #[test]
    fn solo_semantics_are_unchanged_when_share_ease_bits_is_zero() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Default operator config (SIGIL_SHARE_EASE_BITS unset -> 0): vardiff
        // must be a no-op and share_target must stay 0 for every wallet,
        // exactly the pre-vardiff wire behaviour this module documents.
        let b = bridge_at(1, [0u8; 32]);
        let gpu: WalletId = [0xCCu8; 32];
        b.report_hps(Some(gpu), None, Some(500_000_000.0), 0);
        assert_eq!(b.challenge_for(Some(gpu), None, 0).unwrap().share_target, 0);
        assert_eq!(b.challenge_for(None, None, 0).unwrap().share_target, 0);
    }

    // ── the 2026-08-19 stuck-behind-a-stale-solve fix ───────────────────────
    //
    // sigil-node's producer loop popped exactly ONE solve off this queue per
    // tick (`take_solve()`). If that one popped solve was stale, the tick
    // discarded it and checked NOTHING else — even when a fresher, perfectly
    // creditable solve was queued right behind it. Under real multi-miner
    // load with the braid's 16-125ms cadence this let queue backlog build up
    // and starve real miners of credit (live symptom: `queued_solves` growing
    // 5->7->8+ while the wallet's own balance stayed at exactly 0). The fix
    // (`take_creditable_solve` in sigil-node) scans forward past stale
    // entries instead of stopping at the first one. This test proves the
    // underlying MiningBridge queue actually supports that scan — i.e. that a
    // fresh solve queued behind a now-stale one is still reachable, just not
    // to a caller that only ever pops once — which is the property the fix
    // depends on.
    #[test]
    fn a_fresh_solve_queued_behind_a_now_stale_one_is_still_reachable_by_scanning() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let stale_wallet: WalletId = [0x33u8; 32];
        let fresh_wallet: WalletId = [0x44u8; 32];
        let b = bridge_at(5, [0x42u8; 32]);

        // A solve for height 5, valid and queued at submit time.
        let c = b.challenge_for(Some(stale_wallet), None, 0).unwrap();
        let stale_sub = solve_for(&c, &stale_wallet);
        assert!(matches!(b.submit(&stale_sub), SubmitOutcome::Block { height: 5 }));

        // The frontier races far ahead (25 blocks — beyond the default
        // 20-block credit_window()) WITHOUT the producer ever draining the
        // queue, exactly like a backlog building up under load. The queued
        // solve above is now stale relative to the new tip, but it is still
        // sitting at the FRONT of the FIFO queue — nobody has popped it yet.
        let mut parent = [0x42u8; 32];
        for h in 6..=30u64 {
            parent[0] = h as u8;
            b.publish_tip(h, parent);
        }
        assert!(30u64.saturating_sub(5) > credit_window(), "test setup must actually exceed the window");

        // A fresh solve lands for the CURRENT height and queues in behind
        // the stale one (FIFO: push_back).
        let c2 = b.challenge_for(Some(fresh_wallet), None, 0).unwrap();
        let fresh_sub = solve_for(&c2, &fresh_wallet);
        assert!(matches!(b.submit(&fresh_sub), SubmitOutcome::Block { height: 30 }));

        // A single take_solve() call — the OLD producer's entire per-tick
        // budget — surfaces the stale entry first. This is exactly the
        // queue state the live bug hit: a caller that discards this pop as
        // uncreditable and stops (the old code) would report "no creditable
        // work this tick", even though a fresh, perfectly good solve is
        // sitting right behind it.
        let first = b.take_solve().expect("the stale solve is still queued");
        assert_eq!(first.wallet, stale_wallet);
        assert!(
            30u64.saturating_sub(first.height) > credit_window(),
            "confirm this popped entry really is outside the credit window"
        );

        // The fix's whole premise: keep popping (bounded) instead of
        // stopping. The fresh solve was never lost — only queued behind the
        // stale one — and a second pop reaches it intact.
        let second = b.take_solve().expect("the fresh solve is reachable behind the stale one");
        assert_eq!(
            second.wallet, fresh_wallet,
            "the fresh, creditable solve was not lost — just queued behind the stale one, \
             and scanning past the stale front entry (not stopping at it) reaches it"
        );
        assert_eq!(second.height, 30);
    }
}

#[cfg(test)]
mod dynamic_bits_tests {
    use super::*;

    /// Rewind `last_eval` far into the past so the NEXT `dynamic_bits()` call
    /// treats the retarget interval as elapsed — deterministic, no real
    /// sleeping, no env-var mutation (which would be unsafe under parallel
    /// test execution).
    fn force_next_eval(bridge: &MiningBridge) {
        if let Ok(mut g) = bridge.auto_bits.lock() {
            if let Some((bits, _)) = *g {
                *g = Some((bits, 0));
            }
        }
    }

    #[test]
    fn ideal_bits_matches_the_expected_attempts_formula() {
        // 2^20 attempts at 1,000,000 hashes/sec takes exactly 1.048576s.
        assert_eq!(ideal_bits_for(1_000_000.0, 1.048576), Some(20));
        // Doubling hashrate for the same target time must raise ideal bits by ~1
        // (2x attempts/sec means half the target time per attempt-count, i.e.
        // one more doubling of the attempt space fits in the same wall clock).
        assert_eq!(ideal_bits_for(2_000_000.0, 1.048576), Some(21));
        assert_eq!(ideal_bits_for(0.0, 120.0), None, "zero hashrate has nothing to compute from");
        assert_eq!(ideal_bits_for(-5.0, 120.0), None, "negative hashrate is nonsensical, not zero-clamped");
        assert_eq!(ideal_bits_for(1_000_000.0, 0.0), None, "zero target time is nonsensical, not infinite bits");
    }

    /// THE INCIDENT, REPRODUCED AND SOLVED. Real numbers: bits stuck at 40
    /// (this morning's actual manual change), real measured live network
    /// hashrate ~555 MH/s, the default 120s target. Proves the algorithm
    /// converges DOWN from the broken value to the analytically-correct one,
    /// in bounded steps, without needing a human to notice and intervene.
    #[test]
    fn recovers_from_todays_actual_incident_live_hashrate() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let b = MiningBridge::new();
        std::env::remove_var("SIGIL_MINING_BLAKE4_BITS");
        b.report_hps(Some([7u8; 32]), Some("rig-a".into()), Some(555_000_000.0), now_ms_mining());
        *b.auto_bits.lock().unwrap() = Some((40, 0)); // today's broken value

        let ideal = ideal_bits_for(555_000_000.0, target_win_secs()).unwrap();
        assert!(
            ideal < 40,
            "555 MH/s at a {}s target must want an EASIER (lower-bits) target than the broken 40, got ideal={ideal}",
            target_win_secs()
        );

        let mut last = 40i64;
        let mut converged = false;
        for _ in 0..10 {
            force_next_eval(&b);
            let next = b.dynamic_bits() as i64;
            assert!(
                (next - last).abs() <= MAX_STEP_BITS,
                "must never move more than {MAX_STEP_BITS} bits in one evaluation, got {last}->{next}"
            );
            last = next;
            if last == ideal as i64 {
                converged = true;
                break;
            }
        }
        assert!(converged, "must converge to the analytically-ideal bits ({ideal}) within 10 bounded steps, stalled at {last}");
        assert!(last < 40, "must have moved strictly easier than the broken starting value");
    }

    /// Viktor's own description of what actually happened before today's
    /// incident: hashrate dropped, an operator adjusted difficulty down to
    /// compensate, hashrate came back. Proves the algorithm does that
    /// adjustment BY ITSELF, in both directions, without a human in the loop.
    #[test]
    fn tracks_a_hashrate_drop_and_recovery() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let b = MiningBridge::new();
        std::env::remove_var("SIGIL_MINING_BLAKE4_BITS");
        let wallet = [9u8; 32];
        let healthy_hps = 500_000_000.0;

        b.report_hps(Some(wallet), Some("rig-a".into()), Some(healthy_hps), now_ms_mining());
        *b.auto_bits.lock().unwrap() = Some((16, 0)); // cold-start default
        let mut bits = 16u32;
        for _ in 0..20 {
            force_next_eval(&b);
            bits = b.dynamic_bits();
        }
        let healthy_ideal = ideal_bits_for(healthy_hps, target_win_secs()).unwrap();
        assert_eq!(bits, healthy_ideal, "must converge UP to the healthy target from a cold-start 16");

        // Hashrate collapses.
        let dropped_hps = 5_000_000.0;
        b.report_hps(Some(wallet), Some("rig-a".into()), Some(dropped_hps), now_ms_mining());
        for _ in 0..20 {
            force_next_eval(&b);
            bits = b.dynamic_bits();
        }
        let dropped_ideal = ideal_bits_for(dropped_hps, target_win_secs()).unwrap();
        assert_eq!(bits, dropped_ideal);
        assert!(bits < healthy_ideal, "difficulty must ease off when hashrate genuinely drops, or blocks stall out");

        // Hashrate recovers.
        b.report_hps(Some(wallet), Some("rig-a".into()), Some(healthy_hps), now_ms_mining());
        for _ in 0..20 {
            force_next_eval(&b);
            bits = b.dynamic_bits();
        }
        assert_eq!(bits, healthy_ideal, "difficulty must climb back once hashrate genuinely recovers, not stay stuck easy");
    }

    /// The manual-override escape hatch must still work exactly as before —
    /// an explicit env var pins `bits` forever, auto-retargeting never touches it.
    #[test]
    fn pinned_env_var_disables_auto_retargeting_entirely() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SIGIL_MINING_BLAKE4_BITS", "27");
        let b = MiningBridge::new();
        b.report_hps(Some([1u8; 32]), Some("r".into()), Some(999_000_000.0), now_ms_mining());
        for _ in 0..5 {
            force_next_eval(&b);
            assert_eq!(b.dynamic_bits(), 27, "a pinned bits value must never move, however hashrate changes");
        }
        std::env::remove_var("SIGIL_MINING_BLAKE4_BITS");
    }

    /// Cold start with genuinely zero live data must seed from the documented
    /// default, never compute a wild value from an absence of information.
    #[test]
    fn cold_start_seeds_from_the_default_not_a_guess() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("SIGIL_MINING_BLAKE4_BITS");
        let b = MiningBridge::new();
        assert_eq!(b.dynamic_bits(), 16, "with zero live data, must seed from the default, not guess");
    }

    /// Bounds must hold even against an absurd hashrate input — retargeting
    /// itself into "impossible" is exactly the failure mode this exists to end.
    #[test]
    fn never_leaves_the_configured_bounds_however_extreme_the_hashrate() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("SIGIL_MINING_BLAKE4_BITS");
        let b = MiningBridge::new();
        b.report_hps(Some([2u8; 32]), Some("r".into()), Some(1.0e15), now_ms_mining());
        *b.auto_bits.lock().unwrap() = Some((16, 0));
        let mut bits = 16u32;
        for _ in 0..200 {
            force_next_eval(&b);
            bits = b.dynamic_bits();
            assert!(bits >= min_bits() && bits <= max_bits(), "bits {bits} left the configured [{}, {}] bounds", min_bits(), max_bits());
        }
        assert_eq!(bits, max_bits(), "an absurd hashrate should saturate at the ceiling, not exceed it");
    }
}
