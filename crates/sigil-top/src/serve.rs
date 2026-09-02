// sigil-top/src/serve.rs — Embedded HTTP static-file server (v0.7.0)
//
// No external process. No fluxc binary needed. A single TcpListener thread
// serves the wallet + vite-engine + static assets on localhost:9800.
// The wallet HTML is compiled INTO the binary via include_str!.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::local_api::LocalApi;
use crate::mine_local_api;

/// Start the embedded server on 127.0.0.1:port, serving static_dir.
/// Returns a shutdown signal (set to true to stop the server).
///
/// OUT-OF-THE-BOX: the static dir does NOT need to exist. On a user's own machine
/// (who just downloaded sigil-top) there is no `dist-fluxapp` — the wallet, onboarding,
/// explorer, vite-engine and WASM prover are compiled INTO the binary (`include_str!`)
/// and served from there. We bind regardless; the dir supplies everything else (feed
/// JSON, downloads, site pages).
///
/// 2026-08-26: those built-in surfaces are now served IN PREFERENCE to the dir, not as a
/// fallback from it — see [`embedded_surface`]. A dist folder can no longer shadow the
/// wallet a release just shipped. `SIGIL_UI_PREFER_DISK=1` opts back out for local dev.
pub fn start(static_dir: &str, port: u16) -> Result<Arc<AtomicBool>, String> {
    start_with_api(static_dir, port, None)
}

/// v0.11.0: like [`start`], but also serves the explorer's `/api/v1/*` from a LOCAL
/// verified-spine view (blocks / status / aether-verify / cortex / peers) before
/// proxying to the remote node. Pass `None` for the old pure-proxy behaviour.
pub fn start_with_api(
    static_dir: &str,
    port: u16,
    local_api: Option<Arc<LocalApi>>,
) -> Result<Arc<AtomicBool>, String> {
    let dir = PathBuf::from(static_dir); // may not exist — embedded wallet still serves
    let listener =
        TcpListener::bind(format!("127.0.0.1:{port}")).map_err(|e| format!("bind :{port}: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("nonblocking: {e}"))?;

    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();
    thread::spawn(move || {
        serve_loop(listener, dir, stop_clone, local_api);
    });
    Ok(stop)
}

fn serve_loop(
    listener: TcpListener,
    dir: PathBuf,
    stop: Arc<AtomicBool>,
    local_api: Option<Arc<LocalApi>>,
) {
    // Accept-poll cadence — see the WouldBlock arm below for why this is not a constant.
    let mut last_accept = std::time::Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                last_accept = std::time::Instant::now();
                let dir = dir.clone();
                let api = local_api.clone();
                thread::spawn(move || handle_conn(&mut stream, &dir, api.as_deref()));
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // WHY THIS ISN'T A FLAT 50 ms ANY MORE. The listener is non-blocking so the
                // `stop` flag can end this loop, and the poll interval is therefore charged
                // to every single request as pure waiting. At a flat 50 ms that dominated
                // everything: measured on this box 2026-09-02, `GET /api/v1/mine-wallet`
                // (answered locally, no node call at all) took 35-53 ms, while the same class
                // of request straight to `sigil-api` took 0.5 ms — an ~80x tax on every page
                // load, every balance poll and every Send, and the largest single reason the
                // wallet felt slow. Re-measured after this change: min 2.7 ms.
                //
                // A browser session is a continuous stream of requests, so "was anyone here
                // recently?" separates the two regimes cleanly: 1 ms while a wallet is open
                // (~1000 EWOULDBLOCK accepts/second on one thread, far below the noise floor
                // of a process that also mines and syncs), 25 ms once nobody has connected
                // for five seconds, so an idle sigil-top on a laptop stays cheap.
                let hot = last_accept.elapsed() < Duration::from_secs(5);
                thread::sleep(Duration::from_millis(if hot { 1 } else { 25 }));
            }
            Err(_) => {
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn handle_conn(stream: &mut std::net::TcpStream, dir: &PathBuf, local_api: Option<&LocalApi>) {
    let mut buf = [0u8; 4096];
    let n = match stream.read(&mut buf) {
        Ok(n) if n > 0 => n,
        _ => return,
    };
    let req = String::from_utf8_lossy(&buf[..n]);
    let first_line = req.lines().next().unwrap_or("GET / HTTP/1.1");
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let path = parts.next().unwrap_or("/");
    // v7.0.10-fix: capture the request body so POSTs (e.g. /api/v1/send) proxy correctly. The
    // old proxy hardcoded GET + dropped the body, so every wallet Send hit the node as a GET →
    // "unknown route". Body follows the blank line after the headers (small JSON, one read).
    let req_body = req.split("\r\n\r\n").nth(1).unwrap_or("").to_string();

    // 2026-08-26: LOCAL-ONLY fast-path signing/shield endpoints — pressing Shield/Send/
    // Swap/Bridge from THIS box's own wallet (opened via [W], localhost:9800) completes
    // with ZERO prompts when SIGIL_MINE_SEED is configured, the exact same secret this
    // process already uses to mine. Checked BEFORE the generic /api/`/v1 dispatch below
    // for two reasons: (1) these are POST mutations carrying a JSON body `LocalApi::
    // handle()` was never built to see — it only ever answers a bare path+query, GET-
    // style (see its own module docs); (2) the response crosses this box's own trust
    // boundary only and must NEVER be forwarded to a remote node the way `proxy_api`
    // below would. See `mine_local_api.rs`'s module docs for the full design and the
    // secret-never-leaves-the-process invariant.
    // ── PRIVATE NETWORK ACCESS (2026-08-27) ─────────────────────────────────────────
    //
    // Chrome refuses a request from a PUBLIC origin to the LOOPBACK address space unless
    // the loopback server explicitly opts in. Measured from the hosted wallet:
    //
    //   Access to fetch at 'http://127.0.0.1:9800/api/v1/mine-sign' from origin
    //   'https://sigilgraph.org' has been blocked by CORS policy: Permission was denied
    //   for this request to access the `loopback` address space.
    //
    // That is the entire reason the hosted wallet demanded a recovery phrase for Swap and
    // Bridge while a wallet opened from THIS server (same origin, no loopback crossing)
    // never did. Opting in makes the signer reachable from sigilgraph.org, so the hosted
    // wallet signs with the mining seed already in this process — no phrase, no paste.
    //
    // 🔒 ORIGIN-ALLOWLISTED ON PURPOSE. Everything else here answers
    // `Access-Control-Allow-Origin: *`, which is harmless for reads. It is NOT harmless
    // for a SIGNING endpoint: combined with private-network access, `*` would let ANY web
    // page a user happens to have open ask this process to sign with the mining key —
    // a drive-by signature oracle for the user's own money. So the preflight reflects only
    // known SIGIL origins and refuses the rest, and the allowance is granted ONLY on the
    // `mine_local_api` signing paths, never on the generic proxy below.
    fn pna_allowed_origin(req: &str) -> Option<String> {
        const ALLOWED: &[&str] = &[
            "https://sigilgraph.org",
            "https://www.sigilgraph.org",
            "https://sigilgraph.quillon.xyz",
            "http://127.0.0.1:9800",
            "http://localhost:9800",
        ];
        let origin = req
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("origin:"))
            .and_then(|l| l.split_once(':').map(|(_, v)| v.trim().to_string()))?;
        ALLOWED.contains(&origin.as_str()).then_some(origin)
    }

    // Preflight for the signing endpoints. Answered before anything else so the generic
    // `*` responses below can never satisfy a private-network preflight by accident.
    if method == "OPTIONS" && (mine_local_api::is_local_path(path) || pay::is_local_path(path)) {
        let resp = match pna_allowed_origin(&req) {
            Some(origin) => format!(
                "HTTP/1.1 204 No Content\r\n\
                 Access-Control-Allow-Origin: {origin}\r\n\
                 Vary: Origin\r\n\
                 Access-Control-Allow-Methods: POST, OPTIONS\r\n\
                 Access-Control-Allow-Headers: content-type\r\n\
                 Access-Control-Allow-Private-Network: true\r\n\
                 Access-Control-Max-Age: 600\r\n\
                 Content-Length: 0\r\nConnection: close\r\n\r\n"
            ),
            // No ACAO at all -> the browser blocks it. An unknown site gets no signer.
            None => "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string(),
        };
        let _ = stream.write_all(resp.as_bytes());
        let _ = stream.flush();
        return;
    }

    // `/api/v1/pay` rides the SAME origin-allowlisted, private-network-gated dispatch as
    // the three `mine_local_api` signing endpoints, and must: it spends with the mining
    // seed, so it needs exactly that protection and no less.
    if method == "POST" && (mine_local_api::is_local_path(path) || pay::is_local_path(path)) {
        let (status, resp_body) = if pay::is_local_path(path) {
            pay::handle(path, &req_body)
        } else {
            mine_local_api::handle(path, &req_body)
        };
        // Echo the specific allowlisted origin rather than `*`: a response to a request
        // that crossed into the loopback address space must name its origin, and `*` is
        // the wrong answer for a signing endpoint regardless (see `pna_allowed_origin`).
        // A same-origin call (the wallet opened from this server) sends no Origin header
        // and needs no ACAO at all, so `*` remains the correct fallback there.
        let acao = pna_allowed_origin(&req).unwrap_or_else(|| "*".to_string());
        let resp = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: {acao}\r\nVary: Origin\r\nAccess-Control-Allow-Private-Network: true\r\nConnection: close\r\n\r\n",
            resp_body.len()
        );
        let _ = stream.write_all(resp.as_bytes());
        let _ = stream.write_all(resp_body.as_bytes());
        let _ = stream.flush();
        return;
    }

    // /api/* → LOCAL-FIRST. If this node has a verified-spine view that can answer the
    // request (blocks / status / aether-verify / cortex / peers), serve it locally
    // (trust-minimised). Otherwise relay to the SIGIL node over std TCP — same-origin,
    // no CORS / mixed-content. Default node is the public sigil-rpcd; override with
    // SIGIL_NODE_URL to point at a LOCAL node.
    // 2026-08-19: also proxy bare /v1/* the same way. sigil-api mounts MOST routes
    // under both /v1/* and /api/v1/* (see its own route table), but the native SIGIL
    // bridge (/v1/bridge/lock etc., bridge.rs::submit_lock) was only ever registered
    // at the bare /v1/ prefix — no /api/v1/ mirror exists for it. The wallet's Bridge
    // modal called /api/v1/bridge/lock (matching every OTHER mutation route in this
    // app) and got a real HTTP 404 back, live (operator-reported: pressed Lock &
    // Bridge on a real 42-SIGIL balance, saw "✗ HTTP 404"). Fixing this by fixing the
    // ONE modal to call /v1/bridge/lock wouldn't be enough by itself — serve.rs never
    // proxied bare /v1/* at all before this line, so that path would 404 right here
    // regardless of what sigil-api registers. This is the actual fix: forward /v1/*
    // through the exact same LOCAL-FIRST → proxy_api path /api/* already gets.
    if path.starts_with("/api/") || path.starts_with("/v1/") {
        if let Some(api) = local_api {
            if let Some(body) = api.handle(path) {
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
                return;
            }
        }
        let (status, body, ct) = proxy_api(&method, path, &req_body);
        let resp = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(resp.as_bytes());
        let _ = stream.write_all(&body);
        let _ = stream.flush();
        return;
    }

    // Decode URL (static files)
    let path = path.split('?').next().unwrap_or(path);
    let path = if path == "/" { "/sigil-wallet-tron.html" } else { path };
    let safe = path.trim_start_matches('/').replace("..", "").replace('\\', "");

    let (status, body, ct) = serve_file(dir, &safe);
    // v0.59 FEED-LESS HOST FIX: a client with no co-located producer doesn't have the live DAG
    // feed files on disk (they're published only on the producer box), so serve_file 404s, the
    // explorer's same-origin fetch fails, and it falls back to the rpcd MINING sub-chain — showing
    // height ~3k / 56k-high blocks instead of the real multi-million DAG chain. Proxy the known
    // feed files from the sigilgraph host SERVER-SIDE (reqwest HTTPS — no CORS, no mixed-content)
    // so the explorer sees the REAL chain on any host. Everything else keeps its normal 404.
    let (status, body, ct): (&str, Vec<u8>, &str) = if status.starts_with("404") && is_feed_path(&safe) {
        match proxy_feed(&safe) {
            Some((b, c)) => ("200 OK", b, c),
            None => (status, body, ct),
        }
    } else {
        (status, body, ct)
    };

    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}

/// Remote host publishing the live DAG-chain feeds (recent-blocks, tip, tip-live). Override with
/// `SIGIL_FEED_URL`. Used only for the feed-less-host fallback in `handle_conn`.
fn feed_host() -> String {
    std::env::var("SIGIL_FEED_URL").unwrap_or_else(|_| "https://sigilgraph.fluxapp.xyz".into())
}

/// The static feed files a feed-less client may request but won't have on disk. Restricted to this
/// known set so the fallback can never become an open proxy.
fn is_feed_path(safe: &str) -> bool {
    matches!(
        safe,
        "sigil-recent-blocks.json" | "sigil-tip.json" | "sigil-tip-live.json" | "sigil-anchor-key.json"
    )
}

/// Fetch a feed file from `feed_host()` over HTTPS. Blocking (we run per-connection in a thread);
/// `None` on any error so the caller falls back to the original 404.
fn proxy_feed(safe: &str) -> Option<(Vec<u8>, &'static str)> {
    let url = format!("{}/{}", feed_host().trim_end_matches('/'), safe);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(6))
        .build()
        .ok()?;
    let resp = client.get(&url).send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.bytes().ok()?.to_vec();
    Some((body, "application/json; charset=utf-8"))
}

