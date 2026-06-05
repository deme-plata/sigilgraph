//! book_analyze — fluxc-native structural analysis of "Shadows in the Chain".
//!
//! Reads every `assets/chapterN_content_improved.tex`, computes per-chapter
//! structural metrics (no LLM — pure deterministic counts), and prints a table +
//! a cross-chapter arc. The DeepSeek-16B agents do the *literary* scoring on top
//! of this; this binary is the FLUXFOOD-fast factual substrate they reason over.
//!
//!   fluxc run --bin book-analyze        (or the built binary)

use std::fs;
use std::path::PathBuf;

const TECH_TERMS: &[&str] = &[
    "state root", "blockchain", "quantum", "zero-knowledge", "zk", "proof",
    "ledger", "hash", "mesh", "node", "BTC", "bitcoin", "crypto", "wallet",
    "signature", "consensus", "SIGIL", "transaction", "key", "entropy",
];

struct ChapterMetrics {
    n: u32,
    words: usize,
    sentences: usize,
    dialogue_lines: usize,
    scenes: usize,
    tech_hits: usize,
}

impl ChapterMetrics {
    fn avg_sentence_len(&self) -> f64 {
        if self.sentences == 0 { 0.0 } else { self.words as f64 / self.sentences as f64 }
    }
    /// tech-term density per 1000 words — the axis the 72B flagged weakest.
    fn tech_density(&self) -> f64 {
        if self.words == 0 { 0.0 } else { self.tech_hits as f64 * 1000.0 / self.words as f64 }
    }
    fn dialogue_ratio(&self) -> f64 {
        // rough: dialogue lines / scenes-normalized; report as % of paragraphs w/ quotes
        if self.words == 0 { 0.0 } else { self.dialogue_lines as f64 }
    }
}

fn analyze(n: u32, body: &str) -> ChapterMetrics {
    // strip the most common LaTeX commands to count prose, not markup.
    let prose: String = body
        .lines()
        .filter(|l| !l.trim_start().starts_with('\\') || l.contains(' '))
        .collect::<Vec<_>>()
        .join("\n");
    let words = prose.split_whitespace().filter(|w| w.chars().any(|c| c.is_alphabetic())).count();
    let sentences = prose.matches(['.', '!', '?']).count().max(1);
    let dialogue_lines = body.lines().filter(|l| l.contains('“') || l.contains('"') || l.contains("``")).count();
    let scenes = body.matches("\\subsection").count() + body.matches("Scene").count();
    let low = body.to_lowercase();
    let tech_hits = TECH_TERMS.iter().map(|t| low.matches(&t.to_lowercase()).count()).sum();
    ChapterMetrics { n, words, sentences, dialogue_lines, scenes, tech_hits }
}

fn main() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets");
    let mut chapters: Vec<ChapterMetrics> = Vec::new();
    for n in 1..=26u32 {
        let p = dir.join(format!("chapter{n}_content_improved.tex"));
        if let Ok(body) = fs::read_to_string(&p) {
            chapters.push(analyze(n, &body));
        }
    }
    if chapters.is_empty() {
        eprintln!("no chapters found in {}", dir.display());
        std::process::exit(1);
    }

    println!("📖 SHADOWS IN THE CHAIN — structural analysis ({} chapters)\n", chapters.len());
    println!("{:>3} {:>7} {:>6} {:>9} {:>7} {:>9}", "ch", "words", "scenes", "avg-sent", "dialog", "tech/1k");
    println!("{}", "─".repeat(48));
    let (mut tw, mut tt) = (0usize, 0usize);
    for c in &chapters {
        println!(
            "{:>3} {:>7} {:>6} {:>9.1} {:>7} {:>9.1}",
            c.n, c.words, c.scenes, c.avg_sentence_len(), c.dialogue_lines, c.tech_density()
        );
        tw += c.words;
        tt += c.tech_hits;
    }
    println!("{}", "─".repeat(48));
    let avg_words = tw / chapters.len();
    println!("\nTOTAL {} words · avg {} words/chapter · {} tech-term hits", tw, avg_words, tt);

    // arc flags — the cross-chapter signal the 72B couldn't see chapter-by-chapter
    let thinnest = chapters.iter().min_by_key(|c| c.words).unwrap();
    let densest = chapters.iter().max_by(|a, b| a.tech_density().partial_cmp(&b.tech_density()).unwrap()).unwrap();
    let sparsest = chapters.iter().min_by(|a, b| a.tech_density().partial_cmp(&b.tech_density()).unwrap()).unwrap();
    println!("\nARC FLAGS:");
    println!("  • thinnest chapter: ch{} ({} words) — pacing dip to check", thinnest.n, thinnest.words);
    println!("  • most tech-dense:  ch{} ({:.1}/1k)", densest.n, densest.tech_density());
    println!("  • most tech-SPARSE: ch{} ({:.1}/1k) — the 72B's weak-TECH axis likely worst here", sparsest.n, sparsest.tech_density());
    // emit machine-readable line for the DeepSeek agents to anchor on
    println!("\nJSON {{\"chapters\":{},\"total_words\":{},\"tech_hits\":{}}}", chapters.len(), tw, tt);
}
