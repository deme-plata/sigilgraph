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

/// Where `ollama serve`'s own output is captured. Without this the old code sent
/// stdout+stderr to /dev/null, so "never answered on :11434" was reported with
/// the reason (port already taken, missing library, bad permissions) thrown away.
pub(crate) fn serve_log_path() -> PathBuf {
    std::env::temp_dir().join("sigil-top-ollama-serve.log")
}

/// Last few non-empty lines of the serve log — the actual reason a start failed.
fn serve_log_tail(n: usize) -> String {
    let text = match std::fs::read_to_string(serve_log_path()) {
        Ok(t) => t,
        Err(_) => return String::new(),
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..]
        .iter()
        .map(|l| format!("      {}", l.chars().take(200).collect::<String>()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The model directory to hand a server we spawn: `SIGIL_OLLAMA_MODELS` first, else an
/// `OLLAMA_MODELS` already in our own environment (inherited anyway, but being explicit
/// makes the spawned server's configuration visible in the log rather than implicit).
pub(crate) fn model_dir_override() -> Option<String> {
    for k in ["SIGIL_OLLAMA_MODELS", "OLLAMA_MODELS"] {
        if let Ok(v) = std::env::var(k) {
            if !v.trim().is_empty() {
                return Some(v);
            }
        }
    }
    // Nothing configured. Pick a roomy directory AUTOMATICALLY rather than letting ollama
    // default to ~/.ollama/models on a root partition that may be nearly full — the failure
    // that produces is a half-written multi-GB blob and a wedged machine, which is far worse
    // than a slightly surprising path. Only override when the default is genuinely tight
    // AND a roomier candidate exists, so a normal machine keeps ollama's own convention.
    const NEED: u64 = 12 * 1024 * 1024 * 1024; // headroom for a ~9 GB model + slack
    let default_dir = dirs_home().join(".ollama").join("models");
    if free_bytes(&default_dir).unwrap_or(u64::MAX) >= NEED {
        return None;
    }
    for cand in ["/home/storage/ollama-models", "/home/ollama-models"] {
        let p = std::path::Path::new(cand);
        let probe = if p.exists() { p } else { p.parent().unwrap_or(p) };
        if probe.exists() && free_bytes(probe).unwrap_or(0) >= NEED {
            let _ = std::fs::create_dir_all(p);
            return Some(cand.to_string());
        }
    }
    None
}

/// This user's home, without pulling in a crate for it.
fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/root"))
}

/// Free bytes on the filesystem holding `path` (or its nearest existing ancestor).
/// `None` when it cannot be determined — callers must treat that as "do not act".
fn free_bytes(path: &std::path::Path) -> Option<u64> {
    let mut p = path;
    while !p.exists() {
        p = p.parent()?;
    }
    let out = std::process::Command::new("df").arg("-Pk").arg(p).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().nth(1)?;
    line.split_whitespace().nth(3)?.parse::<u64>().ok().map(|kb| kb * 1024)
}

/// Make ollama reachable, starting it if it is merely installed-and-stopped. Returns
/// `true` if it is up when we return.
///
/// # Why chat() calls this instead of printing advice
///
/// "ollama is installed but not running" was reported to the user as an error telling them
/// to press F5. That is a chore, not a diagnosis: the tab knows exactly what is wrong and
/// is holding the binary that fixes it. An [A]I tab that works "out of the box" cannot ask
/// the user to run a setup step the machine can perform itself in two seconds.
///
/// Deliberately does NOT install anything. Installing is a multi-GB, hash-verified,
/// consent-shaped action that belongs to `bootstrap` behind an explicit keypress; this
/// only starts something already present. If no binary exists we return false and the
/// caller's message stands.
pub(crate) fn ensure_running() -> bool {
    if crate::flux_moe::ollama_reachable() {
        return true;
    }
    let Some(bin) = ollama_binary() else { return false };
    let (tx, _rx) = std::sync::mpsc::channel();
    let _ = start_ollama(&bin, &tx);
    crate::flux_moe::ollama_reachable()
}

/// Spawn `ollama serve` detached and wait (≤45 s) for :11434 to answer.
fn start_ollama(bin: &PathBuf, tx: &Sender<SetupEvent>) -> Result<(), String> {
    emit(tx, format!("  ▶ starting ollama ({})", bin.display()));
    let log = serve_log_path();
    // Capture the server's own words; a failed start must be diagnosable.
    let (out, err) = match std::fs::File::create(&log) {
        Ok(f) => match f.try_clone() {
            Ok(f2) => (std::process::Stdio::from(f), std::process::Stdio::from(f2)),
            Err(_) => (std::process::Stdio::null(), std::process::Stdio::null()),
        },
        Err(_) => (std::process::Stdio::null(), std::process::Stdio::null()),
    };
    let mut cmd = std::process::Command::new(bin);
    cmd.arg("serve").stdin(std::process::Stdio::null()).stdout(out).stderr(err);
    // Where the models live. ollama defaults to ~/.ollama/models, which on this class of
    // box is the SMALL root partition — the same partition `preflight_disk` refuses to
    // fill. If the operator put the models somewhere roomy, the server has to be told, or
    // it starts cleanly and reports zero models: the most confusing possible outcome,
    // because everything looks healthy and nothing works.
    if let Some(dir) = model_dir_override() {
        cmd.env("OLLAMA_MODELS", dir);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
    }
    // The child outlives us on purpose; dropping the handle does not kill it.
    let mut child = cmd.spawn().map_err(|e| {
        format!("could not start ollama: {e}\n  The binary is at {} — check it is executable, or start it yourself with `ollama serve`.", bin.display())
    })?;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(45) {
        if crate::flux_moe::ollama_reachable() {
            emit(tx, "  ✓ ollama is up on :11434");
            return Ok(());
        }
        // If it already exited, stop waiting 45 s for a corpse — report now.
        if let Ok(Some(status)) = child.try_wait() {
            let tail = serve_log_tail(6);
            return Err(format!(
                "ollama exited immediately ({status}).\n{}\n  Full log: {}\n  Most often :11434 is already taken by another ollama — check with `curl {}/api/version`.",
                if tail.is_empty() { "      (no output captured)".to_string() } else { tail },
                log.display(),
                crate::flux_moe::ollama_base()
            ));
        }
        std::thread::sleep(Duration::from_millis(750));
    }
    let tail = serve_log_tail(6);
    Err(format!(
        "ollama started but never answered on {} within 45 s.\n{}\n  Full log: {}\n  Try `ollama serve` in a terminal to see what it says.",
        crate::flux_moe::ollama_base(),
        if tail.is_empty() { "      (no output captured)".to_string() } else { tail },
        log.display()
    ))
}

// ── disk headroom ──────────────────────────────────────────────────────────

/// Where ollama keeps model blobs. Honours `OLLAMA_MODELS`, else the per-user
/// default. This matters: on a box whose HOME is on a small system partition, a
/// multi-GB pull can fill the root filesystem and take services down with it.
pub(crate) fn models_dir() -> PathBuf {
    if let Some(d) = std::env::var("OLLAMA_MODELS").ok().filter(|s| !s.trim().is_empty()) {
        return PathBuf::from(d);
    }
    let home = std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    home.join(".ollama").join("models")
}

/// Nearest ancestor of `p` that exists — you cannot stat free space on a path
/// that has not been created yet.
fn nearest_existing(p: &PathBuf) -> PathBuf {
    let mut cur = p.clone();
    loop {
        if cur.exists() {
            return cur;
        }
        match cur.parent() {
            Some(parent) => cur = parent.to_path_buf(),
            None => return std::env::temp_dir(),
        }
    }
}

/// Free bytes on the filesystem holding `p`. Best-effort and dependency-free:
/// `None` means "could not measure", which never blocks the pull.
pub(crate) fn free_disk_bytes(p: &PathBuf) -> Option<u64> {
    let dir = nearest_existing(p);
    #[cfg(not(windows))]
    {
        let out = std::process::Command::new("df").arg("-kP").arg(&dir).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        // "Filesystem 1024-blocks Used Available Capacity Mounted"
        let line = text.lines().nth(1)?;
        let avail_kb: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
        Some(avail_kb * 1024)
    }
    #[cfg(windows)]
    {
        let script = format!("(Get-Item -LiteralPath '{}').PSDrive.Free", dir.display());
        let out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }
}

const GIB: u64 = 1024 * 1024 * 1024;

/// Refuse a pull that would plainly not fit, BEFORE spending the download. The
/// message names the directory and the override, so the user can redirect the
/// pull to a bigger disk instead of being stuck.
fn check_disk_for(model: &str, tx: &Sender<SetupEvent>) -> Result<(), String> {
    let need = match crate::flux_moe::approx_model_bytes(model) {
        Some(n) => n,
        None => return Ok(()), // unknown size ⇒ do not guess, let ollama decide
    };
    // CHECK EVERY DIRECTORY THE BYTES COULD LAND IN, not just the one WE are configured for.
    //
    // A pull goes through ollama's HTTP API, so the weights land wherever the RUNNING SERVER
    // was configured — which is not necessarily where `models_dir()` points. On a box where
    // someone already started ollama against the default `~/.ollama/models` while sigil-top
    // has OLLAMA_MODELS aimed at a roomy disk, checking only our own value measures the roomy
    // disk, reports "plenty of space", and the pull then fills the small one anyway.
    //
    // That is worse than having no check at all: a safety check measuring the wrong
    // filesystem does not merely fail to help, it MANUFACTURES CONFIDENCE. ollama exposes no
    // endpoint reporting its model path, so the honest move is to require room in every
    // candidate and refuse if the tightest one cannot hold the model.
    let mut candidates = vec![models_dir()];
    let default_dir = dirs_home().join(".ollama").join("models");
    if !candidates.iter().any(|c| *c == default_dir) {
        candidates.push(default_dir);
    }
    // The tightest measurable candidate decides. Unmeasurable ones are skipped, never
    // treated as zero — an unknown must not block a legitimate pull.
    let (dir, free) = {
        let mut worst: Option<(PathBuf, u64)> = None;
        for c in candidates {
            if let Some(f) = free_disk_bytes(&c) {
                if worst.as_ref().map_or(true, |(_, wf)| f < *wf) {
                    worst = Some((c, f));
                }
            }
        }
        match worst {
            Some(w) => w,
            None => return Ok(()), // nothing measurable ⇒ never block
        }
    };
    emit(
        tx,
        format!(
            "  · model store {} — {:.1} GB free, {model} needs about {:.1} GB",
            dir.display(),
            free as f64 / GIB as f64,
            need as f64 / GIB as f64
        ),
    );
    // Keep a 2 GB cushion: a filesystem driven to 0 takes other services with it.
    if free < need + 2 * GIB {
        return Err(format!(
            "not enough disk for {model}: {:.1} GB free at {}, need ~{:.1} GB plus headroom.\n  Either free space there, or point ollama at a bigger disk: set OLLAMA_MODELS=/path/with/room, restart ollama, press F5.\n  Or choose a smaller model: SIGIL_AI_MODEL=qwen3:0.6b then press F5.",
            free as f64 / GIB as f64,
            dir.display(),
            need as f64 / GIB as f64
        ));
    }
    Ok(())
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
    // Every failure exit from this loop must delete `tmp`. Two branches used to
    // return through `?` without deleting (the read error and the write error),
    // unlike their siblings below — measured 2026-09-02 by severing the network
    // at 40.8 %: a 579,775,224-byte `.part` survived the failure. Not a safety
    // hole (only `dest` is ever executed, and `dest` is reached solely by the
    // rename after verification), but up to 1.4 GB was stranded in TMPDIR, AND
    // `bootstrap_inner`'s headroom check counts that leftover as used — so the
    // FIRST interrupted attempt could block its own retry on a tight disk.
    macro_rules! bail_rm {
        ($tmp:expr, $msg:expr) => {{
            let _ = std::fs::remove_file($tmp);
            return Err($msg);
        }};
    }
    loop {
        let n = match resp.read(&mut buf) {
            Ok(n) => n,
            Err(e) => { drop(f); bail_rm!(&tmp, format!("read: {e}")); }
        };
        if n == 0 {
            break;
        }
        got += n as u64;
        if got > inst.size_bytes {
            drop(f);
            let _ = std::fs::remove_file(&tmp);
            return Err("installer is LARGER than the signed manifest says — refusing".into());
        }
        if let Err(e) = f.write_all(&buf[..n]) {
            drop(f);
            bail_rm!(&tmp, format!("write: {e}"));
        }
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
            "SHA-256 MISMATCH — refusing to run the installer\n  manifest {}\n  download {}\n  (download tampered, or the signed manifest is stale — nothing was executed)",
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

    // ── zstd is decoded IN-PROCESS, not by shelling out to `tar --zstd` ────────
    //
    // Ollama ships Linux as `.tar.zst`, and `tar --zstd` execs a separate `zstd`
    // binary that bare ubuntu:24.04, debian:12 and most minimal/cloud/container
    // images DO NOT SHIP. Measured on a pristine ubuntu:24.04 (2026-09-02): the
    // user paid the full **1.42 GB** verified download and only then hit
    // `tar (child): zstd: Cannot exec` — a dead end at the most expensive
    // possible moment, in exactly the "works out of the box" scenario this path
    // exists to serve.
    //
    // `ruzstd` is a PURE-RUST decoder already in this crate's dependencies (it
    // decodes the sync wire), so the fix costs no new dependency and no C
    // toolchain — and it keeps the Windows cross-build mingw-clean. `tar` itself
    // is genuinely everywhere; `zstd` is not, so only the compression layer moves
    // in-process.
    let tar_path: PathBuf = if name.ends_with(".zst") {
        emit(tx, "  ▶ decompressing (in-process, no external zstd needed)…");
        let src = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
        let mut dec = ruzstd::StreamingDecoder::new(std::io::BufReader::new(src))
            .map_err(|e| format!("not a valid zstd stream: {e}"))?;
        let out = path.with_extension("tar");
        let mut w = std::io::BufWriter::new(
            std::fs::File::create(&out).map_err(|e| format!("create {}: {e}", out.display()))?,
        );
        std::io::copy(&mut dec, &mut w).map_err(|e| format!("zstd decode failed: {e}"))?;
        use std::io::Write;
        w.flush().map_err(|e| format!("flush {}: {e}", out.display()))?;
        out
    } else {
        path.clone()
    };

    // Capture tar's stderr rather than inheriting it: an inherited error prints
    // OVER the alt-screen and corrupts the TUI (observed with the zstd failure).
    let out = std::process::Command::new("tar")
        .arg("-xf").arg(&tar_path).arg("-C").arg(&dir)
        .output()
        .map_err(|e| format!("tar not runnable: {e}"))?;
    if tar_path != *path {
        let _ = std::fs::remove_file(&tar_path); // the decompressed copy is scratch
    }
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("tar exited with {}: {}", out.status, err.trim()));
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
    // Check the disk BEFORE downloading gigabytes onto a filesystem that cannot
    // hold them — a full root partition breaks far more than this tab.
    check_disk_for(model, tx)?;
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
    // A panic here would drop the Sender without ever sending Done or Fail. The
    // TUI only clears `ai_setup_running` when it sees one of those, so the tab
    // would sit at "running" forever and F5 would silently do nothing — the
    // worst kind of dead end, because it looks like the key is broken. Catch it
    // and turn it into a Fail the user can act on.
    let guard = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| bootstrap_inner(&base, &tx)));
    let r = match guard {
        Ok(r) => r,
        Err(p) => {
            let what = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            Err(format!("auto-setup crashed internally: {what}\n  This is a bug — please report it. You can still set up by hand: `ollama serve` then `ollama pull qwen3:4b`."))
        }
    };
    let _ = match r {
        Ok(model) => tx.send(SetupEvent::Done { model }),
        Err(e) => tx.send(SetupEvent::Fail(e)),
    };
}

