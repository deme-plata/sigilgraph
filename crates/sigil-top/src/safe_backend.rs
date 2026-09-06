//! A ratatui `Backend` wrapper that clamps a degenerate console size to a
//! paintable fallback. Extracted from main.rs (god-file split).
//!
//! Some Windows consoles report `Ok((0,0))` — or error the size query outright —
//! at startup. ratatui's autoresize would then hand `term.draw` a zero-size
//! buffer (or the error would abort the draw and exit the TUI). `size()` here
//! substitutes a 120×30 fallback for both cases; every other call delegates
//! straight to the inner `CrosstermBackend`.

pub(crate) struct SafeSizeBackend<W: std::io::Write> {
    pub(crate) inner: ratatui::backend::CrosstermBackend<W>,
}
impl<W: std::io::Write> std::io::Write for SafeSizeBackend<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> { self.inner.write(buf) }
    fn flush(&mut self) -> std::io::Result<()> { std::io::Write::flush(&mut self.inner) }
}
impl<W: std::io::Write> ratatui::backend::Backend for SafeSizeBackend<W> {
    fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        self.inner.draw(content)
    }
    fn hide_cursor(&mut self) -> std::io::Result<()> { self.inner.hide_cursor() }
    fn show_cursor(&mut self) -> std::io::Result<()> { self.inner.show_cursor() }
    fn get_cursor_position(&mut self) -> std::io::Result<ratatui::layout::Position> {
        self.inner.get_cursor_position()
    }
    fn set_cursor_position<P: Into<ratatui::layout::Position>>(&mut self, position: P) -> std::io::Result<()> {
        self.inner.set_cursor_position(position)
    }
    fn clear(&mut self) -> std::io::Result<()> { self.inner.clear() }
    fn size(&self) -> std::io::Result<ratatui::layout::Size> {
        // Clamp BOTH a degenerate Ok((0,0)) AND an Err (some Windows consoles error the
        // size query at startup) to a paintable fallback, so ratatui's autoresize never
        // gets a 0-size buffer NOR a failure that would abort term.draw and exit the TUI.
        match self.inner.size() {
            Ok(s) if s.width >= 2 && s.height >= 2 => Ok(clamp_cell_budget(s)),
            _ => Ok(ratatui::layout::Size::new(120, 30)),
        }
    }
    fn window_size(&mut self) -> std::io::Result<ratatui::backend::WindowSize> { self.inner.window_size() }
    fn flush(&mut self) -> std::io::Result<()> { ratatui::backend::Backend::flush(&mut self.inner) }
}

/// The UPPER clamp — the other half of the degenerate-size problem, and the one that
/// actually crashed a running TUI (2026-09-06).
///
/// ratatui 0.28's `Rect::area()` returns a **u16** and SATURATES:
///
/// ```text
/// pub const fn area(self) -> u16 { self.width.saturating_mul(self.height) }
/// ```
///
/// `Buffer::resize` then allocates `area.area() as usize` cells while storing the FULL
/// width/height in `Buffer::area`. So on any terminal with more than 65,535 cells — a
/// maximised window at a small font, e.g. 300x220 = 66,000 — the buffer holds 65,535
/// cells but claims to describe 66,000 positions. `Buffer::index_of` computes
/// `(y - area.y) * area.width + (x - area.x)`, runs past the end, and every frame panics:
///
/// ```text
/// index out of bounds: the len is 65535 but the index is 65535
///   @ ratatui-0.28.1/src/buffer/buffer.rs:385   (Buffer::set_style)
/// ```
///
/// `intersection()` does not save you: the rect is inside `Buffer::area`, it is the
/// BACKING STORE that is short. So the guard has to be here, before ratatui ever sees
/// the size.
///
/// Keep the full width (this TUI's panels are wide and a narrowed one reads badly) and
/// trim height until the cell budget fits; only if the width alone cannot admit two rows
/// do we narrow it as well. The result is always paintable and always consistent.
pub(crate) fn clamp_cell_budget(s: ratatui::layout::Size) -> ratatui::layout::Size {
    const MAX_CELLS: u32 = u16::MAX as u32;
    let (mut w, mut h) = (s.width, s.height);
    if u32::from(w) * u32::from(h) <= MAX_CELLS {
        return s;
    }
    // Widest width that still leaves room for 2 rows.
    if u32::from(w) * 2 > MAX_CELLS {
        w = (MAX_CELLS / 2) as u16;
    }
    h = (MAX_CELLS / u32::from(w).max(1)).min(u32::from(h)).max(2) as u16;
    ratatui::layout::Size::new(w, h)
}

#[cfg(test)]
mod tests {
    use super::clamp_cell_budget;
    use ratatui::layout::Size;

    /// An ordinary terminal is handed through untouched.
    #[test]
    fn ordinary_size_is_left_alone() {
        let s = clamp_cell_budget(Size::new(200, 60));
        assert_eq!((s.width, s.height), (200, 60));
    }

    /// The exact shape that crashed the live TUI: 300x220 = 66,000 cells, 465 over the
    /// u16 budget. Width is preserved, height trimmed to fit.
    #[test]
    fn oversized_terminal_is_trimmed_to_the_cell_budget() {
        let s = clamp_cell_budget(Size::new(300, 220));
        assert_eq!(s.width, 300, "width should be preserved");
        assert!(
            u32::from(s.width) * u32::from(s.height) <= u32::from(u16::MAX),
            "clamped size {}x{} still exceeds the u16 cell budget",
            s.width,
            s.height
        );
        assert!(s.height >= 2, "must stay paintable");
    }

    /// A width so large that even two rows overflow: the width must give way too, and the
    /// result must still be paintable rather than zero-sized.
    #[test]
    fn absurd_width_narrows_and_stays_paintable() {
        let s = clamp_cell_budget(Size::new(60_000, 10));
        assert!(u32::from(s.width) * u32::from(s.height) <= u32::from(u16::MAX));
        assert!(s.width >= 2 && s.height >= 2);
    }

    /// Whatever comes out must never make `Rect::area()` saturate — that saturation is
    /// precisely what desynchronises `Buffer::area` from its backing store.
    #[test]
    fn clamped_size_never_saturates_rect_area() {
        for (w, h) in [(300u16, 220u16), (1000, 900), (65535, 65535), (400, 170)] {
            let s = clamp_cell_budget(Size::new(w, h));
            let cells = u32::from(s.width) * u32::from(s.height);
            assert!(cells <= u32::from(u16::MAX), "{w}x{h} -> {}x{}", s.width, s.height);
            let r = ratatui::layout::Rect::new(0, 0, s.width, s.height);
            assert_eq!(
                u32::from(r.area()),
                cells,
                "Rect::area() saturated for {}x{} — the buffer would be short",
                s.width,
                s.height
            );
        }
    }
}
