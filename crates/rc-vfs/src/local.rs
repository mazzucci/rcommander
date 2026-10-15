//! Local filesystem listing and file operations.

use anyhow::{Context, Result, bail};
use rc_core::{Entry, Listing, Location};
use std::fs;
use std::io::Read;
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
pub enum Copied {
    Done,
    Skipped,
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

/// Recursively copy `src` to `dest` (the full destination path, not its parent).
/// Existing destinations are skipped unless `overwrite`; directories are merged.
pub fn copy(src: &Path, dest: &Path, overwrite: bool, progress: &mut dyn FnMut(&str)) -> Result<Copied> {
    let meta = fs::symlink_metadata(src).with_context(|| format!("cannot read {}", src.display()))?;
    check_not_into_itself(src, dest, &meta)?;
    if dest.symlink_metadata().is_ok() && !overwrite {
        return Ok(Copied::Skipped);
    }
    copy_inner(src, dest, &meta, progress)?;
    Ok(Copied::Done)
}

fn copy_inner(src: &Path, dest: &Path, meta: &fs::Metadata, progress: &mut dyn FnMut(&str)) -> Result<()> {
    progress(&src.to_string_lossy());
    if meta.file_type().is_symlink() {
        let target = fs::read_link(src)?;
        // Replace whatever is there (a file or another link), never write through it.
        let _ = fs::remove_file(dest);
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, dest)?;
        #[cfg(not(unix))]
        fs::copy(src, dest).map(|_| drop(target))?;
    } else if meta.is_dir() {
        fs::create_dir_all(dest)?;
        for item in fs::read_dir(src)? {
            let item = item?;
            copy_inner(
                &item.path(),
                &dest.join(item.file_name()),
                &fs::symlink_metadata(item.path())?,
                progress,
            )?;
        }
        fs::set_permissions(dest, meta.permissions())?;
    } else {
        fs::copy(src, dest).with_context(|| format!("cannot copy to {}", dest.display()))?;
    }
    Ok(())
}

pub fn move_item(src: &Path, dest: &Path, overwrite: bool, progress: &mut dyn FnMut(&str)) -> Result<Copied> {
    let meta = fs::symlink_metadata(src).with_context(|| format!("cannot read {}", src.display()))?;
    if meta.is_dir() && resolve(dest).starts_with(resolve(src)) && resolve(dest) != resolve(src) {
        bail!("cannot move {} into itself", src.display());
    }
    // A destination that is the source under another name (e.g. a case-only rename on
    // case-insensitive APFS) is not a conflict.
    let exists = dest.symlink_metadata().is_ok() && !same_file(src, dest);
    if exists && !overwrite {
        return Ok(Copied::Skipped);
    }
    progress(&src.to_string_lossy());
    match fs::rename(src, dest) {
        Ok(()) => Ok(Copied::Done),
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
            copy(src, dest, overwrite, progress)?;
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

    #[test]
    fn copy_move_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("a/sub")).unwrap();
        fs::write(root.join("a/sub/f.txt"), "hi").unwrap();
        fs::create_dir(root.join("b")).unwrap();

        let mut seen = 0;
        let mut p = |_: &str| seen += 1;
        assert!(matches!(
            copy(&root.join("a"), &root.join("b/a"), false, &mut p).unwrap(),
            Copied::Done
        ));
        assert_eq!(fs::read_to_string(root.join("b/a/sub/f.txt")).unwrap(), "hi");
        assert!(matches!(
            copy(&root.join("a"), &root.join("b/a"), false, &mut p).unwrap(),
            Copied::Skipped
        ));
        assert!(copy(&root.join("a"), &root.join("a/sub/x"), true, &mut p).is_err());

        move_item(&root.join("b/a"), &root.join("moved"), false, &mut p).unwrap();
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
        let mut p = |_: &str| {};
        assert!(copy(&f, &f, true, &mut p).is_err());
        assert!(copy(&f, &tmp.path().join("link.txt"), true, &mut p).is_err());
        assert_eq!(fs::read_to_string(&f).unwrap(), "precious");
    }

    #[cfg(unix)]
    #[test]
    fn refuses_to_copy_a_dir_into_itself_via_symlink_or_dotdot() {
        let tmp = tempfile::tempdir().unwrap();
        let x = tmp.path().join("x");
        fs::create_dir_all(x.join("sub")).unwrap();
        std::os::unix::fs::symlink(&x, tmp.path().join("y")).unwrap();
        let mut p = |_: &str| {};
        assert!(copy(&x, &tmp.path().join("y/x"), false, &mut p).is_err());
        assert!(copy(&x, &x.join("sub/../copy"), false, &mut p).is_err());
        assert!(move_item(&x, &tmp.path().join("y/x"), false, &mut p).is_err());
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
        let mut p = |_: &str| {};
        assert!(matches!(copy(&a, &b, false, &mut p).unwrap(), Copied::Skipped));
        assert!(matches!(copy(&a, &b, true, &mut p).unwrap(), Copied::Done));
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
        let mut p = |_: &str| {};
        copy(&link, &tmp.path().join("link2"), false, &mut p).unwrap();
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
}
