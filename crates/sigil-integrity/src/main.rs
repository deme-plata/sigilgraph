//! sigil-integrity — proves the SIGIL nodes hold the same chain and state at the same height, continuously.
//!
//!   sigil-integrity serve [--node name=url]... [--status-dir DIR] [--publish FILE] [--listen ADDR]
//!                         [--hook-listen ADDR --hook-url URL] [--interval-ms N] [--retain N]
//!
//! Per node it (1) follows the node's own event stream (`/v1/events?kinds=certificate`) for the spine hash
//! of every certified block, (2) polls `/v1/integrity` for the four state roots + supply + shielded anchor at
//! the node's applied height, and (3) polls `/v1/dagknight/recent` for the block hashes it holds — so a
//! follower that lags by hundreds of blocks is still checked, block by block, against what the leaders said
//! at those heights. Verdicts are drawn only at equal height and written to `status.json`.
mod hook;
mod ledger;
mod node;
mod report;

use ledger::Ledger;
use node::{now_ms, NodeSpec};
use report::Tracker;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

struct Cfg {
    nodes: Vec<NodeSpec>,
    status_dir: String,
    publish: Option<String>,
    listen: String,
    hook_listen: Option<String>,
    hook_url: Option<String>,
    interval_ms: u64,
    retain: usize,
}

fn parse_args() -> anyhow::Result<Cfg> {
    let mut cfg = Cfg { nodes: vec![], status_dir: "/home/storage/sigil-scratch/sigil-integrity".into(), publish: None, listen: "127.0.0.1:18190".into(), hook_listen: None, hook_url: None, interval_ms: 2000, retain: 20_000 };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let val = |i: &mut usize| -> anyhow::Result<String> { *i += 1; args.get(*i).cloned().ok_or_else(|| anyhow::anyhow!("{} needs a value", args[*i - 1])) };
        match a {
            "serve" => {}
            "--node" => {
                let v = val(&mut i)?;
                let (n, u) = v.split_once('=').ok_or_else(|| anyhow::anyhow!("--node name=url"))?;
                cfg.nodes.push(NodeSpec { name: n.to_string(), url: u.trim_end_matches('/').to_string() });
            }
            "--status-dir" => cfg.status_dir = val(&mut i)?,
            "--publish" => cfg.publish = Some(val(&mut i)?),
            "--listen" => cfg.listen = val(&mut i)?,
            "--hook-listen" => cfg.hook_listen = Some(val(&mut i)?),
            "--hook-url" => cfg.hook_url = Some(val(&mut i)?),
            "--interval-ms" => cfg.interval_ms = val(&mut i)?.parse()?,
            "--retain" => cfg.retain = val(&mut i)?.parse()?,
            "-h" | "--help" => {
                println!("sigil-integrity serve [--node name=url]... [--status-dir DIR] [--publish FILE] [--listen ADDR] [--hook-listen ADDR --hook-url URL] [--interval-ms N] [--retain N]");
                std::process::exit(0);
            }
            _ => anyhow::bail!("unknown arg {a}"),
        }
        i += 1;
    }
    if cfg.nodes.is_empty() {
        cfg.nodes = vec![
            NodeSpec { name: "epsilon".into(), url: "http://127.0.0.1:18181".into() },
            NodeSpec { name: "happysrv".into(), url: "http://10.77.0.5:18181".into() },
            NodeSpec { name: "node3".into(), url: "http://10.77.0.5:18182".into() },
        ];
    }
    Ok(cfg)
}

struct Shared {
    ledger: Mutex<Ledger>,
    tracker: Mutex<Tracker>,
    alerts_path: String,
}

impl Shared {
    fn take_cert(&self, node: &str, c: node::CertEvent, transport: &str) {
        let v = self.ledger.lock().unwrap().record_spine(node, c.height, &c.spine_block_hash, now_ms());
        let mut t = self.tracker.lock().unwrap();
        {
            let st = t.nodes.entry(node.to_string()).or_default();
            st.last_cert_height = Some(c.height.max(st.last_cert_height.unwrap_or(0)));
            st.last_event_ms = now_ms();
            if st.transport != "webhook" {
                st.transport = transport.to_string();
            }
        }
        let before = t.alarms.len();
        t.note_verdict(node, c.height, "spine", &v);
        if t.alarms.len() > before {
            self.append_alert(&t.alarms[t.alarms.len() - 1]);
        }
    }

    fn append_alert(&self, a: &report::Alarm) {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.alerts_path) {
            use std::io::Write;
            let _ = writeln!(f, "{}", serde_json::to_string(a).unwrap_or_default());
        }
    }
}

