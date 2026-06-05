//! flux-sigil-net — SIGIL-specific networking constants + bootstrap parsing.
//!
//! Phase 0 deliberately ships the on-the-wire identifiers (gossipsub topics,
//! port numbers, network_id) and bootstrap-peer env parsing — nothing else.
//! These constants are the contract every other SIGIL crate depends on:
//!
//!  - Track A (consensus/mining) publishes blocks on `TOPIC_BLOCKS`.
//!  - Track B (sigil-updater) publishes releases on `TOPIC_RELEASE`.
//!  - Track C (state/events) doesn't touch the network directly but uses
//!    `NETWORK_ID` when computing canonical header hashes.
//!
//! The actual `NetworkManager` wrapper lands once flux-p2p's subscribe API is
//! stable. Locking the strings now means parallel agents can wire to them
//! without rendezvous on a shared mutable file.

use serde::{Deserialize, Serialize};
use thiserror::Error;

// Re-export the Tor client so the node can bootstrap Arti through one import
// surface (`sigil_net::TorClient`). Real under `--features arti`, stub
// otherwise — the type exists in both modes.
pub use sigil_net_tor::{TorClient, TorConfig, TorError as TorClientError};

// Selective Tor egress policy — "Tor the submission, WireGuard the
// propagation." Reserves slow/anonymous Tor for tiny privacy-critical
// payloads; keeps the bandwidth-heavy validator mesh on fast WireGuard.
pub mod tor_policy;
pub use tor_policy::{route_egress, EgressClass, EgressRoute};

// ── Network identity ────────────────────────────────────────────────────────
//
// Hard-coded to `sigil-g0` so SIGIL traffic never collides with Quillon's
// `mainnet-genesis` — even on the same machine, the libp2p protocol prefix
// makes them mutually invisible.

/// SIGIL network identifier. Bytes form, used wherever Quillon uses the same
/// shape for `network_id`. Lives in block headers (per SIGIL_GENESIS_v0.md §2).
pub const NETWORK_ID: &[u8] = b"sigil-g0";

/// String form of [`NETWORK_ID`], for paths/topics/log messages.
pub const NETWORK_ID_STR: &str = "sigil-g0";

/// libp2p protocol prefix. All SIGIL streams negotiate under this prefix so a
/// Quillon node and a SIGIL node sharing a TCP port (or running on the same
/// host) ignore each other cleanly.
pub const PROTOCOL_PREFIX: &str = "/sigil/g0/";

// ── Gossipsub topics ────────────────────────────────────────────────────────
//
// One topic per concern. Keep them dense (publishers know exactly where their
// message belongs) and stable (changing a topic string is a wire-break).

pub const TOPIC_BLOCKS: &str = "/sigil/g0/blocks";
pub const TOPIC_PEER_HEIGHTS: &str = "/sigil/g0/peer-heights";
pub const TOPIC_TIP_PROOFS: &str = "/sigil/g0/tip-proofs";
pub const TOPIC_TXS: &str = "/sigil/g0/txs";
/// `sigil-updater` broadcasts `ReleaseAnnouncement` JSON on this topic.
pub const TOPIC_RELEASE: &str = "/sigil/g0/release";

/// All SIGIL topics, in the order a freshly-booted node should subscribe.
/// Subscribing to blocks before tip-proofs would mean accepting block-data
/// from peers whose tips we haven't verified yet — the verify-before-sync
/// gate requires subscribing tip-proofs first.
pub const ALL_TOPICS: &[&str] = &[
    TOPIC_TIP_PROOFS,
    TOPIC_PEER_HEIGHTS,
    TOPIC_RELEASE,
    TOPIC_BLOCKS,
    TOPIC_TXS,
];

// ── Default ports ───────────────────────────────────────────────────────────
//
// Distinct from Quillon's :9001 / :8080 so two nodes can co-exist on one box
// (planned for the Phase 0 testnet: Delta runs SIGIL-1, Epsilon co-locates
// SIGIL-2 alongside the Quillon production node).

pub const DEFAULT_P2P_PORT: u16 = 9501;
pub const DEFAULT_API_PORT: u16 = 8181;

