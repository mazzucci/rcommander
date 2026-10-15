//! The Find Files dialog (Alt+F7): a name mask and an optional text, searched on a
//! worker thread; the hits can be gone to (Enter) or viewed (F3).

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rc_core::Location;
use rc_core::api::{FindHit, FindRequest, FindResult, Progress};
use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Field {
    Name,
    Contains,
}

pub struct Find {
    pub root: Location,
    pub name: String,
    pub contains: String,
    pub field: Field,
    /// Search hidden files and folders (from the panel the search started in).
    pub hidden: bool,
    pub search: Option<Search>,
    /// Hits of the last search; `None` while the fields are being edited.
    pub result: Option<FindResult>,
    pub error: Option<String>,
    pub cursor: usize,
    /// First visible result row and rows that fit; kept up to date by the renderer.
    pub scroll: usize,
    pub page: usize,
}

/// A search running on a worker thread. Dropping it (closing the dialog) stops it.
pub struct Search {
    rx: Receiver<Msg>,
    cancel: Arc<AtomicBool>,
    pub progress: Option<Progress>,
}

impl Drop for Search {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

enum Msg {
    Progress(Progress),
    Done(Result<FindResult, String>),
}

/// What the dialog asks the app to do after a key.
#[derive(Debug, PartialEq)]
pub enum Action {
    None,
    Close,
    Start,
    GoTo(FindHit),
    View(FindHit),
}

/// Progress messages from the worker are sent at most this often.
const REPORT_EVERY: Duration = Duration::from_millis(100);

impl Find {
    pub fn new(root: Location, hidden: bool) -> Self {
        Find {
            root,
            name: String::new(),
            contains: String::new(),
            field: Field::Name,
            hidden,
            search: None,
            result: None,
            error: None,
            cursor: 0,
            scroll: 0,
            page: 10,
        }
    }

    /// Start searching with the current fields.
    pub fn start(&mut self) {
        let req = FindRequest {
            root: self.root.clone(),
            name: self.name.trim().to_string(),
            contains: self.contains.clone(),
            hidden: self.hidden,
        };
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&cancel);
        std::thread::spawn(move || {
            let mut last = Instant::now() - REPORT_EVERY;
            let result = rc_vfs::find::find(&req, &mut |p| {
                if last.elapsed() >= REPORT_EVERY {
                    last = Instant::now();
                    let _ = tx.send(Msg::Progress(p.clone()));
                }
                if stop.load(Ordering::Relaxed) {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            });
            let _ = tx.send(Msg::Done(result.map_err(|e| format!("{e:#}"))));
        });
        self.search = Some(Search {
            rx,
            cancel,
            progress: None,
        });
        self.result = None;
        self.error = None;
        self.cursor = 0;
        self.scroll = 0;
    }

    /// Take in what the worker reported. Returns true if a redraw is due.
    pub fn poll(&mut self) -> bool {
        let Some(search) = &mut self.search else { return false };
        let mut changed = false;
        loop {
            match search.rx.try_recv() {
                Ok(Msg::Progress(p)) => {
                    search.progress = Some(p);
                    changed = true;
                }
                Ok(Msg::Done(result)) => {
                    match result {
                        Ok(result) => self.result = Some(result),
                        Err(e) => self.error = Some(e),
                    }
                    self.search = None;
                    return true;
                }
                Err(mpsc::TryRecvError::Empty) => return changed,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.search = None;
                    self.error = Some("the search stopped unexpectedly".into());
                    return true;
                }
            }
        }
    }

