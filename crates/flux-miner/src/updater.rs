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
    /// BLAKE3 of the binary, hex — from the SIGNED manifest. `stage` refuses a download that differs.
    pub blake3: String,
}

/// SIGIL release key 2 (2026-10-23 rotation; key 1 retired 2026-10-24). The same key sigil-top and the
/// wallet trust. Until 2026-10-24 this updater trusted nothing at all: whoever controlled the manifest URL
/// (or the server behind it) could make every miner download and run any binary.
pub const RELEASE_KEY2_HEX: &str = "a78061cfb9e2b2cdd5e876da5d737ba0edce6ef77ba3b91e83acb892741225ae";

/// PURE: accept `body` only if `sig_hex` is an Ed25519 signature over its EXACT bytes by `pubkey`, it parses
/// as `{"version","url","blake3"}`, and the version is strictly newer than `current`. Anything else — no
/// signature, a bad one, an HTML error page, a missing hash — is "no update" (fail closed).
pub fn parse_signed_manifest(body: &[u8], sig_hex: &str, current: &str, pubkey: &[u8; 32]) -> Option<UpdateInfo> {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    let key = VerifyingKey::from_bytes(pubkey).ok()?;
    let sig = <[u8; 64]>::try_from(hex::decode(sig_hex.trim()).ok()?).ok()?;
    key.verify(body, &Signature::from_bytes(&sig)).ok()?;
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let b3 = v.get("blake3").and_then(|x| x.as_str())?.trim().to_ascii_lowercase();
    if b3.len() != 64 || !b3.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut info = parse_manifest(std::str::from_utf8(body).ok()?, current)?;
    info.blake3 = b3;
    Some(info)
}

/// PURE: the staged bytes must hash to the signed manifest's BLAKE3.
pub fn check_blob(bytes: &[u8], blake3_hex: &str) -> Result<(), String> {
    let got = blake3::hash(bytes).to_hex().to_string();
    if got.eq_ignore_ascii_case(blake3_hex.trim()) {
        Ok(())
    } else {
        Err(format!("download does not match the signed manifest (blake3 {}…, expected {}…)", &got[..16], &blake3_hex.trim()[..16.min(blake3_hex.trim().len())]))
    }
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
        // STRICTLY NEWER, not merely different (2026-08-29). `v != current`
        // accepted a downgrade: a miner running 0.2.1 met a stale
        // sigil-miner-latest manifest pinned at 0.1.10 and "updated" BACKWARD,
        // losing every fix in between on every launch — an accidental (or
        // malicious) rollback channel. Semver-compare so only a real upgrade
        // proceeds; a manifest that is equal, older, or unparseable is a no-op.
        (Some(v), Some(u)) if is_strictly_newer(&v, current) => Some(UpdateInfo { version: v, url: u, blake3: String::new() }),
        _ => None,
    }
}

