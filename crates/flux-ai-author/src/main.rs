//! flux-ai-author CLI — drives the LOCAL qwen3.6 (free, on-prem) for CMS authoring.
//!   flux-ai-author generate "<topic>"   draft a body
//!   flux-ai-author tags "<text>"          auto-tags
//!   flux-ai-author summarize "<text>"     one-line summary
//!   flux-ai-author meta "<text>"          SEO title+description+tags (JSON)
//!   flux-ai-author version                genesis-stamped build line
use flux_ai_author::{AiAuthor, OllamaBackend};
use std::env;

fn main() {
    let args: Vec<String> = env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("version");
    let arg = args.get(2).cloned().unwrap_or_default();
    let a = AiAuthor::new(OllamaBackend::default());
    let out = |r: Result<String, String>| match r { Ok(s) => println!("{s}"), Err(e) => { eprintln!("err: {e}"); std::process::exit(1) } };
    match cmd {
        "version" | "--version" => println!("{}", flux_ai_author::stamp().line()),
        "generate" => out(a.generate(&arg)),
        "summarize" => out(a.summarize(&arg)),
        "tags" => match a.tags(&arg) { Ok(t) => println!("{}", t.join(", ")), Err(e) => { eprintln!("err: {e}"); std::process::exit(1) } },
        "meta" => match a.meta(&arg, &arg) { Ok(m) => println!("{}", serde_json::to_string_pretty(&m).unwrap_or_default()), Err(e) => { eprintln!("err: {e}"); std::process::exit(1) } },
        _ => println!("usage: flux-ai-author <generate|tags|summarize|meta|version> \"<text>\""),
    }
}
