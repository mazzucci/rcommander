//! Front-end independent state of one file panel: cursor, selection, sorting, filtering.

use crate::dircmp::CmpMark;
use crate::{Entry, Listing, Location};
use std::collections::{BTreeSet, HashMap};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Ext,
    Size,
    Modified,
}

impl SortKey {
    pub fn next(self) -> SortKey {
        match self {
            SortKey::Name => SortKey::Ext,
            SortKey::Ext => SortKey::Size,
            SortKey::Size => SortKey::Modified,
            SortKey::Modified => SortKey::Name,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SortKey::Name => "name",
            SortKey::Ext => "ext",
            SortKey::Size => "size",
            SortKey::Modified => "date",
        }
    }
}

/// A visible row: the synthetic ".." row or an index into the entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Parent,
    Entry(usize),
}

#[derive(Clone, Debug)]
pub struct Panel {
    pub location: Location,
    pub writable: bool,
    entries: Vec<Entry>,
    rows: Vec<Row>,
    pub cursor: usize,
    pub selected: BTreeSet<String>,
    pub sort: SortKey,
    pub sort_desc: bool,
    pub filter: String,
    pub show_hidden: bool,
    /// Directory-compare marks, keyed by entry name. Cleared on navigation.
    pub marks: HashMap<String, CmpMark>,
    /// Bumped whenever the rows or their contents change (not on cursor moves or
    /// selection), so views can skip rebuilding a large list.
    version: u64,
}

impl Panel {
    pub fn new(location: Location) -> Self {
        Panel {
            location,
            writable: false,
            entries: Vec::new(),
            rows: Vec::new(),
            cursor: 0,
            selected: BTreeSet::new(),
            sort: SortKey::Name,
            sort_desc: false,
            filter: String::new(),
            show_hidden: false,
            marks: HashMap::new(),
            version: 0,
        }
    }

