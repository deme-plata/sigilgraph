//! sigil-dagknight — the DAG-ordering lane crate.
//!
//! **v1 implements DETERMINISTIC BRAID LINEARIZATION**: a topologically-guarded
//! total order over the committed `parent_hash` / `merge_parents` block DAG,
//! with a `(height, producer, hash)` tie-break and a fixed finality window.
//!
//! **It is NOT GHOSTDAG** (no blue score, no k-cluster, no
//! selected-parent-by-blue-work) **and NOT DagKnight-the-paper** (no
//! parameterless min-cut anticone bound). GHOSTDAG-style greedy k-cluster
//! ordering is the scoped v2 upgrade; DagKnight-the-paper is future research
//! and is not promised. Any release notes, science_summary text, or TUI
//! strings sourced from this crate must reproduce these claims and nothing
//! stronger. Binding statement: `docs/SIGIL_DAGKNIGHT_LANE_v0.md` §1.
//!
//! The crate orders [`BlockView`]s only — no state, no transitions, no
//! networking, no tokio. Bodies (full blocks) are a `sigil-node` concern.

#![warn(missing_docs)]

pub mod bitset;
pub mod braid;
pub mod present;
pub mod sim;
pub mod view;

pub use bitset::{BitfieldDag, BitfieldDagStats, VertexBitfield, VertexIndexMap};
pub use braid::{Braid, BraidStats};
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
    /// height *h*, the linearized prefix through *h* is frozen. Default 64
    /// (env `SIGIL_DAG_FINAL_DEPTH`).
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
}

impl Default for BraidConfig {
    fn default() -> Self {
        Self {
            final_depth: 64,
            max_window: 16_384,
            max_pending: 4_096,
            max_merge_parents: 4,
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
