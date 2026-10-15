//! Local filesystem listing and file operations.

use anyhow::{Context, Result, bail};
use rc_core::api::{FolderSummary, OnConflict, Progress};
use rc_core::{Entry, Listing, Location};
use std::fs;
use std::io::{Read, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub fn list(path: &str) -> Result<Listing> {
    let mut names = owners::Names::default();
    let mut entries = Vec::new();
    for item in fs::read_dir(path).with_context(|| format!("cannot read {path}"))? {
        let Ok(item) = item else { continue };
        let name = item.file_name().to_string_lossy().into_owned();
        let Ok(link_meta) = item.metadata() else { continue };
        let is_link = link_meta.file_type().is_symlink();
        // For symlinks, describe the target (falls back to the link itself if dangling).
        let meta = if is_link {
            fs::metadata(item.path()).unwrap_or_else(|_| link_meta.clone())
        } else {
            link_meta.clone()
        };
        let secs = |t: std::io::Result<std::time::SystemTime>| {
            t.ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
        };
        let mut entry = Entry {
            hidden: name.starts_with('.'),
            is_dir: meta.is_dir(),
            is_link,
            size: (!meta.is_dir()).then_some(meta.len()),
            modified: secs(meta.modified()),
            created: secs(meta.created()),
            link_target: is_link
                .then(|| fs::read_link(item.path()).ok())
                .flatten()
                .map(|t| t.to_string_lossy().into_owned()),
            name,
            ..Entry::default()
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // Permissions and ownership of the link itself, like `ls -l`.
            entry.mode = Some(link_meta.mode() & 0o7777);
            entry.owner = Some(names.user(link_meta.uid()));
            entry.group = Some(names.group(link_meta.gid()));
        }
        entries.push(entry);
    }
    Ok(Listing {
        location: Location::local(path),
        entries,
        writable: true,
    })
}

/// uid/gid → name lookups, cached per listing.
mod owners {
    use std::collections::HashMap;

    #[derive(Default)]
    pub struct Names {
        users: HashMap<u32, String>,
        groups: HashMap<u32, String>,
    }

    #[cfg(unix)]
    impl Names {
        pub fn user(&mut self, uid: u32) -> String {
            self.users
                .entry(uid)
                .or_insert_with(|| lookup_user(uid).unwrap_or_else(|| uid.to_string()))
                .clone()
        }

        pub fn group(&mut self, gid: u32) -> String {
            self.groups
                .entry(gid)
                .or_insert_with(|| lookup_group(gid).unwrap_or_else(|| gid.to_string()))
                .clone()
        }
    }

    /// Call a reentrant `get*_r` function, growing the buffer on ERANGE.
    #[cfg(unix)]
    fn with_buffer(mut call: impl FnMut(&mut [libc::c_char]) -> libc::c_int) -> Option<()> {
        let mut size = 1024;
        loop {
            let mut buf = vec![0 as libc::c_char; size];
            match call(&mut buf) {
                0 => return Some(()),
                libc::ERANGE if size < 1 << 20 => size *= 4,
                _ => return None,
            }
        }
    }

    #[cfg(unix)]
    fn lookup_user(uid: u32) -> Option<String> {
        let mut name = None;
        with_buffer(|buf| {
            // SAFETY: all pointers are valid for the duration of the call; the result
            // points into `buf` and is copied out before `buf` is dropped.
            unsafe {
                let mut pwd: libc::passwd = std::mem::zeroed();
                let mut result = std::ptr::null_mut();
                let rc = libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result);
                if rc == 0 && !result.is_null() {
                    name = Some(std::ffi::CStr::from_ptr(pwd.pw_name).to_string_lossy().into_owned());
                }
                rc
            }
        })?;
        name
    }

    #[cfg(unix)]
    fn lookup_group(gid: u32) -> Option<String> {
        let mut name = None;
        with_buffer(|buf| {
            // SAFETY: as in `lookup_user`.
            unsafe {
                let mut grp: libc::group = std::mem::zeroed();
                let mut result = std::ptr::null_mut();
                let rc = libc::getgrgid_r(gid, &mut grp, buf.as_mut_ptr(), buf.len(), &mut result);
                if rc == 0 && !result.is_null() {
                    name = Some(std::ffi::CStr::from_ptr(grp.gr_name).to_string_lossy().into_owned());
                }
                rc
            }
        })?;
        name
    }
}

