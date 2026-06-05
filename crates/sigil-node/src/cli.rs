//! Bare-bones CLI parsing — no clap dependency to keep Phase 0 dep graph
//! tight. Just `sigil-node <subcommand> [args]`.

/// One parsed CLI invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cli {
    /// `sigil-node start` — placeholder for the long-running node loop.
    Start,
    /// `sigil-node show-tip` — print the current chain height + tip hash.
    ShowTip,
    /// `sigil-node mint-genesis` — produce + apply block 0 locally, dump
    /// the resulting header to stdout. Useful for sanity checks; the real
    /// genesis ceremony bakes the producer's SQIsign sig and the
    /// `SIGIL_GENESIS_v0.md` BLAKE3 into the header.
    MintGenesis,
    /// `sigil-node produce-block <txs.json> [--broadcast [--dry-run]]` —
    /// mint genesis, apply, then load a JSON `Vec<SignedTx>` from the given
    /// path, fold them into one `StateTransition`, mint block 1, apply,
    /// print tip.
    ///
    /// With `--broadcast`, spins up a flux-p2p NetworkManager, publishes
    /// the block JSON on the `/sigil/g0/blocks` gossipsub topic, waits a
    /// few seconds for propagation, then shuts down.
    ///
    /// With `--broadcast --dry-run`, performs the wire-format serialization
    /// and a roundtrip-hash assertion, then prints metadata and exits
    /// without touching the network. Useful for local wire-format checks
    /// and for the P3-D divergence demo's pre-flight pass.
    ProduceBlock { tx_file: String, broadcast: bool, dry_run: bool },
    /// `sigil-node wg-up <iface>` — bring the SIGIL WireGuard mesh up via
    /// `wg-quick(8)`. Loads or generates the node's WG private key under
    /// `<SIGIL_DB_PATH>/wg-keys/<iface>.key` (chmod 0600), renders an
    /// interface conf, calls [`sigil_net_wg::CliWgBackend::apply_interface`].
    /// Prints the resulting public key + listen port so the operator can
    /// hand them to other validators.
    WgUp { iface: String },
    /// `sigil-node wg-down <iface>` — tear an interface down via
    /// `wg-quick down <iface>`. Keypair file stays on disk.
    WgDown { iface: String },
    /// `sigil-node wg-add-peer <iface> <pubkey-b64> <endpoint> <allowed_ips>` —
    /// append a peer to `$SIGIL_DB_PATH/wg-peers/<iface>.json` AND shell
    /// `wg set <iface> peer <pubkey> endpoint ... allowed-ips ...` to apply
    /// it live. If the interface isn't up, the peer is saved for the next
    /// `wg-up` call and a warning is logged. `allowed_ips` is comma-separated.
    WgAddPeer {
        iface: String,
        public_key: String,
        endpoint: String,
        allowed_ips: String,
    },
    /// `sigil-node wg-list-peers <iface>` — print the manifest in tabular form.
    WgListPeers { iface: String },
    /// `sigil-node version` — print the schema version + crate version.
    Version,
    /// Unknown / `--help` / no args — print usage.
    Help,
}

impl Cli {
    /// Parse argv. The binary name is expected as argv[0]; the subcommand
    /// (if any) as argv[1].
    pub fn parse<I, S>(argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut it = argv.into_iter();
        let _bin = it.next();
        let sub = it.next().map(|s| s.as_ref().to_string());
        match sub.as_deref() {
            Some("start")         => Cli::Start,
            Some("show-tip")      => Cli::ShowTip,
            Some("mint-genesis")  => Cli::MintGenesis,
            Some("produce-block") => {
                // `sigil-node produce-block <path> [--broadcast [--dry-run]]`.
                let tx_file = it.next().map(|s| s.as_ref().to_string()).unwrap_or_default();
                let mut broadcast = false;
                let mut dry_run = false;
                for arg in it {
                    match arg.as_ref() {
                        "--broadcast" => broadcast = true,
                        "--dry-run"   => dry_run = true,
                        _ => {}
                    }
                }
                if tx_file.is_empty() {
                    Cli::Help
                } else {
                    Cli::ProduceBlock { tx_file, broadcast, dry_run }
                }
            }
            Some("wg-up") => {
                let iface = it.next().map(|s| s.as_ref().to_string()).unwrap_or_default();
                if iface.is_empty() { Cli::Help } else { Cli::WgUp { iface } }
            }
            Some("wg-down") => {
                let iface = it.next().map(|s| s.as_ref().to_string()).unwrap_or_default();
                if iface.is_empty() { Cli::Help } else { Cli::WgDown { iface } }
            }
            Some("wg-add-peer") => {
                let iface       = it.next().map(|s| s.as_ref().to_string()).unwrap_or_default();
                let public_key  = it.next().map(|s| s.as_ref().to_string()).unwrap_or_default();
                let endpoint    = it.next().map(|s| s.as_ref().to_string()).unwrap_or_default();
                let allowed_ips = it.next().map(|s| s.as_ref().to_string()).unwrap_or_default();
                if iface.is_empty() || public_key.is_empty() || endpoint.is_empty() || allowed_ips.is_empty() {
                    Cli::Help
                } else {
                    Cli::WgAddPeer { iface, public_key, endpoint, allowed_ips }
                }
            }
            Some("wg-list-peers") => {
                let iface = it.next().map(|s| s.as_ref().to_string()).unwrap_or_default();
                if iface.is_empty() { Cli::Help } else { Cli::WgListPeers { iface } }
            }
            Some("version") | Some("--version") | Some("-V") => Cli::Version,
            _                     => Cli::Help,
        }
    }

