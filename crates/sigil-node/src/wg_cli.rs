//! WireGuard CLI subcommands (`wg-up` / `wg-down` / `wg-add-peer` / `wg-list-peers`).
//! Extracted from main.rs; the persistence layer they use lives in `wg_manifest`.
//! `use super::*` reaches main.rs's WG_* env consts + the wg_manifest re-exports.
use super::*;
use anyhow::{anyhow, Context, Result};

/// Bring up the SIGIL WireGuard interface via `wg-quick(8)`.
pub(crate) fn run_wg_up(iface: &str) -> Result<()> {
    use sigil_net::SigilNetConfig;
    use sigil_net_wg::{
        CliWgBackend, WgBackend, WgInterface, WgPrivateKey, DEFAULT_INTERFACE_NAME,
        DEFAULT_LISTEN_PORT,
    };

    let cfg = SigilNetConfig::default();
    cfg.validate()?;

    let listen_port: u16 = match std::env::var(WG_LISTEN_PORT_ENV) {
        Ok(s) if !s.is_empty() => s.parse().context("SIGIL_WG_LISTEN_PORT must be a u16")?,
        _ => DEFAULT_LISTEN_PORT,
    };
    let address = std::env::var(WG_ADDRESS_ENV).unwrap_or_else(|_| "10.42.0.1/16".to_string());
    if std::env::var(WG_ADDRESS_ENV).is_err() {
        eprintln!(
            "⚠ SIGIL_WG_ADDRESS unset — using dev default {}. \
             EVERY node on a real SIGIL mesh MUST set this to a unique CIDR.",
            address
        );
    }

    // Key path: <db_path>/wg-keys/<iface>.key
    let keys_dir = cfg.db_path.join("wg-keys");
    let key_path = keys_dir.join(format!("{iface}.key"));

    let private_key = load_or_generate_wg_key(&keys_dir, &key_path)
        .with_context(|| format!("loading or generating WG key at {}", key_path.display()))?;
    let public_key = private_key.public();

    // Load any peers that were saved with `wg-add-peer` so they survive
    // wg-quick down/up cycles.
    let peers = load_peers_manifest(&cfg.db_path, iface).with_context(|| {
        format!("loading WG peers manifest for {}", iface)
    })?;
    let interface = WgInterface {
        name: if iface == DEFAULT_INTERFACE_NAME { iface.to_string() } else { iface.to_string() },
        private_key,
        listen_port,
        addresses: vec![address.clone()],
        mtu: None,
        peers,
    };

    eprintln!("⚙  sigil-node wg-up");
    eprintln!("   interface:   {}", interface.name);
    eprintln!("   key_path:    {}", key_path.display());
    eprintln!("   public_key:  {}", public_key.to_base64());
    eprintln!("   listen_port: {}", listen_port);
    eprintln!("   address:     {}", address);
    if interface.peers.is_empty() {
        eprintln!("   peers:       0 (add via `sigil-node wg-add-peer {} <pubkey> <endpoint> <allowed_ips>`)", interface.name);
    } else {
        eprintln!("   peers:       {} (from manifest)", interface.peers.len());
        for p in &interface.peers {
            let ep = p.endpoint.map(|s| s.to_string()).unwrap_or_else(|| "<no endpoint>".into());
            eprintln!("                - {} → {}", &p.public_key.to_base64()[..16], ep);
        }
    }

    let backend = CliWgBackend::default();
    backend.apply_interface(&interface).with_context(|| {
        format!("wg-quick up {} failed — check that wg/wg-quick is installed and CAP_NET_ADMIN is held", interface.name)
    })?;

    println!("✓ wg-up: interface {} is up. Share this public key with peer operators:", interface.name);
    println!("  {}", public_key.to_base64());
    Ok(())
}

/// Tear down the SIGIL WireGuard interface via `wg-quick(8)`.
pub(crate) fn run_wg_down(iface: &str) -> Result<()> {
    use sigil_net_wg::{CliWgBackend, WgBackend};
    let backend = CliWgBackend::default();
    backend.down(iface).with_context(|| format!("wg-quick down {} failed", iface))?;
    println!("✓ wg-down: interface {} is down. Keypair file left on disk.", iface);
    Ok(())
}

