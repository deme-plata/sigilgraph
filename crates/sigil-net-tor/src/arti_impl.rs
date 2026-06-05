//! Real Arti-backed implementation. Gated on `--features arti` so its
//! transitive dep cost (50+ crates, ~5 min cold compile) doesn't tax every
//! sigil-net-tor build.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use arti_client::{IsolationToken, StreamPrefs, TorClient as ArtiTorClient, TorClientConfig};
use tor_rtcompat::PreferredRuntime;

use super::{TorConfig, TorError};

/// SIGIL-specific Tor client. Wraps an [`arti_client::TorClient`] so
/// callers don't have to deal with Arti's generic runtime parameter.
#[derive(Clone)]
pub struct TorClient {
    inner: Arc<ArtiTorClient<PreferredRuntime>>,
    cfg: TorConfig,
    /// Per-peer Tor stream-isolation tokens. A stable [`IsolationToken`] per
    /// peer-key means same peer → same circuit (cheap, consistent), distinct
    /// peers → distinct circuits (a guard/exit can't CORRELATE that one SIGIL
    /// client is talking to peers A *and* B — the validator's peer set stays
    /// private). This is the security upgrade over a bare `connect()` that
    /// lets Arti coalesce unrelated peer streams onto one circuit.
    isolation: Arc<Mutex<HashMap<String, IsolationToken>>>,
}

impl std::fmt::Debug for TorClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TorClient")
            .field("bootstrap_timeout", &self.cfg.bootstrap_timeout)
            .field("socks_bind", &self.cfg.socks_bind)
            .finish_non_exhaustive()
    }
}

