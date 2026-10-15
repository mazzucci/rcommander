//! Directory watching for live refresh. Watches the directories the panels show
//! (non-recursively) and reports, debounced, which of them changed.

use anyhow::Result;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

/// Changes within this window are reported once.
const DEBOUNCE: Duration = Duration::from_millis(150);

pub struct DirWatcher {
    watcher: RecommendedWatcher,
    watched: BTreeSet<PathBuf>,
}

impl DirWatcher {
    /// `on_change` is called from a background thread with each directory that changed.
    pub fn new(on_change: impl Fn(PathBuf) + Send + 'static) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<PathBuf>();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            for path in event.paths {
                // A change inside a watched directory, or to the directory itself.
                if let Some(parent) = path.parent() {
                    let _ = tx.send(parent.to_path_buf());
                }
                let _ = tx.send(path);
            }
        })?;
        std::thread::spawn(move || {
            while let Ok(first) = rx.recv() {
                let mut dirs = BTreeSet::from([first]);
                std::thread::sleep(DEBOUNCE);
                dirs.extend(rx.try_iter());
                dirs.into_iter().for_each(&on_change);
            }
        });
        Ok(DirWatcher {
            watcher,
            watched: BTreeSet::new(),
        })
    }

    /// Watch exactly `dirs` (duplicates and unwatchable paths are fine).
    pub fn set<'a>(&mut self, dirs: impl IntoIterator<Item = &'a Path>) {
        let wanted: BTreeSet<PathBuf> = dirs.into_iter().map(Path::to_path_buf).collect();
        for gone in self.watched.difference(&wanted) {
            let _ = self.watcher.unwatch(gone);
        }
        for new in wanted.difference(&self.watched) {
            let _ = self.watcher.watch(new, RecursiveMode::NonRecursive);
        }
        self.watched = wanted;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn reports_changes_in_watched_dirs_only() {
        let tmp = tempfile::tempdir().unwrap();
        // Canonical paths: on macOS the temp dir is behind a symlink.
        let a = tmp.path().canonicalize().unwrap().join("a");
        let b = tmp.path().canonicalize().unwrap().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let (tx, rx) = mpsc::channel();
        let mut w = DirWatcher::new(move |dir| {
            let _ = tx.send(dir);
        })
        .unwrap();
        w.set([a.as_path()]);
        std::thread::sleep(Duration::from_millis(100));
        std::fs::write(b.join("ignored"), "x").unwrap();
        std::fs::write(a.join("f"), "x").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = BTreeSet::new();
        while Instant::now() < deadline && !seen.contains(&a) {
            if let Ok(dir) = rx.recv_timeout(Duration::from_millis(100)) {
                seen.insert(dir);
            }
        }
        assert!(seen.contains(&a), "{seen:?}");
        assert!(!seen.contains(&b));
    }
}