/// Rename entries of `dir` (old name, new name) as one batch: first every entry moves to
/// a temporary name, then to its new one, so swaps and chains work. Everything is checked
/// first; if a step fails, what was done is undone as far as possible. Returns how many
/// entries got a new name.
pub fn rename_many(dir: &Path, renames: &[(String, String)]) -> Result<usize> {
    let renames: Vec<&(String, String)> = renames.iter().filter(|(a, b)| a != b).collect();
    let valid = |n: &str| !n.is_empty() && n != "." && n != ".." && !n.contains(['/', '\0']);
    let sources: std::collections::HashSet<&str> = renames.iter().map(|(a, _)| a.as_str()).collect();
    let mut targets = std::collections::HashSet::new();
    for (from, to) in &renames {
        if !valid(from) || !valid(to) {
            bail!("invalid name: {to:?}");
        }
        if !targets.insert(to.as_str()) {
            bail!("two entries would be named {to}");
        }
        dir.join(from)
            .symlink_metadata()
            .with_context(|| format!("cannot rename {from}"))?;
        // Same file under another name = a case-only rename on a case-insensitive disk.
        let target = dir.join(to);
        if !sources.contains(to.as_str()) && target.symlink_metadata().is_ok() && !same_file(&dir.join(from), &target) {
            bail!("{to} already exists");
        }
    }
    let temp = |i: usize| dir.join(format!(".rcmd-rename-{}-{i}", std::process::id()));
    // Phase 1: everything out of the way.
    for (i, (from, _)) in renames.iter().enumerate() {
        if let Err(e) = fs::rename(dir.join(from), temp(i)) {
            for (j, (from, _)) in renames[..i].iter().enumerate().rev() {
                let _ = fs::rename(temp(j), dir.join(from));
            }
            return Err(e).with_context(|| format!("cannot rename {from}"));
        }
    }
    // Phase 2: into place. On failure the rest go back to their old names.
    for (i, (_, to)) in renames.iter().enumerate() {
        if let Err(e) = fs::rename(temp(i), dir.join(to)) {
            let mut lost = Vec::new();
            for (j, (from, _)) in renames.iter().enumerate().skip(i) {
                if fs::rename(temp(j), dir.join(from)).is_err() {
                    lost.push(temp(j).display().to_string());
                }
            }
            let mut msg = format!("cannot rename to {to}: {e}; {i} renamed, the rest kept their names");
            if !lost.is_empty() {
                msg += &format!(" (left as {})", lost.join(", "));
            }
            bail!(msg);
        }
    }
    Ok(renames.len())
}

/// What a file that is not a regular file is, in words.
pub fn special_kind(kind: &fs::FileType) -> &'static str {
    use std::os::unix::fs::FileTypeExt;
    if kind.is_dir() {
        "folder"
    } else if kind.is_fifo() {
        "named pipe"
    } else if kind.is_socket() {
        "socket"
    } else if kind.is_char_device() || kind.is_block_device() {
        "device"
    } else {
        "special file"
    }
}

/// Read up to `limit` bytes of a regular file. Anything else (a named pipe, a device,
/// a socket) is refused without reading: opening or reading it could block forever.
pub fn read(path: &str, limit: u64) -> Result<(Vec<u8>, u64)> {
    use std::os::unix::fs::OpenOptionsExt;
    // Non-blocking, so opening a pipe with no writer returns at once; the check below
    // then looks at what was actually opened.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("cannot open {path}"))?;
    let meta = file.metadata()?;
    let kind = meta.file_type();
    if !kind.is_file() {
        bail!("{path} is a {}; it cannot be shown", special_kind(&kind));
    }
    let size = meta.len();
    let mut buf = Vec::new();
    file.take(limit).read_to_end(&mut buf)?;
    Ok((buf, size))
}