fn poller(spec: NodeSpec, shared: Arc<Shared>, interval_ms: u64) {
    let agent = node::agent();
    let mut tick: u64 = 0;
    loop {
        tick += 1;
        match node::fetch_integrity(&agent, &spec.url) {
            Ok(s) => {
                let v = shared.ledger.lock().unwrap().record_roots(&spec.name, s.applied_height, s.roots.clone(), now_ms());
                let mut t = shared.tracker.lock().unwrap();
                {
                    let st = t.nodes.entry(spec.name.clone()).or_default();
                    st.applied_height = Some(s.applied_height);
                    st.finalized_height = s.finalized_height;
                    st.last_sample_ms = now_ms();
                }
                let before = t.alarms.len();
                t.note_verdict(&spec.name, s.applied_height, "roots", &v);
                if t.alarms.len() > before {
                    shared.append_alert(&t.alarms[t.alarms.len() - 1]);
                }
            }
            Err(e) => {
                let mut t = shared.tracker.lock().unwrap();
                let st = t.nodes.entry(spec.name.clone()).or_default();
                st.errors += 1;
                st.last_error = Some(format!("integrity: {e}"));
            }
        }
        // block hashes every third tick (the route returns 200 blocks; ~6 s at the default interval)
        if tick % 3 == 1 {
            match node::fetch_recent_blocks(&agent, &spec.url) {
                Ok(blocks) => {
                    let mut worst: Option<(u64, ledger::Verdict)> = None;
                    let mut top = 0u64;
                    {
                        let mut l = shared.ledger.lock().unwrap();
                        for (h, hash) in &blocks {
                            top = top.max(*h);
                            let v = l.record_block(&spec.name, *h, hash, now_ms());
                            if matches!(v, ledger::Verdict::Disagree { .. }) {
                                worst = Some((*h, v));
                            } else if worst.is_none() && matches!(v, ledger::Verdict::Agree { .. }) {
                                worst = Some((*h, v));
                            }
                        }
                    }
                    let mut t = shared.tracker.lock().unwrap();
                    {
                        let st = t.nodes.entry(spec.name.clone()).or_default();
                        st.last_block_height = Some(top);
                        st.last_sample_ms = now_ms();
                    }
                    if let Some((h, v)) = worst {
                        let before = t.alarms.len();
                        t.note_verdict(&spec.name, h, "block", &v);
                        if t.alarms.len() > before {
                            shared.append_alert(&t.alarms[t.alarms.len() - 1]);
                        }
                    }
                }
                Err(e) => {
                    let mut t = shared.tracker.lock().unwrap();
                    let st = t.nodes.entry(spec.name.clone()).or_default();
                    st.errors += 1;
                    st.last_error = Some(format!("recent: {e}"));
                }
            }
        }
        thread::sleep(Duration::from_millis(interval_ms));
    }
}

fn sse_follower(spec: NodeSpec, shared: Arc<Shared>) {
    let last_seen = Arc::new(AtomicU64::new(0));
    let mut backoff = 2u64;
    loop {
        let name = spec.name.clone();
        let sh = shared.clone();
        let r = node::follow_sse(&spec.url, &mut |c| sh.take_cert(&name, c, "sse"), &last_seen);
        let idle_for = now_ms().saturating_sub(last_seen.load(Ordering::Relaxed));
        if let Err(e) = r {
            let mut t = shared.tracker.lock().unwrap();
            let st = t.nodes.entry(spec.name.clone()).or_default();
            st.errors += 1;
            st.last_error = Some(format!("sse: {e} (idle {idle_for} ms)"));
        }
        thread::sleep(Duration::from_secs(backoff));
        backoff = (backoff * 2).min(60);
    }
}

fn writer(cfg: &Cfg, shared: Arc<Shared>) {
    let status_path = format!("{}/status.json", cfg.status_dir);
    let publish = cfg.publish.clone();
    loop {
        {
            let mut t = shared.tracker.lock().unwrap();
            t.sweep(90_000, 2_000);
            let rep = { let l = shared.ledger.lock().unwrap(); t.report(&l) };
            drop(t);
            if let Ok(js) = serde_json::to_string_pretty(&rep) {
                let tmp = format!("{status_path}.tmp");
                if std::fs::write(&tmp, &js).is_ok() {
                    let _ = std::fs::rename(&tmp, &status_path);
                }
                if let Some(p) = &publish {
                    let tmp = format!("{p}.tmp");
                    if std::fs::write(&tmp, &js).is_ok() {
                        let _ = std::fs::rename(&tmp, p);
                    }
                }
            }
        }
        thread::sleep(Duration::from_secs(5));
    }
}

