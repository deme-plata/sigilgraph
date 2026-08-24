//! R1: the tx INGEST BRIDGE — a tiny HTTP server that lets external clients
//! (wallets, the sigil-rpcd batch-forwarder, load generators) submit
//! transactions and `AuthorizedBatch`es straight into the producer's mempool.
//!
//! This is the FIRST real user-tx path on the live chain: before this, the
//! producer's mempool had only the in-process `SIGIL_TXGEN` loadgen feeder, and
//! sigil-rpcd (`:8099`) was a fully separate ledger. Env-gated by
//! `SIGIL_API_PORT` (0/unset = off) so it changes nothing until switched on.
//!
//! Routes (all JSON):
//!   GET  /health   -> {"ok":true,"service":"sigil-ingest"}
//!   GET  /mempool  -> {"ok":true,"txs":N,"batches":M,"batch_ops":K}
//!   POST /tx       -> body = SignedTx JSON        -> Mempool::ingest (verify-once)
//!   POST /batch    -> body = AuthorizedBatch JSON -> Mempool::ingest_batch (ONE sig)

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use sigil_narwhal_mempool::MempoolBackend;
use sigil_tx::{AuthorizedBatch, SignedTx};

const MAX_BODY: usize = 8 * 1024 * 1024; // 8 MiB request cap

/// Spawn the ingest server on `port`, sharing the producer's mempool. Runs on its
/// own OS thread (like the TXGEN feeder) so a slow client never stalls production.
///
/// `dandelion_tx` hands off every locally-submitted transaction to the Dandelion++
/// relay actor (dandelion_relay.rs) so it enters the network via a stem-phase
/// relay instead of a raw broadcast — a plain `UnboundedSender::send` is
/// synchronous and works fine from this non-async thread.
pub fn spawn(mempool: Arc<MempoolBackend>, dandelion_tx: crate::dandelion_relay::Sender, port: u16) {
    let _ = std::thread::Builder::new()
        .name("sigil-ingest".into())
        .spawn(move || {
            let listener = match TcpListener::bind(("0.0.0.0", port)) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("⚠ ingest: bind :{port} failed: {e}");
                    return;
                }
            };
            for conn in listener.incoming() {
                if let Ok(mut stream) = conn {
                    let _ = handle(&mut stream, &mempool, &dandelion_tx);
                }
            }
        });
}

fn handle(
    stream: &mut TcpStream,
    mempool: &Arc<MempoolBackend>,
    dandelion_tx: &crate::dandelion_relay::Sender,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).ok();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_headers_end(&buf) {
            let content_len = parse_content_length(&buf[..pos]).min(MAX_BODY);
            if buf.len().saturating_sub(pos + 4) >= content_len {
                break;
            }
        }
        if buf.len() > MAX_BODY {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let (method, path) = request_line(&text);
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    let resp = route(method, path, body, mempool, dandelion_tx);
    let out = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        resp.len(),
        resp
    );
    stream.write_all(out.as_bytes())?;
    Ok(())
}

