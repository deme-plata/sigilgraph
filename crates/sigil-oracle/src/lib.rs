//! sigil-oracle — the committed-in-roots SIGIL price feed.
//!
//! The price (USD per 1 SIGIL, fixed-point — `1e8` = $1.00) lives in a contract
//! storage slot, so it is committed in `contract_state_root` like every other
//! state write — there is NO separate, mutable oracle state to drift or be
//! blindly overwritten (the Quillon-postmortem discipline). Every update goes
//! through `commit_state_transition`. USDS reads this price to mint/redeem at peg.
//!
//! ## Who may push (2026-09-11, the "USDS live" change)
//!
//! The genesis [`ORACLE_AUTHORITY`] placeholder (`[0x0A;32]`) has no keyholder,
//! so `sigil-tx` accepts `OraclePush` from the state-committed **master wallet**
//! (Viktor's dev-fee wallet). That made the price feed a thing only the operator
//! could refresh by hand — a stablecoin whose peg waits for a human to tap a
//! phone is not live. So the master may now **delegate**: one master-signed
//! `OracleDelegate` writes the feeder's wallet into [`FEEDER_SLOT`], and from
//! `sigil_usds::USDS_LIVE_HEIGHT` on, `OraclePush` is accepted from the master
//! OR that feeder. The delegation is state-committed (it rides
//! `contract_state_root`), so every node agrees on who the feeder is — never an
//! env var, which is how two nodes come to disagree about validity.
//!
//! ## Freshness (same change)
//!
//! A price that nobody has refreshed is a price that no longer describes the
//! market, and minting USDS against it is minting against a number, not a
//! value. Every push after activation ALSO commits the height it landed at
//! ([`PRICE_HEIGHT_SLOT`]); `sigil-usds` refuses to mint, redeem or pay welfare
//! when the committed price is older than [`MAX_PRICE_AGE_BLOCKS`]. A dead
//! feeder therefore freezes the stablecoin instead of mispricing it — fail
//! closed, the same posture every other money path on this chain takes.
//!
//! Units: `PRICE_SCALE` = USD×1e8 per WHOLE SIGIL. Heights are block heights.

use sigil_state::{
    commit_state_transition, CommitError, ContractId, SigilState, SlotId, StateMutation,
    StateTransition, WalletId,
};

/// The oracle contract's address (storage namespace for the feed).
pub const ORACLE_CONTRACT: ContractId = [0x0C; 32];
/// Slot holding the current SIGIL price.
pub const PRICE_SLOT: SlotId = [0x01; 32];
/// Slot holding the wallet the master has delegated price pushes to
/// (all-zero = nobody delegated). Written only by a master-signed
/// `SigilTx::OracleDelegate`.
pub const FEEDER_SLOT: SlotId = [0x02; 32];
/// Slot holding the block height of the most recent price push (LE u64 in
/// the first 8 bytes; all-zero = never pushed under the height-stamping
/// rule, which `price_is_fresh` treats as STALE on purpose).
pub const PRICE_HEIGHT_SLOT: SlotId = [0x03; 32];
/// The single wallet permitted to push prices (genesis-pinned, DNS-anchorable).
pub const ORACLE_AUTHORITY: WalletId = [0x0A; 32];
/// Fixed-point scale: price is USD×1e8 per 1 SIGIL. `100_000_000` == $1.00.
pub const PRICE_SCALE: u128 = 100_000_000;
/// How old (in blocks) a committed price may be before USDS refuses to use it.
/// 40,000 blocks ≈ 1.7 h at the 6.6 blk/s measured on g2 (2026-09-02). The
/// feeder re-pushes far more often than that (every ~10 min, and on every
/// ≥0.5 % move), so in normal operation this never binds; it binds exactly
/// when the feeder has died, which is when it should.
pub const MAX_PRICE_AGE_BLOCKS: u64 = 40_000;

