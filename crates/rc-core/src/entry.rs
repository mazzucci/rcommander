use crate::Location;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    #[serde(default)]
    pub is_link: bool,
    #[serde(default)]
    pub hidden: bool,
    pub size: Option<u64>,
    /// Unix seconds.
    pub modified: Option<i64>,
}

impl Entry {
    pub fn file(name: impl Into<String>, size: u64) -> Self {
        Entry {
            name: name.into(),
            is_dir: false,
            is_link: false,
            hidden: false,
            size: Some(size),
            modified: None,
        }
    }

    pub fn dir(name: impl Into<String>) -> Self {
        Entry {
            is_dir: true,
            size: None,
            ..Entry::file(name, 0)
        }
    }

    pub fn extension(&self) -> &str {
        if self.is_dir {
            return "";
        }
        match self.name.rfind('.') {
            Some(0) | None => "",
            Some(i) => &self.name[i + 1..],
        }
    }
}

/// Result of listing a location.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Listing {
    pub location: Location,
    pub entries: Vec<Entry>,
    /// Whether files can be copied/moved/created here.
    #[serde(default)]
    pub writable: bool,
}
