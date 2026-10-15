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
    /// Where "Claude's changes" lives, if available: the folder to show and the folder
    /// with each file's original content (same entry names).
    #[serde(default)]
    pub changes: Option<ChangesDirs>,
}

/// `POST /api/changes/forget` → [`OpResult`]: drop entries (by name) from "Claude's
/// changes". Only the log's own link and saved original are removed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForgetRequest {
    pub names: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChangesDirs {
    pub links: Location,
    pub originals: Location,
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileContent {
    pub location: Location,
    /// `None` for binary files.
    pub text: Option<String>,
    pub size: u64,
    /// The file was larger than the viewer limit and was cut.
    pub truncated: bool,
}

/// `POST /api/preview` with a [`ReadRequest`] → [`Preview`]: what Quick View (Ctrl+Q)
/// shows for the entry under the cursor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Preview {
    /// The beginning of a file.
    File(FileContent),
    Folder(FolderSummary),
}

/// What a folder holds directly (subfolders are not entered).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FolderSummary {
    pub location: Location,
    pub folders: usize,
    pub files: usize,
    /// How many of the above are hidden (dot-files).
    pub hidden: usize,
    /// Total size of the files.
    pub size: u64,
    /// Counting stopped early (a very large folder): the numbers are a lower bound.
    pub partial: bool,
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
        #[serde(default)]
        on_conflict: OnConflict,
    },
    Move {
        sources: Vec<Location>,
        dest: Location,
        #[serde(default)]
        on_conflict: OnConflict,
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
    /// The same copy or move with a different conflict policy; other operations unchanged.
    pub fn with_on_conflict(self, policy: OnConflict) -> FileOp {
        match self {
            FileOp::Copy { sources, dest, .. } => FileOp::Copy {
                sources,
                dest,
                on_conflict: policy,
            },
            FileOp::Move { sources, dest, .. } => FileOp::Move {
                sources,
                dest,
                on_conflict: policy,
            },
            other => other,
        }
    }

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

/// What to do when a copy or move finds something already at the destination.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnConflict {
    /// Leave existing files alone.
    #[default]
    Skip,
    Overwrite,
    /// Replace only files the source has a newer version of.
    IfNewer,
}

/// Progress of a running operation, in bytes of file content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    /// The file being processed.
    pub path: String,
    pub done: u64,
    pub total: u64,
}

impl Progress {
    pub fn percent(&self) -> u8 {
        (self.done.min(self.total) * 100).checked_div(self.total).unwrap_or(0) as u8
    }
}

/// Names a running operation. `POST /api/op?job=ID` runs under `ID` (letters, digits and
/// `-`, at most 64); `POST /api/job` with this body reports on it ([`JobStatus`]) and
/// `POST /api/cancel` stops it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobRequest {
    pub job: String,
}

/// `POST /api/job` / `POST /api/cancel` → where an operation is.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct JobStatus {
    /// False once the operation has finished (or if it never started).
    pub running: bool,
    /// The latest progress report, if any arrived yet.
    pub progress: Option<Progress>,
}

/// `POST /api/find[?job=ID]` → [`FindResult`]: search a folder tree, like Total
/// Commander's Find Files. While it runs, the job's [`Progress`] has the folder being
/// searched as `path`, the number of hits so far as `done`, and `total` 0.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FindRequest {
    pub root: Location,
    /// Name masks (`*.rs;*.toml`); a mask without `*` or `?` matches names containing
    /// it. Empty matches every name.
    #[serde(default)]
    pub name: String,
    /// Only files containing this text (case-sensitive only if it has capitals).
    /// Empty: no content search, and folders match too.
    #[serde(default)]
    pub contains: String,
    /// Look into hidden files and folders (the inside of `.git` is always skipped).
    #[serde(default)]
    pub hidden: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FindHit {
    pub path: String,
    pub is_dir: bool,
    /// Content search: the first matching line (1-based) and its text.
    #[serde(default)]
    pub line: Option<usize>,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FindResult {
    pub hits: Vec<FindHit>,
    /// Stopped at the hit limit; there may be more.
    pub truncated: bool,
    pub cancelled: bool,
    /// Folders and files that could not be read.
    pub unreadable: usize,
}

/// `POST /api/conflicts` with a [`FileOp`] body → [`Conflicts`]: what a copy or move
/// would find already in place, so the UI can ask before running it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Conflicts {
    /// Destination paths that already exist.
    pub existing: Vec<String>,
    /// Number of top-level items the operation covers.
    pub items: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OpResult {
    /// Number of top-level items processed successfully.
    pub done: usize,
    /// Destination paths that already existed and were left alone.
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
    /// The user stopped the operation part-way.
    #[serde(default)]
    pub cancelled: bool,
}

impl OpResult {
    pub fn summary(&self, verb: &str) -> String {
        let mut s = format!("{verb}: {} done", self.done);
        if !self.skipped.is_empty() {
            s += &format!(", {} skipped (already exist)", self.skipped.len());
        }
        if self.cancelled {
            s += ", then cancelled";
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
        #[serde(default)]
        line: Option<usize>,
    },
    Select {
        side: Side,
        mask: String,
    },
    Refresh,
    Message {
        text: String,
    },
    /// The contents of a directory a browser listed changed on disk. `location` is the
    /// location as the listing returned it, i.e. as the panel shows it.
    Changed {
        location: Location,
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
            on_conflict: OnConflict::Skip,
        };
        let json = serde_json::to_value(&op).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "op": "copy",
                "sources": [{ "kind": "local", "path": "/a" }],
                "dest": { "kind": "local", "path": "/b" },
                "on_conflict": "skip"
            })
        );
        let back: FileOp = serde_json::from_value(json).unwrap();
        assert!(matches!(back, FileOp::Copy { .. }));
        let changed = BrowserEvent::Changed {
            location: Location::local("/a"),
        };
        assert_eq!(
            serde_json::to_value(&changed).unwrap(),
            serde_json::json!({ "event": "changed", "location": { "kind": "local", "path": "/a" } })
        );
    }

    #[test]
    fn conflict_policy_applies_to_copy_and_move_only() {
        let (src, dest) = (vec![Location::local("/a")], Location::local("/b"));
        let mv = FileOp::Move {
            sources: src.clone(),
            dest: dest.clone(),
            on_conflict: OnConflict::Skip,
        };
        assert!(matches!(
            mv.with_on_conflict(OnConflict::IfNewer),
            FileOp::Move {
                on_conflict: OnConflict::IfNewer,
                ..
            }
        ));
        let mkdir = FileOp::Mkdir { location: dest };
        assert!(matches!(
            mkdir.with_on_conflict(OnConflict::Overwrite),
            FileOp::Mkdir { .. }
        ));
    }
}
