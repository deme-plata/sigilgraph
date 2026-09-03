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
//! ## Who a skill is FOR (the audience gate)
//!
//! A manifest is one file served to every install, but not every skill belongs
//! to every user. The operator's `slagteren-suensonsvej` butcher-shop pack is
//! useful to exactly one person and is noise — or worse, a privacy leak — in
//! everyone else's model context. So each entry carries an `audience`:
//!
//! * `public`   — the default. Loaded everywhere. Must be about SIGIL itself.
//! * `operator` — loaded ONLY on an install that opted in by name.
//!
//! Opt-in is local and explicit: `SIGIL_SKILLS=slagteren-suensonsvej,other`
//! (env), or a `skills` line in the sigil-top config. There is no server-side
//! targeting — the client decides, so nothing about who you are leaves the box.
//!
//! A skill that is simply not for you is **withheld**, not **rejected**. They
//! are different events and are reported differently: withheld is routine,
//! rejected means an integrity gate failed and someone should look at it.

use serde::Deserialize;

pub(crate) const MANIFEST_NAME: &str = "sigil-skills-latest.json";
/// Hard cap per skill body — keeps the system prompt bounded so a long skill
/// cannot starve the model's context (the measured failure mode on 8k-ctx setups).
pub(crate) const MAX_SKILL_CHARS: usize = 8_000;
/// Hard cap across ALL loaded skills. Per-skill capping alone does not bound the
/// system prompt — ten 8k skills is an 80k prompt, which silently truncates the
/// user's own question on a small-context model. Skills past the budget are
/// withheld with a reason, in manifest order, so the outcome is deterministic.
pub(crate) const MAX_SKILLS_TOTAL_CHARS: usize = 24_000;

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
    /// Who this skill is for. Absent or `"public"` = everyone. Anything else
    /// (by convention `"operator"`) requires a local opt-in naming the skill.
    #[serde(default)]
    pub audience: String,
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
    /// `"public"` or the audience it was opted into with.
    pub audience: String,
}

/// The outcome of one manifest load. Three lists, because three different
/// things can happen to a skill and collapsing them loses the meaning:
/// it loaded, it is not for this install, or it failed an integrity gate.
#[derive(Clone, Debug, Default)]
pub(crate) struct SkillLoad {
    pub loaded: Vec<LoadedSkill>,
    /// Not for this install (audience or context budget). Routine.
    pub withheld: Vec<String>,
    /// Failed a gate — bad name, blake3 mismatch, empty body. Worth reading.
    pub rejected: Vec<String>,
}

impl SkillLoad {
    /// One line for the AI tab status bar.
    pub fn note(&self) -> String {
        let mut s = format!("{} flux-signed", self.loaded.len());
        if !self.withheld.is_empty() {
            s.push_str(&format!(" · {} not for this install", self.withheld.len()));
        }
        if !self.rejected.is_empty() {
            s.push_str(&format!(" · {} REJECTED", self.rejected.len()));
        }
        s
    }
}

