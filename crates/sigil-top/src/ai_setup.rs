//! ai_setup — make the [A]I tab work OUT OF THE BOX.
//!
//! Before this module the tab could only *detect* a local ollama and, failing
//! that, print "install ollama, pull qwen3:8b" and leave the user to it. Now the
//! tab bootstraps itself: install ollama if missing, start it if stopped, pull
//! the right Qwen model if absent — streaming progress into the chat transcript.
//!
//! # Trust model ("no malware in my house")
//!
//! Nothing here runs a byte it did not verify against a *flux-signed* manifest:
//!
//! 1. `sigil-ai-latest.json` (+ detached `.sig`) is fetched from the release
//!    base and verified with the SAME pinned Ed25519 key the auto-updater refuses
//!    to boot without (`release::RELEASE_SIGN_PUBKEY_HEX`). One trust root.
//! 2. The manifest names an installer per platform with its **SHA-256** (Ollama's
//!    own published checksum, so a human can cross-check it against
//!    github.com/ollama/ollama/releases) and its exact **byte size**.
//! 3. The download is hashed *while streaming*; a size or hash mismatch deletes
//!    the file and refuses to execute it. There is no env var that skips this.
//! 4. Model pulls go through ollama's own HTTP API (`/api/pull`) — the tag comes
//!    from the signed manifest, never from a string the model produced.
//!
//! Everything blocking runs on a background thread; the TUI drains `SetupEvent`s.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Progress lines land in the chat transcript; `Done` carries the model to select.
pub(crate) enum SetupEvent {
    Line(String),
    Done { model: String },
    Fail(String),
}

/// One platform's installer, as published in the signed manifest.
#[derive(Deserialize, Clone, Debug)]
pub(crate) struct Installer {
    pub url: String,
    /// Hex SHA-256 of the installer bytes (Ollama's own published checksum).
    pub sha256: String,
    pub size_bytes: u64,
    /// Extra args for the installer (Windows: `/VERYSILENT /NORESTART`).
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Deserialize, Clone, Debug)]
pub(crate) struct AiManifest {
    #[serde(default)]
    pub ollama_version: String,
    pub default_model: String,
    #[serde(default)]
    pub fallback_model: Option<String>,
    #[serde(default)]
    pub installers: std::collections::BTreeMap<String, Installer>,
}

pub(crate) const MANIFEST_NAME: &str = "sigil-ai-latest.json";

fn emit(tx: &Sender<SetupEvent>, s: impl Into<String>) {
    let _ = tx.send(SetupEvent::Line(s.into()));
}

fn client(total_secs: u64) -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(total_secs))
        .connect_timeout(Duration::from_secs(5))
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .user_agent(concat!("sigil-top/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new())
}

/// Which installer key this build wants from the manifest.
pub(crate) fn target_key() -> &'static str {
    if cfg!(windows) {
        "windows-x64"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux-x64"
    }
}

/// Fetch + signature-verify the AI manifest. Fail-closed: an unsigned or
/// mis-signed manifest is an `Err`, never a default.
pub(crate) fn fetch_manifest(base: &str) -> Result<AiManifest, String> {
    let body = crate::release::fetch_signed_text(base, MANIFEST_NAME)?;
    parse_manifest(&body)
}

pub(crate) fn parse_manifest(body: &str) -> Result<AiManifest, String> {
    let m: AiManifest =
        serde_json::from_str(body).map_err(|e| format!("{MANIFEST_NAME} malformed: {e}"))?;
    if m.default_model.trim().is_empty() {
        return Err(format!("{MANIFEST_NAME}: default_model is empty"));
    }
    for (k, i) in &m.installers {
        if i.sha256.len() != 64 || !i.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("{MANIFEST_NAME}: installer {k} has a malformed sha256"));
        }
        if i.size_bytes == 0 {
            return Err(format!("{MANIFEST_NAME}: installer {k} has size 0"));
        }
        if !i.url.starts_with("https://") {
            return Err(format!("{MANIFEST_NAME}: installer {k} url is not https"));
        }
    }
    Ok(m)
}