/// True iff `candidate` is a strictly higher dotted-numeric version than
/// `current`. Non-numeric or empty components sort as 0, missing trailing
/// components are 0 (so `1.2` == `1.2.0`), and an unparseable candidate is
/// never newer (fail-closed — no update rather than a wrong one).
fn is_strictly_newer(candidate: &str, current: &str) -> bool {
    fn parts(s: &str) -> Vec<u64> {
        s.trim()
            .trim_start_matches('v')
            .split('.')
            .map(|p| p.trim().parse::<u64>().unwrap_or(0))
            .collect()
    }
    let (a, b) = (parts(candidate), parts(current));
    let n = a.len().max(b.len());
    for i in 0..n {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

/// Fetch the manifest URL + its `.sig2` and check for a SIGNED newer version than `current`.
pub fn check(manifest_url: &str, current: &str) -> Option<UpdateInfo> {
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(8)).build().ok()?;
    let body = client.get(manifest_url).send().ok()?.error_for_status().ok()?.bytes().ok()?;
    let sig = client.get(format!("{manifest_url}.sig2")).send().ok()?.error_for_status().ok()?.text().ok()?;
    let key = <[u8; 32]>::try_from(hex::decode(RELEASE_KEY2_HEX).ok()?).ok()?;
    parse_signed_manifest(&body, &sig, current, &key)
}

/// Download the new binary to `<current_exe>.new` (staged for swap-on-launch) — only if it matches the
/// BLAKE3 of the signed manifest `info` came from.
pub fn stage(info: &UpdateInfo) -> Result<String, String> {
    let url = info.url.as_str();
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
    check_blob(&bytes, &info.blake3)?;
    std::fs::write(&staged, &bytes).map_err(|e| e.to_string())?;
    // staged binary must be executable on unix (fs::write makes it 0644) or the
    // re-exec fails with EACCES. Windows execs by extension, no bit needed.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755));
    }
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
        assert_eq!(m, Some(UpdateInfo { version: "0.2.0".into(), url: "https://x/miner".into(), blake3: String::new() }));
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

    #[test]
    fn a_stale_manifest_cannot_downgrade() {
        // The live bug (2026-08-29): running 0.2.1, manifest pinned at 0.1.10.
        assert!(parse_manifest(r#"{"version":"0.1.10","url":"https://x"}"#, "0.2.1").is_none());
    }

    #[test]
    fn semver_compares_numerically_not_lexically() {
        // 0.1.10 > 0.1.9 numerically (a string compare would say otherwise).
        assert!(is_strictly_newer("0.1.10", "0.1.9"));
        assert!(!is_strictly_newer("0.1.9", "0.1.10"));
        // 7.4.2 > 7.4.1; equal is not newer; short form pads with zeros.
        assert!(is_strictly_newer("7.4.2", "7.4.1"));
        assert!(!is_strictly_newer("7.4.1", "7.4.1"));
        assert!(is_strictly_newer("1.2", "1.1.9"));
        // unparseable candidate fails closed.
        assert!(!is_strictly_newer("garbage", "0.1.0"));
    }

    fn signed(body: &str, seed: u8) -> (ed25519_dalek::SigningKey, String) {
        use ed25519_dalek::Signer;
        let sk = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
        let sig = hex::encode(sk.sign(body.as_bytes()).to_bytes());
        (sk, sig)
    }

    const B3: &str = "4f2b0a6a8f0c5c1ad7e5b8e1d2c3b4a5968778695a4b3c2d1e0f1a2b3c4d5e6f";

    #[test]
    fn a_signed_manifest_with_a_hash_is_an_update() {
        let body = format!(r#"{{"version":"0.2.0","url":"https://x/m","blake3":"{B3}"}}"#);
        let (sk, sig) = signed(&body, 7);
        let m = parse_signed_manifest(body.as_bytes(), &sig, "0.1.0", &sk.verifying_key().to_bytes()).unwrap();
        assert_eq!((m.version.as_str(), m.blake3.as_str()), ("0.2.0", B3));
    }

    #[test]
    fn unsigned_tampered_or_foreign_manifests_are_refused() {
        let body = format!(r#"{{"version":"0.2.0","url":"https://x/m","blake3":"{B3}"}}"#);
        let (sk, sig) = signed(&body, 7);
        let pk = sk.verifying_key().to_bytes();
        assert!(parse_signed_manifest(body.as_bytes(), "", "0.1.0", &pk).is_none(), "no signature");
        let evil = body.replace("https://x/m", "https://evil/m");
        assert!(parse_signed_manifest(evil.as_bytes(), &sig, "0.1.0", &pk).is_none(), "tampered url");
        let (_, other) = signed(&body, 9);
        assert!(parse_signed_manifest(body.as_bytes(), &other, "0.1.0", &pk).is_none(), "signed by another key");
    }

    #[test]
    fn html_and_hashless_manifests_are_refused_even_when_signed() {
        // 2026-10-24: the live manifest URL answered the Quillon SPA's index.html (HTTP 200).
        let html = "<!doctype html><html></html>";
        let (sk, sig) = signed(html, 7);
        assert!(parse_signed_manifest(html.as_bytes(), &sig, "0.1.0", &sk.verifying_key().to_bytes()).is_none());
        let nohash = r#"{"version":"0.2.0","url":"https://x/m"}"#;
        let (sk, sig) = signed(nohash, 7);
        assert!(parse_signed_manifest(nohash.as_bytes(), &sig, "0.1.0", &sk.verifying_key().to_bytes()).is_none());
    }

    #[test]
    fn a_download_must_match_the_signed_hash() {
        let blob = b"the real miner";
        let good = blake3::hash(blob).to_hex().to_string();
        assert!(check_blob(blob, &good).is_ok());
        assert!(check_blob(b"a swapped binary", &good).is_err());
    }

    #[test]
    fn the_pinned_key_is_release_key_2() {
        assert!(RELEASE_KEY2_HEX.starts_with("a78061cf") && RELEASE_KEY2_HEX.ends_with("25ae"));
        assert_eq!(hex::decode(RELEASE_KEY2_HEX).unwrap().len(), 32);
    }
}
