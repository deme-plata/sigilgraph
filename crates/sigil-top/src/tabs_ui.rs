// LANE-U: extracted from main.rs (pure move, no behavior change).
// `use super::*` reaches main.rs's private helpers/consts/App — the heroes.rs pattern.
#![allow(clippy::too_many_lines)]
use super::*;

pub(crate) fn dim(s: impl Into<String>) -> Span<'static> { Span::styled(s.into(), Style::default().fg(C_DIM)) }

// ── v0.33.2 BOLD NEON helpers ──────────────────────────────────────────
/// A filled "banner" pill: bright text on a saturated neon background — used for the
/// loud status verdicts (LIGHT-SYNCED, SPINE BREAK, LIVE). ASCII-safe (text only).
pub(crate) fn banner(text: impl Into<String>, bg: Color) -> Span<'static> {
    Span::styled(format!(" {} ", text.into()),
        Style::default().bg(bg).fg(Color::Rgb(0x05, 0x05, 0x0d)).add_modifier(Modifier::BOLD))
}

/// A neon block-bar `█████░░░` sized to `frac` over `width` cells. Rich (uses █/░ — both
/// width-1, conhost-safe). Returns a styled Span in the given neon color.
pub(crate) fn neon_bar(frac: f64, width: usize, color: Color) -> Span<'static> {
    let f = (frac.clamp(0.0, 1.0) * width as f64).round() as usize;
    let s = "█".repeat(f.min(width)) + &"░".repeat(width.saturating_sub(f));
    Span::styled(s, Style::default().fg(color))
}

/// Bright value span (neon gold, bold) — the headline numbers.
pub(crate) fn val(s: impl Into<String>) -> Span<'static> {
    Span::styled(s.into(), Style::default().fg(C_NEON_GOLD).add_modifier(Modifier::BOLD))
}

/// thousands-grouped integer (1135287 → "1,135,287")
pub(crate) fn group(n: u64) -> String {
    let s = n.to_string(); let b = s.as_bytes(); let mut o = String::new();
    for (i, c) in b.iter().enumerate() { if i > 0 && (b.len() - i) % 3 == 0 { o.push(','); } o.push(*c as char); }
    o
}

pub(crate) fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() }
    else { format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>()) }
}

#[cfg(test)]
mod fmt_tests {
    use super::{group, trunc};
    #[test]
    fn group_thousands() {
        assert_eq!(group(0), "0");
        assert_eq!(group(999), "999");
        assert_eq!(group(1000), "1,000");
        assert_eq!(group(1_135_287), "1,135,287");
        assert_eq!(group(1_000_000), "1,000,000");
    }
    #[test]
    fn trunc_counts_chars_not_bytes() {
        assert_eq!(trunc("hello", 10), "hello");      // fits, unchanged
        assert_eq!(trunc("hello world", 5), "hell…"); // take n-1, then ellipsis
        assert_eq!(trunc("café", 3), "ca…");          // multibyte: char-count, never splits a byte
        assert_eq!(trunc("", 3), "");
    }
}

/// Stable per-agent color from an id hash — premium control-panel feel, same agent always same hue.
pub(crate) fn agent_color(id: &str) -> Color {
    let pal = [C_CYAN, C_VBRIGHT, C_GREEN, C_GOLD, Color::Magenta, Color::LightBlue];
    let mut h: u32 = 2166136261;
    for b in id.bytes() { h = (h ^ b as u32).wrapping_mul(16777619); }
    pal[(h as usize) % pal.len()]
}

/// Inline unicode mini-bar: `value` normalized to `max` across `width` cells.
pub(crate) fn qug_bar(value: f64, max: f64, width: usize) -> String {
    if max <= 0.0 || width == 0 { return " ".repeat(width); }
    let filled = (((value / max) * width as f64).round() as usize).min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

/// Medal glyph for a 0-based rank.
pub(crate) fn medal(rank: usize) -> &'static str {
    match rank { 0 => "🥇", 1 => "🥈", 2 => "🥉", _ => "  " }
}

/// Relative "Nm ago" from a unix-secs timestamp.
pub(crate) fn rel_time(at: u64) -> String {
    if at == 0 { return "—".into(); }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(at);
    let d = now.saturating_sub(at);
    if d < 60 { format!("{}s", d) }
    else if d < 3600 { format!("{}m", d / 60) }
    else if d < 86400 { format!("{}h", d / 3600) }
    else { format!("{}d", d / 86400) }
}

/// Status dot + color for an agent status string.
pub(crate) fn status_glyph(status: &str) -> Span<'static> {
    let (g, c) = match status.to_lowercase().as_str() {
        "working" | "busy" | "active" | "claimed" => ("●", C_GREEN),
        "idle" => ("○", C_DIM),
        "error" | "failed" => ("●", C_RED),
        _ => ("◦", C_DIM),
    };
    Span::styled(g, Style::default().fg(c))
}

