//! sigil-dagknight — the DAG-ordering lane crate.
//!
//! **v1 implements DETERMINISTIC BRAID LINEARIZATION**: a topologically-guarded
//! total order over the committed `parent_hash` / `merge_parents` block DAG,
//! with a `(height, producer, hash)` tie-break and a fixed finality window.
//! This remains the default (v1 is byte-identical whenever
//! [`BraidConfig::ghostdag_k`] is unset).
//!
//! **v2 (opt-in, see [`ghostdag`]) implements real GHOSTDAG-style k-cluster
//! blue/red coloring**: selected-parent-by-blue-score and a greedy admission
//! test bounding each blue block's concurrent-block ("blue anticone") count
//! by a fixed `k`. **It is still NOT DagKnight-the-paper** (no parameterless
//! min-cut anticone bound — `k` is fixed, not derived from observed network
//! delay) and does not yet use blue **work** (difficulty-weighted) for
//! selection or finality — see the `ghostdag` module doc for the full,
//! current honest-naming statement. Any release notes, science_summary text,
//! or TUI strings sourced from this crate must reproduce these claims and
//! nothing stronger. Binding statement: `docs/SIGIL_DAGKNIGHT_LANE_v0.md` §1
//! (v1) and the `ghostdag` module doc (v2).
//!
//! The crate orders [`BlockView`]s only — no state, no transitions, no
//! networking, no tokio. Bodies (full blocks) are a `sigil-node` concern.

#![warn(missing_docs)]

pub mod bitset;
pub mod braid;
pub mod ghostdag;
pub mod present;
pub mod sim;
pub mod view;

pub use bitset::{BitfieldDag, BitfieldDagStats, VertexBitfield, VertexIndexMap};
pub use braid::{Braid, BraidStats, BlockSummary};
pub use ghostdag::{BlockGhostdagData, GhostdagStore};
pub use present::BraidPresentation;
pub use sim::BraidSimReport;
pub use sigil_header::BlockHash;
pub use view::BlockView;

