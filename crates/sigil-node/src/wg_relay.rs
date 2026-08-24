//! Zero-config WireGuard mesh: every node brings up its own interface at
//! boot and auto-exchanges keys with whatever peers it meets over the
//! regular P2P mesh — no operator ever runs `wg-up`/`wg-add-peer` by hand.
//!
//! # Deliberately additive, never exclusive
//!
//! `sigil-node`'s existing `wg-up`/`wg-add-peer` CLI subcommands (main.rs)
//! already work, but require an operator to manually exchange pubkeys with
//! every peer and only take effect if `SIGIL_TRANSPORT=wireguard:<iface>`
//! REPLACES the primary libp2p listen address — which strands every peer
//! that isn't also on the WG mesh (checked live 2026-08-24: of Epsilon's 5
//! real external peers, only 1 had a WG tunnel at all).
//!
//! This module never touches `SIGIL_TRANSPORT` or the libp2p listen
//! address. It uses its OWN interface (`sigil0`, the crate's own default —
//! deliberately NOT `sigilwg0`, the pre-existing manually-configured
//! interface with its own live peer, so this can never collide with or
//! disrupt that working setup) and its own subnet (`10.99.0.0/24`, chosen
//! to avoid every range already in use on Epsilon: docker0, wg0 10.8.0.0/24,
//! sigilwg0 10.77.0.0/24, tailscale0). The WG mesh grows purely as a side
//! effect of the EXISTING direct P2P mesh growing — zero risk to direct
//! connectivity, since direct stays the only thing libp2p actually listens
//! on. Once the WG mesh reaches critical mass, ADDING it as a genuine
//! transport option (dual-listen, not exclusive) is the natural next step —
//! not attempted here.
//!
//! # Auto address assignment
//!
//! No operator sets `SIGIL_WG_ADDRESS` per node (that was the manual-setup
//! failure mode this replaces). Each node derives its own host octet from
//! `blake3(node_id) % 253 + 1` (range 1..=253, avoiding .0/.255) — good
//! enough for a validator-set-sized mesh; a real collision registry is a
//! future refinement if the network ever approaches 253 WG-mesh members.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use sigil_net_wg::{CliWgBackend, WgBackend, WgInterface, WgPeer, WgPublicKey};

/// This mesh's own interface + subnet — separate from the legacy `sigilwg0`
/// / `10.77.0.0/24` setup on purpose (see module docs).
pub const IFACE: &str = "sigil0";
const SUBNET_PREFIX: &str = "10.99.0";
const LISTEN_PORT: u16 = 51821; // sigilwg0 already holds 51820's neighbor 51821... actually distinct: see below.

/// Point-to-point key-exchange message (same channel as Dandelion's
/// `StemWireMsg` — bincode, tried as a fallback after that fails to parse).
#[derive(Serialize, Deserialize, Clone)]
pub struct WgHelloMsg {
    pub pubkey_b64: String,
    pub wg_port: u16,
    pub wg_ip: String,
}

/// This node's own WG identity, fixed for the process lifetime once the
/// interface is up.
pub struct WgState {
    pub iface: String,
    pub pubkey_b64: String,
    pub wg_ip: String,
    pub wg_port: u16,
    db_path: PathBuf,
    /// peer_id -> the IP we saw them connect from (SwarmAppEvent::PeerConnected).
    /// InboundRequest doesn't carry the sender's IP directly, so this bridges
    /// the two events — populated on connect, consulted on WgHello receipt.
    peer_ips: Mutex<HashMap<flux_p2p::PeerId, String>>,
    /// Peers we've already live-applied, so a peer reconnecting (or a hello
    /// arriving twice) doesn't repeatedly shell out to `wg set`.
    already_peered: Mutex<std::collections::HashSet<String>>,
}

/// Derive this node's WG host octet from its libp2p node_id — deterministic,
/// so restarts don't churn the address, and independent across nodes with
/// negligible collision odds for a validator-set-sized mesh.
fn derive_host_octet(node_id: &str) -> u8 {
    let h = blake3::hash(node_id.as_bytes());
    1 + (h.as_bytes()[0] % 253) // 1..=253, never .0 or .254/.255
}