pub(crate) fn render_tab_bar(app: &App) -> Paragraph<'static> {
    let tab = |label: &'static str, key: &'static str, t: Tab| -> Vec<Span<'static>> {
        let active = app.tab == t;
        let style = if active {
            Style::default().fg(C_BG).bg(C_NEON_CYAN).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(C_DIM)
        };
        vec![Span::styled(format!(" {key} {label} "), style), Span::raw(" ")]
    };
    let mut spans = vec![Span::raw(" ")];
    spans.extend(tab("Node", "1", Tab::Node));
    spans.extend(tab("Sync Log", "2", Tab::SyncLog));
    spans.extend(tab("Mining", "3", Tab::Mining));
    spans.extend(tab("Queues", "4", Tab::Queues));
    // The AI tab wears its state: ⏳ while the boot-time auto-setup pulls the model,
    // ✓ once a local model answers — so nobody has to open it to learn whether it works.
    let ai_label: &'static str = if app.ai_setup_running { "AI ⏳" } else if app.ai_model.is_some() { "AI ✓" } else { "AI" };
    spans.extend(tab(ai_label, "5", Tab::Ai));
    spans.push(Span::styled(" · Tab cycles", Style::default().fg(C_DIM)));
    Paragraph::new(Line::from(spans))
}

/// [A]I tab — flux-moe chat against the user's LOCAL ollama. The chosen model IS
/// the effort dial (a bigger model on a bigger GPU reasons harder). The model call
/// runs off the UI thread (`ai_submit`); this only renders state.
/// Suggested opening questions, shown while the transcript is empty.
///
/// This exists because of a measured failure: asked about "Gordon Klein emission
/// with technical analysis", the model replied that the concept did not exist and
/// listed its own subsystems. Half that fix is the system prompt; the other half
/// is showing people the shape of a question this thing answers well, so the
/// first exchange is a good one instead of a dead end.
pub(crate) const AI_STARTERS: [&str; 5] = [
    "How is new SIGIL created?",
    "Why is my block reward getting smaller?",
    "What makes a private send slow?",
    "How can a phone verify the whole chain?",
    "What is the difference between SIGIL and wSIGIL?",
];

pub(crate) fn draw_ai_tab(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let [status_area, msgs_area, input_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(3),
    ]).areas(area);

    // status: connected model + count, or the setup hint
    let skills = if app.ai_skills_note.is_empty() { String::new() } else { format!("   ·   🔏 skills: {}", app.ai_skills_note) };
    let status = match (&app.ai_model, app.ai_setup_running) {
        (_, true) => format!(" 🧠 setting up your local AI — progress below{skills}"),
        (Some(m), false) => format!(" 🧠 local model: {m}   ·   {} available{skills}   ·   Enter sends · F5 re-setup · F6 reload skills · Tab leaves",
                                    app.ai_models.len()),
        (None, false) => format!(" 🧠 no local model yet — auto-setup runs at start-up and when this tab opens; type `setup` + Enter (or F5) to run it again{skills}"),
    };
    f.render_widget(Paragraph::new(status).style(Style::default().fg(C_DIM)), status_area);

    // transcript
    let mut lines: Vec<Line> = Vec::new();
    if app.ai_msgs.is_empty() {
        lines.push(Line::from(dim("flux-moe — your on-device AI. It knows the SIGIL whitepaper, how mining and")));
        lines.push(Line::from(dim("emission work, the wallet, the Nation and the bridge — and explains them plainly.")));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("  try asking:", Style::default().fg(C_NEON_CYAN))));
        for q in AI_STARTERS {
            lines.push(Line::from(Span::styled(format!("    › {q}"), Style::default().fg(Color::White))));
        }
        lines.push(Line::from(""));
        // Name the loaded skills, so it is visible WHICH instructions are shaping
        // the answers — a skill you cannot see is a skill you cannot audit.
        if !app.ai_skills.is_empty() {
            let names: Vec<String> = app.ai_skills.iter()
                .map(|s| if s.audience == "public" { s.name.clone() } else { format!("{} ({})", s.name, s.audience) })
                .collect();
            lines.push(Line::from(Span::styled(
                format!("  🔏 loaded skills: {}", names.join(", ")),
                Style::default().fg(C_NEON_GOLD),
            )));
        }
        lines.push(Line::from(dim("  It never invents balances or prices — those come from the node.")));
    }
    for (i, (role, content)) in app.ai_msgs.iter().enumerate() {
        // The in-flight assistant message is still empty while the model thinks: the
        // progress block below speaks for it, so don't print a bare "moe ›" yet.
        if app.ai_thinking && Some(i) == app.ai_reply_idx && content.is_empty() { continue; }
        if role == "tool" {
            lines.push(Line::from(Span::styled(format!("  {content}"), Style::default().fg(C_NEON_GOLD))));
            continue;
        }
        if role == "user" {
            lines.push(Line::from(Span::styled("you ›", Style::default().fg(C_NEON_CYAN).add_modifier(Modifier::BOLD))));
        } else {
            lines.push(Line::from(Span::styled("moe ›", Style::default().fg(C_NEON_GOLD).add_modifier(Modifier::BOLD))));
        }
        let width = msgs_area.width.max(8) as usize;
        for l in content.lines() {
            // Wrap HERE, one Line per screen row, so the scroll arithmetic below is exact.
            for row in wrap_rows(&format!("  {l}"), width, 2) {
                lines.push(Line::from(Span::styled(row, Style::default().fg(Color::White))));
            }
        }
        lines.push(Line::from(""));
    }
    if app.ai_thinking {
        lines.extend(ai_progress_lines(app));
    } else if !app.ai_last_stats.is_empty() && !app.ai_msgs.is_empty() {
        lines.push(Line::from(dim(&format!("  ⏱ {}", app.ai_last_stats))));
    }
    for q in &app.ai_queued {
        lines.push(Line::from(Span::styled("you › (queued — sent when moe finishes)", Style::default().fg(C_DIM))));
        lines.push(Line::from(Span::styled(format!("  {q}"), Style::default().fg(C_DIM))));
    }
    // Transcript rows are pre-wrapped above (one Line = one screen row), so the newest row
    // is always the last one — the old count-Lines-then-let-ratatui-wrap mismatch pushed the
    // latest prompt + streaming reply below the visible area. Short status lines still use
    // the estimate.
    let width = msgs_area.width.max(1) as usize;
    let total: usize = lines.iter().map(|l| wrapped_rows(l, width)).sum();
    let inner_h = msgs_area.height.saturating_sub(2) as usize; // top border + breathing room
    let scroll = total.saturating_sub(inner_h).min(u16::MAX as usize) as u16;
    f.render_widget(
        Paragraph::new(lines)
            .wrap(ratatui::widgets::Wrap { trim: false })
            .scroll((scroll, 0))
            .block(Block::default().borders(Borders::TOP).border_style(Style::default().fg(C_DIM))),
        msgs_area,
    );

    // input line
    let cursor = if app.ai_thinking { "" } else { "▏" };
    f.render_widget(
        Paragraph::new(format!(" › {}{cursor}", app.ai_input))
            .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(C_NEON_CYAN)).title(" ask flux-moe ")),
        input_area,
    );
}