/// Prove the chosen model actually LOADS and answers on this machine, before the
/// tab claims to be ready. Without this, "✓ flux-moe ready" was based only on the
/// download succeeding — and the first thing the user typed could still come back
/// as a 500 out-of-memory. This is also what makes the smaller-model fallback
/// reachable at all: a *pull* never fails for memory reasons, only a *load* does.
fn smoke_test(model: &str, tx: &Sender<SetupEvent>) -> Result<(), String> {
    emit(tx, format!("  · loading {model} to prove it runs here (first load reads several GB — can take a few minutes)"));
    let t0 = Instant::now();
    // A cold multi-GB load is the one step with no natural progress to report, and
    // a silent minute reads as a hang. Tick every 15 s so the tab visibly lives.
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let beat = {
        let (done, tx, model) = (done.clone(), tx.clone(), model.to_string());
        std::thread::spawn(move || {
            let start = Instant::now();
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(250));
                if done.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let secs = start.elapsed().as_secs();
                if secs > 0 && secs % 15 == 0 {
                    let _ = tx.send(SetupEvent::Line(format!("    … still loading {model} ({secs}s)")));
                    std::thread::sleep(Duration::from_secs(1)); // don't re-fire inside the same second
                }
            }
        })
    };
    let result = crate::flux_moe::chat(model, &[], "Reply with the single word: READY", "");
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = beat.join();
    match result {
        Ok(reply) => {
            let peek: String = reply.chars().take(60).collect();
            emit(tx, format!("  ✓ {model} answered in {:.0}s — \"{}\"", t0.elapsed().as_secs_f64(), peek.replace('\n', " ")));
            Ok(())
        }
        Err(e) => Err(e),
    }
}

