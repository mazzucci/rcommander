//! Local filesystem listing and file operations.

use anyhow::{Context, Result, bail};
use rc_core::api::{OnConflict, Progress};
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

pub fn read(path: &str, limit: u64) -> Result<(Vec<u8>, u64)> {
    let file = fs::File::open(path).with_context(|| format!("cannot open {path}"))?;
    let size = file.metadata()?.len();
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

/// Recursively copy `src` to `dest` (the full destination path, not its parent).
/// Existing files are handled by `policy`; directories are merged. Modification times
/// are preserved. `done` counts bytes copied so far, for progress.
pub fn copy(
    src: &Path,
    dest: &Path,
    policy: OnConflict,
    done: &mut u64,
    total: u64,
    progress: &mut ProgressFn,
) -> Result<Copied> {
    let meta = fs::symlink_metadata(src).with_context(|| format!("cannot read {}", src.display()))?;
    check_not_into_itself(src, dest, &meta)?;
    if dest.symlink_metadata().is_ok() && policy == OnConflict::Skip {
        return Ok(Copied::Skipped);
    }
    let mut ctx = CopyCtx {
        policy,
        done,
        total,
        progress,
    };
    ctx.copy(src, dest, &meta)?;
    Ok(Copied::Done)
}

struct CopyCtx<'a, 'b> {
    policy: OnConflict,
    done: &'a mut u64,
    total: u64,
    progress: &'a mut ProgressFn<'b>,
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
        if meta.is_dir() {
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
        if dest.symlink_metadata().is_ok() && !replaces(self.policy, meta, dest) {
            *self.done += meta.len();
            return Ok(());
        }
        if meta.file_type().is_symlink() {
            let target = fs::read_link(src)?;
            // Replace whatever is there (a file or another link), never write through it.
            let _ = fs::remove_file(dest);
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, dest)?;
            #[cfg(not(unix))]
            fs::copy(src, dest).map(|_| drop(target))?;
            return Ok(());
        }
        // Replace a link at the destination instead of writing through it.
        if dest.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
            fs::remove_file(dest)?;
        }
        let result = self.copy_file(src, dest, meta);
        if result.is_err() {
            // Never leave a half-written file behind.
            let _ = fs::remove_file(dest);
        }
        result
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

/// Move `src` to `dest`. Conflicts follow `policy`; a directory moved onto an existing
/// directory is merged entry by entry, leaving behind only what was skipped.
pub fn move_item(
    src: &Path,
    dest: &Path,
    policy: OnConflict,
    done: &mut u64,
    total: u64,
    progress: &mut ProgressFn,
) -> Result<Copied> {
    let meta = fs::symlink_metadata(src).with_context(|| format!("cannot read {}", src.display()))?;
    if meta.is_dir() && resolve(dest).starts_with(resolve(src)) && resolve(dest) != resolve(src) {
        bail!("cannot move {} into itself", src.display());
    }
    // A destination that is the source under another name (e.g. a case-only rename on
    // case-insensitive APFS) is not a conflict.
    let dest_meta = fs::symlink_metadata(dest).ok().filter(|_| !same_file(src, dest));
    if let Some(dm) = &dest_meta {
        if meta.is_dir() && dm.is_dir() {
            if policy == OnConflict::Skip {
                return Ok(Copied::Skipped);
            }
            let mut moved_all = true;
            for item in fs::read_dir(src)? {
                let item = item?;
                let r = move_item(
                    &item.path(),
                    &dest.join(item.file_name()),
                    policy,
                    done,
                    total,
                    progress,
                )?;
                moved_all &= r == Copied::Done;
            }
            if moved_all {
                let _ = fs::remove_dir(src);
            }
            return Ok(Copied::Done);
        }
        if !replaces(policy, &meta, dest) {
            return Ok(Copied::Skipped);
        }
        if dm.is_dir() {
            bail!("cannot replace folder {} with a file", dest.display());
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
            Ok(Copied::Done)
        }
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
            copy(src, dest, policy, done, total, progress)?;
            remove(src)?;
            Ok(Copied::Done)
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

    fn cp(src: &Path, dest: &Path, policy: OnConflict) -> Result<Copied> {
        copy(src, dest, policy, &mut 0, 0, &mut |_| ControlFlow::Continue(()))
    }

    fn mv(src: &Path, dest: &Path, policy: OnConflict) -> Result<Copied> {
        move_item(src, dest, policy, &mut 0, 0, &mut |_| ControlFlow::Continue(()))
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
        assert_eq!(mv(&a, &b, OnConflict::Skip).unwrap(), Copied::Skipped);
        assert_eq!(mv(&a, &b, OnConflict::Overwrite).unwrap(), Copied::Done);
        assert_eq!(fs::read_to_string(b.join("sub/clash")).unwrap(), "a");
        assert!(b.join("sub/new").exists());
        assert!(!a.exists(), "emptied source removed");
    }
}
