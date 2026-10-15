//! Terminal UI state and key handling. Drawing lives in `ui.rs`; this module has no
//! terminal I/O so it can be unit-tested with plain key events.

use super::views::{DiffView, Viewer};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rc_core::api::{FileOp, OpResult};
use rc_core::panel::SortKey;
use rc_core::{Location, Panel, Row, dircmp};
use rc_vfs::Vfs;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};

pub const LEFT: usize = 0;
pub const RIGHT: usize = 1;

pub enum Mode {
    Normal,
    /// Typing into the active panel's quick filter.
    Filter,
    Prompt(Prompt),
    Confirm(Confirm),
    Viewer(Viewer),
    Diff(DiffView),
    Help,
}

pub struct Prompt {
    pub title: String,
    pub input: String,
    pub action: PromptAction,
}

pub enum PromptAction {
    Copy(Vec<Location>),
    Move(Vec<Location>),
    Mkdir,
    GoTo,
    SelectMask { select: bool },
}

pub struct Confirm {
    pub text: String,
    pub op: FileOp,
}

/// What the event loop must do outside the app (things that need the real terminal).
#[derive(Debug, PartialEq)]
pub enum Effect {
    None,
    Quit,
    /// Suspend the TUI and run `$EDITOR` on this path.
    Edit(String),
}

/// A file operation running on a worker thread.
pub struct Job {
    pub verb: &'static str,
    pub current: String,
    rx: Receiver<JobMsg>,
}

enum JobMsg {
    Progress(String),
    Done(OpResult),
}

pub struct App {
    pub vfs: Arc<Vfs>,
    pub panels: [Panel; 2],
    /// First visible row per panel; adjusted by the renderer to keep the cursor visible.
    pub scroll: [usize; 2],
    pub active: usize,
    pub mode: Mode,
    pub message: Option<(String, bool)>,
    pub job: Option<Job>,
    /// Rows that fit in a panel, updated by the renderer; used for PageUp/PageDown.
    pub page: usize,
}

impl App {
    pub fn new(vfs: Arc<Vfs>, left: Location, right: Location) -> Self {
        let left = vfs.normalize(left, None);
        let right = vfs.normalize(right, None);
        let mut app = App {
            vfs,
            panels: [Panel::new(left.clone()), Panel::new(right.clone())],
            scroll: [0, 0],
            active: LEFT,
            mode: Mode::Normal,
            message: None,
            job: None,
            page: 20,
        };
        app.load(LEFT, left, None);
        app.load(RIGHT, right, None);
        app
    }

    pub fn active_panel(&self) -> &Panel {
        &self.panels[self.active]
    }

    fn active_mut(&mut self) -> &mut Panel {
        &mut self.panels[self.active]
    }

    fn other(&self) -> usize {
        1 - self.active
    }