/// True when `safe` is exactly `name`, or a path whose last segment is `name`.
///
/// Byte-compares the separator instead of building `format!("/{name}")` so this stays
/// allocation-free on the hot request path.
fn names(safe: &str, name: &str) -> bool {
    safe == name
        || (safe.len() > name.len()
            && safe.ends_with(name)
            && safe.as_bytes()[safe.len() - name.len() - 1] == b'/')
}

/// Every UI surface compiled INTO the binary, so a downloaded `sigil-top` carries the
/// complete wallet + explorer + prover with no filesystem overlay and no network fetch.
///
/// 2026-08-26 — THIS IS NOW CHECKED BEFORE THE FILESYSTEM, and that reversal is the
/// whole point. Previously `serve_file` read `dir` first and only fell back here, so any
/// box that happened to have a `dist-fluxapp/` (every producer box, and any user who ever
/// unpacked one) served whatever stale HTML was sitting on disk and silently shadowed the
/// wallet shipped inside the binary. Measured on Epsilon the day this changed: `GET /`
/// returned an Aug-19 `sigil-wallet-tron.html` (240,979 B) whose Send button still POSTed
/// to the retired `/api/v1/send`, while the v7.1.86 wallet compiled into the running
/// binary (321,359 B, zero-prompt signing via `mine_local_api`) was never reachable. A
/// shipped fix that a week-old file on disk can veto is not shipped.
///
/// Consequence to keep in mind when editing: a release now genuinely updates these
/// surfaces for everyone, so `dist-fluxapp` is no longer a way to hot-patch the wallet
/// without cutting a build. Set `SIGIL_UI_PREFER_DISK=1` for that (see `prefer_disk_ui`).
// 2026-08-26: gui/sigil-wallet-tron-embedded.html + gui/enter-sigil.html gained the
// MetaMask/Polygon surface (connect, wallet-from-signature, bidirectional bridge modal,
// miner drill-down, address book, balance-history chart). sigil-metamask.js is INLINED
// into both because only the surfaces listed below are served on :9800 — an external
// script-src would 404 there. This comment also exists to bump the .rs mtime: the flux
// wrapper cache keys only .rs sources, so a gui/-only edit would NOT rebuild this unit.
/// One-action send, injected into the wallet THIS server hands out.
///
/// Not part of `gui/sigil-wallet-tron-embedded.html`: that file is shared with
/// `sigilgraph.org`, which has no `/api/v1/pay` and no local seed. Injecting here keeps the
/// hosted copy byte-identical to what it serves today while the copy served from this box —
/// the one `[W]` opens, the one a user who just downloaded sigil-top sees — gets the
/// single-call payment path. The script is self-contained and hands control straight back to
/// the page's own `doUnifiedSend` on a 404, so a build with no seed behaves exactly as before.
const ONE_ACTION_SEND_JS: &str = r##"
<script>
/* ══ ONE-ACTION SEND, INJECTED BY sigil-top's :9800 SERVER ═══════════════════════════
 *
 * Injected by crates/sigil-top/src/serve.rs (see `inject_one_action_send`), NOT part of
 * gui/sigil-wallet-tron-embedded.html. It only ever runs on this box's own :9800, where
 * /api/v1/pay exists; the hosted copy at sigilgraph.org is untouched and keeps its
 * current behaviour.
 *
 * WHY IT OVERRIDES doUnifiedSend RATHER THAN PATCHING IT. The page's own unified flow
 * cannot succeed, for two independent SCOPE bugs (both read from source, both in the
 * shipped file):
 *
 *   1. `showReceipt` is declared inside the IIFE that spans the send script — it is NOT
 *      on `window`. doUnifiedSend's step 1 detects a successful shield by swapping
 *      `window.showReceipt` for a probe; doShield calls the LEXICAL `showReceipt`, so the
 *      probe never fires, `shielded` stays false, and step 1 reports FAILURE on every
 *      shield that actually succeeded.
 *   2. doPrivateSend lives in a LATER, separate IIFE and calls the same non-global
 *      `showReceipt` — a ReferenceError, swallowed by its own try/catch. `data-kind="send"`
 *      is therefore never set on the receipt pane, and step 2's success test
 *      (`sent && rcPane.getAttribute('data-kind')==='send'`) can never be true.
 *
 * So the browser flow reports failure whether or not money moved. Overriding the entry
 * point sidesteps both, and moves the sequencing to the server where it is one call.
 *
 * UNITS. `RAW_PER_UNIT` is 1e8, matching what the REST of this page uses for both display
 * and entry. The chain is 10 dp (`sigil_state::SIGIL_DECIMALS = 10`), so every SIGIL label
 * on this page is 100x off — a real, separate bug in the HTML. It is deliberately NOT
 * "fixed" here: display and entry are consistent with each other today, and changing only
 * the entry scale would make a typed amount mean 100x what the balance above it claims.
 */