// ── locating / starting ollama ─────────────────────────────────────────────

/// Where an installed ollama binary lives on this platform, if we can find one.
pub(crate) fn ollama_binary() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    #[cfg(windows)]
    {
        if let Ok(la) = std::env::var("LOCALAPPDATA") {
            candidates.push(PathBuf::from(&la).join("Programs").join("Ollama").join("ollama.exe"));
        }
        if let Ok(pf) = std::env::var("ProgramFiles") {
            candidates.push(PathBuf::from(&pf).join("Ollama").join("ollama.exe"));
        }
    }
    #[cfg(not(windows))]
    {
        candidates.push(ollama_home().join("bin").join("ollama"));
        candidates.push(PathBuf::from("/usr/local/bin/ollama"));
        candidates.push(PathBuf::from("/usr/bin/ollama"));
    }
    for c in candidates {
        if c.is_file() {
            return Some(c);
        }
    }
    // Last resort: is it on PATH?
    let name = if cfg!(windows) { "ollama.exe" } else { "ollama" };
    if std::process::Command::new(name).arg("--version").output().map(|o| o.status.success()).unwrap_or(false) {
        return Some(PathBuf::from(name));
    }
    None
}

/// Per-user dir we extract a Linux/macOS ollama into (never system-wide, never root).
#[cfg(not(windows))]
fn ollama_home() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".local").join("share")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("sigil-top").join("ollama")
}

/// Spawn `ollama serve` detached and wait (≤45 s) for :11434 to answer.
fn start_ollama(bin: &PathBuf, tx: &Sender<SetupEvent>) -> Result<(), String> {
    emit(tx, format!("  ▶ starting ollama ({})", bin.display()));
    let mut cmd = std::process::Command::new(bin);
    cmd.arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
    }
    // The child outlives us on purpose; dropping the handle does not kill it.
    let _child = cmd.spawn().map_err(|e| format!("could not start ollama: {e}"))?;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(45) {
        if crate::flux_moe::ollama_reachable() {
            emit(tx, "  ✓ ollama is up on :11434");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(750));
    }
    Err("ollama started but never answered on :11434 within 45 s".into())
}

// ── verified download + install ────────────────────────────────────────────

fn file_sha256(p: &PathBuf) -> Result<String, String> {
    let mut f = std::fs::File::open(p).map_err(|e| e.to_string())?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// Stream the installer to `dest`, hashing as it goes. Refuses (and deletes)
/// on ANY size or SHA-256 mismatch against the signed manifest.
fn download_verified(inst: &Installer, dest: &PathBuf, tx: &Sender<SetupEvent>) -> Result<(), String> {
    if let Ok(meta) = std::fs::metadata(dest) {
        if meta.len() == inst.size_bytes {
            emit(tx, "  · installer already on disk — re-verifying its SHA-256…");
            if file_sha256(dest)?.eq_ignore_ascii_case(&inst.sha256) {
                emit(tx, "  ✓ installer verified (cached)");
                return Ok(());
            }
            let _ = std::fs::remove_file(dest);
        }
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    emit(tx, format!("  ⬇ downloading {} ({} MB) — verifying SHA-256 as it streams", inst.url, inst.size_bytes >> 20));
    let mut resp = client(3 * 3600)
        .get(&inst.url)
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("download failed: {e}"))?;
    let tmp = dest.with_extension("part");
    let mut f = std::fs::File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut got: u64 = 0;
    let mut last_bucket: u64 = 0;
    loop {
        let n = resp.read(&mut buf).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            break;
        }
        got += n as u64;
        if got > inst.size_bytes {
            drop(f);
            let _ = std::fs::remove_file(&tmp);
            return Err("installer is LARGER than the signed manifest says — refusing".into());
        }
        f.write_all(&buf[..n]).map_err(|e| format!("write: {e}"))?;
        hasher.update(&buf[..n]);
        let bucket = got * 20 / inst.size_bytes.max(1); // every 5 %
        if bucket > last_bucket {
            last_bucket = bucket;
            emit(tx, format!("  ⬇ {} / {} MB  ({}%)", got >> 20, inst.size_bytes >> 20, bucket * 5));
        }
    }
    f.flush().map_err(|e| e.to_string())?;
    drop(f);
    if got != inst.size_bytes {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("size mismatch: got {got} bytes, manifest says {} — refusing", inst.size_bytes));
    }
    let hex_got = hex::encode(hasher.finalize());
    if !hex_got.eq_ignore_ascii_case(&inst.sha256) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "SHA-256 MISMATCH — refusing to run the installer\n    manifest {}\n    download {}\n  (download tampered, or the signed manifest is stale — nothing was executed)",
            inst.sha256, hex_got
        ));
    }
    std::fs::rename(&tmp, dest).map_err(|e| format!("rename: {e}"))?;
    emit(tx, "  ✓ SHA-256 and size match the flux-signed manifest");
    Ok(())
}