/// Tunables for the braid ordering window. Node wiring sources these from the
/// environment via [`BraidConfig::from_env`]; the defaults are the design-doc
/// values (`docs/SIGIL_DAGKNIGHT_LANE_v0.md` §2).
#[derive(Debug, Clone)]
pub struct BraidConfig {
    /// Finality window depth: once the selected tip is ≥ `final_depth` above
    /// height *h*, the linearized prefix through *h* is frozen. Default 512
    /// (env `SIGIL_DAG_FINAL_DEPTH`) — bumped from 64 on 2026-08-15.
    ///
    /// **Why 512, not 64:** `computed_final`'s height-offset finality rule
    /// is only sound when the network's actual block-reordering never
    /// exceeds `final_depth` (see that method's doc comment for the full
    /// argument). Measured directly with `examples/k_probe.rs` (P=6
    /// concurrent producers, k=1): under a REALISTIC bounded-reorder
    /// delivery model, two independent nodes converge byte-for-byte up
    /// through a reorder window of 32 — the exact same test starts
    /// diverging at a reorder window of 64, i.e. right at the OLD
    /// `final_depth`. 512 gives an 8× margin against that boundary — real
    /// gossip reordering this network has ever measured is nowhere close
    /// to it — while still finalizing in well under a minute at SIGIL's
    /// measured adaptive block rate (8-60 blk/s). It does not make the rule
    /// sound against literally UNBOUNDED adversarial reordering (no finite
    /// depth can — see the module's `computed_final` doc comment); that
    /// needs a structurally different, anticone-bounded finality rule
    /// (tracked, not done here).
    pub final_depth: u64,
    /// Hard cap on active (non-finalized) views held in the window. Bitfield
    /// memory is O(window²/8) bytes, so this cap is load-bearing. Default
    /// 16_384 (env `SIGIL_DAG_MAX_WINDOW`).
    pub max_window: usize,
    /// Hard cap on parked parent-missing views. Default 4_096
    /// (env `SIGIL_DAG_MAX_PENDING`).
    pub max_pending: usize,
    /// Headers carrying more merge parents than this are rejected. Default 4
    /// (env `SIGIL_DAG_MAX_MERGE_PARENTS`).
    pub max_merge_parents: usize,
    /// GHOSTDAG-style v2 k-cluster bound. `None` (default) = v1 linearization
    /// only, byte-identical to before this option existed. `Some(k)` opts a
    /// braid into real blue/red coloring + blue-score tip selection — see
    /// the [`crate::ghostdag`] module doc for exactly what that does and does
    /// not claim. Env `SIGIL_DAG_GHOSTDAG_K` (unset or unparsable = `None`).
    pub ghostdag_k: Option<u32>,
    /// v2.1, opt-in separately from `ghostdag_k` (2026-08-15): finality
    /// measured in **blue-score depth** instead of raw height-offset, for
    /// braids with GHOSTDAG coloring active. `None` (default) = even with
    /// `ghostdag_k` set, `computed_final` still uses the classic
    /// height-offset rule (`final_depth`) — this is a SEPARATE, more
    /// conservative opt-in on top of v2 coloring, not something v2 flips on
    /// automatically. `Some(d)` switches `computed_final` to walk the
    /// selected-parent spine back from the tip until blue score drops by
    /// `d`, and uses THAT spine block's height as the finality line.
    ///
    /// **What this does and does not fix — read before enabling.** The
    /// investigation that produced this field found that `final_depth`'s
    /// plain height-offset rule breaks the instant network reordering
    /// reaches the depth itself, for a structural reason: a block that
    /// arrives late gets rejected `BelowFinal` at the DOOR — before the
    /// GHOSTDAG coloring algorithm (which the crate's own
    /// `coloring_is_order_invariant_across_two_valid_insertion_orders` test
    /// already proves is arrival-order-invariant once a block is actually
    /// let in) ever gets a chance to absorb it. Tying the SAME kind of
    /// height-threshold to blue-score depth instead of a flat count is a
    /// real improvement in ONE specific way: it correctly excludes RED
    /// blocks from ever being treated as spine/finalized (the height rule
    /// has no concept of red vs. blue at all), and it ties the threshold to
    /// genuine absorbed DAG structure rather than an arbitrary wall-clock-ish
    /// count. **It is NOT a proof that finality is now safe against
    /// unbounded adversarial reordering** — that needs the actual GHOSTDAG/
    /// PHANTOM/DagKnight safety argument (a block is irreversible once no
    /// alternative chain respecting the same k-cluster bound could ever
    /// exclude it from every future blue set — a "k+1 blue confirmations"
    /// style anticone-majority argument, not a score-threshold), which this
    /// field does not implement. See `examples/k_probe_bluefinal.rs` for the
    /// measured, honest comparison against the height-offset rule at the
    /// exact adversarial scenario that exposed the original bug. Env
    /// `SIGIL_DAG_FINAL_BLUE_DEPTH` (unset or unparsable = `None`).
    pub final_blue_depth: Option<u64>,
    /// 2026-08-21 (the "no eviction path for a stuck pending pool" fragility,
    /// found live on both Epsilon and happysrv — see
    /// `computed_final`'s doc comment for the mechanism this closes). When
    /// the pending pool is genuinely AT its `max_pending` cap — not just
    /// "some entry is old", but structurally full — that is a much stronger
    /// signal of real trouble than ordinary fork-resolution lag, and the
    /// system should self-heal far sooner than the general `max_window`
    /// hard-floor (16,384 heights ⇒ 7-40+ minutes at SIGIL's measured
    /// throughput) allows. Default 2_048 (env
    /// `SIGIL_DAG_SATURATED_SELF_HEAL_WINDOW`) — a few multiples of
    /// `final_depth` (still generous margin for genuinely-recoverable
    /// forks), but self-heals in roughly a minute instead of the better
    /// part of an hour. Only takes effect when `pending.len() >=
    /// max_pending`; below the cap, behavior is byte-for-byte unchanged
    /// (uses `max_window` exactly as before).
    pub saturated_self_heal_window: usize,