/// Pure: greedy word-wrap of `text` into rows at most `width` columns wide; continuation
/// rows are indented by `indent` spaces. Over-long words are hard-split.
pub(crate) fn wrap_rows(text: &str, width: usize, indent: usize) -> Vec<String> {
    let width = width.max(indent + 4);
    let w = |s: &str| Span::raw(s.to_string()).width();
    let mut rows: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_inclusive(' ') {
        let fits = w(&cur) + w(word.trim_end_matches(' ')) <= width;
        if !fits && !cur.trim().is_empty() {
            rows.push(cur.trim_end().to_string());
            cur = " ".repeat(indent);
        }
        if w(&cur) + w(word) > width {
            for ch in word.chars() {                       // hard-split a word longer than a row
                if w(&cur) + w(&ch.to_string()) > width { rows.push(std::mem::take(&mut cur)); cur = " ".repeat(indent); }
                cur.push(ch);
            }
        } else {
            cur.push_str(word);
        }
    }
    if !cur.trim().is_empty() || rows.is_empty() { rows.push(cur.trim_end().to_string()); }
    rows
}

/// Pure: how many screen rows `line` occupies when word-wrapped (trim off) to `width`,
/// mirroring ratatui's WordWrapper closely enough that the newest row is never hidden.
pub(crate) fn wrapped_rows(line: &Line, width: usize) -> usize {
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    if text.is_empty() { return 1; }
    let (mut rows, mut col) = (1usize, 0usize);
    for word in text.split_inclusive(' ') {
        let w = Span::raw(word).width();
        if col + w.saturating_sub(if word.ends_with(' ') { 1 } else { 0 }) > width && col > 0 {
            rows += 1;
            col = 0;
        }
        if w > width {                     // a word longer than the row is hard-split
            rows += (col + w - 1) / width;
            col = (col + w) % width;
        } else {
            col += w;
        }
    }
    rows
}

/// Pure: the progress estimate for an in-flight reply. Returns (fraction 0..1, eta secs).
/// A model's reply length is not known in advance, so the target is the running average
/// of this session's replies (default 300 tokens) and the bar is capped at 95 % until the
/// model says it is done — the ETA is an estimate and is labelled as one.
pub(crate) fn ai_progress(tokens: u64, rate_tok_s: f64, avg_reply_tokens: f64) -> (f64, Option<f64>) {
    let target = if avg_reply_tokens >= 20.0 { avg_reply_tokens } else { 300.0 };
    let frac = (tokens as f64 / target).min(0.95);
    let eta = if rate_tok_s > 0.5 { Some(((target - tokens as f64).max(0.0)) / rate_tok_s) } else { None };
    (frac, eta)
}

