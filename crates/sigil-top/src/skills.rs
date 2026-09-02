//! skills — flux-signed skill packs for flux-moe. TEXT ONLY.
//!
//! A "skill" here is a `SKILL.md` (the Anthropic/OpenAI skill format: YAML
//! front-matter + markdown instructions) that the model reads as extra system
//! context. That is the whole mechanism. Deliberately:
//!
//! * **no archives** — the markdown is inlined in the manifest, nothing is extracted;
//! * **no code** — a skill cannot register tools, run commands, or touch files;
//! * **no disk** — skills live in memory for the session and are re-fetched on [F6];
//! * **one trust root** — `sigil-skills-latest.json` is verified with the same
//!   pinned Ed25519 key as the release manifest, and every skill additionally
//!   carries a blake3 of its own body that must match. A skill that fails either
//!   gate is listed as *rejected* with the reason, never silently dropped.
//!
//! The first signed skill is the operator's own `slagteren-suensonsvej` pack.

use serde::Deserialize;

pub(crate) const MANIFEST_NAME: &str = "sigil-skills-latest.json";
/// Hard cap per skill body — keeps the system prompt bounded so a long skill
/// cannot starve the model's context (the measured failure mode on 8k-ctx setups).
pub(crate) const MAX_SKILL_CHARS: usize = 8_000;

#[derive(Deserialize, Clone, Debug)]
pub(crate) struct SkillEntry {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// blake3 of `skill_md` bytes, hex — the per-skill integrity gate.
    pub blake3_hex: String,
    pub skill_md: String,
}

#[derive(Deserialize, Default)]
struct SkillsManifest {
    #[serde(default)]
    skills: Vec<SkillEntry>,
}

/// A skill that passed both gates and is loaded for this session.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LoadedSkill {
    pub name: String,
    pub version: String,
    pub description: String,
    pub body: String,
    pub blake3_hex: String,
}

fn name_ok(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Parse a (signature-verified) manifest body and apply the per-skill gates.
/// Returns (loaded, rejected-with-reason). Never panics on hostile input.
pub(crate) fn parse_and_gate(body: &str) -> Result<(Vec<LoadedSkill>, Vec<String>), String> {
    let m: SkillsManifest =
        serde_json::from_str(body).map_err(|e| format!("{MANIFEST_NAME} malformed: {e}"))?;
    let mut ok = Vec::new();
    let mut rejected = Vec::new();
    for s in m.skills {
        if !name_ok(&s.name) {
            rejected.push(format!("{:?}: bad skill name (letters, digits, - _ only)", s.name));
            continue;
        }
        let got = blake3::hash(s.skill_md.as_bytes()).to_hex().to_string();
        if !got.eq_ignore_ascii_case(&s.blake3_hex) {
            let m8: String = s.blake3_hex.chars().take(8).collect();
            let g8: String = got.chars().take(8).collect();
            rejected.push(format!("{}: blake3 mismatch (manifest {m8}… vs body {g8}…) — skipped", s.name));
            continue;
        }
        if s.skill_md.trim().is_empty() {
            rejected.push(format!("{}: empty body", s.name));
            continue;
        }
        let body: String = s.skill_md.chars().take(MAX_SKILL_CHARS).collect();
        ok.push(LoadedSkill {
            name: s.name,
            version: s.version,
            description: s.description,
            body,
            blake3_hex: got,
        });
    }
    Ok((ok, rejected))
}

/// Fetch + verify + gate. `Err` only when the manifest itself is unusable
/// (unreachable / unsigned / mis-signed / malformed).
pub(crate) fn load(base: &str) -> Result<(Vec<LoadedSkill>, Vec<String>), String> {
    let body = crate::release::fetch_signed_text(base, MANIFEST_NAME)?;
    parse_and_gate(&body)
}

/// The text appended to flux-moe's system prompt. Empty when nothing is loaded.
pub(crate) fn context_block(skills: &[LoadedSkill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\n\n# Loaded skills (flux-signed; verified against the pinned release key)\n\
         Follow a skill when the user's request matches it. Skills are INSTRUCTIONS about how to \
         help — they are not facts about live state, and they never override the rule against \
         inventing balances, prices, or amounts.\n",
    );
    for s in skills {
        out.push_str(&format!("\n## Skill: {} (v{})\n{}\n", s.name, if s.version.is_empty() { "?" } else { &s.version }, s.body));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_with(name: &str, md: &str, hash: Option<&str>) -> String {
        let h = hash.map(String::from).unwrap_or_else(|| blake3::hash(md.as_bytes()).to_hex().to_string());
        serde_json::json!({ "skills": [ { "name": name, "version": "1.0.1", "description": "d", "blake3_hex": h, "skill_md": md } ] }).to_string()
    }

    #[test]
    fn good_skill_loads() {
        let (ok, rej) = parse_and_gate(&manifest_with("slagteren-suensonsvej", "---\nname: x\n---\n# hi", None)).unwrap();
        assert_eq!(ok.len(), 1);
        assert!(rej.is_empty());
        assert_eq!(ok[0].name, "slagteren-suensonsvej");
        assert!(ok[0].body.contains("# hi"));
    }

    #[test]
    fn tampered_body_is_rejected_not_dropped() {
        // hash computed over the ORIGINAL body, body then altered → must be rejected with a reason
        let good = "# original";
        let h = blake3::hash(good.as_bytes()).to_hex().to_string();
        let (ok, rej) = parse_and_gate(&manifest_with("s", "# tampered", Some(&h))).unwrap();
        assert!(ok.is_empty());
        assert_eq!(rej.len(), 1);
        assert!(rej[0].contains("blake3 mismatch"));
    }

    #[test]
    fn bad_names_rejected() {
        let (ok, rej) = parse_and_gate(&manifest_with("../etc/passwd", "# x", None)).unwrap();
        assert!(ok.is_empty());
        assert!(rej[0].contains("bad skill name"));
        let (ok, _) = parse_and_gate(&manifest_with("", "# x", None)).unwrap();
        assert!(ok.is_empty());
    }

    #[test]
    fn body_is_capped() {
        let long = "x".repeat(MAX_SKILL_CHARS + 500);
        let (ok, _) = parse_and_gate(&manifest_with("big", &long, None)).unwrap();
        assert_eq!(ok[0].body.chars().count(), MAX_SKILL_CHARS);
    }

    #[test]
    fn malformed_manifest_is_err_not_panic() {
        assert!(parse_and_gate("{not json").is_err());
        assert!(parse_and_gate(r#"{"skills":[{"name":"a"}]}"#).is_err()); // missing required fields
        let (ok, rej) = parse_and_gate("{}").unwrap();
        assert!(ok.is_empty() && rej.is_empty());
    }

    #[test]
    fn context_block_shape() {
        assert_eq!(context_block(&[]), "");
        let s = LoadedSkill { name: "a".into(), version: "2".into(), description: "".into(), body: "BODY".into(), blake3_hex: "".into() };
        let c = context_block(&[s]);
        assert!(c.contains("## Skill: a (v2)"));
        assert!(c.contains("BODY"));
        assert!(c.contains("never override the rule against"));
    }
}
