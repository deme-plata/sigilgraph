//! Peer + interface config — mirrors `wg-quick(8)`'s `.conf` format.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use crate::key::{WgPresharedKey, WgPrivateKey, WgPublicKey};

/// One peer entry — what a `[Peer]` block in a `wg-quick` config carries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WgPeer {
    /// Peer's WireGuard public key (their identity).
    pub public_key: WgPublicKey,

    /// Optional pre-shared key. If `Some`, XORed into both sides' session key
    /// derivation. Set by the [`crate::HybridPsk`] rotor every epoch.
    pub preshared_key: Option<WgPresharedKey>,

    /// Where to send packets — `IP:port`. None means we only accept incoming.
    pub endpoint: Option<SocketAddr>,

    /// Source-address allowlist. Packets from this peer's tunnel must claim
    /// an address inside one of these CIDRs; matches WireGuard's
    /// cryptokey-routing model. Stored as strings for now (CIDR parsing
    /// lands when we wire the userspace backend).
    pub allowed_ips: Vec<String>,

    /// Optional persistent-keepalive interval in seconds. WireGuard NAT
    /// punching needs this when the peer is behind a strict NAT.
    pub persistent_keepalive: Option<u16>,
}

impl WgPeer {
    /// Convenience: peer with just a public key and endpoint, no PSK or
    /// keepalive. Useful for tests + the initial bootstrap path.
    pub fn new(public_key: WgPublicKey, endpoint: SocketAddr) -> Self {
        Self {
            public_key,
            preshared_key: None,
            endpoint: Some(endpoint),
            allowed_ips: Vec::new(),
            persistent_keepalive: None,
        }
    }

    /// Render as the `[Peer]` block of a `wg-quick(8)` config file.
    pub fn to_conf_block(&self) -> String {
        let mut s = String::new();
        s.push_str("[Peer]\n");
        s.push_str(&format!("PublicKey = {}\n", self.public_key.to_base64()));
        if let Some(psk) = &self.preshared_key {
            s.push_str(&format!("PresharedKey = {}\n", psk.to_base64()));
        }
        if let Some(ep) = &self.endpoint {
            s.push_str(&format!("Endpoint = {}\n", ep));
        }
        if !self.allowed_ips.is_empty() {
            s.push_str(&format!("AllowedIPs = {}\n", self.allowed_ips.join(", ")));
        }
        if let Some(k) = self.persistent_keepalive {
            s.push_str(&format!("PersistentKeepalive = {}\n", k));
        }
        s
    }
}

/// Interface config — mirrors `[Interface]` in a `wg-quick` `.conf`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WgInterface {
    /// Linux interface name. Default [`crate::DEFAULT_INTERFACE_NAME`].
    pub name: String,
    /// Local private key.
    pub private_key: WgPrivateKey,
    /// UDP port to bind.
    pub listen_port: u16,
    /// Local addresses to assign on the interface (CIDRs).
    pub addresses: Vec<String>,
    /// MTU override. None → kernel default.
    pub mtu: Option<u16>,
    /// Peers reachable via this interface.
    pub peers: Vec<WgPeer>,
}

impl WgInterface {
    /// Render the full `wg-quick(8)` config — `[Interface]` block + every
    /// `[Peer]` block. Suitable for `echo "$cfg" | wg-quick strip - > /etc/wireguard/sigil0.conf`.
    pub fn to_conf_file(&self) -> String {
        let mut s = String::new();
        s.push_str("[Interface]\n");
        s.push_str(&format!("PrivateKey = {}\n", self.private_key.to_base64()));
        s.push_str(&format!("ListenPort = {}\n", self.listen_port));
        if !self.addresses.is_empty() {
            s.push_str(&format!("Address = {}\n", self.addresses.join(", ")));
        }
        if let Some(mtu) = self.mtu {
            s.push_str(&format!("MTU = {}\n", mtu));
        }
        for p in &self.peers {
            s.push('\n');
            s.push_str(&p.to_conf_block());
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn fresh_peer() -> WgPeer {
        let sk = WgPrivateKey::generate();
        let pk = sk.public();
        let mut p = WgPeer::new(pk, SocketAddr::from_str("203.0.113.5:51820").unwrap());
        p.allowed_ips.push("10.42.0.1/32".into());
        p.persistent_keepalive = Some(25);
        p
    }

    #[test]
    fn peer_conf_block_contains_all_set_fields() {
        let p = fresh_peer();
        let s = p.to_conf_block();
        assert!(s.starts_with("[Peer]\n"));
        assert!(s.contains("PublicKey ="));
        assert!(s.contains("Endpoint = 203.0.113.5:51820"));
        assert!(s.contains("AllowedIPs = 10.42.0.1/32"));
        assert!(s.contains("PersistentKeepalive = 25"));
    }

    #[test]
    fn interface_conf_has_interface_and_peer_blocks() {
        let iface = WgInterface {
            name: "sigil0".into(),
            private_key: WgPrivateKey::generate(),
            listen_port: 51820,
            addresses: vec!["10.42.0.2/16".into()],
            mtu: Some(1420),
            peers: vec![fresh_peer(), fresh_peer()],
        };
        let s = iface.to_conf_file();
        assert!(s.contains("[Interface]"));
        assert_eq!(s.matches("[Peer]").count(), 2, "both peers rendered");
        assert!(s.contains("ListenPort = 51820"));
        assert!(s.contains("MTU = 1420"));
    }

    #[test]
    fn psk_appears_when_set() {
        let mut p = fresh_peer();
        p.preshared_key = Some(crate::key::WgPresharedKey::generate());
        let s = p.to_conf_block();
        assert!(s.contains("PresharedKey ="));
    }
}
