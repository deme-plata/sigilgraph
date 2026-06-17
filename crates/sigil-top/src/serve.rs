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

/// Start the embedded server on 127.0.0.1:port, serving static_dir.
/// Returns a shutdown signal (set to true to stop the server).
///
/// OUT-OF-THE-BOX: the static dir does NOT need to exist. On a user's own machine
/// (who just downloaded sigil-top) there is no `dist-fluxapp` — but the wallet +
/// vite-engine are compiled INTO the binary (`include_str!`), so `serve_file` falls
/// through to the embedded copies. We bind regardless; the dir is just an optional
/// overlay for richer assets when present (e.g. on a server with the full dist).
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
    let _method = parts.next().unwrap_or("GET");
    let path = parts.next().unwrap_or("/");

    // /api/* → LOCAL-FIRST. If this node has a verified-spine view that can answer the
    // request (blocks / status / aether-verify / cortex / peers), serve it locally
    // (trust-minimised). Otherwise relay to the SIGIL node over std TCP — same-origin,
    // no CORS / mixed-content. Default node is the public sigil-rpcd; override with
    // SIGIL_NODE_URL to point at a LOCAL node.
    if path.starts_with("/api/") {
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
        let (status, body, ct) = proxy_api(path);
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

    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}

fn serve_file(dir: &PathBuf, safe: &str) -> (&'static str, Vec<u8>, &'static str) {
    // 1. Try the filesystem
    let file_path = dir.join(safe);
    if file_path.exists() && file_path.starts_with(dir) {
        if let Ok(data) = std::fs::read(&file_path) {
            return ("200 OK", data, content_type(safe));
        }
    }

    // 2. SPA fallback: try index.html in the requested path's directory
    if let Some(slash) = safe.rfind('/') {
        let parent = &safe[..slash];
        let index_path = dir.join(format!("{parent}/index.html"));
        if index_path.exists() {
            if let Ok(data) = std::fs::read(&index_path) {
                return ("200 OK", data, "text/html; charset=utf-8");
            }
        }
    }

    // 3. Built-in wallet — compiled into the binary
    if safe == "sigil-wallet-tron.html" || safe.ends_with("/sigil-wallet-tron.html") {
        let html = include_str!("../../../gui/sigil-wallet-tron-embedded.html");
        return ("200 OK", html.as_bytes().to_vec(), "text/html; charset=utf-8");
    }

    // 3b. Built-in onboarding — 6-word mnemonic → fresh wallet (the wallet gate
    // redirects fresh users here; it must be served or [W] dead-ends on a 404).
    if safe == "enter-sigil.html" || safe.ends_with("/enter-sigil.html") {
        let html = include_str!("../../../gui/enter-sigil.html");
        return ("200 OK", html.as_bytes().to_vec(), "text/html; charset=utf-8");
    }

    // 3c. Built-in SIGIL Explorer (flux-search/flux-db) — served here so the wallet's
    // Activity iframe is same-origin and its /api/v1/search hits the node proxy.
    if safe == "sigil-explorer.html" || safe.ends_with("/sigil-explorer.html") {
        let html = include_str!("../../../gui/sigil-explorer.html");
        return ("200 OK", html.as_bytes().to_vec(), "text/html; charset=utf-8");
    }

    // 4. Built-in vite-engine
    if safe == "vite-engine.html" || safe.ends_with("/vite-engine.html") {
        let html = include_str!("../../../gui/vite-engine-embedded.html");
        return ("200 OK", html.as_bytes().to_vec(), "text/html; charset=utf-8");
    }

    ("404 Not Found", b"404 Not Found\n".to_vec(), "text/plain")
}

/// Proxy a `GET /api/...` to the SIGIL node over std TCP and relay its JSON body.
/// Node = `SIGIL_NODE_URL` env (point at a LOCAL node), else the public sigil-rpcd.
/// The node speaks plain HTTP, so this works from the http://localhost wallet with
/// no CORS / mixed-content issues. sigil-rpcd strips `/api/v1` itself.
fn proxy_api(path_and_query: &str) -> (&'static str, Vec<u8>, &'static str) {
    let node = std::env::var("SIGIL_NODE_URL")
        .unwrap_or_else(|_| "http://sigilgraph.quillon.xyz:8099".into());
    let hostport = node
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let hostport = hostport.split('/').next().unwrap_or(hostport);
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(8099)),
        None => (hostport.to_string(), 8099),
    };

    let mut stream = match std::net::TcpStream::connect((host.as_str(), port)) {
        Ok(s) => s,
        Err(e) => {
            return (
                "502 Bad Gateway",
                format!("{{\"error\":\"node unreachable: {e}\"}}").into_bytes(),
                "application/json",
            )
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(6)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(6)));
    let req = format!(
        "GET {path_and_query} HTTP/1.1\r\nHost: {host}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(req.as_bytes()).is_err() {
        return (
            "502 Bad Gateway",
            b"{\"error\":\"node write failed\"}".to_vec(),
            "application/json",
        );
    }
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    // split off the HTTP headers; relay the body (sigil-rpcd sends Content-Length + close)
    let body = match raw.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(i) => raw[i + 4..].to_vec(),
        None => raw,
    };
    ("200 OK", body, "application/json")
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
