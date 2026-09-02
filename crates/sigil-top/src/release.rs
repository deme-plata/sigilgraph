//! Release channel: manifest model, signature verification, and the fetch/anchor
//! helpers the auto-updater runs on. Extracted from main.rs (god-file split);
//! pairs with `self_update.rs`, which drives these to swap the binary in place.
//!
//! `verify_manifest_sig` / `RELEASE_SIGN_PUBKEY_HEX` are the fail-closed gate: no
//! runtime bypass, the pinned key is compiled in, an unsigned/mis-signed manifest
//! is refused. Nothing here mutates chain state.

use serde::Deserialize;
use std::time::Duration;

use super::{ACTIVE_BASE, CHANNEL_BASES, FLUX_REV, VERSION};

/// One platform's prebuilt in the manifest. Our per-OS extension to the flux
/// release-channel shape (`flux_release_check` reads the top-level fields only).
/// Backward-compat: v0.3.x manifests used `blake3` and `size` keys.
#[derive(Deserialize, Default, Clone)]
pub(crate) struct Target {
    pub(crate) url: String,
    #[serde(default, alias = "blake3")]
    pub(crate) blake3_hex: String,
    #[serde(default, alias = "size")]
    pub(crate) size_bytes: u64,
}

/// `sigil-top-latest.json` — same shape `flux_release_publish` writes, plus a
/// `targets` map so one channel serves both the Linux build and the Windows .exe.
/// Backward-compat: v0.3.x manifests used `blake3` and `size` keys, and
/// target triple keys like `x86_64-unknown-linux-musl`.
#[derive(Deserialize)]
pub(crate) struct Release {
    #[serde(default)]
    pub(crate) version: String,
    #[serde(default)]
    pub(crate) url: String,
    #[serde(default, alias = "blake3")]
    pub(crate) blake3_hex: String,
    #[serde(default, alias = "size")]
    pub(crate) size_bytes: u64,
    #[serde(default)]
    pub(crate) targets: std::collections::HashMap<String, Target>,
    /// v0.38: flux-rev `full:` source id of the published build (display/ledger;
    /// the binary gate stays the per-target BLAKE3).
    #[serde(default)]
    pub(crate) flux_rev: String,
}

#[cfg(test)]
mod release_parse_tests {
    use super::Release;
    #[test]
    fn full_manifest_with_aliases() {
        let j = r#"{"version":"7.6.0","url":"https://x/bin","blake3":"abc","size":1234,"flux_rev":"full:deadbeef","targets":{}}"#;
        let r: Release = serde_json::from_str(j).unwrap();
        assert_eq!(r.version, "7.6.0");
        assert_eq!(r.blake3_hex, "abc");        // via alias "blake3"
        assert_eq!(r.size_bytes, 1234);         // via alias "size"
        assert_eq!(r.flux_rev, "full:deadbeef");
    }
    #[test]
    fn partial_manifest_uses_defaults() {
        // Every field is #[serde(default)] → a manifest missing fields must still parse,
        // yielding a safe empty-url Release (the update path then just does nothing).
        let r: Release = serde_json::from_str(r#"{"version":"7.6.0"}"#).unwrap();
        assert_eq!(r.version, "7.6.0");
        assert_eq!(r.url, "");
        assert_eq!(r.size_bytes, 0);
    }
    #[test]
    fn malformed_json_errs_not_panics() {
        assert!(serde_json::from_str::<Release>("{not json").is_err());
        assert!(serde_json::from_str::<Release>("").is_err());
    }
}

/// Old manifest target triples that map to our short platform names.
/// v0.38 VARIANT PINNING: a GPU build reads ONLY its -gpu channel key — it must
/// never cross-grade itself to the CPU binary (or vice versa). No -gpu key in the
/// manifest -> a GPU build simply reports "no build for windows-x64-gpu" and stays.
const LEGACY_SELF_KEYS: &[&str] = if cfg!(all(windows, feature = "gpu")) {
    &["windows-x64-gpu"]
} else if cfg!(windows) {
    &["windows-x64", "x86_64-pc-windows-gnu"]
} else if cfg!(target_os = "macos") {
    &["macos-arm64", "aarch64-apple-darwin"]
} else if cfg!(feature = "gpu") {
    &["linux-x64-gpu"]
} else {
    &["linux-x64", "x86_64-unknown-linux-musl", "x86_64-unknown-linux-gnu"]
};

impl Release {
    /// The download for THIS platform: try the current `SELF_TARGET` key first,
    /// then legacy target triples, else fall back to top-level single-binary fields.
    pub(crate) fn for_self(&self) -> Target {
        for key in LEGACY_SELF_KEYS {
            if let Some(t) = self.targets.get(*key) {
                return t.clone();
            }
        }
        if cfg!(feature = "gpu") {
            // v0.38: the top-level single-binary fields are the CPU build — a GPU
            // build must NOT fall back to them (that silently downgrades GPU->CPU).
            return Target { url: String::new(), blake3_hex: String::new(), size_bytes: 0 };
        }
        Target {
            url: self.url.clone(),
            blake3_hex: self.blake3_hex.clone(),
            size_bytes: self.size_bytes,
        }
    }
}

/// v0.38: best-effort lifecycle telemetry -> the flux webhook bus (the cockpit
/// feed at :4178/api/mcp-webhook). OFF unless SIGIL_TOP_WEBHOOK is set — a use
/// install posts nothing anywhere by default. Fire-and-forget on a thread; a dead
/// bus can never slow or break the node.
pub(crate) fn flux_webhook(event: &str, detail: &str) {
    let Ok(url) = std::env::var("SIGIL_TOP_WEBHOOK") else { return };
    if url.trim().is_empty() { return; }
    let body = format!(
        r#"{{"source":"sigil-top","version":"{}","flux_rev":"{}","event":{},"detail":{}}}"#,
        VERSION, FLUX_REV,
        serde_json::to_string(event).unwrap_or_else(|_| "\"?\"".into()),
        serde_json::to_string(detail).unwrap_or_else(|_| "\"\"".into()),
    );
    std::thread::spawn(move || {
        let _ = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(4))
            .build()
            .and_then(|c| c.post(&url).header("content-type", "application/json").body(body).send());
    });
}