/// Append a peer to the persisted manifest and apply it live via `wg set`.
/// Live application is best-effort — if `wg set` fails (interface down,
/// `wg` missing), the manifest write still succeeds and a warning is logged.
pub(crate) fn run_wg_add_peer(iface: &str, public_key: &str, endpoint: &str, allowed_ips: &str) -> Result<()> {
    use sigil_net::SigilNetConfig;
    use sigil_net_wg::{WgPeer, WgPublicKey};

    let cfg = SigilNetConfig::default();
    cfg.validate()?;

    // Validate inputs upfront so a bad pubkey/endpoint doesn't poison the manifest.
    let pk = WgPublicKey::from_base64(public_key)
        .with_context(|| format!("parsing WG public key {:?}", public_key))?;
    let ep: std::net::SocketAddr = endpoint
        .parse()
        .with_context(|| format!("parsing endpoint {:?} as <host>:<port>", endpoint))?;
    let allowed_list: Vec<String> = allowed_ips
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if allowed_list.is_empty() {
        return Err(anyhow!("allowed_ips must contain at least one CIDR"));
    }

    let mut peers = load_peers_manifest(&cfg.db_path, iface)?;
    // Replace any existing peer with the same public key — operators expect
    // re-running wg-add-peer with the same key to update the endpoint, not
    // duplicate the entry.
    let replaced = peers.iter().position(|p| p.public_key == pk).is_some();
    peers.retain(|p| p.public_key != pk);
    let new_peer = WgPeer {
        public_key: pk,
        preshared_key: None,
        endpoint: Some(ep),
        allowed_ips: allowed_list.clone(),
        persistent_keepalive: None,
    };
    peers.push(new_peer);
    save_peers_manifest(&cfg.db_path, iface, &peers)?;

    let action = if replaced { "updated" } else { "added" };
    println!("✓ wg-add-peer: {} peer {} ({} total in manifest)", action, public_key, peers.len());
    println!("  manifest: {}", peers_manifest_path(&cfg.db_path, iface).display());

    // Best-effort live apply.
    let status = std::process::Command::new("wg")
        .arg("set").arg(iface)
        .arg("peer").arg(public_key)
        .arg("endpoint").arg(endpoint)
        .arg("allowed-ips").arg(allowed_ips)
        .status();
    match status {
        Ok(s) if s.success() => println!("  live: wg set succeeded (peer reachable immediately)"),
        Ok(s) => eprintln!(
            "⚠ live apply: wg set {iface} exited {:?} — manifest saved, peer takes effect on next `sigil-node wg-up {iface}`",
            s.code()
        ),
        Err(e) => eprintln!(
            "⚠ live apply: wg binary not invokable ({e}) — manifest saved, peer takes effect on next `sigil-node wg-up {iface}`"
        ),
    }
    Ok(())
}

/// Print the persisted peer manifest for `iface`.
pub(crate) fn run_wg_list_peers(iface: &str) -> Result<()> {
    use sigil_net::SigilNetConfig;
    let cfg = SigilNetConfig::default();
    cfg.validate()?;
    let peers = load_peers_manifest(&cfg.db_path, iface)?;
    let p = peers_manifest_path(&cfg.db_path, iface);
    println!("manifest: {}", p.display());
    if peers.is_empty() {
        println!("(no peers — add with `sigil-node wg-add-peer {iface} <pubkey> <endpoint> <allowed_ips>`)");
        return Ok(());
    }
    println!("{:<4} {:<48} {:<22} {}", "#", "public_key", "endpoint", "allowed_ips");
    for (i, peer) in peers.iter().enumerate() {
        let ep = peer.endpoint.map(|e| e.to_string()).unwrap_or_else(|| "<none>".into());
        println!(
            "{:<4} {:<48} {:<22} {}",
            i,
            peer.public_key.to_base64(),
            ep,
            peer.allowed_ips.join(",")
        );
    }
    Ok(())
}
