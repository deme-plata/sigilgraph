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

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(2))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new())
}

/// The models the local ollama has pulled, newest first. Empty ⇒ ollama not
/// running (or no models). The caller shows the "install a model" hint then.
pub(crate) fn list_models() -> Vec<String> {
    let url = format!("{}/api/tags", ollama_base());
    let resp = match client().get(&url).send() {
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
pub(crate) fn chat(model: &str, history: &[(String, String)], user: &str) -> Result<String, String> {
    let mut messages = vec![serde_json::json!({"role":"system","content":system_prompt()})];
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
    if !resp.status().is_success() {
        return Err(format!("local model returned {}", resp.status()));
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
    "No local model found. To chat here:\n\
     1. install ollama  (ollama.com)\n\
     2. pull one your GPU can run:  ollama pull qwen3:8b   (bigger GPU = bigger model = more reasoning)\n\
     3. run it:  ollama serve\n\
     Then reopen this tab and pick your model."
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
