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
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let dir = dir.clone();
                let api = local_api.clone();
                thread::spawn(move || handle_conn(&mut stream, &dir, api.as_deref()));
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
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
    if method == "OPTIONS" && mine_local_api::is_local_path(path) {
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

    if method == "POST" && mine_local_api::is_local_path(path) {
        let (status, resp_body) = mine_local_api::handle(path, &req_body);
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
    if names(safe, "sigil-wallet-tron.html") {
        return Some((
            include_str!("../../../gui/sigil-wallet-tron-embedded.html").as_bytes().to_vec(),
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

#[cfg(test)]
mod tests {
    use super::*;

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
