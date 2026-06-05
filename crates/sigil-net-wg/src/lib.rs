//! sigil-net-wg — WireGuard transport layer for SIGIL.
//!
//! Provides typed keypairs, peer-config structs, and a backend trait. Phase 0
//! ships exactly one concrete backend: [`CliWgBackend`], which shells out to
//! `wg(8)` / `wg-quick(8)` on Linux. A userspace `boringtun` backend slots in
//! P1 when SIGIL needs to operate inside a container that can't load the
//! kernel `wireguard` module.
//!
//! ## Why WireGuard at all?
//!
//! Bare gossipsub over libp2p is fine for an open mesh. SIGIL's validator
//! set, however, wants:
//!
//! 1. **Confidentiality of even the existence of the SIGIL network from
//!    on-path observers**. libp2p TLS handshake leaks SNI and the libp2p
//!    protocol id; WG looks like UDP noise.
//! 2. **A separate trust domain for "are you in the validator set?" vs
//!    "are you a member of the public mesh?"**. WG's static-key model
//!    enforces this at the network layer.
//! 3. **Wire-level survival across ISP filters** — UDP-on-arbitrary-port
//!    plus optional obfsproxy-style wrappers later.
//!
//! On top of WG, [`sigil_net::TOPIC_BLOCKS`] still gossipsubs as usual. Tor
//! (Arti) is then layered on top of WG for egress anonymity from the
//! validator's perspective, so an attacker who breaks BOTH the WG layer
//! AND a Tor circuit still only learns "this packet came from somewhere in
//! the SIGIL validator set" — which is public anyway.
//!
//! ## Hybrid PQ PSK
//!
//! WireGuard 1.0 supports a 32-byte pre-shared key (PSK) that's XORed into
//! the session-key derivation. We use the [Rosenpass](https://rosenpass.eu/)
//! pattern: rotate the PSK every epoch with the output of a Kyber-1024 KEM
//! against the peer's published Kyber pubkey. That makes the session key
//! safe even if Curve25519 is broken (the WG base) — the only remaining
//! attack is breaking Kyber-1024 AND the WG handshake in the same epoch.
//!
//! Phase 0 stubs this — see [`HybridPsk`]. Real Kyber wiring lands when
//! `flux-eternal-cypher` ports from Quillon (skill rule #19 §11).

#![warn(missing_docs)]

pub mod backend;
pub mod hybrid;
pub mod key;
pub mod peer;

pub use backend::{CliWgBackend, WgBackend, WgBackendError};
pub use hybrid::{HybridPsk, HybridPskError};
pub use key::{WgPrivateKey, WgPublicKey, WgPresharedKey, KeyError};
pub use peer::{WgInterface, WgPeer};

/// WireGuard's UDP port default in upstream config. SIGIL operators are
/// free to pick anything; this is a sensible single-machine default.
pub const DEFAULT_LISTEN_PORT: u16 = 51820;

/// Interface name SIGIL nodes default to. Operators can override; this is
/// the human-friendly value `wg show` prints by default.
pub const DEFAULT_INTERFACE_NAME: &str = "sigil0";