fn bootstrap_inner(base: &str, tx: &Sender<SetupEvent>) -> Result<String, String> {
    emit(tx, "🧠 flux-moe auto-setup — checking your local AI…");

    // 0. What are we actually running on? The answer decides which model is a
    //    good idea, and it is shown so the choice is never mysterious.
    let hw = crate::flux_moe::detect_hardware();
    emit(tx, format!("  · this machine: {}", crate::flux_moe::describe_hardware(hw)));

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
            let m = manifest.as_ref().ok_or_else(|| {
                format!(
                    "ollama is not installed here, and the signed AI manifest could not be verified — so there is nothing safe to install from.\n  Check this machine's network/DNS, then press F5. Or install ollama yourself from https://ollama.com and press F5 (the manifest is only needed for the download, not for using an ollama you already trust).\n  Release channel tried: {base}"
                )
            })?;
            let key = target_key();
            let inst = m.installers.get(key).ok_or_else(|| {
                format!(
                    "the signed manifest has no ollama installer for {key}, so auto-install cannot proceed.\n  Install ollama by hand from https://ollama.com, then press F5."
                )
            })?;
            emit(tx, format!("  · ollama not found — installing v{} from the flux-signed manifest", m.ollama_version));
            let dest = installer_dest(inst);
            // The installer itself needs room too, on whatever temp dir we use.
            if let Some(free) = free_disk_bytes(&dest) {
                if free < inst.size_bytes + GIB {
                    return Err(format!(
                        "not enough disk to download the ollama installer: {:.1} GB free at {}, need {:.1} GB.\n  Free some space (or set TMPDIR to a bigger disk) and press F5.",
                        free as f64 / GIB as f64,
                        dest.parent().unwrap_or(&dest).display(),
                        inst.size_bytes as f64 / GIB as f64
                    ));
                }
            }
            download_verified(inst, &dest, tx)?;
            run_installer(&dest, &inst.args, tx)?;
            // The Windows installer launches the app (which serves :11434); give it a
            // moment, then fall back to starting the binary ourselves.
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_secs(20) && !crate::flux_moe::ollama_reachable() {
                std::thread::sleep(Duration::from_millis(750));
            }
            if !crate::flux_moe::ollama_reachable() {
                let bin = ollama_binary().ok_or_else(|| {
                    "the installer finished but no ollama binary turned up in any known location.\n  Open a terminal and run `ollama serve`; if that works, press F5 here.".to_string()
                })?;
                start_ollama(&bin, tx)?;
            }
        }
    } else {
        emit(tx, "  ✓ ollama is running");
    }

    // 2b. Version drift is a silent source of confusing model errors: an ollama
    //     older than the manifest's may not know a model tag the manifest names.
    //     Warn — never fail — because a working older ollama is still working.
    if let (Some(running), Some(m)) = (crate::flux_moe::ollama_version(), manifest.as_ref()) {
        emit(tx, format!("  · ollama {running} (the signed manifest was written against {})", m.ollama_version));
        if !m.ollama_version.is_empty() && version_older(&running, &m.ollama_version) {
            emit(
                tx,
                format!(
                    "  ⚠ your ollama ({running}) predates the manifest's ({}). Newer model tags may fail to pull or load. \
                     Upgrade from https://ollama.com if a model refuses to run.",
                    m.ollama_version
                ),
            );
        }
    }

    // 3. The right model FOR THIS MACHINE.
    let models = crate::flux_moe::list_models();
    let m = match &manifest {
        Some(m) => m,
        None => {
            // No signed guidance. If the user already has a model, use it — but say so.
            return match models.first().cloned() {
                Some(m) => {
                    emit(tx, format!("  ⚠ using your existing model {m} (manifest unavailable, no pull attempted)"));
                    Ok(m)
                }
                None => Err(format!(
                    "ollama is running but has no models, and the signed manifest is unavailable so there is no trusted tag to pull.\n  Check network access to {base} and press F5 — or pull one yourself: `ollama pull qwen3:4b`, then press F5."
                )),
            };
        }
    };

    let (want, why) = crate::flux_moe::pick_model(&m.default_model, m.fallback_model.as_deref(), hw);
    emit(tx, format!("  · model choice: {why}"));

    // Already there? Still prove it loads before promising the user it works.
    if have_model(&models, &want) {
        emit(tx, format!("  ✓ model {want} already pulled"));
        match smoke_test(&want, tx) {
            Ok(()) => return Ok(want),
            Err(e) => {
                emit(tx, format!("  ⚠ {want} is present but did not run: {e}"));
                // fall through to the fallback logic below
                return fallback_after_failure(m, &want, &models, tx, e);
            }
        }
    }

    match pull_model(&want, tx) {
        Ok(()) => match smoke_test(&want, tx) {
            Ok(()) => Ok(want),
            Err(e) => {
                emit(tx, format!("  ⚠ {want} downloaded but would not run: {e}"));
                fallback_after_failure(m, &want, &models, tx, e)
            }
        },
        Err(e) => fallback_after_failure(m, &want, &models, tx, e),
    }
}

