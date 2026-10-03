//! flux_moe — the on-device AI brain for sigil-top's [A]I tab.
//!
//! Every user runs their OWN model, locally, via ollama (http://localhost:11434).
//! No central endpoint, no rented GPU, nothing leaves the machine. This is the
//! Rust-side counterpart to the browser AI console: it detects the local models,
//! lets the user pick one (the pick IS the effort dial — a bigger model on a
//! bigger GPU = more reasoning), and holds a chat.
//!
//! Tool-execution (the model actually firing wallet/mining actions) is layered on
//! top later; this module is the conversation + model-management core. It depends
//! only on `reqwest::blocking` + `serde_json` (both already in sigil-top), so it
//! compiles independently of the TUI wiring.
//!
//! # Measured facts this module is built around (Epsilon, 48-core CPU, NO GPU)
//!
//! Same one-sentence question, ollama 0.20.2, model already warm:
//!
//! | model | thinking | wall | decode | content |
//! |---|---|---|---|---|
//! | `qwen3:0.6b` | default (on) | 9.8 s | 23.1 tok/s | clean answer |
//! | `qwen3:4b` | on | 43.8 s | 7.0 tok/s | clean answer |
//! | `qwen3:4b` | forced OFF | 42.3 s | 7.0 tok/s | **monologue leaked into `content`** |
//!
//! Three consequences are baked in below.
//!
//! 1. **The chat timeout was too short.** It was 120 s. A warm `qwen3:4b` turn is
//!    43 s — but that is the manifest's *fallback*; the default is `qwen3:8b`, ~2x
//!    the parameters at roughly half the tok/s, plus a cold 5 GB load. The first
//!    such turn overruns 120 s, and the old code reported it as "can't reach your
//!    local model — is ollama running?", which is a wrong diagnosis of a slow one.
//! 2. **Thinking is left at ollama's own default.** Forcing `think:false` was
//!    measured to be a *regression*, not a speedup: no time saved, and the
//!    reasoning ends up in `content` instead of the separate `thinking` field.
//! 3. **An empty `content` is not an error.** A thinking model that spends its
//!    budget reasoning returns `content:""` with `done_reason:"length"` (measured
//!    on `qwen3:0.6b`). That used to surface as the dead-end `empty response from
//!    model`; it now reports what happened and what to do.

use std::sync::OnceLock;
use std::time::Duration;

/// Where the user's own ollama listens. Overridable for exotic setups.
pub(crate) const DEFAULT_OLLAMA: &str = "http://localhost:11434";

/// The host part of a URL, without scheme, userinfo or port. IPv6 literals keep
/// their brackets (`[::1]:11434` -> `[::1]`).
fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    if let Some(end) = authority.find(']') {
        return Some(authority[..=end].to_string()); // IPv6 literal
    }
    Some(authority.split(':').next()?.to_string())
}

/// Is this host on THIS machine? `localhost`, any `127.x.y.z`, or IPv6 `::1`.
fn is_loopback_host(host: &str) -> bool {
    let h = host.trim().trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    if h == "localhost" || h.ends_with(".localhost") || h == "::1" || h == "0:0:0:0:0:0:0:1" {
        return true;
    }
    match h.strip_prefix("127.") {
        Some(rest) => {
            let parts: Vec<&str> = rest.split('.').collect();
            parts.len() == 3 && parts.iter().all(|o| o.parse::<u8>().is_ok())
        }
        None => false,
    }
}

/// Decide the endpoint flux-moe will talk to, and any warning the user must see.
///
/// **Why this is not just `env::var().unwrap_or(default)` any more (2026-09-20).**
/// flux-moe's whole promise is in this module's first paragraph: *"No central
/// endpoint, no rented GPU, nothing leaves the machine."* A single environment
/// variable could silently break that promise — point `SIGIL_OLLAMA` at a remote
/// box and every word of every conversation (including anything the user pastes)
/// is shipped off-device, with no indication in the UI. That was the ONLY
/// data-exfiltration surface in the whole AI path (there is no tool execution, no
/// file write, no DB read, no network listener — audited 2026-09-20).
///
/// So a non-loopback endpoint is now REFUSED by default and the on-device default
/// is used instead. An operator who genuinely wants a remote model sets
/// `SIGIL_OLLAMA_ALLOW_REMOTE=1` and is warned every launch — an explicit, visible
/// choice rather than an invisible one. Pure function (config in, decision out) so
/// it is tested without touching the process-global env this module already warns
/// about racing — see `list_models_at`'s doc.
pub(crate) fn resolve_ollama_base(configured: Option<&str>, allow_remote: bool) -> (String, Option<String>) {
    let raw = match configured.map(str::trim).filter(|s| !s.is_empty()) {
        Some(r) => r,
        None => return (DEFAULT_OLLAMA.to_string(), None),
    };
    let host = host_of(raw).unwrap_or_default();
    if is_loopback_host(&host) {
        return (raw.to_string(), None);
    }
    if allow_remote {
        return (
            raw.to_string(),
            Some(format!(
                "⚠ SIGIL_OLLAMA points at the REMOTE host {host} and SIGIL_OLLAMA_ALLOW_REMOTE=1 is set —                  every word of your AI conversation leaves this machine. Unset it to keep flux-moe on-device."
            )),
        );
    }
    (
        DEFAULT_OLLAMA.to_string(),
        Some(format!(
            "⚠ SIGIL_OLLAMA points at the REMOTE host {host} — REFUSED, using {DEFAULT_OLLAMA} instead.              flux-moe is on-device by design: nothing leaves your machine.              Set SIGIL_OLLAMA_ALLOW_REMOTE=1 as well if you really intend to send conversations off-box."
        )),
    )
}

pub(crate) fn ollama_base() -> String {
    let configured = std::env::var("SIGIL_OLLAMA").ok();
    let allow_remote = matches!(std::env::var("SIGIL_OLLAMA_ALLOW_REMOTE").as_deref(), Ok("1"));
    let (base, warning) = resolve_ollama_base(configured.as_deref(), allow_remote);
    if let Some(w) = warning {
        // Once per process: this is called on every chat turn and every model poll.
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| crate::tlog!("{w}"));
    }
    base
}

fn client_with_timeout(total_secs: u64) -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(total_secs))
        .connect_timeout(Duration::from_secs(2))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new())
}

/// Wall-clock budget for ONE chat turn. The old hard-coded 120 s was measured to
/// be *shorter* than a single `qwen3:4b` turn on a CPU-only box, so a working
/// setup looked broken. Default generously; `SIGIL_OLLAMA_TIMEOUT` (seconds)
/// overrides for people who want to fail fast.
pub(crate) fn chat_timeout_secs() -> u64 {
    std::env::var("SIGIL_OLLAMA_TIMEOUT")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(900)
}

/// Whether to override ollama's own thinking default. `None` (no env var) means
/// SEND NOTHING and let ollama decide — which is what the measurements support:
/// forcing `think:false` on `qwen3:4b` saved no time (42.3 s vs 43.8 s) and made
/// the output worse, because the reasoning then lands in `content` instead of the
/// separate `thinking` field. `SIGIL_OLLAMA_THINK=0|1` forces it either way for
/// users who have measured their own box and disagree.
fn think_override() -> Option<bool> {
    match std::env::var("SIGIL_OLLAMA_THINK").ok().as_deref().map(str::trim) {
        Some("1") | Some("true") | Some("yes") => Some(true),
        Some("0") | Some("false") | Some("no") => Some(false),
        _ => None,
    }
}

/// Chat may legitimately take a while (the model is generating), so it gets a
/// generous timeout — and it runs OFF the UI thread.
pub(crate) fn client() -> reqwest::blocking::Client { client_with_timeout(chat_timeout_secs()) }

/// The models the local ollama has pulled, newest first. Empty ⇒ ollama not
/// running (or no models). The caller shows the "install a model" hint then.
pub(crate) fn list_models() -> Vec<String> {
    list_models_at(&ollama_base())
}

/// The pure half — same split, same reason, as [`pick_from_ladder_with`] and
/// [`pick_model_with`]: the host is an argument, so a test does not have to set a
/// PROCESS-GLOBAL env var that races every other test in the binary.
///
/// `no_model_when_ollama_down` did exactly that: `set_var("SIGIL_OLLAMA", <closed
/// port>)`, assert empty, `remove_var`. Two tests either side of it also touch that
/// variable, and THIS BOX RUNS A REAL OLLAMA on :11434 — so whenever the removal
/// landed inside this test's window, `list_models()` reached the live daemon,
/// returned real models, and the assertion failed. It passed in isolation and
/// failed in the suite, which is the signature of a schedule-dependent flake, and
/// it surfaced on 2026-09-06 only because adding tests elsewhere moved the timing.
/// Nothing about the production path changes: `list_models()` still reads the env.
pub(crate) fn list_models_at(base: &str) -> Vec<String> {
    let url = format!("{}/api/tags", base);
    // Detection runs on the UI thread (the [5] keypress), so cap it hard: a hung ollama
    // that accepts the connection but never answers must NOT freeze the dashboard.
    let resp = match client_with_timeout(4).get(&url).send() {
        Ok(r) if r.status().is_success() => r,
        _ => return Vec::new(),
    };
    let json: serde_json::Value = match resp.json() {
        Ok(j) => j,
        Err(_) => return Vec::new(),
    };
    let arr = match json.get("models").and_then(|m| m.as_array()) {
        Some(a) => a,
        None => return Vec::new(),
    };
    // The doc above promised "newest first" but nothing ever sorted — ollama
    // returns them in its own order. The TUI picks `first()` as the active model,
    // so an unsorted list means the active model was effectively arbitrary. Sort
    // by ollama's own `modified_at` (RFC-3339, so lexicographic order IS time
    // order) so the most recently pulled model is the one that gets used.
    let mut pairs: Vec<(String, String)> = arr
        .iter()
        // ollama reports both "name" and "model"; take whichever is present.
        .filter_map(|m| {
            let name = m.get("name").or_else(|| m.get("model")).and_then(|n| n.as_str())?;
            let when = m.get("modified_at").and_then(|t| t.as_str()).unwrap_or("");
            Some((name.to_string(), when.to_string()))
        })
        .collect();
    pairs.sort_by(|a, b| b.1.cmp(&a.1));
    pairs.into_iter().map(|(n, _)| n).collect()
}

/// True if a local model is reachable — cheap probe for the tab header.
pub(crate) fn available() -> bool {
    !list_models().is_empty()
}

/// Is the ollama SERVER up at all (even with zero models pulled)? 2 s cap.
/// Auto-setup uses this to tell "not installed / not running" from "no model".
pub(crate) fn ollama_reachable() -> bool {
    let url = format!("{}/api/version", ollama_base());
    client_with_timeout(2).get(&url).send().map(|r| r.status().is_success()).unwrap_or(false)
}

/// The running server's version string (`0.33.2`), or None if it isn't up.
/// Used by auto-setup to warn when the installed ollama predates the one the
/// signed manifest was written against — a silent source of weird model errors.
pub(crate) fn ollama_version() -> Option<String> {
    let url = format!("{}/api/version", ollama_base());
    let r = client_with_timeout(2).get(&url).send().ok()?;
    if !r.status().is_success() {
        return None;
    }
    let j: serde_json::Value = r.json().ok()?;
    j.get("version").and_then(|v| v.as_str()).map(String::from)
}

// ── hardware → which model is actually a good idea here ────────────────────

/// What this machine can realistically run. Every field is best-effort: `None`
/// means "could not measure", which is NEVER treated as "too small" — an
/// unmeasurable box gets the manifest default and a note, not a downgrade.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Hardware {
    /// Largest single GPU's memory, bytes. `None` = no GPU found / not measurable.
    pub vram_bytes: Option<u64>,
    /// Total system RAM, bytes.
    pub ram_bytes: Option<u64>,
    /// Human GPU name for the transcript.
    pub gpu_name: Option<String>,
}

const GIB: u64 = 1024 * 1024 * 1024;