#[derive(Debug, thiserror::Error)]
pub enum OracleError {
    #[error("only the pinned oracle authority may push prices")]
    Unauthorized,
    #[error("commit: {0}")]
    Commit(#[from] CommitError),
}

/// Encode a price into a 32-byte slot value — LE u128 in the first 16 bytes.
/// ONE encoder shared by the direct path and `sigil-tx`'s `OraclePush` arm,
/// so `read_price` sees exactly what either wrote.
pub fn encode_price(price: u128) -> [u8; 32] {
    let mut value = [0u8; 32];
    value[..16].copy_from_slice(&price.to_le_bytes());
    value
}

/// Encode a height into a 32-byte slot value — LE u64 in the first 8 bytes.
pub fn encode_height(height: u64) -> [u8; 32] {
    let mut value = [0u8; 32];
    value[..8].copy_from_slice(&height.to_le_bytes());
    value
}

/// The mutations one price push commits under the height-stamping rule:
/// the price itself AND the height it landed at. `sigil-tx` applies exactly
/// these for an `OraclePush` at or after `USDS_LIVE_HEIGHT`; before that it
/// writes only the price slot (the pre-activation shape, byte-identical to
/// what every node already computes for historical pushes).
pub fn price_mutations(price: u128, height: u64) -> [StateMutation; 2] {
    [
        StateMutation::SetContractSlot { contract: ORACLE_CONTRACT, slot: PRICE_SLOT, value: encode_price(price) },
        StateMutation::SetContractSlot { contract: ORACLE_CONTRACT, slot: PRICE_HEIGHT_SLOT, value: encode_height(height) },
    ]
}

/// The mutation a master-signed `OracleDelegate` commits: the feeder's wallet
/// into [`FEEDER_SLOT`]. Delegating to the all-zero wallet REVOKES (nobody
/// but the master may push again).
pub fn delegate_mutation(feeder: WalletId) -> StateMutation {
    StateMutation::SetContractSlot { contract: ORACLE_CONTRACT, slot: FEEDER_SLOT, value: feeder }
}

/// Push a new price (USD×1e8 per SIGIL) directly — the standalone/test path.
/// Authority-gated on the genesis placeholder; committed in
/// `contract_state_root` with the height stamp, exactly like an accepted
/// `OraclePush` after activation.
pub fn update_price(
    state: &mut SigilState,
    height: u64,
    feeder: WalletId,
    price: u128,
) -> Result<(), OracleError> {
    if feeder != ORACLE_AUTHORITY {
        return Err(OracleError::Unauthorized);
    }
    let t = StateTransition { at_height: height, mutations: price_mutations(price, height).to_vec() };
    commit_state_transition(state, &t, height)?;
    Ok(())
}

/// The committed price (USD×1e8 per SIGIL). `0` if never set.
pub fn read_price(state: &SigilState) -> u128 {
    let v = state.contract_slot(&ORACLE_CONTRACT, &PRICE_SLOT);
    let mut b = [0u8; 16];
    b.copy_from_slice(&v[..16]);
    u128::from_le_bytes(b)
}

/// Height of the most recent height-stamped push. `0` if the price was never
/// pushed under the stamping rule (including a price pushed by a pre-activation
/// binary, which is deliberately treated as stale).
pub fn read_price_height(state: &SigilState) -> u64 {
    let v = state.contract_slot(&ORACLE_CONTRACT, &PRICE_HEIGHT_SLOT);
    let mut b = [0u8; 8];
    b.copy_from_slice(&v[..8]);
    u64::from_le_bytes(b)
}

/// The wallet the master has delegated pushes to, if any.
pub fn read_feeder(state: &SigilState) -> Option<WalletId> {
    let v = state.contract_slot(&ORACLE_CONTRACT, &FEEDER_SLOT);
    if v == [0u8; 32] { None } else { Some(v) }
}

/// Is the committed price usable at `at_height`? Requires a non-zero price,
/// a height stamp, and `at_height - stamp <= MAX_PRICE_AGE_BLOCKS`. A stamp
/// in the FUTURE (only possible on a reorg/replay edge) is also refused —
/// a price from a block that has not happened is not a price.
pub fn price_is_fresh(state: &SigilState, at_height: u64) -> bool {
    let stamp = read_price_height(state);
    read_price(state) != 0
        && stamp != 0
        && stamp <= at_height
        && at_height - stamp <= MAX_PRICE_AGE_BLOCKS
}


// ═══════════════════════════════════════════════════════════════════════════════════════════
// GaugePush — Kristensen gauges (K⊕, K_bio, the breathing, the Lloyd ladder) as committed slots
// ═══════════════════════════════════════════════════════════════════════════════════════════
//
// 2026-09-15 (Viktor: "byg GaugePush"). The K gauges are computed off-chain by `sigil-earth`
// four times a day, signed, and anchored as shielded memos — on-chain, but as text nothing
// can compute against. A `GaugePush` writes a reading into a contract slot the same way
// `OraclePush` writes the price, so `contract_state_root` commits what Earth did and a
// contract (native today, WASM when `ContractCall` executes) can settle on it.
//
// One slot per feed holds `value_e6 ‖ reading_date ‖ height ‖ nonce`; a sibling slot holds
// the BLAKE3 digest of the `latest.json` the reading came from — the same digest the
// sigil-earth attest chain signs and anchors, so the two records name each other.
//
// Who may push: the master wallet or the delegated oracle FEEDER (`FEEDER_SLOT`) — the same
// trust root as the price. The chain does NOT verify the sigil-earth Ed25519 attest
// signature (the attest key is not state-committed); the feeder's authority is the consensus
// rule, the digest is the cross-check anyone can run against /v1/earth/attest.
//
// Activation: [`GAUGE_LIVE_HEIGHT`] is DORMANT (`u64::MAX`). A `GaugePush` before it is
// refused by every node identically, so shipping this binary changes nothing a producer
// emits. Scheduling the height is an operator commit, after the chronos scenario
// (`sigil-node/tests/chronos_gauge_push_settles.rs`) and with BOTH nodes on the binary
// before the height — the v9 master-rotation discipline.

/// The gauge contract's address (storage namespace for every feed).
pub const GAUGE_CONTRACT: ContractId = [0x0D; 32];
/// Activation height for `GaugePush`. DORMANT until an operator schedules a real height.
pub const GAUGE_LIVE_HEIGHT: u64 = u64::MAX;
/// Fixed-point scale of a gauge value: `value_e6 = round(value × 1e6)`.
pub const GAUGE_SCALE: i128 = 1_000_000;
/// Readings older than this (in blocks) are STALE for any consumer; at 8 blk/s idle the
/// 6-hourly sigil-earth cadence is ~170 k blocks, at 110 blk/s ~2.4 M — so the window is
/// one missed publication at full speed, three at idle.
pub const MAX_GAUGE_AGE_BLOCKS: u64 = 3_000_000;

/// Canonical feed names. Anything else is refused at apply — an unknown feed is a typo, not
/// a new gauge; new gauges are added here, in a commit every node runs.
pub const FEED_NAMES: [&str; 6] = ["earth.k_resid", "earth.k_p99", "bio.k_bio", "bio.breathing_pgc_day", "bio.ops_atp_log10", "bio.ppm"];

/// Feed id = BLAKE3("sigil-gauge-v1|" ‖ name). Stable, name-derived, never reordered.
pub fn feed_id(name: &str) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-gauge-v1|");
    h.update(name.as_bytes());
    *h.finalize().as_bytes()
}

