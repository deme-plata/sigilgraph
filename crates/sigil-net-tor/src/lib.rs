//! sigil-net-tor — SIGIL Tor egress layer.
//!
//! Layered ABOVE [`sigil_net_wg`](../sigil_net_wg/index.html). The mental
//! model is:
//!
//! ```text
//!     ┌──────────────────────────────┐
//!     │ sigil-net (gossipsub/libp2p) │
//!     └──────┬───────────────┬───────┘
//!            │               │
//!            ▼               ▼
//!     ┌──────────────┐  ┌──────────────┐
//!     │ sigil-net-wg │  │ sigil-net-tor │
//!     └──────┬───────┘  └──────┬───────┘
//!            │                 │
//!            ▼                 ▼
//!         kernel              Arti
//!        WireGuard          (Rust Tor)
//! ```
//!
//! - WG is for validator-set confidentiality (operators dialing each other
//!   over a private mesh).
//! - Tor is for outbound reach to non-validators: downloading a release
//!   binary, fetching an external oracle, exposing a `.onion` JSON-RPC.
//!
//! The two compose: a validator can dial *another validator's* `.onion`
//! over Tor to add a circuit-level anonymity layer ABOVE WG.
//!
//! ## Build modes
//!
//! - **Default (no features)** — a typed stub surface. Builds in 5 s
//!   instead of 5 min, returns [`TorError::ArtiDisabled`] from every call.
//!   Useful for CI, for crates that just want to type-check the surface,
//!   and for environments where Arti's transitive deps don't compile.
//! - **`--features arti`** — pulls real `arti-client`, returns real
//!   [`TorClient`] handles, opens real circuits.
//!
//! Default-disabled is intentional: Arti is heavy (50+ transitive crates,
//! ~5 min cold compile). Enabling it on `sigil-node` builds is one
//! `--features arti` flag away when an operator actually wants Tor on.

#![warn(missing_docs)]

use std::time::Duration;

use thiserror::Error;

/// Errors from any Tor layer call. `ArtiDisabled` is the cheap-to-return
/// signal when the crate was built without `--features arti`; callers can
/// log + fall back to direct dial.
#[derive(Debug, Error)]
pub enum TorError {
    /// The `arti` feature wasn't enabled at compile time. The crate is in
    /// stub mode; rebuild with `--features arti` to use this call.
    #[error("arti feature disabled at compile time — rebuild sigil-net-tor with --features arti")]
    ArtiDisabled,

    /// Arti's bootstrap (loading consensus, building entry circuit) failed
    /// or timed out. Recoverable — usually means the operator is behind a
    /// hostile network. Retry with a longer timeout or a bridge.
    #[error("arti bootstrap: {0}")]
    Bootstrap(String),

    /// Circuit construction or stream attach failed.
    #[error("arti circuit: {0}")]
    Circuit(String),

    /// I/O on the resulting stream.
    #[error("io: {0}")]
    Io(String),

    /// Address didn't parse as `host:port`.
    #[error("invalid target {0}: {1}")]
    InvalidTarget(String, String),
}

/// Sensible default for the initial Arti bootstrap. Tor's consensus fetch
/// is slow on first run; subsequent runs cache the directory and bootstrap
/// in seconds. Sigil operators on residential connections see 30–90 s
/// first boot, 1–3 s thereafter.
pub const DEFAULT_BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(120);

/// SIGIL-specific defaults the Arti client cares about. Kept feature-flag
/// agnostic so the surface compiles in both stub and arti modes.
#[derive(Debug, Clone)]
pub struct TorConfig {
    /// Bootstrap timeout. None = use Arti's own default.
    pub bootstrap_timeout: Option<Duration>,
    /// Local SOCKS5 bind address. Format `host:port`. Used by
    /// [`spawn_socks_listener`]; ignored for direct [`TorClient::dial`].
    pub socks_bind: String,
    /// Per-(class,peer) Tor STREAM ISOLATION. When `true` (default), each
    /// egress class + peer gets its own circuit so a hostile guard/exit can't
    /// correlate that one SIGIL client is talking to multiple peers — or
    /// distinguish a private submission from a light-client query. Set `false`
    /// only to trade this anonymity property for fewer circuits.
    pub isolate_streams: bool,
    /// Circuit ROTATION period, seconds. Isolated circuits rotate every this
    /// many seconds (a time-epoch folded into the isolation key → new epoch,
    /// new circuit). Defaults to 600s (10 min, matching Tor's own circuit
    /// dirtiness window). `0` disables time-rotation.
    pub circuit_rotation_secs: u64,
}

impl Default for TorConfig {
    fn default() -> Self {
        Self {
            bootstrap_timeout: Some(DEFAULT_BOOTSTRAP_TIMEOUT),
            socks_bind: "127.0.0.1:9050".into(),
            isolate_streams: true,
            circuit_rotation_secs: 600,
        }
    }
}

// ── Arti-backed implementation ──────────────────────────────────────────────

#[cfg(feature = "arti")]
mod arti_impl;

#[cfg(feature = "arti")]
pub use arti_impl::*;

// ── Stub implementation (default — no Arti deps) ────────────────────────────

#[cfg(not(feature = "arti"))]
mod stub_impl {
    use super::*;

    /// Stub TorClient. All methods return [`TorError::ArtiDisabled`].
    #[derive(Debug, Default, Clone)]
    pub struct TorClient {
        _cfg: TorConfig,
    }

    impl TorClient {
        /// Construct the stub. Doesn't error — feature is the gate.
        pub async fn bootstrap(cfg: TorConfig) -> Result<Self, TorError> {
            Ok(Self { _cfg: cfg })
        }

        /// Stub dial.
        pub async fn dial(&self, target: &str) -> Result<TorStream, TorError> {
            let _ = target;
            Err(TorError::ArtiDisabled)
        }

        /// Stub of the stream-isolated dial (real impl in `arti_impl`).
        pub async fn dial_isolated(&self, target: &str, peer_key: &str) -> Result<TorStream, TorError> {
            let _ = (target, peer_key);
            Err(TorError::ArtiDisabled)
        }
    }

    /// Stub of a Tor-tunneled stream. In the arti build this wraps a
    /// `DataStream` and impls `AsyncRead+AsyncWrite`.
    #[derive(Debug)]
    pub struct TorStream;

    /// Stub SOCKS5 listener handle. Holding the handle keeps the listener
    /// alive; dropping it shuts it down.
    #[derive(Debug)]
    pub struct SocksListener;

    /// Stub SOCKS5 spawn — `--features arti` to enable.
    pub async fn spawn_socks_listener(_client: &TorClient) -> Result<SocksListener, TorError> {
        Err(TorError::ArtiDisabled)
    }
}

#[cfg(not(feature = "arti"))]
pub use stub_impl::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stub_dial_errors_when_arti_disabled_at_compile_time() {
        let client = TorClient::bootstrap(TorConfig::default()).await.unwrap();
        let err = client.dial("example.com:80").await.unwrap_err();
        // Either stub error or, with --features arti, a runtime bootstrap
        // error — both are non-panicking and informative.
        let _ = err;
    }

    #[test]
    fn defaults_are_sane() {
        let c = TorConfig::default();
        assert!(c.bootstrap_timeout.unwrap().as_secs() >= 60);
        assert!(c.socks_bind.contains(':'));
    }
}