    /// Single-screen usage text — exit code 0 on `Help`, 64 on unknown
    /// subcommand to match the standard `EX_USAGE` from sysexits.h.
    pub fn usage() -> &'static str {
        "\
sigil-node — SIGIL block producer / verifier (Phase 0)

Usage:
  sigil-node <subcommand>

Subcommands:
  start            Run the node (Phase 0: no-op, exits after init)
  show-tip         Print current chain height + tip hash
  mint-genesis     Mint block 0 locally and print the header
  produce-block <txs.json> [--broadcast [--dry-run]]
                   Mint genesis + apply, load JSON Vec<SignedTx>, apply
                   each into one batched block 1, print resulting tip.
                   With --broadcast, also publishes block 1 on the
                   /sigil/g0/blocks gossipsub topic before exiting.
                   Add --dry-run after --broadcast to serialize + roundtrip
                   without starting the network (wire-format pre-flight).
                   Phase 0 demo of the tx → state → block (→ network) pipeline.
  wg-up <iface>    Bring up the SIGIL WireGuard interface via wg-quick(8).
                   Loads or generates the node's WG keypair at
                   $SIGIL_DB_PATH/wg-keys/<iface>.key (chmod 0600). Tunable
                   via SIGIL_WG_LISTEN_PORT (default 51820) and
                   SIGIL_WG_ADDRESS (default 10.42.0.1/16 — REQUIRES
                   per-node override on a real mesh).
  wg-down <iface>  Tear down a WireGuard interface (wg-quick down).
  wg-add-peer <iface> <pubkey-b64> <endpoint> <allowed_ips>
                   Append a peer to the persisted peer manifest at
                   $SIGIL_DB_PATH/wg-peers/<iface>.json AND `wg set` it live
                   if the interface is up. `allowed_ips` is comma-separated
                   (e.g. 10.42.0.5/32 or 10.42.0.5/32,10.43.0.0/24).
  wg-list-peers <iface>
                   Print the persisted peer manifest in tabular form.
  version          Print schema version + crate version

Run `sigil-node version` to confirm your binary's CARGO_PKG_VERSION + header schema version.
"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_subcommands() {
        assert_eq!(Cli::parse(["sigil-node", "start"]),        Cli::Start);
        assert_eq!(Cli::parse(["sigil-node", "show-tip"]),     Cli::ShowTip);
        assert_eq!(Cli::parse(["sigil-node", "mint-genesis"]), Cli::MintGenesis);
        assert_eq!(Cli::parse(["sigil-node", "version"]),      Cli::Version);
        assert_eq!(Cli::parse(["sigil-node", "--version"]),    Cli::Version);
        assert_eq!(Cli::parse(["sigil-node", "-V"]),           Cli::Version);
        assert_eq!(Cli::parse(["sigil-node"]),                 Cli::Help);
        assert_eq!(Cli::parse(["sigil-node", "bogus"]),        Cli::Help);
        assert_eq!(
            Cli::parse(["sigil-node", "produce-block", "/tmp/txs.json"]),
            Cli::ProduceBlock { tx_file: "/tmp/txs.json".into(), broadcast: false, dry_run: false }
        );
        assert_eq!(
            Cli::parse(["sigil-node", "produce-block", "/tmp/txs.json", "--broadcast"]),
            Cli::ProduceBlock { tx_file: "/tmp/txs.json".into(), broadcast: true, dry_run: false }
        );
        assert_eq!(
            Cli::parse(["sigil-node", "produce-block", "/tmp/txs.json", "--broadcast", "--dry-run"]),
            Cli::ProduceBlock { tx_file: "/tmp/txs.json".into(), broadcast: true, dry_run: true }
        );
        // Missing path falls through to Help — keeps the binary friendly.
        assert_eq!(Cli::parse(["sigil-node", "produce-block"]), Cli::Help);
        // WG subcommands.
        assert_eq!(
            Cli::parse(["sigil-node", "wg-up", "sigil0"]),
            Cli::WgUp { iface: "sigil0".into() }
        );
        assert_eq!(
            Cli::parse(["sigil-node", "wg-down", "sigil0"]),
            Cli::WgDown { iface: "sigil0".into() }
        );
        // Missing iface → Help.
        assert_eq!(Cli::parse(["sigil-node", "wg-up"]),   Cli::Help);
        assert_eq!(Cli::parse(["sigil-node", "wg-down"]), Cli::Help);

        assert_eq!(
            Cli::parse(["sigil-node", "wg-add-peer", "sigil0", "B29K...=",
                        "203.0.113.5:51820", "10.42.0.5/32"]),
            Cli::WgAddPeer {
                iface: "sigil0".into(),
                public_key: "B29K...=".into(),
                endpoint: "203.0.113.5:51820".into(),
                allowed_ips: "10.42.0.5/32".into(),
            }
        );
        // Any missing positional arg → Help (4 required).
        assert_eq!(Cli::parse(["sigil-node", "wg-add-peer", "sigil0", "B29K...="]), Cli::Help);
        assert_eq!(
            Cli::parse(["sigil-node", "wg-list-peers", "sigil0"]),
            Cli::WgListPeers { iface: "sigil0".into() }
        );
        assert_eq!(Cli::parse(["sigil-node", "wg-list-peers"]), Cli::Help);
    }
}