    /// 2026-08-26 — the OTHER half of the "stuck pending pool" fragility, and the
    /// one that was actually biting live.
    ///
    /// [`Self::saturated_self_heal_window`] deliberately only engages when the pool
    /// is AT `max_pending` ("below the cap, behavior is byte-for-byte unchanged").
    /// But `computed_final` clamps the finality line to `pending_floor - 1`, and
    /// `pending_floor` is the LOWEST pending height — so exactly ONE pending entry
    /// whose parent never arrives pins the finality line where it is, forever, with
    /// a pool of 1 out of 4,096. Finality then advances only via the `max_window`
    /// hard floor: 16,384 heights behind the tip, which at the rate measured live
    /// on Epsilon (2026-08-26: 3 blocks in 3 minutes) is over a week. Every
    /// settlement-gated subsystem stalls behind it — shielded registrations were
    /// simply the first place it was noticed.
    ///
    /// So: a pending entry is evicted once the tip has advanced this many heights
    /// past where it was parked, whatever the pool occupancy. The bound is measured
    /// in TIP HEIGHT, never wall-clock — braid state has to converge identically on
    /// every node, and a clock does not. `final_depth` is the principled default:
    /// past that the missing parent is below the finality line and `insert()` would
    /// refuse it anyway, so continuing to wait cannot succeed.
    ///
    /// Same safety category as `pending_floor` and `saturated_self_heal_window`
    /// already are — this changes HOW SOON a node stops waiting, never WHAT it
    /// orders. Env `SIGIL_DAG_PENDING_MAX_TIP_LAG`; 0 disables eviction entirely
    /// (the pre-2026-08-26 behavior).
    pub pending_max_tip_lag: u64,
}

impl Default for BraidConfig {
    fn default() -> Self {
        Self {
            final_depth: 512,
            max_window: 16_384,
            max_pending: 4_096,
            max_merge_parents: 4,
            ghostdag_k: None,
            final_blue_depth: None,
            saturated_self_heal_window: 2_048,
            pending_max_tip_lag: 512,
        }
    }
}

impl BraidConfig {
    /// Build a config from `SIGIL_DAG_*` environment variables, falling back
    /// to the defaults for anything unset or unparsable.
    pub fn from_env() -> Self {
        fn get<T: std::str::FromStr>(key: &str, default: T) -> T {
            std::env::var(key)
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(default)
        }
        let d = Self::default();
        Self {
            final_depth: get("SIGIL_DAG_FINAL_DEPTH", d.final_depth),
            max_window: get("SIGIL_DAG_MAX_WINDOW", d.max_window),
            max_pending: get("SIGIL_DAG_MAX_PENDING", d.max_pending),
            max_merge_parents: get("SIGIL_DAG_MAX_MERGE_PARENTS", d.max_merge_parents),
            ghostdag_k: std::env::var("SIGIL_DAG_GHOSTDAG_K")
                .ok()
                .and_then(|v| v.trim().parse().ok()),
            final_blue_depth: std::env::var("SIGIL_DAG_FINAL_BLUE_DEPTH")
                .ok()
                .and_then(|v| v.trim().parse().ok()),
            saturated_self_heal_window: get(
                "SIGIL_DAG_SATURATED_SELF_HEAL_WINDOW",
                d.saturated_self_heal_window,
            ),
            pending_max_tip_lag: get("SIGIL_DAG_PENDING_MAX_TIP_LAG", d.pending_max_tip_lag),
        }
    }
}

/// Outcome of inserting a [`BlockView`] into the braid. Every input yields a
/// structured outcome — the ordering layer never panics on foreign data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertOutcome {
    /// Accepted; `newly_ready` blocks became orderable (drain them).
    Inserted {
        /// Number of blocks (this one + unparked pendings) that became
        /// orderable as a result of this insert.
        newly_ready: usize,
    },
    /// Already present — no-op, order unchanged.
    Duplicate,
    /// Parked in the pending set; caller should backfill these hashes.
    MissingParents(Vec<BlockHash>),
    /// Height ≤ finalized height — refused from ordering (reorg window guard).
    BelowFinal {
        /// The current finalized height the insert fell at or below.
        finalized: u64,
    },
    /// Structurally invalid (self-parent, dup merge parent, > max_merge_parents,
    /// height not parent.height+1, pending overflow). Never panics.
    Rejected(&'static str),
}
