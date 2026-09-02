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
/// Chat may legitimately take a while (the model is generating), so it gets a
/// generous timeout — and it runs OFF the UI thread.
fn client() -> reqwest::blocking::Client { client_with_timeout(120) }

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
    json.get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default()
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

    let body = serde_json::json!({ "model": model, "messages": messages, "stream": false });
    let url = format!("{}/api/chat", ollama_base());
    let resp = client()
        .post(&url)
        .json(&body)
        .send()
        .map_err(|e| format!("can't reach your local model — is ollama running? ({e})"))?;
    let status = resp.status();
    if !status.is_success() {
        // ollama puts the REAL reason in the body (model not found, not enough VRAM/RAM to
        // load it, etc.) — surface it so a 500 is diagnosable instead of a dead end.
        let detail = resp.text().unwrap_or_default();
        let detail = detail.trim();
        return Err(if detail.is_empty() {
            format!("ollama returned {status}")
        } else {
            let short: String = detail.chars().take(300).collect();
            format!("ollama {status} — {short}\n(if it's a memory error, pick a smaller model, e.g. `ollama pull qwen3:4b`)")
        });
    }
    let json: serde_json::Value = resp.json().map_err(|e| format!("bad model response: {e}"))?;
    json.get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(|s| s.trim().to_string())
        .ok_or_else(|| "empty response from model".to_string())
}

/// Human guidance shown when no local model is found — kept here so the tab and
/// any headless helper print the same words.
pub(crate) fn setup_hint() -> &'static str {
    "No local model yet. Press F5 and flux-moe sets itself up:\n\
     1. installs ollama from the flux-SIGNED manifest (SHA-256 + size verified before anything runs)\n\
     2. pulls the manifest's model (qwen3:8b; a smaller fallback if memory is short)\n\
     3. you chat here.\n\
     By hand instead: install ollama (ollama.com), `ollama pull qwen3:8b`, `ollama serve`, reopen this tab."
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
}