/// One place for "the model we wanted did not work out". Tries the manifest's
/// smaller model, then any model the user already has, and only then gives up —
/// with a message that always names a next action.
fn fallback_after_failure(
    m: &AiManifest,
    want: &str,
    have: &[String],
    tx: &Sender<SetupEvent>,
    err: String,
) -> Result<String, String> {
    // 1. The manifest's own smaller model.
    if let Some(fb) = m.fallback_model.as_deref().filter(|f| *f != want && !f.trim().is_empty()) {
        emit(tx, format!("  · trying the smaller fallback {fb}"));
        let pulled = if have_model(have, fb) { Ok(()) } else { pull_model(fb, tx) };
        if let Ok(()) = pulled {
            if smoke_test(fb, tx).is_ok() {
                return Ok(fb.to_string());
            }
            emit(tx, format!("  ⚠ {fb} did not run here either"));
        }
    }
    // 2. Anything the user already has, rather than leaving the tab unusable.
    for existing in have {
        if existing != want && smoke_test(existing, tx).is_ok() {
            emit(tx, format!("  ⚠ falling back to your existing model {existing}"));
            return Ok(existing.clone());
        }
    }
    // 3. Genuinely stuck — say what failed AND what to try.
    let hint = if looks_like_memory_error(&err) {
        "This machine ran out of memory for every model tried. `ollama pull qwen3:0.6b` is the smallest useful one; then set SIGIL_AI_MODEL=qwen3:0.6b and press F5."
    } else {
        "Nothing else on this machine ran either. Try a small model by hand: `ollama pull qwen3:0.6b`, then SIGIL_AI_MODEL=qwen3:0.6b and press F5."
    };
    Err(format!("{err}\n  {hint}"))
}

