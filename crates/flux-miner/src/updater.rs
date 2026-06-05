//! updater.rs — software auto-updater for the miner (the "software autoupdater,
//! first flux" ask). Polls a version manifest; if newer, downloads + stages
//! `<exe>.new`; swaps it in on next launch. The proven Flux Cockpit pattern,
//! miner-side — so a fleet of 200 miners self-upgrades without re-downloads.

use std::time::Duration;

/// An advertised newer release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    pub version: String,
    pub url: String,
}

/// PURE: parse a manifest body (JSON `{"version","url"}` or a bare version) and
/// decide if it's newer than `current`. Unit-testable without a network.
pub fn parse_manifest(body: &str, current: &str) -> Option<UpdateInfo> {
    let (ver, url) = match serde_json::from_str::<serde_json::Value>(body) {
        Ok(v) => (
            v.get("version").and_then(|x| x.as_str()).map(String::from),
            v.get("url").and_then(|x| x.as_str()).map(String::from),
        ),
        Err(_) => (Some(body.trim().to_string()), None),
    };
    match (ver, url) {
        (Some(v), Some(u)) if v != current && !v.is_empty() => Some(UpdateInfo { version: v, url: u }),
        _ => None,
    }
}

/// Fetch the manifest URL and check for a newer version than `current`.
pub fn check(manifest_url: &str, current: &str) -> Option<UpdateInfo> {
    let body = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .ok()?
        .get(manifest_url)
        .send()
        .ok()?
        .error_for_status()
        .ok()?
        .text()
        .ok()?;
    parse_manifest(&body, current)
}

/// Download the new binary to `<current_exe>.new` (staged for swap-on-launch).
pub fn stage(url: &str) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let staged = exe.with_extension("new");
    let bytes = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| e.to_string())?
        .get(url)
        .send()
        .map_err(|e| e.to_string())?
        .bytes()
        .map_err(|e| e.to_string())?;
    std::fs::write(&staged, &bytes).map_err(|e| e.to_string())?;
    Ok(staged.display().to_string())
}

/// On startup: if `<exe>.new` exists, swap it in (self→.old, .new→self). Works
/// on Linux + Windows (a running exe can be renamed). Best-effort.
pub fn swap_on_launch() {
    if let Ok(exe) = std::env::current_exe() {
        let staged = exe.with_extension("new");
        if staged.exists() {
            let old = exe.with_extension("old");
            let _ = std::fs::remove_file(&old);
            if std::fs::rename(&exe, &old).is_ok() && std::fs::rename(&staged, &exe).is_ok() {
                let _ = std::fs::remove_file(&old);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_version_detected() {
        let m = parse_manifest(r#"{"version":"0.2.0","url":"https://x/miner"}"#, "0.1.0");
        assert_eq!(m, Some(UpdateInfo { version: "0.2.0".into(), url: "https://x/miner".into() }));
    }

    #[test]
    fn same_version_is_no_update() {
        assert!(parse_manifest(r#"{"version":"0.1.0","url":"https://x"}"#, "0.1.0").is_none());
    }

    #[test]
    fn version_without_url_is_no_update() {
        // bare/urlless manifest can't be auto-applied → no UpdateInfo.
        assert!(parse_manifest("0.9.9", "0.1.0").is_none());
    }
}