fn installer_dest(inst: &Installer) -> PathBuf {
    let name = inst.url.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("ollama-installer");
    std::env::temp_dir().join("sigil-top-ollama").join(name)
}

#[cfg(windows)]
fn run_installer(path: &PathBuf, args: &[String], tx: &Sender<SetupEvent>) -> Result<(), String> {
    emit(tx, "  ▶ running the verified Ollama installer silently — this takes a minute…");
    let status = std::process::Command::new(path)
        .args(args)
        .status()
        .map_err(|e| format!("could not launch installer: {e}"))?;
    if !status.success() {
        return Err(format!("installer exited with {status}"));
    }
    emit(tx, "  ✓ ollama installed");
    Ok(())
}

#[cfg(not(windows))]
fn run_installer(path: &PathBuf, _args: &[String], tx: &Sender<SetupEvent>) -> Result<(), String> {
    // Ollama ships Linux/macOS as a .tar.zst / .tgz. Extract into a PER-USER dir —
    // never `curl | sh`, never system-wide, never as root.
    let dir = ollama_home();
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    emit(tx, format!("  ▶ extracting into {}", dir.display()));
    let name = path.to_string_lossy();
    let mut cmd = std::process::Command::new("tar");
    if name.ends_with(".zst") {
        cmd.arg("--zstd");
    }
    cmd.arg("-xf").arg(path).arg("-C").arg(&dir);
    let status = cmd.status().map_err(|e| format!("tar not runnable: {e}"))?;
    if !status.success() {
        return Err(format!("tar exited with {status} (is zstd installed?)"));
    }
    emit(tx, "  ✓ ollama extracted");
    Ok(())
}

// ── model pull ─────────────────────────────────────────────────────────────

fn have_model(models: &[String], want: &str) -> bool {
    // ollama reports "qwen3:8b" for a pull of "qwen3:8b"; a bare "qwen3" pull shows as "qwen3:latest".
    let want_l = want.to_ascii_lowercase();
    models.iter().any(|m| {
        let m = m.to_ascii_lowercase();
        m == want_l || (!want_l.contains(':') && m == format!("{want_l}:latest"))
    })
}