fn ai_progress_lines(app: &App) -> Vec<Line<'static>> {
    const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let elapsed = app.ai_started.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
    let spin = SPIN[((elapsed * 10.0) as usize) % SPIN.len()];
    let rate = match app.ai_first_token {
        Some(t0) if app.ai_tokens > 1 => app.ai_tokens as f64 / t0.elapsed().as_secs_f64().max(0.05),
        _ => 0.0,
    };
    let load = app.ai_load.lock().map(|l| l.clone()).unwrap_or_default();
    let mut gauges: Vec<Span<'static>> = Vec::new();
    match (load.gpu_pct, load.vram_used_mb, load.vram_total_mb) {
        (Some(g), Some(u), Some(t)) => gauges.push(Span::styled(
            format!("GPU {g:>3}% · VRAM {:.1}/{:.1} GB", u as f64 / 1024.0, t as f64 / 1024.0),
            Style::default().fg(if g >= 50 { C_NEON_GREEN } else { C_GOLD }))),
        _ => gauges.push(dim(if crate::flux_moe::ollama_is_local() { "GPU —" } else { "GPU remote" })),
    }
    if let Some(c) = load.cpu_pct { gauges.push(dim(&format!(" · CPU {c}%"))); }

    let mut out = vec![Line::from(Span::styled("moe ›", Style::default().fg(C_NEON_GOLD).add_modifier(Modifier::BOLD)))];
    if app.ai_tokens == 0 {
        // Still reasoning (qwen3 thinks before it answers) or loading the model.
        let what = if app.ai_think_chars > 0 { format!("thinking · {} chars of reasoning", app.ai_think_chars) }
                   else { "waiting for the model (loading / reading the prompt)".to_string() };
        let mut l = vec![Span::styled(format!("  {spin} "), Style::default().fg(C_NEON_CYAN)), dim(&format!("{what} · {elapsed:.0}s   "))];
        l.extend(gauges);
        out.push(Line::from(l));
    } else {
        let (frac, eta) = ai_progress(app.ai_tokens, rate, app.ai_avg_reply_tokens);
        let w = 24usize;
        let filled = (frac * w as f64).round() as usize;
        let bar = format!("{}{}", "█".repeat(filled), "░".repeat(w - filled));
        let eta_s = match eta { Some(e) => format!("eta ~{e:.0}s (est.)"), None => "eta —".to_string() };
        let mut l = vec![
            Span::styled(format!("  {spin} "), Style::default().fg(C_NEON_CYAN)),
            Span::styled(bar, Style::default().fg(C_NEON_CYAN)),
            dim(&format!(" {} tok · {rate:.1} tok/s · {eta_s} · {elapsed:.0}s   ", app.ai_tokens)),
        ];
        l.extend(gauges);
        out.push(Line::from(l));
    }
    out
}

/// [2] MCP Swarm AI — the live job-index board from the Claude Code sessions.
pub(crate) fn render_swarm_ai(app: &App) -> Paragraph<'static> {
    let sw = &app.swarm;
    let mut lines: Vec<Line> = Vec::new();
    if let Some(e) = &sw.err {
        lines.push(Line::from(Span::styled(format!(" ⚠ {e}"), Style::default().fg(C_GOLD))));
        lines.push(Line::from(""));
    }
    let real: Vec<&SwarmAgent> = sw.agents.iter().filter(|a| !a.id.starts_with("test_")).collect();
    lines.push(Line::from(vec![
        Span::styled("  AGENTS ", Style::default().fg(C_CYAN).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{}", real.len()), Style::default().fg(C_VBRIGHT)),
        Span::styled("   TASKS ", Style::default().fg(C_CYAN).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{}", sw.tasks.len()), Style::default().fg(C_VBRIGHT)),
        Span::styled("   FILES ", Style::default().fg(C_CYAN).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{}", sw.claims.len()), Style::default().fg(C_VBRIGHT)),
        Span::styled("   DONE ", Style::default().fg(C_CYAN).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{}", sw.completed_count), Style::default().fg(C_GREEN)),
        Span::styled("   QUG PAID ", Style::default().fg(C_CYAN).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{:.1}", sw.qug_paid), Style::default().fg(C_GOLD)),
    ]));
    lines.push(Line::from(""));
    // ── TASK BOARD: claimed jobs with priority + QUG bounty (the index board) ──
    let max_b = sw.tasks.iter().map(|t| t.est_qug).fold(0.0f64, f64::max);
    lines.push(Line::from(Span::styled(" ▸ TASK BOARD — claimed jobs · priority · bounty", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD))));
    for t in sw.tasks.iter().take(7) {
        let (ptxt, pcol) = match t.priority {
            0 | 1 => (format!("P{}", t.priority), C_GOLD),
            2 => ("P2".to_string(), C_CYAN),
            _ => (format!("P{}", t.priority), C_DIM),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {:<3}", ptxt), Style::default().fg(pcol).add_modifier(Modifier::BOLD)),
            Span::styled(format!("{:<15}", trunc(&t.agent, 15)), Style::default().fg(agent_color(&t.agent)).add_modifier(Modifier::BOLD)),
            Span::styled(format!("{:<18}", trunc(&t.crates, 18)), Style::default().fg(C_CYAN)),
            Span::styled(qug_bar(t.est_qug, max_b, 8), Style::default().fg(C_GOLD)),
            Span::styled(format!(" {:>5.1} QUG", t.est_qug), Style::default().fg(C_GOLD)),
        ]));
    }
    lines.push(Line::from(""));
    // ── AGENTS leaderboard ──
    let max_q = real.iter().map(|a| a.qug).fold(0.0f64, f64::max);
    lines.push(Line::from(Span::styled(" ▸ AGENTS — leaderboard by QUG earned", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD))));
    for (i, a) in real.iter().take(5).enumerate() {
        lines.push(Line::from(vec![
            Span::raw(format!("  {} ", medal(i))),
            status_glyph(&a.status),
            Span::styled(format!(" {:<20}", trunc(&a.id, 20)), Style::default().fg(agent_color(&a.id)).add_modifier(Modifier::BOLD)),
            Span::styled(qug_bar(a.qug, max_q, 10), Style::default().fg(C_GOLD)),
            Span::styled(format!(" {:>8.1} QUG", a.qug), Style::default().fg(C_GOLD)),
        ]));
    }
    lines.push(Line::from(""));
    // ── BROADCAST FEED: the human-readable coordination board ──
    lines.push(Line::from(Span::styled(" ▸ 📢 BROADCAST FEED", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD))));
    for m in sw.feed.iter().take(5) {
        lines.push(Line::from(vec![
            Span::styled(format!("  {:>4} ", rel_time(m.at)), Style::default().fg(C_DIM)),
            Span::styled(format!("{:<16}", trunc(&m.from, 16)), Style::default().fg(agent_color(&m.from)).add_modifier(Modifier::BOLD)),
            Span::styled(trunc(&m.text, 56), Style::default().fg(C_DIM)),
        ]));
    }
    lines.push(Line::from(""));
    // ── LIVE ACTIVITY (compact) ──
    lines.push(Line::from(Span::styled(" ▸ LIVE ACTIVITY", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD))));
    for ev in sw.activity.iter().filter(|e| !e.agent.starts_with("test_")).take(6) {
        lines.push(Line::from(vec![
            Span::styled(format!("  {:>4} ", rel_time(ev.at)), Style::default().fg(C_DIM)),
            Span::styled(format!("{:<16}", trunc(&ev.agent, 16)), Style::default().fg(agent_color(&ev.agent))),
            Span::styled(format!("{:<14}", trunc(&ev.kind, 14)), Style::default().fg(C_GOLD)),
            Span::styled(trunc(&ev.detail, 44), Style::default().fg(C_DIM)),
        ]));
    }
    Paragraph::new(lines).block(card_block(" ✦ MCP SWARM AI — JOB INDEX BOARD", C_NEON_PINK))
}

