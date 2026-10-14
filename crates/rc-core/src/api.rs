//! Request/response types of the HTTP API between the web front end and `rcmd web`.
//! All endpoints are `POST /api/<name>` with JSON bodies; errors are `{"error": "..."}`.

use crate::{DiffResult, Location};
use serde::{Deserialize, Serialize};

/// `POST /api/init` → initial panel locations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InitResponse {
    pub left: Location,
    pub right: Location,
    pub home: String,
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

/// `POST /api/open` → `{}`. Opens a local file with the system default application.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpenRequest {
    pub location: Location,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
}