/// Is `feed` one of [`FEED_NAMES`]? Returns the name.
pub fn feed_name(feed: &[u8; 32]) -> Option<&'static str> {
    FEED_NAMES.iter().copied().find(|n| feed_id(n) == *feed)
}

/// The digest slot that pairs with a feed's value slot: the feed id with every byte inverted.
pub fn digest_slot(feed: &[u8; 32]) -> SlotId {
    let mut s = *feed;
    for b in s.iter_mut() {
        *b = !*b;
    }
    s
}

/// A committed gauge reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GaugeReading {
    /// value × 1e6 (signed: the breathing rate is negative while the biosphere inhales).
    pub value_e6: i128,
    /// The reading's own date as YYYYMMDD (the day the measurement describes, not the push).
    pub reading_date: u32,
    /// Block height the push landed at (freshness).
    pub height: u64,
    /// Pushes landed for this feed so far (monotone; 0 = never).
    pub nonce: u32,
    /// BLAKE3 of the sigil-earth `latest.json` the reading came from.
    pub attest_blake3: [u8; 32],
}

/// value_e6 (LE i128, 16 B) ‖ reading_date (LE u32, 4 B) ‖ height (LE u64, 8 B) ‖ nonce (LE u32, 4 B) = 32 B.
pub fn encode_gauge(value_e6: i128, reading_date: u32, height: u64, nonce: u32) -> [u8; 32] {
    let mut v = [0u8; 32];
    v[..16].copy_from_slice(&value_e6.to_le_bytes());
    v[16..20].copy_from_slice(&reading_date.to_le_bytes());
    v[20..28].copy_from_slice(&height.to_le_bytes());
    v[28..32].copy_from_slice(&nonce.to_le_bytes());
    v
}

fn decode_gauge(v: &[u8; 32]) -> (i128, u32, u64, u32) {
    (
        i128::from_le_bytes(v[..16].try_into().unwrap()),
        u32::from_le_bytes(v[16..20].try_into().unwrap()),
        u64::from_le_bytes(v[20..28].try_into().unwrap()),
        u32::from_le_bytes(v[28..32].try_into().unwrap()),
    )
}

/// Is the gauge feature active at `height`?
pub fn gauge_active(height: u64) -> bool {
    gauge_active_at(height, gauge_live_height())
}
/// The gate as a pure function of both heights (what the chronos scenario drives).
pub fn gauge_active_at(height: u64, live_height: u64) -> bool {
    height >= live_height
}

