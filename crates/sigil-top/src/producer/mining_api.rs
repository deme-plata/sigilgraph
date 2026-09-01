//! Local mining HTTP API — 2026-08-25, operator-directed ("let a miner mine against
//! their OWN locally-running node instead of always hitting the central Epsilon node").
//!
//! The gap this closes: `sigil-top`'s producer mode (`producer/run.rs`) syncs a real
//! chain, mints real blocks, and publishes them to the real sigil-g0 mesh — but nothing
//! in that path ever listened on an HTTP port. A miner pointed at `http://127.0.0.1:<port>`
//! had nothing there to answer. This module starts a REAL `sigil-api` HTTP server (the
//! exact same axum router + `MiningBridge`/`share_target_for`/`submit()`/dynamic-difficulty
//! machinery `sigil-node`'s own binary already embeds — see `sigil_api::router` and
//! `sigil_api::mining` doc comments) bound to a LOCAL port, backed by the SAME
//! `sigil_api::AppState` handles the producer's own tick loop drains/publishes into. This
//! is deliberately NOT a hand-rolled mining server: reusing `sigil-api::AppState::new` +
//! `sigil_api::router` + `sigil_api::serve` verbatim means every fix already made to
//! `MiningBridge` this session (see `crates/sigil-api/src/mining.rs`'s own changelog)
//! applies here automatically, with zero duplicated verification/difficulty logic.
//!
//! Gating (mirrors `producer/mod.rs`'s two-gate contract exactly — no new gate invented):
//! this server is only ever spawned from [`super::run::maybe_start`], which itself only
//! runs when BOTH `SIGIL_TOP_PRODUCER=1` AND `SIGIL_TOP_PRODUCE=1` are set (and the
//! sync-then-produce bootstrap succeeds). That's a deliberate choice, not laziness: a
//! solve submitted here only means something if THIS process is actually minting blocks
//! from it (`ProducerState::tick` calls `sigil_node::solve_credit::take_creditable_solve`
//! against the SAME `MiningBridge` this server publishes/pops through) — a producer that
//! is only syncing+following (`SIGIL_TOP_PRODUCER=1` alone, no `SIGIL_TOP_PRODUCE`) never
//! mints anything of its own, so a mining API in front of it would accept real proof-of-work
//! and silently have nowhere to credit it. See `run.rs`'s module doc for why
//! `SIGIL_TOP_PRODUCER=1` alone currently starts nothing at all (no sync, no follow loop) —
//! there is no "follow-only" mode this could otherwise piggyback on today.
//!
//! Default (no env vars set) impact: **zero**. This module's only mutable state is a
//! process-wide `AtomicBool` that starts `false` and is only ever flipped by
//! [`spawn_local_mining_api`], which is only ever called from the gated path above. No
//! listener is bound, no port is touched, `local_mining_api_is_up()` always returns
//! `false`, and `engine_node_url()` in `main.rs` falls through to the historical remote
//! default exactly as before this module existed.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Distinct from `wallet_ui::WALLET_PORT` (9800, the embedded wallet/explorer UI) and
/// from the conventional remote `sigil-api` port (18181, e.g. Epsilon's live
/// `SIGIL_MONEY_API`) — a local producer's own mining API needs its own port so running
/// `sigil-top` producer mode alongside a real local `sigil-node` (or the embedded wallet
/// server) on the same box can never collide.
pub const LOCAL_MINING_API_PORT: u16 = 18183;

/// Flips to `true` only once [`spawn_local_mining_api`]'s bind-confirmation probe has
/// actually observed the listener accepting connections — not merely "we asked it to
/// start". See that function's doc for why a probe is used instead of a bind-success
/// callback (keeps this module from needing a direct `axum`/`tokio::net` dependency
/// beyond what it already uses for the probe itself).
static LOCAL_MINING_API_UP: AtomicBool = AtomicBool::new(false);

