//! template-forge CLI — auto-propose project templates.
//!
//!   template-forge                 # summary of the curated catalog
//!   template-forge json            # full catalog.json to stdout (UI feed)
//!   template-forge propose [N]     # propose N templates (summary)
//!   template-forge --out <DIR>     # save catalog.json + per-template JSON

use template_forge::{catalog, next_suggestions, propose, save_local};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.first().map(|s| s.as_str()) == Some("json") {
        println!("{}", serde_json::to_string_pretty(&catalog()).unwrap());
        return;
    }

    if let Some(i) = args.iter().position(|a| a == "--out") {
        let dir = args.get(i + 1).cloned().unwrap_or_else(|| ".".into());
        match save_local(&dir) {
            Ok(p) => println!("📁 catalog written: {}", p.display()),
            Err(e) => {
                eprintln!("save failed: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let templates = if args.first().map(|s| s.as_str()) == Some("propose") {
        let n = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(4);
        propose(n)
    } else {
        catalog().templates
    };

    println!("✨ {} template(s) proposed — each ~50 features:\n", templates.len());
    for t in &templates {
        println!("{} {} ({})  —  {}", t.icon, t.name, t.kind, t.tagline);
        println!("   {} features · {} sigil tasks", t.feature_count(), t.sigil_tasks.len());
        // a taste of the advisor on a fresh (nothing-built) project
        if let Some(line) = next_suggestions(t, &[]).into_iter().nth(1) {
            println!("   next: {}", line);
        }
        println!();
    }
}
