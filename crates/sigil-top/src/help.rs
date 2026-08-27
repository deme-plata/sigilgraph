//! `sigil-top --help` — the full command reference.
//!
//! Lives in its own module rather than inline in `main.rs` for two reasons: `main.rs` is a
//! heavily-contended file in this tree (several agents edit it concurrently), and help text
//! that sits next to argument parsing tends to document the flags someone happened to be
//! looking at rather than the whole surface.
//!
//! # What was wrong with the old help
//!
//! It was 13 lines. It documented six flags and two subcommands out of **20+ subcommands
//! and 25+ environment variables**, so the majority of what this binary does was
//! undiscoverable without reading the source.
//!
//! Worse, it was actively misleading in one place: it advertised
//! `--api https://sigilgraph.fluxapp.xyz/api/v1/status` as the default. Those `fluxapp`
//! vhosts still proxy `/api` and `/v1` to `127.0.0.1:8099` — `sigil-rpcd`, which was
//! retired and permanently disabled on 2026-08-17. The page loads, the URL looks right, and
//! every call fails. Anyone who copied that line out of `--help` inherited a monitor
//! pointed at a dead backend. The default is now `sigilgraph.org`, the one domain where the
//! wallet and a live API actually coexist.
//!
//! Contents are grouped by what a reader is trying to DO, not by how the flags happen to be
//! parsed — the sections a newcomer needs (what this binary IS, how to watch a node, how to
//! mine) come before the ones only an operator needs (env overrides, integration).

/// The status endpoint used when `--api` is not given.
///
/// **Must not be a `fluxapp` domain.** See the module docs: those route to a retired
/// backend and fail every call while looking healthy.
pub const DEFAULT_API: &str = "https://sigilgraph.org/api/v1/status";

pub fn print_help(version: &str) {
    print!("{}", render(version));
}