/// LANE-C: the pinned SIGIL release signing public key (ed25519, 32 B hex). The auto-update
/// REFUSES any manifest not signed by the matching secret — so a compromised release server / MITM
/// that serves a matching-blake3 (but attacker-built) binary can't push it: without the secret it
/// can't forge the manifest signature, and the blake3 the client trusts comes ONLY from a signed
/// manifest. The secret lives solely on the release host (`/root/.config/sigil/release-sign.seed`,
/// chmod 600). Ed25519 today; the sigil-oauth IssuerSigner abstraction lets a SQIsign-L5 PQ key
/// replace it without a wire change once flux-sqisign is in the cross-build.
const RELEASE_SIGN_PUBKEY_HEX: &str =
    "150fb84d4b2c83e6e81a27f629e60686acf8663be5ce73f46208cce4f5686402";

/// LANE-C: verify the release manifest is signed by [`RELEASE_SIGN_PUBKEY_HEX`]. The detached
/// signature is published at `<manifest>.sig` as 128-hex (ed25519 over the EXACT manifest bytes).
/// Returns `Err` (fail-closed → no update) on any missing/invalid signature. No runtime bypass:
/// this check always runs — there is no env var that skips it.
fn verify_manifest_sig(base: &str, manifest_body: &str) -> Result<(), String> {
    verify_signed_body(base, "sigil-top-latest.json", manifest_body)
}

/// Verify `body` against `<base>/<name>.sig` with the pinned release key. Generic so the
/// [A]I manifest (`sigil-ai-latest.json`) and the skills manifest (`sigil-skills-latest.json`)
/// share the auto-updater's ONE trust root — no second key, no second code path. Fail-closed.
pub(crate) fn verify_signed_body(base: &str, name: &str, body: &str) -> Result<(), String> {
    let pk: [u8; 32] = hex::decode(RELEASE_SIGN_PUBKEY_HEX).ok()
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| "pinned release key malformed".to_string())?;
    let bust = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs()).unwrap_or(0);
    let sig_url = format!("{base}/{name}.sig?t={bust}");
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(8))
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .user_agent(concat!("sigil-top/", env!("CARGO_PKG_VERSION")))
        .build().map_err(|e| format!("sig client init: {e}"))?;
    let sig_hex = client.get(&sig_url).send().and_then(|r| r.error_for_status()).and_then(|r| r.text())
        .map_err(|e| format!("{name}: no signature ({e}) — refusing"))?;
    let sig: [u8; 64] = hex::decode(sig_hex.trim()).ok()
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| "release-manifest signature malformed (want 128-hex ed25519)".to_string())?;
    if sigil_oauth::verify_sig(&pk, body.as_bytes(), &sig) {
        Ok(())
    } else {
        // Keep the literal "MANIFEST SIGNATURE INVALID" — self_update.rs + main.rs branch on it.
        Err(format!("MANIFEST SIGNATURE INVALID ({name}) — refusing (possible compromised release server)"))
    }
}

/// The release base currently selected by the channel failover.
pub(crate) fn active_base() -> &'static str {
    let i = ACTIVE_BASE.load(std::sync::atomic::Ordering::Relaxed);
    CHANNEL_BASES[i.min(CHANNEL_BASES.len().saturating_sub(1))]
}

/// Fetch `<base>/<name>` + its detached `.sig`, verify with the pinned release key, return
/// the body. The [A]I + skills manifests go through here. Fail-closed: no body without a
/// valid signature, ever.
pub(crate) fn fetch_signed_text(base: &str, name: &str) -> Result<String, String> {
    let bust = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs()).unwrap_or(0);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(12))
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .user_agent(concat!("sigil-top/", env!("CARGO_PKG_VERSION")))
        .build().map_err(|e| format!("client init: {e}"))?;
    let body = client.get(format!("{base}/{name}?t={bust}")).send()
        .and_then(|r| r.error_for_status()).and_then(|r| r.text())
        .map_err(|e| format!("{name}: {e}"))?;
    verify_signed_body(base, name, &body)?;
    Ok(body)
}

