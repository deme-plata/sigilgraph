//! L4 — vetting an on-device rewrite before it leaves. Shared by sigil-top and the
//! browser (sigil-veil-wasm) so both run the SAME tested rules.
//!
//! Every rule here comes from a real model measured on Epsilon (2026-10-03):
//! qwen3:4b ignored `think:false` and answered with its reasoning, which QUOTED the
//! original; qwen3:0.6b rewrote well but kept an employer ("Aalborg Sygehus").

pub const REWRITE_SYSTEM: &str = "You prepare a question for an outside expert who must not learn who is asking. \
Rewrite the user's LATEST request as ONE self-contained question. Keep every fact the expert needs to answer \
correctly: numbers, amounts, error messages, code, technical terms, placeholders like [HEX_1]. Remove names of \
people, places, employers, dates and personal anecdotes, and anything else that identifies the asker. Use plain, \
neutral wording, not the user's own style. Output ONLY the rewritten question.";

/// Small and fast first — this is a paraphrase, not an answer (qwen3:4b measured
/// 133–148 s and thinking anyway; qwen3:0.6b 21–33 s).
pub const REWRITER_LADDER: &[&str] = &["qwen3:1.7b", "qwen3:0.6b", "llama3.2:3b", "qwen3:4b", "qwen3:8b"];

pub fn pick_rewriter(models: &[String], forced: Option<&str>) -> Option<String> {
    if let Some(f) = forced.map(str::trim).filter(|f| !f.is_empty()) {
        return Some(f.to_string());
    }
    for want in REWRITER_LADDER {
        if let Some(m) = models.iter().find(|m| m.as_str() == *want || m.starts_with(&format!("{want}-"))) {
            return Some(m.clone());
        }
    }
    models.first().cloned()
}

/// The user-turn content for the rewriter: up to 6 recent turns as private
/// context (user/assistant only — never tool output), then the latest request.
pub fn rewrite_prompt(history: &[(String, String)], user: &str) -> String {
    let ctx: Vec<String> = history.iter().rev().filter(|(r, _)| r == "user" || r == "assistant").take(6)
        .collect::<Vec<_>>().into_iter().rev()
        .map(|(r, c)| format!("{r}: {}", c.chars().take(600).collect::<String>())).collect();
    let mut p = String::new();
    if !ctx.is_empty() {
        p.push_str("Conversation so far (private context, do not repeat personal details):\n");
        p.push_str(&ctx.join("\n"));
        p.push_str("\n\n");
    }
    p.push_str("Latest request:\n");
    p.push_str(user);
    p
}

fn strip_think(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(a) = rest.find("<think>") {
        out.push_str(&rest[..a]);
        match rest[a..].find("</think>") {
            Some(b) => rest = &rest[a + b + "</think>".len()..],
            None => { rest = ""; break; }
        }
    }
    out.push_str(rest);
    out
}

/// Accept a rewrite only if it looks like ONE question — not an essay, an answer,
/// a refusal, the model talking about the task, or a copy of the original.
pub fn clean_rewrite(raw: &str, original: &str) -> Option<String> {
    let mut t = strip_think(raw).trim().to_string();
    for label in ["Rewritten question:", "Rewritten request:", "Question:"] {
        if let Some(rest) = t.strip_prefix(label) { t = rest.trim().to_string(); }
    }
    let t = t.trim_matches(|c| c == '"' || c == '“' || c == '”').trim().to_string();
    let limit = (original.chars().count() * 4).max(600);
    if t.is_empty() || t.chars().count() > limit {
        return None;
    }
    let lower = t.to_lowercase();
    if lower.starts_with("i can't") || lower.starts_with("i cannot") || lower.starts_with("sorry") {
        return None;
    }
    // whole words — a substring test rejected "wallet's" for containing "let's"
    let words = format!(" {} ", lower.chars().map(|c| if c.is_alphanumeric() || c == '\'' { c } else { ' ' })
        .collect::<String>().split_whitespace().collect::<Vec<_>>().join(" "));
    for meta in ["the user", "rewrite", "rewriting", "rewritten", "original", "let's", "steps", "self contained",
                 "output only", "translating", "translation", "here is", "here's"] {
        if words.contains(&format!(" {meta} ")) {
            return None;
        }
    }
    let norm = |x: &str| x.to_lowercase().split_whitespace().map(|w| w.trim_matches(|c: char| !c.is_alphanumeric())).filter(|w| !w.is_empty()).map(String::from).collect::<Vec<_>>();
    let (o, r) = (norm(original), norm(&t));
    if o.len() >= 6 {
        let rj = format!(" {} ", r.join(" "));
        if o.windows(6).any(|w| rj.contains(&format!(" {} ", w.join(" ")))) {
            return None;
        }
    }
    Some(t)
}