    /// Replace contents with a fresh listing. If the location is unchanged
    /// (a refresh) the cursor and selection are kept where possible.
    /// `focus` puts the cursor on a named entry (e.g. the dir we came up from).
    pub fn set_listing(&mut self, listing: Listing, focus: Option<&str>) {
        let same_place = listing.location == self.location;
        let prev_name = self.current().map(|e| e.name.clone());
        self.location = listing.location;
        self.writable = listing.writable;
        self.entries = listing.entries;
        if same_place {
            let names: BTreeSet<&str> = self.entries.iter().map(|e| e.name.as_str()).collect();
            self.selected.retain(|n| names.contains(n.as_str()));
        } else {
            self.selected.clear();
            self.filter.clear();
            self.marks.clear();
            self.cursor = 0;
        }
        self.rebuild();
        let focus = focus.map(str::to_string).or(if same_place { prev_name } else { None });
        if let Some(name) = focus {
            self.focus_name(&name);
        }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn row_entry(&self, row: usize) -> Option<&Entry> {
        match self.rows.get(row)? {
            Row::Parent => None,
            Row::Entry(i) => self.entries.get(*i),
        }
    }

    pub fn current_row(&self) -> Option<Row> {
        self.rows.get(self.cursor).copied()
    }

    pub fn current(&self) -> Option<&Entry> {
        self.row_entry(self.cursor)
    }

    pub fn is_selected(&self, entry: &Entry) -> bool {
        self.selected.contains(&entry.name)
    }

    pub fn focus_name(&mut self, name: &str) {
        if let Some(pos) = (0..self.rows.len()).find(|&r| self.row_entry(r).is_some_and(|e| e.name == name)) {
            self.cursor = pos;
        }
    }

    pub fn move_cursor(&mut self, delta: isize) {
        if self.rows.is_empty() {
            self.cursor = 0;
            return;
        }
        let max = self.rows.len() as isize - 1;
        self.cursor = (self.cursor as isize + delta).clamp(0, max) as usize;
    }

    pub fn cursor_home(&mut self) {
        self.cursor = 0;
    }

    pub fn cursor_end(&mut self) {
        self.cursor = self.rows.len().saturating_sub(1);
    }

    /// Toggle selection of the entry under the cursor and move down (Insert / Space).
    pub fn toggle_current(&mut self) {
        if let Some(name) = self.current().map(|e| e.name.clone())
            && !self.selected.remove(&name)
        {
            self.selected.insert(name);
        }
        self.move_cursor(1);
    }

    /// Select (or deselect) all visible entries matching a wildcard mask like `*.rs`.
    /// Directories are only matched when the mask contains no dot, like Total Commander.
    pub fn select_mask(&mut self, mask: &str, select: bool) -> usize {
        let names: Vec<String> = self
            .visible_entries()
            .filter(|e| !e.is_dir || !mask.contains('.'))
            .filter(|e| wildcard_match(mask, &e.name))
            .map(|e| e.name.clone())
            .collect();
        for n in &names {
            if select {
                self.selected.insert(n.clone());
            } else {
                self.selected.remove(n);
            }
        }
        names.len()
    }

    pub fn invert_selection(&mut self) {
        let names: Vec<String> = self.visible_entries().map(|e| e.name.clone()).collect();
        for n in names {
            if !self.selected.remove(&n) {
                self.selected.insert(n);
            }
        }
    }

    pub fn clear_selection(&mut self) {
        self.selected.clear();
    }

    /// The entries an operation applies to: the visible selection, or else the entry under
    /// the cursor. Selected entries hidden by the filter or the hidden-files toggle are
    /// never included, so an operation only touches what the user can see.
    pub fn targets(&self) -> Vec<&Entry> {
        let selected: Vec<&Entry> = self.visible_entries().filter(|e| self.is_selected(e)).collect();
        if selected.is_empty() {
            self.current().into_iter().collect()
        } else {
            selected
        }
    }

    /// Count and total size of the visible selection.
    pub fn selection_summary(&self) -> (usize, u64) {
        self.visible_entries()
            .filter(|e| self.is_selected(e))
            .fold((0, 0), |(n, sz), e| (n + 1, sz + e.size.unwrap_or(0)))
    }

    pub fn set_sort(&mut self, key: SortKey) {
        if self.sort == key {
            self.sort_desc = !self.sort_desc;
        } else {
            self.sort = key;
            self.sort_desc = false;
        }
        self.rebuild_keep_cursor();
    }

    pub fn cycle_sort(&mut self) {
        self.sort = self.sort.next();
        self.sort_desc = false;
        self.rebuild_keep_cursor();
    }

    /// Narrow the list to names containing `filter`. While filtering, the cursor moves off
    /// ".." to the first match so Enter/F-keys act on what was searched for.
    pub fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_string();
        self.rebuild_keep_cursor();
        if !self.filter.is_empty() && self.current_row() == Some(Row::Parent) && self.rows.len() > 1 {
            self.cursor = 1;
        }
    }

    /// See the `version` field.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Replace the directory-compare marks.
    pub fn set_marks(&mut self, marks: HashMap<String, CmpMark>) {
        self.marks = marks;
        self.version += 1;
    }