    pub fn hits(&self) -> &[FindHit] {
        self.result.as_ref().map_or(&[], |r| &r.hits)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if let Some(search) = &self.search {
            if key.code == KeyCode::Esc {
                // Stops at the next folder or file; the hits so far stay.
                search.cancel.store(true, Ordering::Relaxed);
            }
            return Action::None;
        }
        if self.result.is_some() {
            return self.results_key(key);
        }
        let text = match self.field {
            Field::Name => &mut self.name,
            Field::Contains => &mut self.contains,
        };
        match key.code {
            KeyCode::Esc => return Action::Close,
            KeyCode::Enter => return Action::Start,
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => {
                self.field = match self.field {
                    Field::Name => Field::Contains,
                    Field::Contains => Field::Name,
                }
            }
            KeyCode::Backspace => drop(text.pop()),
            KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => text.push(c),
            _ => {}
        }
        Action::None
    }

    fn results_key(&mut self, key: KeyEvent) -> Action {
        let last = self.hits().len().saturating_sub(1);
        let page = self.page.max(1);
        match key.code {
            // Back to the fields for another search.
            KeyCode::Esc => {
                self.result = None;
                self.error = None;
            }
            KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down => self.cursor = (self.cursor + 1).min(last),
            KeyCode::PageUp => self.cursor = self.cursor.saturating_sub(page),
            KeyCode::PageDown => self.cursor = (self.cursor + page).min(last),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = last,
            KeyCode::Enter => {
                if let Some(hit) = self.hits().get(self.cursor) {
                    return Action::GoTo(hit.clone());
                }
            }
            KeyCode::F(3) | KeyCode::Char('3') => {
                if let Some(hit) = self.hits().get(self.cursor).filter(|h| !h.is_dir) {
                    return Action::View(hit.clone());
                }
            }
            _ => {}
        }
        Action::None
    }

    /// A hit's path as shown: relative to the folder searched.
    pub fn relative<'a>(&self, path: &'a str) -> &'a str {
        let Location::Local { path: root } = &self.root;
        let root = root.trim_end_matches('/');
        path.strip_prefix(root)
            .and_then(|p| p.strip_prefix('/'))
            .unwrap_or(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn wait(f: &mut Find) {
        let start = Instant::now();
        while f.search.is_some() {
            assert!(start.elapsed() < Duration::from_secs(10), "search did not finish");
            f.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn edit_search_browse_and_go_back() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/a.rs"), "x\nneedle\n").unwrap();
        std::fs::write(tmp.path().join("src/b.rs"), "nothing").unwrap();
        let mut f = Find::new(Location::local(tmp.path().to_string_lossy()), false);
        for c in "*.rs".chars() {
            f.handle_key(key(KeyCode::Char(c)));
        }
        f.handle_key(key(KeyCode::Tab));
        for c in "needle".chars() {
            f.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!((f.name.as_str(), f.contains.as_str()), ("*.rs", "needle"));
        assert_eq!(f.handle_key(key(KeyCode::Enter)), Action::Start);
        f.start();
        wait(&mut f);
        let hits = f.hits().to_vec();
        assert_eq!(hits.len(), 1);
        assert_eq!(f.relative(&hits[0].path), "src/a.rs");
        assert_eq!(hits[0].line, Some(2));
        assert_eq!(f.handle_key(key(KeyCode::F(3))), Action::View(hits[0].clone()));
        assert_eq!(f.handle_key(key(KeyCode::Enter)), Action::GoTo(hits[0].clone()));
        // Esc goes back to the fields, a second Esc closes.
        assert_eq!(f.handle_key(key(KeyCode::Esc)), Action::None);
        assert!(f.result.is_none() && f.name == "*.rs");
        assert_eq!(f.handle_key(key(KeyCode::Esc)), Action::Close);
    }

    #[test]
    fn errors_are_shown_and_dropping_stops_the_worker() {
        let mut f = Find::new(Location::local("/definitely/not/here"), false);
        f.start();
        wait(&mut f);
        assert!(f.error.as_deref().unwrap().contains("not a folder"));
        f.start();
        let cancel = Arc::clone(&f.search.as_ref().unwrap().cancel);
        drop(f);
        assert!(cancel.load(Ordering::Relaxed));
    }
}