/// Pull `model` through ollama's own API, streaming progress to the transcript.
pub(crate) fn pull_model(model: &str, tx: &Sender<SetupEvent>) -> Result<(), String> {
    emit(tx, format!("  ⬇ pulling model {model} (this is a multi-GB download; progress below)"));
    let url = format!("{}/api/pull", crate::flux_moe::ollama_base());
    let body = serde_json::json!({ "name": model, "stream": true });
    let resp = client(4 * 3600)
        .post(&url)
        .json(&body)
        .send()
        .map_err(|e| format!("pull request failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let detail = resp.text().unwrap_or_default();
        let short: String = detail.trim().chars().take(300).collect();
        return Err(format!("ollama pull {status}: {short}"));
    }
    // NDJSON: one {"status":..,"completed":..,"total":..} per line.
    let mut reader = std::io::BufReader::new(resp);
    let mut line = String::new();
    let mut last_bucket: u64 = u64::MAX;
    let mut last_status = String::new();
    use std::io::BufRead;
    loop {
        line.clear();
        let n = reader.read_line(&mut line).map_err(|e| format!("pull stream: {e}"))?;
        if n == 0 {
            break;
        }
        let v: serde_json::Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
            return Err(format!("ollama pull error: {err}"));
        }
        let st = v.get("status").and_then(|s| s.as_str()).unwrap_or("").to_string();
        let done = v.get("completed").and_then(|c| c.as_u64());
        let total = v.get("total").and_then(|t| t.as_u64());
        match (done, total) {
            (Some(d), Some(t)) if t > 0 => {
                let bucket = d * 20 / t;
                if bucket != last_bucket {
                    last_bucket = bucket;
                    emit(tx, format!("  ⬇ {}  {} / {} MB  ({}%)", st, d >> 20, t >> 20, bucket * 5));
                }
            }
            _ => {
                if st != last_status && !st.is_empty() {
                    last_status = st.clone();
                    emit(tx, format!("  · {st}"));
                }
            }
        }
        if st == "success" {
            emit(tx, format!("  ✓ model {model} ready"));
            return Ok(());
        }
    }
    // Stream ended without "success" — confirm via the tag list before failing.
    if have_model(&crate::flux_moe::list_models(), model) {
        emit(tx, format!("  ✓ model {model} ready"));
        Ok(())
    } else {
        Err("pull stream ended before ollama reported success".into())
    }
}

fn looks_like_memory_error(e: &str) -> bool {
    let l = e.to_ascii_lowercase();
    l.contains("memory") || l.contains("space") || l.contains("vram") || l.contains("ram")
}

// ── the bootstrap ──────────────────────────────────────────────────────────

/// The whole out-of-the-box flow. Runs on a background thread; every step
/// reports into `tx`. Ends with exactly one `Done` or `Fail`.
pub(crate) fn bootstrap(base: String, tx: Sender<SetupEvent>) {
    let r = bootstrap_inner(&base, &tx);
    let _ = match r {
        Ok(model) => tx.send(SetupEvent::Done { model }),
        Err(e) => tx.send(SetupEvent::Fail(e)),
    };
}