pub(crate) fn render_results(app: &App) -> Paragraph<'static> {
    let sw = &app.swarm;
    let mut lines: Vec<Line> = Vec::new();
    let mut totals: std::collections::HashMap<String, (f64, u32)> = std::collections::HashMap::new();
    for r in sw.results.iter().filter(|r| !r.agent.starts_with("test_")) {
        let e = totals.entry(r.agent.clone()).or_insert((0.0, 0));
        e.0 += r.qug; e.1 += 1;
    }
    let mut tv: Vec<(String, (f64, u32))> = totals.into_iter().collect();
    tv.sort_by(|a, b| b.1 .0.partial_cmp(&a.1 .0).unwrap_or(std::cmp::Ordering::Equal));
    let max_e = tv.iter().map(|(_, (q, _))| *q).fold(0.0f64, f64::max);
    lines.push(Line::from(Span::styled(" ▸ EARNINGS — leaderboard", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD))));
    for (i, (ag, (qug, n))) in tv.iter().take(7).enumerate() {
        lines.push(Line::from(vec![
            Span::raw(format!("  {} ", medal(i))),
            Span::styled(format!("{:<22}", trunc(ag, 22)), Style::default().fg(agent_color(ag)).add_modifier(Modifier::BOLD)),
            Span::styled(qug_bar(*qug, max_e, 12), Style::default().fg(C_GOLD)),
            Span::styled(format!(" {:>3}t", n), Style::default().fg(C_DIM)),
            Span::styled(format!("{:>10.2} QUG", qug), Style::default().fg(C_GOLD)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(" ▸ COMPLETED TASKS (newest first)", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD))));
    for r in sw.results.iter().filter(|r| !r.agent.starts_with("test_")).take(13) {
        let mark = if r.success { Span::styled("✓", Style::default().fg(C_GREEN)) } else { Span::styled("✗", Style::default().fg(C_RED)) };
        lines.push(Line::from(vec![
            Span::raw(sa("  ")), mark, Span::raw(sa(" ")),
            Span::styled(format!("{:>4} ", rel_time(r.at)), Style::default().fg(C_DIM)),
            Span::styled(format!("{:<14}", trunc(&r.agent, 14)), Style::default().fg(agent_color(&r.agent))),
            Span::styled(format!("{:<18}", trunc(&r.crates, 18)), Style::default().fg(C_CYAN)),
            Span::styled(format!("{:>7.2} QUG", r.qug), Style::default().fg(C_GOLD)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  TOTAL SETTLED: ", Style::default().fg(C_CYAN).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{:.1} QUG", sw.qug_paid), Style::default().fg(C_GOLD).add_modifier(Modifier::BOLD)),
        Span::styled(format!("  ·  {} tasks completed", sw.completed_count), Style::default().fg(C_DIM)),
    ]));
    Paragraph::new(lines).block(card_block("🏆 RESULTS — SETTLED WORK", C_GOLD))
}

pub(crate) fn draw_ui(f: &mut Frame, app: &App) {
    let area = f.area();
    if let Some(until) = app.splash_until {
        if Instant::now() < until {
            f.render_widget(render_update_splash(app.splash_frame), area);
            return;
        }
    }
    // v0.13: tab bar between header and body — [1] Node · [2] Swarm AI · [3] Results.
    let [header_area, tab_area, body_area, footer_area] =
        Layout::vertical([Constraint::Length(2), Constraint::Length(1), Constraint::Min(0), Constraint::Length(footer_height(app, area.width))]).areas(area);

    f.render_widget(render_header(app), header_area);
    f.render_widget(render_tab_bar(app), tab_area);

    match app.tab {
        Tab::Node => {
            // v0.33.3: SYNC full-width HERO band on top; v0.36.1: an equally-large MINING hero
            // (network power + personal mining) right below it; the other cards flow beneath.
            // 10.4.27: a full node gets a third hero — the ◆ PRODUCER band (seal + block belt).
            let prod_h = if crate::forge_ui::band_visible() { 8 } else { 0 };
            let [hero_sync, hero_prod, hero_mining, cards_area] =
                Layout::vertical([Constraint::Length(11), Constraint::Length(prod_h), Constraint::Length(9), Constraint::Min(0)]).areas(body_area);
            draw_sync_hero(f, app, hero_sync);
            if prod_h > 0 { crate::forge_ui::draw_producer_hero(f, app, hero_prod); }
            draw_mining_hero(f, app, hero_mining);
            draw_node_body(f, app, cards_area);
        }
        Tab::SyncLog => f.render_widget(render_sync_log(app), body_area),
        Tab::Mining => draw_mining_tab(f, app, body_area),
        Tab::Queues => crate::sync_ui::draw_queues_tab(f, app, body_area),
        Tab::Ai => draw_ai_tab(f, app, body_area),
    }

    f.render_widget(render_footer(app, footer_area.width), footer_area);

    // LANE-B v0.50: float the SIGIL rune band ABOVE the live cockpit — drawn last
    // so it overlays every tab, fades on its own clock, and steals no layout slot.
    if let (Some(start), Some(until)) = (app.rune_started, app.rune_until) {
        if Instant::now() < until {
            draw_rune_band(f, app, area, start, until);
        }
    }

    // LANE-U v0.67: first-launch WELCOME — centered card with the SIGIL emblem + a giant F
    // call-to-action. Drawn LAST so it floats above everything; any key or 14s clears it.
    if app.welcome_until.map(|u| Instant::now() < u).unwrap_or(false) {
        draw_welcome_modal(f, area);
    }

    // v7.1.28: live block-store-open progress — floats above everything else, same as
    // the welcome modal, but self-gates (draw_store_open_overlay returns immediately
    // once the open finishes or if there was never a WAL to replay).
    draw_store_open_overlay(f, app, area);
}

pub(crate) fn render_fleet_card(app: &App) -> Paragraph<'static> {
    // Clone all fleet data to avoid borrow-from-app lifetime issues
    let nodes: Vec<(String, bool, u64, String)> = app.fleet_nodes.iter().map(|n| {
        let ver = if n.version.is_empty() { "?".to_string() }
            else if version_gt(VERSION, &n.version) { format!("!{}", n.version) }
            else { n.version.clone() };
        (n.name.clone(), n.online, n.height, ver)
    }).collect();
    let checking = app.fleet_last_check.elapsed() < Duration::from_secs(30);
    let total = nodes.len();
    let online = nodes.iter().filter(|n| n.1).count();
    let outdated = nodes.iter().filter(|n| n.1 && n.3.starts_with('!')).count();

    // v0.8: Mesh health from flux-p2p (if P2P sync is running)
    let mesh = app.p2p_state.mesh_peer_count;
    let mesh_quality = if mesh >= 4 { "healthy" } else if mesh >= 1 { "warming" } else { "empty" };
    let mesh_blk = app.p2p_blocks_synced;

    // Status line with fleet + mesh summary
    let status_color = if online == total && outdated == 0 { C_GREEN }
        else if online > 0 { C_GOLD }
        else { C_RED };
    let status_line = Line::from(vec![
        dim("fleet   "),
        Span::styled(format!("{}/{}", online, total),
            Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
        dim(if checking { "  checking…" } else { "" }),
        if outdated > 0 {
            Span::styled(format!("  {} behind", outdated),
                Style::default().fg(C_RED).add_modifier(Modifier::BOLD))
        } else { Span::raw(sa("")) },
    ]);
    // v0.8: Mesh health line
    let mesh_line = Line::from(vec![
        dim("mesh    "),
        Span::styled(format!("{} peers", mesh), Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD)),
        dim("  "),
        Span::styled(mesh_quality, Style::default().fg(
            if mesh_quality == "healthy" { C_GREEN } else if mesh_quality == "warming" { C_GOLD } else { C_RED }
        )),
        dim(format!("  {} blk synced", group(mesh_blk))),
    ]);

    // Per-node lines with owned data
    let node_lines: Vec<Line> = nodes.iter().map(|(name, online, height, ver)| {
        let dot = if *online { Span::styled("●", Style::default().fg(C_GREEN)) }
                  else { Span::styled("○", Style::default().fg(C_RED)) };
        let ver_color = if ver == "?" { C_DIM }
            else if ver.starts_with('!') { C_RED }
            else { C_GREEN };
        Line::from(vec![
            dot,
            Span::raw(sa(" ")),
            Span::styled(name.clone(), Style::default().fg(C_CYAN)),
            dim(format!("  h{}", group(*height))),
            Span::raw(sa("  ")),
            Span::styled(ver.clone(), Style::default().fg(ver_color).add_modifier(Modifier::BOLD)),
        ])
    }).collect();

    let mut lines = vec![status_line, mesh_line];
    lines.extend(node_lines);

    Paragraph::new(lines).block(card_block(" ● FLEET · MESH", C_NEON_CYAN))
}