(function(){
  if (window.__sigilOneActionSend) return;
  window.__sigilOneActionSend = true;

  var RAW_PER_UNIT = 100000000n;   // see UNITS note above
  var DP = 8;
  /* The page defines doUnifiedSend in a script ABOVE this one, so it is already
     here to hand back to on a 404. */
  var legacy = (typeof window.doUnifiedSend === 'function') ? window.doUnifiedSend : null;

  function el(id){ return document.getElementById(id); }
  function toRaw(s){
    var p = String(s).split('.');
    return BigInt(p[0]||'0')*RAW_PER_UNIT + BigInt(((p[1]||'')+'00000000').slice(0,DP));
  }
  function fmt(raw){
    var b = BigInt(raw), w = b/RAW_PER_UNIT, f = (b%RAW_PER_UNIT).toString().padStart(DP,'0');
    return w + '.' + f;
  }
  function msg(t,c){ var m=el('uniMsg'); if(m){ m.textContent=t||''; m.style.color=c||'#8fb3c2'; } }
  function step(n,state,label){
    var e = el('uniStep'+n); if(!e) return;
    var mark = {todo:'○',busy:'◐',done:'●',fail:'✗'}[state]||'○';
    var col  = {todo:'#5a93a8',busy:'#6df3ff',done:'#00e0c6',fail:'#ff8a8a'}[state]||'#5a93a8';
    e.textContent = mark+' '+n+' · '+label; e.style.color = col;
  }
  function myNotesRaw(addr){
    try{
      var a = JSON.parse(localStorage.getItem('sigil-shielded-notes-'+addr)||'[]');
      /* Only ONE note is ever spent (the circuit is 1-in/2-out), and serve.rs reads the
         request in a single 4 KB read, so send the biggest few rather than the lot. */
      return a.filter(function(n){ return n && !n.spent; })
              .map(function(n){ return {index:Number(n.index), value:String(n.value)}; })
              .sort(function(x,y){ return (BigInt(y.value) > BigInt(x.value)) ? 1 : -1; })
              .slice(0, 24);
    }catch(e){ return []; }
  }
  function recordNote(addr,index,value){
    try{
      var k='sigil-shielded-notes-'+addr;
      var a=JSON.parse(localStorage.getItem(k)||'[]');
      a.push({index:index, value:String(value), ts:Date.now()});
      localStorage.setItem(k, JSON.stringify(a));
    }catch(e){}
  }
  function short(h){ return h ? (h.slice(0,10)+'…'+h.slice(-8)) : '—'; }
  function row(label, value, colour){
    return '<div style="display:flex;justify-content:space-between;gap:12px;padding:8px 0;border-bottom:1px solid rgba(33,212,253,.10);font-family:\'JetBrains Mono\';font-size:10.5px">'
         + '<span style="color:#5a93a8">'+label+'</span>'
         + '<span style="color:'+(colour||'#cfe9f2')+';word-break:break-all;text-align:right">'+value+'</span></div>';
  }
  /* Receipt written straight into the DOM — deliberately NOT via the page's showReceipt,
     which is unreachable from here (bug 2 above). */
  function receipt(kind, amtStr, rows, foot){
    var rc = el('sendReceipt'); if(!rc) return;
    var icon = el('rcIcon'), head = el('rcHead'), amt = el('rcAmt'),
        body = el('rcRows'), ft = el('rcFoot');
    var spec = {
      pending: ['⏳','PAYMENT STARTED','#6df3ff'],
      paid:    ['🔒','SENT PRIVATELY','#c0a8fa'],
      failed:  ['✗','NOT SENT','#fbbf24']
    }[kind] || ['⏳','PAYMENT STARTED','#6df3ff'];
    if(icon) icon.textContent = spec[0];
    if(head){ head.textContent = spec[1]; head.style.color = spec[2]; }
    if(amt)  amt.textContent = amtStr + ' SIGIL';
    if(body) body.innerHTML = rows;
    if(ft)   ft.textContent = foot || '';
    rc.setAttribute('data-kind', kind === 'paid' ? 'send' : kind);
    var form = el('sendForm'); if(form) form.style.display='none';
    rc.style.display='flex';
  }

  async function post(path, payload){
    var r = await fetch(path, {method:'POST', headers:{'Content-Type':'application/json'},
                              body: JSON.stringify(payload)});
    var j = null; try{ j = await r.json(); }catch(e){}
    return {status:r.status, ok:r.ok, json:j};
  }

  window.doUnifiedSend = async function(){
    var btn  = el('uniGo');
    var addr = (el('uniAddr').value||'').trim().toLowerCase();
    var amtS = (el('uniAmt').value||'').trim();
    msg('');
    if(!/^[0-9a-f]{64}$/.test(addr)){ msg("Enter the recipient's 64-character SIGIL address first.", '#ff8a8a'); return; }
    if(!/^\d+(\.\d{1,8})?$/.test(amtS) || Number(amtS) <= 0){ msg('Enter an amount greater than 0 (up to 8 decimals).', '#ff8a8a'); return; }

    var from = (window.MINE_ADDR||window.ADDR||'').toLowerCase();
    if(!/^[0-9a-f]{64}$/.test(from)){ msg('No wallet address loaded.', '#ff8a8a'); return; }

    var amountRaw = toRaw(amtS);
    var steps = el('uniSteps'); if(steps) steps.style.display='flex';
    step(1,'busy','submitting your payment');
    step(2,'todo','confirming on the DAG');
    if(btn){ btn.disabled = true; btn.style.opacity = '.55'; }
    function done(){ if(btn){ btn.disabled=false; btn.style.opacity='1'; } }

    var res;
    try{
      res = await post('/api/v1/pay', {
        to: addr, amount: amountRaw.toString(), memo: '',
        from: from, notes: myNotesRaw(from)
      });
    }catch(e){ res = {status:0, ok:false, json:null}; }

    /* 404 = this box has no local seed (or an older sigil-top). Hand straight back to the
       page's own flow — this override is additive, never a required step. */
    if(res.status === 404 || res.status === 0){
      step(1,'todo','move funds into your private balance');
      step(2,'todo','pay the recipient privately');
      done();
      if(typeof legacy === 'function') return legacy.apply(this, arguments);
      msg('Local payment service unavailable on this host.', '#fbbf24');
      return;
    }

    var j = res.json || {};
    if(!j.ok){
      step(1,'fail','submitting your payment');
      msg(j.error || 'The payment could not be started.', '#ff8a8a');
      done();
      return;
    }

    /* Already had a covering private note → this IS the payment txid, right now. */
    if(j.stage === 'paid'){
      step(1,'done','private balance already covered it');
      step(2,'done','paid');
      window.__lastTxid = j.txid || '';
      receipt('paid', fmt(amountRaw),
        row('to', short(addr)) + row('payment txid', short(j.txid), '#c0a8fa'),
        'Paid from a note that was already in your private balance.');
      if(window.refresh) setTimeout(window.refresh, 600);
      done();
      return;
    }

    /* Otherwise the shield is already on-chain and its txid exists NOW. Show it, then
       track the payment leg. The receipt never calls this a payment txid. */
    window.__lastTxid = j.shield_txid || j.txid || '';
    if(j.stage === 'shielding'){
      step(1,'done','funds moved into your private balance');
      if(j.shield_index !== null && j.shield_index !== undefined){
        recordNote(from, j.shield_index, j.shield_value);
      }
    }else{
      step(1,'done','private balance already covered it');
    }
    step(2,'busy','confirming on the DAG, then paying');
    receipt('pending', fmt(amountRaw),
      row('to', short(addr))
      + (j.shield_txid ? row('shield txid (on-chain now)', short(j.shield_txid), '#00e0c6') : '')
      + row('payment txid', 'waiting for the note to land…', '#6df3ff'),
      'Your funds are in the private pool. The payment is submitted as soon as that note '
      + 'appears on the DAG — this window updates itself.');

    var job = j.job, tries = 0;
    while(tries < 90){
      tries++;
      await new Promise(function(r){ setTimeout(r, 2000); });
      var s;
      try{ s = await post('/api/v1/pay/status', {job: job}); }catch(e){ continue; }
      var sj = (s && s.json) || {};
      if(sj.stage === 'paid'){
        step(2,'done','paid');
        window.__lastTxid = sj.txid || '';
        receipt('paid', fmt(amountRaw),
          row('to', short(addr))
          + (sj.shield_txid ? row('shield txid', short(sj.shield_txid), '#00e0c6') : '')
          + row('payment txid', short(sj.txid), '#c0a8fa'),
          'Sent privately. Nothing on-chain links the shield above to this payment.');
        if(window.refresh) setTimeout(window.refresh, 600);
        done();
        return;
      }
      if(sj.stage === 'failed'){
        step(2,'fail','payment could not be built');
        receipt('failed', fmt(amountRaw),
          row('to', short(addr))
          + (sj.shield_txid ? row('shield txid', short(sj.shield_txid), '#00e0c6') : '')
          + row('why', sj.error || 'unknown', '#fbbf24'),
          'Your money is NOT lost — it is in your private balance. Press SEND again; '
          + 'the note is already there, so it will pay directly this time.');
        done();
        return;
      }
    }
    step(2,'fail','still confirming');
    msg('Still waiting for the DAG. Your funds are in your private balance — press SEND again in a moment.', '#fbbf24');
    done();
  };

})();
</script>
"##;

