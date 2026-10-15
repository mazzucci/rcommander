//! Shared model for rcommander. No I/O here, so it compiles for both native and wasm32.

pub mod api;
pub mod diff;
pub mod dircmp;
mod entry;
pub mod format;
mod location;
pub mod panel;

pub use diff::{DiffResult, diff_text};
pub use entry::{Entry, Listing};
pub use location::Location;
pub use panel::{Panel, Row, SortKey};

/// Which two files "Compare by content" (F2) should diff, left side first.
///
/// - Exactly two files selected in the active panel: those two.
/// - Otherwise: the file under the active cursor against the file under the other cursor,
///   or, if that is not a file, the entry with the same name in the other panel's directory.
///
/// The file from the left panel is always the left side of the diff.
pub fn diff_targets(left: &Panel, right: &Panel, left_active: bool) -> Option<(Location, Location)> {
    let (active, other) = if left_active { (left, right) } else { (right, left) };
    let selected: Vec<&Entry> = active.targets().into_iter().filter(|e| !e.is_dir).collect();
    if active.selected.len() == 2 && selected.len() == 2 {
        return Some((
            active.location.join(&selected[0].name),
            active.location.join(&selected[1].name),
        ));
    }
    let mine = active.current().filter(|e| !e.is_dir)?;
    let theirs = match other.current() {
        Some(e) if !e.is_dir => other.location.join(&e.name),
        _ => other.location.join(&mine.name),
    };
    let mine = active.location.join(&mine.name);
    Some(if left_active { (mine, theirs) } else { (theirs, mine) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panel(dir: &str, files: &[&str]) -> Panel {
        let mut p = Panel::new(Location::local(dir));
        p.set_listing(
            Listing {
                location: Location::local(dir),
                entries: files
                    .iter()
                    .map(|f| Entry::file(*f, 1))
                    .chain([Entry::dir("sub")])
                    .collect(),
                writable: true,
            },
            None,
        );
        p
    }

    #[test]
    fn diff_targets_cursor_and_selection() {
        let mut l = panel("/l", &["a.txt", "b.txt"]);
        let mut r = panel("/r", &["a.txt", "c.txt"]);
        l.focus_name("a.txt");
        r.focus_name("c.txt");
        let loc = |s: &str| Location::local(s);
        assert_eq!(diff_targets(&l, &r, true), Some((loc("/l/a.txt"), loc("/r/c.txt"))));
        // Right panel active: still left file on the left.
        assert_eq!(diff_targets(&l, &r, false), Some((loc("/l/a.txt"), loc("/r/c.txt"))));
        // Other cursor on a directory: same name on the other side.
        r.focus_name("sub");
        assert_eq!(diff_targets(&l, &r, true), Some((loc("/l/a.txt"), loc("/r/a.txt"))));
        // Two selected files in the active panel.
        l.select_mask("*.txt", true);
        assert_eq!(diff_targets(&l, &r, true), Some((loc("/l/a.txt"), loc("/l/b.txt"))));
        // Cursor on a directory with no selection: nothing to diff.
        l.clear_selection();
        l.focus_name("sub");
        assert_eq!(diff_targets(&l, &r, true), None);
    }
}