static GAUGE_LIVE_OVERRIDE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);

/// The effective activation height: the consensus constant, unless a chronos harness has
/// scheduled an earlier one in THIS process.
pub fn gauge_live_height() -> u64 {
    GAUGE_LIVE_HEIGHT.min(GAUGE_LIVE_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed))
}

/// Chronos/test seam ONLY — schedule the activation height inside this process so the real
/// apply path can be driven through it before an operator commits a consensus height. The
/// live node never calls this (grep for callers before believing otherwise). Same class of
/// seam as `mint_next_block_with_schedule`.
pub fn chronos_schedule_gauge_live_height(h: u64) {
    GAUGE_LIVE_OVERRIDE.store(h, std::sync::atomic::Ordering::Relaxed);
}

/// The two mutations one push writes: the reading and its digest.
pub fn gauge_mutations(feed: [u8; 32], value_e6: i128, reading_date: u32, height: u64, nonce: u32, attest_blake3: [u8; 32]) -> [StateMutation; 2] {
    [
        StateMutation::SetContractSlot { contract: GAUGE_CONTRACT, slot: feed, value: encode_gauge(value_e6, reading_date, height, nonce) },
        StateMutation::SetContractSlot { contract: GAUGE_CONTRACT, slot: digest_slot(&feed), value: attest_blake3 },
    ]
}

/// Read a feed. `None` when nothing has ever been pushed for it.
pub fn read_gauge(state: &SigilState, feed: &[u8; 32]) -> Option<GaugeReading> {
    let v = state.contract_slot(&GAUGE_CONTRACT, feed);
    let (value_e6, reading_date, height, nonce) = decode_gauge(&v);
    if nonce == 0 {
        return None; // all-zero slot (never pushed) decodes as nonce 0
    }
    let attest_blake3 = state.contract_slot(&GAUGE_CONTRACT, &digest_slot(feed));
    Some(GaugeReading { value_e6, reading_date, height, nonce, attest_blake3 })
}

/// A reading is fresh when it landed within [`MAX_GAUGE_AGE_BLOCKS`] of `at_height`.
pub fn gauge_is_fresh(r: &GaugeReading, at_height: u64) -> bool {
    at_height.saturating_sub(r.height) <= MAX_GAUGE_AGE_BLOCKS
}

/// Is `d` a plausible YYYYMMDD? (The chain cannot know the calendar; it can refuse nonsense.)
pub fn plausible_reading_date(d: u32) -> bool {
    let (y, m, day) = (d / 10_000, (d / 100) % 100, d % 100);
    (2000..=2200).contains(&y) && (1..=12).contains(&m) && (1..=31).contains(&day)
}

#[cfg(test)]
mod gauge_tests {
    use super::*;

    #[test]
    fn feed_ids_are_name_derived_and_the_digest_slot_never_collides() {
        let k = feed_id("bio.k_bio");
        assert_eq!(feed_name(&k), Some("bio.k_bio"));
        assert_eq!(feed_name(&feed_id("bio.nope")), None);
        assert_ne!(digest_slot(&k), k);
        for a in FEED_NAMES {
            for b in FEED_NAMES {
                assert_ne!(feed_id(a), digest_slot(&feed_id(b)), "{a} value slot == {b} digest slot");
            }
        }
    }

    #[test]
    fn gauge_round_trips_through_state_and_is_none_before_any_push() {
        let mut s = SigilState::new();
        let feed = feed_id("bio.breathing_pgc_day");
        assert!(read_gauge(&s, &feed).is_none());
        let m = gauge_mutations(feed, -53_320, 20260913, 18_042_000, 1, [0xAB; 32]);
        commit_state_transition(&mut s, &StateTransition { at_height: 18_042_000, mutations: m.to_vec() }, 18_042_000).unwrap();
        let r = read_gauge(&s, &feed).unwrap();
        assert_eq!(r, GaugeReading { value_e6: -53_320, reading_date: 20260913, height: 18_042_000, nonce: 1, attest_blake3: [0xAB; 32] });
        assert!(gauge_is_fresh(&r, 18_042_000 + MAX_GAUGE_AGE_BLOCKS));
        assert!(!gauge_is_fresh(&r, 18_042_001 + MAX_GAUGE_AGE_BLOCKS));
        // a zero-nonce slot (never pushed) reads as None even if bytes exist
        commit_state_transition(&mut s, &StateTransition { at_height: 1, mutations: vec![StateMutation::SetContractSlot { contract: GAUGE_CONTRACT, slot: feed, value: encode_gauge(5, 20260101, 1, 0) }] }, 1).unwrap();
        assert!(read_gauge(&s, &feed).is_none());
    }