impl TorClient {
    /// Bootstrap an Arti client. Loads consensus, builds entry circuit.
    /// First run is slow (30–90 s); subsequent runs use the cached
    /// directory in `~/.local/share/arti/`.
    pub async fn bootstrap(cfg: TorConfig) -> Result<Self, TorError> {
        let arti_cfg = TorClientConfig::default();
        let inner = ArtiTorClient::create_bootstrapped(arti_cfg)
            .await
            .map_err(|e| TorError::Bootstrap(format!("{}", e)))?;
        Ok(Self {
            inner: Arc::new(inner),
            cfg,
            isolation: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Open a Tor-tunneled stream to `target` ("host:port"), isolated by the
    /// target itself. Secure-by-default: even a bare `dial` no longer lets
    /// unrelated destinations coalesce onto one circuit.
    pub async fn dial(&self, target: &str) -> Result<TorStream, TorError> {
        self.dial_isolated(target, target).await
    }

    /// Open a Tor-tunneled stream to `target`, pinned to the circuit reserved
    /// for `peer_key`. All streams sharing a `peer_key` reuse one circuit;
    /// streams with different keys are placed on DIFFERENT circuits so a
    /// hostile guard/exit cannot correlate them to the same client.
    ///
    /// `peer_key` should be a stable per-peer identifier (e.g. the peer's
    /// libp2p PeerId or `.onion` address) — NOT the raw `host:port`, when the
    /// caller wants one circuit per logical peer.
    pub async fn dial_isolated(&self, target: &str, peer_key: &str) -> Result<TorStream, TorError> {
        let (host, port) = parse_target(target)?;
        let mut prefs = StreamPrefs::new();
        if self.cfg.isolate_streams {
            // Dedicated circuit per key, ROTATED every `circuit_rotation_secs`.
            // The key already carries the (class::peer) distinction from the
            // caller (`tor_send` qualifies it with the egress class); here we
            // fold in a time-EPOCH so a new epoch ⇒ new token ⇒ a fresh
            // circuit. Stale-epoch tokens are pruned so the map stays bounded.
            let key = self.circuit_key(peer_key);
            let suffix = self.epoch_suffix();
            let token = {
                let mut map = self
                    .isolation
                    .lock()
                    .map_err(|_| TorError::Circuit("isolation map poisoned".into()))?;
                if map.len() > 256 {
                    map.retain(|k, _| k.ends_with(&suffix)); // drop past epochs
                }
                *map.entry(key).or_insert_with(IsolationToken::new)
            };
            prefs.set_isolation(token);
        }
        let stream = self
            .inner
            .connect_with_prefs((host.as_str(), port), &prefs)
            .await
            .map_err(|e| TorError::Circuit(format!("{}", e)))?;
        Ok(TorStream { inner: stream })
    }

    /// `|e<n>` suffix for the current rotation epoch. With
    /// `circuit_rotation_secs = R`, the epoch is `now/R`, so the suffix flips
    /// every R seconds → the circuit key changes → a fresh isolated circuit.
    /// `R = 0` disables time-rotation (single epoch `e0`).
    fn epoch_suffix(&self) -> String {
        let epoch = if self.cfg.circuit_rotation_secs > 0 {
            now_secs() / self.cfg.circuit_rotation_secs
        } else {
            0
        };
        format!("|e{epoch}")
    }

    /// The full isolation-map key for a caller `peer_key` (which already
    /// encodes the egress class via `tor_send`) at the current epoch.
    fn circuit_key(&self, peer_key: &str) -> String {
        format!("{peer_key}{}", self.epoch_suffix())
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Tor-tunneled stream. Wraps Arti's `DataStream` so callers never need
/// the `arti_client` types in their crates.
pub struct TorStream {
    inner: arti_client::DataStream,
}

impl std::fmt::Debug for TorStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TorStream").finish_non_exhaustive()
    }
}

impl TorStream {
    /// Read bytes from the Tor stream.
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize, TorError> {
        use tokio::io::AsyncReadExt;
        self.inner
            .read(buf)
            .await
            .map_err(|e| TorError::Io(e.to_string()))
    }

    /// Write bytes to the Tor stream. **Flushes after writing** — Arti's
    /// `DataStream` buffers, so without an explicit flush the bytes never leave
    /// the local buffer onto the circuit (the request silently never reaches the
    /// exit → the peer never responds → 0 bytes back). Flushing here makes
    /// `write` mean "send this," which is what every caller expects.
    pub async fn write(&mut self, buf: &[u8]) -> Result<usize, TorError> {
        use tokio::io::AsyncWriteExt;
        let n = self
            .inner
            .write(buf)
            .await
            .map_err(|e| TorError::Io(e.to_string()))?;
        self.inner
            .flush()
            .await
            .map_err(|e| TorError::Io(e.to_string()))?;
        Ok(n)
    }

    /// Explicit flush — push any buffered bytes onto the Tor circuit.
    pub async fn flush(&mut self) -> Result<(), TorError> {
        use tokio::io::AsyncWriteExt;
        self.inner.flush().await.map_err(|e| TorError::Io(e.to_string()))
    }
}

/// Local SOCKS5 listener. P1 task — the design is fixed (bind to
/// `cfg.socks_bind`, route each accepted connection through `client.dial`),
/// but the implementation is not yet wired here so the surface stays
/// honest. Spawns will land in the same module under this feature flag.
pub struct SocksListener {
    _handle: tokio::task::JoinHandle<()>,
}

/// Start a SOCKS5 listener bound to `client.cfg.socks_bind` that routes
/// through `client`'s circuits.
pub async fn spawn_socks_listener(_client: &TorClient) -> Result<SocksListener, TorError> {
    Err(TorError::Bootstrap(
        "SOCKS5 listener implementation slots into P1 — see sigil-net-tor::arti_impl".into(),
    ))
}

fn parse_target(target: &str) -> Result<(String, u16), TorError> {
    let (host, port) = target
        .rsplit_once(':')
        .ok_or_else(|| TorError::InvalidTarget(target.into(), "missing ':port' suffix".into()))?;
    let port: u16 = port
        .parse()
        .map_err(|e: std::num::ParseIntError| TorError::InvalidTarget(target.into(), e.to_string()))?;
    Ok((host.to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_target_ok() {
        let (h, p) = parse_target("example.com:443").unwrap();
        assert_eq!(h, "example.com");
        assert_eq!(p, 443);
    }

    #[test]
    fn parse_target_missing_port() {
        let e = parse_target("example.com").unwrap_err();
        assert!(matches!(e, TorError::InvalidTarget(_, _)));
    }
}