fn bootstrap_inner(base: &str, tx: &Sender<SetupEvent>) -> Result<String, String> {
    emit(tx, "🧠 flux-moe auto-setup — checking your local AI…");

    // 1. The signed manifest first: it tells us WHICH model is correct, and which
    //    installer is trusted. Fail-closed on signature; degrade only if ollama
    //    already has a model and the channel is merely unreachable.
    let manifest = match fetch_manifest(base) {
        Ok(m) => Some(m),
        Err(e) => {
            emit(tx, format!("  ⚠ AI manifest: {e}"));
            None
        }
    };

    // 2. ollama reachable? else installed-but-stopped? else install (verified).
    if !crate::flux_moe::ollama_reachable() {
        if let Some(bin) = ollama_binary() {
            emit(tx, "  · ollama is installed but not running");
            start_ollama(&bin, tx)?;
        } else {
            let m = manifest
                .as_ref()
                .ok_or_else(|| "ollama is not installed and the signed AI manifest could not be verified — refusing to fetch an installer from anywhere else".to_string())?;
            let key = target_key();
            let inst = m
                .installers
                .get(key)
                .ok_or_else(|| format!("signed manifest has no installer for {key}"))?;
            emit(tx, format!("  · ollama not found — installing v{} from the flux-signed manifest", m.ollama_version));
            let dest = installer_dest(inst);
            download_verified(inst, &dest, tx)?;
            run_installer(&dest, &inst.args, tx)?;
            // The Windows installer launches the app (which serves :11434); give it a
            // moment, then fall back to starting the binary ourselves.
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_secs(20) && !crate::flux_moe::ollama_reachable() {
                std::thread::sleep(Duration::from_millis(750));
            }
            if !crate::flux_moe::ollama_reachable() {
                let bin = ollama_binary().ok_or_else(|| "installer finished but no ollama binary was found".to_string())?;
                start_ollama(&bin, tx)?;
            }
        }
    } else {
        emit(tx, "  ✓ ollama is running");
    }

    // 3. The correct model.
    let models = crate::flux_moe::list_models();
    let want = match &manifest {
        Some(m) => m.default_model.clone(),
        None => {
            // No signed guidance. If the user already has a model, use it — but say so.
            return models.first().cloned().map(|m| {
                emit(tx, format!("  ⚠ using your existing model {m} (manifest unavailable, no pull attempted)"));
                m
            }).ok_or_else(|| "no local model and the signed manifest is unavailable".to_string());
        }
    };
    if have_model(&models, &want) {
        emit(tx, format!("  ✓ model {want} already pulled"));
        return Ok(want);
    }
    match pull_model(&want, tx) {
        Ok(()) => Ok(want),
        Err(e) => {
            let fb = manifest.as_ref().and_then(|m| m.fallback_model.clone());
            match fb {
                Some(fb) if looks_like_memory_error(&e) && fb != want => {
                    emit(tx, format!("  ⚠ {e}"));
                    emit(tx, format!("  · trying the smaller fallback {fb}"));
                    pull_model(&fb, tx)?;
                    Ok(fb)
                }
                _ => Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"{
      "ollama_version": "0.33.2",
      "default_model": "qwen3:8b",
      "fallback_model": "qwen3:4b",
      "installers": {
        "windows-x64": { "url": "https://github.com/ollama/ollama/releases/download/v0.33.2/OllamaSetup.exe",
                         "sha256": "5a91c1cf92480e28a84cd99e437219be719df5a50d5fa0fd5fe5b5c4a122f506",
                         "size_bytes": 1565765264, "args": ["/VERYSILENT", "/NORESTART"] }
      }
    }"#;

    #[test]
    fn manifest_parses_and_gates() {
        let m = parse_manifest(GOOD).expect("good manifest");
        assert_eq!(m.default_model, "qwen3:8b");
        assert_eq!(m.fallback_model.as_deref(), Some("qwen3:4b"));
        let w = m.installers.get("windows-x64").unwrap();
        assert_eq!(w.args, vec!["/VERYSILENT", "/NORESTART"]);
        assert_eq!(w.size_bytes, 1_565_765_264);
    }

    #[test]
    fn manifest_rejects_bad_sha_and_http() {
        let bad_sha = GOOD.replace("5a91c1cf", "zz91c1cf");
        assert!(parse_manifest(&bad_sha).is_err());
        let http = GOOD.replace("https://", "http://");
        assert!(parse_manifest(&http).unwrap_err().contains("not https"));
        let empty = GOOD.replace("\"qwen3:8b\"", "\"\"");
        assert!(parse_manifest(&empty).unwrap_err().contains("default_model"));
    }

    #[test]
    fn have_model_handles_latest_alias() {
        let m = vec!["qwen3:8b".to_string(), "llama3:latest".to_string()];
        assert!(have_model(&m, "qwen3:8b"));
        assert!(have_model(&m, "llama3"));
        assert!(!have_model(&m, "qwen3:4b"));
    }

    #[test]
    fn download_refuses_size_and_hash_mismatch() {
        // A tiny local "installer" served from a file:// is not fetchable by reqwest,
        // so exercise the post-download gate directly: hash of the wrong bytes ≠ manifest.
        let dir = std::env::temp_dir().join(format!("sigil-top-aitest-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("x.bin");
        std::fs::write(&p, b"hello").unwrap();
        let got = file_sha256(&p).unwrap();
        assert_eq!(got, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
        let inst = Installer { url: "https://x/y".into(), sha256: "00".repeat(32), size_bytes: 5, args: vec![] };
        // size matches, hash does not → cached-path re-verify must reject and delete.
        let (tx, _rx) = std::sync::mpsc::channel();
        let r = download_verified(&inst, &p, &tx);
        assert!(r.is_err(), "must not accept a hash mismatch");
        assert!(!p.exists(), "mismatched file must be deleted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn memory_error_heuristic() {
        assert!(looks_like_memory_error("model requires more system memory (8.2 GiB) than is available"));
        assert!(!looks_like_memory_error("pull model manifest: file does not exist"));
    }
}