#[cfg(target_os = "linux")]
fn detect_ram() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb: u64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn detect_ram() -> Option<u64> {
    let out = std::process::Command::new("sysctl").args(["-n", "hw.memsize"]).output().ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

#[cfg(windows)]
fn detect_ram() -> Option<u64> {
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", "(Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn detect_ram() -> Option<u64> { None }

/// NVIDIA first (the common case). Apple Silicon is handled separately: its GPU
/// shares system RAM, so unified memory IS the VRAM budget.
fn detect_gpu() -> (Option<u64>, Option<String>) {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        // Unified memory: the GPU can address (most of) system RAM.
        if let Some(ram) = detect_ram() {
            return (Some(ram / 2), Some("Apple Silicon (unified memory)".to_string()));
        }
    }
    let out = match std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=memory.total,name", "--format=csv,noheader,nounits"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return (None, None),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut best: Option<(u64, String)> = None;
    for line in text.lines() {
        let mut parts = line.splitn(2, ',');
        let mib: u64 = match parts.next().map(|s| s.trim().parse::<u64>()) {
            Some(Ok(v)) => v,
            _ => continue,
        };
        let name = parts.next().unwrap_or("GPU").trim().to_string();
        if best.as_ref().map(|(b, _)| mib > *b).unwrap_or(true) {
            best = Some((mib, name));
        }
    }
    match best {
        Some((mib, name)) => (Some(mib * 1024 * 1024), Some(name)),
        None => (None, None),
    }
}

/// Measured once per process — `nvidia-smi` spawns a subprocess, so never call
/// this from the UI thread on a hot path.
pub(crate) fn detect_hardware() -> &'static Hardware {
    static HW: OnceLock<Hardware> = OnceLock::new();
    HW.get_or_init(|| {
        let (vram_bytes, gpu_name) = detect_gpu();
        Hardware { vram_bytes, ram_bytes: detect_ram(), gpu_name }
    })
}

/// One line for the transcript, so the user can see what we measured and judge
/// the model choice for themselves.
pub(crate) fn describe_hardware(hw: &Hardware) -> String {
    let ram = hw.ram_bytes.map(|b| format!("{:.0} GB RAM", b as f64 / GIB as f64)).unwrap_or_else(|| "RAM unknown".into());
    match (&hw.gpu_name, hw.vram_bytes) {
        (Some(n), Some(v)) => format!("{n}, {:.0} GB VRAM, {ram}", v as f64 / GIB as f64),
        _ => format!("no GPU detected (CPU inference), {ram}"),
    }
}

/// Parameter count in billions, parsed out of an ollama tag (`qwen3:8b` → 8.0,
/// `qwen3:0.6b` → 0.6). `None` when the tag carries no size (e.g. `llama3:latest`).
pub(crate) fn tag_params_b(tag: &str) -> Option<f64> {
    let after = tag.rsplit(':').next()?.to_ascii_lowercase();
    // Take the leading number followed by `b`, e.g. "8b", "0.6b", "14b-instruct".
    let num: String = after.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    if num.is_empty() {
        return None;
    }
    let rest = &after[num.len()..];
    if !rest.starts_with('b') {
        return None;
    }
    num.parse().ok()
}

/// Rough weights-on-disk/in-memory cost of a tag, bytes. Calibrated on the
/// MEASURED Q4_K_M sizes ollama actually pulled: qwen3:0.6b = 522 MB for 0.75 B
/// params, qwen3:4b ≈ 2.5 GB ⇒ ~0.65 GB per billion params, plus ~1 GB of KV
/// cache and runtime overhead.
pub(crate) fn approx_model_bytes(tag: &str) -> Option<u64> {
    tag_params_b(tag).map(|b| ((b * 0.65 + 1.0) * GIB as f64) as u64)
}

/// Choose between the signed manifest's `default_model` and its `fallback_model`
/// using MEASURED hardware. Returns `(tag, human reason)` — the reason is always
/// shown, so the choice is never mysterious.
///
/// The rule, and why:
/// * `SIGIL_AI_MODEL` wins outright — the user's explicit choice is never overridden.
/// * A GPU that fits the big model → big model. This is the case it was written for.
/// * A GPU too small → the smaller model, because spilling to host RAM is slower
///   than just running the smaller one.
/// * **No GPU → it depends on the CPU box, and that is the point.** This bullet
///   used to read "no GPU → the smaller model", on the reasoning that 8b would be
///   memory-bandwidth bound and land near 1.5 min per turn. Re-measured 2026-09-02
///   on 48 cores / 62 GB and that turned out to be wrong: `qwen3:8b` answered in
///   44.7 s and `qwen3:4b` in ~43.8 s — the same wall clock — so downgrading cost
///   capability and bought nothing. The distinguishing measurement is not the
///   absence of a GPU, it is RAM headroom and core count. A 2-core 4 GB VPS is the
///   case the fallback exists for. See the no-GPU arm below for the exact gate.
/// * Anything unmeasurable → the manifest default, never a silent downgrade.
pub(crate) fn pick_model(default_model: &str, fallback_model: Option<&str>, hw: &Hardware) -> (String, String) {
    if let Some(forced) = std::env::var("SIGIL_AI_MODEL").ok().filter(|s| !s.trim().is_empty()) {
        let forced = forced.trim().to_string();
        return (forced.clone(), format!("SIGIL_AI_MODEL={forced} (your explicit choice)"));
    }
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    pick_model_with(default_model, fallback_model, hw, cores)
}

/// The pure half — same split, and for the same reason, as
/// [`pick_from_ladder_with`]: every input is an argument, nothing is probed.
///
/// This existed as one function until 2026-09-06 and the CPU arm read the core
/// count straight off the host with `available_parallelism()`. That made the
/// choice depend on WHICH MACHINE ASKED, which is fine in production and fatal in
/// a test: `cpu_only_box_gets_the_smaller_model` passed on a small runner and
/// failed on this 48-core box, for the same source. A test whose verdict depends
/// on the runner is worse than a failing one — it goes green somewhere and hides.
pub(crate) fn pick_model_with(
    default_model: &str,
    fallback_model: Option<&str>,
    hw: &Hardware,
    cores: usize,
) -> (String, String) {
    let fb = match fallback_model.filter(|f| !f.trim().is_empty() && *f != default_model) {
        Some(f) => f,
        // Nothing to fall back TO — say so rather than pretending we chose.
        None => {
            return (
                default_model.to_string(),
                format!("{default_model} (the manifest names no smaller fallback)"),
            )
        }
    };
    let need = approx_model_bytes(default_model);
    match (hw.vram_bytes, need) {
        // GPU big enough for the default → use it, that's what it's for.
        (Some(v), Some(n)) if v >= n => (
            default_model.to_string(),
            format!("{default_model} — {:.0} GB VRAM fits it (~{:.1} GB needed)", v as f64 / GIB as f64, n as f64 / GIB as f64),
        ),
        // GPU present but too small → the smaller model beats spilling to host RAM.
        (Some(v), Some(n)) => (
            fb.to_string(),
            format!("{fb} — {:.0} GB VRAM is under the ~{:.1} GB {default_model} needs", v as f64 / GIB as f64, n as f64 / GIB as f64),
        ),
        // GPU present, size of the default unknown → trust the manifest.
        (Some(_), None) => (default_model.to_string(), format!("{default_model} (GPU present; model size not derivable from the tag)")),
        // NO GPU — but "no GPU" is not the same as "not capable", and treating them as
        // identical silently denied the operator the model the manifest actually names.
        //
        // Measured on this class of box (48 cores, 62 GB RAM, no GPU, 2026-09-02): qwen3:8b
        // answered in 44.7 s with a clean `done_reason: stop`, and qwen3:4b in ~43.8 s at
        // 7.0 tok/s. The 8b model is not meaningfully slower here — it is roughly the same
        // wall-clock — so downgrading bought nothing and cost capability. A 2-core, 4 GB VPS
        // is the case the fallback exists for, and RAM plus core count is what distinguishes
        // the two; the absence of a GPU does not.
        //
        // So: with enough RAM to hold the model with real headroom AND enough cores for CPU
        // inference to stay interactive, honour the manifest's default. Otherwise fall back,
        // and SAY WHICH MEASUREMENT decided it — a user who disagrees can then point at the
        // number rather than guess. `SIGIL_AI_MODEL` still wins over all of this.
        (None, need_opt) => {
            const MIN_CORES: usize = 8;
            // 2.5x the weights: activations, KV cache and the OS all need room, and a model
            // that swaps is far worse than a smaller model that does not.
            let ram_ok = match (hw.ram_bytes, need_opt) {
                (Some(ram), Some(need)) => ram >= need.saturating_mul(5) / 2,
                // Unmeasurable either way ⇒ do NOT gamble on the bigger model.
                _ => false,
            };
            if ram_ok && cores >= MIN_CORES {
                let ram_gb = hw.ram_bytes.unwrap_or(0) as f64 / GIB as f64;
                (
                    default_model.to_string(),
                    format!(
                        "{default_model} — no GPU, but {cores} cores and {ram_gb:.0} GB RAM run it                          comfortably on CPU (measured: qwen3:8b ~45 s/answer on 48 cores)"
                    ),
                )
            } else {
                let why = if !ram_ok { "not enough RAM headroom".to_string() } else { format!("only {cores} cores") };
                (
                    fb.to_string(),
                    format!("{fb} — no GPU and {why}, so the smaller model keeps the tab interactive (set SIGIL_AI_MODEL={default_model} to override)"),
                )
            }
        }
    }
}

/// The system prompt that turns a plain chat model into "flux-moe": SIGIL-aware,
/// honest, and deferring live numbers to the node instead of inventing them.
///
/// Rewritten after a measured failure: asked about "Gordon Klein emission with
/// technical analysis", the model answered *"There is no Gordon Klein emission
/// in SIGIL — that concept doesn't exist"* and then listed its own feature
/// inventory. Every sentence was true and the reply was useless. Two faults,
/// both addressed below:
///
/// 1. **It stonewalled on an unfamiliar name** instead of answering the question
///    underneath it. Not recognising a name is not a reason to refuse; SIGIL's
///    emission schedule is a real thing the user can be told about.
/// 2. **It talked about itself** — a bulleted list of subsystems is the model
///    describing its own scope, which is machine self-talk, not an answer.
///
/// So: answer the question that was meant, in the user's own language, and keep
/// the honesty rule exactly where it was — live numbers still come from the node.
pub(crate) fn system_prompt() -> &'static str {
    "You are flux-moe, the on-device AI inside the SIGIL sigil-top node. You know the SIGIL \
     whitepaper well — the block header and its commitments, dual-lane mining (Φ proof-of-work \
     × Ω proof-of-time), state agreement at one chokepoint, shielded-by-default privacy, the \
     light-client fold, the 21M cap and its halving schedule, the DEX, the Nation welfare \
     stipend, the Polygon bridge, and running the node.\n\n\
     HOW TO SPEAK — this matters as much as being right:\n\
     • Answer the person, not the topic. Open with a plain-language sentence that actually \
       answers what they asked. No preamble.\n\
     • Start from something they already hold in their hands — a picture, an everyday \
       comparison — and put the technical name AFTER the picture has landed, never before. \
       'A note is like a sealed envelope with an amount inside' comes first; 'commitment' second.\n\
     • Write short paragraphs of ordinary prose. Do not answer with a bulleted inventory of \
       SIGIL's features — that is you describing yourself, and it tells the user nothing.\n\
     • Never say 'as an AI', never narrate what you are about to do, never list what you can \
       and cannot help with. Just help.\n\
     • Define any abbreviation the first time you use it, every time.\n\
     • If a number matters, show where it comes from, so they can check it.\n\n\
     WHEN YOU DO NOT RECOGNISE SOMETHING: do not stop at 'that does not exist'. Say briefly \
     that the name is not one you know in SIGIL, then answer the question underneath it and \
     ask one short question to confirm you understood. A person asking about 'emission' wants \
     to know how new SIGIL is created — tell them that.\n\n\
     HONESTY — unchanged and absolute: for anything that needs a LIVE number or moves money \
     (balance, height, supply, hashrate, price, a send), say which screen or command shows it \
     rather than guessing. Never invent balances, prices, hashrates or amounts. Separate what \
     you are sure of from what you are unsure of, and say plainly when you do not know."
}