    #[test]
    fn the_gate_is_dormant_until_scheduled_in_process() {
        assert_eq!(GAUGE_LIVE_HEIGHT, u64::MAX, "must ship DORMANT — the activation height is an operator commit");
        assert!(!gauge_active_at(u64::MAX - 1, GAUGE_LIVE_HEIGHT));
        assert!(gauge_active_at(10, 10) && !gauge_active_at(9, 10));
        assert!(plausible_reading_date(20260913) && !plausible_reading_date(20261341) && !plausible_reading_date(913));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> SigilState {
        SigilState::new()
    }

    #[test]
    fn authority_can_push_and_read_committed() {
        let mut s = fresh();
        let roots_before = s.roots();
        // $2.50 per SIGIL → 2.5 × 1e8
        update_price(&mut s, 0, ORACLE_AUTHORITY, 250_000_000).unwrap();
        assert_eq!(read_price(&s), 250_000_000);
        // committed → contract_state_root changed
        assert_ne!(s.roots().contract_state_root, roots_before.contract_state_root);
    }

    #[test]
    fn non_authority_rejected() {
        let mut s = fresh();
        let imposter: WalletId = [0x99; 32];
        let r = update_price(&mut s, 0, imposter, 100_000_000);
        assert!(matches!(r, Err(OracleError::Unauthorized)));
        assert_eq!(read_price(&s), 0, "no price written by imposter");
    }

    #[test]
    fn unset_price_is_zero() {
        assert_eq!(read_price(&fresh()), 0);
        assert_eq!(read_price_height(&fresh()), 0);
        assert_eq!(read_feeder(&fresh()), None);
    }

    #[test]
    fn price_updates_overwrite() {
        let mut s = fresh();
        update_price(&mut s, 0, ORACLE_AUTHORITY, PRICE_SCALE).unwrap();
        update_price(&mut s, 1, ORACLE_AUTHORITY, 3 * PRICE_SCALE).unwrap();
        assert_eq!(read_price(&s), 3 * PRICE_SCALE);
        assert_eq!(read_price_height(&s), 1, "the stamp follows the latest push");
    }

    #[test]
    fn freshness_is_a_window_after_the_stamp() {
        let mut s = fresh();
        assert!(!price_is_fresh(&s, 10), "no price at all is stale");
        update_price(&mut s, 100, ORACLE_AUTHORITY, PRICE_SCALE).unwrap();
        assert!(price_is_fresh(&s, 100), "same block as the push");
        assert!(price_is_fresh(&s, 100 + MAX_PRICE_AGE_BLOCKS), "exactly at the edge is still fresh");
        assert!(!price_is_fresh(&s, 101 + MAX_PRICE_AGE_BLOCKS), "one past the edge is stale");
        assert!(!price_is_fresh(&s, 99), "a stamp in the future is refused");
    }

    #[test]
    fn a_price_slot_without_a_height_stamp_is_stale() {
        // The pre-activation shape: only PRICE_SLOT written. Must read as
        // stale so a mint cannot key off a price whose age is unknown.
        let mut s = fresh();
        let t = StateTransition {
            at_height: 5,
            mutations: vec![StateMutation::SetContractSlot {
                contract: ORACLE_CONTRACT, slot: PRICE_SLOT, value: encode_price(PRICE_SCALE),
            }],
        };
        commit_state_transition(&mut s, &t, 5).unwrap();
        assert_eq!(read_price(&s), PRICE_SCALE);
        assert!(!price_is_fresh(&s, 5));
    }

    #[test]
    fn delegation_round_trips_and_zero_revokes() {
        let mut s = fresh();
        let feeder: WalletId = [0xFE; 32];
        let t = StateTransition { at_height: 1, mutations: vec![delegate_mutation(feeder)] };
        commit_state_transition(&mut s, &t, 1).unwrap();
        assert_eq!(read_feeder(&s), Some(feeder));
        let t = StateTransition { at_height: 2, mutations: vec![delegate_mutation([0u8; 32])] };
        commit_state_transition(&mut s, &t, 2).unwrap();
        assert_eq!(read_feeder(&s), None, "all-zero revokes");
    }

    #[test]
    fn encoders_are_le_prefixes() {
        assert_eq!(&encode_price(PRICE_SCALE)[..16], &PRICE_SCALE.to_le_bytes());
        assert_eq!(&encode_height(6_400_000)[..8], &6_400_000u64.to_le_bytes());
    }
}
