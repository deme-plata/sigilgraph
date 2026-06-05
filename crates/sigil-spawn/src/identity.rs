//! [`ChainIdentity`] — everything that must be UNIQUE per chain so two spawned
//! chains never collide on the wire: network id, magic, address prefix, P2P/RPC
//! ports, and gossip topics. All deterministically derived from the chain name
//! via blake3, so spawning N chains from N names yields N non-overlapping
//! identities automatically.

use crate::spec::ChainSpec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainIdentity {
    /// 16-hex-char id = first 8 bytes of blake3(name).
    pub network_id: String,
    /// 4-byte network magic for framing / handshake.
    pub network_magic: u32,
    /// Bech32-style human prefix for addresses (lowercased ticker).
    pub address_prefix: String,
    pub p2p_port: u16,
    pub rpc_port: u16,
    /// Gossip topics, namespaced by network_id so chains don't cross-talk.
    pub topics: Vec<String>,
}

impl ChainIdentity {
    pub fn derive(spec: &ChainSpec) -> Self {
        let digest = blake3::hash(spec.name.as_bytes());
        let b = digest.as_bytes();

        let network_id = hex::encode(&b[..8]);
        let network_magic = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);

        // Deterministic per-name port offset in [0, 1000) so concurrent chains
        // land on distinct ports without manual bookkeeping.
        let offset = u16::from_le_bytes([b[8], b[9]]) % 1000;
        let p2p_port = spec.base_p2p_port.wrapping_add(offset);
        let rpc_port = spec.base_rpc_port.wrapping_add(offset);

        let address_prefix = spec
            .ticker
            .to_lowercase()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect();

        let topics = vec![
            format!("/sigil/{network_id}/blocks"),
            format!("/sigil/{network_id}/tx"),
            format!("/sigil/{network_id}/tip"),
        ];

        Self { network_id, network_magic, address_prefix, p2p_port, rpc_port, topics }
    }
}
