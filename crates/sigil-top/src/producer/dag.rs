//! Producer-mode DagKnight/GHOSTDAG braid wiring — INERT STUB, Phase 1 (see
//! `producer/mod.rs`).
//!
//! Phase 2 ports `dag_seed_braid()`, `dag_build_frontier()`, `dag_drain_apply()`, and
//! `compute_topology_commitment()` out of `sigil-node/src/main.rs`, backed by the
//! `sigil-dagknight` crate (real GHOSTDAG-family blue/red k-cluster coloring — NOT the
//! adaptive DagKnight-the-paper algorithm; SIGIL runs a fixed operator-chosen `k`,
//! production default `k=4` via `SIGIL_DAG_GHOSTDAG_K`). This is the module
//! `cathedral.rs`'s own doc comment anticipated: *"Real flux-narwhal-core /
//! flux-consensus linearizer can be dropped in the `run_dagknight_linearize` slot
//! later without changing the surface."*
//!
//! Known, documented limitation carried forward from sigil-dagknight itself (not
//! something this module fixes): blue-score uses raw block COUNT, not
//! difficulty-weighted work — correct only when producers have roughly uniform
//! mining power. Epsilon and a home PC will not. Phase 5's own doc comment repeats
//! this as an explicit operator judgment call before ever enabling real production
//! against the live mesh — it is not resolved by this port.
