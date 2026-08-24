//! LIVE HTTP PROOF — the multi-rig hashrate fix (2026-08-24), completed.
//!
//! `MiningBridge`'s unit tests already prove the fix at the Rust-call level. This proves
//! it the way a real miner or the wallet UI actually reaches it: a real axum server, bound
//! to a real TCP port, hit with real HTTP requests, reading back real JSON. The previous
//! pass skipped this because `AppState` construction had no obvious precedent to copy —
//! `state()` in `sigil_api::tests` (src/lib.rs) turned out to be exactly that precedent.
//!
//! Uses `curl` via `std::process::Command` rather than adding a `reqwest` dev-dependency —
//! `sigil-api`'s own Cargo.toml deliberately keeps `reqwest` out of its dependency tree
//! (see the `flux-miner` dependency comment: "the path form is the only one that actually
//! holds the line" against pulling an HTTP client into the node's money API); a test-only
//! HTTP client is a reasonable thing to add, but curl is already on every box this runs on
//! and avoids the question entirely.

use std::sync::{Arc, RwLock};

use sigil_api::{router, AppState};
use sigil_narwhal_mempool::MempoolBackend;
use sigil_state::SigilState;

fn curl_json(url: &str) -> serde_json::Value {
    let out = std::process::Command::new("curl")
        .args(["-s", "-w", "\n%{http_code}", url])
        .output()
        .expect("curl must be runnable");
    let raw = String::from_utf8_lossy(&out.stdout);
    let (body, code) = raw.rsplit_once('\n').unwrap_or((&raw, ""));
    serde_json::from_str(body)
        .unwrap_or_else(|e| panic!("bad JSON from {url} (http {code}): {e}\nbody={body}"))
}

/// THE LIVE GATE: two rigs, one wallet, real HTTP — must sum, not clobber.
///
/// Also proves the old-client path (no `&rig=` at all) still works end-to-end rather than
/// erroring — the backward-compat guarantee the fix is supposed to preserve.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_rigs_on_one_wallet_sum_over_real_http() {
    let state = AppState::new(
        Arc::new(MempoolBackend::legacy()),
        Arc::new(RwLock::new(SigilState::new())),
    );
    // Seed a mineable frontier — without this, /mining/challenge 503s (no tip published).
    state.mining.publish_tip(1, [0u8; 32]);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral local port");
    let addr = listener.local_addr().unwrap();
    let app = router(state);
    let server = tokio::spawn(async move {
        axum::serve(listener, app.into_make_service()).await.unwrap();
    });
    // Let the accept loop actually start before the first request lands.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let base = format!("http://{addr}");
    let wallet = "aa".repeat(32);

    // Rig A polls a few times, like a real miner's loop.
    for _ in 0..3 {
        let v = curl_json(&format!(
            "{base}/v1/mining/challenge?wallet={wallet}&hps=600000000&rig=rigA"
        ));
        assert!(v.get("height").is_some(), "rig A challenge must succeed, got {v}");
    }
    // Rig B: SAME wallet, DIFFERENT rig id, different rate.
    for _ in 0..3 {
        let v = curl_json(&format!(
            "{base}/v1/mining/challenge?wallet={wallet}&hps=500000000&rig=rigB"
        ));
        assert!(v.get("height").is_some(), "rig B challenge must succeed, got {v}");
    }

    let miners = curl_json(&format!("{base}/v1/mining/miners?wallet={wallet}"));
    let net_hps = miners["data"]["net_hps"].as_f64().expect("net_hps present");
    let my_hps = miners["data"]["my_hps"].as_f64().expect("my_hps present");
    assert_eq!(
        net_hps, 1_100_000_000.0,
        "THE BUG, over real HTTP: two rigs on one wallet must SUM (600M+500M), got {miners}"
    );
    assert_eq!(
        my_hps, 1_100_000_000.0,
        "the wallet-total readback (the UI's 'my hashrate' pill) must also sum both rigs, got {miners}"
    );
    let live_miners = miners["data"]["live_miners"].as_u64().expect("live_miners present");
    assert_eq!(live_miners, 2, "two distinct (wallet,rig) entries must count as 2 live miners, got {miners}");

    // BACKWARD COMPAT: an old client that never sends `&rig=` at all must still work —
    // degrade to the pre-fix single-slot behavior, not error or crash.
    let old_wallet = "bb".repeat(32);
    let v = curl_json(&format!("{base}/v1/mining/challenge?wallet={old_wallet}&hps=250000000"));
    assert!(v.get("height").is_some(), "an old client with no rig param must still get a challenge: {v}");
    let old_miners = curl_json(&format!("{base}/v1/mining/miners?wallet={old_wallet}"));
    assert_eq!(
        old_miners["data"]["my_hps"].as_f64().unwrap(),
        250_000_000.0,
        "old-client single report must still read back correctly: {old_miners}"
    );

    server.abort();
}