/// The local mining API port. Defaults to [`LOCAL_MINING_API_PORT`] (18183) but is
/// overridable via `SIGIL_LOCAL_MINING_API_PORT`. The default was chosen to be distinct
/// from the node's own ports, but a sigil-node's raw tx-ingest bridge ALSO binds 18183 —
/// so a sigil-top producer sharing a box with a node (or a test running on such a box)
/// must be able to relocate this loopback API instead of failing to bind.
pub fn local_mining_api_port() -> u16 {
    std::env::var("SIGIL_LOCAL_MINING_API_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(LOCAL_MINING_API_PORT)
}

/// `127.0.0.1:<port>` — loopback only. This is a node the operator is running for
/// themselves; it has no business accepting mining traffic from the network.
pub fn local_mining_api_addr() -> String {
    format!("127.0.0.1:{}", local_mining_api_port())
}

/// Serializes the two tests that read/override `SIGIL_LOCAL_MINING_API_PORT` — env vars
/// are process-global and cargo runs tests in parallel.
#[cfg(test)]
pub(crate) static PORT_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The URL a local miner (or `engine_node_url()` in `main.rs`) should point at.
pub fn local_mining_api_url() -> String {
    format!("http://{}", local_mining_api_addr())
}

/// Has the local mining API actually come up? Cheap (one atomic load) — safe to call from
/// a hot path like the Mining tab's per-frame render (`mining_ui.rs`), unlike a real
/// network probe would be.
pub fn local_mining_api_is_up() -> bool {
    LOCAL_MINING_API_UP.load(Ordering::Relaxed)
}

/// Start the local `sigil-api` HTTP server on the current tokio runtime, sharing `app`
/// with whatever else already holds it (the caller's `ProducerState::api`, so a share
/// submitted through `POST /v1/mining/submit` here lands in the EXACT `MiningBridge`
/// queue the producer's own tick loop drains via
/// `sigil_node::solve_credit::take_creditable_solve`).
///
/// Must be called from within a running tokio runtime (it uses `tokio::spawn`, not its
/// own thread+runtime) — [`super::run::spawn_networked_loop`]'s `rt.block_on` context is
/// the intended caller.
///
/// Bind-confirmation: `sigil_api::serve` binds and blocks in one call, so there's no
/// callback point between "bind succeeded" and "now serving" to hook `LOCAL_MINING_API_UP`
/// from the inside. Instead, a second short-lived task polls the port from the OUTSIDE
/// with real TCP connects (bounded: ~10s at 50ms) — the same "don't trust it started, go
/// observe it" discipline this session's other work already applied to mesh/producer
/// startup. If the port never answers within the window, this logs it loudly and leaves
/// `local_mining_api_is_up()` false — `engine_node_url()` then correctly keeps using the
/// remote default instead of pointing a miner at a server that never came up.
pub fn spawn_local_mining_api(app: sigil_api::AppState) {
    let addr = local_mining_api_addr();
    let serve_addr = addr.clone();
    tokio::spawn(async move {
        if let Err(e) = sigil_api::serve(&serve_addr, app).await {
            crate::tlog!("[producer] \u{26a0} local mining API on {serve_addr} failed: {e}");
        }
        // The serve future only ever returns on bind failure or shutdown — either way
        // the port is no longer ours to claim as "up".
        LOCAL_MINING_API_UP.store(false, Ordering::SeqCst);
    });
    tokio::spawn(async move {
        // ~10s budget, not 2s: this confirmation poller is an ordinary tokio task
        // competing for the same executor as the serve task and everything else on
        // the box. Under load it can be scheduled late, and a 2s window was short
        // enough that a genuinely-serving API sometimes never got its flag set —
        // leaving `engine_node_url()` pointing the miner at the remote default even
        // though a working local server was right there. A longer budget only ever
        // helps the success case; a truly unbindable port still logs failure (just
        // later), with the flag correctly staying false the whole time.
        for _ in 0..200 {
            if tokio::net::TcpStream::connect(&addr).await.is_ok() {
                LOCAL_MINING_API_UP.store(true, Ordering::SeqCst);
                crate::tlog!(
                    "[producer] \u{26cf} local mining API confirmed up on {addr} — \
                     GET /v1/mining/challenge, POST /v1/mining/submit"
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        crate::tlog!("[producer] \u{26a0} local mining API did not come up on {addr} within 10s");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // No test asserts `!local_mining_api_is_up()` here: `LOCAL_MINING_API_UP` is a
    // process-wide static shared by every test in this binary, so such an assertion
    // would be order-dependent (flaky) the moment any other test ever calls
    // `spawn_local_mining_api`. The real safety contract — that it stays down for
    // everyone who never opts into producer mode — is structural (nothing outside
    // `run::maybe_start`'s two-gate-checked path calls this module's spawn function
    // at all) and is exercised end-to-end in the live verification for this change,
    // not as a unit test against shared global state.
    #[test]
    fn port_is_distinct_from_wallet_and_remote_convention() {
        let _g = PORT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("SIGIL_LOCAL_MINING_API_PORT"); // assert the DEFAULT
        assert_ne!(LOCAL_MINING_API_PORT, 9800, "must not collide with the embedded wallet server");
        assert_ne!(LOCAL_MINING_API_PORT, 18181, "must not collide with the conventional remote sigil-api port");
        assert_eq!(local_mining_api_addr(), "127.0.0.1:18183");
        assert_eq!(local_mining_api_url(), "http://127.0.0.1:18183");
    }
}