pub(crate) fn render_block_stream(app: &App) -> Paragraph<'static> {
    let tip_ok = app.verify.as_ref().map_or(false, |v| v.ok);
    let title: &'static str = if app.st.blocks_per_sec >= 5000.0 {
        " BLOCK STREAM · 5k+ blk/s"
    } else if app.st.blocks_per_sec >= 1000.0 {
        " BLOCK STREAM · 1k+ blk/s"
    } else if app.st.blocks_per_sec > 0.0 {
        " BLOCK STREAM · turbo"
    } else {
        " BLOCK STREAM · live"
    };
    let lines: Vec<Line> = if app.blocks.is_empty() {
        vec![Line::from(dim("streaming…"))]
    } else {
        app.blocks.iter().take(14).enumerate().map(|(i, b)| {
            let hash_pref: String = b.hash.chars().take_while(|c| c.is_ascii_hexdigit()).take(8).collect();
            let producer = if b.producer.is_empty() { "—".to_string() } else { b.producer.chars().take(12).collect() };
            // colored block marker (no dingbat): green tip, violet history
            let mark = if i == 0 && tip_ok { Span::styled("█ ", Style::default().fg(C_GREEN)) }
                       else { Span::styled("▌ ", Style::default().fg(C_VIOLET)) };
            Line::from(vec![
                mark,
                Span::styled(format!("{:>10} ", group(b.height)), Style::default().fg(C_GOLD)),
                Span::styled(format!("{}… ", hash_pref), Style::default().fg(C_DIM)),
                Span::styled(producer, Style::default().fg(C_CYAN)),
                dim(if b.tip_ms == 0 { "  —".to_string() } else { format!("  {}ms", b.tip_ms) }),
            ])
        }).collect()
    };
    Paragraph::new(lines).block(card_block(&title, C_VBRIGHT))
}


