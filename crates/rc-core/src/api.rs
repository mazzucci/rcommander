//! Request/response types of the HTTP API between the web front end and `rcmd web`.
//! All endpoints are `POST /api/<name>` with JSON bodies; errors are `{"error": "..."}`.

use crate::control::Side;
use crate::{DiffResult, Location, Settings};
use serde::{Deserialize, Serialize};

/// `POST /api/init` → initial panel locations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InitResponse {
    pub left: Location,
    pub right: Location,
    pub home: String,
    #[serde(default)]
    pub settings: Settings,
}

/// `POST /api/list` → [`crate::Listing`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ListRequest {
    pub location: Location,
}

/// `POST /api/read` → [`FileContent`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReadRequest {
    pub location: Location,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileContent {
    pub location: Location,
    /// `None` for binary files.
    pub text: Option<String>,
    pub size: u64,
    /// The file was larger than the viewer limit and was cut.
    pub truncated: bool,
}

/// `POST /api/diff` → [`DiffResponse`]. A missing side diffs against an empty file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiffRequest {
    pub left: Option<Location>,
    pub right: Option<Location>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiffResponse {
    pub left_title: String,
    pub right_title: String,
    /// `None` when at least one side is binary; see `note`.
    pub result: Option<DiffResult>,
    pub note: Option<String>,
}

/// `POST /api/diff_head` with a [`ReadRequest`] body → [`DiffResponse`] of the file
/// against its committed (git HEAD) version.
///
/// `POST /api/op` → [`OpResult`].
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum FileOp {
    /// Like `cp`: if `dest` is an existing directory the sources go inside it; otherwise
    /// (single source only) `dest` is the new full path, i.e. "copy as" / rename.
    Copy {
        sources: Vec<Location>,
        dest: Location,
        overwrite: bool,
    },
    Move {
        sources: Vec<Location>,
        dest: Location,
        overwrite: bool,
    },
    /// `permanent: false` moves to the system trash.
    Delete {
        targets: Vec<Location>,
        permanent: bool,
    },
    Mkdir {
        location: Location,
    },
    Rename {
        from: Location,
        to: Location,
    },
}

impl FileOp {
    pub fn verb(&self) -> &'static str {
        match self {
            FileOp::Copy { .. } => "Copy",
            FileOp::Move { .. } => "Move",
            FileOp::Delete { .. } => "Delete",
            FileOp::Mkdir { .. } => "Create directory",
            FileOp::Rename { .. } => "Rename",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OpResult {
    /// Number of top-level items processed successfully.
    pub done: usize,
    /// Destination paths that already existed and were left alone.
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
}

impl OpResult {
    pub fn summary(&self, verb: &str) -> String {
        let mut s = format!("{verb}: {} done", self.done);
        if !self.skipped.is_empty() {
            s += &format!(", {} skipped (already exist)", self.skipped.len());
        }
        if let Some(e) = self.errors.first() {
            s += &format!(", {} failed: {e}", self.errors.len());
        }
        s
    }
}

/// `POST /api/settings` with a [`Settings`] body saves them and returns the saved value.
///
/// `POST /api/open` → `{}`. Opens a local file with the system default application.
/// `POST /api/edit` → `{}`. Same body; opens it in the configured editor (or, without
/// one, the system default application).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpenRequest {
    pub location: Location,
}

/// `GET /api/events?token=…`: server-sent events, each one JSON [`BrowserEvent`]. They
/// carry remote-control commands (`rcmd ctl`) to the browser, paths already resolved.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum BrowserEvent {
    /// Show `location` in a panel, with the cursor on `focus` if given.
    Navigate {
        side: Side,
        location: Location,
        focus: Option<String>,
    },
    Diff {
        left: Location,
        right: Location,
    },
    View {
        location: Location,
    },
    Select {
        side: Side,
        mask: String,
    },
    Refresh,
    Message {
        text: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The JSON shapes are the contract with the web front end.
    #[test]
    fn wire_format() {
        let op = FileOp::Copy {
            sources: vec![Location::local("/a")],
            dest: Location::local("/b"),
            overwrite: false,
        };
        let json = serde_json::to_value(&op).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "op": "copy",
                "sources": [{ "kind": "local", "path": "/a" }],
                "dest": { "kind": "local", "path": "/b" },
                "overwrite": false
            })
        );
        let back: FileOp = serde_json::from_value(json).unwrap();
        assert!(matches!(back, FileOp::Copy { .. }));
    }
}