/// Capitalised, not sentence-initial, not an ACRONYM, ≥3 letters: names, places, employers.
pub fn proper_nouns(original: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut sentence_start = true;
    for raw in original.split_whitespace() {
        let w: String = raw.trim_matches(|c: char| !c.is_alphanumeric()).to_string();
        let first_upper = w.chars().next().map(|c| c.is_uppercase()).unwrap_or(false);
        let all_caps = w.chars().filter(|c| c.is_alphabetic()).all(|c| c.is_uppercase());
        if !sentence_start && first_upper && !all_caps && w.chars().count() >= 3 && w.chars().all(|c| c.is_alphabetic()) && !out.contains(&w) {
            out.push(w.clone());
        }
        sentence_start = raw.ends_with('.') || raw.ends_with('!') || raw.ends_with('?') || raw.ends_with(':');
    }
    out
}

/// Deterministic backstop: any proper noun of the original that survived → [NAME].
pub fn scrub_carried_names(rewrite: &str, original: &str) -> (String, usize) {
    let mut out = rewrite.to_string();
    let mut n = 0;
    for name in proper_nouns(original) {
        if out.contains(&name) {
            out = out.replace(&name, "[NAME]");
            n += 1;
        }
    }
    (out, n)
}

/// The whole vetting step: clean, then scrub. None = do not use this rewrite.
pub fn vet(raw: &str, original: &str) -> Option<String> {
    clean_rewrite(raw, original).map(|t| scrub_carried_names(&t, original).0)
}

#[cfg(test)]
mod tests {
    use super::*;
    const Q1: &str = "I'm Viktor from Frederikshavn, my mum lent me 200 SIGIL and now my wallet send to [HEX_1] fails with HTTP 413 — what's going on?";
    const Q2: &str = "Hej, jeg hedder Anna og arbejder på Aalborg Sygehus. Min chef Lars siger min mining rig laver 525 MH/s men jeg får 0 SIGIL — hvorfor?";

    #[test]
    fn measured_outputs_are_judged_correctly() {
        let good = "What is going on with the wallet sending to [HEX_1] when a 413 HTTP error occurs?";
        assert_eq!(vet(good, Q1).as_deref(), Some(good));
        let leaky = "What is the reason for receiving a 0 SIGIL error when a mining rig produces 525 MH/s at Aalborg Sygehus?";
        let v = vet(leaky, Q2).unwrap();
        assert!(!v.contains("Aalborg") && !v.contains("Sygehus") && v.contains("[NAME] [NAME]"), "{v}");
        assert_eq!(vet("We are rewriting the user's latest request as ONE self-contained question. Original user request: \"I'm Viktor", Q1), None);
        assert_eq!(vet("Okay, let's tackle this. The user wants me to rewrite", Q2), None);
        assert_eq!(vet("my mum lent me 200 SIGIL and now my wallet send fails", Q1), None, "a copy is not a rewrite");
        let fine = "Why would a cryptocurrency wallet's send request fail with HTTP 413?";
        assert_eq!(vet(fine, Q1).as_deref(), Some(fine), "\"wallet's\" is not \"let's\"");
    }

    #[test]
    fn prompt_and_picker() {
        let p = rewrite_prompt(&[("user".into(), "hi".into()), ("tool".into(), "{secret}".into())], "q?");
        assert!(p.contains("user: hi") && p.ends_with("Latest request:\nq?") && !p.contains("{secret}"));
        let m = vec!["qwen3:8b".to_string(), "qwen3:4b".to_string(), "qwen3:0.6b".to_string()];
        assert_eq!(pick_rewriter(&m, None).as_deref(), Some("qwen3:0.6b"), "measured: 0.6b before 4b");
        assert_eq!(proper_nouns(Q2), vec!["Anna", "Aalborg", "Sygehus", "Lars"]);
        assert!(proper_nouns("why does SIGIL show HTTP 413?").is_empty());
    }
}
