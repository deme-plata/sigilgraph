//! Terminal display-width math for the TUI. Extracted from `main.rs`.
//!
//! Layout (padding, centering, box-drawing) depends on the *visible* column
//! count of a string, which is NOT its `.len()` or even its `.chars().count()`:
//! ANSI color escapes occupy zero columns, and emoji/CJK occupy two. Getting
//! this wrong misaligns every framed panel, so the rules are unit-tested here.

/// Visible terminal columns a rendered string occupies, ignoring ANSI SGR
/// escape sequences (`\x1b…m`) and counting wide glyphs as two columns.
pub(crate) fn display_width(s: &str) -> usize {
    let mut w = 0usize;
    let mut in_esc = false;
    for ch in s.chars() {
        if in_esc {
            if ch == 'm' {
                in_esc = false;
            }
            continue;
        }
        if ch == '\x1b' {
            in_esc = true;
            continue;
        }
        w += char_cols(ch);
    }
    w
}

/// Terminal column count for one char. Covers the emoji/CJK ranges this UI can
/// actually reach; text-presentation marks (✓ ✗ · … µ → ∀ ⊃ ● █ ░) stay 1 col.
pub(crate) fn char_cols(c: char) -> usize {
    let u = c as u32;
    let wide = (0x1100..=0x115F).contains(&u)   // Hangul Jamo
        || (0x2B00..=0x2BFF).contains(&u)        // ⬆ ⬡ and friends (emoji arrows/symbols)
        || (0x1F000..=0x1FAFF).contains(&u)      // emoji
        || (0x2E80..=0xA4CF).contains(&u)        // CJK
        || (0xFF00..=0xFF60).contains(&u)        // fullwidth forms
        || matches!(u, 0x26CF | 0x26A1 | 0x231B | 0x23F3); // ⛏ ⚡ ⌛ ⏳ (emoji-presentation)
    if u == 0 { 0 } else if wide { 2 } else { 1 }
}

#[cfg(test)]
mod text_width_tests {
    use super::{char_cols, display_width};

    #[test]
    fn char_cols_ascii_and_text_marks_are_one_column() {
        for c in ['a', 'Z', '9', ' ', '·', '✓', '✗', '…', 'µ', '→', '●', '█', '░'] {
            assert_eq!(char_cols(c), 1, "{c:?} should be 1 column");
        }
        assert_eq!(char_cols('\0'), 0, "NUL occupies no column");
    }

    #[test]
    fn char_cols_emoji_and_cjk_are_two_columns() {
        for c in ['⛏', '⚡', '⌛', '⏳', '好', '🚀', '🔥'] {
            assert_eq!(char_cols(c), 2, "{c:?} should be 2 columns");
        }
        // Documented scope limit, locked on purpose: char_cols covers only the
        // ranges THIS UI reaches. Hangul *syllables* (U+AC00..U+D7AF) are outside
        // them and measure as 1 — fine, the TUI never renders Korean. Pinned so a
        // future reader sees the gap is intentional, not an oversight.
        assert_eq!(char_cols('한'), 1);
    }

    #[test]
    fn display_width_ignores_ansi_and_counts_wide_glyphs() {
        assert_eq!(display_width("hello"), 5);
        // ANSI color codes occupy zero visible columns.
        assert_eq!(display_width("\x1b[38;5;220mhello\x1b[0m"), 5);
        // A wide glyph is two columns; the surrounding color is still zero.
        assert_eq!(display_width("\x1b[1m⚡\x1b[0m go"), 2 + 1 + 2); // ⚡ + space + "go"
        assert_eq!(display_width(""), 0);
    }
}
