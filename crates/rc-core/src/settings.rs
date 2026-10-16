//! User preferences shared by both front ends. The native side persists them as TOML;
//! the browser gets and saves them through the API.

use crate::format::{format_time, human_size, permissions};
use crate::{Entry, SortKey};
use serde::{Deserialize, Serialize};

/// Optional columns. The name column is always shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Column {
    Git,
    Ext,
    Size,
    Modified,
    Created,
    Permissions,
    Owner,
    Group,
}

impl Column {
    /// Every column, in display order.
    pub const ALL: [Column; 8] = [
        Column::Git,
        Column::Ext,
        Column::Size,
        Column::Modified,
        Column::Created,
        Column::Permissions,
        Column::Owner,
        Column::Group,
    ];

    /// Column header; fits within [`Column::width`].
    pub fn title(self) -> &'static str {
        match self {
            Column::Permissions => "Perms",
            other => other.label(),
        }
    }

    /// Full name, for the options dialog.
    pub fn label(self) -> &'static str {
        match self {
            Column::Git => "Git",
            Column::Ext => "Ext",
            Column::Size => "Size",
            Column::Modified => "Modified",
            Column::Created => "Created",
            Column::Permissions => "Permissions",
            Column::Owner => "Owner",
            Column::Group => "Group",
        }
    }

    /// Width in characters (terminal) or a hint for the browser.
    pub fn width(self) -> u16 {
        match self {
            Column::Git => 3,
            Column::Ext => 8,
            Column::Size => 7,
            Column::Modified | Column::Created => 16,
            Column::Permissions => 10,
            Column::Owner | Column::Group => 10,
        }
    }

    /// Parse the settings-file name of a column (`"permissions"`).
    pub fn from_name(name: &str) -> Option<Column> {
        Column::ALL.into_iter().find(|c| c.name() == name)
    }

    /// The settings-file name of a column.
    pub fn name(self) -> &'static str {
        match self {
            Column::Git => "git",
            Column::Ext => "ext",
            Column::Size => "size",
            Column::Modified => "modified",
            Column::Created => "created",
            Column::Permissions => "permissions",
            Column::Owner => "owner",
            Column::Group => "group",
        }
    }

    /// When a panel is too narrow, columns are hidden in this order (highest first).
    fn drop_priority(self) -> u8 {
        match self {
            Column::Created => 7,
            Column::Group => 6,
            Column::Owner => 5,
            Column::Ext => 4,
            Column::Git => 3,
            Column::Modified => 2,
            Column::Permissions => 1,
            Column::Size => 0,
        }
    }

    pub fn right_aligned(self) -> bool {
        matches!(self, Column::Size)
    }

    /// The panel sort order this column's header selects, if the panel can sort by it.
    pub fn sort_key(self) -> Option<SortKey> {
        match self {
            Column::Ext => Some(SortKey::Ext),
            Column::Size => Some(SortKey::Size),
            Column::Modified => Some(SortKey::Modified),
            Column::Git | Column::Created | Column::Permissions | Column::Owner | Column::Group => None,
        }
    }

    /// Cell text for an entry. `utc_offset` converts timestamps to local time.
    pub fn text(self, e: &Entry, utc_offset: impl Fn(i64) -> i64) -> String {
        let time = |t: Option<i64>| t.map(|t| format_time(t, utc_offset(t))).unwrap_or_default();
        match self {
            Column::Git => e.git.map(|g| g.letter().to_string()).unwrap_or_default(),
            Column::Ext => e.extension().to_string(),
            Column::Size if e.is_dir => "—".to_string(),
            Column::Size => e.size.map(human_size).unwrap_or_default(),
            Column::Modified => time(e.modified),
            Column::Created => time(e.created),
            Column::Permissions => e.mode.map(|m| permissions(m, e.is_dir, e.is_link)).unwrap_or_default(),
            Column::Owner => e.owner.clone().unwrap_or_default(),
            Column::Group => e.group.clone().unwrap_or_default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Visible optional columns (shown in [`Column::ALL`] order). Unknown names in the
    /// file (from a newer or older version) are ignored.
    #[serde(deserialize_with = "lenient_columns")]
    pub columns: Vec<Column>,
    /// Show dot-files when a panel opens. `.` toggles per session.
    pub show_hidden: bool,
    /// Terminal UI F4 (Edit) command, e.g. `vim`, `nano`, `hx`. Empty means
    /// `$VISUAL`, then `$EDITOR`, then `vi`.
    pub editor: String,
    /// Browser UI F4 command, which runs without a terminal: a GUI editor such as `code`,
    /// `zed` or `open -t`. Empty means the system default application.
    pub gui_editor: String,
    /// Hotlist directories (Ctrl+D), in the order shown.
    pub bookmarks: Vec<String>,
    /// Claude Code command used for maps (`i`). Empty means `claude` on `PATH` (or in
    /// its usual install folders).
    pub claude_command: String,
    /// Model for maps, as `claude --model` takes it (`sonnet`, `haiku`, `opus`, or a
    /// full model name). Empty means `sonnet`.
    pub claude_model: String,
}

/// Longest editor command accepted.
pub const MAX_EDITOR_LEN: usize = 1024;

fn lenient_columns<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Column>, D::Error> {
    let names = Vec::<String>::deserialize(d)?;
    let mut settings = Settings {
        columns: names.iter().filter_map(|n| Column::from_name(n)).collect(),
        ..Settings::default()
    };
    settings.columns = settings.visible_columns().collect();
    Ok(settings.columns)
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            columns: vec![Column::Git, Column::Size, Column::Modified],
            show_hidden: true,
            editor: String::new(),
            gui_editor: String::new(),
            bookmarks: Vec::new(),
            claude_command: String::new(),
            claude_model: String::new(),
        }
    }
}

