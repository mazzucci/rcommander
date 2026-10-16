//! The Claude map panel (`i`): the panel that is not active shows Claude's summary of
//! a file or folder, with links to follow. Made on a worker thread; closing stops it.

use rc_core::Location;
use rc_core::api::{FileMap, MapItem};
use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::Instant;

pub enum MapState {
    Loading { started: Instant },
    Ready(FileMap),
    Failed(String),
}

pub struct MapPanel {
    pub target: Location,
    pub state: MapState,
    /// The keyboard is in the map (Tab), not in the file panel.
    pub focused: bool,
    /// Selected link: index into `links()`.
    pub cursor: usize,
    /// First visible line, kept up to date by the renderer.
    pub scroll: usize,
    rx: Option<Receiver<Result<FileMap, String>>>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// Seconds shown by the loading timer.
    shown_secs: u64,
    cancel: Arc<AtomicBool>,
}

impl Drop for MapPanel {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl MapPanel {
    /// Start making the map of `target` with `mapper` (from the cache unless `refresh`).
    pub fn start(target: Location, mapper: rc_vfs::map::Mapper, refresh: bool) -> Self {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&cancel);
        let loc = target.clone();
        let worker = std::thread::spawn(move || {
            let result = mapper.map(&loc, refresh, &mut |_| {
                if stop.load(Ordering::Relaxed) {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            });
            let _ = tx.send(result.map_err(|e| format!("{e:#}")));
        });
        MapPanel {
            target,
            state: MapState::Loading {
                started: Instant::now(),
            },
            focused: false,
            cursor: 0,
            scroll: 0,
            rx: Some(rx),
            worker: Some(worker),
            shown_secs: 0,
            cancel,
        }
    }

    /// Take in the worker's answer. Returns true if a redraw is due (also once a second
    /// while loading, for the timer).
    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.rx else { return false };
        match rx.try_recv() {
            Ok(result) => {
                self.state = match result {
                    Ok(map) => MapState::Ready(map),
                    Err(e) => MapState::Failed(e),
                };
                self.rx = None;
                true
            }
            Err(mpsc::TryRecvError::Empty) => {
                let MapState::Loading { started } = self.state else {
                    return false;
                };
                let secs = started.elapsed().as_secs();
                std::mem::replace(&mut self.shown_secs, secs) != secs
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.state = MapState::Failed("the map stopped unexpectedly".into());
                self.rx = None;
                true
            }
        }
    }

    /// Stop Claude and wait until it is gone (quitting must not leave it running).
    pub fn stop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }

    /// The items that link somewhere, in reading order.
    pub fn links(&self) -> Vec<&MapItem> {
        match &self.state {
            MapState::Ready(map) => map
                .sections
                .iter()
                .flat_map(|s| &s.items)
                .filter(|i| i.path.is_some())
                .collect(),
            _ => Vec::new(),
        }
    }

    pub fn current(&self) -> Option<MapItem> {
        self.links().get(self.cursor).map(|i| (*i).clone())
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let n = self.links().len();
        if n > 0 {
            self.cursor = self.cursor.saturating_add_signed(delta).min(n - 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn stop_ends_claude_before_returning() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("claude");
        let pid_file = tmp.path().join("pid");
        std::fs::write(
            &fake,
            format!("#!/bin/sh\necho $$ > {}\nexec sleep 30\n", pid_file.display()),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mapper = rc_vfs::map::Mapper {
            command: fake.to_string_lossy().into(),
            model: String::new(),
            cache: None,
        };
        let mut panel = MapPanel::start(Location::local(tmp.path().to_string_lossy()), mapper, true);
        let start = Instant::now();
        while !pid_file.exists() {
            assert!(start.elapsed().as_secs() < 10);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        let pid: i32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        let start = Instant::now();
        panel.stop();
        assert!(start.elapsed().as_secs() < 3, "stop waited {:?}", start.elapsed());
        // SAFETY: signal 0 only checks whether the process exists.
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "claude is still running");
    }

    #[test]
    fn loads_in_the_background_and_lists_links() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "x").unwrap();
        let fake = tmp.path().join("claude");
        let answer = r#"{"is_error":false,"structured_output":{"summary":"S","sections":[{"heading":"H","items":[{"text":"a","path":"a.txt","line":1},{"text":"none","path":"","line":null},{"text":"dir","path":".","line":null}]}]}}"#;
        std::fs::write(&fake, format!("#!/bin/sh\ncat <<'EOF'\n{answer}\nEOF\n")).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mapper = rc_vfs::map::Mapper {
            command: fake.to_string_lossy().into(),
            model: String::new(),
            cache: None,
        };
        let mut panel = MapPanel::start(Location::local(tmp.path().to_string_lossy()), mapper, false);
        let start = Instant::now();
        while matches!(panel.state, MapState::Loading { .. }) {
            panel.poll();
            assert!(start.elapsed().as_secs() < 10);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(panel.links().len(), 2, "only items with a path are links");
        panel.move_cursor(5);
        assert_eq!(panel.cursor, 1);
        assert!(panel.current().unwrap().is_dir);
    }
}