    pub fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.rebuild_keep_cursor();
    }

    fn visible_entries(&self) -> impl Iterator<Item = &Entry> {
        self.rows.iter().filter_map(|r| match r {
            Row::Parent => None,
            Row::Entry(i) => self.entries.get(*i),
        })
    }

    fn rebuild_keep_cursor(&mut self) {
        let name = self.current().map(|e| e.name.clone());
        self.rebuild();
        if let Some(n) = name {
            self.focus_name(&n);
        }
    }

    fn rebuild(&mut self) {
        self.version += 1;
        let filter = self.filter.to_lowercase();
        let mut idx: Vec<usize> = (0..self.entries.len())
            .filter(|&i| {
                let e = &self.entries[i];
                (self.show_hidden || !e.hidden) && (filter.is_empty() || e.name.to_lowercase().contains(&filter))
            })
            .collect();
        let entries = &self.entries;
        let (key, desc) = (self.sort, self.sort_desc);
        idx.sort_by(|&a, &b| {
            let (a, b) = (&entries[a], &entries[b]);
            // Directories always first, regardless of direction.
            b.is_dir.cmp(&a.is_dir).then_with(|| {
                let ord = match key {
                    SortKey::Name => natural_cmp(&a.name, &b.name),
                    SortKey::Ext => a
                        .extension()
                        .to_lowercase()
                        .cmp(&b.extension().to_lowercase())
                        .then_with(|| natural_cmp(&a.name, &b.name)),
                    SortKey::Size => a.size.cmp(&b.size).then_with(|| natural_cmp(&a.name, &b.name)),
                    SortKey::Modified => a.modified.cmp(&b.modified).then_with(|| natural_cmp(&a.name, &b.name)),
                };
                if desc { ord.reverse() } else { ord }
            })
        });
        self.rows.clear();
        if self.location.parent().is_some() {
            self.rows.push(Row::Parent);
        }
        self.rows.extend(idx.into_iter().map(Row::Entry));
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
    }
}

/// Case-insensitive `*` / `?` wildcard match. Several masks may be separated by `;` or spaces.
/// As in Total Commander, `*.*` matches every name, including ones without an extension.
pub fn wildcard_match(mask: &str, name: &str) -> bool {
    let name: Vec<char> = name.to_lowercase().chars().collect();
    mask.split([';', ' '])
        .filter(|p| !p.is_empty())
        .any(|p| p == "*.*" || glob(&p.to_lowercase().chars().collect::<Vec<_>>(), &name))
}