impl Settings {
    pub fn shows(&self, c: Column) -> bool {
        self.columns.contains(&c)
    }

    /// Turn a column on or off, keeping the canonical order.
    pub fn toggle_column(&mut self, c: Column) {
        if self.shows(c) {
            self.columns.retain(|&x| x != c);
        } else {
            self.columns.push(c);
        }
        self.columns.sort_by_key(|c| Column::ALL.iter().position(|x| x == c));
    }

    /// Visible columns in display order.
    pub fn visible_columns(&self) -> impl Iterator<Item = Column> + '_ {
        Column::ALL.into_iter().filter(|c| self.shows(*c))
    }

    /// The visible columns that fit in `width` characters next to a name column of at
    /// least `min_name` (each column takes its width plus one space; `fixed` is any other
    /// space the row needs). The least important columns are dropped first.
    pub fn fit_columns(&self, width: u16, min_name: u16, fixed: u16) -> Vec<Column> {
        let mut cols: Vec<Column> = self.visible_columns().collect();
        let needed = |cols: &[Column]| cols.iter().map(|c| c.width() + 1).sum::<u16>() + min_name + fixed;
        while !cols.is_empty() && width < needed(&cols) {
            let worst = cols
                .iter()
                .enumerate()
                .max_by_key(|(_, c)| c.drop_priority())
                .map(|(i, _)| i);
            if let Some(i) = worst {
                cols.remove(i);
            }
        }
        cols
    }

    /// Canonical form for saving: columns deduplicated in display order, editor commands
    /// trimmed and bounded.
    pub fn normalized(mut self) -> Self {
        self.columns = Column::ALL.into_iter().filter(|c| self.columns.contains(c)).collect();
        for e in [
            &mut self.editor,
            &mut self.gui_editor,
            &mut self.claude_command,
            &mut self.claude_model,
        ] {
            *e = e
                .trim()
                .chars()
                .filter(|c| !c.is_control())
                .take(MAX_EDITOR_LEN)
                .collect();
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggling_keeps_order() {
        let mut s = Settings::default();
        s.toggle_column(Column::Permissions);
        s.toggle_column(Column::Ext);
        assert_eq!(
            s.visible_columns().collect::<Vec<_>>(),
            [
                Column::Git,
                Column::Ext,
                Column::Size,
                Column::Modified,
                Column::Permissions
            ]
        );
        s.toggle_column(Column::Size);
        assert!(!s.shows(Column::Size));
    }

    #[test]
    fn narrow_panels_drop_the_least_important_columns() {
        let s = Settings {
            columns: Column::ALL.to_vec(),
            ..Settings::default()
        };
        let all = s.fit_columns(500, 12, 2);
        assert_eq!(all.len(), Column::ALL.len());
        let some = s.fit_columns(60, 12, 2);
        assert!(
            some.contains(&Column::Size) && some.contains(&Column::Permissions),
            "{some:?}"
        );
        assert!(!some.contains(&Column::Created));
        assert!(s.fit_columns(10, 12, 2).is_empty());
    }

    #[test]
    fn unknown_and_duplicate_columns_are_tolerated() {
        let s: Settings = serde_json::from_str(r#"{"columns":["owner","bogus","size","size"]}"#).unwrap();
        assert_eq!(s.columns, [Column::Size, Column::Owner]);
        let n = Settings {
            columns: vec![Column::Owner, Column::Size, Column::Owner],
            editor: "  vim\n".into(),
            ..Settings::default()
        }
        .normalized();
        assert_eq!(n.columns, [Column::Size, Column::Owner]);
        assert_eq!(n.editor, "vim");
    }

    #[test]
    fn missing_fields_take_defaults() {
        let s: Settings = serde_json::from_str(r#"{"editor":"vim"}"#).unwrap();
        assert_eq!(s.editor, "vim");
        assert!(s.show_hidden);
        assert_eq!(s.columns, Settings::default().columns);
    }

    #[test]
    fn titles_fit_their_columns() {
        for c in Column::ALL {
            assert!(c.title().len() <= c.width() as usize, "{c:?}");
        }
    }

    #[test]
    fn sortable_columns() {
        assert_eq!(Column::Size.sort_key(), Some(SortKey::Size));
        assert_eq!(Column::Modified.sort_key(), Some(SortKey::Modified));
        assert_eq!(Column::Owner.sort_key(), None);
    }

    #[test]
    fn cell_text() {
        let e = Entry {
            mode: Some(0o755),
            owner: Some("me".into()),
            modified: Some(0),
            ..Entry::file("run.sh", 2048)
        };
        let utc = |_| 0;
        assert_eq!(Column::Permissions.text(&e, utc), "-rwxr-xr-x");
        assert_eq!(Column::Size.text(&e, utc), "2.0K");
        assert_eq!(Column::Ext.text(&e, utc), "sh");
        assert_eq!(Column::Owner.text(&e, utc), "me");
        assert_eq!(Column::Created.text(&e, utc), "");
        assert_eq!(Column::Modified.text(&e, utc), "1970-01-01 00:00");
    }
}