/// `GET /status` (the same JSON as status.json) and, when configured, `POST /hook/:node` for the nodes' signed pushes.
async fn http(listen: String, hook_listen: Option<String>, shared: Arc<Shared>, secrets: Arc<Mutex<std::collections::HashMap<String, String>>>, status_path: String) {
    use axum::{extract::{Path, State}, http::{HeaderMap, StatusCode}, routing::{get, post}, Router};
    #[derive(Clone)]
    struct S { shared: Arc<Shared>, secrets: Arc<Mutex<std::collections::HashMap<String, String>>>, status_path: String }
    async fn status(State(s): State<S>) -> (StatusCode, [(&'static str, &'static str); 1], String) {
        let body = std::fs::read_to_string(&s.status_path).unwrap_or_else(|_| "{}".into());
        (StatusCode::OK, [("content-type", "application/json")], body)
    }
    async fn hook(State(s): State<S>, Path(node): Path<String>, headers: HeaderMap, body: axum::body::Bytes) -> StatusCode {
        let secret = match s.secrets.lock().unwrap().get(&node).cloned() { Some(x) => x, None => return StatusCode::NOT_FOUND };
        let sig = headers.get(hook::SIGNATURE_HEADER).and_then(|v| v.to_str().ok()).unwrap_or("");
        if !hook::verify(&secret, &body, sig) {
            return StatusCode::UNAUTHORIZED;
        }
        if let Ok(txt) = std::str::from_utf8(&body) {
            if let Some(c) = node::parse_event_json(txt) {
                s.shared.take_cert(&node, c, "webhook");
                if let Some(st) = s.shared.tracker.lock().unwrap().nodes.get_mut(&node) { st.transport = "webhook".into(); }
            }
        }
        StatusCode::NO_CONTENT
    }
    let st = S { shared, secrets, status_path };
    let app = Router::new().route("/status", get(status)).route("/hook/:node", post(hook)).with_state(st.clone());
    let mut tasks = Vec::new();
    for addr in std::iter::once(listen).chain(hook_listen.into_iter()).collect::<std::collections::BTreeSet<_>>() {
        let app = app.clone();
        tasks.push(tokio::spawn(async move {
            match tokio::net::TcpListener::bind(&addr).await {
                Ok(l) => { eprintln!("http on {addr}"); let _ = axum::serve(l, app).await; }
                Err(e) => eprintln!("cannot bind {addr}: {e}"),
            }
        }));
    }
    for t in tasks { let _ = t.await; }
}

fn main() -> anyhow::Result<()> {
    let cfg = parse_args()?;
    std::fs::create_dir_all(&cfg.status_dir)?;
    let shared = Arc::new(Shared { ledger: Mutex::new(Ledger::new(cfg.retain)), tracker: Mutex::new(Tracker::new(&cfg.nodes)), alerts_path: format!("{}/alerts.jsonl", cfg.status_dir) });
    let secrets: Arc<Mutex<std::collections::HashMap<String, String>>> = Arc::new(Mutex::new(Default::default()));
    eprintln!("sigil-integrity: {} nodes, poll {} ms, retain {} heights, status {}/status.json", cfg.nodes.len(), cfg.interval_ms, cfg.retain, cfg.status_dir);
    for spec in cfg.nodes.clone() {
        let s = shared.clone();
        thread::Builder::new().name(format!("poll-{}", spec.name)).spawn(move || poller(spec, s, cfg.interval_ms))?;
    }
    for spec in cfg.nodes.clone() {
        let s = shared.clone();
        thread::Builder::new().name(format!("sse-{}", spec.name)).spawn(move || sse_follower(spec, s))?;
    }
    // the push path: register a signed webhook on every node when a public receiver url is configured, and renew
    // it before the node's 1 h TTL — the node itself refuses private targets, so this is opt-in by configuration
    if let Some(url) = cfg.hook_url.clone() {
        let nodes = cfg.nodes.clone();
        let secrets = secrets.clone();
        let shared = shared.clone();
        thread::spawn(move || {
            let agent = node::agent();
            loop {
                for spec in &nodes {
                    let secret = { let mut s = secrets.lock().unwrap(); s.entry(spec.name.clone()).or_insert_with(|| hex::encode(rand_bytes())).clone() };
                    match hook::register(&agent, &spec.url, &format!("{}/hook/{}", url.trim_end_matches('/'), spec.name), &secret) {
                        Ok(id) => eprintln!("webhook on {} → id {id}", spec.name),
                        Err(e) => { let mut t = shared.tracker.lock().unwrap(); let st = t.nodes.entry(spec.name.clone()).or_default(); st.last_error = Some(format!("webhook register: {e}")); }
                    }
                }
                thread::sleep(Duration::from_secs(45 * 60));
            }
        });
    }
    {
        let s = shared.clone();
        let status_path = format!("{}/status.json", cfg.status_dir);
        let listen = cfg.listen.clone();
        let hook_listen = cfg.hook_listen.clone();
        let secrets = secrets.clone();
        thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio");
            rt.block_on(http(listen, hook_listen, s, secrets, status_path));
        });
    }
    writer(&cfg, shared);
    Ok(())
}

/// 32 random bytes from the OS for a webhook secret (no rand crate: /dev/urandom is always there on the fleet).
fn rand_bytes() -> [u8; 32] {
    use std::io::Read;
    let mut b = [0u8; 32];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut b);
    }
    b
}
