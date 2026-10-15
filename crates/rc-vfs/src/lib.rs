//! Native back end behind one facade. Both the terminal UI and the web server call this.
//! Everything is blocking; the web server wraps calls in `spawn_blocking`.
//! New sources (archives, SFTP, GitHub) plug in by matching on new `Location` variants.

pub mod changes;
pub mod find;
pub mod git;
pub mod local;
pub mod settings;
pub mod watch;

use anyhow::{Result, bail};
use local::Copied;
use rc_core::api::{Conflicts, DiffResponse, FileContent, FileOp, OpResult, Progress};
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

    /// List a directory. The returned listing carries the normalized location, so
    /// `/a/b/` and `/a/b` are recognized as the same place by the panel.
    pub fn list(&self, loc: &Location) -> Result<Listing> {
        match self.normalize(loc.clone(), None) {
            Location::Local { path } => local::list(&path),
        }
    }

    /// `list` plus each entry's git status, for when the Git column is shown (it costs
    /// two git processes per listing).
    pub fn list_with_git(&self, loc: &Location) -> Result<Listing> {
        let mut listing = self.list(loc)?;
        let Location::Local { path } = &listing.location;
        let status = git::statuses(Path::new(path));
        for e in &mut listing.entries {
            e.git = status.get(&e.name).copied();
        }
        Ok(listing)
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

    /// Diff two files. Only the first [`VIEW_LIMIT`] bytes are compared; if either file is
    /// longer the note says so and the files are never reported as identical.
    pub fn diff(&self, left: Option<&Location>, right: Option<&Location>) -> Result<DiffResponse> {
        let load = |loc: Option<&Location>| -> Result<Vec<u8>> {
            match loc {
                Some(l) => Ok(self.read(l, VIEW_LIMIT + 1)?.0),
                None => Ok(Vec::new()),
            }
        };
        let title = |l: Option<&Location>| l.map_or("(does not exist)".to_string(), |l| l.to_string());
        Ok(diff_contents(title(left), load(left)?, title(right), load(right)?))
    }

    /// Diff a file against its committed version (git HEAD).
    pub fn diff_head(&self, loc: &Location) -> Result<DiffResponse> {
        let Location::Local { path } = loc;
        if Path::new(path)
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
        {
            bail!("{path} is a symlink; git stores the link, not the file it points to");
        }
        let Some(head) = git::head_content(Path::new(path)) else {
            bail!("{path} is not tracked by git (or has no commits)");
        };
        let (current, _) = self.read(loc, VIEW_LIMIT + 1)?;
        let name = loc.file_name().unwrap_or(path);
        Ok(diff_contents(format!("HEAD:{name}"), head, path.clone(), current))
    }

    /// Where each source of a copy/move ends up, or an error message.
    fn destinations(sources: &[Location], dest: &Location) -> Result<Vec<(PathBuf, PathBuf)>, String> {
        let Location::Local { path: dest_path } = dest;
        let into_dir = Path::new(dest_path).is_dir();
        if !into_dir && sources.len() > 1 {
            return Err(format!("{dest_path} is not a directory"));
        }
        sources
            .iter()
            .map(|src| {
                let Location::Local { path } = src;
                let name = src.file_name().ok_or_else(|| format!("cannot copy or move {src}"))?;
                let target = if into_dir {
                    Path::new(dest_path).join(name)
                } else {
                    PathBuf::from(dest_path)
                };
                Ok((PathBuf::from(path), target))
            })
            .collect()
    }

    /// What a copy/move would find already at its destinations.
    pub fn conflicts(&self, op: &FileOp) -> Conflicts {
        let (FileOp::Copy { sources, dest, .. } | FileOp::Move { sources, dest, .. }) = op else {
            return Conflicts::default();
        };
        let pairs = Self::destinations(sources, dest).unwrap_or_default();
        let existing = pairs
            .iter()
            .filter(|(src, target)| target.symlink_metadata().is_ok() && !local::same_file(src, target))
            .map(|(_, target)| target.to_string_lossy().into_owned())
            .collect();
        Conflicts {
            existing,
            items: sources.len(),
        }
    }

    /// Run a file operation. `progress` hears about every file and every copied chunk;
    /// answering `Break` cancels (a partly copied file is removed).
    pub fn op(&self, op: &FileOp, progress: &mut local::ProgressFn) -> OpResult {
        let mut res = OpResult::default();
        let record = |res: &mut OpResult, what: &str, r: Result<Copied>| match r {
            Ok(Copied::Done) => res.done += 1,
            Ok(Copied::Skipped) => res.skipped.push(what.to_string()),
            Err(e) if e.is::<local::Cancelled>() => res.cancelled = true,
            Err(e) => res.errors.push(format!("{e:#}")),
        };
        // Copies and moves report every skipped file, not just top-level items.
        let record_outcome = |res: &mut OpResult, r: Result<local::Outcome>| match r {
            Ok(o) => {
                if o.result() == Copied::Done {
                    res.done += 1;
                }
                res.skipped
                    .extend(o.skipped.iter().map(|p| p.to_string_lossy().into_owned()));
            }
            Err(e) if e.is::<local::Cancelled>() => res.cancelled = true,
            Err(e) => res.errors.push(format!("{e:#}")),
        };
        match op {
            FileOp::Copy {
                sources,
                dest,
                on_conflict,
            }
            | FileOp::Move {
                sources,
                dest,
                on_conflict,
            } => {
                let is_move = matches!(op, FileOp::Move { .. });
                let pairs = match Self::destinations(sources, dest) {
                    Ok(p) => p,
                    Err(e) => {
                        res.errors.push(e);
                        return res;
                    }
                };
                let total = pairs.iter().map(|(src, _)| local::total_size(src)).sum();
                let mut done = 0;
                for (src, target) in pairs {
                    let r = if is_move {
                        local::move_item(&src, &target, *on_conflict, &mut done, total, progress)
                    } else {
                        local::copy(&src, &target, *on_conflict, &mut done, total, progress)
                    };
                    record_outcome(&mut res, r);
                    if res.cancelled {
                        break;
                    }
                }
            }
            FileOp::Delete { targets, permanent } => {
                for t in targets {
                    let Location::Local { path } = t;
                    if progress(&Progress {
                        path: path.clone(),
                        done: 0,
                        total: 0,
                    })
                    .is_break()
                    {
                        res.cancelled = true;
                        break;
                    }
                    let r = if *permanent {
                        local::remove(Path::new(path))
                    } else {
                        trash_context()
                            .delete(path)
                            .map_err(|e| anyhow::anyhow!("cannot move {path} to trash: {e}"))
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
                // Same file under another name = case-only rename on case-insensitive APFS.
                let r = if Path::new(b).symlink_metadata().is_ok() && !local::same_file(Path::new(a), Path::new(b)) {
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

    /// Open a file in `editor` (a command line such as `code` or `subl -n`) without
    /// waiting for it; an empty editor falls back to the system default application.
    /// Meant for GUI editors: there is no terminal to run `vim` in. The editor runs fully
    /// detached: no inherited stdio, its own process group, and reaped when it exits.
    pub fn edit_external(&self, loc: &Location, editor: &str) -> Result<()> {
        let Location::Local { path } = loc;
        if Path::new(path).is_dir() {
            bail!("{path} is a folder");
        }
        let mut parts = editor.split_whitespace();
        let Some(program) = parts.next() else {
            return self.open_external(loc);
        };
        check_program(program)?;
        let mut cmd = std::process::Command::new(program);
        cmd.args(parts)
            .arg(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("cannot run `{editor}`: {e}"))?;
        std::thread::spawn(move || child.wait());
        Ok(())
    }
}

/// Fail unless `program` names an executable file, directly or on `PATH`.
pub fn check_program(program: &str) -> Result<()> {
    let is_exec = |p: &Path| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            p.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        }
        #[cfg(not(unix))]
        {
            p.is_file()
        }
    };
    let found = if program.contains('/') {
        is_exec(Path::new(program))
    } else {
        std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|d| is_exec(&d.join(program))))
    };
    if found {
        Ok(())
    } else {
        bail!("editor `{program}` not found")
    }
}

/// Diff two byte buffers (each read with up to `VIEW_LIMIT + 1` bytes).
fn diff_contents(left_title: String, mut a: Vec<u8>, right_title: String, mut b: Vec<u8>) -> DiffResponse {
    let truncated = a.len() as u64 > VIEW_LIMIT || b.len() as u64 > VIEW_LIMIT;
    a.truncate(VIEW_LIMIT as usize);
    b.truncate(VIEW_LIMIT as usize);
    let limit_note = format!(
        "Files are larger than {} MiB; only the beginning was compared",
        VIEW_LIMIT >> 20
    );
    let (result, note) = if diff::looks_binary(&a) || diff::looks_binary(&b) {
        let note = match (a == b, truncated) {
            (true, false) => "Binary files are identical".to_string(),
            (true, true) => format!("Binary files: {limit_note}, no difference found there"),
            (false, _) => "Binary files differ".to_string(),
        };
        (None, Some(note))
    } else {
        let r = rc_core::diff_text(&String::from_utf8_lossy(&a), &String::from_utf8_lossy(&b));
        let note = if truncated {
            Some(limit_note)
        } else if r.identical() && a != b {
            Some("Files differ only in line endings or the final newline".to_string())
        } else {
            r.identical().then(|| "Files are identical".to_string())
        };
        (Some(r), note)
    };
    DiffResponse {
        left_title,
        right_title,
        result,
        note,
    }
}

fn trash_context() -> trash::TrashContext {
    #[allow(unused_mut)]
    let mut ctx = trash::TrashContext::default();
    // The default on macOS drives Finder via AppleScript: slow, asks for automation
    // permission, and fails over ssh. NSFileManager trashes directly.
    #[cfg(target_os = "macos")]
    {
        use trash::macos::{DeleteMethod, TrashContextExtMacos};
        ctx.set_delete_method(DeleteMethod::NsFileManager);
    }
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_core::api::OnConflict;
    use std::ops::ControlFlow;

    #[cfg(unix)]
    #[test]
    fn edit_runs_the_editor_detached_and_rejects_folders() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("ed");
        let marker = tmp.path().join("argv");
        std::fs::write(&script, format!("#!/bin/sh\necho \"$@\" > {}\n", marker.display())).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let file = tmp.path().join("a b.txt");
        std::fs::write(&file, "x").unwrap();
        let vfs = Vfs::new();
        let editor = format!("{} --flag", script.display());
        vfs.edit_external(&Location::local(file.to_string_lossy()), &editor)
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap().trim(),
            format!("--flag {}", file.display())
        );
        assert!(
            vfs.edit_external(&Location::local(tmp.path().to_string_lossy()), &editor)
                .is_err()
        );
        assert!(
            vfs.edit_external(&Location::local(file.to_string_lossy()), "no-such-editor-xyz")
                .is_err()
        );
    }

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
        assert_eq!(vfs.normalize(Location::local("/../.."), None), Location::local("/"));
        let sub = vfs.normalize(Location::local("~/x"), None);
        assert_eq!(
            sub,
            Location::local(Path::new(&Vfs::home()).join("x").to_string_lossy())
        );
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
            &mut |_| ControlFlow::Continue(()),
        );
        assert_eq!(r.done, 1);
        let r = vfs.op(
            &FileOp::Copy {
                sources: vec![Location::local(p("a.txt")), Location::local(p("b.txt"))],
                dest: Location::local(p("d")),
                on_conflict: OnConflict::Skip,
            },
            &mut |_| ControlFlow::Continue(()),
        );
        assert_eq!((r.done, r.errors.len()), (2, 0));
        // Single source + non-existing destination = rename.
        let r = vfs.op(
            &FileOp::Move {
                sources: vec![Location::local(p("d/a.txt"))],
                dest: Location::local(p("d/c.txt")),
                on_conflict: OnConflict::Skip,
            },
            &mut |_| ControlFlow::Continue(()),
        );
        assert_eq!(r.done, 1);
        assert!(Path::new(&p("d/c.txt")).exists());
        let r = vfs.op(
            &FileOp::Delete {
                targets: vec![Location::local(p("d"))],
                permanent: true,
            },
            &mut |_| ControlFlow::Continue(()),
        );
        assert_eq!(r.done, 1);
    }
}