// ── Bootstrap peers ─────────────────────────────────────────────────────────
//
// SIGIL has NO hardcoded bootstrap peer list. That's the root cause of the
// Quillon Delta-stall — peer ids drifted between releases. Operators set
// `SIGIL_BOOTSTRAP_PEERS` as a comma-separated multiaddr list; this crate
// parses it once. There's no fallback list to be stale.

pub const BOOTSTRAP_ENV: &str = "SIGIL_BOOTSTRAP_PEERS";

/// Parse the comma-separated env var into a list of multiaddrs. Whitespace is
/// trimmed and empty entries are skipped. Missing var returns an empty list
/// (lets single-node bootstrap work without env).
pub fn read_bootstrap_peers() -> Vec<String> {
    parse_bootstrap_list(&std::env::var(BOOTSTRAP_ENV).unwrap_or_default())
}

/// Pure-function form for tests and explicit overrides.
pub fn parse_bootstrap_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect()
}

// ── Config struct ───────────────────────────────────────────────────────────
//
// A future `NetworkManager` wrapper takes this. Phase 0 just defines the
// shape so callers in Tracks A/C can construct one without churning later.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigilNetConfig {
    pub network_id: Vec<u8>,
    pub protocol_prefix: String,
    pub p2p_port: u16,
    pub api_port: u16,
    pub bootstrap_peers: Vec<String>,
    pub db_path: std::path::PathBuf,
    /// Which transport layer to use under gossipsub. Defaults to `Direct`.
    pub transport: SigilTransport,
}

/// Transport composition for SIGIL gossipsub frames. The two stacked
/// modes (`WireGuard`, `WireGuardThenTor`) layer on top of the always-on
/// libp2p layer so consensus code doesn't change between them.
///
/// - `Direct`: plain libp2p TCP/quic-quic, no privacy layer. Mesh peers
///   see your real IP. Useful for local-net testing and bootstrap nodes.
/// - `WireGuard`: validators dial each other inside a WG mesh. Anyone
///   off-mesh sees only UDP noise — no SNI, no libp2p protocol id, no
///   peer-id leak. Set per skill section "Why WireGuard at all?". The
///   `wg_interface_name` field selects which `wg(8)` interface to bind
///   the libp2p listener to.
/// - `Tor`: every outbound dial goes through an Arti circuit. No WG.
///   For non-validator clients that want anonymity but don't need
///   validator-mesh confidentiality.
/// - `WireGuardThenTor`: dial peers over WG first, fall back to Tor for
///   anything that isn't a validator. Belt + suspenders. Use when one
///   SIGIL node is acting as both validator and public RPC.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind")]
pub enum SigilTransport {
    /// Plain libp2p. The Phase 0 default.
    #[default]
    Direct,
    /// Run inside a WireGuard mesh.
    WireGuard {
        /// `wg-quick(8)` interface name to bind to. Defaults to `sigil0`.
        wg_interface_name: String,
    },
    /// Egress via Arti Tor.
    Tor,
    /// WireGuard mesh first, Tor for everything off-mesh.
    WireGuardThenTor {
        /// WG interface name as above.
        wg_interface_name: String,
    },
}

impl SigilTransport {
    /// Cheap classifier — does this transport need a WireGuard interface?
    pub fn needs_wireguard(&self) -> bool {
        matches!(self, SigilTransport::WireGuard { .. } | SigilTransport::WireGuardThenTor { .. })
    }

    /// Cheap classifier — does this transport need Tor egress?
    pub fn needs_tor(&self) -> bool {
        matches!(self, SigilTransport::Tor | SigilTransport::WireGuardThenTor { .. })
    }

    /// Human-readable single-word label for log lines.
    pub fn label(&self) -> &'static str {
        match self {
            SigilTransport::Direct                => "direct",
            SigilTransport::WireGuard { .. }      => "wireguard",
            SigilTransport::Tor                   => "tor",
            SigilTransport::WireGuardThenTor {..} => "wireguard+tor",
        }
    }

    /// WG interface name if this transport carries one. Useful for the
    /// `sigil-node start` banner.
    pub fn wg_interface(&self) -> Option<&str> {
        match self {
            SigilTransport::WireGuard { wg_interface_name }
            | SigilTransport::WireGuardThenTor { wg_interface_name } => Some(wg_interface_name),
            _ => None,
        }
    }
}

