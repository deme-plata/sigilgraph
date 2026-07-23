// delivery-probe — live-network measurement harness for the gossip delivery law.
//
// The law (sigil-top-delivery-law.pdf): D(p, r) = 1 - p^r for a message sent with
// redundancy r under per-transmission loss probability p. Shipped as
// FRONTIER_REDUNDANCY = 3 in sigil-top/src/block_sync/mod.rs — the frontier chunk is
// requested from up to 3 peers IN PARALLEL over flux-p2p request/response
// (/sigil/backfill/1), REQ_TIMEOUT = 10s, delivery = first reply wins.
//
// This harness measures that primitive on a REAL lossy network: real flux-p2p swarms,
// real TCP, loss induced by the kernel (netem on veth pairs in network namespaces),
// NOT by any simulator. The unit of "transmission" is one request/response attempt to
// one peer — the exact granularity the law is deployed at.
//
//   serve mode: minimal responder speaking the real backfill protocol; answers every
//     inbound request with --resp-bytes of payload (default 70_700 B ≈ one
//     1000-header frontier chunk at the live 70.7 B/header compressed size).
//   probe mode: real client. Each trial picks r distinct peers round-robin (the
//     block_sync healthy-rotation pattern), fires send_request to all r in parallel,
//     delivery = ≥1 success within --timeout-ms. EVERY per-attempt outcome is
//     recorded (peer, ok, latency) as JSONL — p̂ is measured from attempts, D̂ from
//     trials, and the law is tested as D̂ ≟ 1 - p̂^r with no assumed p anywhere.
//   peer-id mode: print the deterministic peer id for a node name (orchestration).

use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

#[derive(serde::Serialize)]
struct BackfillReq {
    from: u64,
    to: u64,
    headers_only: bool,
    codec: u8,
}

#[derive(serde::Serialize)]
struct AttemptOut {
    peer: String,
    ok: bool,
    ms: u64,
}

#[derive(serde::Serialize)]
struct TrialOut {
    trial: u64,
    r: usize,
    delivered: bool,
    first_ms: Option<u64>,
    attempts: Vec<AttemptOut>,
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    match mode.as_str() {
        "peer-id" => {
            let name = arg("--name").expect("--name");
            println!("{}", flux_p2p::swarm::peer_id_string(&name));
        }
        "serve" => serve(),
        "probe" => probe(),
        _ => {
            eprintln!("usage: delivery-probe serve|probe|peer-id [args]");
            std::process::exit(2);
        }
    }
}