/// Older ollama builds inline the reasoning block in `content` instead of
/// splitting it into the `thinking` field. Strip it so the transcript shows the
/// answer, not the monologue. (Measured: ollama 0.20.2 splits it correctly —
/// this is defensive, for other versions.)
pub(crate) fn strip_think_blocks(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("<think>") {
        out.push_str(&rest[..start]);
        match rest[start..].find("</think>") {
            Some(end) => rest = &rest[start + end + "</think>".len()..],
            // Unterminated block: everything after it is monologue, drop it.
            None => return out.trim().to_string(),
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// Turn a transport error into words that name the ACTUAL problem. The old code
/// mapped every failure to "is ollama running?", which is a wrong diagnosis for
/// the most common case on a CPU box (the model is simply still generating).
fn transport_error_message(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        format!(
            "the model did not finish within {}s.\n\
             It is probably still generating — big models on a CPU are slow (measured on 48 cores, no GPU: qwen3:4b answers in ~43 s at 7 tok/s; qwen3:8b is roughly twice that, plus a cold multi-GB load).\n\
             Fixes, cheapest first: ask a shorter question · switch to a smaller model with SIGIL_AI_MODEL=qwen3:0.6b · raise the budget with SIGIL_OLLAMA_TIMEOUT=1800 · or run on a machine with a GPU.",
            chat_timeout_secs()
        )
    } else if e.is_connect() {
        format!(
            "cannot reach ollama at {}.\n\
             It is not running (or is listening elsewhere). Press F5 to let flux-moe start it, or run `ollama serve` yourself. Point elsewhere with SIGIL_OLLAMA=http://host:11434.",
            ollama_base()
        )
    } else {
        format!("talking to ollama at {} failed: {e}", ollama_base())
    }
}

/// Turn a non-200 from ollama into an actionable line. `status` + `body` are what
/// ollama gave us; the point is that the user always learns the NEXT ACTION.
fn http_error_message(status: reqwest::StatusCode, body: &str, model: &str) -> String {
    let detail: String = body.trim().chars().take(300).collect();
    let lower = detail.to_ascii_lowercase();
    if status == reqwest::StatusCode::NOT_FOUND || lower.contains("not found") {
        return format!(
            "the model `{model}` is not pulled on this machine.\n\
             Press F5 to let flux-moe pull it from the signed manifest, or run `ollama pull {model}` yourself."
        );
    }
    if lower.contains("memory") || lower.contains("vram") || lower.contains("system memory") {
        return format!(
            "`{model}` does not fit in this machine's memory.\n\
             ollama said: {detail}\n\
             Pick a smaller model — SIGIL_AI_MODEL=qwen3:4b (or qwen3:0.6b) then press F5."
        );
    }
    if detail.is_empty() {
        format!("ollama returned {status} with no detail. Check `ollama serve` output; press F5 to re-run setup.")
    } else {
        format!("ollama returned {status}: {detail}\n(press F5 to re-run auto-setup)")
    }
}

/// One turn of chat against the chosen local model. `history` is prior
/// (role, content) pairs; `user` is the new message. Blocking — the caller runs
/// it off the UI thread. Returns the assistant's reply or a human error string.
/// `extra_system` is appended to the system prompt — the flux-signed skills block
/// (see `skills::context_block`); empty when no skills are loaded.
pub(crate) fn chat(model: &str, history: &[(String, String)], user: &str, extra_system: &str) -> Result<String, String> {
    let system = if extra_system.is_empty() { system_prompt().to_string() } else { format!("{}{}", system_prompt(), extra_system) };
    let mut messages = vec![serde_json::json!({"role":"system","content":system})];
    for (role, content) in history {
        messages.push(serde_json::json!({"role": role, "content": content}));
    }
    messages.push(serde_json::json!({"role":"user","content":user}));
    // SIGIL Veil: only when the model is OFF this machine (SIGIL_OLLAMA_ALLOW_REMOTE).
    let route = current_ai_route();
    if let AiRoute::Refuse(msg) = &route { return Err(msg.clone()); }
    let veil = if ollama_is_local() { None } else { Some(veil_outbound(&mut messages, &route_label(&ollama_base(), &route), Some(&veil_ledger_path()))?) };

    // No `think` key unless the user explicitly asked for one: measured, forcing it
    // off is a regression and forcing it on is already ollama's default. `think` is
    // also only understood by newer ollama; a model that rejects it is retried
    // without it just below.
    let mut body = serde_json::json!({
        "model": model,
        "messages": messages,
        "stream": false,
    });
    if let (Some(t), Some(o)) = (think_override(), body.as_object_mut()) {
        o.insert("think".to_string(), serde_json::Value::Bool(t));
    }
    let body = body;
    let url = format!("{}/api/chat", ollama_base());
    let http = chat_client(&route)?;
    let send = |b: &serde_json::Value| http.post(&url).json(b).send();

    // SELF-HEAL (v8.0.1): "ollama is installed but not running" is not an error worth
    // reporting — it is a two-second fix the tab can perform itself, holding the very
    // binary that fixes it. Telling the user to press F5 turned a solvable state into a
    // chore, and it is the single most common way this tab appears broken: the server was
    // simply stopped (a reboot, a `pkill`, a session that tidied up after itself).
    //
    // Only a CONNECT failure triggers this, and only once. A timeout means the model is
    // generating and restarting the server would destroy real work; a 404 means the model
    // is missing and no restart helps. Installing is NOT attempted here — that is a
    // multi-GB, hash-verified, consent-shaped action that stays behind the explicit [F5]
    // keypress in `ai_setup::bootstrap`.
    let resp = match send(&body) {
        Ok(r) => r,
        Err(e) if e.is_connect() && crate::ai_setup::ensure_running() => {
            send(&body).map_err(|e2| transport_error_message(&e2))?
        }
        Err(e) => return Err(transport_error_message(&e)),
    };
    let status = resp.status();
    let resp = if !status.is_success() {
        let detail = resp.text().unwrap_or_default();
        // A model that does not support the `think` switch must not become a
        // dead end — drop the field and try once more before reporting.
        // SELF-HEAL (v8.0.1): the model simply is not pulled yet. The tab knows the model
        // it wants and ollama can fetch it — so fetch it, rather than telling the user to
        // press F5. This is the other half of "works out of the box": a fresh machine has
        // a running server and no weights, and that state must resolve itself.
        //
        // Bounded on purpose: ONE attempt, and `pull_model`'s own `check_disk_for` still
        // refuses when the target filesystem cannot hold the weights. A pull is minutes of
        // multi-GB download, so it happens once and then never again for that model.
        let lower_once = detail.to_ascii_lowercase();
        if status == reqwest::StatusCode::NOT_FOUND || lower_once.contains("not found") {
            let (tx, _rx) = std::sync::mpsc::channel();
            if crate::ai_setup::pull_model(model, &tx).is_ok() {
                let r2 = send(&body).map_err(|e| transport_error_message(&e))?;
                if r2.status().is_success() {
                    r2
                } else {
                    let s2 = r2.status();
                    let d2 = r2.text().unwrap_or_default();
                    return Err(http_error_message(s2, &d2, model));
                }
            } else {
                return Err(http_error_message(status, &detail, model));
            }
        } else if detail.to_ascii_lowercase().contains("think") {
            let mut retry = body.clone();
            if let Some(o) = retry.as_object_mut() {
                o.remove("think");
            }
            let r2 = send(&retry).map_err(|e| transport_error_message(&e))?;
            if !r2.status().is_success() {
                let s2 = r2.status();
                let d2 = r2.text().unwrap_or_default();
                return Err(http_error_message(s2, &d2, model));
            }
            r2
        } else {
            return Err(http_error_message(status, &detail, model));
        }
    } else {
        resp
    };

    let json: serde_json::Value = resp.json().map_err(|e| format!("bad model response: {e}"))?;
    let msg = json.get("message");
    let content = msg
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(strip_think_blocks)
        .unwrap_or_default();
    let content = match &veil { Some(m) => sigil_veil::rehydrate(&content, m), None => content };
    if !content.is_empty() {
        return Ok(content);
    }

    // ── the old dead end ──
    // A thinking model that spends its whole budget reasoning returns
    // content:"" with done_reason:"length" (MEASURED on qwen3:0.6b). The old
    // code answered "empty response from model" and stopped. Now: show what it
    // DID produce, and say exactly what to do about it.
    let thinking = msg
        .and_then(|m| m.get("thinking"))
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim();
    let reason = json.get("done_reason").and_then(|d| d.as_str()).unwrap_or("");
    let advice = if reason == "length" {
        "It hit its output budget while thinking and never got to an answer. Ask a narrower question, or try SIGIL_OLLAMA_THINK=0 so it answers directly."
    } else {
        "The model returned no text. Ask again, or press F5 to re-check the setup."
    };
    if thinking.is_empty() {
        Err(format!("`{model}` produced no answer. {advice}"))
    } else {
        let peek: String = thinking.chars().take(400).collect();
        let peek = match &veil { Some(m) => sigil_veil::rehydrate(&peek, m), None => peek };
        Ok(format!("(no final answer — {advice})\n\nIts reasoning so far:\n{peek}"))
    }
}

/// One event of a streamed reply. Sent over a channel from the worker thread to the UI.
#[derive(Debug, Clone)]
pub(crate) enum StreamEv {
    /// A piece of the model's hidden reasoning (qwen3 thinks before it answers).
    Thinking(String),
    /// A piece of the visible answer.
    Token(String),
    /// Finished. `eval_count`/`eval_ns` are ollama's own measurement of the answer.
    Done { eval_count: u64, eval_ns: u64, prompt_count: u64, reason: String },
    /// The model asked for tools (raw ollama `tool_calls` entries).
    ToolCalls(Vec<serde_json::Value>),
    /// A read tool ran; one line for the transcript ("🔧 balance a3f1…  → 12.5 SIGIL").
    ToolRan(String),
    /// The model wants an ACTION (create wallet, start mining): the user must type `yes`.
    Confirm { name: String, args: serde_json::Value, summary: String },
    /// Streaming was not possible; this is the whole reply from the non-streaming path
    /// (which carries all the self-heal logic), or an error already formatted for the user.
    Whole(String),
}

/// Pure: parse one NDJSON line of ollama's streamed `/api/chat` into events.
pub(crate) fn parse_stream_line(line: &str) -> Vec<StreamEv> {
    let mut out = Vec::new();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else { return out };
    if let Some(m) = v.get("message") {
        if let Some(t) = m.get("thinking").and_then(|t| t.as_str()) {
            if !t.is_empty() { out.push(StreamEv::Thinking(t.to_string())); }
        }
        if let Some(c) = m.get("content").and_then(|c| c.as_str()) {
            if !c.is_empty() { out.push(StreamEv::Token(c.to_string())); }
        }
        if let Some(tc) = m.get("tool_calls").and_then(|t| t.as_array()) {
            if !tc.is_empty() { out.push(StreamEv::ToolCalls(tc.clone())); }
        }
    }
    if v.get("done").and_then(|d| d.as_bool()) == Some(true) {
        let n = |k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        out.push(StreamEv::Done {
            eval_count: n("eval_count"),
            eval_ns: n("eval_duration"),
            prompt_count: n("prompt_eval_count"),
            reason: v.get("done_reason").and_then(|d| d.as_str()).unwrap_or("").to_string(),
        });
    }
    out
}

/// Streamed chat: words appear as the model produces them. Any failure before the first
/// byte (connect error, 404, a model that rejects `think`) falls back to [`chat`], whose
/// self-heal paths (start ollama, pull the model, retry without `think`) stay the single
/// source of truth for recovery — streaming adds speed of feedback, not a second
/// error-handling path.
pub(crate) fn chat_stream(model: &str, history: &[(String, String)], user: &str, extra_system: &str,
                          tx: &std::sync::mpsc::Sender<StreamEv>) {
    use std::io::BufRead;
    let system = if extra_system.is_empty() { system_prompt().to_string() } else { format!("{}{}", system_prompt(), extra_system) };
    let mut messages = vec![serde_json::json!({"role":"system","content":system})];
    for (role, content) in history {
        messages.push(serde_json::json!({"role": role, "content": content}));
    }
    messages.push(serde_json::json!({"role":"user","content":user}));
    let route = current_ai_route();
    if let AiRoute::Refuse(msg) = &route { let _ = tx.send(StreamEv::Whole(msg.clone())); return; }
    let veil = if ollama_is_local() {
        None
    } else {
        match veil_outbound(&mut messages, &route_label(&ollama_base(), &route), Some(&veil_ledger_path())) {
            Ok(m) => Some(m),
            Err(e) => { let _ = tx.send(StreamEv::Whole(e)); return; }
        }
    };
    let mut body = serde_json::json!({ "model": model, "messages": messages, "stream": true });
    if let (Some(t), Some(o)) = (think_override(), body.as_object_mut()) {
        o.insert("think".to_string(), serde_json::Value::Bool(t));
    }
    let url = format!("{}/api/chat", ollama_base());
    let fallback = |tx: &std::sync::mpsc::Sender<StreamEv>| {
        let r = match chat(model, history, user, extra_system) { Ok(r) => r, Err(e) => format!("⚠ {e}") };
        let _ = tx.send(StreamEv::Whole(r));
    };
    let http = match chat_client(&route) { Ok(c) => c, Err(e) => { let _ = tx.send(StreamEv::Whole(e)); return; } };
    let resp = match http.post(&url).json(&body).send() {
        Ok(r) if r.status().is_success() => r,
        _ => return fallback(tx),
    };
    let mut got_any = false;
    let reader = std::io::BufReader::new(resp);
    let mut answer_rh = sigil_veil::StreamRehydrator::new();
    let mut think_rh = sigil_veil::StreamRehydrator::new();
    for line in reader.lines() {
        let Ok(line) = line else { break };
        for ev in parse_stream_line(&line) {
            got_any = true;
            let done = matches!(ev, StreamEv::Done { .. });
            // Off-box model: placeholders become real values again, here, locally.
            let ev = match (&veil, ev) {
                (None, ev) => ev,
                (Some(m), StreamEv::Token(t)) => StreamEv::Token(answer_rh.push(&t, m)),
                (Some(m), StreamEv::Thinking(t)) => StreamEv::Thinking(think_rh.push(&t, m)),
                (Some(m), StreamEv::ToolCalls(mut calls)) => {
                    for c in calls.iter_mut() { rehydrate_json(c, m); }
                    StreamEv::ToolCalls(calls)
                }
                (Some(m), ev @ StreamEv::Done { .. }) => {
                    let tail = answer_rh.finish(m);
                    if !tail.is_empty() && tx.send(StreamEv::Token(tail)).is_err() { return; }
                    ev
                }
                (Some(_), ev) => ev,
            };
            if let StreamEv::Token(t) | StreamEv::Thinking(t) = &ev { if t.is_empty() { continue; } }
            if tx.send(ev).is_err() { return; } // UI went away
            if done { return; }
        }
    }
    if !got_any { return fallback(tx); }
    // Stream ended without a `done` line (connection dropped mid-answer): close it honestly.
    let _ = tx.send(StreamEv::Done { eval_count: 0, eval_ns: 0, prompt_count: 0, reason: "interrupted".into() });
}

// ── SIGIL Veil: the egress gate on the one path that leaves the machine ──────
//
// The model runs on loopback by default and then nothing below runs. Only with
// SIGIL_OLLAMA_ALLOW_REMOTE=1 and an off-box host does a request leave — and then:
// a recovery phrase or labelled key in anything the user/tools wrote refuses the
// request outright; every other identifier becomes a placeholder before the bytes
// leave and is restored in the answer here; every send lands in a local ledger.

// ── SIGIL Veil L3: the provider must not learn WHERE you are ─────────────────
//
// zkAPI's answer to "the provider sees your IP" is "use Tor yourself". Here it is
// the default whenever Tor is reachable: an off-box AI request rides a local Tor
// SOCKS proxy (socks5h — DNS resolves inside Tor too). Without Tor the request goes
// direct and the privacy view says so; SIGIL_AI_REQUIRE_TOR=1 refuses instead.

/// How an off-box AI request travels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AiRoute {
    /// Model on this device: nothing leaves, nothing to hide.
    Local,
    /// Through this SOCKS proxy (Tor).
    Proxied(String),
    /// Straight to the provider, who sees this machine's IP.
    Direct,
    /// Strict mode with no proxy: do not send.
    Refuse(String),
}

/// Pure: validate a configured proxy URL. Only SOCKS (where DNS can stay inside
/// Tor) — an http:// proxy would leak the hostname and isn't what this is for.
pub(crate) fn validate_ai_proxy(v: &str) -> Result<String, String> {
    let v = v.trim();
    let lower = v.to_ascii_lowercase();
    if lower.starts_with("socks5h://") || lower.starts_with("socks5://") {
        Ok(v.to_string())
    } else {
        Err(format!("SIGIL_AI_PROXY must be socks5h://host:port (got {v:?}) — e.g. socks5h://127.0.0.1:9050 for Tor"))
    }
}

/// Pure: the routing decision.
pub(crate) fn decide_ai_route(local: bool, proxy: Option<String>, require_tor: bool, host: &str) -> AiRoute {
    if local {
        return AiRoute::Local;
    }
    match proxy {
        Some(p) => AiRoute::Proxied(p),
        None if require_tor => AiRoute::Refuse(format!(
            "🕶 Not sent: SIGIL_AI_REQUIRE_TOR=1 and no Tor proxy is reachable, so {host} would see this machine's IP address. \
             Start Tor (port 9050), or set SIGIL_AI_PROXY=socks5h://host:port."
        )),
        None => AiRoute::Direct,
    }
}

/// The configured or auto-detected Tor SOCKS proxy: SIGIL_AI_PROXY if set (and
/// valid), else a SOCKS port answering on loopback (9050 tor, 9150 Tor Browser).
pub(crate) fn ai_proxy() -> Result<Option<String>, String> {
    if let Ok(v) = std::env::var("SIGIL_AI_PROXY") {
        if !v.trim().is_empty() {
            return validate_ai_proxy(&v).map(Some);
        }
    }
    for port in [9050u16, 9150] {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(150)).is_ok() {
            return Ok(Some(format!("socks5h://127.0.0.1:{port}")));
        }
    }
    Ok(None)
}