    fn info(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), false));
    }

    fn error(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), true));
    }

    /// List `loc` into panel `side`. On failure the panel stays where it was.
    fn load(&mut self, side: usize, loc: Location, focus: Option<&str>) -> bool {
        match self.vfs.list(&loc) {
            Ok(listing) => {
                if listing.location != self.panels[side].location {
                    self.scroll[side] = 0;
                }
                self.panels[side].set_listing(listing, focus);
                true
            }
            Err(e) => {
                self.error(format!("{e:#}"));
                false
            }
        }
    }

    pub fn refresh(&mut self, side: usize) {
        let loc = self.panels[side].location.clone();
        self.load(side, loc, None);
    }

    fn refresh_all(&mut self) {
        self.refresh(LEFT);
        self.refresh(RIGHT);
    }

    fn go_up(&mut self) {
        let p = self.active_panel();
        if let Some(parent) = p.location.parent() {
            let came_from = p.location.file_name().map(str::to_string);
            self.load(self.active, parent, came_from.as_deref());
        }
    }

    /// Enter: go up on "..", into directories, view files.
    fn activate(&mut self) {
        let p = self.active_panel();
        match p.current_row() {
            Some(Row::Parent) => self.go_up(),
            Some(Row::Entry(_)) => {
                let e = p.current().expect("entry row");
                if e.is_dir {
                    let loc = p.location.join(&e.name);
                    self.load(self.active, loc, None);
                } else {
                    self.view();
                }
            }
            None => {}
        }
    }

    fn view(&mut self) {
        let p = self.active_panel();
        let Some(e) = p.current() else { return };
        let loc = p.location.join(&e.name);
        if e.is_dir {
            self.load(self.active, loc, None);
            return;
        }
        match self.vfs.read_content(&loc) {
            Ok(content) => self.mode = Mode::Viewer(Viewer::new(content)),
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    fn diff(&mut self) {
        let Some((left, right)) = rc_core::diff_targets(&self.panels[LEFT], &self.panels[RIGHT], self.active == LEFT)
        else {
            self.error("Diff: put the cursor on a file (or select two files)");
            return;
        };
        match self.vfs.diff(Some(&left), Some(&right)) {
            Ok(resp) => self.mode = Mode::Diff(DiffView::new(resp)),
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    fn compare_dirs(&mut self) {
        if !self.panels[LEFT].marks.is_empty() || !self.panels[RIGHT].marks.is_empty() {
            self.panels.iter_mut().for_each(|p| p.marks.clear());
            self.info("Directory compare cleared");
            return;
        }
        let (l, r) = dircmp::compare(self.panels[LEFT].entries(), self.panels[RIGHT].entries());
        let differing = |m: &std::collections::HashMap<String, dircmp::CmpMark>| {
            m.values().filter(|&&v| v != dircmp::CmpMark::Same).count()
        };
        self.info(format!(
            "Compared: {} differ on the left, {} on the right",
            differing(&l),
            differing(&r)
        ));
        self.panels[LEFT].marks = l;
        self.panels[RIGHT].marks = r;
    }

    fn target_locations(&self) -> Vec<Location> {
        let p = self.active_panel();
        p.targets().iter().map(|e| p.location.join(&e.name)).collect()
    }

    fn describe(targets: &[Location]) -> String {
        match targets {
            [one] => format!("\"{}\"", one.file_name().unwrap_or("?")),
            many => format!("{} items", many.len()),
        }
    }

    fn prompt(&mut self, title: String, input: String, action: PromptAction) {
        self.mode = Mode::Prompt(Prompt { title, input, action });
    }

    fn start_copy_move(&mut self, is_move: bool) {
        let targets = self.target_locations();
        if targets.is_empty() {
            return;
        }
        let verb = if is_move { "Move/rename" } else { "Copy" };
        let title = format!("{verb} {} to:", Self::describe(&targets));
        let dest = self.panels[self.other()].location.to_string();
        let action = if is_move {
            PromptAction::Move(targets)
        } else {
            PromptAction::Copy(targets)
        };
        self.prompt(title, dest, action);
    }

    fn start_goto(&mut self) {
        let current = self.active_panel().location.to_string();
        self.prompt("Go to:".into(), current, PromptAction::GoTo);
    }

    fn start_delete(&mut self, permanent: bool) {
        let targets = self.target_locations();
        if targets.is_empty() {
            return;
        }
        let text = if permanent {
            format!(
                "PERMANENTLY delete {}? This cannot be undone.",
                Self::describe(&targets)
            )
        } else {
            format!("Move {} to the Trash?", Self::describe(&targets))
        };
        self.mode = Mode::Confirm(Confirm {
            text,
            op: FileOp::Delete { targets, permanent },
        });
    }

    fn submit_prompt(&mut self, prompt: Prompt) {
        let base = self.active_panel().location.clone();
        let input = prompt.input.trim();
        match prompt.action {
            PromptAction::SelectMask { select } => {
                let n = self.active_mut().select_mask(input, select);
                self.info(format!(
                    "{} {n} entries",
                    if select { "Selected" } else { "Deselected" }
                ));
            }
            PromptAction::GoTo => {
                if let Some(loc) = Location::resolve_input(input, &base) {
                    let loc = self.vfs.normalize(loc, Some(&base));
                    self.load(self.active, loc, None);
                }
            }
            PromptAction::Mkdir => {
                if let Some(loc) = Location::resolve_input(input, &base) {
                    let name = input.split('/').next().unwrap_or(input).to_string();
                    let loc = self.vfs.normalize(loc, Some(&base));
                    let res = self.vfs.op(&FileOp::Mkdir { location: loc }, &mut |_| {});
                    self.finish_op("Create directory", res);
                    self.active_mut().focus_name(&name);
                }
            }
            PromptAction::Copy(sources) | PromptAction::Move(sources) if input.is_empty() => drop(sources),
            PromptAction::Copy(sources) => self.run_job(FileOp::Copy {
                sources,
                dest: self.dest(input, &base),
                overwrite: false,
            }),
            PromptAction::Move(sources) => self.run_job(FileOp::Move {
                sources,
                dest: self.dest(input, &base),
                overwrite: false,
            }),
        }
    }

    fn dest(&self, input: &str, base: &Location) -> Location {
        let loc = Location::resolve_input(input, base).unwrap_or_else(|| base.clone());
        self.vfs.normalize(loc, Some(base))
    }

    /// Run a (possibly long) file operation on a worker thread; `tick` collects the result.
    fn run_job(&mut self, op: FileOp) {
        if self.job.is_some() {
            self.error("Another operation is still running");
            return;
        }
        let (tx, rx) = mpsc::channel();
        let vfs = Arc::clone(&self.vfs);
        let verb = op.verb();
        std::thread::spawn(move || {
            let progress_tx = tx.clone();
            let res = vfs.op(&op, &mut |path| {
                let _ = progress_tx.send(JobMsg::Progress(path.to_string()));
            });
            let _ = tx.send(JobMsg::Done(res));
        });
        self.job = Some(Job {
            verb,
            current: String::new(),
            rx,
        });
    }

    /// Poll the running job. Returns true if something changed and a redraw is due.
    pub fn tick(&mut self) -> bool {
        let Some(job) = &mut self.job else { return false };
        let mut changed = false;
        let mut done = None;
        while let Ok(msg) = job.rx.try_recv() {
            changed = true;
            match msg {
                JobMsg::Progress(p) => job.current = p,
                JobMsg::Done(res) => done = Some(res),
            }
        }
        if let Some(res) = done {
            let verb = job.verb;
            self.job = None;
            self.finish_op(verb, res);
        }
        changed
    }

    fn finish_op(&mut self, verb: &str, res: OpResult) {
        let summary = res.summary(verb);
        if res.errors.is_empty() {
            self.info(summary);
        } else {
            self.error(summary);
        }
        for p in &mut self.panels {
            p.clear_selection();
        }
        self.refresh_all();
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Effect {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Effect::Quit;
        }
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => return self.normal_key(key),
            Mode::Help => {}
            Mode::Filter => self.filter_key(key),
            Mode::Prompt(p) => self.prompt_key(p, key),
            Mode::Confirm(c) => match key.code {
                KeyCode::Enter | KeyCode::Char('y' | 'Y') => self.run_job(c.op),
                KeyCode::Esc | KeyCode::Char('n' | 'N') => {}
                _ => self.mode = Mode::Confirm(c),
            },
            Mode::Viewer(mut v) => {
                if !v.handle_key(key) {
                    self.mode = Mode::Viewer(v);
                }
            }
            Mode::Diff(mut d) => {
                if !d.handle_key(key) {
                    self.mode = Mode::Diff(d);
                }
            }
        }
        Effect::None
    }

    fn filter_key(&mut self, key: KeyEvent) {
        let mut filter = self.active_panel().filter.clone();
        match key.code {
            KeyCode::Esc => filter.clear(),
            KeyCode::Enter => return,
            KeyCode::Backspace => {
                filter.pop();
                self.mode = Mode::Filter;
            }
            KeyCode::Char(c) => {
                filter.push(c);
                self.mode = Mode::Filter;
            }
            // Let the cursor move while filtering.
            KeyCode::Up | KeyCode::Down => {
                self.active_mut()
                    .move_cursor(if key.code == KeyCode::Up { -1 } else { 1 });
                self.mode = Mode::Filter;
                return;
            }
            _ => self.mode = Mode::Filter,
        }
        self.active_mut().set_filter(&filter);
    }

    fn prompt_key(&mut self, mut p: Prompt, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {}
            KeyCode::Enter => self.submit_prompt(p),
            KeyCode::Backspace => {
                p.input.pop();
                self.mode = Mode::Prompt(p);
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                p.input.clear();
                self.mode = Mode::Prompt(p);
            }
            KeyCode::Char(c) => {
                p.input.push(c);
                self.mode = Mode::Prompt(p);
            }
            _ => self.mode = Mode::Prompt(p),
        }
    }

    fn normal_key(&mut self, key: KeyEvent) -> Effect {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let page = self.page.max(1) as isize;
        // Digits stand in for function keys: Mac laptop keyboards need fn for F-keys.
        let code = match key.code {
            KeyCode::Char(c @ '1'..='9') if !ctrl => KeyCode::F(c as u8 - b'0'),
            other => other,
        };
        match code {
            KeyCode::Char('q') | KeyCode::F(10) => return Effect::Quit,
            KeyCode::Up => self.active_mut().move_cursor(-1),
            KeyCode::Down => self.active_mut().move_cursor(1),
            KeyCode::PageUp => self.active_mut().move_cursor(-page),
            KeyCode::PageDown => self.active_mut().move_cursor(page),
            KeyCode::Home => self.active_mut().cursor_home(),
            KeyCode::End => self.active_mut().cursor_end(),
            KeyCode::Enter => self.activate(),
            KeyCode::Backspace | KeyCode::Left if !ctrl => self.go_up(),
            KeyCode::Right => {
                if self.active_panel().current().is_some_and(|e| e.is_dir) {
                    self.activate();
                }
            }
            KeyCode::Tab | KeyCode::BackTab => self.active = self.other(),
            KeyCode::Char(' ') | KeyCode::Insert => self.active_mut().toggle_current(),
            KeyCode::Char('+') => self.prompt(
                "Select files matching:".into(),
                "*".into(),
                PromptAction::SelectMask { select: true },
            ),
            KeyCode::Char('-') => self.prompt(
                "Deselect files matching:".into(),
                "*".into(),
                PromptAction::SelectMask { select: false },
            ),
            KeyCode::Char('*') => self.active_mut().invert_selection(),
            KeyCode::Esc => {
                let p = self.active_mut();
                p.clear_selection();
                p.set_filter("");
            }
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Char('f') if ctrl => self.mode = Mode::Filter,
            KeyCode::Char('.') => self.active_mut().toggle_hidden(),
            KeyCode::Char('r') if ctrl => self.refresh_all(),
            KeyCode::Char('u') if ctrl => {
                self.panels.swap(LEFT, RIGHT);
                self.scroll.swap(LEFT, RIGHT);
            }
            KeyCode::Char('s') if ctrl => self.active_mut().cycle_sort(),
            KeyCode::F(n @ 3..=6) if ctrl => {
                let key = [SortKey::Name, SortKey::Ext, SortKey::Modified, SortKey::Size][n as usize - 3];
                self.active_mut().set_sort(key);
            }
            KeyCode::Char('g') => self.start_goto(),
            KeyCode::Char('l') if ctrl => self.start_goto(),
            KeyCode::F(1) | KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::F(2) => self.diff(),
            KeyCode::F(3) => self.view(),
            KeyCode::F(4) => {
                let p = self.active_panel();
                if let Some(e) = p.current().filter(|e| !e.is_dir) {
                    let Location::Local { path } = p.location.join(&e.name);
                    return Effect::Edit(path);
                }
            }
            KeyCode::F(5) => self.start_copy_move(false),
            KeyCode::F(6) => self.start_copy_move(true),
            KeyCode::F(7) => self.prompt("New directory:".into(), String::new(), PromptAction::Mkdir),
            KeyCode::F(8) | KeyCode::Delete => self.start_delete(shift),
            KeyCode::F(9) => self.compare_dirs(),
            _ => {}
        }
        Effect::None
    }

    /// Status line for the job in progress, if any.
    pub fn job_status(&self) -> Option<String> {
        self.job.as_ref().map(|j| format!("{}: {}", j.verb, j.current))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, Instant};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn typed(app: &mut App, s: &str) {
        for c in s.chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
    }

    fn wait_job(app: &mut App) {
        let start = Instant::now();
        while app.job.is_some() {
            app.tick();
            assert!(start.elapsed() < Duration::from_secs(10), "job timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn setup() -> (tempfile::TempDir, App) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("left/sub")).unwrap();
        fs::create_dir_all(root.join("right")).unwrap();
        fs::write(root.join("left/a.txt"), "one\ntwo\n").unwrap();
        fs::write(root.join("left/b.txt"), "b\n").unwrap();
        fs::write(root.join("right/a.txt"), "one\nTWO\n").unwrap();
        let loc = |d: &str| Location::local(root.join(d).to_string_lossy());
        let app = App::new(Arc::new(Vfs::new()), loc("left"), loc("right"));
        (tmp, app)
    }

    #[test]
    fn navigate_into_and_out_of_directories() {
        let (_tmp, mut app) = setup();
        app.panels[LEFT].focus_name("sub");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.panels[LEFT].location.to_string().ends_with("/left/sub"));
        app.handle_key(key(KeyCode::Backspace));
        assert!(app.panels[LEFT].location.to_string().ends_with("/left"));
        assert_eq!(
            app.panels[LEFT].current().unwrap().name,
            "sub",
            "cursor returns to the dir we left"
        );
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.active, RIGHT);
    }

    #[test]
    fn copy_via_prompt_runs_job_and_refreshes() {
        let (tmp, mut app) = setup();
        app.panels[LEFT].focus_name("b.txt");
        app.handle_key(key(KeyCode::F(5)));
        let Mode::Prompt(p) = &app.mode else {
            panic!("expected prompt")
        };
        assert_eq!(p.input, app.panels[RIGHT].location.to_string());
        app.handle_key(key(KeyCode::Enter));
        wait_job(&mut app);
        assert!(tmp.path().join("right/b.txt").exists());
        assert!(app.panels[RIGHT].entries().iter().any(|e| e.name == "b.txt"));
        assert!(!app.message.as_ref().unwrap().1, "{:?}", app.message);
    }

    #[test]
    fn mkdir_and_delete_permanently() {
        let (tmp, mut app) = setup();
        app.handle_key(key(KeyCode::F(7)));
        typed(&mut app, "newdir");
        app.handle_key(key(KeyCode::Enter));
        assert!(tmp.path().join("left/newdir").is_dir());
        assert_eq!(app.panels[LEFT].current().unwrap().name, "newdir");

        app.handle_key(KeyEvent::new(KeyCode::F(8), KeyModifiers::SHIFT));
        assert!(matches!(app.mode, Mode::Confirm(_)));
        app.handle_key(key(KeyCode::Char('y')));
        wait_job(&mut app);
        assert!(!tmp.path().join("left/newdir").exists());
    }

    #[test]
    fn digits_act_as_function_keys_and_diff_opens() {
        let (_tmp, mut app) = setup();
        app.panels[LEFT].focus_name("a.txt");
        app.panels[RIGHT].focus_name("a.txt");
        app.handle_key(key(KeyCode::Char('2')));
        let Mode::Diff(d) = &app.mode else {
            panic!("expected diff view")
        };
        assert_eq!(d.response.result.as_ref().unwrap().hunks.len(), 1);
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn filter_mask_selection_and_compare() {
        let (_tmp, mut app) = setup();
        app.handle_key(key(KeyCode::Char('/')));
        typed(&mut app, "b.");
        assert_eq!(app.panels[LEFT].len(), 2, "'..' plus b.txt");
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.panels[LEFT].filter, "");

        app.handle_key(key(KeyCode::Char('+')));
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        typed(&mut app, "*.txt");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.panels[LEFT].selected.len(), 2);

        app.handle_key(key(KeyCode::F(9)));
        assert_eq!(app.panels[LEFT].marks.get("b.txt"), Some(&dircmp::CmpMark::Orphan));
        app.handle_key(key(KeyCode::F(9)));
        assert!(app.panels[LEFT].marks.is_empty());
    }

    #[test]
    fn goto_reports_errors_and_stays() {
        let (_tmp, mut app) = setup();
        let before = app.panels[LEFT].location.clone();
        app.handle_key(key(KeyCode::Char('g')));
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        typed(&mut app, "/definitely/not/here");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.panels[LEFT].location, before);
        assert!(app.message.as_ref().unwrap().1);
    }
}
