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

/// Start the embedded server on 127.0.0.1:port, serving static_dir.
/// Returns a shutdown signal (set to true to stop the server).
pub fn start(static_dir: &str, port: u16) -> Result<Arc<AtomicBool>, String> {
    let dir = PathBuf::from(static_dir);
    if !dir.is_dir() {
        return Err(format!("static dir not found: {static_dir}"));
    }
    let listener =
        TcpListener::bind(format!("127.0.0.1:{port}")).map_err(|e| format!("bind :{port}: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("nonblocking: {e}"))?;

    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();
    thread::spawn(move || {
        serve_loop(listener, dir, stop_clone);
    });
    Ok(stop)
}

fn serve_loop(listener: TcpListener, dir: PathBuf, stop: Arc<AtomicBool>) {
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let dir = dir.clone();
                thread::spawn(move || handle_conn(&mut stream, &dir));
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

fn handle_conn(stream: &mut std::net::TcpStream, dir: &PathBuf) {
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

    // Decode URL
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

    // 4. Built-in vite-engine
    if safe == "vite-engine.html" || safe.ends_with("/vite-engine.html") {
        let html = include_str!("../../../gui/vite-engine-embedded.html");
        return ("200 OK", html.as_bytes().to_vec(), "text/html; charset=utf-8");
    }

    ("404 Not Found", b"404 Not Found\n".to_vec(), "text/plain")
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