fn route(
    method: &str,
    path: &str,
    body: &str,
    mempool: &Arc<MempoolBackend>,
    dandelion_tx: &crate::dandelion_relay::Sender,
) -> String {
    // strip any query string
    let path = path.split('?').next().unwrap_or(path);
    match (method, path) {
        ("GET", "/health") => r#"{"ok":true,"service":"sigil-ingest"}"#.to_string(),
        ("GET", "/mempool") => {
            format!(
                r#"{{"ok":true,"txs":{},"batches":{},"batch_ops":{}}}"#,
                mempool.len(),
                mempool.batch_count(),
                mempool.pending_batch_ops()
            )
        }
        ("POST", "/tx") => match serde_json::from_str::<SignedTx>(body) {
            Ok(tx) => {
                let r = mempool.ingest(vec![tx]);
                // Only hand off what the mempool actually accepted as new —
                // an invalid or duplicate submission has nothing to relay.
                if r.accepted > 0 {
                    if let Some((id, bytes)) = crate::dandelion_relay::wrap(
                        crate::dandelion_relay::RelayedTx::Legacy(body.as_bytes().to_vec()),
                    ) {
                        let _ = dandelion_tx.send(crate::dandelion_relay::Cmd::Originate { id, bytes });
                    }
                }
                format!(
                    r#"{{"ok":true,"accepted":{},"invalid":{},"dupe":{}}}"#,
                    r.accepted, r.invalid, r.dupe
                )
            }
            Err(e) => err(&format!("bad SignedTx JSON: {e}")),
        },
        ("POST", "/batch") => match serde_json::from_str::<AuthorizedBatch>(body) {
            Ok(batch) => match mempool.ingest_batch(batch) {
                Ok(ops) => format!(r#"{{"ok":true,"accepted_ops":{ops}}}"#),
                Err(e) => err(&format!("batch rejected: {e}")),
            },
            Err(e) => err(&format!("bad AuthorizedBatch JSON: {e}")),
        },
        _ => err("not found"),
    }
}

fn err(msg: &str) -> String {
    serde_json::json!({ "ok": false, "error": msg }).to_string()
}
fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}
fn parse_content_length(headers: &[u8]) -> usize {
    let h = String::from_utf8_lossy(headers).to_lowercase();
    for line in h.lines() {
        if let Some(v) = line.strip_prefix("content-length:") {
            return v.trim().parse().unwrap_or(0);
        }
    }
    0
}
fn request_line(text: &str) -> (&str, &str) {
    let first = text.lines().next().unwrap_or("");
    let mut it = first.split_whitespace();
    (it.next().unwrap_or(""), it.next().unwrap_or("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_tx::{ed25519_keygen, AuthorizedBatch, SigilTx, NATIVE};

    fn dandelion_channel() -> crate::dandelion_relay::Sender {
        tokio::sync::mpsc::unbounded_channel().0
    }

    #[test]
    fn route_batch_ingest_status_and_dedup() {
        let mp = Arc::new(MempoolBackend::legacy());
        let dtx = dandelion_channel();
        assert!(route("GET", "/health", "", &mp, &dtx).contains("sigil-ingest"));

        let (sk, pk, author) = ed25519_keygen();
        let ops = vec![SigilTx::Send { from: author, to: [9u8; 32], amount: 1, token: NATIVE, fee: 0 }];
        let batch = AuthorizedBatch::sign_ed25519(ops, 1, &sk, &pk);
        let body = serde_json::to_string(&batch).unwrap();

        let r = route("POST", "/batch", &body, &mp, &dtx);
        assert!(r.contains("\"ok\":true") && r.contains("accepted_ops"), "ingest: {r}");
        assert!(route("GET", "/mempool", "", &mp, &dtx).contains("\"batches\":1"));
        // replay of the same batch is rejected at ingest.
        assert!(route("POST", "/batch", &body, &mp, &dtx).contains("duplicate batch"));
        // malformed body is rejected, not panicked.
        assert!(route("POST", "/batch", "{garbage", &mp, &dtx).contains("bad AuthorizedBatch"));
    }

    /// A single-tx POST /tx must both accept into the mempool AND hand the
    /// transaction off to the Dandelion++ relay actor — this is the actual
    /// tx-gossip wiring point, so prove the Cmd arrives, not just that the
    /// HTTP response looks right.
    #[test]
    fn accepted_tx_is_handed_to_the_dandelion_relay() {
        use sigil_tx::ed25519_sign_tx;

        let mp = Arc::new(MempoolBackend::legacy());
        let (dtx, mut drx) = tokio::sync::mpsc::unbounded_channel();

        let (sk, pk, author) = ed25519_keygen();
        let tx = ed25519_sign_tx(
            SigilTx::Send { from: author, to: [9u8; 32], amount: 1, token: NATIVE, fee: 0 },
            &sk,
            &pk,
        );
        let body = serde_json::to_string(&tx).unwrap();

        let r = route("POST", "/tx", &body, &mp, &dtx);
        assert!(r.contains("\"accepted\":1"), "ingest: {r}");
        assert_eq!(mp.len(), 1, "the mempool must hold the accepted tx");

        let cmd = drx.try_recv().expect("dandelion actor must receive an Originate cmd");
        match cmd {
            crate::dandelion_relay::Cmd::Originate { id, bytes } => {
                let expected = crate::dandelion_relay::wrap(
                    crate::dandelion_relay::RelayedTx::Legacy(body.as_bytes().to_vec()),
                ).expect("wrap must succeed for a well-formed tx body");
                assert_eq!(id, expected.0, "relayed id must match what wrap() derives from the same bytes");
                assert_eq!(bytes, expected.1, "relayed bytes must be exactly what wrap() produced");
                let relayed: crate::dandelion_relay::RelayedTx =
                    bincode::deserialize(&bytes).expect("relayed bytes must decode as RelayedTx");
                let crate::dandelion_relay::RelayedTx::Legacy(tx_json) = relayed else {
                    panic!("expected RelayedTx::Legacy for the ingest.rs path")
                };
                let round_tripped: SignedTx =
                    serde_json::from_slice(&tx_json).expect("relayed bytes must round-trip as the same SignedTx");
                assert_eq!(round_tripped.tx.hash(), tx.tx.hash());
            }
            _ => panic!("expected Cmd::Originate"),
        }
    }
}