pub(crate) fn current_ai_route() -> AiRoute {
    let require = matches!(std::env::var("SIGIL_AI_REQUIRE_TOR").as_deref(), Ok("1") | Ok("true") | Ok("yes"));
    let local = ollama_is_local();
    let proxy = if local { None } else {
        match ai_proxy() {
            Ok(p) => p,
            Err(e) => return AiRoute::Refuse(format!("🕶 Not sent: {e}")),
        }
    };
    decide_ai_route(local, proxy, require, &ollama_base())
}

/// HTTP client for one chat turn along `route`. Tor circuits take longer to
/// build than a loopback connect, so the connect budget grows with it.
/// FAIL CLOSED: a proxied route whose proxy cannot be configured is an error,
/// never a silent direct connection — the privacy view would otherwise claim Tor
/// while the provider saw this machine's IP.
pub(crate) fn chat_client(route: &AiRoute) -> Result<reqwest::blocking::Client, String> {
    let b = reqwest::blocking::Client::builder().timeout(Duration::from_secs(chat_timeout_secs()));
    let b = match route {
        AiRoute::Proxied(p) => {
            let px = reqwest::Proxy::all(p.as_str())
                .map_err(|e| format!("🕶 Not sent: the Tor proxy {p} could not be configured ({e}), and Veil does not fall back to a direct connection."))?;
            b.connect_timeout(Duration::from_secs(45)).proxy(px)
        }
        // never let a system HTTP(S)_PROXY env var silently reroute a local model
        AiRoute::Local => b.connect_timeout(Duration::from_secs(2)).no_proxy(),
        _ => b.connect_timeout(Duration::from_secs(2)),
    };
    b.build().map_err(|e| format!("🕶 Not sent: could not build the HTTP client ({e})."))
}

/// What the ledger records as the destination: host plus how it travelled.
pub(crate) fn route_label(base: &str, route: &AiRoute) -> String {
    match route {
        AiRoute::Proxied(p) => format!("{base} via Tor ({p})"),
        AiRoute::Direct => format!("{base} (direct: provider sees this IP)"),
        _ => base.to_string(),
    }
}

// ── SIGIL Veil L4: the provider must not recognise YOU in the question ──────
//
// Hiding identifiers is not enough: zkAPI's own write-up says sessions "may be re-linked
// by the inference provider if the same personal details, writing style, reused
// conversation history, or project documents are presented", and names local models as
// the mitigation without building it. Here: when the model is off-box and a small model
// runs ON this device, the latest message (with local context) is rewritten into ONE
// self-contained, de-identified question, and only that leaves — no history.

pub(crate) const REWRITE_SYSTEM: &str = "You prepare a question for an outside expert who must not learn who is asking. \
Rewrite the user's LATEST request as ONE self-contained question. Keep every fact the expert needs to answer \
correctly: numbers, amounts, error messages, code, technical terms, placeholders like [HEX_1]. Remove names of \
people, places, employers, dates and personal anecdotes, and anything else that identifies the asker. Use plain, \
neutral wording, not the user's own style. Output ONLY the rewritten question.";

/// The on-device model that rewrites outbound questions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rewriter {
    pub base: String,
    pub model: String,
}

/// `SIGIL_AI_REWRITE`: "0|off|false|no" disables, "require" refuses to send when a
/// rewrite cannot be made, anything else = auto (use one when available).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RewriteMode { Off, Auto, Require }

pub(crate) fn rewrite_mode_from(v: Option<&str>) -> RewriteMode {
    match v.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("0") | Some("off") | Some("false") | Some("no") => RewriteMode::Off,
        Some("require") | Some("required") | Some("strict") => RewriteMode::Require,
        _ => RewriteMode::Auto,
    }
}

/// Pure: the rewriter endpoint. It sees the RAW question, so it must be on this
/// device — a non-loopback URL is refused (None), never used.
pub(crate) fn rewriter_base_from(v: Option<&str>) -> Option<String> {
    let base = v.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("http://localhost:11434").trim_end_matches('/').to_string();
    host_of(&base).filter(|h| is_loopback_host(h)).map(|_| base)
}

/// Pure: which installed model rewrites. Small and fast first — this is a
/// paraphrase, not an answer.
pub(crate) fn pick_rewriter(models: &[String], forced: Option<&str>) -> Option<String> {
    if let Some(f) = forced.map(str::trim).filter(|f| !f.is_empty()) {
        return Some(f.to_string());
    }
    // Measured on Epsilon 2026-10-03 (CPU, load ~60, think off): qwen3:0.6b rewrote in
    // 25–32 s; qwen3:4b took 133–148 s AND ignored think:false, answering with its
    // reasoning (which quoted the original). Small first.
    for want in ["qwen3:1.7b", "qwen3:0.6b", "llama3.2:3b", "qwen3:4b", "qwen3:8b"] {
        if let Some(m) = models.iter().find(|m| m.as_str() == want || m.starts_with(&format!("{want}-"))) {
            return Some(m.clone());
        }
    }
    models.first().cloned()
}

/// Pure: what the rewriter is asked — the last few turns as private context.
pub(crate) fn rewrite_request_messages(history: &[(String, String)], user: &str) -> Vec<serde_json::Value> {
    let ctx: Vec<String> = history.iter().rev().filter(|(r, _)| r == "user" || r == "assistant").take(6)
        .collect::<Vec<_>>().into_iter().rev()
        .map(|(r, c)| format!("{r}: {}", c.chars().take(600).collect::<String>())).collect();
    let mut prompt = String::new();
    if !ctx.is_empty() {
        prompt.push_str("Conversation so far (private context, do not repeat personal details):\n");
        prompt.push_str(&ctx.join("\n"));
        prompt.push_str("\n\n");
    }
    prompt.push_str("Latest request:\n");
    prompt.push_str(user);
    vec![
        serde_json::json!({"role":"system","content":REWRITE_SYSTEM}),
        serde_json::json!({"role":"user","content":prompt}),
    ]
}

/// Pure: accept a rewrite only if it looks like one question, not an essay, an
/// answer or a refusal.
pub(crate) fn clean_rewrite(raw: &str, original: &str) -> Option<String> {
    let mut t = strip_think_blocks(raw).trim().to_string();
    for label in ["Rewritten question:", "Rewritten request:", "Question:"] {
        if let Some(rest) = t.strip_prefix(label) { t = rest.trim().to_string(); }
    }
    let t = t.trim_matches(|c| c == '"' || c == '“' || c == '”').trim().to_string();
    let limit = (original.chars().count() * 4).max(600);
    if t.is_empty() || t.chars().count() > limit {
        return None;
    }
    let lower = t.to_lowercase();
    if lower.starts_with("i can't") || lower.starts_with("i cannot") || lower.starts_with("sorry") {
        return None;
    }
    // A model that "thinks out loud" instead of rewriting (measured: qwen3:4b with
    // think:false) talks ABOUT the task — and quotes the original while doing it.
    // Whole-word match: a substring test rejected "wallet's" for containing "let's".
    let words = format!(" {} ", lower.chars().map(|c| if c.is_alphanumeric() || c == '\'' { c } else { ' ' })
        .collect::<String>().split_whitespace().collect::<Vec<_>>().join(" "));
    for meta in ["the user", "rewrite", "rewriting", "rewritten", "original", "let's", "steps", "self contained",
                 "output only", "translating", "translation", "here is", "here's"] {
        if words.contains(&format!(" {meta} ")) {
            return None;
        }
    }
    // Any 6-word run copied verbatim from the original means it was not rewritten.
    let norm = |x: &str| x.to_lowercase().split_whitespace().map(|w| w.trim_matches(|c: char| !c.is_alphanumeric())).filter(|w| !w.is_empty()).map(String::from).collect::<Vec<_>>();
    let (o, r) = (norm(original), norm(&t));
    if o.len() >= 6 {
        let rj = format!(" {} ", r.join(" "));
        if o.windows(6).any(|w| rj.contains(&format!(" {} ", w.join(" ")))) {
            return None;
        }
    }
    Some(t)
}

/// Proper-noun-like words of the original: Capitalised, not sentence-initial, not an
/// ACRONYM (SIGIL, HTTP), at least 3 letters. Names, places, employers.
pub(crate) fn proper_nouns(original: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut sentence_start = true;
    for raw in original.split_whitespace() {
        let w: String = raw.trim_matches(|c: char| !c.is_alphanumeric()).to_string();
        let first_upper = w.chars().next().map(|c| c.is_uppercase()).unwrap_or(false);
        let all_caps = w.chars().filter(|c| c.is_alphabetic()).all(|c| c.is_uppercase());
        if !sentence_start && first_upper && !all_caps && w.chars().count() >= 3 && w.chars().all(|c| c.is_alphabetic()) {
            if !out.contains(&w) { out.push(w.clone()); }
        }
        sentence_start = raw.ends_with('.') || raw.ends_with('!') || raw.ends_with('?') || raw.ends_with(':');
    }
    out
}

/// Deterministic backstop for a small rewriter that misses some (measured: qwen3:0.6b
/// kept "Aalborg Sygehus"): any proper noun of the original that survived becomes [NAME].
pub(crate) fn scrub_carried_names(rewrite: &str, original: &str) -> (String, usize) {
    let mut out = rewrite.to_string();
    let mut n = 0;
    for name in proper_nouns(original) {
        if out.contains(&name) {
            out = out.replace(&name, "[NAME]");
            n += 1;
        }
    }
    (out, n)
}

/// Ask the on-device rewriter. None on any failure (caller decides by mode).
pub(crate) fn rewrite_with(rw: &Rewriter, history: &[(String, String)], user: &str) -> Option<String> {
    // think:false + a short budget: a paraphrase needs no chain of thought, and on a CPU
    // box thinking alone blew a 240 s budget (measured, qwen3:4b).
    let body = serde_json::json!({"model": rw.model, "messages": rewrite_request_messages(history, user),
        "stream": false, "think": false, "options": {"num_predict": 160, "temperature": 0.2}});
    let http = reqwest::blocking::Client::builder().timeout(Duration::from_secs(90))
        .connect_timeout(Duration::from_secs(2)).no_proxy().build().ok()?;
    let r = http.post(format!("{}/api/chat", rw.base)).json(&body).send().ok()?;
    if !r.status().is_success() { return None; }
    let j: serde_json::Value = r.json().ok()?;
    let t = clean_rewrite(j.pointer("/message/content")?.as_str()?, user)?;
    Some(scrub_carried_names(&t, user).0)
}

/// The production rewriter: SIGIL_AI_REWRITE_URL (loopback only) + an installed
/// small model (or SIGIL_AI_REWRITE_MODEL). None when there is no local model.
pub(crate) fn find_rewriter() -> Option<Rewriter> {
    let base = rewriter_base_from(std::env::var("SIGIL_AI_REWRITE_URL").ok().as_deref())?;
    let forced = std::env::var("SIGIL_AI_REWRITE_MODEL").ok();
    let models = if forced.as_deref().map(|f| !f.trim().is_empty()).unwrap_or(false) { Vec::new() } else { list_models_at(&base) };
    let model = pick_rewriter(&models, forced.as_deref())?;
    Some(Rewriter { base, model })
}

