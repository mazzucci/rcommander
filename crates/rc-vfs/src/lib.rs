//! Native back end behind one facade. Both the terminal UI and the web server call this.
//! Everything is blocking; the web server wraps calls in `spawn_blocking`.
//! New sources (archives, SFTP, GitHub) plug in by matching on new `Location` variants.

pub mod local;

use anyhow::Result;
use local::Copied;
use rc_core::api::{DiffResponse, FileContent, FileOp, OpResult};
use rc_core::{Listing, Location, diff};
use std::path::{Path, PathBuf};

/// Files larger than this are cut in the viewer and diff.
pub const VIEW_LIMIT: u64 = 8 << 20;
#[derive(Default)]
pub struct Vfs {}

impl Vfs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn home() -> String {
        dirs::home_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".into())
    }

    /// Startup locations: current directory on the left, home on the right.
    pub fn default_locations() -> (Location, Location) {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| Self::home());
        (Location::local(cwd), Location::local(Self::home()))
    }

    /// Expand `~`, make relative paths absolute and drop trailing slashes. Non-local
    /// locations pass through unchanged.
    pub fn normalize(&self, loc: Location, base: Option<&Location>) -> Location {
        let Location::Local { path } = &loc;
        let mut p = if path == "~" {
            PathBuf::from(Self::home())
        } else if let Some(rest) = path.strip_prefix("~/") {
            Path::new(&Self::home()).join(rest)
        } else if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            match base {
                Some(Location::Local { path: b }) => Path::new(b).join(path),
                None => std::env::current_dir().unwrap_or_default().join(path),
            }
        };
        // Resolve `.` and `..` lexically so the panel shows a clean path.
        let mut clean = PathBuf::new();
        for c in p.components() {
            match c {
                std::path::Component::ParentDir => {
                    clean.pop();
                }
                std::path::Component::CurDir => {}
                other => clean.push(other),
            }
        }
        p = clean;
        Location::local(p.to_string_lossy().into_owned())
    }

    pub fn list(&self, loc: &Location) -> Result<Listing> {
        match loc {
            Location::Local { path } => local::list(path),
        }
    }

    /// Read up to `limit` bytes. Returns the bytes and the full size (if known).
    pub fn read(&self, loc: &Location, limit: u64) -> Result<(Vec<u8>, Option<u64>)> {
        match loc {
            Location::Local { path } => local::read(path, limit).map(|(b, s)| (b, Some(s))),
        }
    }

    pub fn read_content(&self, loc: &Location) -> Result<FileContent> {
        let (bytes, size) = self.read(loc, VIEW_LIMIT + 1)?;
        let truncated = bytes.len() as u64 > VIEW_LIMIT;
        let bytes = &bytes[..bytes.len().min(VIEW_LIMIT as usize)];
        Ok(FileContent {
            location: loc.clone(),
            text: (!diff::looks_binary(bytes)).then(|| String::from_utf8_lossy(bytes).into_owned()),
            size: size.unwrap_or(bytes.len() as u64),
            truncated,
        })
    }

    pub fn diff(&self, left: Option<&Location>, right: Option<&Location>) -> Result<DiffResponse> {
        let load = |loc: Option<&Location>| -> Result<Vec<u8>> {
            match loc {
                Some(l) => Ok(self.read(l, VIEW_LIMIT)?.0),
                None => Ok(Vec::new()),
            }
        };
        let (a, b) = (load(left)?, load(right)?);
        let title = |l: Option<&Location>| l.map_or("(does not exist)".to_string(), |l| l.to_string());
        let (result, note) = if diff::looks_binary(&a) || diff::looks_binary(&b) {
            let note = if a == b {
                "Binary files are identical"
            } else {
                "Binary files differ"
            };
            (None, Some(note.to_string()))
        } else {
            let r = rc_core::diff_text(&String::from_utf8_lossy(&a), &String::from_utf8_lossy(&b));
            let note = r.identical().then(|| "Files are identical".to_string());
            (Some(r), note)
        };
        Ok(DiffResponse {
            left_title: title(left),
            right_title: title(right),
            result,
            note,
        })
    }

    /// Run a file operation. `progress` is called with each path as it is processed.
    pub fn op(&self, op: &FileOp, progress: &mut dyn FnMut(&str)) -> OpResult {
        let mut res = OpResult::default();
        let record = |res: &mut OpResult, what: &str, r: Result<Copied>| match r {
            Ok(Copied::Done) => res.done += 1,
            Ok(Copied::Skipped) => res.skipped.push(what.to_string()),
            Err(e) => res.errors.push(format!("{e:#}")),
        };
        match op {
            FileOp::Copy {
                sources,
                dest,
                overwrite,
            }
            | FileOp::Move {
                sources,
                dest,
                overwrite,
            } => {
                let is_move = matches!(op, FileOp::Move { .. });
                let Location::Local { path: dest_path } = dest;
                let into_dir = Path::new(dest_path).is_dir();
                if !into_dir && sources.len() > 1 {
                    res.errors.push(format!("{dest_path} is not a directory"));
                    return res;
                }
                for src in sources {
                    let Some(name) = src.file_name() else { continue };
                    let dest = if into_dir {
                        Path::new(dest_path).join(name)
                    } else {
                        PathBuf::from(dest_path)
                    };
                    let Location::Local { path } = src;
                    let r = if is_move {
                        local::move_item(Path::new(path), &dest, *overwrite, progress)
                    } else {
                        local::copy(Path::new(path), &dest, *overwrite, progress)
                    };
                    record(&mut res, &dest.to_string_lossy(), r);
                }
            }
            FileOp::Delete { targets, permanent } => {
                for t in targets {
                    let Location::Local { path } = t;
                    progress(path);
                    let r = if *permanent {
                        local::remove(Path::new(path))
                    } else {
                        trash::delete(path).map_err(|e| anyhow::anyhow!("cannot move {path} to trash: {e}"))
                    };
                    record(&mut res, path, r.map(|_| Copied::Done));
                }
            }
            FileOp::Mkdir { location } => {
                let Location::Local { path } = location;
                let r = std::fs::create_dir_all(path).map_err(Into::into);
                record(&mut res, &location.to_string(), r.map(|_| Copied::Done));
            }
            FileOp::Rename { from, to } => {
                let (Location::Local { path: a }, Location::Local { path: b }) = (from, to);
                let r = if Path::new(b).exists() {
                    Err(anyhow::anyhow!("{b} already exists"))
                } else {
                    std::fs::rename(a, b).map_err(Into::into)
                };
                record(&mut res, &to.to_string(), r.map(|_| Copied::Done));
            }
        }
        res
    }

    /// Open with the system default application (Finder association on macOS).
    pub fn open_external(&self, loc: &Location) -> Result<()> {
        let Location::Local { path } = loc;
        Ok(open::that_detached(path)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_paths() {
        let vfs = Vfs::new();
        let base = Location::local("/a/b");
        assert_eq!(
            vfs.normalize(Location::local("../c"), Some(&base)),
            Location::local("/a/c")
        );
        assert_eq!(vfs.normalize(Location::local("/x/./y/"), None), Location::local("/x/y"));
        let home = vfs.normalize(Location::local("~"), None);
        assert_eq!(home, Location::local(Vfs::home()));
    }

    #[test]
    fn local_diff_and_ops() {
        let tmp = tempfile::tempdir().unwrap();
        let p = |n: &str| tmp.path().join(n).to_string_lossy().into_owned();
        std::fs::write(p("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(p("b.txt"), "one\nthree\n").unwrap();
        let vfs = Vfs::new();
        let d = vfs
            .diff(Some(&Location::local(p("a.txt"))), Some(&Location::local(p("b.txt"))))
            .unwrap();
        assert_eq!(d.result.unwrap().hunks.len(), 1);

        let r = vfs.op(
            &FileOp::Mkdir {
                location: Location::local(p("d")),
            },
            &mut |_| {},
        );
        assert_eq!(r.done, 1);
        let r = vfs.op(
            &FileOp::Copy {
                sources: vec![Location::local(p("a.txt")), Location::local(p("b.txt"))],
                dest: Location::local(p("d")),
                overwrite: false,
            },
            &mut |_| {},
        );
        assert_eq!((r.done, r.errors.len()), (2, 0));
        // Single source + non-existing destination = rename.
        let r = vfs.op(
            &FileOp::Move {
                sources: vec![Location::local(p("d/a.txt"))],
                dest: Location::local(p("d/c.txt")),
                overwrite: false,
            },
            &mut |_| {},
        );
        assert_eq!(r.done, 1);
        assert!(Path::new(&p("d/c.txt")).exists());
        let r = vfs.op(
            &FileOp::Delete {
                targets: vec![Location::local(p("d"))],
                permanent: true,
            },
            &mut |_| {},
        );
        assert_eq!(r.done, 1);
    }
}
