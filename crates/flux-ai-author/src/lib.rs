//! flux-ai-author — Flux's killer CMS feature: **built-in, FREE, on-prem AI authoring.**
//!
//! Content generation, auto-tagging and SEO-meta, driven by a LOCAL ollama model (qwen3.6) — no API
//! key, no per-token cost, no data leaving the box. No mainstream CMS ships a free local AI; this is
//! the unfair advantage. The model is behind an [`LlmBackend`] trait, so tests run offline with
//! [`MockBackend`] and production uses [`OllamaBackend`] (std-only HTTP to 127.0.0.1:11434).

use serde::{Deserialize, Serialize};

/// Pluggable LLM backend. Default: [`OllamaBackend`] (local qwen3.6). Tests use [`MockBackend`].
pub trait LlmBackend {
    /// Run a single-shot completion; returns the model's text.
    fn complete(&self, prompt: &str) -> Result<String, String>;
}

/// Deterministic offline backend for tests — echoes a canned shape so logic is testable without a model.
pub struct MockBackend(pub String);
impl LlmBackend for MockBackend {
    fn complete(&self, _prompt: &str) -> Result<String, String> { Ok(self.0.clone()) }
}

/// Real backend: talks to a local ollama server over std TCP (no external HTTP crate).
pub struct OllamaBackend {
    pub host: String,
    pub port: u16,
    pub model: String,
}
impl Default for OllamaBackend {
    fn default() -> Self { Self { host: "127.0.0.1".into(), port: 11434, model: "qwen3.6".into() } }
}
impl LlmBackend for OllamaBackend {
    fn complete(&self, prompt: &str) -> Result<String, String> {
        use std::io::{Read, Write};
        let body = serde_json::json!({
            "model": self.model, "think": false, "stream": false,
            "messages": [{"role": "user", "content": prompt}]
        }).to_string();
        // HTTP/1.0 + Connection: close → plain (un-chunked) body
        let req = format!(
            "POST /api/chat HTTP/1.0\r\nHost: {}:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            self.host, self.port, body.len(), body);
        let mut s = std::net::TcpStream::connect((self.host.as_str(), self.port)).map_err(|e| e.to_string())?;
        let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(150)));
        s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
        let mut resp = String::new();
        s.read_to_string(&mut resp).map_err(|e| e.to_string())?;
        let payload = resp.splitn(2, "\r\n\r\n").nth(1).ok_or("no http body")?;
        let v: serde_json::Value = serde_json::from_str(payload).map_err(|e| e.to_string())?;
        let mut txt = v.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_str()).unwrap_or("").to_string();
        // strip any <think>…</think> a reasoning model might emit
        while let (Some(a), Some(b)) = (txt.find("<think>"), txt.find("</think>")) {
            if b > a { txt.replace_range(a..b + 8, ""); } else { break; }
        }
        Ok(txt.trim().to_string())
    }
}

/// SEO metadata the author produces for a piece of content.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Meta {
    pub title: String,
    pub description: String,
    pub tags: Vec<String>,
}

/// The AI author — wraps a backend and exposes CMS authoring verbs.
pub struct AiAuthor<B: LlmBackend> {
    pub llm: B,
}

fn clean_tags(raw: &str) -> Vec<String> {
    raw.split(|c| c == ',' || c == '\n' || c == ';')
        .map(|t| t.trim().trim_start_matches('#').trim().to_lowercase())
        .map(|t| t.split_whitespace().collect::<Vec<_>>().join("-"))
        .filter(|t| !t.is_empty() && t.len() <= 30)
        .take(8)
        .collect()
}

impl<B: LlmBackend> AiAuthor<B> {
    pub fn new(llm: B) -> Self { Self { llm } }

    /// Draft a body for a topic.
    pub fn generate(&self, topic: &str) -> Result<String, String> {
        self.llm.complete(&format!(
            "Write a concise, engaging CMS article body (120-200 words, no preamble) about: {topic}"))
    }

    /// One-paragraph summary of existing content.
    pub fn summarize(&self, text: &str) -> Result<String, String> {
        self.llm.complete(&format!("Summarize this in ONE sentence (max 30 words), no preamble:\n{text}"))
    }

    /// Auto-tags for content (lowercased, hyphenated, ≤8).
    pub fn tags(&self, text: &str) -> Result<Vec<String>, String> {
        let raw = self.llm.complete(&format!(
            "Return 4-6 lowercase topic tags for this content as a comma-separated list, tags only, no prose:\n{text}"))?;
        Ok(clean_tags(&raw))
    }

    /// Full SEO meta (title + description + tags) in one go.
    pub fn meta(&self, title_hint: &str, body: &str) -> Result<Meta, String> {
        let title = self.llm.complete(&format!("Write ONE SEO title (max 60 chars), no quotes, for: {title_hint}"))?
            .lines().next().unwrap_or(title_hint).trim().chars().take(60).collect::<String>();
        let description = self.llm.complete(&format!("Write ONE SEO meta description (max 155 chars), no quotes, for:\n{body}"))?
            .lines().next().unwrap_or("").trim().chars().take(155).collect::<String>();
        let tags = self.tags(body)?;
        Ok(Meta { title, description, tags })
    }
}

/// Genesis provenance stamp for this build.
pub fn stamp() -> flux_stamp::Stamp { flux_stamp::flux_stamp!() }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_uses_backend() {
        let a = AiAuthor::new(MockBackend("A drafted article body.".into()));
        assert_eq!(a.generate("flux").unwrap(), "A drafted article body.");
    }

    #[test]
    fn tags_are_cleaned() {
        let a = AiAuthor::new(MockBackend("Flux, #Rust , agentic money, On-Chain, ".into()));
        let t = a.tags("x").unwrap();
        assert!(t.contains(&"flux".to_string()));
        assert!(t.contains(&"rust".to_string()));
        assert!(t.contains(&"on-chain".to_string()));
        assert!(t.iter().all(|x| x == &x.to_lowercase() && !x.is_empty()));
    }

    #[test]
    fn meta_truncates_and_tags() {
        let long = "x".repeat(300);
        let a = AiAuthor::new(MockBackend(long));
        let m = a.meta("a very long title hint", "body text about flux and sigil").unwrap();
        assert!(m.title.len() <= 60);
        assert!(m.description.len() <= 155);
        // tags from a single 300-char blob are correctly dropped (len>30 filter) → empty is valid here
        assert!(m.tags.iter().all(|t| t.len() <= 30));
    }

    #[test]
    fn think_tags_stripped_shape() {
        // MockBackend can't exercise OllamaBackend's network path, but the clean_tags + truncation
        // logic above is the testable core; this guards the tag cap.
        let a = AiAuthor::new(MockBackend("one,two,three,four,five,six,seven,eight,nine,ten".into()));
        assert!(a.tags("x").unwrap().len() <= 8);
    }
}
