//! Locations are the "addresses" panels can point at. Only the local filesystem exists
//! today; remote sources (GitHub, SFTP, archives) become new variants later.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Location {
    Local { path: String },
}

impl Location {
    pub fn local(path: impl Into<String>) -> Self {
        Location::Local { path: path.into() }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Location::Local { .. })
    }

    /// Parse what the user typed in the go-to box. `~` expansion and relative paths are
    /// resolved on the native side, which knows the home and current directory.
    pub fn parse(input: &str) -> Option<Location> {
        let s = input.trim();
        (!s.is_empty()).then(|| Location::local(s))
    }

    /// Resolve user input relative to `base` (the active panel). Absolute and `~` paths
    /// are kept as typed; the native side expands `~` and cleans up `..`.
    pub fn resolve_input(input: &str, base: &Location) -> Option<Location> {
        let s = input.trim();
        if s.is_empty() {
            None
        } else if s.starts_with('/') || s == "~" || s.starts_with("~/") {
            Some(Location::local(s))
        } else {
            Some(base.join(s))
        }
    }

    /// Parent location, or `None` at the root.
    pub fn parent(&self) -> Option<Location> {
        let Location::Local { path } = self;
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() {
            return None;
        }
        let idx = trimmed.rfind('/')?;
        Some(Location::local(if idx == 0 { "/" } else { &trimmed[..idx] }))
    }

    /// Child location for an entry name.
    pub fn join(&self, name: &str) -> Location {
        let Location::Local { path } = self;
        let sep = if path.ends_with('/') { "" } else { "/" };
        Location::local(format!("{path}{sep}{name}"))
    }

    /// Last path component, used to put the cursor back on the directory we came from.
    pub fn file_name(&self) -> Option<&str> {
        let Location::Local { path } = self;
        path.trim_end_matches('/').rsplit('/').next().filter(|s| !s.is_empty())
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Location::Local { path } => write!(f, "{path}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_and_join() {
        let l = Location::local("/a/b");
        assert_eq!(l.parent().unwrap(), Location::local("/a"));
        assert_eq!(Location::local("/a").parent().unwrap(), Location::local("/"));
        assert_eq!(Location::local("/").parent(), None);
        assert_eq!(Location::local("/").join("x"), Location::local("/x"));
        assert_eq!(Location::local("/a/b/").file_name(), Some("b"));
        assert_eq!(Location::parse("  "), None);
        let base = Location::local("/a");
        assert_eq!(
            Location::resolve_input("b/c", &base).unwrap(),
            Location::local("/a/b/c")
        );
        assert_eq!(Location::resolve_input("~/x", &base).unwrap(), Location::local("~/x"));
    }
}
