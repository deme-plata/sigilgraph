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
            Ok(s) if s.width >= 2 && s.height >= 2 => Ok(s),
            _ => Ok(ratatui::layout::Size::new(120, 30)),
        }
    }
    fn window_size(&mut self) -> std::io::Result<ratatui::backend::WindowSize> { self.inner.window_size() }
    fn flush(&mut self) -> std::io::Result<()> { ratatui::backend::Backend::flush(&mut self.inner) }
}
