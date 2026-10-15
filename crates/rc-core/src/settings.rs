//! User preferences shared by both front ends. The native side persists them as TOML;
//! the browser gets and saves them through the API.

use crate::Entry;
use crate::format::{format_time, human_size, permissions};
use serde::{Deserialize, Serialize};

/// Optional columns. The name column is always shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Column {
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
    pub const ALL: [Column; 7] = [
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
            Column::Ext => 5,
            Column::Size => 7,
            Column::Modified | Column::Created => 16,
            Column::Permissions => 10,
            Column::Owner | Column::Group => 9,
        }
    }

    pub fn right_aligned(self) -> bool {
        matches!(self, Column::Size)
    }

    /// Cell text for an entry. `utc_offset` converts timestamps to local time.
    pub fn text(self, e: &Entry, utc_offset: impl Fn(i64) -> i64) -> String {
        let time = |t: Option<i64>| t.map(|t| format_time(t, utc_offset(t))).unwrap_or_default();
        match self {
            Column::Ext => e.extension().to_string(),
            Column::Size if e.is_dir => "<DIR>".to_string(),
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
    /// Visible optional columns (shown in [`Column::ALL`] order).
    pub columns: Vec<Column>,
    /// Show dot-files when a panel opens. `.` toggles per session.
    pub show_hidden: bool,
    /// Command used by F4 (Edit), e.g. `vim`, `nano`, `code --wait`. Empty means
    /// `$VISUAL`/`$EDITOR` in the terminal and the system default app in the browser.
    pub editor: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            columns: vec![Column::Size, Column::Modified],
            show_hidden: true,
            editor: String::new(),
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
            [Column::Ext, Column::Size, Column::Modified, Column::Permissions]
        );
        s.toggle_column(Column::Size);
        assert!(!s.shows(Column::Size));
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