/// The help text as a string. Split from `print_help` so the tests assert on what a user
/// ACTUALLY sees rather than on this file's source — an earlier draft used
/// `include_str!("help.rs")`, which contains every needle by construction and would have
/// passed even if `print_help` printed nothing at all.
pub fn render(version: &str) -> String {
    format!(
        r#"sigil-top {version} — SIGIL node monitor, light client, miner and wallet host

  A single binary that watches a SIGIL chain, VERIFIES it for itself rather than
  trusting a feed, mines against it, and serves the wallet UI locally. Run it with
  no arguments for the live dashboard.

USAGE
  sigil-top [FLAGS]                 live dashboard (default)
  sigil-top <SUBCOMMAND> [ARGS]

DASHBOARD
  -l, --lite                        compact scorecard instead of the full dashboard
  -1, --once                        print one snapshot and exit (scripts, CI, cron)
  -n, --interval N                  seconds between refreshes (default 2)
      --tui                         interactive alt-screen TUI with key bindings
      --api URL                     status endpoint
                                    (default {DEFAULT_API})
      --feed URL                    live block feed the dashboard syncs from
  -h, --help                        this text

  In the TUI: [M]ine  [F]ull  [V]erify  [Y]resync  [U]pdate  [L]ogin  [T]stats  [Q]uit

CHAIN — download and verify
  full-sync                         download AND verify genesis→tip, then exit 0 once the
                                    verified spine reaches the network tip.
                                    [--target H] [--timeout S]
  verify-chain                      re-verify the LOCAL store (precheck + parent linkage).
                                    Exit 1 on a break, 0 for a clean spine to genesis.
                                    [--json]

    Verifying is the point: `verified` in the dashboard is the height THIS process has
    checked itself. `tip` is only what the network claims. A gap means catching up, not
    breakage — but a `verify_break` means the store is not a connected chain, and that
    matters far more than being behind.

MINING
  mine                              mine using the seed/wallet resolved below
  mine-rig <NODE_URL>               mine against a specific node

    Reward address resolution, HIGHEST priority first — this order surprises people:
      1. SIGIL_MINE_SEED            (a seed: it both signs and sets the address)
      2. SIGIL_MINE_WALLET          (an address only)
      3. the wallet chosen in the UI  (GET /api/v1/use-wallet)
      4. a hash of the hostname     ← unspendable fallback; nobody holds this key

    So a SIGIL_MINE_SEED left over from an earlier session silently keeps crediting THAT
    wallet no matter what you pick in the UI. `GET /api/v1/mine-wallet` reports the address
    actually being credited — check it rather than assuming.

WALLET AND API
  serve                             serve the wallet UI + local API on 127.0.0.1:9800
  login / logout                    wallet session on this machine
  host                              host mode

    The local API is documented, live and runnable at https://sigilgraph.org/api.html
    (machine-readable: https://sigilgraph.org/sigil-top-openapi.json).

    🔒 The endpoints under /api/v1/mine-sign, /mine-shield, /mine-send-private and
    /adopt-seed SIGN WITH THE MINING SEED in this process. They are bound to loopback and
    must stay there: anything that can reach them can spend as you.

UPDATES AND INTEGRATION
  update                            check for and install a signed release
  autostart enable|disable          start sigil-top on login
  provenance                        show build provenance for this binary
  flux-register / flux-unregister   register the flux:// URL scheme with the desktop
  flux-open <URL>                   open a flux:// URL
  vscode                            VS Code integration

ENVIRONMENT
  Identity and rewards
    SIGIL_MINE_SEED                 seed to mine and sign with (highest priority)
    SIGIL_MINE_WALLET               reward address only, no signing
    SIGIL_MINE_CPU=1                force CPU mining even where a GPU is present
    SIGIL_MINE_DIFFICULTY           override share difficulty
    SIGIL_MINE_NODE / SIGIL_MINE_URL  node to mine against

  Endpoints
    SIGIL_NODE_URL                  node this process proxies /api and /v1 to
    SIGIL_FEED_URL                  live block feed
    SIGIL_LEDGER_URL                ledger/supply endpoint

  Storage
    SIGIL_TOP_DB                    local block store location
    SIGIL_AETHER_DIR                aether artifact store
    SIGIL_SWARM_DIR                 swarm coordination directory
    SIGIL_TOP_BOOT_STORE_LIMIT_MB   cap the store read at boot

  Behaviour
    SIGIL_HEADLESS=1                never enter the TUI (same as --once for automation)
    SIGIL_AUTOMINE=1                start mining on launch
    SIGIL_AUTOFULLSYNC=1            start a full-sync on launch
    SIGIL_FULLSYNC / SIGIL_SPINE_SYNC / SIGIL_SYNC_RECENT
                                    sync-path toggles
    SIGIL_ASCII=1                   ASCII-only output (terminals without box drawing)
    FLUX_STATIC_DIR                 directory the wallet UI is served FROM.
                                    Point it at the current wallet files if the bundled
                                    copy is older than the deployed one.
    FLUX_WALLET_URL                 wallet URL to open
    SIGIL_BROWSER                   browser used to open the wallet

EXIT CODES
  0   success — for `full-sync`, the verified spine reached the tip;
      for `verify-chain`, the local store is a clean connected chain to genesis
  1   verification failed, or the requested state was not reached

EXAMPLES
  sigil-top                                     watch a node
  sigil-top --lite --interval 5                 quiet scorecard, refreshed every 5s
  sigil-top --once                              one snapshot for a script
  sigil-top verify-chain --json                 audit the local store, machine-readable
  SIGIL_MINE_SEED=$(cat seed) sigil-top mine    mine and be paid to that seed's wallet
  sigil-top full-sync --timeout 900             download+verify, give up after 15 minutes
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The old help advertised a `fluxapp` status endpoint. Those vhosts route `/api` and
    /// `/v1` to `sigil-rpcd` on :8099, retired and disabled since 2026-08-17 — so the URL
    /// looks fine and fails every call. Anyone copying it out of `--help` inherits a
    /// monitor pointed at a dead backend, which is the most expensive kind of wrong: it
    /// reads as a bug in their setup.
    #[test]
    fn the_default_api_is_not_a_dead_fluxapp_domain() {
        assert!(
            !DEFAULT_API.contains("fluxapp"),
            "default API must not be a fluxapp domain — those proxy to the retired \
             sigil-rpcd (:8099) and fail every call while returning HTTP 200 for pages"
        );
        assert!(DEFAULT_API.starts_with("https://"), "must be https");
    }

    /// Help that omits most of the binary is how a surface becomes undiscoverable. These
    /// are the things a reader cannot find any other way without reading the source.
    #[test]
    fn help_covers_the_surface_a_reader_cannot_guess() {
        let out = render("0.0.0-test");
        for needle in [
            "full-sync", "verify-chain", "mine-rig", "serve", "update", "autostart",
            "SIGIL_MINE_SEED", "SIGIL_MINE_WALLET", "FLUX_STATIC_DIR", "SIGIL_HEADLESS",
            "EXIT CODES", "9800", "--interval", "--lite",
        ] {
            assert!(out.contains(needle), "--help must document `{needle}`");
        }
    }

    /// The reward-resolution order is the single most surprising thing about this binary —
    /// a stale `SIGIL_MINE_SEED` silently outranks the wallet chosen in the UI. If that
    /// explanation ever falls out of the help, people lose rewards to an address they are
    /// not watching.
    #[test]
    fn help_explains_that_a_stale_seed_outranks_the_chosen_wallet() {
        let out = render("0.0.0-test");
        assert!(out.contains("HIGHEST priority first"));
        assert!(out.contains("unspendable fallback"));
        assert!(out.contains("/api/v1/mine-wallet"));
    }

    /// The signing endpoints spend the mining seed. Help that lists them without saying so
    /// invites someone to expose them.
    #[test]
    fn help_warns_that_the_signing_endpoints_spend_the_seed() {
        let out = render("0.0.0-test");
        assert!(out.contains("mine-sign"));
        assert!(out.contains("SIGN WITH THE MINING SEED"));
        assert!(out.contains("loopback"));
    }

    /// It must render the version it was given, not a placeholder.
    #[test]
    fn version_is_interpolated() {
        assert!(render("9.9.9").contains("sigil-top 9.9.9"));
    }
}
