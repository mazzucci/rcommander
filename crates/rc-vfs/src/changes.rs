//! "Claude's changes": a log of files an agent edited, kept as an ordinary directory so
//! both UIs can show it like any folder. `links/` holds one symlink per edited file;
//! `originals/` holds each file's content from before its first recorded edit.

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub struct ChangeLog {
    links: PathBuf,
    originals: PathBuf,
}

impl ChangeLog {
    /// Use (and create) `links` and `originals` under `dir`, which the caller has made
    /// private to the user.
    pub fn open(dir: &Path) -> Result<Self> {
        let log = ChangeLog {
            links: dir.join("links"),
            originals: dir.join("originals"),
        };
        for d in [&log.links, &log.originals] {
            fs::create_dir_all(d).with_context(|| format!("cannot create {}", d.display()))?;
        }
        Ok(log)
    }

    /// The folder to show in a panel.
    pub fn links_dir(&self) -> &Path {
        &self.links
    }

    pub fn originals_dir(&self) -> &Path {
        &self.originals
    }

    /// Note that `file` is being edited, with `original` its content before the edit.
    /// Only the first edit of a file is recorded, so the diff shows everything changed.
    /// Returns false if the file was already in the log.
    pub fn record(&self, file: &Path, original: &[u8]) -> Result<bool> {
        if self.entry_for(file).is_some() {
            return Ok(false);
        }
        let base = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        // Same-named files from different folders get " (2)", " (3)", …
        let name = (1..)
            .map(|i| if i == 1 { base.clone() } else { format!("{base} ({i})") })
            .find(|n| self.links.join(n).symlink_metadata().is_err())
            .expect("an unused name");
        write_private(&self.originals.join(&name), original)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(file, self.links.join(&name))?;
        Ok(true)
    }

    /// The log entry (link name) for `file`, if recorded.
    pub fn entry_for(&self, file: &Path) -> Option<String> {
        fs::read_dir(&self.links).ok()?.flatten().find_map(|e| {
            let target = fs::read_link(e.path()).ok()?;
            (target == file).then(|| e.file_name().to_string_lossy().into_owned())
        })
    }

    /// The original content of an entry, and the file it stands for.
    pub fn pair(&self, name: &str) -> Option<(PathBuf, PathBuf)> {
        let target = fs::read_link(self.links.join(name)).ok()?;
        Some((self.originals.join(name), target))
    }

    /// Drop an entry from the log. The edited file itself is never touched.
    pub fn forget(&self, name: &str) -> Result<()> {
        let _ = fs::remove_file(self.originals.join(name));
        fs::remove_file(self.links.join(name)).with_context(|| format!("no change entry {name}"))
    }

    /// Drop every entry.
    pub fn clear(&self) -> Result<()> {
        for e in fs::read_dir(&self.links)?.flatten() {
            self.forget(&e.file_name().to_string_lossy())?;
        }
        Ok(())
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let _ = fs::remove_file(path);
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    opts.mode(0o600);
    opts.open(path)?.write_all(bytes)?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn records_first_edit_only_and_names_collisions() {
        let tmp = tempfile::tempdir().unwrap();
        let log = ChangeLog::open(&tmp.path().join("log")).unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let (fa, fb) = (a.join("lib.rs"), b.join("lib.rs"));
        fs::write(&fa, "v1").unwrap();
        fs::write(&fb, "x").unwrap();
        assert!(log.record(&fa, b"v0").unwrap());
        assert!(!log.record(&fa, b"v1").unwrap(), "only the first edit is kept");
        assert!(log.record(&fb, b"").unwrap());
        assert_eq!(log.entry_for(&fa).as_deref(), Some("lib.rs"));
        assert_eq!(log.entry_for(&fb).as_deref(), Some("lib.rs (2)"));
        let (orig, target) = log.pair("lib.rs").unwrap();
        assert_eq!(fs::read_to_string(orig).unwrap(), "v0");
        assert_eq!(target, fa);
        log.forget("lib.rs").unwrap();
        assert!(fa.exists(), "forgetting never touches the file");
        assert_eq!(log.entry_for(&fa), None);
        log.clear().unwrap();
        assert_eq!(fs::read_dir(log.links_dir()).unwrap().count(), 0);
        assert_eq!(fs::read_dir(log.originals_dir()).unwrap().count(), 0);
    }
}