/// Splice [`ONE_ACTION_SEND_JS`] in just before `</body>`, so it runs after every script the
/// page defines (it needs `window.doUnifiedSend` to already exist in order to capture it as
/// the fallback). Appends if there is no `</body>` at all rather than dropping the script.
fn inject_one_action_send(html: &str) -> Vec<u8> {
    match html.rfind("</body>") {
        Some(i) => {
            let mut out = String::with_capacity(html.len() + ONE_ACTION_SEND_JS.len() + 8);
            out.push_str(&html[..i]);
            out.push_str(ONE_ACTION_SEND_JS);
            out.push_str(&html[i..]);
            out.into_bytes()
        }
        None => {
            let mut out = html.to_string();
            out.push_str(ONE_ACTION_SEND_JS);
            out.into_bytes()
        }
    }
}

fn embedded_surface(safe: &str) -> Option<(Vec<u8>, &'static str)> {
    const HTML: &str = "text/html; charset=utf-8";
    const JS: &str = "text/javascript; charset=utf-8";

    // Wallet visual layer — kept beside the embedded HTML it styles.
    if names(safe, "sigil-wallet-codex.css") {
        return Some((
            include_str!("../../../gui/sigil-wallet-codex.css").as_bytes().to_vec(),
            "text/css; charset=utf-8",
        ));
    }
    // The wallet itself. Carries the #stats network-stats modal ([T]), the #activity
    // deep-link ([B]) which opens the Explorer same-origin so its /api/v1/search proxies
    // to THIS node, the Swap modal, and the shielded Send modal whose local fast path
    // signs with SIGIL_MINE_SEED and never prompts for a recovery phrase.
    // NOTE: the flux wrapper cache keys only .rs sources, so an edit to the embedded HTML
    // alone does NOT rebuild this unit — touch this file alongside any gui/ change.
    // 2026-09-02: the gate (gui/enter-sigil.html, imported from sigilgraph.org) redirects
    // to sigil-wallet-tron-embedded.html, the hosted file name — alias it here or a
    // fresh wallet created at localhost:9800/enter-sigil.html lands on a 404.
    if names(safe, "sigil-wallet-tron.html") || names(safe, "sigil-wallet-tron-embedded.html") {
        return Some((
            inject_one_action_send(include_str!("../../../gui/sigil-wallet-tron-embedded.html")),
            HTML,
        ));
    }
    // Onboarding — 6-word mnemonic → fresh wallet. The wallet gate redirects fresh users
    // here; it must be served or [W] dead-ends on a 404.
    if names(safe, "enter-sigil.html") {
        return Some((include_str!("../../../gui/enter-sigil.html").as_bytes().to_vec(), HTML));
    }
    if names(safe, "sigil-explorer.html") {
        return Some((include_str!("../../../gui/sigil-explorer.html").as_bytes().to_vec(), HTML));
    }
    if names(safe, "vite-engine.html") {
        return Some((include_str!("../../../gui/vite-engine-embedded.html").as_bytes().to_vec(), HTML));
    }
    if names(safe, "sigil-tron-p2p.js") {
        return Some((
            include_str!("../../../gui/sigil-wallet/dist-tron-p2p/sigil-tron-p2p.js").as_bytes().to_vec(),
            JS,
        ));
    }
    // The private-send WASM prover, so a shielded spend can be proven with no CDN.
    if names(safe, "wasm/sigil_shield.js") {
        return Some((include_str!("../../../gui/wasm/sigil_shield.js").as_bytes().to_vec(), JS));
    }
    if names(safe, "wasm/sigil_shield_bg.wasm") {
        return Some((include_bytes!("../../../gui/wasm/sigil_shield_bg.wasm").to_vec(), "application/wasm"));
    }
    // ── 2026-09-02: parity with sigilgraph.org ──────────────────────────────────
    // The wallet compiled in here is now the SAME file sigilgraph.org serves (Nation ›
    // Welfare + SIGIL OS, v2 Argon2id derivation, per-commitment /shielded/has check).
    // That file pulls these siblings by RELATIVE path, and only what is listed in this
    // function exists on :9800 — anything missing 404s silently (sigil-metamask.js and
    // sigil-pq.mjs had been 404ing here since 2026-08-26). Keep this list in step with
    // `<script src=`/`import` in the three HTML surfaces above.
    if names(safe, "flux-os.html") {            // SIGIL OS — the Nation › SIGIL OS modal's iframe
        return Some((include_str!("../../../gui/flux-os.html").as_bytes().to_vec(), HTML));
    }
    if names(safe, "sigil-nation-whitepaper.html") {
        return Some((include_str!("../../../gui/sigil-nation-whitepaper.html").as_bytes().to_vec(), HTML));
    }
    if names(safe, "sigil-argon2.mjs") {        // v2 (12-word) key derivation — Send is dead without it
        return Some((include_str!("../../../gui/sigil-argon2.mjs").as_bytes().to_vec(), JS));
    }
    if names(safe, "sigil-ed25519.mjs") {
        return Some((include_str!("../../../gui/sigil-ed25519.mjs").as_bytes().to_vec(), JS));
    }
    if names(safe, "sigil-sha3.mjs") {
        return Some((include_str!("../../../gui/sigil-sha3.mjs").as_bytes().to_vec(), JS));
    }
    if names(safe, "sigil-sha512.mjs") {
        return Some((include_str!("../../../gui/sigil-sha512.mjs").as_bytes().to_vec(), JS));
    }
    if names(safe, "sigil-pq.mjs") {
        return Some((include_str!("../../../gui/sigil-pq.mjs").as_bytes().to_vec(), JS));
    }
    if names(safe, "sigil-metamask.js") {
        return Some((include_str!("../../../gui/sigil-metamask.js").as_bytes().to_vec(), JS));
    }
    if names(safe, "wsigil-market.js") {        // the wSIGIL/USDC ribbon; absent = ribbon shows nothing
        return Some((include_str!("../../../gui/wsigil-market.js").as_bytes().to_vec(), JS));
    }
    None
}

