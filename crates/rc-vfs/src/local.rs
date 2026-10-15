//! Local filesystem listing and file operations.

use anyhow::{Context, Result, bail};
use rc_core::{Entry, Listing, Location};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub fn list(path: &str) -> Result<Listing> {
    let mut entries = Vec::new();
    for item in fs::read_dir(path).with_context(|| format!("cannot read {path}"))? {
        let Ok(item) = item else { continue };
        let name = item.file_name().to_string_lossy().into_owned();
        let Ok(meta) = item.metadata() else { continue };
        let is_link = meta.file_type().is_symlink();
        // For symlinks, describe the target (falls back to the link itself if dangling).
        let meta = if is_link {
            fs::metadata(item.path()).unwrap_or(meta)
        } else {
            meta
        };
        entries.push(Entry {
            hidden: name.starts_with('.'),
            is_dir: meta.is_dir(),
            is_link,
            size: (!meta.is_dir()).then_some(meta.len()),
            modified: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64),
            name,
        });
    }
    Ok(Listing {
        location: Location::local(path),
        entries,
        writable: true,
    })
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

    #[test]
    fn errors_name_the_path() {
        let err = remove(Path::new("/definitely/missing")).unwrap_err();
        assert!(format!("{err:#}").contains("/definitely/missing"));
        assert!(format!("{:#}", list("/definitely/missing").unwrap_err()).contains("/definitely/missing"));
    }
}