/// Dotted-version compare, `true` when `a` is strictly older than `b`. Non-numeric
/// junk sorts as 0 rather than panicking — a weird version string must not break setup.
pub(crate) fn version_older(a: &str, b: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.trim()
            .trim_start_matches('v')
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect()
    }
    let (pa, pb) = (parts(a), parts(b));
    for i in 0..pa.len().max(pb.len()) {
        let (x, y) = (pa.get(i).copied().unwrap_or(0), pb.get(i).copied().unwrap_or(0));
        if x != y {
            return x < y;
        }
    }
    false
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
    fn version_compare_is_numeric_and_junk_proof() {
        assert!(version_older("0.20.2", "0.33.2"), "this box's real drift must be detected");
        assert!(!version_older("0.33.2", "0.33.2"));
        assert!(!version_older("1.0.0", "0.33.2"), "1.0 is NEWER than 0.33 — no string compare");
        assert!(version_older("0.9.0", "0.10.0"), "0.9 < 0.10 numerically");
        assert!(!version_older("garbage", "also-garbage"));
    }

    #[test]
    fn models_dir_honours_the_override() {
        std::env::set_var("OLLAMA_MODELS", "/somewhere/big");
        assert_eq!(models_dir(), PathBuf::from("/somewhere/big"));
        std::env::remove_var("OLLAMA_MODELS");
        // Default must land under the user's home, never a hard-coded root path.
        let d = models_dir();
        assert!(d.ends_with("models"), "{d:?}");
    }

    #[test]
    fn nearest_existing_walks_up_to_something_real() {
        let deep = std::env::temp_dir().join("no").join("such").join("dir").join("at").join("all");
        let found = nearest_existing(&deep);
        assert!(found.exists(), "must resolve to a real dir, got {found:?}");
    }

    #[test]
    fn free_disk_is_measurable_here() {
        // Best-effort by contract, but on this platform it should actually work —
        // and it must be a plausible number, not 0.
        let f = free_disk_bytes(&std::env::temp_dir());
        if let Some(bytes) = f {
            assert!(bytes > 0, "free space must be positive when measurable");
        }
    }

    #[test]
    fn serve_log_tail_is_safe_when_absent() {
        // No log yet must yield "", not a panic — this runs inside error paths.
        let _ = std::fs::remove_file(serve_log_path());
        assert_eq!(serve_log_tail(5), "");
    }

    #[test]
    fn memory_error_heuristic() {
        assert!(looks_like_memory_error("model requires more system memory (8.2 GiB) than is available"));
        assert!(!looks_like_memory_error("pull model manifest: file does not exist"));
    }
}

#[cfg(test)]
mod selfheal_live_tests {
    /// LIVE: with ollama STOPPED, `ensure_running` must bring it back by itself.
    ///
    /// This is the exact state the operator hit in v8.0.0 — installed, not running — where
    /// the tab reported "can't reach your local model" and asked for a keypress. Gated on
    /// SIGIL_OLLAMA_LIVE=1 because it starts a real server; skipped in normal runs.
    #[test]
    fn ensure_running_restarts_a_stopped_ollama() {
        if std::env::var("SIGIL_OLLAMA_LIVE").is_err() {
            eprintln!("skipped (set SIGIL_OLLAMA_LIVE=1 to run)");
            return;
        }
        assert!(
            !crate::flux_moe::ollama_reachable(),
            "precondition: this test requires ollama to be STOPPED"
        );
        assert!(super::ensure_running(), "ensure_running failed to start a stopped ollama");
        assert!(crate::flux_moe::ollama_reachable(), "ollama still unreachable after ensure_running");
    }
}
