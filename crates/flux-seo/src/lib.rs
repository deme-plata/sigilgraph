//! flux-seo — SEF URLs + meta + sitemap (4SEO ⊕ Pathauto+Metatag).
pub fn slugify(title: &str) -> String {
    let mut out = String::new(); let mut dash = false;
    for c in title.chars() { if c.is_ascii_alphanumeric() { out.push(c.to_ascii_lowercase()); dash = false } else if !dash && !out.is_empty() { out.push('-'); dash = true } }
    out.trim_end_matches('-').to_string()
}
pub struct Meta { pub title: String, pub description: String }
impl Meta { pub fn tags(&self) -> String { format!("<title>{}</title><meta name=\"description\" content=\"{}\">", self.title, self.description) } }
pub fn sitemap_entry(loc: &str, prio: f32) -> String { format!("<url><loc>{}</loc><priority>{:.1}</priority></url>", loc, prio) }

/// Genesis provenance stamp for this build.
pub fn stamp() -> flux_stamp::Stamp { flux_stamp::flux_stamp!() }

#[cfg(test)]
mod tests { use super::*;
 #[test] fn slug() { assert_eq!(slugify("Hello, World! 2026"), "hello-world-2026"); }
 #[test] fn sitemap() { assert!(sitemap_entry("/x", 0.8).contains("<loc>/x</loc>")); }
}