/// Outcome of copying one top-level item.
#[derive(Debug, PartialEq)]
pub enum Copied {
    Done,
    Skipped,
}

/// Returned (inside `anyhow::Error`) when the user cancelled through the progress callback.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// Reports bytes as they are copied; `Break` cancels the operation.
pub type ProgressFn<'a> = dyn FnMut(&Progress) -> ControlFlow<()> + 'a;

/// Entries counted for a folder summary before giving up (a huge folder must not
/// stall every cursor move).
const SUMMARY_LIMIT: usize = 100_000;
/// Time spent counting before giving up.
const SUMMARY_TIME: std::time::Duration = std::time::Duration::from_millis(150);

/// What `path` holds directly: folders, files, hidden ones, total file size.
pub fn summarize(path: &str) -> Result<FolderSummary> {
    let mut sum = FolderSummary {
        location: Location::local(path),
        folders: 0,
        files: 0,
        hidden: 0,
        size: 0,
        partial: false,
    };
    let read = fs::read_dir(path).with_context(|| format!("cannot read {path}"))?;
    let start = std::time::Instant::now();
    for (i, entry) in read.enumerate() {
        // Every cursor move may ask: stop counting on huge or slow (network) folders.
        if i == SUMMARY_LIMIT || (i % 256 == 255 && start.elapsed() > SUMMARY_TIME) {
            sum.partial = true;
            break;
        }
        let Ok(entry) = entry else { continue };
        if entry.file_name().to_string_lossy().starts_with('.') {
            sum.hidden += 1;
        }
        // The type comes with the listing; only links (counted as what they point to,
        // as the panels show them) and files (for their size) need a stat.
        let kind = entry.file_type().ok();
        if kind.is_some_and(|k| k.is_dir()) {
            sum.folders += 1;
            continue;
        }
        let meta = match kind {
            Some(k) if k.is_symlink() => fs::metadata(entry.path()),
            _ => entry.metadata(),
        };
        match meta {
            Ok(m) if m.is_dir() => sum.folders += 1,
            Ok(m) => {
                sum.files += 1;
                sum.size += m.len();
            }
            Err(_) => sum.files += 1,
        }
    }
    Ok(sum)
}

/// Total size of the regular files under `path` (symlinks not followed).
pub fn total_size(path: &Path) -> u64 {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| total_size(&e.path()))
            .sum(),
        Ok(m) if m.is_file() => m.len(),
        _ => 0,
    }
}

/// `path` with symlinks in its existing ancestors resolved. The last component is kept as
/// is (a symlink stays the link itself) and paths that don't exist yet resolve through
/// their deepest existing ancestor.
fn resolve(path: &Path) -> PathBuf {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return path.to_path_buf();
    };
    let mut existing = parent.to_path_buf();
    let mut rest = vec![name.to_os_string()];
    loop {
        if let Ok(p) = fs::canonicalize(&existing) {
            return rest.iter().rev().fold(p, |acc, c| acc.join(c));
        }
        match (existing.file_name(), existing.parent()) {
            (Some(n), Some(up)) => {
                rest.push(n.to_os_string());
                existing = up.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Do two existing paths refer to the same file (hard links, case-insensitive names)?
pub fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (fs::metadata(a), fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(x), Ok(y)) if x == y)
    }
}

/// Refuse operations that would destroy their own source: copying a file onto itself
/// (truncates it) or a directory into its own subtree (recurses forever).
fn check_not_into_itself(src: &Path, dest: &Path, meta: &fs::Metadata) -> Result<()> {
    if meta.is_dir() && resolve(dest).starts_with(resolve(src)) {
        bail!("cannot copy {} into itself", src.display());
    }
    if !meta.file_type().is_symlink() && dest.exists() && same_file(src, dest) {
        bail!("{} and {} are the same file", src.display(), dest.display());
    }
    Ok(())
}