/// Iterative glob with single-star backtracking: O(pattern × name), no exponential blowup.
fn glob(p: &[char], s: &[char]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let mut star: Option<(usize, usize)> = None; // (pattern index after '*', name index it matched up to)
    while si < s.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some((pi + 1, si));
                pi += 1;
            }
            Some(&c) if c == '?' || c == s[si] => {
                pi += 1;
                si += 1;
            }
            _ => match star {
                // Let the last '*' swallow one more character and retry.
                Some((sp, ss)) => {
                    star = Some((sp, ss + 1));
                    pi = sp;
                    si = ss + 1;
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// Case-insensitive comparison treating digit runs as numbers ("file2" < "file10").
/// Names that compare equal this way ("A.txt"/"a.txt", "a01"/"a1") fall back to plain
/// byte order so the sort is total and stable across refreshes.
pub fn natural_cmp(a_str: &str, b_str: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut a, mut b) = (a_str.chars().peekable(), b_str.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return a_str.cmp(b_str),
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut s = String::new();
                    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
                        s.push(c);
                        it.next();
                    }
                    s
                };
                let (na, nb) = (take(&mut a), take(&mut b));
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                let ord = x.to_lowercase().cmp(y.to_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                a.next();
                b.next();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panel() -> Panel {
        let mut p = Panel::new(Location::local("/x"));
        p.set_listing(
            Listing {
                location: Location::local("/x"),
                entries: vec![
                    Entry::file("b10.rs", 5),
                    Entry::file("b2.rs", 50),
                    Entry::dir("src"),
                    Entry {
                        hidden: true,
                        ..Entry::file(".env", 1)
                    },
                    Entry::file("README.md", 10),
                ],
                writable: true,
            },
            None,
        );
        p
    }

    fn names(p: &Panel) -> Vec<String> {
        (0..p.len())
            .map(|r| p.row_entry(r).map_or("..".into(), |e| e.name.clone()))
            .collect()
    }

    #[test]
    fn sorts_dirs_first_and_naturally() {
        let mut p = panel();
        assert_eq!(names(&p), ["..", "src", "b2.rs", "b10.rs", "README.md"]);
        p.set_sort(SortKey::Size);
        assert_eq!(names(&p), ["..", "src", "b10.rs", "README.md", "b2.rs"]);
        p.set_sort(SortKey::Size);
        assert_eq!(names(&p), ["..", "src", "b2.rs", "README.md", "b10.rs"]);
        p.toggle_hidden();
        assert!(names(&p).contains(&".env".to_string()));
    }

    #[test]
    fn selection_and_targets() {
        let mut p = panel();
        assert_eq!(p.select_mask("*.rs", true), 2);
        assert_eq!(p.targets().len(), 2);
        p.clear_selection();
        p.cursor = 2;
        assert_eq!(p.targets()[0].name, "b2.rs");
        p.toggle_current();
        assert_eq!(p.cursor, 3);
        assert!(p.selected.contains("b2.rs"));
    }

    #[test]
    fn filter_and_refresh_keep_cursor() {
        let mut p = panel();
        p.focus_name("README.md");
        p.set_filter("read");
        assert_eq!(p.current().unwrap().name, "README.md");
        let listing = Listing {
            location: Location::local("/x"),
            entries: p.entries().to_vec(),
            writable: true,
        };
        p.set_listing(listing, None);
        assert_eq!(p.current().unwrap().name, "README.md");
    }

    #[test]
    fn operations_only_target_visible_selection() {
        let mut p = panel();
        p.toggle_hidden();
        p.select_mask("*", true);
        assert_eq!(p.selection_summary().0, 5);
        p.toggle_hidden();
        p.set_filter("b");
        let names: Vec<&str> = p.targets().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["b2.rs", "b10.rs"]);
        assert_eq!(p.selection_summary(), (2, 55));
    }

    #[test]
    fn filtering_moves_cursor_to_first_match() {
        let mut p = panel();
        p.cursor_home();
        p.set_filter("read");
        assert_eq!(p.current().unwrap().name, "README.md");
    }

    #[test]
    fn version_changes_with_rows_not_with_cursor_or_selection() {
        let mut p = panel();
        let v = p.version();
        p.move_cursor(1);
        p.toggle_current();
        assert_eq!(p.version(), v);
        p.set_filter("b");
        assert!(p.version() > v);
        let v = p.version();
        p.set_marks(HashMap::new());
        assert!(p.version() > v);
    }

    #[test]
    fn invert_includes_directories() {
        let mut p = panel();
        p.focus_name("src");
        p.toggle_current();
        p.invert_selection();
        assert!(!p.selected.contains("src"));
        assert_eq!(p.selected.len(), 3);
    }

    #[test]
    fn navigation_resets_state_and_focuses() {
        let mut p = panel();
        p.select_mask("*", true);
        p.set_filter("b");
        let listing = Listing {
            location: Location::local("/y"),
            entries: vec![Entry::file("one", 1), Entry::file("two", 1)],
            writable: true,
        };
        p.set_listing(listing, Some("two"));
        assert!(p.selected.is_empty() && p.filter.is_empty());
        assert_eq!(p.current().unwrap().name, "two");
    }

    #[test]
    fn natural_order_is_total() {
        use std::cmp::Ordering;
        assert_eq!(natural_cmp("file2", "file10"), Ordering::Less);
        assert_ne!(natural_cmp("A.txt", "a.txt"), Ordering::Equal);
        assert_ne!(natural_cmp("a01", "a1"), Ordering::Equal);
    }

    #[test]
    fn wildcard_is_not_exponential() {
        let start = std::time::Instant::now();
        assert!(!wildcard_match("*a*a*a*a*a*a*a*a*a*a*b", &"a".repeat(40)));
        assert!(start.elapsed().as_millis() < 100);
        assert!(wildcard_match("*", ""));
        assert!(wildcard_match("a*", "a"));
        assert!(!wildcard_match("a?", "a"));
        assert!(wildcard_match("*.*", "Makefile"));
    }

    #[test]
    fn wildcards() {
        assert!(wildcard_match("*.RS", "main.rs"));
        assert!(wildcard_match("*.md;*.rs", "a.rs"));
        assert!(wildcard_match("a?c", "abc"));
        assert!(!wildcard_match("*.rs", "main.rsx"));
    }
}
