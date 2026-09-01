//! Console glyph subsystem — extracted from main.rs (god-file split, 2026-09-01).
//! Legacy Windows conhost renders emoji-class glyphs at the wrong cell width and smears
//! ratatui's layout; ASCII mode maps them to width-1 stand-ins so alignment stays exact.
//! Pure leaf module (std only) — no other sigil-top item depends on it beyond these calls.

/// v0.33: Windows consoles render emoji-class glyphs (⛏ ⛓ ⬇ ▲ ●) at the WRONG
/// cell width vs what `unicode-width` (ratatui's layout) assumes → every cell after
/// them shifts and the whole TUI "smears". ASCII mode swaps those for width-1 ASCII
/// so layout is exact everywhere. Auto-on for Windows; `SIGIL_ASCII=0` forces it off,
/// `SIGIL_ASCII=1` forces it on (e.g. a Linux box over a dumb terminal).
static UI_ASCII: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub(crate) fn ui_ascii() -> bool { UI_ASCII.load(std::sync::atomic::Ordering::Relaxed) }
pub(crate) fn init_ui_ascii() {
    let on = match std::env::var("SIGIL_ASCII").ok().as_deref() {
        Some("1") | Some("true") => true,
        Some("0") | Some("false") => false,
        // v0.33.2: default RICH on modern terminals. Windows Terminal (WT_SESSION /
        // WT_PROFILE_ID), VS Code's integrated terminal (TERM_PROGRAM=vscode), and every
        // *nix terminal render Unicode + emoji at the correct cell width → full rich icons.
        // Only LEGACY Windows conhost (none of those signals) falls back to refined ASCII.
        _ => {
            cfg!(windows)
                && std::env::var_os("WT_SESSION").is_none()
                && std::env::var_os("WT_PROFILE_ID").is_none()
                && std::env::var("TERM_PROGRAM").ok().as_deref() != Some("vscode")
        }
    };
    UI_ASCII.store(on, std::sync::atomic::Ordering::Relaxed);
}
/// Sanitize a UI string for legacy ASCII terminals: replace ONLY true emoji-presentation
/// (width-2-in-font, width-1-in-unicode-width) glyphs that smear layout on old conhost.
/// v0.33.2: width-1 BMP symbols (● ○ ✓ → ▲ ▼ █ ░ ▌ ◆ Δ Ε ≈ box-drawing) render AND align
/// fine even on legacy conhost — keeping them is what restores "rich text + icons". Modern
/// terminals (Windows Terminal, VS Code, any *nix) skip this entirely (ui_ascii=false).
pub(crate) fn sa<S: Into<String>>(s: S) -> String {
    let s = s.into();
    if !ui_ascii() { return s; }
    // v0.33.5: CP437 raster consoles (classic conhost) LACK heavy/rounded/diagonal box-
    // drawing, geometric icons, arrows and emoji → they render as `?`. Map every such glyph
    // to a CP437-safe ASCII stand-in. KEEP (fall through `other`): light box-drawing
    // (─│┌┐└┘├┤┬┴┼) + block elements (█▓▒░▌▐) + · µ — those ARE in CP437 and render fine.
    s.chars().map(|c| match c {
        '◆' | '✦' | '✶' | '★' | '◇' | '⬣' | '⬢' | '⬡' | '🏆' => '*',
        '◈' | '▦' | '⛓' | '▩' | '▣' => '#',
        '●' | '◐' => 'o', '○' | '◦' | '∙' => '.',
        '✓' | '✔' => 'v', '✗' | '✘' | '✕' | '×' | '╳' => 'x',
        '→' | '➜' | '↩' | '⟳' | '➤' => '>', '←' => '<',
        '⬆' | '↑' | '▲' => '^', '⬇' | '↓' | '▼' => 'v',
        '≈' => '~', '…' => '.', '⚡' | '⚠' | '⛏' => '!',
        '‹' | '«' => '<', '›' | '»' => '>',
        '╱' => '/', '╲' => '\\', '▕' | '▏' | '▎' | '▍' => '|',
        '━' => '-', '┃' => '|',
        '┏' | '┓' | '┗' | '┛' | '╭' | '╮' | '╰' | '╯' => '+',
        'Δ' => 'D', 'Ε' => 'E',
        other => other,
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn sa_passes_through_in_rich_mode() {
        // Rich terminals keep the real glyphs — sa must be an identity.
        UI_ASCII.store(false, Ordering::Relaxed);
        assert_eq!(sa("◆ mined ⛏ 100%"), "◆ mined ⛏ 100%");
    }

    #[test]
    fn sa_maps_wide_glyphs_in_ascii_mode() {
        // Legacy conhost: emoji-presentation glyphs that smear layout become width-1 ASCII;
        // CP437-safe glyphs (box-drawing, blocks, ·) fall through untouched.
        UI_ASCII.store(true, Ordering::Relaxed);
        assert_eq!(sa("◆"), "*");
        assert_eq!(sa("✓ ✗"), "v x");
        assert_eq!(sa("→ ← ▲ ▼"), "> < ^ v");
        assert_eq!(sa("⚡ done"), "! done");
        // every output char must be single-width ASCII (the whole point).
        assert!(sa("◆✓→⚡⛓").chars().all(|c| c.is_ascii()));
        // pass-through set stays intact.
        assert_eq!(sa("─│█░·"), "─│█░·");
        UI_ASCII.store(false, Ordering::Relaxed); // don't leak state to other tests
    }
}