/// Whether an existing `dest` should be replaced by `src` under `policy`.
fn replaces(policy: OnConflict, src: &fs::Metadata, dest: &Path) -> bool {
    match policy {
        OnConflict::Skip => false,
        OnConflict::Overwrite => true,
        OnConflict::IfNewer => match (src.modified(), fs::symlink_metadata(dest).and_then(|m| m.modified())) {
            (Ok(a), Ok(b)) => a > b,
            _ => false,
        },
    }
}

/// What happened to one top-level item of a copy or move.
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    /// Files (or whole renamed items) written.
    pub copied: usize,
    /// Destinations left as they were because of the conflict policy.
    pub skipped: Vec<PathBuf>,
}

impl Outcome {
    /// Done if anything was written, or if there was nothing to skip.
    pub fn result(&self) -> Copied {
        if self.copied == 0 && !self.skipped.is_empty() {
            Copied::Skipped
        } else {
            Copied::Done
        }
    }

    fn add(&mut self, other: Outcome) {
        self.copied += other.copied;
        self.skipped.extend(other.skipped);
    }
}

/// A scratch name next to `dest`: files are written there and renamed over `dest` only
/// once complete, so a failure or cancel never damages what was already at `dest`.
fn partial_path(dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    dest.with_file_name(format!(".{name}.rcmd-{}.partial", std::process::id()))
}

/// Recursively copy `src` to `dest` (the full destination path, not its parent).
/// Directories are merged; existing files are handled by `policy`. Files are replaced
/// atomically (a hard link at the destination is replaced, not written through) and keep
/// their modification time. `done` counts bytes for progress.
pub fn copy(
    src: &Path,
    dest: &Path,
    policy: OnConflict,
    done: &mut u64,
    total: u64,
    progress: &mut ProgressFn,
) -> Result<Outcome> {
    let meta = fs::symlink_metadata(src).with_context(|| format!("cannot read {}", src.display()))?;
    check_not_into_itself(src, dest, &meta)?;
    let mut ctx = CopyCtx {
        policy,
        done,
        total,
        progress,
        outcome: Outcome::default(),
    };
    ctx.copy(src, dest, &meta)?;
    Ok(ctx.outcome)
}

struct CopyCtx<'a, 'b> {
    policy: OnConflict,
    done: &'a mut u64,
    total: u64,
    progress: &'a mut ProgressFn<'b>,
    outcome: Outcome,
}

impl CopyCtx<'_, '_> {
    fn report(&mut self, path: &Path) -> Result<()> {
        let p = Progress {
            path: path.to_string_lossy().into_owned(),
            done: *self.done,
            total: self.total,
        };
        match (self.progress)(&p) {
            ControlFlow::Continue(()) => Ok(()),
            ControlFlow::Break(()) => Err(Cancelled.into()),
        }
    }

