//! flux-ai-cms — the headless AI-CMS, end-to-end on a FREE local model.
//!   flux-ai-cms "<topic>"   → qwen3.6 writes the article + tags → stored → delivered as headless JSON
use flux_ai_author::{AiAuthor, OllamaBackend};
use flux_content_api::Store;
use std::collections::BTreeMap;
use std::env;

fn slug(s: &str) -> String {
    let mut o = String::new(); let mut dash = false;
    for c in s.chars() { if c.is_ascii_alphanumeric() { o.push(c.to_ascii_lowercase()); dash = false } else if !dash && !o.is_empty() { o.push('-'); dash = true } }
    o.trim_end_matches('-').chars().take(48).collect()
}

fn main() {
    let topic = { let t = env::args().skip(1).collect::<Vec<_>>().join(" "); if t.trim().is_empty() { "Why a built-in on-prem AI is a CMS's unfair advantage".to_string() } else { t } };
    eprintln!("🤖 flux-ai-cms · generating \"{topic}\" on local qwen3.6 (free, on-prem)…");
    let ai = AiAuthor::new(OllamaBackend::default());
    let body = match ai.generate(&topic) { Ok(b) => b, Err(e) => { eprintln!("generate failed: {e}"); std::process::exit(1) } };
    eprintln!("✍️  body: {} chars", body.len());
    let tags = ai.tags(&body).unwrap_or_default();
    eprintln!("🏷️  tags: {}", tags.join(", "));
    let title: String = topic.chars().take(60).collect();
    let description: String = body.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(155).collect();
    let id = slug(&title);

    // store it via the headless content-api, published
    let mut store = Store::new();
    let mut fields = BTreeMap::new();
    fields.insert("title".into(), title);
    fields.insert("description".into(), description);
    fields.insert("tags".into(), tags.join(","));
    fields.insert("body".into(), body);
    store.put(&id, "article", fields, true);

    // headless delivery — exactly what a frontend would GET
    eprintln!("🌐 headless delivery (/content?kind=article):");
    println!("{}", store.deliver("article"));
    eprintln!("✅ AI-CMS round-trip complete · {} · $0 · {}", id, flux_ai_author::stamp().line());
}
