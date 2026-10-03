//! sigil-veil-wasm — the SIGIL OS browser AI app's egress gate.
//!
//! The page lets a user point its chat at ANY model endpoint. When that endpoint
//! is not on this device, every outbound message goes through [`Veil::gate`]
//! first: recovery phrases and labelled keys are refused, identifiers become
//! placeholders, and the streamed answer is restored with [`Veil::push`] /
//! [`Veil::finish`]. One `Veil` per conversation keeps placeholders stable.

use sigil_veil::{Kind, StreamRehydrator, Verdict, VeilMap};
use wasm_bindgen::prelude::*;

fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn tags(kinds: &[Kind]) -> String {
    let v: Vec<String> = kinds.iter().map(|k| json_str(k.tag())).collect();
    format!("[{}]", v.join(","))
}

/// True when the URL's host is this device (localhost, 127.x, ::1) — then the
/// page needs no gate at all: nothing leaves.
#[wasm_bindgen]
pub fn veil_is_loopback(url: &str) -> bool {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return false; // http://localhost@evil.example is NOT local
    }
    let host = if let Some(h) = authority.strip_prefix('[') {
        h.split(']').next().unwrap_or("")
    } else {
        authority.rsplit_once(':').map(|(h, _)| h).unwrap_or(authority)
    }
    .to_ascii_lowercase();
    host == "localhost" || host.ends_with(".localhost") || host == "::1" || host.starts_with("127.")
}

#[wasm_bindgen]
pub struct Veil {
    map: VeilMap,
    answer: StreamRehydrator,
}

impl Default for Veil {
    fn default() -> Self {
        Self::new()
    }
}

#[wasm_bindgen]
impl Veil {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Veil {
        Veil { map: VeilMap::new(), answer: StreamRehydrator::new() }
    }

    /// Gate text the USER (or a tool) wrote. JSON:
    /// `{"ok":true,"text":"…","hidden":["QNK",…]}` or `{"ok":false,"refused":["MNEMONIC"]}`.
    pub fn gate(&mut self, text: &str) -> String {
        match sigil_veil::gate(text, &mut self.map) {
            Verdict::Send { text, findings } => {
                let mut k: Vec<Kind> = findings.iter().map(|(k, _)| *k).collect();
                k.sort();
                k.dedup();
                format!("{{\"ok\":true,\"text\":{},\"hidden\":{}}}", json_str(&text), tags(&k))
            }
            Verdict::Refuse { reasons } => format!("{{\"ok\":false,\"refused\":{}}}", tags(&reasons)),
        }
    }

    /// Text the user did not write (system prompt, page context): hidden, never refused.
    pub fn pseudonymize(&mut self, text: &str) -> String {
        sigil_veil::pseudonymize(text, &mut self.map).0
    }

    /// Restore placeholders in a complete answer.
    pub fn rehydrate(&self, text: &str) -> String {
        sigil_veil::rehydrate(text, &self.map)
    }

    /// Streaming answer: feed each chunk, show what comes back.
    pub fn push(&mut self, chunk: &str) -> String {
        self.answer.push(chunk, &self.map)
    }

    /// End of a streamed answer: release anything held back.
    pub fn finish(&mut self) -> String {
        self.answer.finish(&self.map)
    }

    /// How many distinct values this conversation has hidden so far.
    pub fn hidden_count(&self) -> usize {
        self.map.len()
    }

    /// One egress-ledger JSON line for what was actually sent (the page keeps
    /// these in its own storage — "what did my AI send out?").
    pub fn ledger_line(&self, destination: &str, sent: &str) -> String {
        sigil_veil::LedgerEntry::new(destination, sent, &[]).to_json_line()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_detection_matches_sigil_top() {
        for ok in ["http://localhost:11434", "http://127.0.0.1:11444/", "http://[::1]:11434", "http://ai.localhost"] {
            assert!(veil_is_loopback(ok), "{ok}");
        }
        for bad in ["http://gpu.example:11434", "http://10.0.0.5:11434", "http://localhost@evil.example", "https://api.deepseek.com"] {
            assert!(!veil_is_loopback(bad), "{bad}");
        }
    }

    #[test]
    fn gate_json_shapes() {
        let mut v = Veil::new();
        let ok = v.gate("send to qnk4973498a9865b291636faef205f728a49d98890f001e9e806479043f038ebf6c");
        assert_eq!(ok, "{\"ok\":true,\"text\":\"send to [QNK_1]\",\"hidden\":[\"QNK\"]}");
        let no = v.gate("abandon ability able about above absent absorb abstract absurd abuse access accident");
        assert_eq!(no, "{\"ok\":false,\"refused\":[\"MNEMONIC\"]}");
        assert_eq!(v.push("ok [QN"), "ok ");
        assert!(v.push("K_1].").starts_with("qnk4973"));
        assert_eq!(v.hidden_count(), 1);
    }
}
