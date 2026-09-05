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
pub(crate) fn ollama_base() -> String {
    std::env::var("SIGIL_OLLAMA")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http://localhost:11434".to_string())
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
fn client() -> reqwest::blocking::Client { client_with_timeout(chat_timeout_secs()) }

/// The models the local ollama has pulled, newest first. Empty ⇒ ollama not
/// running (or no models). The caller shows the "install a model" hint then.
pub(crate) fn list_models() -> Vec<String> {
    let url = format!("{}/api/tags", ollama_base());
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
/// * **No GPU → the smaller model.** Not a capacity limit (this box has 44 GB
///   free and would happily *load* 8b) but a SPEED one: measured on 48 CPU cores,
///   `qwen3:4b` decodes at 7.0 tok/s — ~43 s for one short answer. `qwen3:8b` has
///   ~2x the parameters and is memory-bandwidth bound, so it lands near 1.5 min
///   per turn before the cold 5 GB load. An [A]I tab that takes minutes per turn
///   is a dead end with extra steps.
/// * Anything unmeasurable → the manifest default, never a silent downgrade.
pub(crate) fn pick_model(default_model: &str, fallback_model: Option<&str>, hw: &Hardware) -> (String, String) {
    if let Some(forced) = std::env::var("SIGIL_AI_MODEL").ok().filter(|s| !s.trim().is_empty()) {
        let forced = forced.trim().to_string();
        return (forced.clone(), format!("SIGIL_AI_MODEL={forced} (your explicit choice)"));
    }
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
            let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
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
fn system_prompt() -> &'static str {
    "You are flux-moe, the on-device AI inside the SIGIL sigil-top node. Be concise, warm and \
     honest. You help with SIGIL: mining, the wallet, the Nation welfare stipend, the DEX, the \
     Polygon bridge, and running the node. For anything that needs a LIVE number or a money \
     action (balance, height, supply, mining stats, a send), say which node command shows it \
     rather than guessing — never invent balances, prices, hashrates or amounts. When unsure, \
     say so plainly."
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
    let send = |b: &serde_json::Value| client().post(&url).json(b).send();

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
        Ok(format!("(no final answer — {advice})\n\nIts reasoning so far:\n{peek}"))
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
    !matches!(
        std::env::var("SIGIL_AI_AUTOSETUP").ok().as_deref().map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("0") | Some("false") | Some("no") | Some("off")
    )
}

/// Running inside Termux on Android. Termux sets `TERMUX_VERSION` and its `PREFIX` is
/// `/data/data/com.termux/files/usr`; either is proof enough. Matters twice: ollama is
/// installed with `pkg`, not from a glibc tarball (Android's libc is bionic), and a phone
/// wants the small model tier.
pub(crate) fn is_termux() -> bool {
    std::env::var_os("TERMUX_VERSION").is_some()
        || std::env::var("PREFIX").map(|p| p.contains("com.termux")).unwrap_or(false)
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
mod tests {
    use super::*;

    #[test]
    fn ollama_base_defaults_to_localhost() {
        // Without the override, it targets the user's own machine.
        std::env::remove_var("SIGIL_OLLAMA");
        assert_eq!(ollama_base(), "http://localhost:11434");
    }

    #[test]
    fn system_prompt_forbids_inventing_numbers() {
        let p = system_prompt();
        assert!(p.contains("never invent"));
        assert!(p.to_lowercase().contains("sigil"));
    }

    #[test]
    fn no_model_when_ollama_down() {
        // Point at a closed port; list must be empty, not panic.
        std::env::set_var("SIGIL_OLLAMA", "http://127.0.0.1:1");
        assert!(list_models().is_empty());
        assert!(!available());
        std::env::remove_var("SIGIL_OLLAMA");
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

    #[test]
    fn cpu_only_box_gets_the_smaller_model() {
        std::env::remove_var("SIGIL_AI_MODEL");
        // This is EXACTLY the measured Epsilon shape: no GPU, plenty of RAM.
        let hw = Hardware { vram_bytes: None, ram_bytes: Some(62 * GIB), gpu_name: None };
        let (m, why) = pick_model("qwen3:8b", Some("qwen3:4b"), &hw);
        assert_eq!(m, "qwen3:4b", "no GPU ⇒ smaller model, for SPEED not capacity");
        assert!(why.contains("no GPU"), "the reason must be shown: {why}");
    }

    #[test]
    fn big_gpu_gets_the_big_model() {
        std::env::remove_var("SIGIL_AI_MODEL");
        let hw = Hardware { vram_bytes: Some(24 * GIB), ram_bytes: Some(64 * GIB), gpu_name: Some("RTX 4090".into()) };
        assert_eq!(pick_model("qwen3:8b", Some("qwen3:4b"), &hw).0, "qwen3:8b");
        // A 6 GB card cannot hold an 8b model (~6.2 GB) plus KV cache.
        let small = Hardware { vram_bytes: Some(6 * GIB), ram_bytes: Some(16 * GIB), gpu_name: Some("RTX 2060".into()) };
        assert_eq!(pick_model("qwen3:8b", Some("qwen3:4b"), &small).0, "qwen3:4b");
    }

    #[test]
    fn no_fallback_never_invents_one() {
        std::env::remove_var("SIGIL_AI_MODEL");
        let hw = Hardware { vram_bytes: None, ram_bytes: Some(8 * GIB), gpu_name: None };
        let (m, why) = pick_model("qwen3:8b", None, &hw);
        assert_eq!(m, "qwen3:8b");
        assert!(why.contains("no smaller fallback"));
    }

    #[test]
    fn explicit_choice_always_wins() {
        std::env::set_var("SIGIL_AI_MODEL", "llama3:70b");
        let hw = Hardware { vram_bytes: None, ram_bytes: Some(4 * GIB), gpu_name: None };
        assert_eq!(pick_model("qwen3:8b", Some("qwen3:4b"), &hw).0, "llama3:70b");
        std::env::remove_var("SIGIL_AI_MODEL");
    }

    #[test]
    fn every_error_message_names_a_next_action() {
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