/// Most rows a long toast may take (10.4.39). The footer used to give the toast ONE row, so a
/// link or an ssh hint was cut at the terminal's edge (operator, 2026-10-18: "i cant see the
/// whole line for the wallet link"). Now it wraps; only what is longer still ends in "…".
pub(crate) const TOAST_MAX_ROWS: usize = 8;

/// The toast as rows that each fit `width` columns: every `\n` part on its own row(s), words
/// wrapped, a link longer than a row hard-split, continuation rows indented under the text.
pub(crate) fn wrap_toast(toast: &str, width: usize) -> Vec<String> {
    if toast.is_empty() {
        return vec![String::new()];
    }
    let w = width.max(20);
    let mut rows = Vec::new();
    for (k, part) in toast.split('\n').enumerate() {
        let text = if k == 0 { format!(" › {part}") } else { format!("   {part}") };
        rows.extend(wrap_rows(&text, w, 3));
    }
    if rows.len() > TOAST_MAX_ROWS {
        rows.truncate(TOAST_MAX_ROWS);
        if let Some(last) = rows.last_mut() {
            let mut keep = last.clone();
            while Span::raw(keep.clone()).width() + 1 > w {
                keep.pop();
            }
            *last = format!("{keep}…");
        }
    }
    rows
}

/// Footer rows for this frame: the toast's rows (one while offline) + the key row.
pub(crate) fn footer_height(app: &App, width: u16) -> u16 {
    let toast_rows = if app.online { wrap_toast(&app.toast, width as usize).len() } else { 1 };
    toast_rows as u16 + 1
}

pub(crate) fn render_footer(app: &App, width: u16) -> Paragraph<'static> {
    let keys = |c: &'static str, rest: &'static str, col: Color| -> Vec<Span<'static>> {
        vec![Span::styled(c, Style::default().fg(col).add_modifier(Modifier::BOLD)), dim(rest), Span::raw(sa("  "))]
    };
    let mut kb = vec![Span::raw(sa(" "))];
    kb.extend(keys("[M]", "ine", C_GOLD));
    kb.extend(keys("[F]", "ull", C_GREEN));
    kb.extend(keys("[V]", "erify", C_GREEN));
    kb.extend(keys("[Y]", "esync", C_CYAN));
    kb.extend(keys("[W]", "allet", C_CYAN));
    kb.extend(keys("[T]", "stats", C_CYAN));
    kb.extend(keys("[B]", "activity", C_CYAN));
    kb.extend(keys("[U]", "pdate", C_VBRIGHT));
    kb.extend(keys("[N]", "odes", C_CYAN));
    kb.extend(keys("[L]", "ogin", C_VBRIGHT));
    kb.extend(keys("[Q]", "uit", C_RED));
    // 10.4.39: the old third "⚡ serve" line is gone — the layout only ever gave the footer two
    // rows, so it was never on screen.
    // v0.10.5.1: when offline, the top line becomes a calm status banner with a
    // live "offline for X · retry in Ns" countdown instead of a stale gold toast —
    // the operator always knows the cockpit is reconnecting, not frozen.
    let mut lines: Vec<Line<'static>> = if !app.online {
        let dur = app.offline_since.map(|t| fmt_uptime(t.elapsed().as_secs())).unwrap_or_else(|| "0s".into());
        let txt = if app.refresh_inflight {
            format!(" ⚠ offline {} · reconnecting…", dur)
        } else {
            let next = app.refresh_delay().as_secs().saturating_sub(app.last_fetch.elapsed().as_secs());
            format!(" ⚠ offline {} · retry in {}s", dur, next)
        };
        vec![Line::from(Span::styled(txt, Style::default().fg(C_RED).add_modifier(Modifier::BOLD)))]
    } else {
        wrap_toast(&app.toast, width as usize)
            .into_iter()
            .map(|r| Line::from(Span::styled(r, Style::default().fg(C_GOLD))))
            .collect()
    };
    lines.push(Line::from(kb));
    Paragraph::new(lines)
}