pub(crate) fn rewrite_mode() -> RewriteMode {
    rewrite_mode_from(std::env::var("SIGIL_AI_REWRITE").ok().as_deref())
}

/// `$SIGIL_TOP_HOME/veil-egress.jsonl` (default `~/.sigil-top/`) — "what did my AI send out?"
pub(crate) fn veil_ledger_path() -> std::path::PathBuf {
    std::env::var("SIGIL_TOP_HOME").ok().map(std::path::PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| std::path::Path::new(&h).join(".sigil-top")))
        .or_else(|| std::env::var("USERPROFILE").ok().map(|h| std::path::Path::new(&h).join(".sigil-top")))
        .unwrap_or_else(std::env::temp_dir)
        .join("veil-egress.jsonl")
}

/// Gate a chat request bound for an OFF-BOX model, in place. System text
/// (our prompt + signed skills) is pseudonymised but never refused; user,
/// assistant and tool text is gated (hard stops refuse). Returns the map that
/// rehydrates the answer, or the message to show instead of sending.
pub(crate) fn veil_outbound(messages: &mut [serde_json::Value], destination: &str,
                            ledger: Option<&std::path::Path>) -> Result<sigil_veil::VeilMap, String> {
    let mut map = sigil_veil::VeilMap::new();
    let mut findings: Vec<(sigil_veil::Kind, String)> = Vec::new();
    for m in messages.iter_mut() {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("").to_string();
        let Some(content) = m.get("content").and_then(|c| c.as_str()).map(str::to_string) else { continue };
        let text = if role == "system" {
            let (t, f) = sigil_veil::pseudonymize(&content, &mut map);
            findings.extend(f);
            t
        } else {
            match sigil_veil::gate(&content, &mut map) {
                sigil_veil::Verdict::Send { text, findings: f } => { findings.extend(f); text }
                sigil_veil::Verdict::Refuse { reasons } => {
                    let what: Vec<&str> = reasons.iter().map(|k| match k {
                        sigil_veil::Kind::Mnemonic => "a recovery phrase",
                        _ => "a secret key",
                    }).collect();
                    return Err(format!(
                        "🕶 Veil stopped this message: it contains {}, and the model is OFF this machine ({}). \
                         Nothing was sent. Remove it — or use the on-device model (unset SIGIL_OLLAMA_ALLOW_REMOTE), \
                         where nothing ever leaves.",
                        what.join(" and "), destination));
                }
            }
        };
        if let Some(o) = m.as_object_mut() { o.insert("content".into(), serde_json::Value::String(text)); }
    }
    if let Some(path) = ledger {
        let sent = serde_json::to_string(messages).unwrap_or_default();
        let _ = sigil_veil::ledger_append(path, &sigil_veil::LedgerEntry::new(destination, &sent, &findings));
    }
    Ok(map)
}

/// Chat-box words that open the privacy view: "what did my AI send out?"
pub(crate) fn is_privacy_command(s: &str) -> bool {
    let w = s.trim().trim_start_matches('/').trim_end_matches('?').to_ascii_lowercase();
    matches!(w.as_str(), "privacy" | "egress" | "veil" | "what left" | "what did you send" | "what did my ai send")
}

/// The privacy view: where the model runs right now, and the egress ledger.
pub(crate) fn privacy_report() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let path = veil_ledger_path();
    let where_ = match current_ai_route() {
        AiRoute::Local => format!("Your model runs ON THIS DEVICE ({}) — conversations never leave it.", ollama_base()),
        AiRoute::Proxied(p) => format!(
            "Your model is OFF this device ({}). Every message passes SIGIL Veil first, and travels through Tor ({p}) — the provider does not see your IP.",
            ollama_base()),
        AiRoute::Direct => format!(
            "⚠ Your model is OFF this device ({}). Every message passes SIGIL Veil first — but the connection is DIRECT, so the provider sees this machine's IP. \
             Start Tor (port 9050) and it is used automatically; SIGIL_AI_REQUIRE_TOR=1 refuses to send without it.",
            ollama_base()),
        AiRoute::Refuse(m) => m,
    };
    let remote = !matches!(current_ai_route(), AiRoute::Local);
    let granted = crate::ai_tools::granted_scopes(remote, std::env::var("SIGIL_AI_REMOTE_SCOPES").ok().as_deref());
    let names: Vec<&str> = granted.iter().map(|s| crate::ai_tools::scope_label(*s)).collect();
    let perms = if remote {
        format!("   permissions of the off-device model: {} (wallet, payments and actions only if granted: SIGIL_AI_REMOTE_SCOPES=wallet,payments,actions)", names.join(", "))
    } else {
        "   permissions: everything — the model is on this device, so nothing it sees leaves.".to_string()
    };
    let ledger = std::fs::read_to_string(&path).unwrap_or_default();
    let body = sigil_veil::render_summary(&sigil_veil::ledger_summary(&ledger, 5), now);
    format!("{where_}\n{perms}\n{body}\n   ledger: {}", path.display())
}

/// Ledger one more outbound request of an agent turn (a follow-up step that carries
/// tool results back to the off-box model).
pub(crate) fn veil_ledger_step(messages: &[serde_json::Value], destination: &str,
                               findings: &[(sigil_veil::Kind, String)], ledger: Option<&std::path::Path>) {
    if let Some(path) = ledger {
        let sent = serde_json::to_string(messages).unwrap_or_default();
        let _ = sigil_veil::ledger_append(path, &sigil_veil::LedgerEntry::new(destination, &sent, findings));
    }
}

/// Restore placeholders inside a JSON value (tool-call arguments from an/// Restore placeholders inside a JSON value (tool-call arguments from an/// Restore placeholders inside a JSON value (tool-call arguments from an
/// off-box model) so the tool layer acts on the real address, never `[QNK_1]`.
pub(crate) fn rehydrate_json(v: &mut serde_json::Value, map: &sigil_veil::VeilMap) {
    match v {
        serde_json::Value::String(s) => *s = sigil_veil::rehydrate(s, map),
        serde_json::Value::Array(a) => a.iter_mut().for_each(|x| rehydrate_json(x, map)),
        serde_json::Value::Object(o) => o.values_mut().for_each(|x| rehydrate_json(x, map)),
        _ => {}
    }
}

/// Live machine load while a reply is generating: GPU via `nvidia-smi` (only meaningful when
/// ollama runs on THIS machine), CPU from `/proc/stat` deltas. `None` = not measurable here.
#[derive(Debug, Clone, Default)]
pub(crate) struct Load { pub gpu_pct: Option<u32>, pub vram_used_mb: Option<u64>, pub vram_total_mb: Option<u64>, pub cpu_pct: Option<u32> }

pub(crate) fn ollama_is_local() -> bool {
    host_of(&ollama_base()).map(|h| is_loopback_host(&h)).unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn cpu_times() -> Option<(u64, u64)> {
    let s = std::fs::read_to_string("/proc/stat").ok()?;
    let f: Vec<u64> = s.lines().next()?.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
    if f.len() < 4 { return None; }
    let idle = f[3] + f.get(4).copied().unwrap_or(0);
    Some((f.iter().sum(), idle))
}
#[cfg(not(target_os = "linux"))]
fn cpu_times() -> Option<(u64, u64)> { None }

/// Sample load once a second until `stop` is set; each sample replaces `slot`.
pub(crate) fn sample_load(slot: std::sync::Arc<std::sync::Mutex<Load>>, stop: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    let local = ollama_is_local();
    let mut prev = cpu_times();
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(1000));
        let mut l = Load::default();
        if local {
            if let Ok(o) = std::process::Command::new("nvidia-smi")
                .args(["--query-gpu=utilization.gpu,memory.used,memory.total", "--format=csv,noheader,nounits"]).output() {
                let s = String::from_utf8_lossy(&o.stdout);
                let p: Vec<u64> = s.lines().next().unwrap_or("").split(',').filter_map(|x| x.trim().parse().ok()).collect();
                if p.len() == 3 { l.gpu_pct = Some(p[0] as u32); l.vram_used_mb = Some(p[1]); l.vram_total_mb = Some(p[2]); }
            }
        }
        let now = cpu_times();
        if let (Some((t0, i0)), Some((t1, i1))) = (prev, now) {
            let dt = t1.saturating_sub(t0);
            if dt > 0 { l.cpu_pct = Some((100 * dt.saturating_sub(i1.saturating_sub(i0)) / dt) as u32); }
        }
        prev = now;
        if let Ok(mut g) = slot.lock() { *g = l; }
    }
}

/// Human guidance shown when no local model is found — kept here so the tab and
/// any headless helper print the same words. Written for a reader who may have NO
/// function keys (a phone under Termux): the setup starts by itself, and the typed
/// word `setup` is the universal way to run it again.
pub(crate) fn setup_hint() -> &'static str {
    "No local model yet — flux-moe is setting itself up now (progress appears above):\n\
     1. installs ollama from the flux-SIGNED manifest (SHA-256 + size verified before anything runs; on a phone under Termux: `pkg install ollama`)\n\
     2. pulls the model that fits THIS machine (GPU → qwen3:8b · CPU box → qwen3:4b · phone or < 6 GB RAM → qwen3:1.7b)\n\
     3. you chat here.\n\
     To run it again: type `setup` and press Enter (F5 does the same on a keyboard that has it).\n\
     By hand instead: install ollama (ollama.com), `ollama serve`, `ollama pull qwen3:4b`, reopen this tab.\n\
     Overrides: SIGIL_AI_MODEL=<tag> forces a model · SIGIL_OLLAMA=<url> points at another host · SIGIL_AI_AUTOSETUP=0 never installs without being asked."
}

/// Chat-box words that mean "run the out-of-the-box setup". Function keys are not
/// universal — a phone has none, some terminals eat F5 — so a typed command must work
/// everywhere the chat box does.
pub(crate) fn is_setup_command(s: &str) -> bool {
    matches!(
        s.trim().trim_start_matches('/').to_ascii_lowercase().as_str(),
        "setup" | "set up" | "install" | "retry" | "f5" | "auto-setup" | "autosetup"
    )
}

/// `SIGIL_AI_AUTOSETUP=0|false|no|off` turns the unattended install/pull OFF (the probe
/// still runs; `setup` / F5 still work). Default ON: an [A]I tab that opens and does
/// nothing is not "out of the box".
pub(crate) fn autosetup_enabled() -> bool {
    enabled_from_setting(std::env::var("SIGIL_AI_AUTOSETUP").ok().as_deref())
}

/// Pure: what a given `SIGIL_AI_AUTOSETUP` value means.
pub(crate) fn enabled_from_setting(v: Option<&str>) -> bool {
    !matches!(
        v.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("0") | Some("false") | Some("no") | Some("off")
    )
}

/// Pure: and whether it runs at BOOT rather than only when the tab is opened.
pub(crate) fn boot_from_setting(v: Option<&str>) -> bool {
    enabled_from_setting(v) && !matches!(v.map(|s| s.trim().to_ascii_lowercase()).as_deref(), Some("tab"))
}

/// Running inside Termux on Android. Termux sets `TERMUX_VERSION` and its `PREFIX` is
/// `/data/data/com.termux/files/usr`; either is proof enough. Matters twice: ollama is
/// installed with `pkg`, not from a glibc tarball (Android's libc is bionic), and a phone
/// wants the small model tier.
pub(crate) fn is_termux() -> bool {
    std::env::var_os("TERMUX_VERSION").is_some()
        || std::env::var("PREFIX").map(|p| p.contains("com.termux")).unwrap_or(false)
}

/// When auto-setup runs. `SIGIL_AI_AUTOSETUP`:
///   unset / `1` / `boot` → at start-up, in the background (default since 8.0.6 — the
///                          [A]I tab is ready by the time anyone opens it);
///   `tab`                → only when the [A]I tab is opened (the 8.0.4 behaviour);
///   `0` / `off`          → never (the tab still says how to run it by hand).
pub(crate) fn autosetup_at_boot() -> bool {
    boot_from_setting(std::env::var("SIGIL_AI_AUTOSETUP").ok().as_deref())
}

/// On CPU, anything past this many billion parameters is minutes per answer: a 27 B model
/// decodes at roughly a third of a 9 B one and the 9 B already takes ~45 s on 48 cores.
const CPU_MAX_PARAMS_B: f64 = 10.0;
/// What a phone stays conversational with (≈2 tok/s at 4 B; 1–2 B is the usable band).
const PHONE_MAX_PARAMS_B: f64 = 2.5;

/// Pick the best rung of a model LADDER — largest first, as the signed manifest lists
/// them — for THIS machine. Returns `(tag, why)`; the reason names the measurement so a
/// user who disagrees can point at the number. Rules, in order:
///  * `SIGIL_AI_MODEL` wins outright.
///  * A phone (Termux) takes the largest rung ≤ 2.5 B parameters.
///  * A GPU takes the largest rung whose weights fit its VRAM.
///  * No GPU takes the largest rung ≤ 10 B that leaves 2.5× headroom in RAM and, above
///    5 B, has at least 8 cores to keep the tab interactive.
///  * Nothing measured as fitting → the smallest rung, and the reason says so.
///  * Tags without a size (`foo:latest`) cannot be judged and are skipped, unless every
///    rung is unsized — then the manifest's first choice stands.
pub(crate) fn pick_from_ladder(rungs: &[String], hw: &Hardware) -> (String, String) {
    if let Some(forced) = std::env::var("SIGIL_AI_MODEL").ok().filter(|s| !s.trim().is_empty()) {
        let forced = forced.trim().to_string();
        return (forced.clone(), format!("SIGIL_AI_MODEL={forced} (your explicit choice)"));
    }
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    pick_from_ladder_with(rungs, hw, cores, is_termux())
}

