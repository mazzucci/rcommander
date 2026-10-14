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
        let names: Vec<String> = self
            .visible_entries()
            .filter(|e| !e.is_dir)
            .map(|e| e.name.clone())
            .collect();
        for n in names {
            if !self.selected.remove(&n) {
                self.selected.insert(n);
            }
        }
    }

    pub fn clear_selection(&mut self) {
        self.selected.clear();
    }

    /// The entries an operation applies to: the selection, or else the entry under the cursor.
    pub fn targets(&self) -> Vec<&Entry> {
        if self.selected.is_empty() {
            self.current().into_iter().collect()
        } else {
            self.entries
                .iter()
                .filter(|e| self.selected.contains(&e.name))
                .collect()
        }
    }

    pub fn selection_summary(&self) -> (usize, u64) {
        self.entries
            .iter()
            .filter(|e| self.selected.contains(&e.name))
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

    pub fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_string();
        self.rebuild_keep_cursor();
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
pub fn wildcard_match(mask: &str, name: &str) -> bool {
    fn m(p: &[char], s: &[char]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some('*'), _) => m(&p[1..], s) || (!s.is_empty() && m(p, &s[1..])),
            (Some('?'), Some(_)) => m(&p[1..], &s[1..]),
            (Some(a), Some(b)) => a == b && m(&p[1..], &s[1..]),
            _ => false,
        }
    }
    let name: Vec<char> = name.to_lowercase().chars().collect();
    mask.split([';', ' '])
        .filter(|p| !p.is_empty())
        .any(|p| m(&p.to_lowercase().chars().collect::<Vec<_>>(), &name))
}

/// Case-insensitive comparison treating digit runs as numbers ("file2" < "file10").
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
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
    fn wildcards() {
        assert!(wildcard_match("*.RS", "main.rs"));
        assert!(wildcard_match("*.md;*.rs", "a.rs"));
        assert!(wildcard_match("a?c", "abc"));
        assert!(!wildcard_match("*.rs", "main.rsx"));
    }
}