    fn copy(&mut self, src: &Path, dest: &Path, meta: &fs::Metadata) -> Result<()> {
        self.report(src)?;
        let existing = fs::symlink_metadata(dest).ok();
        if meta.is_dir() {
            if existing
                .as_ref()
                .is_some_and(|m| !m.is_dir() && !m.file_type().is_symlink())
            {
                bail!("cannot replace file {} with a folder", dest.display());
            }
            // Note: an existing symlink to a folder is followed, like `cp -R`.
            fs::create_dir_all(dest)?;
            for item in fs::read_dir(src)? {
                let item = item?;
                self.copy(
                    &item.path(),
                    &dest.join(item.file_name()),
                    &fs::symlink_metadata(item.path())?,
                )?;
            }
            fs::set_permissions(dest, meta.permissions())?;
            return Ok(());
        }
        if let Some(existing) = &existing {
            if existing.is_dir() {
                bail!("cannot replace folder {} with a file", dest.display());
            }
            // Checked at every level: a hard link or second path to the same file deep in
            // a tree would otherwise be "replaced" by itself.
            if !meta.file_type().is_symlink() && !existing.file_type().is_symlink() && same_file(src, dest) {
                bail!("{} and {} are the same file", src.display(), dest.display());
            }
            if !replaces(self.policy, meta, dest) {
                *self.done += meta.len();
                self.outcome.skipped.push(dest.to_path_buf());
                return Ok(());
            }
        }
        let partial = partial_path(dest);
        let _ = fs::remove_file(&partial);
        let written = if meta.file_type().is_symlink() {
            let target = fs::read_link(src)?;
            #[cfg(unix)]
            let r = std::os::unix::fs::symlink(target, &partial).map_err(Into::into);
            #[cfg(not(unix))]
            let r = fs::copy(src, &partial).map(|_| drop(target)).map_err(Into::into);
            r
        } else {
            self.copy_file(src, &partial, meta)
        };
        let result = written
            .and_then(|()| fs::rename(&partial, dest).with_context(|| format!("cannot write {}", dest.display())));
        if result.is_err() {
            let _ = fs::remove_file(&partial);
        }
        result?;
        self.outcome.copied += 1;
        Ok(())
    }

    /// Copy contents in chunks so progress and cancellation work mid-file.
    fn copy_file(&mut self, src: &Path, dest: &Path, meta: &fs::Metadata) -> Result<()> {
        let mut input = fs::File::open(src).with_context(|| format!("cannot read {}", src.display()))?;
        let mut output = fs::File::create(dest).with_context(|| format!("cannot write {}", dest.display()))?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            output.write_all(&buf[..n])?;
            *self.done += n as u64;
            self.report(src)?;
        }
        output.set_permissions(meta.permissions())?;
        if let Ok(mtime) = meta.modified() {
            output.set_modified(mtime)?;
        }
        Ok(())
    }
}