/// The pure half: no environment, no probing — every input is an argument, so the rules
/// are testable without setting a process-global variable that races a parallel test.
pub(crate) fn pick_from_ladder_with(rungs: &[String], hw: &Hardware, cores: usize, phone: bool) -> (String, String) {
    let rungs: Vec<&str> = rungs.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
    let Some(&first) = rungs.first() else {
        return ("qwen3:8b".to_string(), "qwen3:8b (the manifest names no models; built-in default)".to_string());
    };
    let smallest = *rungs.last().unwrap_or(&first);
    if phone {
        let pick = rungs.iter().copied().find(|t| tag_params_b(t).map(|p| p <= PHONE_MAX_PARAMS_B).unwrap_or(false)).unwrap_or(smallest);
        return (pick.to_string(), format!("{pick} — this is a phone (Termux); the small model keeps answers under a minute on a phone CPU (SIGIL_AI_MODEL=<tag> overrides)"));
    }
    let gib = |b: u64| b as f64 / GIB as f64;
    let mut any_sized = false;
    for &tag in &rungs {
        let (Some(params), Some(need)) = (tag_params_b(tag), approx_model_bytes(tag)) else { continue };
        any_sized = true;
        match hw.vram_bytes {
            Some(v) if v >= need => {
                return (tag.to_string(), format!("{tag} — {:.0} GB VRAM fits it (~{:.1} GB needed)", gib(v), gib(need)));
            }
            Some(_) => continue,
            None => {
                if params > CPU_MAX_PARAMS_B {
                    continue;
                }
                let ram_ok = hw.ram_bytes.map(|r| r >= need.saturating_mul(5) / 2).unwrap_or(false);
                let cores_ok = params <= 5.0 || cores >= 8;
                if ram_ok && cores_ok {
                    let ram = hw.ram_bytes.unwrap_or(0);
                    return (tag.to_string(), format!("{tag} — no GPU, but {cores} cores and {:.0} GB RAM run it on CPU (~{:.1} GB needed)", gib(ram), gib(need)));
                }
            }
        }
    }
    if !any_sized {
        return (first.to_string(), format!("{first} (model sizes not derivable from the tags; taking the manifest's first choice)"));
    }
    (smallest.to_string(), format!("{smallest} — the smallest rung; nothing bigger measured as fitting ({}, {cores} cores)", describe_hardware(hw)))
}

/// [`pick_model`] with one more rung below the fallback: the manifest's `small_model`
/// (qwen3:1.7b) for a phone or any box under 6 GB RAM. Measured on a Snapdragon-class
/// phone a 4b model decodes at ~2 tok/s (a minute per short answer); 1.7b is the largest
/// that stays conversational there. `SIGIL_AI_MODEL` still wins over everything.
pub(crate) fn pick_model_tiered(
    default_model: &str,
    fallback_model: Option<&str>,
    small_model: Option<&str>,
    hw: &Hardware,
) -> (String, String) {
    if let Some(forced) = std::env::var("SIGIL_AI_MODEL").ok().filter(|s| !s.trim().is_empty()) {
        let forced = forced.trim().to_string();
        return (forced.clone(), format!("SIGIL_AI_MODEL={forced} (your explicit choice)"));
    }
    if let Some(small) = small_model.filter(|s| !s.trim().is_empty()) {
        const SMALL_RAM: u64 = 6 * GIB;
        if is_termux() {
            return (small.to_string(), format!("{small} — this is a phone (Termux); the small model keeps answers under a minute on a phone CPU (SIGIL_AI_MODEL=<tag> overrides)"));
        }
        if let Some(ram) = hw.ram_bytes {
            if ram < SMALL_RAM && hw.vram_bytes.is_none() {
                return (small.to_string(), format!("{small} — only {:.1} GB RAM and no GPU; the small model is the one that fits (SIGIL_AI_MODEL=<tag> overrides)", ram as f64 / GIB as f64));
            }
        }
    }
    pick_model(default_model, fallback_model, hw)
}