/// `SIGIL_UI_PREFER_DISK=1` restores the pre-2026-08-26 order (filesystem first), for the
/// one case that reversal takes away: iterating on a surface in `FLUX_STATIC_DIR` without
/// rebuilding the binary. Off by default — a shipped wallet should not be overridable by
/// a stale file nobody remembers putting there.
fn prefer_disk_ui() -> bool {
    std::env::var("SIGIL_UI_PREFER_DISK")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn serve_file(dir: &PathBuf, safe: &str) -> (&'static str, Vec<u8>, &'static str) {
    // 1. Built-in surfaces win over the filesystem — see `embedded_surface` for why.
    if !prefer_disk_ui() {
        if let Some((body, ct)) = embedded_surface(safe) {
            return ("200 OK", body, ct);
        }
    }

    // 2. Filesystem: everything that is NOT a built-in surface (feed JSON, downloads,
    //    site pages, any asset a deployment adds) still comes from `dir`.
    let file_path = dir.join(safe);
    if file_path.exists() && file_path.starts_with(dir) {
        if let Ok(data) = std::fs::read(&file_path) {
            return ("200 OK", data, content_type(safe));
        }
    }

    // 3. SPA fallback: index.html in the requested path's directory.
    if let Some(slash) = safe.rfind('/') {
        let parent = &safe[..slash];
        let index_path = dir.join(format!("{parent}/index.html"));
        if index_path.exists() {
            if let Ok(data) = std::fs::read(&index_path) {
                return ("200 OK", data, "text/html; charset=utf-8");
            }
        }
    }

    // 4. Disk-preferred mode still falls back to the built-in copy, so an override dir
    //    that only carries one file doesn't 404 every other surface.
    if prefer_disk_ui() {
        if let Some((body, ct)) = embedded_surface(safe) {
            return ("200 OK", body, ct);
        }
    }

    ("404 Not Found", b"404 Not Found\n".to_vec(), "text/plain")
}

/// Proxy a `GET /api/...` to the SIGIL node over std TCP and relay its JSON body.
/// Node = `SIGIL_NODE_URL` env (point at a LOCAL node), else the live braid API.
/// The node speaks plain HTTP, so this works from the http://localhost wallet with
/// no CORS / mixed-content issues.
///
/// ⚠️ BUG FIX (2026-08-16): this defaulted to `:8099` (sigil-rpcd) — dead since
/// 2026-08-15, frozen at height 325651. `engine_node_url()` (the mining default,
/// a few hundred lines away) already correctly points at `:18181` (the live
/// sigil-api braid money API, same host). This function — everything the wallet's
/// balance/status/send/search calls go through — never got the same fix, so the
/// wallet kept silently showing pre-reset-era balances from the dead chain with
/// no visible error. Operator-reported live: wallet showed 6,211 SIGIL from the
/// wallet's session cache (refresh() deliberately never regresses on a null
/// fetch, so an old good value sticks around forever if new fetches quietly
/// fail) while the server-verified balance for that exact address was 0 on the
/// live chain. sigil-api aliases /api/v1/balance + /api/v1/supply (this same
/// session, sigil-api/src/lib.rs) so this port actually serves what the wallet
/// asks for now — status/recent/search/send are NOT yet ported (separate,
/// larger follow-up: sigil-api has no transaction-history indexing yet).
fn proxy_api(method: &str, path_and_query: &str, req_body: &str) -> (String, Vec<u8>, &'static str) {
    let node = std::env::var("SIGIL_NODE_URL")
        .unwrap_or_else(|_| "http://sigilgraph.quillon.xyz:18181".into());
    let hostport = node
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let hostport = hostport.split('/').next().unwrap_or(hostport);
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(18181)),
        None => (hostport.to_string(), 18181),
    };

    let mut stream = match std::net::TcpStream::connect((host.as_str(), port)) {
        Ok(s) => s,
        Err(e) => {
            return (
                "502 Bad Gateway".to_string(),
                format!("{{\"error\":\"node unreachable: {e}\"}}").into_bytes(),
                "application/json",
            )
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(6)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(6)));
    // v7.0.10-fix: forward the REAL method + body (was hardcoded GET) so POST /api/v1/send etc.
    // reach the node's POST routes instead of falling through to "unknown route".
    let req = format!(
        "{method} {path_and_query} HTTP/1.1\r\nHost: {host}\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{req_body}",
        req_body.len()
    );
    if stream.write_all(req.as_bytes()).is_err() {
        return (
            "502 Bad Gateway".to_string(),
            b"{\"error\":\"node write failed\"}".to_vec(),
            "application/json",
        );
    }
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let header_end = raw.windows(4).position(|w| w == b"\r\n\r\n");
    // Relay the UPSTREAM's real status line ("200 OK", "404 Not Found", ...)
    // instead of hardcoding success. Previously this always returned "200 OK"
    // regardless of what the node actually answered, so a real 404 (e.g. a
    // route that didn't exist yet, like /api/v1/send before it was added)
    // reached the browser as a fake 200 with an unparseable body — the wallet
    // saw `HTTP 200` in its error banner while the request had actually
    // failed. Parsed from the head, not assumed.
    let status = {
        let head = match header_end {
            Some(i) => &raw[..i],
            None => raw.as_slice(),
        };
        String::from_utf8_lossy(head)
            .lines()
            .next()
            .and_then(|line| line.splitn(2, ' ').nth(1))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "502 Bad Gateway".to_string())
    };
    // split off the HTTP headers; relay the body (sigil-rpcd sends Content-Length + close)
    let body = match header_end {
        Some(i) => raw[i + 4..].to_vec(),
        None => raw,
    };
    (status, body, "application/json")
}

fn content_type(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".html") || lower.ends_with(".htm") {
        "text/html; charset=utf-8"
    } else if lower.ends_with(".js") {
        "application/javascript; charset=utf-8"
    } else if lower.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if lower.ends_with(".json") {
        "application/json"
    } else if lower.ends_with(".svg") {
        "image/svg+xml"
    } else if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".ico") {
        "image/x-icon"
    } else if lower.ends_with(".wasm") {
        "application/wasm"
    } else {
        "application/octet-stream"
    }
}


// ══ ONE-ACTION PAYMENT (`POST /api/v1/pay`) ═════════════════════════════════════════
//
// WHY THIS EXISTS. SIGIL has no transparent send — `sigil_tx::SHIELDED_ONLY_HEIGHT == 0`,
// so `POST /v1/send` refuses unconditionally (sigil-api/src/lib.rs:416). A payment on this
// chain IS `shield` → `shielded_send`. Mining, meanwhile, pays out as TRANSPARENT coinbase
// balance. So the very first thing a new user has (mined rewards) is in the one domain from
// which nothing can be paid, and the wallet asked them to understand and sequence the ramp
// themselves. That is the whole of "sending is too complicated".
//
// `mine_local_api` already performs each leg completely on this box (derive → build → prove
// a real `spend_full_v4` STARK → sign → submit). What was missing is the ORCHESTRATION: a
// single call that decides whether a shield is even needed, sizes it correctly, submits it,
// and then keeps trying the payment leg until the freshly-shielded note is visible in the
// pool. That is what this module is. It composes `mine_local_api::handle` and adds no
// crypto of its own — a spend/proof bug cannot be introduced here.
//
// ── WHAT "INSTANT TXID" HONESTLY MEANS HERE ──────────────────────────────────────────
// The payment txid is `blake3(encode(SigilTx::ShieldedSend{ .., proof }))`. The proof cannot
// exist until the note being spent has LANDED in the pool, which needs a block. So for a
// user starting from transparent mined balance there is no way — client-side precomputation
// included — to know the payment txid at the moment Confirm is pressed. What CAN be returned
// in that instant is the SHIELD txid, which is real, on-chain and checkable at
// `/v1/transactions/<hash>`. So:
//
//   * a covering shielded note already exists  → the payment is done inline and the response
//     carries the PAYMENT txid (`stage:"paid"`). Cost is one STARK proof.
//   * no covering note                          → the shield is submitted and the response
//     returns immediately with the SHIELD txid (`stage:"shielding"`) plus a `job` id; a
//     background thread finishes the payment leg and `/api/v1/pay/status` reports its txid.
//
// The response never calls a shield txid a payment txid. `stage` is the field that says
// which one you are holding.
//
// ── THE 1-IN/2-OUT RULE, WHICH IS WHY "SHIELD THE SHORTFALL" IS WRONG ────────────────
// The spend circuit takes ONE input note. A payment therefore needs ONE note worth at least
// `amount + SHIELDED_FEE` — the SUM of your notes is irrelevant. Topping up by the shortfall
// (`amount - sum(notes)`) can leave a wallet holding plenty of value and still unable to pay,
// forever. So when a shield is needed this module shields exactly ONE ramp denomination that
// is >= amount + fee, which by construction produces a single covering note.
pub mod pay {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    pub const PAY: &str = "/api/v1/pay";
    pub const PAY_STATUS: &str = "/api/v1/pay/status";

    /// How long the background leg keeps trying before giving up. A shield lands as soon as
    /// it is included in a block (measured live 2026-09-02: 6.6 blk/s), not at finality
    /// (512 blocks ~= 78 s) — but the pool's leaf view updates on commit, so this is sized
    /// generously past the finality window rather than tightly against block time.
    const MAX_ATTEMPTS: u32 = 60;
    const RETRY_EVERY: Duration = Duration::from_secs(2);