fn serve() {
    let name = arg("--name").expect("--name");
    let listen = arg("--listen").unwrap_or_else(|| "/ip4/0.0.0.0/tcp/9501".into());
    let resp_bytes: usize = arg("--resp-bytes")
        .map(|s| s.parse().expect("--resp-bytes"))
        .unwrap_or(70_700);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio rt");
    rt.block_on(async move {
        let mut config = flux_p2p::NetworkConfig::default();
        config.node_id = name.clone();
        config.listen_addr = listen;
        config.bootstrap_peers = Vec::new(); // servers only listen
        config.dagknight_enabled = false;
        config.sap_enabled = false;
        config.x_algo_enabled = false;
        config.entanglement_enabled = false;
        config.gossipsub_topics = vec![flux_p2p::SIGIL_G0_BLOCKS_TOPIC.to_string()];
        let mut mgr = flux_p2p::NetworkManager::new(config);
        mgr.start().await.expect("swarm start");
        println!(
            "SERVE name={} peer_id={} resp_bytes={}",
            name,
            flux_p2p::swarm::peer_id_string(&name),
            resp_bytes
        );
        let payload = vec![0x53u8; resp_bytes]; // 'S'
        let mut served: u64 = 0;
        loop {
            for ev in mgr.drain_events() {
                if let flux_p2p::SwarmAppEvent::InboundRequest { request_id, .. } = ev {
                    mgr.respond(request_id, payload.clone());
                    served += 1;
                    if served % 500 == 0 {
                        eprintln!("served={served}");
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    });
}

fn probe() {
    let peers_arg = arg("--peers").expect("--peers addr1,addr2,...");
    let r: usize = arg("--r").map(|s| s.parse().unwrap()).unwrap_or(3);
    let trials: u64 = arg("--trials").map(|s| s.parse().unwrap()).unwrap_or(600);
    let concurrency: usize = arg("--concurrency").map(|s| s.parse().unwrap()).unwrap_or(16);
    let timeout_ms: u64 = arg("--timeout-ms").map(|s| s.parse().unwrap()).unwrap_or(10_000);
    let settle_secs: u64 = arg("--settle-secs").map(|s| s.parse().unwrap()).unwrap_or(15);
    let out_path = arg("--out").expect("--out results.jsonl");
    // Range plan: cycle requests through [base, base+window) in --span steps. For a
    // live node, base must sit inside the range its chain log actually serves
    // (finalized, below the tip) — requests beyond the served range go unanswered.
    let base: u64 = arg("--base").map(|s| s.parse().unwrap()).unwrap_or(0);
    let span: u64 = arg("--span").map(|s| s.parse().unwrap()).unwrap_or(8_192);
    let window: u64 = arg("--window").map(|s| s.parse().unwrap()).unwrap_or(65_536);

    let peer_addrs: Vec<String> = peers_arg.split(',').map(|s| s.trim().to_string()).collect();
    let expect_peers = peer_addrs.len();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("tokio rt");
    rt.block_on(async move {
        let mut config = flux_p2p::NetworkConfig::default();
        config.node_id = format!("dp-client-{}", std::process::id());
        eprintln!(
            "probe client peer_id={}",
            flux_p2p::swarm::peer_id_string(&config.node_id)
        );
        config.listen_addr = "/ip4/0.0.0.0/tcp/0".into();
        config.bootstrap_peers = peer_addrs;
        config.dagknight_enabled = false;
        config.sap_enabled = false;
        config.x_algo_enabled = false;
        config.entanglement_enabled = false;
        // Do NOT subscribe to the live block topic: against a producing node it is a
        // multi-blk/s firehose into an event queue this probe never drains. The probe
        // only exercises the request/response lane.
        config.gossipsub_topics = vec!["/sigil/g0/dp-probe-quiet".to_string()];
        let mut mgr = flux_p2p::NetworkManager::new(config);
        mgr.start().await.expect("swarm start");
        let mgr = Arc::new(mgr);

        // The peers we are ALLOWED to probe: exactly the /p2p/<id> set from --peers.
        // On a live mesh, kad/identify discovery adds peers we never asked for —
        // trials must never round-robin onto those.
        let allowed: std::collections::HashSet<String> = mgr
            .summary()
            .bootstrap_peers
            .iter()
            .filter_map(|a| a.rsplit_once("/p2p/").map(|(_, id)| id.to_string()))
            .collect();
        // Wait for the target peers to connect (loss on the links makes this slow).
        let deadline = Instant::now() + Duration::from_secs(settle_secs.max(5));
        let peers = loop {
            let targets: Vec<_> = mgr
                .connected_peers()
                .into_iter()
                .filter(|p| allowed.is_empty() || allowed.contains(&p.to_string()))
                .collect();
            if targets.len() >= expect_peers || Instant::now() > deadline {
                eprintln!(
                    "connected {}/{expect_peers} target peers ({} total incl. discovered)",
                    targets.len(),
                    mgr.connected_peers().len()
                );
                break targets;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        };
        assert!(
            peers.len() >= r.max(1),
            "need at least r={r} connected TARGET peers, have {}",
            peers.len()
        );
        let peers = Arc::new(peers);

        let sem = Arc::new(tokio::sync::Semaphore::new(concurrency));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<TrialOut>();
        // create the output file BEFORE the (slow, semaphore-gated) spawn loop so a
        // killed run still leaves its partial records on disk
        let mut out = std::io::BufWriter::new(std::fs::File::create(&out_path).expect("out file"));

        let n_peers = peers.len();
        for t in 0..trials {
            let permit = sem.clone().acquire_owned().await.unwrap();
            let mgr2 = mgr.clone();
            let peers2 = peers.clone();
            let tx2 = tx.clone();
            tokio::spawn(async move {
                let _permit = permit;
                // round-robin window of r distinct peers, like block_sync's rotation
                let chosen: Vec<_> =
                    (0..r).map(|k| peers2[(t as usize + k) % n_peers]).collect();
                // mimic the shipped frontier request shape, cycling within the
                // served range (see --base/--span/--window)
                let from = base + (t * span) % window.max(span);
                let req = BackfillReq {
                    from,
                    to: from + span,
                    headers_only: true,
                    codec: 1,
                };
                let payload = serde_json::to_vec(&req).unwrap();
                let started = Instant::now();
                let mut futs = Vec::new();
                for p in chosen.iter().cloned() {
                    let m = mgr2.clone();
                    let pl = payload.clone();
                    futs.push(tokio::spawn(async move {
                        let t0 = Instant::now();
                        let res = tokio::time::timeout(
                            Duration::from_millis(timeout_ms),
                            m.send_request(p, pl),
                        )
                        .await;
                        let ok = matches!(res, Ok(Ok(ref b)) if !b.is_empty());
                        (p.to_string(), ok, t0.elapsed().as_millis() as u64)
                    }));
                }
                let mut attempts = Vec::new();
                let mut first_ms: Option<u64> = None;
                for f in futs {
                    if let Ok((peer, ok, ms)) = f.await {
                        if ok {
                            let done = started.elapsed().as_millis() as u64;
                            first_ms = Some(first_ms.map_or(done.min(ms), |x: u64| x.min(ms)));
                        }
                        attempts.push(AttemptOut { peer, ok, ms });
                    }
                }
                let delivered = attempts.iter().any(|a| a.ok);
                let _ = tx2.send(TrialOut { trial: t, r, delivered, first_ms, attempts });
            });
        }
        drop(tx);

        let (mut done, mut delivered_n, mut att_n, mut att_fail) = (0u64, 0u64, 0u64, 0u64);
        while let Some(trial) = rx.recv().await {
            delivered_n += trial.delivered as u64;
            att_n += trial.attempts.len() as u64;
            att_fail += trial.attempts.iter().filter(|a| !a.ok).count() as u64;
            writeln!(out, "{}", serde_json::to_string(&trial).unwrap()).unwrap();
            done += 1;
            if done % 100 == 0 {
                eprintln!(
                    "trials={done}/{trials} D̂={:.4} p̂={:.4}",
                    delivered_n as f64 / done as f64,
                    att_fail as f64 / att_n.max(1) as f64
                );
            }
        }
        out.flush().unwrap();
        let p_hat = att_fail as f64 / att_n.max(1) as f64;
        let d_hat = delivered_n as f64 / done.max(1) as f64;
        let d_law = 1.0 - p_hat.powi(r as i32);
        println!(
            "RESULT r={r} trials={done} attempts={att_n} p_hat={p_hat:.5} d_hat={d_hat:.5} d_law={d_law:.5} residual_pp={:+.3}",
            (d_hat - d_law) * 100.0
        );
    });
}