/// Fetch the live release manifest (short timeout — runs on the UI thread). `None`
/// if the channel is unreachable or malformed.
pub(crate) fn fetch_latest() -> Result<Release, String> {
    // 8s, not 3s: a cold Windows SChannel handshake to quillon.xyz can take >3s on
    // first contact (the same fetch_feed uses 6s and works). And we DON'T swallow the
    // error (was `.ok()?` → blind "unreachable") — surface the real reason instead.
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(8))
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .user_agent(concat!("sigil-top/", env!("CARGO_PKG_VERSION")))
        .build().map_err(|e| format!("client init: {e}"))?;
    // Read the body as text + parse explicitly (reqwest's .json() Display hides the
    // real serde error behind a generic "error decoding response body"). Two attempts
    // guard a transient truncated body on a flaky link; on failure we surface the
    // ACTUAL serde error + what arrived, so the toast says exactly what's wrong.
    // Cache-bust: "works on server, fails on client" usually means a stale/corrupt
    // cached manifest (q-flux / CDN / OS). A fresh ?t= each call bypasses every cache.
    let bust = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs()).unwrap_or(0);
    let mut last = String::from("no response");
    // v7.0.26: try every channel base — HTTPS first, then the plain-HTTP :8099 mirror
    // (filtered-network escape hatch). Signature is verified against the SAME base.
    for (bi, base) in CHANNEL_BASES.iter().enumerate() {
    let url = format!("{base}/sigil-top-latest.json?t={bust}");
    for _ in 0..2 {
        match client.get(&url).send().and_then(|r| r.error_for_status()) {
            Ok(resp) => match resp.text() {
                Ok(body) => {
                    // LANE-C: AUTHENTICATE the manifest before we trust a single field (incl. the
                    // blake3 we'd verify the binary against). A bad signature is fatal — fail closed.
                    if let Err(e) = verify_manifest_sig(base, &body) {
                        return Err(e);
                    }
                    ACTIVE_BASE.store(bi, std::sync::atomic::Ordering::Relaxed);
                    match serde_json::from_str::<Release>(&body) {
                        Ok(rel) => return Ok(rel),
                        Err(e) => last = format!("parse: {e} [{}B: {:?}]",
                            body.len(), body.chars().take(48).collect::<String>()),
                    }
                }
                Err(e) => last = format!("read body: {e}"),
            },
            Err(e) => {
                last = if e.is_timeout() { "timed out (>8s) — slow link".into() }
                       else if e.is_connect() { format!("connect failed: {e}") }
                       else { format!("request failed: {e}") };
            }
        }
    }
    } // CHANNEL_BASES — fall through to the next mirror (incl. the plain-HTTP :8099 one)
    Err(last)
}

/// v0.3.1: fetch the _sigil-tip DNS anchor via Cloudflare DoH, parse with
/// sigil-dns-anchor, and return a human-readable status. Composes with the
/// DNS-3 resolver-verifier lane once SQIsign verify is wired.
pub(crate) fn fetch_dns_anchor() -> String {
    const ANCHOR: &str = "_sigil-tip.sigilgraph.quillon.xyz";
    let url = format!("https://cloudflare-dns.com/dns-query?name={ANCHOR}&type=TXT");
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .build()
    {
        Ok(c) => c,
        Err(e) => return format!("✗ DNS anchor: client init failed: {e}"),
    };
    let resp = match client
        .get(&url)
        .header("accept", "application/dns-json")
        .send()
    {
        Ok(r) => r,
        Err(e) => return format!("✗ DNS anchor: DoH request failed: {e}"),
    };
    let body = match resp.text() {
        Ok(b) => b,
        Err(e) => return format!("✗ DNS anchor: read body: {e}"),
    };
    // Parse the DNS JSON response, extract the first TXT record
    let txt: String = match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(v) => v
            .get("Answer")
            .and_then(|a| a.get(0))
            .and_then(|r| r.get("data"))
            .and_then(|d| d.as_str())
            .map(|s| s.trim_matches('"').to_string())
            .unwrap_or_default(),
        Err(e) => return format!("✗ DNS anchor: JSON parse: {e}"),
    };
    if txt.is_empty() {
        return "✗ DNS anchor: _sigil-tip TXT record not published yet".into();
    }
    // Structural-validate with sigil-dns-anchor
    match sigil_dns_anchor::decode(&txt) {
        Ok(anchor) => format!(
            "✓ DNS anchor: {} @ height {} · key {}… (SQIsign sig present, verify pending)",
            anchor.record_type,
            anchor.height,
            &anchor.key_id[..8]
        ),
        Err(e) => format!("✗ DNS anchor: parse failed: {e}"),
    }
}
