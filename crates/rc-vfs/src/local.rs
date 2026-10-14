//! Local filesystem listing and file operations.

use anyhow::{Context, Result, bail};
use rc_core::{Entry, Listing, Location};
use std::fs;
use std::io::Read;
use std::path::Path;
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

/// Recursively copy `src` to `dest` (the full destination path, not its parent).
/// Existing destinations are skipped unless `overwrite`; directories are merged.
pub fn copy(src: &Path, dest: &Path, overwrite: bool, progress: &mut dyn FnMut(&str)) -> Result<Copied> {
    let meta = fs::symlink_metadata(src).with_context(|| format!("cannot read {}", src.display()))?;
    if meta.is_dir() && dest.starts_with(src) {
        bail!("cannot copy {} into itself", src.display());
    }
    if dest.exists() && !overwrite {
        return Ok(Copied::Skipped);
    }
    copy_inner(src, dest, &meta, progress)?;
    Ok(Copied::Done)
}

fn copy_inner(src: &Path, dest: &Path, meta: &fs::Metadata, progress: &mut dyn FnMut(&str)) -> Result<()> {
    progress(&src.to_string_lossy());
    if meta.file_type().is_symlink() {
        let target = fs::read_link(src)?;
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
    if dest.exists() && !overwrite {
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
    let meta = fs::symlink_metadata(path)?;
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
}