/// Bring up (or reuse) this node's own WG interface. Idempotent: if
/// `sigil0` is already up from a prior boot, this just reloads the
/// persisted key/address instead of re-invoking `wg-quick` (which would
/// fail loudly on an already-configured interface, same failure mode a
/// manual `wg-up` hits — see main.rs's `run_wg_up`).
///
/// Best-effort: any failure (no CAP_NET_ADMIN, `wg-quick` missing, a genuine
/// address collision) is logged and returns `None` — a node without WG
/// capability still runs fine on direct transport alone, exactly as every
/// node does today.
pub fn ensure_up(db_path: &Path, node_id: &str) -> Option<Arc<WgState>> {
    use sigil_net_wg::WgPrivateKey;

    let keys_dir = db_path.join("wg-keys");
    let key_path = keys_dir.join(format!("{IFACE}.key"));

    let private_key = if key_path.exists() {
        match std::fs::read_to_string(&key_path)
            .ok()
            .and_then(|s| WgPrivateKey::from_base64(s.trim()).ok())
        {
            Some(k) => k,
            None => {
                eprintln!("⚠ wg-relay: couldn't read/parse existing key at {} — skipping auto-WG", key_path.display());
                return None;
            }
        }
    } else {
        if let Err(e) = std::fs::create_dir_all(&keys_dir) {
            eprintln!("⚠ wg-relay: couldn't create {}: {e} — skipping auto-WG", keys_dir.display());
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&keys_dir) {
                let mut perms = meta.permissions();
                perms.set_mode(0o700);
                let _ = std::fs::set_permissions(&keys_dir, perms);
            }
        }
        let sk = WgPrivateKey::generate();
        if let Err(e) = std::fs::write(&key_path, sk.to_base64()) {
            eprintln!("⚠ wg-relay: couldn't write key to {}: {e} — skipping auto-WG", key_path.display());
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&key_path) {
                let mut perms = meta.permissions();
                perms.set_mode(0o600);
                let _ = std::fs::set_permissions(&key_path, perms);
            }
        }
        sk
    };

    let host = derive_host_octet(node_id);
    let wg_ip = format!("{SUBNET_PREFIX}.{host}");
    let public_key = private_key.public();

    // Already up from a prior boot on THIS host+octet? Don't re-invoke
    // wg-quick (it fails loudly on an already-configured interface) — just
    // reuse it. `ip addr show` succeeding with our expected address is
    // enough evidence; a mismatch (stale interface from a different key/IP)
    // is surfaced, not silently overridden.
    let already_up = std::process::Command::new("ip")
        .args(["-4", "addr", "show", IFACE])
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains(&format!("{wg_ip}/24")))
        .unwrap_or(false);

    if !already_up {
        let peers = load_peers_manifest_for(db_path, IFACE);
        let interface = WgInterface {
            name: IFACE.to_string(),
            private_key,
            listen_port: LISTEN_PORT,
            addresses: vec![format!("{wg_ip}/24")],
            mtu: None,
            peers,
        };
        let backend = CliWgBackend::default();
        if let Err(e) = backend.apply_interface(&interface) {
            eprintln!("⚠ wg-relay: auto wg-up failed ({e}) — nodes will still connect via direct transport, just without the WG side-mesh");
            return None;
        }
        eprintln!("✓ wg-relay: auto-brought-up {IFACE} at {wg_ip}/24 (pubkey {}…)", &public_key.to_base64()[..12]);
    } else {
        eprintln!("✓ wg-relay: {IFACE} already up at {wg_ip}/24 (pubkey {}…) — reusing", &public_key.to_base64()[..12]);
    }

    Some(Arc::new(WgState {
        iface: IFACE.to_string(),
        pubkey_b64: public_key.to_base64(),
        wg_ip,
        wg_port: LISTEN_PORT,
        db_path: db_path.to_path_buf(),
        peer_ips: Mutex::new(HashMap::new()),
        already_peered: Mutex::new(std::collections::HashSet::new()),
    }))
}

/// Record a peer's source IP from `SwarmAppEvent::PeerConnected` — needed
/// later to build their WG `endpoint` when their Hello arrives (InboundRequest
/// doesn't carry the sender's IP directly).
pub fn note_peer_ip(state: &WgState, peer: flux_p2p::PeerId, addr: &str) {
    // addr is a multiaddr string like "/ip4/1.2.3.4/tcp/9501" — pull the IP out.
    if let Some(ip) = addr.split('/').nth(2) {
        state.peer_ips.lock().unwrap().insert(peer, ip.to_string());
    }
}

pub fn our_hello(state: &WgState) -> WgHelloMsg {
    WgHelloMsg { pubkey_b64: state.pubkey_b64.clone(), wg_port: state.wg_port, wg_ip: state.wg_ip.clone() }
}