/// Env var for selecting the transport at startup.
///
/// Format:
/// - `direct`                       → `SigilTransport::Direct`
/// - `wireguard:<iface>`            → `SigilTransport::WireGuard { wg_interface_name }`
/// - `tor`                          → `SigilTransport::Tor`
/// - `wg+tor:<iface>`               → `SigilTransport::WireGuardThenTor { wg_interface_name }`
///
/// Unrecognized → [`NetError::UnknownTransport`].
pub const TRANSPORT_ENV: &str = "SIGIL_TRANSPORT";

/// Env var to override the libp2p listen multiaddr when in any WG mode.
/// Operators typically set this to the interface's link-local
/// (e.g. `/ip4/10.42.0.2/tcp/9501`) so libp2p only accepts mesh traffic.
/// If unset in WG mode, sigil-node defaults to `/ip4/127.0.0.1/tcp/<port>`
/// — only loopback, which surfaces the "you forgot to set it" mistake
/// loudly instead of accidentally falling open to 0.0.0.0.
pub const WG_LISTEN_ADDR_ENV: &str = "SIGIL_WG_LISTEN_ADDR";

/// Parse the `SIGIL_TRANSPORT` env value (or any caller-supplied string)
/// into a [`SigilTransport`]. Empty / unset → `Direct`.
pub fn parse_transport_str(s: &str) -> Result<SigilTransport, NetError> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(SigilTransport::Direct);
    }
    let lc = s.to_ascii_lowercase();
    match lc.as_str() {
        "direct"          => Ok(SigilTransport::Direct),
        "tor"             => Ok(SigilTransport::Tor),
        _ => {
            if let Some(iface) = lc.strip_prefix("wireguard:") {
                let iface = iface.trim();
                if iface.is_empty() {
                    return Err(NetError::EmptyWgInterfaceName);
                }
                return Ok(SigilTransport::WireGuard { wg_interface_name: iface.into() });
            }
            if let Some(iface) = lc.strip_prefix("wg+tor:")
                .or_else(|| lc.strip_prefix("wg-then-tor:"))
            {
                let iface = iface.trim();
                if iface.is_empty() {
                    return Err(NetError::EmptyWgInterfaceName);
                }
                return Ok(SigilTransport::WireGuardThenTor { wg_interface_name: iface.into() });
            }
            Err(NetError::UnknownTransport(s.into()))
        }
    }
}

/// Convenience: read `SIGIL_TRANSPORT` from env (or default to `Direct`).
pub fn read_transport_env() -> Result<SigilTransport, NetError> {
    parse_transport_str(&std::env::var(TRANSPORT_ENV).unwrap_or_default())
}

/// Re-export the underlying layer types so consumers don't have to
/// path-dep both crates separately.
pub use sigil_net_tor as tor;
pub use sigil_net_wg as wg;

impl Default for SigilNetConfig {
    fn default() -> Self {
        Self {
            network_id: NETWORK_ID.to_vec(),
            protocol_prefix: PROTOCOL_PREFIX.into(),
            p2p_port: DEFAULT_P2P_PORT,
            api_port: DEFAULT_API_PORT,
            bootstrap_peers: read_bootstrap_peers(),
            // Per skill rule #3: DB path is ALWAYS absolute. Default is a
            // user-owned path; the operator overrides via SIGIL_DB_PATH or
            // by mutating this field explicitly. NEVER use a relative path
            // (Quillon's Q_DB_PATH foot-gun).
            db_path: default_db_path(),
            transport: SigilTransport::default(),
        }
    }
}