    /// Paths `serve.rs` routes here. Deliberately reuses the SAME origin-allowlisted,
    /// private-network-gated dispatch as `mine_local_api`: this endpoint spends money with
    /// the mining seed, so it needs exactly that protection and no less.
    pub fn is_local_path(path: &str) -> bool {
        path == PAY || path == PAY_STATUS
    }

    /// Live state of one payment. Serialised verbatim as the `/api/v1/pay/status` body.
    ///
    /// SECRET-SAFETY: every field here is a public outcome (a txid, a note index, a
    /// denomination, an error string) — the same invariant `mine_local_api` documents. The
    /// seed and anything derived from it never appears in this struct.
    #[derive(Clone, Default, serde::Serialize)]
    pub struct Job {
        /// `shielding` | `waiting` | `paid` | `failed`
        pub stage: String,
        pub shield_txid: String,
        pub shield_index: Option<u64>,
        pub shield_value: String,
        pub txid: String,
        pub attempts: u32,
        pub error: Option<String>,
        pub updated_ms: u64,
    }

    fn jobs() -> &'static Mutex<HashMap<String, Job>> {
        static J: OnceLock<Mutex<HashMap<String, Job>>> = OnceLock::new();
        J.get_or_init(|| Mutex::new(HashMap::new()))
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn put_job(id: &str, mut job: Job) {
        job.updated_ms = now_ms();
        let mut g = jobs().lock().unwrap_or_else(|p| p.into_inner());
        // A single operator's own box; a few hundred receipts is the whole lifetime of a
        // session. Trim rather than grow without bound if one is ever left running for weeks.
        if g.len() > 512 {
            let oldest = g
                .iter()
                .min_by_key(|(_, j)| j.updated_ms)
                .map(|(k, _)| k.clone());
            if let Some(k) = oldest {
                g.remove(&k);
            }
        }
        g.insert(id.to_string(), job);
    }

    pub fn get_job(id: &str) -> Option<Job> {
        jobs()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .cloned()
    }

    fn new_job_id(seed_addr: &str, to: &str, amount: u128) -> String {
        let mut h = blake3::Hasher::new();
        h.update(seed_addr.as_bytes());
        h.update(to.as_bytes());
        h.update(&amount.to_le_bytes());
        h.update(&now_ms().to_le_bytes());
        hex::encode(&h.finalize().as_bytes()[..8])
    }

    /// The smallest ramp denomination that covers `need`.
    ///
    /// `sigil_state::shielded::DENOMINATIONS` is the 1/2/5 x 10^k ladder and is sorted
    /// ascending, so the first entry >= `need` is the smallest one. Overshoot is at most
    /// 2.5x and is NOT lost — it becomes change back into this wallet's own shielded
    /// balance on the very spend it enables.
    pub fn smallest_denomination_at_least(need: u128) -> Option<u128> {
        sigil_state::shielded::DENOMINATIONS
            .iter()
            .copied()
            .find(|d| *d >= need)
    }

    pub fn is_hex64(s: &str) -> bool {
        s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
    }

    #[derive(Clone, serde::Deserialize, serde::Serialize)]
    pub struct CandidateNote {
        pub index: u64,
        /// Raw base units (glyphs) as a decimal string — the wallet's own localStorage shape.
        pub value: String,
    }

    #[derive(serde::Deserialize)]
    struct PayReq {
        to: String,
        /// Raw base units (glyphs), decimal string — same convention as `mine-shield`.
        amount: String,
        #[serde(default)]
        memo: String,
        /// This wallet's own already-shielded notes, exactly the `{index, value}` pairs the
        /// browser keeps in `localStorage['sigil-shielded-notes-'+addr]`. Optional: an empty
        /// list simply means "nothing shielded yet", which is the out-of-the-box case.
        #[serde(default)]
        notes: Vec<CandidateNote>,
        /// The address the CALLER believes it is spending from. Guard, not input: this box
        /// can only ever spend the wallet its own seed derives, so a mismatch means the
        /// browser is showing a different wallet and must use the manual flow instead of
        /// silently moving the miner's money.
        #[serde(default)]
        from: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct StatusReq {
        job: String,
    }

    fn ok_json(v: serde_json::Value) -> (&'static str, String) {
        ("200 OK", v.to_string())
    }

    /// Feature-absent / wrong-wallet / no-seed: HTTP 404, mirroring `mine_local_api`'s
    /// availability contract so the page's existing "fall through to the manual flow"
    /// branch fires unchanged.
    fn not_available(reason: &str) -> (&'static str, String) {
        (
            "404 Not Found",
            serde_json::json!({ "ok": false, "error": reason }).to_string(),
        )
    }

    /// Understood the request, could not complete it. HTTP 200 with `ok:false` — same
    /// reasoning as `mine_local_api::bad_request`.
    fn failed(stage: &str, reason: impl Into<String>) -> (&'static str, String) {
        ok_json(serde_json::json!({
            "ok": false, "stage": stage, "error": reason.into(),
        }))
    }

    /// One shared client for the whole process.
    ///
    /// Measured 2026-09-02 on this box: building a fresh `reqwest::blocking::Client` costs
    /// ~25 ms because the rustls builder loads the bundled webpki root store every time —
    /// which dwarfs the actual call (a loopback round-trip to `sigil-api` measured 0.4 ms
    /// mean over 10 samples). Two builds per `/api/v1/pay` were most of that endpoint's
    /// latency. Building once removes it and is the difference between "instant" being a
    /// claim and being true.
    fn client() -> Result<&'static reqwest::blocking::Client, String> {
        static C: OnceLock<Option<reqwest::blocking::Client>> = OnceLock::new();
        C.get_or_init(|| {
            reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .ok()
        })
        .as_ref()
        .ok_or_else(|| "http client init failed".to_string())
    }

    /// The wallet this box holds the seed for, lowercase hex. `None` when no seed is
    /// configured, which is the whole availability gate for this endpoint.
    fn local_wallet() -> Option<String> {
        crate::miner_keypair().map(|kp| kp.pubkey_hex().to_ascii_lowercase())
    }

