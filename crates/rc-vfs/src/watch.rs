//! Directory watching for live refresh. Watches the directories the panels show
//! (non-recursively) and reports, debounced, which of them changed.

use anyhow::Result;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

/// Changes within this window are reported once.
const DEBOUNCE: Duration = Duration::from_millis(150);

pub struct DirWatcher {
    watcher: RecommendedWatcher,
    /// Shared with the event handler, which only forwards these directories.
    watched: Arc<Mutex<BTreeSet<PathBuf>>>,
}

impl DirWatcher {
    /// `on_change` is called from a background thread with each directory that changed.
    pub fn new(on_change: impl Fn(PathBuf) + Send + 'static) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<PathBuf>();
        let watched = Arc::new(Mutex::new(BTreeSet::<PathBuf>::new()));
        let filter = Arc::clone(&watched);
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            // Reading a directory (as every refresh does) is not a change; reporting it
            // would make a watcher that refreshes on change loop forever.
            if matches!(event.kind, EventKind::Access(_)) {
                return;
            }
            let watched = filter.lock().expect("watch set");
            for path in event.paths {
                // A change inside a watched directory, or to the directory itself. Only
                // directories are forwarded, so a burst of writes stays a handful of messages.
                let dir = if watched.contains(&path) {
                    Some(path.as_path())
                } else {
                    path.parent()
                };
                if let Some(dir) = dir.filter(|d| watched.contains(*d)) {
                    let _ = tx.send(dir.to_path_buf());
                }
            }
        })?;
        std::thread::Builder::new().name("dir-watch".into()).spawn(move || {
            while let Ok(first) = rx.recv() {
                let mut dirs = BTreeSet::from([first]);
                std::thread::sleep(DEBOUNCE);
                dirs.extend(rx.try_iter());
                dirs.into_iter().for_each(&on_change);
            }
        })?;
        Ok(DirWatcher { watcher, watched })
    }

    /// Watch exactly `dirs` (duplicates and unwatchable paths are fine).
    pub fn set<'a>(&mut self, dirs: impl IntoIterator<Item = &'a Path>) {
        let wanted: BTreeSet<PathBuf> = dirs.into_iter().map(Path::to_path_buf).collect();
        let mut watched = self.watched.lock().expect("watch set");
        for gone in watched.difference(&wanted) {
            let _ = self.watcher.unwatch(gone);
        }
        for new in wanted.difference(&watched) {
            let _ = self.watcher.watch(new, RecursiveMode::NonRecursive);
        }
        *watched = wanted;
    }

    /// Watch `dir` afresh. After a directory is deleted and recreated under the same
    /// name, the OS has dropped the old watch.
    pub fn rewatch(&mut self, dir: &Path) {
        let _ = self.watcher.unwatch(dir);
        let _ = self.watcher.watch(dir, RecursiveMode::NonRecursive);
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

    #[test]
    fn bursts_are_coalesced_and_unwatched_dirs_go_quiet() {
        let tmp = tempfile::tempdir().unwrap();
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
        for i in 0..200 {
            std::fs::write(a.join(format!("f{i}")), "x").unwrap();
        }
        std::thread::sleep(Duration::from_millis(600));
        let reports: Vec<PathBuf> = rx.try_iter().collect();
        assert!(
            !reports.is_empty() && reports.len() < 20,
            "{} reports for 200 writes",
            reports.len()
        );
        assert!(reports.iter().all(|d| d == &a), "only the directory is reported");

        w.set([b.as_path()]);
        std::thread::sleep(Duration::from_millis(100));
        std::fs::write(a.join("late"), "x").unwrap();
        std::thread::sleep(Duration::from_millis(400));
        assert!(rx.try_iter().all(|d| d != a), "a is no longer watched");
    }

    #[test]
    fn reading_a_directory_is_not_a_change() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        std::fs::write(dir.join("f"), "x").unwrap();
        let (tx, rx) = mpsc::channel();
        let mut w = DirWatcher::new(move |dir| {
            let _ = tx.send(dir);
        })
        .unwrap();
        w.set([dir.as_path()]);
        std::thread::sleep(Duration::from_millis(100));
        let _ = std::fs::read_dir(&dir).unwrap().count();
        let _ = std::fs::read(dir.join("f")).unwrap();
        assert!(rx.recv_timeout(DEBOUNCE * 4).is_err(), "reads were reported as changes");
    }
}
