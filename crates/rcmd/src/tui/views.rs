//! Full-screen viewer (F3) and diff (F2) state.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rc_core::api::{DiffResponse, FileContent};
use rc_core::diff::Visible;
use rc_core::format::human_size;

/// Unchanged lines kept around each change when folding.
const CONTEXT: usize = 3;

pub struct Viewer {
    pub title: String,
    pub lines: Vec<String>,
    pub scroll: usize,
    pub hscroll: usize,
    /// Visible text rows, set by the renderer.
    pub page: usize,
}

impl Viewer {
    pub fn new(content: FileContent) -> Self {
        let mut title = format!("{}  ({})", content.location, human_size(content.size));
        let lines = match &content.text {
            Some(text) => text.lines().map(sanitize).collect(),
            None => vec![format!("Binary file, {} bytes.", content.size)],
        };
        if content.truncated {
            title += "  [truncated]";
        }
        Viewer {
            title,
            lines,
            scroll: 0,
            hscroll: 0,
            page: 20,
        }
    }

    /// Returns true when the viewer should close.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let max = self.lines.len().saturating_sub(self.page);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q' | '3') | KeyCode::F(3) | KeyCode::F(10) => return true,
            code => scroll_key(code, &mut self.scroll, &mut self.hscroll, self.page, max),
        }
        false
    }
}

pub struct DiffView {
    pub response: DiffResponse,
    pub fold: bool,
    /// Rows (or folds) currently shown, in order.
    pub items: Vec<Visible>,
    pub scroll: usize,
    pub hscroll: usize,
    /// Index into `hunks` of the change last jumped to.
    pub hunk: Option<usize>,
    pub page: usize,
}

impl DiffView {
    pub fn new(response: DiffResponse) -> Self {
        let mut view = DiffView {
            response,
            fold: true,
            items: Vec::new(),
            scroll: 0,
            hscroll: 0,
            hunk: None,
            page: 20,
        };
        view.rebuild();
        view
    }

    fn rebuild(&mut self) {
        let Some(result) = &self.response.result else { return };
        self.items = if self.fold {
            result.visible(CONTEXT)
        } else {
            (0..result.rows.len()).map(Visible::Row).collect()
        };
    }

    pub fn hunks(&self) -> &[usize] {
        self.response.result.as_ref().map_or(&[], |r| &r.hunks)
    }

    /// Scroll so hunk `idx` sits a few lines below the top.
    fn jump(&mut self, idx: usize) {
        let Some(&row) = self.hunks().get(idx) else { return };
        self.hunk = Some(idx);
        if let Some(pos) = self
            .items
            .iter()
            .position(|v| matches!(v, Visible::Row(r) if *r == row))
        {
            self.scroll = pos.saturating_sub(CONTEXT);
        }
    }

    /// Returns true when the view should close.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let n = self.hunks().len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q' | '2') | KeyCode::F(2) | KeyCode::F(10) => return true,
            KeyCode::Char('n') if n > 0 => self.jump(self.hunk.map_or(0, |h| (h + 1).min(n - 1))),
            KeyCode::Down if alt && n > 0 => self.jump(self.hunk.map_or(0, |h| (h + 1).min(n - 1))),
            KeyCode::Char('p') if n > 0 => self.jump(self.hunk.map_or(0, |h| h.saturating_sub(1))),
            KeyCode::Up if alt && n > 0 => self.jump(self.hunk.map_or(0, |h| h.saturating_sub(1))),
            KeyCode::Char('f') => {
                self.fold = !self.fold;
                self.rebuild();
                match self.hunk {
                    Some(h) => self.jump(h),
                    None => self.scroll = 0,
                }
            }
            code => {
                let max = self.items.len().saturating_sub(self.page);
                scroll_key(code, &mut self.scroll, &mut self.hscroll, self.page, max);
            }
        }
        false
    }
}

fn scroll_key(code: KeyCode, scroll: &mut usize, hscroll: &mut usize, page: usize, max: usize) {
    match code {
        KeyCode::Up => *scroll = scroll.saturating_sub(1),
        KeyCode::Down => *scroll = (*scroll + 1).min(max),
        KeyCode::PageUp => *scroll = scroll.saturating_sub(page),
        KeyCode::PageDown | KeyCode::Char(' ') => *scroll = (*scroll + page).min(max),
        KeyCode::Home => *scroll = 0,
        KeyCode::End => *scroll = max,
        KeyCode::Left => *hscroll = hscroll.saturating_sub(8),
        KeyCode::Right => *hscroll += 8,
        _ => {}
    }
}

/// Expand tabs and replace control characters so they can't corrupt the terminal.
pub fn sanitize(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for c in line.chars() {
        match c {
            '\t' => out.push_str("    "),
            c if c.is_control() => out.push('·'),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_core::Location;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn viewer_scrolls_within_bounds() {
        let text: String = (0..100).map(|i| format!("line {i}\t\x1b\n")).collect();
        let mut v = Viewer::new(FileContent {
            location: Location::local("/f"),
            size: text.len() as u64,
            text: Some(text),
            truncated: false,
        });
        assert_eq!(v.lines[0], "line 0    ·");
        v.page = 10;
        v.handle_key(press(KeyCode::End));
        assert_eq!(v.scroll, 90);
        v.handle_key(press(KeyCode::PageDown));
        assert_eq!(v.scroll, 90);
        assert!(v.handle_key(press(KeyCode::Esc)));
    }

    #[test]
    fn diff_hunk_navigation_and_folding() {
        let left: String = (0..50).map(|i| format!("{i}\n")).collect();
        let right = left.replace("10\n", "ten\n").replace("40\n", "forty\n");
        let mut d = DiffView::new(DiffResponse {
            left_title: "l".into(),
            right_title: "r".into(),
            result: Some(rc_core::diff_text(&left, &right)),
            note: None,
        });
        assert!(d.items.len() < 50, "unchanged lines are folded");
        d.handle_key(press(KeyCode::Char('n')));
        d.handle_key(press(KeyCode::Char('n')));
        assert_eq!(d.hunk, Some(1));
        let Visible::Row(r) = d.items[d.scroll + CONTEXT] else {
            panic!("expected a row")
        };
        assert_eq!(r, d.hunks()[1]);
        d.handle_key(press(KeyCode::Char('f')));
        assert_eq!(d.items.len(), 50);
        assert_eq!(d.scroll, 40 - CONTEXT);
        d.handle_key(press(KeyCode::Char('p')));
        assert_eq!(d.hunk, Some(0));
    }
}