#[cfg(test)]
mod wrap_tests {
    use super::wrapped_rows;
    use ratatui::text::Line;

    /// 10.4.39: the [W] toast is never cut — it wraps inside the terminal, the link survives
    /// intact (rows joined), two-part toasts start a new row, and only a runaway one is capped.
    #[test]
    fn a_long_toast_wraps_instead_of_being_cut() {
        use super::{wrap_toast, TOAST_MAX_ROWS};
        let width = |r: &String| ratatui::text::Span::raw(r.clone()).width();
        assert_eq!(wrap_toast("", 80), vec![String::new()]);
        assert_eq!(wrap_toast("◆ local wallet → http://localhost:9800/w", 80).len(), 1, "the short link fits one row");
        let url = format!("http://localhost:9800/?addr={}", "c0ffee".repeat(10) + "abcd");
        let rows = wrap_toast(&format!("◆ local wallet → {url}"), 80);
        assert!(rows.len() >= 2 && rows.iter().all(|r| width(r) <= 80), "{rows:?}");
        let joined: String = rows.iter().map(|r| r.trim()).collect();
        assert!(joined.contains(&url), "the whole link is on screen: {rows:?}");
        let two = wrap_toast(&format!("🔐 ssh -L 9800:localhost:9800 me@host  then open http://localhost:9800/w\n🔗 or, prompted: https://sigilgraph.org/sigil-wallet-tron-embedded.html?addr={}", "ab".repeat(32)), 100);
        assert!(two.len() >= 3 && two.len() <= TOAST_MAX_ROWS && two.iter().all(|r| width(r) <= 100), "{two:?}");
        assert!(two.iter().any(|r| r.trim_start().starts_with("🔗")), "the second part starts its own row");
        let long = wrap_toast(&"word ".repeat(400), 60);
        assert_eq!(long.len(), TOAST_MAX_ROWS);
        assert!(long.last().unwrap().ends_with('…') && long.iter().all(|r| width(r) <= 60));
    }
    #[test]
    fn a_long_reply_counts_every_wrapped_row() {
        assert_eq!(wrapped_rows(&Line::from(""), 10), 1);
        assert_eq!(wrapped_rows(&Line::from("short"), 10), 1);
        assert_eq!(wrapped_rows(&Line::from("aaaa bbbb cccc dddd"), 10), 2);
        assert!(wrapped_rows(&Line::from("word ".repeat(100)), 20) >= 25);
        assert_eq!(wrapped_rows(&Line::from("x".repeat(25)), 10), 3);
    }

    #[test]
    fn pre_wrapped_rows_never_exceed_the_width() {
        use super::wrap_rows;
        let text = format!("  {}", "SIGIL keeps a fingerprint of every block, not the block itself. ".repeat(12));
        for width in [20usize, 37, 80, 237] {
            let rows = wrap_rows(&text, width, 2);
            assert!(rows.len() > 1 || width >= text.len());
            for r in &rows { assert!(ratatui::text::Span::raw(r.clone()).width() <= width, "{width}: {r:?}"); }
            let joined: String = rows.iter().map(|r| r.trim()).collect::<Vec<_>>().join(" ");
            assert_eq!(joined.split_whitespace().count(), text.split_whitespace().count(), "no word lost at {width}");
        }
        assert_eq!(wrap_rows("", 10, 2), vec![String::new()]);
    }
}

#[cfg(test)]
mod ai_progress_tests {
    use super::ai_progress;
    #[test]
    fn bar_never_claims_done_before_the_model_does_and_eta_needs_a_rate() {
        let (f, eta) = ai_progress(10_000, 20.0, 300.0);
        assert!(f <= 0.95);
        assert_eq!(eta, Some(0.0));
        assert_eq!(ai_progress(10, 0.0, 300.0).1, None);
        // no history yet → default 300-token target: 150 tok at 20 tok/s → 7.5 s left
        let (f, eta) = ai_progress(150, 20.0, 0.0);
        assert!((f - 0.5).abs() < 1e-9 && (eta.unwrap() - 7.5).abs() < 1e-9);
    }
}