fn default_db_path() -> std::path::PathBuf {
    if let Ok(explicit) = std::env::var("SIGIL_DB_PATH") {
        let p = std::path::PathBuf::from(explicit);
        if p.is_absolute() {
            return p;
        }
    }
    // Fall back to ~/sigil-data — still absolute, never relative.
    if let Ok(home) = std::env::var("HOME") {
        return std::path::PathBuf::from(home).join("sigil-data");
    }
    std::path::PathBuf::from("/var/lib/sigil-data")
}

#[derive(Debug, Error)]
pub enum NetError {
    #[error("bootstrap peer string is empty")]
    EmptyBootstrap,
    #[error("db path is not absolute: {0}")]
    NonAbsoluteDbPath(String),
    /// WireGuard interface name missing or empty when the transport mode
    /// requires one.
    #[error("WireGuard transport requires a non-empty wg_interface_name")]
    EmptyWgInterfaceName,
    /// `SIGIL_TRANSPORT` env value didn't match a known transport.
    #[error("unknown SIGIL_TRANSPORT value: {0:?}")]
    UnknownTransport(String),
}

impl SigilNetConfig {
    /// Validate constraints that aren't expressible in the type system.
    /// Currently: db_path must be absolute (matches skill rule #3).
    pub fn validate(&self) -> Result<(), NetError> {
        if !self.db_path.is_absolute() {
            return Err(NetError::NonAbsoluteDbPath(
                self.db_path.display().to_string(),
            ));
        }
        match &self.transport {
            SigilTransport::WireGuard { wg_interface_name }
            | SigilTransport::WireGuardThenTor { wg_interface_name } => {
                if wg_interface_name.is_empty() {
                    return Err(NetError::EmptyWgInterfaceName);
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_topics_start_with_protocol_prefix() {
        for t in ALL_TOPICS {
            assert!(t.starts_with(PROTOCOL_PREFIX), "topic missing prefix: {t}");
        }
    }

    #[test]
    fn tip_proofs_subscribed_before_blocks() {
        let tip_idx = ALL_TOPICS.iter().position(|t| *t == TOPIC_TIP_PROOFS).unwrap();
        let block_idx = ALL_TOPICS.iter().position(|t| *t == TOPIC_BLOCKS).unwrap();
        assert!(
            tip_idx < block_idx,
            "verify-before-sync requires tip-proofs subscribed before blocks"
        );
    }

    #[test]
    fn parse_bootstrap_list_trims_and_filters_empties() {
        let s = "  /ip4/1.2.3.4/tcp/9501/p2p/abc , , /ip4/5.6.7.8/tcp/9501/p2p/def  ";
        let parsed = parse_bootstrap_list(s);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0], "/ip4/1.2.3.4/tcp/9501/p2p/abc");
        assert_eq!(parsed[1], "/ip4/5.6.7.8/tcp/9501/p2p/def");
    }

    #[test]
    fn parse_bootstrap_list_empty_returns_empty() {
        assert!(parse_bootstrap_list("").is_empty());
        assert!(parse_bootstrap_list("   ").is_empty());
        assert!(parse_bootstrap_list(",,,").is_empty());
    }

    #[test]
    fn default_config_validates() {
        let cfg = SigilNetConfig::default();
        cfg.validate().expect("default config should validate");
        // The default db_path must be absolute.
        assert!(cfg.db_path.is_absolute(), "default db_path is not absolute");
    }

    #[test]
    fn validate_rejects_relative_db_path() {
        let mut cfg = SigilNetConfig::default();
        cfg.db_path = std::path::PathBuf::from("./sigil-data");
        assert!(matches!(cfg.validate(), Err(NetError::NonAbsoluteDbPath(_))));
    }

    #[test]
    fn ports_are_distinct_from_quillon() {
        // Quillon uses 9001 + 8080. SIGIL must not collide.
        assert_ne!(DEFAULT_P2P_PORT, 9001);
        assert_ne!(DEFAULT_API_PORT, 8080);
    }

    #[test]
    fn network_id_str_matches_bytes() {
        assert_eq!(NETWORK_ID_STR.as_bytes(), NETWORK_ID);
    }

    #[test]
    fn default_transport_is_direct() {
        let cfg = SigilNetConfig::default();
        assert_eq!(cfg.transport, SigilTransport::Direct);
        assert!(!cfg.transport.needs_wireguard());
        assert!(!cfg.transport.needs_tor());
    }

    #[test]
    fn classifiers_match_each_variant() {
        let wg = SigilTransport::WireGuard { wg_interface_name: "sigil0".into() };
        let tor = SigilTransport::Tor;
        let both = SigilTransport::WireGuardThenTor { wg_interface_name: "sigil0".into() };
        assert!(wg.needs_wireguard()  && !wg.needs_tor());
        assert!(!tor.needs_wireguard() && tor.needs_tor());
        assert!(both.needs_wireguard() && both.needs_tor());
    }

    #[test]
    fn validate_rejects_empty_wg_interface_name() {
        let mut cfg = SigilNetConfig::default();
        cfg.transport = SigilTransport::WireGuard { wg_interface_name: "".into() };
        assert!(matches!(cfg.validate(), Err(NetError::EmptyWgInterfaceName)));
        cfg.transport = SigilTransport::WireGuardThenTor { wg_interface_name: "".into() };
        assert!(matches!(cfg.validate(), Err(NetError::EmptyWgInterfaceName)));
    }

    #[test]
    fn validate_accepts_populated_wg_interface_name() {
        let mut cfg = SigilNetConfig::default();
        cfg.transport = SigilTransport::WireGuard { wg_interface_name: "sigil0".into() };
        cfg.validate().expect("populated WG interface name should validate");
    }

    #[test]
    fn parse_transport_str_recognized_forms() {
        assert_eq!(parse_transport_str("").unwrap(), SigilTransport::Direct);
        assert_eq!(parse_transport_str("   ").unwrap(), SigilTransport::Direct);
        assert_eq!(parse_transport_str("direct").unwrap(), SigilTransport::Direct);
        assert_eq!(parse_transport_str("DIRECT").unwrap(), SigilTransport::Direct);
        assert_eq!(parse_transport_str("tor").unwrap(), SigilTransport::Tor);
        assert_eq!(
            parse_transport_str("wireguard:sigil0").unwrap(),
            SigilTransport::WireGuard { wg_interface_name: "sigil0".into() }
        );
        assert_eq!(
            parse_transport_str("wg+tor:sigil0").unwrap(),
            SigilTransport::WireGuardThenTor { wg_interface_name: "sigil0".into() }
        );
        assert_eq!(
            parse_transport_str("wg-then-tor:sigil1").unwrap(),
            SigilTransport::WireGuardThenTor { wg_interface_name: "sigil1".into() }
        );
    }

    #[test]
    fn parse_transport_str_rejects_unknown() {
        assert!(matches!(
            parse_transport_str("yolo"),
            Err(NetError::UnknownTransport(_))
        ));
        assert!(matches!(
            parse_transport_str("wireguard:"),
            Err(NetError::EmptyWgInterfaceName)
        ));
        assert!(matches!(
            parse_transport_str("wg+tor:"),
            Err(NetError::EmptyWgInterfaceName)
        ));
    }

    #[test]
    fn transport_label_and_wg_interface_helpers() {
        assert_eq!(SigilTransport::Direct.label(), "direct");
        assert_eq!(SigilTransport::Tor.label(), "tor");
        assert_eq!(
            SigilTransport::WireGuard { wg_interface_name: "sigil0".into() }.wg_interface(),
            Some("sigil0")
        );
        assert_eq!(SigilTransport::Direct.wg_interface(), None);
        assert_eq!(SigilTransport::Tor.wg_interface(), None);
    }

    #[test]
    fn transport_round_trips_through_json() {
        for t in [
            SigilTransport::Direct,
            SigilTransport::Tor,
            SigilTransport::WireGuard { wg_interface_name: "sigil0".into() },
            SigilTransport::WireGuardThenTor { wg_interface_name: "sigil1".into() },
        ] {
            let s = serde_json::to_string(&t).unwrap();
            let t2: SigilTransport = serde_json::from_str(&s).unwrap();
            assert_eq!(t, t2);
        }
    }
}