/// Move `src` to `dest`. A folder moved onto an existing folder is merged entry by entry;
/// whatever `policy` skips stays behind at the source, which is kept in that case.
pub fn move_item(
    src: &Path,
    dest: &Path,
    policy: OnConflict,
    done: &mut u64,
    total: u64,
    progress: &mut ProgressFn,
) -> Result<Outcome> {
    let meta = fs::symlink_metadata(src).with_context(|| format!("cannot read {}", src.display()))?;
    if meta.is_dir() && resolve(dest).starts_with(resolve(src)) && resolve(dest) != resolve(src) {
        bail!("cannot move {} into itself", src.display());
    }
    // A destination that is the source under another name (e.g. a case-only rename on
    // case-insensitive APFS) is not a conflict.
    let dest_meta = fs::symlink_metadata(dest).ok().filter(|_| !same_file(src, dest));
    if let Some(dm) = &dest_meta {
        if meta.is_dir() && dm.is_dir() {
            let mut outcome = Outcome::default();
            for item in fs::read_dir(src)? {
                let item = item?;
                outcome.add(move_item(
                    &item.path(),
                    &dest.join(item.file_name()),
                    policy,
                    done,
                    total,
                    progress,
                )?);
            }
            if outcome.skipped.is_empty() {
                let _ = fs::remove_dir(src);
            }
            return Ok(outcome);
        }
        if dm.is_dir() {
            bail!("cannot replace folder {} with a file", dest.display());
        }
        if !replaces(policy, &meta, dest) {
            return Ok(Outcome {
                copied: 0,
                skipped: vec![dest.to_path_buf()],
            });
        }
    }
    let p = Progress {
        path: src.to_string_lossy().into_owned(),
        done: *done,
        total,
    };
    if progress(&p).is_break() {
        return Err(Cancelled.into());
    }
    match fs::rename(src, dest) {
        Ok(()) => {
            *done += total_size(dest);
            Ok(Outcome {
                copied: 1,
                skipped: Vec::new(),
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
            let outcome = copy(src, dest, policy, done, total, progress)?;
            // Keep the source when anything was skipped: it may hold the only copy.
            if outcome.skipped.is_empty() {
                remove(src)?;
            }
            Ok(outcome)
        }
        Err(e) => Err(e).with_context(|| format!("cannot move {}", src.display())),
    }
}

pub fn remove(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("cannot delete {}", path.display()))?;
    if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
    .with_context(|| format!("cannot delete {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_many_handles_swaps_chains_and_refusals() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        for (n, c) in [("a", "A"), ("b", "B"), ("c", "C"), ("keep", "K")] {
            fs::write(d.join(n), c).unwrap();
        }
        let r = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
        };
        let read = |n: &str| fs::read_to_string(d.join(n)).unwrap();

        // Swap and chain in one batch.
        assert_eq!(rename_many(d, &r(&[("a", "b"), ("b", "a"), ("c", "d")])).unwrap(), 3);
        assert_eq!((read("a"), read("b"), read("d")), ("B".into(), "A".into(), "C".into()));
        assert!(!d.join("c").exists());

        // Refused before touching anything.
        for pairs in [
            r(&[("a", "keep")]),
            r(&[("a", "x"), ("b", "x")]),
            r(&[("a", "x/y")]),
            r(&[("missing", "x")]),
            r(&[("a", "..")]),
        ] {
            assert!(rename_many(d, &pairs).is_err(), "{pairs:?}");
        }
        assert_eq!(
            (read("a"), read("b"), read("keep")),
            ("B".into(), "A".into(), "K".into())
        );
        let leftovers: Vec<_> = fs::read_dir(d)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".rcmd-rename"))
            .collect();
        assert!(leftovers.is_empty());
        // Unchanged names are fine and not counted.
        assert_eq!(rename_many(d, &r(&[("a", "a"), ("b", "e")])).unwrap(), 1);
    }

    fn cp(src: &Path, dest: &Path, policy: OnConflict) -> Result<Copied> {
        copy(src, dest, policy, &mut 0, 0, &mut |_| ControlFlow::Continue(())).map(|o| o.result())
    }

    fn mv(src: &Path, dest: &Path, policy: OnConflict) -> Result<Copied> {
        move_item(src, dest, policy, &mut 0, 0, &mut |_| ControlFlow::Continue(())).map(|o| o.result())
    }

    #[test]
    fn copy_move_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("a/sub")).unwrap();
        fs::write(root.join("a/sub/f.txt"), "hi").unwrap();
        fs::create_dir(root.join("b")).unwrap();

        assert!(matches!(
            cp(&root.join("a"), &root.join("b/a"), OnConflict::Skip).unwrap(),
            Copied::Done
        ));
        assert_eq!(fs::read_to_string(root.join("b/a/sub/f.txt")).unwrap(), "hi");
        assert!(matches!(
            cp(&root.join("a"), &root.join("b/a"), OnConflict::Skip).unwrap(),
            Copied::Skipped
        ));
        assert!(cp(&root.join("a"), &root.join("a/sub/x"), OnConflict::Overwrite).is_err());

        mv(&root.join("b/a"), &root.join("moved"), OnConflict::Skip).unwrap();
        assert!(root.join("moved/sub/f.txt").exists() && !root.join("b/a").exists());
        remove(&root.join("moved")).unwrap();
        assert!(!root.join("moved").exists());

        let names: Vec<_> = list(root.to_str().unwrap())
            .unwrap()
            .entries
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn refuses_to_copy_a_file_onto_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("f.txt");
        fs::write(&f, "precious").unwrap();
        fs::hard_link(&f, tmp.path().join("link.txt")).unwrap();
        assert!(cp(&f, &f, OnConflict::Overwrite).is_err());
        assert!(cp(&f, &tmp.path().join("link.txt"), OnConflict::Overwrite).is_err());
        assert_eq!(fs::read_to_string(&f).unwrap(), "precious");
    }

    #[cfg(unix)]
    #[test]
    fn refuses_to_copy_a_dir_into_itself_via_symlink_or_dotdot() {
        let tmp = tempfile::tempdir().unwrap();
        let x = tmp.path().join("x");
        fs::create_dir_all(x.join("sub")).unwrap();
        std::os::unix::fs::symlink(&x, tmp.path().join("y")).unwrap();
        assert!(cp(&x, &tmp.path().join("y/x"), OnConflict::Skip).is_err());
        assert!(cp(&x, &x.join("sub/../copy"), OnConflict::Skip).is_err());
        assert!(mv(&x, &tmp.path().join("y/x"), OnConflict::Skip).is_err());
        assert_eq!(
            fs::read_dir(&x).unwrap().count(),
            1,
            "nothing was created inside the source"
        );
    }

    #[test]
    fn overwrite_replaces_file_content_and_merges_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("f"), "new").unwrap();
        fs::write(b.join("f"), "old").unwrap();
        fs::write(b.join("extra"), "keep").unwrap();
        assert!(matches!(cp(&a, &b, OnConflict::Skip).unwrap(), Copied::Skipped));
        assert!(matches!(cp(&a, &b, OnConflict::Overwrite).unwrap(), Copied::Done));
        assert_eq!(fs::read_to_string(b.join("f")).unwrap(), "new");
        assert_eq!(fs::read_to_string(b.join("extra")).unwrap(), "keep");
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_copied_and_removed_as_links() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("f"), "x").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        cp(&link, &tmp.path().join("link2"), OnConflict::Skip).unwrap();
        assert!(
            fs::symlink_metadata(tmp.path().join("link2"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        remove(&link).unwrap();
        assert!(target.join("f").exists(), "deleting a link must not delete its target");
    }

    #[cfg(unix)]
    #[test]
    fn lists_unix_metadata() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("run.sh");
        fs::write(&f, "#!/bin/sh").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o750)).unwrap();
        std::os::unix::fs::symlink("run.sh", tmp.path().join("link")).unwrap();
        let listing = list(tmp.path().to_str().unwrap()).unwrap();
        let get = |n: &str| listing.entries.iter().find(|e| e.name == n).unwrap();
        assert_eq!(get("run.sh").mode, Some(0o750));
        assert!(get("run.sh").owner.as_deref().is_some_and(|o| !o.is_empty()));
        assert!(get("run.sh").group.is_some());
        assert_eq!(get("link").link_target.as_deref(), Some("run.sh"));
        assert!(get("link").is_link);
    }

    #[test]
    fn errors_name_the_path() {
        let err = remove(Path::new("/definitely/missing")).unwrap_err();
        assert!(format!("{err:#}").contains("/definitely/missing"));
        assert!(format!("{:#}", list("/definitely/missing").unwrap_err()).contains("/definitely/missing"));
    }

    #[test]
    fn if_newer_and_mtime_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let set = |p: &Path, text: &str, t: std::time::SystemTime| {
            fs::write(p, text).unwrap();
            fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(t)
                .unwrap();
        };
        set(&a.join("newer"), "src", std::time::SystemTime::now());
        set(&b.join("newer"), "dest", old);
        set(&a.join("older"), "src", old);
        set(&b.join("older"), "dest", std::time::SystemTime::now());
        cp(&a, &b, OnConflict::IfNewer).unwrap();
        assert_eq!(fs::read_to_string(b.join("newer")).unwrap(), "src");
        assert_eq!(fs::read_to_string(b.join("older")).unwrap(), "dest");
        // Copies keep the source's modification time.
        fs::remove_file(b.join("older")).unwrap();
        cp(&a.join("older"), &b.join("older"), OnConflict::Skip).unwrap();
        assert_eq!(fs::metadata(b.join("older")).unwrap().modified().unwrap(), old);
    }

    #[test]
    fn cancel_removes_the_partial_file() {
        let tmp = tempfile::tempdir().unwrap();
        let big = tmp.path().join("big");
        fs::write(&big, vec![7u8; 3 << 20]).unwrap();
        let dest = tmp.path().join("copy");
        let mut chunks = 0;
        let r = copy(&big, &dest, OnConflict::Skip, &mut 0, 3 << 20, &mut |p| {
            chunks += 1;
            if p.done >= 1 << 20 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });
        assert!(r.unwrap_err().is::<Cancelled>());
        assert!(!dest.exists(), "partial copy removed");
        assert!(chunks >= 2);
    }

    #[test]
    fn moving_a_dir_onto_a_dir_merges() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        fs::create_dir_all(a.join("sub")).unwrap();
        fs::create_dir_all(b.join("sub")).unwrap();
        fs::write(a.join("sub/new"), "n").unwrap();
        fs::write(a.join("sub/clash"), "a").unwrap();
        fs::write(b.join("sub/clash"), "b").unwrap();
        // Skip still merges: new files move, existing ones stay, and so does the source.
        assert_eq!(mv(&a, &b, OnConflict::Skip).unwrap(), Copied::Done);
        assert!(b.join("sub/new").exists() && !a.join("sub/new").exists());
        assert_eq!(fs::read_to_string(b.join("sub/clash")).unwrap(), "b");
        assert!(a.join("sub/clash").exists(), "skipped file kept at the source");
        assert_eq!(mv(&a, &b, OnConflict::Overwrite).unwrap(), Copied::Done);
        assert_eq!(fs::read_to_string(b.join("sub/clash")).unwrap(), "a");
        assert!(!a.exists(), "emptied source removed");
    }

    #[cfg(unix)]
    #[test]
    fn failed_or_cancelled_overwrites_keep_the_old_file() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let (src, dest) = (tmp.path().join("src"), tmp.path().join("dest"));
        fs::write(&dest, "old").unwrap();
        // Unreadable source: the destination is untouched.
        fs::write(&src, "new").unwrap();
        fs::set_permissions(&src, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&src).is_err() {
            assert!(cp(&src, &dest, OnConflict::Overwrite).is_err());
            assert_eq!(fs::read_to_string(&dest).unwrap(), "old");
        }
        fs::set_permissions(&src, fs::Permissions::from_mode(0o644)).unwrap();
        // Cancelled mid-file: the old content survives, no scratch file is left.
        fs::write(&src, vec![1u8; 3 << 20]).unwrap();
        let r = copy(&src, &dest, OnConflict::Overwrite, &mut 0, 3 << 20, &mut |p| {
            if p.done >= 1 << 20 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });
        assert!(r.unwrap_err().is::<Cancelled>());
        assert_eq!(fs::read_to_string(&dest).unwrap(), "old");
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 2, "no partial file left");
        // A read-only destination can still be replaced (it is renamed over).
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o444)).unwrap();
        fs::write(&src, "new").unwrap();
        assert_eq!(cp(&src, &dest, OnConflict::Overwrite).unwrap(), Copied::Done);
        assert_eq!(fs::read_to_string(&dest).unwrap(), "new");
    }

    #[test]
    fn nested_hard_links_are_never_truncated() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("x"), "precious").unwrap();
        fs::hard_link(a.join("x"), b.join("x")).unwrap();
        assert!(cp(&a, &b, OnConflict::Overwrite).is_err());
        assert_eq!(fs::read_to_string(a.join("x")).unwrap(), "precious");
    }

    #[test]
    fn single_file_not_newer_is_reported_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dest) = (tmp.path().join("src"), tmp.path().join("dest"));
        fs::write(&src, "older").unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        fs::File::options()
            .write(true)
            .open(&src)
            .unwrap()
            .set_modified(old)
            .unwrap();
        fs::write(&dest, "newer").unwrap();
        assert_eq!(cp(&src, &dest, OnConflict::IfNewer).unwrap(), Copied::Skipped);
        assert_eq!(fs::read_to_string(&dest).unwrap(), "newer");
    }
}