/// Apply a peer's Hello: live `wg set` + persist to the manifest, using the
/// IP we recorded for them at connect time. No-op (not an error) if we never
/// saw a PeerConnected for them, or if they're already peered.
pub fn handle_hello(state: &Arc<WgState>, peer: flux_p2p::PeerId, hello: &WgHelloMsg) {
    if state.pubkey_b64 == hello.pubkey_b64 {
        return; // a hello echoed back to ourselves — never peer with our own key
    }
    if !state.already_peered.lock().unwrap().insert(hello.pubkey_b64.clone()) {
        return; // already applied this peer
    }
    let Some(ip) = state.peer_ips.lock().unwrap().get(&peer).cloned() else {
        eprintln!("⚠ wg-relay: hello from {peer} but no known IP for them — skipping");
        state.already_peered.lock().unwrap().remove(&hello.pubkey_b64);
        return;
    };
    let Ok(pk) = WgPublicKey::from_base64(&hello.pubkey_b64) else {
        eprintln!("⚠ wg-relay: hello from {peer} carried an unparseable pubkey — skipping");
        state.already_peered.lock().unwrap().remove(&hello.pubkey_b64);
        return;
    };
    let endpoint: SocketAddr = match format!("{ip}:{}", hello.wg_port).parse() {
        Ok(a) => a,
        Err(_) => {
            eprintln!("⚠ wg-relay: bad endpoint {ip}:{} from {peer} — skipping", hello.wg_port);
            state.already_peered.lock().unwrap().remove(&hello.pubkey_b64);
            return;
        }
    };
    let allowed = format!("{}/32", hello.wg_ip);

    // Live apply — best-effort, matching run_wg_add_peer's own tolerance:
    // a failure here just means this peer waits for the next full wg-up.
    let status = std::process::Command::new("wg")
        .arg("set").arg(&state.iface)
        .arg("peer").arg(&hello.pubkey_b64)
        .arg("endpoint").arg(endpoint.to_string())
        .arg("allowed-ips").arg(&allowed)
        .status();
    match status {
        Ok(s) if s.success() => {
            eprintln!("✓ wg-relay: peered {peer} live — {} @ {endpoint} (allowed {allowed})", &hello.pubkey_b64[..12]);
        }
        Ok(s) => eprintln!("⚠ wg-relay: wg set for {peer} exited {:?}", s.code()),
        Err(e) => eprintln!("⚠ wg-relay: wg binary not invokable ({e}) — {peer} not peered this session"),
    }

    // Persist regardless of live-apply outcome, same as run_wg_add_peer —
    // a peer added here survives the next full wg-up even if `wg set` failed
    // right now (interface briefly down, etc).
    let mut peers = load_peers_manifest_for(&state.db_path, &state.iface);
    peers.retain(|p| p.public_key != pk);
    peers.push(WgPeer { public_key: pk, preshared_key: None, endpoint: Some(endpoint), allowed_ips: vec![allowed], persistent_keepalive: Some(25) });
    save_peers_manifest_for(&state.db_path, &state.iface, &peers);
}

fn peers_manifest_path_for(db_path: &Path, iface: &str) -> PathBuf {
    db_path.join("wg-peers").join(format!("{iface}.json"))
}

fn load_peers_manifest_for(db_path: &Path, iface: &str) -> Vec<WgPeer> {
    let p = peers_manifest_path_for(db_path, iface);
    let Ok(bytes) = std::fs::read(&p) else { return Vec::new() };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn save_peers_manifest_for(db_path: &Path, iface: &str, peers: &[WgPeer]) {
    let p = peers_manifest_path_for(db_path, iface);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = p.with_extension("json.tmp");
    if let Ok(bytes) = serde_json::to_vec_pretty(peers) {
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_octet_is_deterministic_and_in_range() {
        let a = derive_host_octet("sigil-g0-epsilon");
        let b = derive_host_octet("sigil-g0-epsilon");
        assert_eq!(a, b, "same node_id must always derive the same octet");
        assert!(a >= 1 && a <= 253);
    }

    #[test]
    fn different_node_ids_usually_differ() {
        // Not a proof (hash collisions are possible), but catches a broken
        // derivation that maps everything to the same octet.
        let ids: Vec<u8> = (0..20).map(|i| derive_host_octet(&format!("node-{i}"))).collect();
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert!(unique.len() > 15, "20 distinct node_ids should mostly land on distinct octets, got {} unique", unique.len());
    }
}