/// Skills this install has explicitly opted into, lowercased.
///
/// Source of truth is the local machine only — `SIGIL_SKILLS` (comma or space
/// separated). `SIGIL_SKILLS=all` takes everything the manifest offers, which
/// is the operator's own convenience and is deliberately not the default.
pub(crate) fn local_opt_ins() -> Vec<String> {
    std::env::var("SIGIL_SKILLS")
        .unwrap_or_default()
        .split([',', ' ', ';'])
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Is `audience` open to everyone?
fn is_public(audience: &str) -> bool {
    let a = audience.trim();
    a.is_empty() || a.eq_ignore_ascii_case("public") || a.eq_ignore_ascii_case("all")
}

fn name_ok(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Parse a (signature-verified) manifest body and apply every gate:
/// name → blake3 → non-empty → audience → context budget.
/// Never panics on hostile input.
pub(crate) fn parse_and_gate(body: &str, opt_ins: &[String]) -> Result<SkillLoad, String> {
    let m: SkillsManifest =
        serde_json::from_str(body).map_err(|e| format!("{MANIFEST_NAME} malformed: {e}"))?;
    // Compare case-insensitively HERE rather than trusting the caller to have
    // normalized: a gate that silently fails closed on "SIGIL_SKILLS=Slagteren"
    // looks exactly like the skill being missing, which is unfixable by the user.
    let take_all = opt_ins.iter().any(|o| o.trim().eq_ignore_ascii_case("all"));
    let mut out = SkillLoad::default();
    let mut budget = MAX_SKILLS_TOTAL_CHARS;
    for s in m.skills {
        if !name_ok(&s.name) {
            out.rejected.push(format!("{:?}: bad skill name (letters, digits, - _ only)", s.name));
            continue;
        }
        let got = blake3::hash(s.skill_md.as_bytes()).to_hex().to_string();
        if !got.eq_ignore_ascii_case(&s.blake3_hex) {
            let m8: String = s.blake3_hex.chars().take(8).collect();
            let g8: String = got.chars().take(8).collect();
            out.rejected.push(format!("{}: blake3 mismatch (manifest {m8}… vs body {g8}…) — skipped", s.name));
            continue;
        }
        if s.skill_md.trim().is_empty() {
            out.rejected.push(format!("{}: empty body", s.name));
            continue;
        }
        // Audience: public loads everywhere; anything else needs a local opt-in
        // naming this skill (or SIGIL_SKILLS=all). The check is client-side, so
        // no request ever tells the server which install asked.
        if !is_public(&s.audience) {
            if !take_all && !opt_ins.iter().any(|o| o.trim().eq_ignore_ascii_case(&s.name)) {
                out.withheld.push(format!(
                    "{} (audience: {}) — enable with SIGIL_SKILLS={}",
                    s.name, s.audience.trim(), s.name
                ));
                continue;
            }
        }
        let body: String = s.skill_md.chars().take(MAX_SKILL_CHARS).collect();
        let cost = body.chars().count();
        if cost > budget {
            out.withheld.push(format!(
                "{}: skipped — context budget spent ({MAX_SKILLS_TOTAL_CHARS} chars)", s.name
            ));
            continue;
        }
        budget -= cost;
        let audience = if is_public(&s.audience) { "public".to_string() } else { s.audience.trim().to_string() };
        out.loaded.push(LoadedSkill {
            name: s.name,
            version: s.version,
            description: s.description,
            body,
            blake3_hex: got,
            audience,
        });
    }
    Ok(out)
}

/// Fetch + verify + gate. `Err` only when the manifest itself is unusable
/// (unreachable / unsigned / mis-signed / malformed).
pub(crate) fn load(base: &str) -> Result<SkillLoad, String> {
    let body = crate::release::fetch_signed_text(base, MANIFEST_NAME)?;
    parse_and_gate(&body, &local_opt_ins())
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

    fn none() -> Vec<String> { Vec::new() }

    fn entry(name: &str, md: &str, hash: Option<&str>, audience: Option<&str>) -> serde_json::Value {
        let h = hash.map(String::from).unwrap_or_else(|| blake3::hash(md.as_bytes()).to_hex().to_string());
        let mut v = serde_json::json!({ "name": name, "version": "1.0.1", "description": "d", "blake3_hex": h, "skill_md": md });
        if let Some(a) = audience { v["audience"] = serde_json::json!(a); }
        v
    }
    fn manifest(entries: Vec<serde_json::Value>) -> String {
        serde_json::json!({ "skills": entries }).to_string()
    }
    fn manifest_with(name: &str, md: &str, hash: Option<&str>) -> String {
        manifest(vec![entry(name, md, hash, None)])
    }

    #[test]
    fn good_skill_loads() {
        let r = parse_and_gate(&manifest_with("sigil-trading", "---\nname: x\n---\n# hi", None), &none()).unwrap();
        assert_eq!(r.loaded.len(), 1);
        assert!(r.rejected.is_empty() && r.withheld.is_empty());
        assert_eq!(r.loaded[0].name, "sigil-trading");
        assert_eq!(r.loaded[0].audience, "public");
        assert!(r.loaded[0].body.contains("# hi"));
    }

    #[test]
    fn missing_audience_field_is_public() {
        // Back-compat: manifests written before the audience gate must keep working.
        let r = parse_and_gate(&manifest_with("legacy", "# body", None), &none()).unwrap();
        assert_eq!(r.loaded.len(), 1);
        assert_eq!(r.loaded[0].audience, "public");
    }

    #[test]
    fn operator_skill_is_withheld_without_opt_in() {
        let m = manifest(vec![entry("slagteren-suensonsvej", "# butcher", None, Some("operator"))]);
        let r = parse_and_gate(&m, &none()).unwrap();
        assert!(r.loaded.is_empty(), "an operator skill must NOT reach a normal install");
        assert_eq!(r.withheld.len(), 1);
        assert!(r.withheld[0].contains("SIGIL_SKILLS=slagteren-suensonsvej"));
        // withheld is NOT rejected — nothing failed integrity here
        assert!(r.rejected.is_empty());
    }

    #[test]
    fn operator_skill_loads_when_named() {
        let m = manifest(vec![entry("slagteren-suensonsvej", "# butcher", None, Some("operator"))]);
        let r = parse_and_gate(&m, &vec!["slagteren-suensonsvej".to_string()]).unwrap();
        assert_eq!(r.loaded.len(), 1);
        assert_eq!(r.loaded[0].audience, "operator");
        // and case-insensitively
        let r2 = parse_and_gate(&m, &vec!["SLAGTEREN-SUENSONSVEJ".to_string()]).unwrap();
        assert_eq!(r2.loaded.len(), 1);
    }

    #[test]
    fn opt_in_all_takes_everything() {
        let m = manifest(vec![entry("priv", "# p", None, Some("operator"))]);
        assert_eq!(parse_and_gate(&m, &vec!["all".to_string()]).unwrap().loaded.len(), 1);
    }

    #[test]
    fn opt_in_does_not_leak_across_skills() {
        // Opting into ONE private skill must not unlock a different one.
        let m = manifest(vec![
            entry("mine", "# a", None, Some("operator")),
            entry("theirs", "# b", None, Some("operator")),
        ]);
        let r = parse_and_gate(&m, &vec!["mine".to_string()]).unwrap();
        assert_eq!(r.loaded.len(), 1);
        assert_eq!(r.loaded[0].name, "mine");
        assert_eq!(r.withheld.len(), 1);
    }

    #[test]
    fn public_skills_still_load_beside_a_withheld_one() {
        // The regression this whole gate exists to prevent: one private skill
        // must not take the public ones down with it.
        let m = manifest(vec![
            entry("slagteren-suensonsvej", "# butcher", None, Some("operator")),
            entry("sigil-trading", "# trading", None, None),
            entry("sigil-whitepaper", "# paper", None, Some("public")),
        ]);
        let r = parse_and_gate(&m, &none()).unwrap();
        let names: Vec<_> = r.loaded.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["sigil-trading", "sigil-whitepaper"]);
        assert_eq!(r.withheld.len(), 1);
    }

    #[test]
    fn tampered_body_is_rejected_not_dropped() {
        let good = "# original";
        let h = blake3::hash(good.as_bytes()).to_hex().to_string();
        let r = parse_and_gate(&manifest_with("s", "# tampered", Some(&h)), &none()).unwrap();
        assert!(r.loaded.is_empty());
        assert_eq!(r.rejected.len(), 1);
        assert!(r.rejected[0].contains("blake3 mismatch"));
    }

    #[test]
    fn integrity_gate_runs_before_the_audience_gate() {
        // A tampered private skill must be reported as REJECTED (someone should
        // look), not silently filed as "not for you".
        let h = blake3::hash(b"# original").to_hex().to_string();
        let m = manifest(vec![entry("priv", "# tampered", Some(&h), Some("operator"))]);
        let r = parse_and_gate(&m, &none()).unwrap();
        assert_eq!(r.rejected.len(), 1);
        assert!(r.withheld.is_empty());
    }

    #[test]
    fn bad_names_rejected() {
        let r = parse_and_gate(&manifest_with("../etc/passwd", "# x", None), &none()).unwrap();
        assert!(r.loaded.is_empty());
        assert!(r.rejected[0].contains("bad skill name"));
        assert!(parse_and_gate(&manifest_with("", "# x", None), &none()).unwrap().loaded.is_empty());
    }

    #[test]
    fn body_is_capped() {
        let long = "x".repeat(MAX_SKILL_CHARS + 500);
        let r = parse_and_gate(&manifest_with("big", &long, None), &none()).unwrap();
        assert_eq!(r.loaded[0].body.chars().count(), MAX_SKILL_CHARS);
    }

    #[test]
    fn total_budget_is_enforced_in_order() {
        // Four max-size skills exceed the 24k budget: the first three load,
        // the fourth is withheld — deterministically, in manifest order.
        let big = "y".repeat(MAX_SKILL_CHARS);
        let m = manifest((0..4).map(|i| entry(&format!("s{i}"), &big, None, None)).collect());
        let r = parse_and_gate(&m, &none()).unwrap();
        let total: usize = r.loaded.iter().map(|s| s.body.chars().count()).sum();
        assert!(total <= MAX_SKILLS_TOTAL_CHARS, "prompt budget must bound the total");
        assert_eq!(r.loaded.len(), MAX_SKILLS_TOTAL_CHARS / MAX_SKILL_CHARS);
        assert_eq!(r.withheld.len(), 1);
        assert!(r.withheld[0].contains("context budget"));
    }

    #[test]
    fn malformed_manifest_is_err_not_panic() {
        assert!(parse_and_gate("{not json", &none()).is_err());
        assert!(parse_and_gate(r#"{"skills":[{"name":"a"}]}"#, &none()).is_err());
        let r = parse_and_gate("{}", &none()).unwrap();
        assert!(r.loaded.is_empty() && r.rejected.is_empty() && r.withheld.is_empty());
    }

    #[test]
    fn note_reads_as_english() {
        let mut l = SkillLoad::default();
        assert_eq!(l.note(), "0 flux-signed");
        l.withheld.push("x".into());
        assert!(l.note().contains("1 not for this install"));
        l.rejected.push("y".into());
        assert!(l.note().contains("1 REJECTED"));
    }

    #[test]
    fn context_block_shape() {
        assert_eq!(context_block(&[]), "");
        let s = LoadedSkill { name: "a".into(), version: "2".into(), description: "".into(), body: "BODY".into(), blake3_hex: "".into(), audience: "public".into() };
        let c = context_block(&[s]);
        assert!(c.contains("## Skill: a (v2)"));
        assert!(c.contains("BODY"));
        assert!(c.contains("never override the rule against"));
    }
}