#[cfg(test)]
pub(crate) mod tests {
    /// Env vars are process-global and cargo runs tests in parallel threads: one
    /// test's remove_var landed mid-another's assert (explicit_choice_always_wins,
    /// then small_tier_only_for_phones_and_tiny_boxes — both failed in the suite and
    /// passed alone, 2026-10-03). EVERY test here that calls set_var/remove_var holds this.
    static AI_MODEL_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());
    pub(crate) fn ai_model_env() -> std::sync::MutexGuard<'static, ()> {
        AI_MODEL_ENV.lock().unwrap_or_else(|p| p.into_inner())
    }

    // ── SIGIL Veil wiring ──
    const VEIL_HEX: &str = "4973498a9865b291636faef205f728a49d98890f001e9e806479043f038ebf6c";

    #[test]
    fn veil_refuses_a_recovery_phrase_bound_off_box_and_sends_nothing() {
        let dir = std::env::temp_dir().join(format!("veil-wire-{}", std::process::id()));
        let ledger = dir.join("veil-egress.jsonl");
        let mut msgs = vec![
            serde_json::json!({"role":"system","content":"You are SIGIL's assistant."}),
            serde_json::json!({"role":"user","content":"is this safe: abandon ability able about above absent absorb abstract absurd abuse access accident"}),
        ];
        let r = veil_outbound(&mut msgs, "http://gpu.example:11434", Some(&ledger));
        assert!(r.unwrap_err().contains("recovery phrase"));
        assert!(!ledger.exists(), "a refused request must not even be ledgered as sent");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ai_route_decisions() {
        assert_eq!(decide_ai_route(true, Some("socks5h://127.0.0.1:9050".into()), true, "h"), AiRoute::Local,
            "a local model is never proxied — Tor cannot reach loopback ollama");
        assert_eq!(decide_ai_route(false, Some("socks5h://127.0.0.1:9050".into()), false, "h"),
            AiRoute::Proxied("socks5h://127.0.0.1:9050".into()));
        assert_eq!(decide_ai_route(false, None, false, "h"), AiRoute::Direct);
        assert!(matches!(decide_ai_route(false, None, true, "gpu.example"), AiRoute::Refuse(m) if m.contains("gpu.example")));
    }

    /// The property users rely on, measured instead of assumed: a request on the
    /// Proxied route really goes THROUGH the SOCKS proxy, and the proxy is handed the
    /// HOSTNAME (socks5h) — so not even the DNS lookup leaves outside Tor.
    #[test]
    fn proxied_route_really_travels_through_socks_with_the_hostname() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || -> (String, u16, String) {
            let (mut c, _) = listener.accept().unwrap();
            let mut b = [0u8; 2];
            c.read_exact(&mut b).unwrap();                       // VER, NMETHODS
            let mut m = vec![0u8; b[1] as usize];
            c.read_exact(&mut m).unwrap();
            c.write_all(&[5, 0]).unwrap();                        // no auth
            let mut h = [0u8; 4];
            c.read_exact(&mut h).unwrap();                        // VER CMD RSV ATYP
            assert_eq!(h[3], 3, "ATYP must be DOMAIN — the client must not resolve DNS itself");
            let mut l = [0u8; 1];
            c.read_exact(&mut l).unwrap();
            let mut host = vec![0u8; l[0] as usize];
            c.read_exact(&mut host).unwrap();
            let mut p = [0u8; 2];
            c.read_exact(&mut p).unwrap();
            c.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).unwrap(); // granted
            let mut req = vec![0u8; 4096];
            let n = c.read(&mut req).unwrap();
            let body = r#"{"message":{"content":"ok"},"done":true}"#;
            let _ = c.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}", body.len(), body).as_bytes());
            (String::from_utf8_lossy(&host).to_string(), u16::from_be_bytes(p), String::from_utf8_lossy(&req[..n]).to_string())
        });
        let route = AiRoute::Proxied(format!("socks5h://127.0.0.1:{port}"));
        let client = chat_client(&route).expect("socks proxy must configure in this build");
        let r = client.post("http://remote-model.example:11434/api/chat").body("{\"q\":\"[QNK_1]\"}").send().expect("through the proxy");
        assert!(r.status().is_success());
        let (host, p, req) = server.join().unwrap();
        assert_eq!((host.as_str(), p), ("remote-model.example", 11434));
        assert!(req.starts_with("POST /api/chat") && req.contains("[QNK_1]"), "{req}");
    }

    #[test]
    fn rewrite_config_and_picking() {
        assert_eq!(rewrite_mode_from(None), RewriteMode::Auto);
        assert_eq!(rewrite_mode_from(Some("off")), RewriteMode::Off);
        assert_eq!(rewrite_mode_from(Some("require")), RewriteMode::Require);
        assert_eq!(rewriter_base_from(None).as_deref(), Some("http://localhost:11434"));
        assert_eq!(rewriter_base_from(Some("http://127.0.0.1:11500/")).as_deref(), Some("http://127.0.0.1:11500"));
        assert_eq!(rewriter_base_from(Some("http://gpu.example:11434")), None, "the rewriter sees the RAW question: never off-box");
        assert_eq!(rewriter_base_from(Some("http://localhost@evil.example")), None);
        let m = vec!["qwen3:8b".to_string(), "qwen3:1.7b".to_string()];
        assert_eq!(pick_rewriter(&m, None).as_deref(), Some("qwen3:1.7b"));
        assert_eq!(pick_rewriter(&m, Some("phi4:mini")).as_deref(), Some("phi4:mini"));
        assert_eq!(pick_rewriter(&[], None), None);
    }

    /// Real outputs measured on Epsilon (2026-10-03), pinned as regressions.
    #[test]
    fn measured_rewrites_are_judged_correctly() {
        let q1 = "I'm Viktor from Frederikshavn, my mum lent me 200 SIGIL and now my wallet send to [HEX_1] fails with HTTP 413 — what's going on?";
        let good = "What is going on with the wallet sending to [HEX_1] when a 413 HTTP error occurs?";
        assert_eq!(clean_rewrite(good, q1).as_deref(), Some(good), "qwen3:0.6b's good rewrite passes");
        assert_eq!(scrub_carried_names(good, q1).1, 0);
        let q2 = "Hej, jeg hedder Anna og arbejder på Aalborg Sygehus. Min chef Lars siger min mining rig laver 525 MH/s men jeg får 0 SIGIL — hvorfor?";
        let leaky = "What is the reason for receiving a 0 SIGIL error when a mining rig produces 525 MH/s at Aalborg Sygehus?";
        let ok = clean_rewrite(leaky, q2).expect("form is fine");
        let (scrubbed, n) = scrub_carried_names(&ok, q2);
        assert_eq!(n, 2);
        assert!(!scrubbed.contains("Aalborg") && !scrubbed.contains("Sygehus") && scrubbed.contains("525 MH/s"), "{scrubbed}");
        // qwen3:4b with think:false answered with its reasoning, quoting the original
        let reasoning = "We are rewriting the user's latest request as ONE self-contained question.\n Steps:\n 1. Remove names \
                         Original user request: \"I'm Viktor from Frederikshavn, my mum lent me 200 SIGIL";
        assert_eq!(clean_rewrite(reasoning, q1), None);
        let reasoning2 = "Okay, let's tackle this. The user wants me to rewrite their latest request";
        assert_eq!(clean_rewrite(reasoning2, q2), None);
        // ...but ordinary words that merely CONTAIN a meta word are fine
        let fine = "Why would a cryptocurrency wallet's send request fail with HTTP 413?";
        assert_eq!(clean_rewrite(fine, q1).as_deref(), Some(fine), "\"wallet's\" is not \"let's\"");
        // copying the original verbatim is not a rewrite
        assert_eq!(clean_rewrite("my mum lent me 200 SIGIL and now my wallet send fails", q1), None);
    }

    /// Real model, real machine: `fluxc test -p sigil-top --bin sigil-top -- --ignored real_local_rewrite`.
    /// Needs ollama on localhost with qwen3:0.6b (or SIGIL_AI_REWRITE_MODEL).
    #[test]
    #[ignore]
    fn real_local_rewrite_smoke() {
        let model = std::env::var("SIGIL_AI_REWRITE_MODEL").unwrap_or_else(|_| "qwen3:0.6b".into());
        let rw = Rewriter { base: "http://localhost:11434".into(), model };
        for q in ["I'm Viktor from Frederikshavn, my mum lent me 200 SIGIL and now my wallet send to [HEX_1] fails with HTTP 413 — what's going on?",
                  "Hej, jeg hedder Anna og arbejder på Aalborg Sygehus. Min chef Lars siger min mining rig laver 525 MH/s men jeg får 0 SIGIL — hvorfor?"] {
            let t0 = std::time::Instant::now();
            let out = rewrite_with(&rw, &[], q);
            eprintln!("[{:.0}s] {q}\n   → {out:?}", t0.elapsed().as_secs_f64());
            if let Some(o) = out {
                for leak in ["Viktor", "Frederikshavn", "Anna", "Aalborg", "Sygehus", "Lars"] {
                    assert!(!o.contains(leak), "{leak} survived: {o}");
                }
            }
        }
    }

    #[test]
    fn proper_noun_heuristic() {
        assert_eq!(proper_nouns("Hej, jeg hedder Anna og arbejder på Aalborg Sygehus. Min chef Lars siger"),
                   vec!["Anna", "Aalborg", "Sygehus", "Lars"]);
        assert!(proper_nouns("why does SIGIL show HTTP 413?").is_empty(), "acronyms are not names");
    }

    #[test]
    fn rewrite_output_is_vetted() {
        let orig = "my name is Viktor in Frederikshavn — why does my send fail with error 413?";
        assert_eq!(clean_rewrite("<think>hmm</think>Rewritten question: \"Why would a send fail with HTTP 413?\"", orig).as_deref(),
            Some("Why would a send fail with HTTP 413?"));
        assert_eq!(clean_rewrite("", orig), None);
        assert_eq!(clean_rewrite("I can't help with that.", orig), None);
        assert_eq!(clean_rewrite(&"x".repeat(5000), orig), None, "an essay is not a rewrite");
        let msgs = rewrite_request_messages(&[("user".into(), "hi".into()), ("tool".into(), "{secret}".into())], orig);
        let p = msgs[1]["content"].as_str().unwrap();
        assert!(p.contains("Latest request:") && p.contains("user: hi") && !p.contains("{secret}"), "{p}");
    }

    #[test]
    fn only_socks_proxies_are_accepted() {
        assert!(validate_ai_proxy("socks5h://127.0.0.1:9050").is_ok());
        assert!(validate_ai_proxy(" SOCKS5://10.0.0.2:1080 ").is_ok());
        assert!(validate_ai_proxy("http://proxy.example:8080").is_err(), "http proxies leak the hostname");
        assert!(validate_ai_proxy("127.0.0.1:9050").is_err());
    }

    #[test]
    fn ledger_labels_say_how_it_travelled() {
        assert!(route_label("http://gpu.example:11434", &AiRoute::Proxied("socks5h://127.0.0.1:9050".into())).contains("via Tor"));
        assert!(route_label("http://gpu.example:11434", &AiRoute::Direct).contains("provider sees this IP"));
    }

    #[test]
    fn privacy_command_words() {
        for w in ["privacy", "/privacy", "Egress", "veil", "what left?", "what did my AI send?"] {
            assert!(is_privacy_command(w), "{w:?}");
        }
        for w in ["private send", "send privately", "egress fee"] {
            assert!(!is_privacy_command(w), "{w:?}");
        }
    }

    #[test]
    fn veil_pseudonymises_and_ledgers_what_left() {
        let dir = std::env::temp_dir().join(format!("veil-wire2-{}", std::process::id()));
        let ledger = dir.join("veil-egress.jsonl");
        let mut msgs = vec![
            serde_json::json!({"role":"system","content":format!("Wallet in use: qnk{VEIL_HEX}. example seed = {VEIL_HEX}")}),
            serde_json::json!({"role":"user","content":format!("why did my send to qnk{VEIL_HEX} fail? mail me at viktor@sigilgraph.org")}),
        ];
        let map = veil_outbound(&mut msgs, "http://gpu.example:11434", Some(&ledger)).expect("system text is never refused");
        let wire = serde_json::to_string(&msgs).unwrap();
        assert!(!wire.contains(VEIL_HEX) && !wire.contains("sigilgraph.org"), "{wire}");
        assert!(wire.contains("[QNK_1]") && wire.contains("[EMAIL_1]"), "{wire}");
        let line = std::fs::read_to_string(&ledger).unwrap();
        assert!(line.contains("gpu.example") && !line.contains(VEIL_HEX));
        // the answer and a tool call come back with real values
        assert_eq!(sigil_veil::rehydrate("Resend to [QNK_1].", &map), format!("Resend to qnk{VEIL_HEX}."));
        let mut call = serde_json::json!({"function":{"name":"balance","arguments":{"address":"[QNK_1]"}}});
        rehydrate_json(&mut call, &map);
        assert_eq!(call["function"]["arguments"]["address"], format!("qnk{VEIL_HEX}"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The on-device promise is enforced, not just documented: a REMOTE endpoint is
    /// refused and the localhost default used instead, unless the operator has also
    /// opted in explicitly. Pure — never touches the process-global env.
    #[test]
    fn a_remote_ollama_endpoint_is_refused_unless_explicitly_allowed() {
        // Nothing configured -> the on-device default, no warning.
        assert_eq!(super::resolve_ollama_base(None, false), (super::DEFAULT_OLLAMA.to_string(), None));
        assert_eq!(super::resolve_ollama_base(Some("  "), false).0, super::DEFAULT_OLLAMA);

        // Loopback in all its spellings -> honoured silently.
        for ok in ["http://localhost:11434", "http://127.0.0.1:11434", "http://127.1.2.3:1234", "http://[::1]:11434", "https://my.localhost:443"] {
            let (base, warn) = super::resolve_ollama_base(Some(ok), false);
            assert_eq!(base, ok, "loopback {ok} must be honoured");
            assert!(warn.is_none(), "loopback {ok} must not warn");
        }

        // Remote WITHOUT opt-in -> refused, falls back on-device, warns.
        let (base, warn) = super::resolve_ollama_base(Some("http://evil.example.com:11434"), false);
        assert_eq!(base, super::DEFAULT_OLLAMA, "a remote endpoint must not be used");
        assert!(warn.unwrap().contains("REFUSED"));

        // Remote WITH opt-in -> honoured, but still warns every launch.
        let (base, warn) = super::resolve_ollama_base(Some("http://gpu.example.com:11434"), true);
        assert_eq!(base, "http://gpu.example.com:11434");
        assert!(warn.unwrap().contains("leaves this machine"));

        // Credentials in the URL must not smuggle a remote host past the check.
        let (base, _) = super::resolve_ollama_base(Some("http://localhost@evil.example.com:11434"), false);
        assert_eq!(base, super::DEFAULT_OLLAMA, "userinfo must not spoof the host");
    }

    use super::*;

    fn gb(n: u64) -> u64 { n * GIB }
    fn ladder() -> Vec<String> {
        ["qwen3.8:27b", "qwen3.5:9b", "qwen3.5:4b", "qwen3.5:2b", "qwen3.5:0.8b"].iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn ladder_gpu_takes_largest_rung_that_fits_vram() {
        // RTX 2080: 8 GB → the 9b (~6.9 GB), not the 27b (~18.6 GB)
        let hw = Hardware { vram_bytes: Some(gb(8)), ram_bytes: Some(gb(32)), gpu_name: Some("RTX 2080".into()) };
        let (tag, why) = pick_from_ladder_with(&ladder(), &hw, 8, false);
        assert_eq!(tag, "qwen3.5:9b", "{why}");
        // 24 GB card → the 27b
        let hw = Hardware { vram_bytes: Some(gb(24)), ram_bytes: Some(gb(64)), gpu_name: Some("RTX 4090".into()) };
        assert_eq!(pick_from_ladder_with(&ladder(), &hw, 16, false).0, "qwen3.8:27b");
        // 4 GB card → the 4b (~3.6 GB)
        let hw = Hardware { vram_bytes: Some(gb(4)), ram_bytes: Some(gb(16)), gpu_name: Some("GTX 1050".into()) };
        assert_eq!(pick_from_ladder_with(&ladder(), &hw, 8, false).0, "qwen3.5:4b");
    }

    #[test]
    fn ladder_cpu_caps_at_ten_billion_and_needs_headroom() {
        // Epsilon-class: no GPU, 63 GB, 48 cores → 9b, never the 27b on CPU
        let hw = Hardware { vram_bytes: None, ram_bytes: Some(gb(63)), gpu_name: None };
        let (tag, why) = pick_from_ladder_with(&ladder(), &hw, 48, false);
        assert_eq!(tag, "qwen3.5:9b", "{why}");
        // 4-core laptop, 16 GB → 4b (the 9b wants 8 cores)
        let hw = Hardware { vram_bytes: None, ram_bytes: Some(gb(16)), gpu_name: None };
        assert_eq!(pick_from_ladder_with(&ladder(), &hw, 4, false).0, "qwen3.5:4b");
        // 2-core 4 GB VPS → the smallest rung
        let hw = Hardware { vram_bytes: None, ram_bytes: Some(gb(4)), gpu_name: None };
        assert_eq!(pick_from_ladder_with(&ladder(), &hw, 2, false).0, "qwen3.5:0.8b");
        // Unmeasurable box → smallest rung, never a gamble on the big one
        let hw = Hardware::default();
        assert_eq!(pick_from_ladder_with(&ladder(), &hw, 8, false).0, "qwen3.5:0.8b");
    }

    #[test]
    fn ladder_phone_takes_the_largest_small_rung() {
        let hw = Hardware { vram_bytes: None, ram_bytes: Some(gb(8)), gpu_name: None };
        assert_eq!(pick_from_ladder_with(&ladder(), &hw, 8, true).0, "qwen3.5:2b");
    }

    #[test]
    fn ladder_unsized_tags_are_skipped_or_taken_first() {
        let hw = Hardware { vram_bytes: Some(gb(8)), ram_bytes: Some(gb(32)), gpu_name: None };
        let l: Vec<String> = ["qwen3.5:latest", "qwen3.5:9b"].iter().map(|s| s.to_string()).collect();
        assert_eq!(pick_from_ladder_with(&l, &hw, 8, false).0, "qwen3.5:9b");
        let l: Vec<String> = ["qwen3.5:latest", "llama3:latest"].iter().map(|s| s.to_string()).collect();
        assert_eq!(pick_from_ladder_with(&l, &hw, 8, false).0, "qwen3.5:latest");
    }

    #[test]
    fn autosetup_boot_vs_tab() {
        // Pure over the setting's TEXT: touching the real variable would race every other
        // test in this process — the exact bug this file already paid for once.
        assert!(boot_from_setting(None) && enabled_from_setting(None));
        assert!(!boot_from_setting(Some("tab")) && enabled_from_setting(Some("tab")));
        assert!(!boot_from_setting(Some("0")) && !enabled_from_setting(Some("0")));
        assert!(!boot_from_setting(Some("off")));
        assert!(boot_from_setting(Some("1")));
    }

    #[test]
    fn ollama_base_defaults_to_localhost() {
        let _env = ai_model_env();
        // Without the override, it targets the user's own machine.
        std::env::remove_var("SIGIL_OLLAMA");
        assert_eq!(ollama_base(), "http://localhost:11434");
    }

    #[test]
    fn system_prompt_forbids_inventing_numbers() {
        let p = system_prompt().to_lowercase();
        assert!(p.contains("never invent"));
        assert!(p.contains("sigil"));
    }

    #[test]
    fn system_prompt_bans_the_measured_failure_modes() {
        // Regression pins for the "Gordon Klein emission" reply: the model
        // stonewalled on an unfamiliar name and then listed its own features.
        let p = system_prompt().to_lowercase();
        assert!(p.contains("do not stop at"), "must not stonewall on an unknown name");
        assert!(p.contains("bulleted inventory"), "must not answer with a feature list");
        assert!(p.contains("as an ai"), "must ban the self-referential opener");
    }

    #[test]
    fn system_prompt_knows_the_whitepaper_vocabulary() {
        let p = system_prompt().to_lowercase();
        for term in ["whitepaper", "proof-of-work", "proof-of-time", "halving", "shielded"] {
            assert!(p.contains(term), "system prompt is missing {term}");
        }
    }

    #[test]
    fn no_model_when_ollama_down() {
        // Point at a closed port; list must be empty, not panic. Passed as an
        // ARGUMENT, not through SIGIL_OLLAMA — see list_models_at for the race
        // that the env-var version lost against this box's live ollama.
        assert!(list_models_at("http://127.0.0.1:1").is_empty());
    }

    #[test]
    fn live_models_come_back_newest_first() {
        if std::env::var("SIGIL_OLLAMA_LIVE").as_deref() != Ok("1") {
            eprintln!("skipped: needs a live ollama with >1 model");
            return;
        }
        let m = list_models();
        eprintln!("ordered models: {m:?}");
        assert!(!m.is_empty());
    }

    #[test]
    fn timeout_is_generous_and_overridable() {
        let _env = ai_model_env();
        std::env::remove_var("SIGIL_OLLAMA_TIMEOUT");
        // The old 120 s was BELOW one measured qwen3:4b turn on CPU.
        assert!(chat_timeout_secs() > 120, "default budget must exceed a real CPU turn");
        std::env::set_var("SIGIL_OLLAMA_TIMEOUT", "42");
        assert_eq!(chat_timeout_secs(), 42);
        std::env::set_var("SIGIL_OLLAMA_TIMEOUT", "junk");
        assert!(chat_timeout_secs() > 120, "garbage must fall back, not disable the budget");
        std::env::remove_var("SIGIL_OLLAMA_TIMEOUT");
    }

    #[test]
    fn think_defers_to_ollama_unless_explicitly_set() {
        let _env = ai_model_env();
        std::env::remove_var("SIGIL_OLLAMA_THINK");
        // MEASURED: forcing think:false on qwen3:4b saved no time and pushed the
        // monologue into `content`. So by default we send no `think` key at all.
        assert_eq!(think_override(), None, "must not override ollama's own default");
        std::env::set_var("SIGIL_OLLAMA_THINK", "1");
        assert_eq!(think_override(), Some(true));
        std::env::set_var("SIGIL_OLLAMA_THINK", "0");
        assert_eq!(think_override(), Some(false));
        std::env::remove_var("SIGIL_OLLAMA_THINK");
    }

    #[test]
    fn strips_inline_think_blocks() {
        assert_eq!(strip_think_blocks("<think>hmm</think>Answer."), "Answer.");
        assert_eq!(strip_think_blocks("A<think>x</think>B"), "AB");
        // Unterminated: keep only what came before it.
        assert_eq!(strip_think_blocks("Hi <think>still going"), "Hi");
        assert_eq!(strip_think_blocks("plain"), "plain");
    }

    #[test]
    fn tag_size_parsing() {
        assert_eq!(tag_params_b("qwen3:8b"), Some(8.0));
        assert_eq!(tag_params_b("qwen3:0.6b"), Some(0.6));
        assert_eq!(tag_params_b("qwen3:14b-instruct"), Some(14.0));
        assert_eq!(tag_params_b("llama3:latest"), None);
        assert_eq!(tag_params_b("mistral"), None);
        // 8b ≈ 8*0.65+1 ≈ 6.2 GB — calibrated on the measured Q4_K_M pulls.
        let b = approx_model_bytes("qwen3:8b").unwrap();
        assert!(b > 5 * GIB && b < 8 * GIB, "got {b}");
    }

    /// RENAMED AND INVERTED 2026-09-06, deliberately, after it failed on this box.
    ///
    /// It used to assert `no GPU => the smaller model`, which matched the rule as
    /// first written. The rule then changed on EVIDENCE (see the no-GPU arm: 8b
    /// measured 44.7 s against 4b's 43.8 s on 48 cores — the same wall clock), and
    /// the test was not updated with it. So the failure was the test being stale,
    /// not the code regressing. Correcting it in the direction of the measurement.
    ///
    /// It also read the host's core count, so it answered differently depending on
    /// which machine ran it. Both cases below now pin `cores` explicitly through
    /// `pick_model_with`, so the verdict is a property of the rule and nothing else.
    #[test]
    fn a_capable_cpu_box_keeps_the_default_model() {
        // EXACTLY the measured Epsilon shape: no GPU, plenty of RAM, plenty of cores.
        let hw = Hardware { vram_bytes: None, ram_bytes: Some(62 * GIB), gpu_name: None };
        let (m, why) = pick_model_with("qwen3:8b", Some("qwen3:4b"), &hw, 48);
        assert_eq!(m, "qwen3:8b", "62 GB and 48 cores run 8b in ~45 s — downgrading buys nothing");
        assert!(why.contains("48 cores"), "the deciding measurement must be shown: {why}");
    }

    #[test]
    fn a_small_vps_still_gets_the_smaller_model() {
        let hw = Hardware { vram_bytes: None, ram_bytes: Some(62 * GIB), gpu_name: None };
        // Same generous RAM, but only 2 cores: CPU decode is the binding constraint.
        let (m, why) = pick_model_with("qwen3:8b", Some("qwen3:4b"), &hw, 2);
        assert_eq!(m, "qwen3:4b", "2 cores cannot keep an 8b tab interactive");
        assert!(why.contains("only 2 cores"), "say WHICH measurement decided it: {why}");

        // And the other way the fallback triggers: cores fine, RAM too tight for
        // 2.5x the weights (8b needs ~6.2 GB, so ~15.5 GB with headroom).
        let tight = Hardware { vram_bytes: None, ram_bytes: Some(8 * GIB), gpu_name: None };
        let (m2, why2) = pick_model_with("qwen3:8b", Some("qwen3:4b"), &tight, 48);
        assert_eq!(m2, "qwen3:4b");
        assert!(why2.contains("RAM"), "{why2}");
    }

    /// The host-dependence guard. `pick_model_with` must be a pure function of its
    /// arguments — if this ever fails, someone has reintroduced a probe inside it
    /// and the suite's verdict silently depends on the runner again.
    #[test]
    fn the_pure_picker_ignores_the_machine_it_runs_on() {
        let hw = Hardware { vram_bytes: None, ram_bytes: Some(62 * GIB), gpu_name: None };
        for _ in 0..3 {
            assert_eq!(pick_model_with("qwen3:8b", Some("qwen3:4b"), &hw, 2).0, "qwen3:4b");
            assert_eq!(pick_model_with("qwen3:8b", Some("qwen3:4b"), &hw, 48).0, "qwen3:8b");
        }
    }

    #[test]
    fn big_gpu_gets_the_big_model() {
        let _env = ai_model_env();
        std::env::remove_var("SIGIL_AI_MODEL");
        let hw = Hardware { vram_bytes: Some(24 * GIB), ram_bytes: Some(64 * GIB), gpu_name: Some("RTX 4090".into()) };
        assert_eq!(pick_model("qwen3:8b", Some("qwen3:4b"), &hw).0, "qwen3:8b");
        // A 6 GB card cannot hold an 8b model (~6.2 GB) plus KV cache.
        let small = Hardware { vram_bytes: Some(6 * GIB), ram_bytes: Some(16 * GIB), gpu_name: Some("RTX 2060".into()) };
        assert_eq!(pick_model("qwen3:8b", Some("qwen3:4b"), &small).0, "qwen3:4b");
    }

    #[test]
    fn no_fallback_never_invents_one() {
        let _env = ai_model_env();
        std::env::remove_var("SIGIL_AI_MODEL");
        let hw = Hardware { vram_bytes: None, ram_bytes: Some(8 * GIB), gpu_name: None };
        let (m, why) = pick_model("qwen3:8b", None, &hw);
        assert_eq!(m, "qwen3:8b");
        assert!(why.contains("no smaller fallback"));
    }

    #[test]
    fn explicit_choice_always_wins() {
        let _env = ai_model_env();
        std::env::set_var("SIGIL_AI_MODEL", "llama3:70b");
        let hw = Hardware { vram_bytes: None, ram_bytes: Some(4 * GIB), gpu_name: None };
        assert_eq!(pick_model("qwen3:8b", Some("qwen3:4b"), &hw).0, "llama3:70b");
        std::env::remove_var("SIGIL_AI_MODEL");
    }

    #[test]
    fn every_error_message_names_a_next_action() {
        let _env = ai_model_env();
        // The whole point of the tab not dead-ending: each message says what to DO.
        let m404 = http_error_message(reqwest::StatusCode::NOT_FOUND, r#"{"error":"model 'qwen3:8b' not found"}"#, "qwen3:8b");
        assert!(m404.contains("F5") || m404.contains("ollama pull"), "{m404}");
        let mmem = http_error_message(reqwest::StatusCode::INTERNAL_SERVER_ERROR, "model requires more system memory", "qwen3:8b");
        assert!(mmem.contains("SIGIL_AI_MODEL"), "{mmem}");
        let mempty = http_error_message(reqwest::StatusCode::BAD_GATEWAY, "", "m");
        assert!(mempty.contains("F5"), "{mempty}");
    }

    /// LIVE test — needs a real ollama with a real model. Off by default so CI
    /// stays hermetic; run with `SIGIL_OLLAMA_LIVE=1` (+ `SIGIL_AI_MODEL=<tag>`).
    #[test]
    fn live_ollama_roundtrip() {
        let _env = ai_model_env();
        if std::env::var("SIGIL_OLLAMA_LIVE").as_deref() != Ok("1") {
            eprintln!("skipped: set SIGIL_OLLAMA_LIVE=1 to run against a real ollama");
            return;
        }
        assert!(ollama_reachable(), "ollama must be up for the live test");
        let models = list_models();
        eprintln!("live models: {models:?}");
        assert!(!models.is_empty(), "pull a model first");
        eprintln!("live ollama version: {:?}", ollama_version());
        eprintln!("live hardware: {}", describe_hardware(detect_hardware()));
        let model = std::env::var("SIGIL_AI_MODEL").unwrap_or_else(|_| models[0].clone());
        let t0 = std::time::Instant::now();
        let reply = chat(&model, &[], "Reply with exactly the word: READY", "").expect("chat must succeed");
        eprintln!("live reply in {:?}: {reply}", t0.elapsed());
        assert!(!reply.trim().is_empty(), "reply must not be empty");
        // And the missing-model path must be actionable, not a bare status code.
        let e = chat("definitely-not-a-real-model:1b", &[], "hi", "").unwrap_err();
        eprintln!("live missing-model error: {e}");
        assert!(e.contains("not pulled"), "missing model must be diagnosed: {e}");
    }
}

#[cfg(test)]
mod oob_tests {
    use super::*;

    #[test]
    fn setup_commands_are_recognised_everywhere_a_keyboard_is_not() {
        for w in ["setup", "  SETUP ", "/setup", "install", "retry", "f5", "F5", "auto-setup"] {
            assert!(is_setup_command(w), "{w:?} must trigger setup");
        }
        for w in ["", "hello", "set", "setup please", "what is a nullifier"] {
            assert!(!is_setup_command(w), "{w:?} must NOT trigger setup");
        }
    }

    #[test]
    fn autosetup_is_on_by_default_and_off_only_when_asked() {
        let _env = crate::flux_moe::tests::ai_model_env();
        std::env::remove_var("SIGIL_AI_AUTOSETUP");
        assert!(autosetup_enabled());
        for v in ["0", "false", "NO", " off "] {
            std::env::set_var("SIGIL_AI_AUTOSETUP", v);
            assert!(!autosetup_enabled(), "{v:?} must disable");
        }
        std::env::set_var("SIGIL_AI_AUTOSETUP", "1");
        assert!(autosetup_enabled());
        std::env::remove_var("SIGIL_AI_AUTOSETUP");
    }

    #[test]
    fn termux_is_detected_from_prefix_or_version() {
        let _env = crate::flux_moe::tests::ai_model_env();
        std::env::remove_var("TERMUX_VERSION");
        let saved = std::env::var("PREFIX").ok();
        std::env::set_var("PREFIX", "/usr");
        assert!(!is_termux());
        std::env::set_var("PREFIX", "/data/data/com.termux/files/usr");
        assert!(is_termux());
        std::env::set_var("PREFIX", "/usr");
        std::env::set_var("TERMUX_VERSION", "0.118.0");
        assert!(is_termux());
        std::env::remove_var("TERMUX_VERSION");
        match saved { Some(p) => std::env::set_var("PREFIX", p), None => std::env::remove_var("PREFIX") }
    }

    #[test]
    fn small_tier_only_for_phones_and_tiny_boxes() {
        let _env = crate::flux_moe::tests::ai_model_env();
        std::env::remove_var("SIGIL_AI_MODEL");
        std::env::remove_var("TERMUX_VERSION");
        let saved = std::env::var("PREFIX").ok();
        std::env::set_var("PREFIX", "/usr");
        // 4 GB, no GPU → small.
        let tiny = Hardware { vram_bytes: None, ram_bytes: Some(4 * GIB), gpu_name: None };
        let (m, why) = pick_model_tiered("qwen3:8b", Some("qwen3:4b"), Some("qwen3:1.7b"), &tiny);
        assert_eq!(m, "qwen3:1.7b"); assert!(why.contains("RAM"), "{why}");
        // 64 GB, no GPU → NOT small; defers to pick_model (its own rules decide).
        let big = Hardware { vram_bytes: None, ram_bytes: Some(64 * GIB), gpu_name: None };
        let (m, _) = pick_model_tiered("qwen3:8b", Some("qwen3:4b"), Some("qwen3:1.7b"), &big);
        assert_ne!(m, "qwen3:1.7b");
        // No small model in the manifest → identical to pick_model.
        let (m2, _) = pick_model_tiered("qwen3:8b", Some("qwen3:4b"), None, &tiny);
        assert_eq!(m2, pick_model("qwen3:8b", Some("qwen3:4b"), &tiny).0);
        // Termux → small regardless of RAM.
        std::env::set_var("PREFIX", "/data/data/com.termux/files/usr");
        let (m3, why3) = pick_model_tiered("qwen3:8b", Some("qwen3:4b"), Some("qwen3:1.7b"), &big);
        assert_eq!(m3, "qwen3:1.7b"); assert!(why3.contains("phone"), "{why3}");
        match saved { Some(p) => std::env::set_var("PREFIX", p), None => std::env::remove_var("PREFIX") }
    }
}

#[cfg(test)]
mod stream_tests {
    use super::{parse_stream_line, StreamEv};

    #[test]
    fn thinking_and_content_chunks_become_separate_events() {
        let ev = parse_stream_line(r#"{"message":{"role":"assistant","content":"","thinking":"hmm"},"done":false}"#);
        assert!(matches!(ev.as_slice(), [StreamEv::Thinking(t)] if t == "hmm"));
        let ev = parse_stream_line(r#"{"message":{"role":"assistant","content":"Hel"},"done":false}"#);
        assert!(matches!(ev.as_slice(), [StreamEv::Token(t)] if t == "Hel"));
    }

    #[test]
    fn done_line_carries_ollamas_own_measurement() {
        let ev = parse_stream_line(r#"{"message":{"role":"assistant","content":""},"done":true,"done_reason":"stop","eval_count":120,"eval_duration":6000000000,"prompt_eval_count":40}"#);
        match ev.as_slice() {
            [StreamEv::Done { eval_count: 120, eval_ns: 6_000_000_000, prompt_count: 40, reason }] => assert_eq!(reason, "stop"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn garbage_and_blank_lines_are_ignored_not_fatal() {
        assert!(parse_stream_line("").is_empty());
        assert!(parse_stream_line("not json").is_empty());
    }
}
