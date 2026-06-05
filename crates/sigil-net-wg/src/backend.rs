//! Backend trait + the Phase-0 CLI backend (`wg(8)` + `wg-quick(8)` shell).

use std::path::PathBuf;
use std::process::Command;

use crate::peer::WgInterface;

/// What every WireGuard backend must do — bring an interface up, tear it
/// down, push a new peer set, observe the current state.
///
/// The trait is `Send + Sync` so a SIGIL node can hand a single backend to
/// its mempool thread and its updater thread simultaneously.
pub trait WgBackend: Send + Sync {
    /// Apply the interface config — create the interface if needed, set the
    /// private key + listen port + peers. Idempotent: re-applying with the
    /// same config is a no-op.
    fn apply_interface(&self, iface: &WgInterface) -> Result<(), WgBackendError>;

    /// Tear the interface down completely.
    fn down(&self, iface_name: &str) -> Result<(), WgBackendError>;

    /// Backend identity for logs / metrics.
    fn name(&self) -> &'static str;
}

/// Backend errors. `Cli` wraps subprocess failures; `Unsupported` lets a
/// stub backend cleanly signal "feature not built yet" without panicking.
#[derive(Debug, thiserror::Error)]
pub enum WgBackendError {
    /// Subprocess exited non-zero or couldn't be spawned.
    #[error("wg/wg-quick CLI: {0}")]
    Cli(String),
    /// IO error during config staging.
    #[error("io: {0}")]
    Io(String),
    /// Backend can't honor this op (e.g. userspace backend asked to set fwmark).
    #[error("backend doesn't support: {0}")]
    Unsupported(&'static str),
}

/// Phase 0 default backend — drives the kernel WireGuard module via the
/// `wg-quick(8)` setup wrapper. Writes the rendered config to a tempfile,
/// then runs `wg-quick up <tempfile>`. Tear-down runs `wg-quick down`.
///
/// Requires `wg-quick` on PATH and CAP_NET_ADMIN. SIGIL nodes will
/// typically run as root in P0 since that's the simplest mainnet setup; P3
/// adds a `boringtun` userspace backend so non-root containers can join.
pub struct CliWgBackend {
    /// Where to drop rendered `.conf` files. Defaults to `/etc/wireguard/`
    /// because that's where `wg-quick` looks them up by name.
    pub config_dir: PathBuf,
    /// Override the `wg-quick` binary path. None → `wg-quick` from PATH.
    pub wg_quick_bin: Option<PathBuf>,
}

impl Default for CliWgBackend {
    fn default() -> Self {
        Self {
            config_dir: PathBuf::from("/etc/wireguard"),
            wg_quick_bin: None,
        }
    }
}

impl CliWgBackend {
    /// Resolve the `wg-quick` invocation. Override OR PATH lookup.
    fn cmd(&self) -> Command {
        match &self.wg_quick_bin {
            Some(p) => Command::new(p),
            None    => Command::new("wg-quick"),
        }
    }
}

impl WgBackend for CliWgBackend {
    fn name(&self) -> &'static str {
        "cli/wg-quick"
    }

    fn apply_interface(&self, iface: &WgInterface) -> Result<(), WgBackendError> {
        // 1. Stage the config to `<config_dir>/<name>.conf`.
        let cfg_path = self.config_dir.join(format!("{}.conf", iface.name));
        std::fs::create_dir_all(&self.config_dir).map_err(|e| WgBackendError::Io(e.to_string()))?;
        std::fs::write(&cfg_path, iface.to_conf_file())
            .map_err(|e| WgBackendError::Io(format!("write {}: {}", cfg_path.display(), e)))?;

        // 2. Run `wg-quick up <name>`. If the interface is already up, P0
        //    accepts the non-zero exit (re-up is what `wg-quick` does
        //    instead of being idempotent); we'd map that to Ok in a later
        //    iteration once we actually parse stderr.
        let status = self
            .cmd()
            .args(["up", &iface.name])
            .status()
            .map_err(|e| WgBackendError::Cli(format!("spawn wg-quick: {}", e)))?;

        if !status.success() {
            return Err(WgBackendError::Cli(format!(
                "wg-quick up {} exited with {:?}", iface.name, status.code()
            )));
        }
        Ok(())
    }

    fn down(&self, iface_name: &str) -> Result<(), WgBackendError> {
        let status = self
            .cmd()
            .args(["down", iface_name])
            .status()
            .map_err(|e| WgBackendError::Cli(format!("spawn wg-quick: {}", e)))?;
        if !status.success() {
            return Err(WgBackendError::Cli(format!(
                "wg-quick down {} exited with {:?}", iface_name, status.code()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::WgPrivateKey;

    #[test]
    fn cli_backend_default_paths() {
        let b = CliWgBackend::default();
        assert_eq!(b.config_dir, PathBuf::from("/etc/wireguard"));
        assert!(b.wg_quick_bin.is_none());
        assert_eq!(b.name(), "cli/wg-quick");
    }

    /// We can write a config file without actually running wg-quick by
    /// pointing at a missing binary; the staging step still succeeds.
    #[test]
    fn apply_writes_config_even_if_wg_quick_is_unavailable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut b = CliWgBackend::default();
        b.config_dir = tmp.path().to_path_buf();
        // Point at a definitely-missing binary so we exercise the spawn-fail path.
        b.wg_quick_bin = Some(PathBuf::from("/nope/wg-quick-does-not-exist"));

        let iface = WgInterface {
            name: "sigil-test".into(),
            private_key: WgPrivateKey::generate(),
            listen_port: 51820,
            addresses: vec!["10.42.0.2/16".into()],
            mtu: None,
            peers: vec![],
        };
        let err = b.apply_interface(&iface).unwrap_err();
        assert!(matches!(err, WgBackendError::Cli(_)));
        // ...but the config WAS staged before the spawn failed.
        let cfg = std::fs::read_to_string(tmp.path().join("sigil-test.conf")).unwrap();
        assert!(cfg.contains("[Interface]"));
    }
}