    /// `(pk_shield, pk_encrypt)` for a recipient, from the node's own registry.
    ///
    /// Both halves are required: `pk_shield` binds the output note to the recipient inside
    /// the proof, `pk_encrypt` is what the note ciphertext is sealed to. A wallet that
    /// registered only the first can be paid to in the circuit but could never OPEN the
    /// note, so refusing here is the honest answer, not a limitation to route around.
    fn recipient_keys(to: &str) -> Result<(String, String), String> {
        let node = crate::engine_node_url();
        let url = format!(
            "{}/v1/shielded/address?wallet={to}",
            node.trim_end_matches('/')
        );
        let c = client()?;
        let v: serde_json::Value = c
            .get(&url)
            .send()
            .map_err(|e| format!("could not reach the node: {e}"))?
            .json()
            .map_err(|e| format!("bad response from the node: {e}"))?;
        if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
            return Err(v
                .get("error")
                .and_then(|x| x.as_str())
                .unwrap_or("this address cannot receive private payments yet")
                .to_string());
        }
        let pk_shield = v
            .get("pk_shield")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string();
        let pk_encrypt = v
            .get("pk_encrypt")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string();
        if pk_shield.is_empty() || pk_encrypt.is_empty() {
            return Err(
                "the recipient has not published an encryption key, so a private note \
                 could be paid to them but never opened by them"
                    .into(),
            );
        }
        Ok((pk_shield, pk_encrypt))
    }

    fn transparent_balance(wallet: &str) -> Result<u128, String> {
        let node = crate::engine_node_url();
        let url = format!("{}/v1/balance?wallet={wallet}", node.trim_end_matches('/'));
        let c = client()?;
        let v: serde_json::Value = c
            .get(&url)
            .send()
            .map_err(|e| format!("could not reach the node: {e}"))?
            .json()
            .map_err(|e| format!("bad response from the node: {e}"))?;
        v.get("data")
            .and_then(|d| d.get("balance"))
            .and_then(|b| b.as_str())
            .and_then(|s| s.parse::<u128>().ok())
            .ok_or_else(|| "could not read this wallet's balance".to_string())
    }

    /// Everything the payment leg needs, cloneable into the background thread.
    #[derive(Clone)]
    struct SendCtx {
        pk_shield: String,
        pk_encrypt: String,
        amount: u128,
        memo: String,
        notes: Vec<CandidateNote>,
    }

    /// One attempt at the payment leg. Composes `mine_local_api`'s existing endpoint rather
    /// than re-deriving any of its crypto.
    fn send_once(ctx: &SendCtx) -> Result<String, String> {
        let payload = serde_json::json!({
            "recipient_pk_shield": ctx.pk_shield,
            "recipient_pk_encrypt": ctx.pk_encrypt,
            "amount": ctx.amount.to_string(),
            "memo": ctx.memo,
            "notes": ctx.notes,
        });
        let (_status, body) =
            crate::mine_local_api::handle("/api/v1/mine-send-private", &payload.to_string());
        let v: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| format!("bad local response: {e}"))?;
        if v.get("ok").and_then(|x| x.as_bool()) == Some(true) {
            return Ok(v
                .get("txid")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string());
        }
        Err(v
            .get("error")
            .and_then(|x| x.as_str())
            .unwrap_or("unknown error")
            .to_string())
    }

    /// Keep trying the payment leg until the note is visible in the pool. Runs detached so
    /// `POST /api/v1/pay` can return its txid in the same millisecond it obtains one.
    fn spawn_payment_leg(id: String, ctx: SendCtx, mut job: Job) {
        std::thread::spawn(move || {
            for attempt in 1..=MAX_ATTEMPTS {
                job.attempts = attempt;
                job.stage = "waiting".into();
                put_job(&id, job.clone());
                match send_once(&ctx) {
                    Ok(txid) => {
                        job.stage = "paid".into();
                        job.txid = txid;
                        job.error = None;
                        put_job(&id, job);
                        return;
                    }
                    Err(e) => {
                        job.error = Some(e);
                    }
                }
                std::thread::sleep(RETRY_EVERY);
            }
            job.stage = "failed".into();
            put_job(&id, job);
        });
    }

    pub fn handle(path: &str, body: &str) -> (&'static str, String) {
        match path {
            PAY => start(body),
            PAY_STATUS => status(body),
            _ => not_available("unknown local endpoint"),
        }
    }

    fn status(body: &str) -> (&'static str, String) {
        let req: StatusReq = match serde_json::from_str(body) {
            Ok(r) => r,
            Err(e) => return failed("status", format!("bad request body: {e}")),
        };
        match get_job(&req.job) {
            Some(j) => {
                let mut v = serde_json::to_value(&j).unwrap_or_else(|_| serde_json::json!({}));
                v["ok"] = serde_json::json!(j.stage != "failed");
                v["job"] = serde_json::json!(req.job);
                ok_json(v)
            }
            None => failed("status", "no such payment"),
        }
    }

    fn start(body: &str) -> (&'static str, String) {
        let Some(seed_addr) = local_wallet() else {
            return not_available("no local mining seed configured (SIGIL_MINE_SEED unset)");
        };
        let req: PayReq = match serde_json::from_str(body) {
            Ok(r) => r,
            Err(e) => return failed("request", format!("bad request body: {e}")),
        };

        // Wrong-wallet guard — see `PayReq::from`.
        if let Some(f) = req.from.as_ref() {
            let f = f.trim().to_ascii_lowercase();
            if !f.is_empty() && f != seed_addr {
                return not_available(
                    "this page's wallet is not the wallet this sigil-top holds the seed for",
                );
            }
        }

        let to = req.to.trim().to_ascii_lowercase();
        if !is_hex64(&to) {
            return failed(
                "recipient",
                "the recipient must be a 64-character hex SIGIL address",
            );
        }
        if to == seed_addr {
            return failed("recipient", "that is this wallet's own address");
        }
        let amount: u128 = match req.amount.trim().parse() {
            Ok(a) if a > 0 => a,
            _ => {
                return failed(
                    "amount",
                    "amount must be a positive base-10 integer string (raw base units)",
                )
            }
        };

        let (pk_shield, pk_encrypt) = match recipient_keys(&to) {
            Ok(k) => k,
            Err(e) => return failed("recipient", e),
        };

        let fee = sigil_state::shielded::SHIELDED_FEE;
        let need = amount.saturating_add(fee);

        let mut ctx = SendCtx {
            pk_shield,
            pk_encrypt,
            amount,
            memo: req.memo.clone(),
            notes: req.notes.clone(),
        };

        // Does ONE existing note already cover this? (See the 1-in/2-out note in the module
        // docs — summing the notes is the wrong question and answering it that way is what
        // leaves a funded wallet permanently unable to pay.)
        let covered = req
            .notes
            .iter()
            .filter_map(|n| n.value.trim().parse::<u128>().ok())
            .any(|v| v >= need);

        let id = new_job_id(&seed_addr, &to, amount);
        let mut job = Job {
            stage: "waiting".into(),
            ..Default::default()
        };

        if covered {
            // Nothing to shield. The only latency is building + proving the STARK, so do it
            // inline and hand back the real PAYMENT txid.
            match send_once(&ctx) {
                Ok(txid) => {
                    job.stage = "paid".into();
                    job.txid = txid.clone();
                    job.attempts = 1;
                    put_job(&id, job);
                    return ok_json(serde_json::json!({
                        "ok": true, "stage": "paid", "job": id, "txid": txid,
                        "note": "paid from a note that was already in your private balance",
                    }));
                }
                Err(e) => {
                    // A covering note exists but is not visible in the pool yet (it was
                    // shielded moments ago). Do NOT shield again — that would move a second
                    // helping of money into the pool for one payment. Wait for the one we
                    // already have.
                    job.error = Some(e);
                    spawn_payment_leg(id.clone(), ctx, job);
                    return ok_json(serde_json::json!({
                        "ok": true, "stage": "waiting", "job": id, "txid": serde_json::Value::Null,
                        "note": "your private balance covers this, but the note has not \
                                 appeared in the pool yet — waiting for it, no new funds moved",
                    }));
                }
            }
        }

        // ── Shield exactly ONE denomination that covers the payment ──────────────────
        let Some(denom) = smallest_denomination_at_least(need) else {
            return failed(
                "shield",
                format!("{need} is larger than the largest shield denomination"),
            );
        };
        match transparent_balance(&seed_addr) {
            Ok(bal) if bal < denom => {
                return failed(
                    "shield",
                    format!(
                        "this payment needs a single private note of {denom} base units \
                         (the next step up the ramp ladder from {need}); this wallet holds \
                         {bal}"
                    ),
                )
            }
            Ok(_) => {}
            Err(e) => return failed("shield", e),
        }

        let (_st, sbody) = crate::mine_local_api::handle(
            "/api/v1/mine-shield",
            &serde_json::json!({ "amount": denom.to_string() }).to_string(),
        );
        let sv: serde_json::Value = match serde_json::from_str(&sbody) {
            Ok(v) => v,
            Err(e) => return failed("shield", format!("bad local response: {e}")),
        };
        if sv.get("ok").and_then(|x| x.as_bool()) != Some(true) {
            return failed(
                "shield",
                sv.get("error")
                    .and_then(|x| x.as_str())
                    .unwrap_or("could not move funds into your private balance")
                    .to_string(),
            );
        }
        // `decompose` of an exact denomination is a single part, so `landed[0]` IS the note
        // this payment will spend. Assert that rather than assume it.
        let landed = sv.get("landed").and_then(|x| x.as_array()).cloned().unwrap_or_default();
        let Some(first) = landed.first() else {
            return failed("shield", "the node accepted no part of the shield");
        };
        let shield_txid = first.get("txid").and_then(|x| x.as_str()).unwrap_or_default().to_string();
        let shield_index = first.get("index").and_then(|x| x.as_u64());
        let shield_value = first
            .get("value")
            .and_then(|x| x.as_u64())
            .map(|v| v.to_string())
            .unwrap_or_else(|| denom.to_string());

        if let (Some(index), Ok(value)) = (shield_index, shield_value.parse::<u128>()) {
            if value >= need {
                ctx.notes.push(CandidateNote {
                    index,
                    value: shield_value.clone(),
                });
            }
        }

        job.stage = "shielding".into();
        job.shield_txid = shield_txid.clone();
        job.shield_index = shield_index;
        job.shield_value = shield_value.clone();
        put_job(&id, job.clone());
        spawn_payment_leg(id.clone(), ctx, job);

        ok_json(serde_json::json!({
            "ok": true,
            "stage": "shielding",
            "job": id,
            // THE INSTANT TXID. Real, on-chain, checkable at /v1/transactions/<hash>. It is
            // the SHIELD, not the payment — `stage` says so and the wallet labels it so.
            "txid": shield_txid,
            "shield_txid": shield_txid,
            "shield_index": shield_index,
            "shield_value": shield_value,
            "note": "funds are moving into your private balance; the payment follows \
                     automatically — poll /api/v1/pay/status for its txid",
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ══ ONE-ACTION SEND (/api/v1/pay) ═══════════════════════════════════════════════

    #[test]
    fn the_wallet_this_server_hands_out_carries_the_one_action_send() {
        let (body, ct) = embedded_surface("sigil-wallet-tron.html").expect("wallet served");
        let html = String::from_utf8(body).expect("wallet is utf-8");
        assert!(ct.starts_with("text/html"));
        // Injected exactly once — a second copy would re-override the override.
        assert_eq!(
            html.matches("__sigilOneActionSend").count(),
            2,
            "the guard flag should appear exactly twice (test + set), i.e. one injection"
        );
        assert!(html.contains("/api/v1/pay"), "the injected script must call /api/v1/pay");
        // And it must run AFTER the page's own scripts, or there is nothing to capture as
        // the fallback and a 404 would dead-end instead of falling back.
        let injected = html.find("__sigilOneActionSend").expect("injected");
        let close = html.rfind("</body>").expect("wallet has a </body>");
        assert!(injected < close, "the script must sit inside <body>");
        let unified = html
            .find("window.doUnifiedSend=async function")
            .expect("the page still defines its own doUnifiedSend");
        assert!(unified < injected, "the override must come after the definition it replaces");
    }

    #[test]
    fn injection_never_silently_drops_the_script() {
        // A page with no </body> at all still gets it, appended.
        let out = String::from_utf8(inject_one_action_send("<p>hi</p>")).unwrap();
        assert!(out.starts_with("<p>hi</p>"));
        assert!(out.contains("__sigilOneActionSend"));
        // A page WITH one gets it spliced before the LAST </body>.
        let out = String::from_utf8(inject_one_action_send("<body>x</body></html>")).unwrap();
        let i = out.find("__sigilOneActionSend").unwrap();
        let j = out.rfind("</body>").unwrap();
        assert!(i < j);
    }

    #[test]
    fn pay_paths_route_locally_and_nothing_else_does() {
        assert!(pay::is_local_path("/api/v1/pay"));
        assert!(pay::is_local_path("/api/v1/pay/status"));
        assert!(!pay::is_local_path("/api/v1/payload"));
        assert!(!pay::is_local_path("/api/v1/pay?x=1"));
        assert!(!pay::is_local_path("/v1/send"));
        // Must not collide with the three signing endpoints it rides alongside.
        for p in ["/api/v1/mine-sign", "/api/v1/mine-shield", "/api/v1/mine-send-private"] {
            assert!(crate::mine_local_api::is_local_path(p));
            assert!(!pay::is_local_path(p));
        }
    }

    /// The rule that makes a payment possible at all: the spend circuit is 1-in/2-out, so
    /// ONE note must cover `amount + fee`. Shielding the shortfall (what the browser flow
    /// did) can leave a funded wallet permanently unable to pay.
    #[test]
    fn a_shield_is_sized_to_one_note_that_covers_the_whole_payment() {
        use pay::smallest_denomination_at_least as up;
        // An exact denomination is not rounded up past itself.
        assert_eq!(up(1), Some(1));
        assert_eq!(up(100), Some(100));
        assert_eq!(up(5_000_000_000), Some(5_000_000_000));
        // Anything between rungs takes the next rung, never the one below.
        assert_eq!(up(3), Some(5));
        assert_eq!(up(6), Some(10));
        assert_eq!(up(101), Some(200));
        // The ladder is 1/2/5 x 10^k, so overshoot is bounded at 2.5x — real, and change
        // comes straight back to the sender's own shielded balance.
        for need in [3u128, 7, 11, 23, 4_100, 999_999] {
            let d = up(need).expect("covered by the ladder");
            assert!(d >= need, "{d} must cover {need}");
            assert!(d * 2 <= need * 5, "{d} overshoots {need} by more than 2.5x");
            assert!(
                sigil_state::shielded::is_denomination(d),
                "{d} must be a legal ramp denomination"
            );
            // ...and it must decompose to exactly ONE note, or it is not one covering note.
            assert_eq!(sigil_state::shielded::decompose(d), Some(vec![d]));
        }
        // Above the ladder there is no single covering note, and we say so rather than
        // shielding something that cannot pay.
        assert_eq!(up(u128::MAX), None);
    }

    #[test]
    fn a_payment_needs_amount_plus_the_shielded_fee_not_just_the_amount() {
        let fee = sigil_state::shielded::SHIELDED_FEE;
        assert!(fee > 0, "this test is meaningless if the fee is free");
        // A note worth exactly the amount cannot pay it — the fee comes out of the same note.
        let amount = 100_000_000u128;
        let need = amount + fee;
        assert!(pay::smallest_denomination_at_least(need).unwrap() >= need);
    }

    #[test]
    fn addresses_are_validated_before_anything_is_signed_or_spent() {
        assert!(pay::is_hex64(&"a".repeat(64)));
        assert!(pay::is_hex64(&"0".repeat(64)));
        assert!(!pay::is_hex64(&"a".repeat(63)));
        assert!(!pay::is_hex64(&"a".repeat(65)));
        assert!(!pay::is_hex64(&"g".repeat(64)));
        assert!(!pay::is_hex64(""));
    }

    /// AVAILABILITY CONTRACT (same as `mine_local_api`'s): with no local seed this endpoint
    /// must answer 404, because that is what makes the injected script hand control back to
    /// the page's own flow instead of dead-ending.
    #[test]
    fn without_a_local_seed_pay_is_a_404_so_the_browser_falls_back() {
        if std::env::var("SIGIL_MINE_SEED").is_ok() {
            return; // a real seed in the environment; this assertion isn't the one to make
        }
        let (status, body) = pay::handle(
            "/api/v1/pay",
            &serde_json::json!({"to": "a".repeat(64), "amount": "1"}).to_string(),
        );
        assert_eq!(status, "404 Not Found");
        assert!(body.contains("SIGIL_MINE_SEED"), "the 404 must say why: {body}");
    }

    #[test]
    fn an_unknown_job_is_reported_not_invented() {
        let (status, body) = pay::handle(
            "/api/v1/pay/status",
            &serde_json::json!({"job": "deadbeefdeadbeef"}).to_string(),
        );
        assert_eq!(status, "200 OK");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], serde_json::json!(false));
        assert!(pay::get_job("deadbeefdeadbeef").is_none());
    }

    #[test]
    fn segment_match_does_not_fire_on_a_name_that_merely_ends_the_same_way() {
        assert!(names("enter-sigil.html", "enter-sigil.html"));
        assert!(names("nested/dir/enter-sigil.html", "enter-sigil.html"));
        // The bug this guards: a `ends_with` without the separator check would serve the
        // built-in onboarding page for someone's unrelated `fake-enter-sigil.html`.
        assert!(!names("fake-enter-sigil.html", "enter-sigil.html"));
        assert!(!names("enter-sigil.htmlx", "enter-sigil.html"));
        assert!(!names("", "enter-sigil.html"));
    }

    #[test]
    fn every_built_in_surface_resolves_to_non_empty_bytes() {
        for name in [
            "sigil-wallet-codex.css",
            "sigil-wallet-tron.html",
            "enter-sigil.html",
            "sigil-explorer.html",
            "vite-engine.html",
            "sigil-tron-p2p.js",
            "wasm/sigil_shield.js",
            "wasm/sigil_shield_bg.wasm",
        ] {
            let got = embedded_surface(name);
            assert!(got.is_some(), "{name} is not served from the binary");
            let (body, ct) = got.unwrap();
            assert!(!body.is_empty(), "{name} resolved to an empty body");
            assert!(!ct.is_empty(), "{name} has no content type");
        }
        assert!(embedded_surface("sigil-recent-blocks.json").is_none());
        assert!(embedded_surface("downloads/sigil-top-linux-x64").is_none());
    }

    /// The regression this whole change exists to prevent: a stale wallet sitting in the
    /// static dir must NOT be able to shadow the one compiled into the binary.
    #[test]
    fn a_file_on_disk_cannot_shadow_the_built_in_wallet() {
        let dir = std::env::temp_dir().join(format!(
            "sigil-serve-precedence-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let stale = b"<html>stale wallet from a week-old dist</html>";
        std::fs::write(dir.join("sigil-wallet-tron.html"), stale).unwrap();

        let (status, body, _ct) = serve_file(&dir, "sigil-wallet-tron.html");
        assert_eq!(status, "200 OK");
        assert_ne!(body.as_slice(), stale, "the disk copy shadowed the built-in wallet");
        assert_eq!(
            body,
            embedded_surface("sigil-wallet-tron.html").unwrap().0,
            "served bytes are not the built-in wallet"
        );

        // Anything that ISN'T a built-in surface must still come from disk, or a
        // deployment's own assets would disappear.
        std::fs::write(dir.join("sigil-tip.json"), b"{\"tip\":1}").unwrap();
        let (status, body, _) = serve_file(&dir, "sigil-tip.json");
        assert_eq!(status, "200 OK");
        assert_eq!(body, b"{\"tip\":1}");

        std::fs::remove_dir_all(&dir).ok();
    }
}
