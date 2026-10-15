//! Terminal UI state and key handling. Drawing lives in `ui.rs`; this module has no
//! terminal I/O so it can be unit-tested with plain key events.

use super::views::{DiffView, Viewer};
use crate::control::Request;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rc_core::api::{FileOp, OpResult};
use rc_core::api::{OnConflict, Progress};
use rc_core::control::{Command, Reply};
use rc_core::format::human_size;
use rc_core::history::History;
use rc_core::panel::SortKey;
use rc_core::{Column, Location, Panel, Row, Settings, dircmp};
use rc_vfs::Vfs;
use rc_vfs::watch::DirWatcher;
use std::collections::{BTreeSet, HashMap};
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

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
    Options(Options),
    /// The hotlist (bookmarks); the number is the cursor row.
    Hotlist(usize),
    Conflict(ConflictPrompt),
}

/// The options dialog: one row per optional column, then "show hidden", then the editor.
pub struct Options {
    pub cursor: usize,
    /// Text being typed into the editor field, if it is being edited.
    pub editing: Option<String>,
    /// Settings when the dialog opened, restored on Esc.
    original: Settings,
}

pub const HIDDEN_ROW: usize = Column::ALL.len();
pub const EDITOR_ROW: usize = Column::ALL.len() + 1;

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
    Zoxide,
    SelectMask { select: bool },
}

pub struct Confirm {
    pub text: String,
    pub action: ConfirmAction,
    /// Whether Enter means yes. Off for irreversible actions, which need an explicit `y`.
    pub enter_confirms: bool,
}

pub enum ConfirmAction {
    Run(FileOp),
    /// Quit even though a job is running (it is abandoned half-way).
    Quit,
}

/// What the event loop must do outside the app (things that need the real terminal).
#[derive(Debug, PartialEq)]
pub enum Effect {
    None,
    Quit,
    /// Suspend the TUI and edit `path` with `editor` (empty: `$VISUAL`/`$EDITOR`).
    Edit {
        path: String,
        editor: String,
    },
}

/// A file operation running on a worker thread.
pub struct Job {
    pub verb: &'static str,
    pub progress: Option<Progress>,
    rx: Receiver<JobMsg>,
    /// Set to stop the worker at its next progress report.
    cancel: Arc<AtomicBool>,
}

enum JobMsg {
    Progress(Progress),
    Done(OpResult),
}

/// A copy/move that would replace existing files, waiting for the user's choice.
pub struct ConflictPrompt {
    pub op: FileOp,
    pub existing: Vec<String>,
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
    pub settings: Settings,
    /// Where settings are saved; `None` keeps them in memory (tests).
    settings_path: Option<PathBuf>,
    /// Remote-control requests from `rcmd ctl` (see `control.rs`).
    pub control: Option<Receiver<Request>>,
    /// Live refresh: the watcher, the directories it reports, and what it watches now
    /// (canonical paths, which is what the OS reports).
    watcher: Option<DirWatcher>,
    changes: Option<Receiver<PathBuf>>,
    watching: [Option<PathBuf>; 2],
    watched_locations: [Location; 2],
    /// Entries that changed in a live refresh, highlighted briefly.
    pub flash: [HashMap<String, Instant>; 2],
    history: [History; 2],
}

/// How long a changed row stays highlighted after a live refresh.
pub const FLASH: Duration = Duration::from_millis(1500);

impl App {
    #[cfg(test)]
    pub fn new(vfs: Arc<Vfs>, left: Location, right: Location) -> Self {
        Self::with_settings(vfs, left, right, Settings::default(), None)
    }

    pub fn with_settings(
        vfs: Arc<Vfs>,
        left: Location,
        right: Location,
        settings: Settings,
        settings_path: Option<PathBuf>,
    ) -> Self {
        let left = vfs.normalize(left, None);
        let right = vfs.normalize(right, None);
        let mut panels = [Panel::new(left.clone()), Panel::new(right.clone())];
        for p in &mut panels {
            p.show_hidden = settings.show_hidden;
        }
        let mut app = App {
            vfs,
            panels,
            scroll: [0, 0],
            active: LEFT,
            mode: Mode::Normal,
            message: None,
            job: None,
            page: 20,
            settings,
            settings_path,
            control: None,
            watcher: None,
            changes: None,
            watching: [None, None],
            watched_locations: [Location::local(""), Location::local("")],
            flash: [HashMap::new(), HashMap::new()],
            history: [History::default(), History::default()],
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
        self.show(side, loc, focus, true)
    }

    /// List `loc` into a panel; `record` adds the place we leave to the back history.
    fn show(&mut self, side: usize, loc: Location, focus: Option<&str>, record: bool) -> bool {
        let listed = if self.settings.shows(Column::Git) {
            self.vfs.list_with_git(&loc)
        } else {
            self.vfs.list(&loc)
        };
        match listed {
            Ok(listing) => {
                if listing.location != self.panels[side].location {
                    self.scroll[side] = 0;
                    // Highlights belong to the directory they were made in.
                    self.flash[side].clear();
                    // Only places the user chose count (like a shell's cd hook), not
                    // back/forward steps.
                    if record {
                        if !self.panels[side].location.to_string().is_empty() {
                            self.history[side].visit(self.panels[side].location.clone());
                        }
                        let Location::Local { path } = &listing.location;
                        zoxide_add(path);
                    }
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

    /// Alt+← / Alt+→: step through the panel's history.
    fn history_step(&mut self, forward: bool) {
        let side = self.active;
        let current = self.panels[side].location.clone();
        let target = if forward {
            self.history[side].forward(current)
        } else {
            self.history[side].back(current)
        };
        let Some(mut target) = target else { return };
        // Put the cursor back where we were when coming back out of a subdirectory.
        let came_from = self.panels[side].location.clone();
        loop {
            let focus = (came_from.parent().as_ref() == Some(&target))
                .then(|| came_from.file_name().map(str::to_string))
                .flatten();
            if self.show(side, target, focus.as_deref(), false) {
                return;
            }
            // That place is gone: forget it and try the next one.
            let next = if forward {
                self.history[side].discard_forward();
                self.history[side].forward(came_from.clone())
            } else {
                self.history[side].discard_back();
                self.history[side].back(came_from.clone())
            };
            match next {
                Some(t) => target = t,
                None => return,
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

    /// F2: diff the two files `diff_targets` picks. When the other panel has no such file,
    /// fall back to the file's committed version, as Shift+F2 does explicitly.
    fn diff(&mut self) {
        let Some((left, right)) = rc_core::diff_targets(&self.panels[LEFT], &self.panels[RIGHT], self.active == LEFT)
        else {
            self.error("Diff: put the cursor on a file (or select two files)");
            return;
        };
        let exists = |l: &Location| {
            let Location::Local { path } = l;
            std::path::Path::new(path).exists()
        };
        if !exists(&left) || !exists(&right) {
            // No counterpart in the other panel: compare with git, explaining both if that fails.
            let p = self.active_panel();
            let Some(e) = p.current().filter(|e| !e.is_dir) else {
                return;
            };
            match self.vfs.diff_head(&p.location.join(&e.name)) {
                Ok(resp) => self.mode = Mode::Diff(DiffView::new(resp)),
                Err(err) => self.error(format!("No {} in the other panel to compare with ({err:#})", e.name)),
            }
            return;
        }
        match self.vfs.diff(Some(&left), Some(&right)) {
            Ok(resp) => self.mode = Mode::Diff(DiffView::new(resp)),
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    /// Shift+F2: diff the file under the cursor against git HEAD.
    fn diff_head(&mut self) {
        let p = self.active_panel();
        let Some(e) = p.current().filter(|e| !e.is_dir) else {
            self.error("Diff against HEAD: put the cursor on a file");
            return;
        };
        match self.vfs.diff_head(&p.location.join(&e.name)) {
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
        let title = format!("{verb} {} to", Self::describe(&targets));
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
        self.prompt("Go to".into(), current, PromptAction::GoTo);
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
            action: ConfirmAction::Run(FileOp::Delete { targets, permanent }),
            enter_confirms: !permanent,
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
            PromptAction::Zoxide => match zoxide_query(input, &base.to_string()) {
                Ok(path) => {
                    self.load(self.active, Location::local(path), None);
                }
                Err(e) => self.error(e),
            },
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
                    let res = self.vfs.op(&FileOp::Mkdir { location: loc }, &mut |_| {
                        std::ops::ControlFlow::Continue(())
                    });
                    self.report(&res.summary("Create directory"), res.errors.is_empty());
                    self.refresh_all();
                    self.active_mut().focus_name(&name);
                }
            }
            PromptAction::Copy(sources) | PromptAction::Move(sources) if input.is_empty() => drop(sources),
            PromptAction::Copy(sources) => self.start_transfer(FileOp::Copy {
                sources,
                dest: self.dest(input, &base),
                on_conflict: OnConflict::Skip,
            }),
            PromptAction::Move(sources) => self.start_transfer(FileOp::Move {
                sources,
                dest: self.dest(input, &base),
                on_conflict: OnConflict::Skip,
            }),
        }
    }

    fn dest(&self, input: &str, base: &Location) -> Location {
        let loc = Location::resolve_input(input, base).unwrap_or_else(|| base.clone());
        self.vfs.normalize(loc, Some(base))
    }

    /// Run a (possibly long) file operation on a worker thread; `tick` collects the result.
    /// Run a copy/move, first asking what to do if it would meet existing files.
    fn start_transfer(&mut self, op: FileOp) {
        if self.job.is_some() {
            self.error("Another operation is still running");
            return;
        }
        let existing = self.vfs.conflicts(&op).existing;
        if existing.is_empty() {
            self.run_job(op);
        } else {
            self.mode = Mode::Conflict(ConflictPrompt { op, existing });
        }
    }

    fn conflict_key(&mut self, c: ConflictPrompt, key: KeyEvent) {
        // 'n' is "no" everywhere else, so it cancels here too; "only newer" is 'u' (update).
        let policy = match key.code {
            KeyCode::Char('o' | 'O') => OnConflict::Overwrite,
            KeyCode::Char('u' | 'U') => OnConflict::IfNewer,
            KeyCode::Char('s' | 'S') => OnConflict::Skip,
            KeyCode::Esc | KeyCode::Char('n' | 'N') => return,
            _ => {
                self.mode = Mode::Conflict(c);
                return;
            }
        };
        let op = match c.op {
            FileOp::Copy { sources, dest, .. } => FileOp::Copy {
                sources,
                dest,
                on_conflict: policy,
            },
            FileOp::Move { sources, dest, .. } => FileOp::Move {
                sources,
                dest,
                on_conflict: policy,
            },
            other => other,
        };
        self.run_job(op);
    }

    fn run_job(&mut self, op: FileOp) {
        if self.job.is_some() {
            self.error("Another operation is still running");
            return;
        }
        let (tx, rx) = mpsc::channel();
        let vfs = Arc::clone(&self.vfs);
        let verb = op.verb();
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&cancel);
        std::thread::spawn(move || {
            let progress_tx = tx.clone();
            let mut last: Option<Instant> = None;
            let res = vfs.op(&op, &mut |p| {
                if stop.load(Ordering::Relaxed) {
                    return ControlFlow::Break(());
                }
                // Many small files would flood the UI; a few updates a second suffice.
                if last.is_none_or(|t| t.elapsed() >= Duration::from_millis(50)) {
                    last = Some(Instant::now());
                    let _ = progress_tx.send(JobMsg::Progress(p.clone()));
                }
                ControlFlow::Continue(())
            });
            let _ = tx.send(JobMsg::Done(res));
        });
        self.job = Some(Job {
            verb,
            progress: None,
            rx,
            cancel,
        });
    }

    /// Apply pending remote-control commands and poll the running job. Returns true if
    /// something changed and a redraw is due.
    pub fn tick(&mut self) -> bool {
        let mut changed = false;
        while let Some(req) = self.control.as_ref().and_then(|rx| rx.try_recv().ok()) {
            let reply = self.apply(req.command);
            let _ = req.reply.send(reply);
            changed = true;
        }
        changed |= self.tick_watch();
        self.tick_job() || changed
    }

    /// Start live refresh: panels re-read their directory when it changes on disk.
    pub fn enable_live_refresh(&mut self) -> anyhow::Result<()> {
        let (tx, rx) = mpsc::channel();
        self.watcher = Some(DirWatcher::new(move |dir| {
            let _ = tx.send(dir);
        })?);
        self.changes = Some(rx);
        Ok(())
    }

    /// Keep the watcher on the panels' directories and refresh panels that changed.
    fn tick_watch(&mut self) -> bool {
        let Some(watcher) = &mut self.watcher else { return false };
        // Resolve symlinks only when a panel moved, not on every tick (a stalled network
        // mount would block the UI).
        let locations = self.panels.each_ref().map(|p| p.location.clone());
        if locations != self.watched_locations {
            let dirs = locations.each_ref().map(|l| {
                let Location::Local { path } = l;
                std::fs::canonicalize(path).ok()
            });
            watcher.set(dirs.iter().flatten().map(PathBuf::as_path));
            self.watching = dirs;
            self.watched_locations = locations;
        }
        let changed: BTreeSet<PathBuf> = self
            .changes
            .as_ref()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default();
        // The directory itself may have been deleted and recreated, which drops its watch.
        for dir in self.watching.iter().flatten().filter(|d| changed.contains(*d)) {
            watcher.rewatch(dir);
        }
        let mut redraw = false;
        for side in [LEFT, RIGHT] {
            if self.watching[side].as_ref().is_some_and(|d| changed.contains(d)) {
                self.live_refresh(side);
                redraw = true;
            }
        }
        // Redraw while highlights are fading out.
        let now = Instant::now();
        for flash in &mut self.flash {
            let before = flash.len();
            flash.retain(|_, t| now.duration_since(*t) < FLASH);
            redraw |= flash.len() != before;
        }
        redraw
    }

    /// Refresh one panel and remember which entries are new or changed.
    fn live_refresh(&mut self, side: usize) {
        let key = |e: &rc_core::Entry| (e.size, e.modified, e.is_dir);
        let before: HashMap<String, _> = self.panels[side]
            .entries()
            .iter()
            .map(|e| (e.name.clone(), key(e)))
            .collect();
        self.refresh(side);
        let now = Instant::now();
        for e in self.panels[side].entries() {
            if before.get(&e.name) != Some(&key(e)) {
                self.flash[side].insert(e.name.clone(), now);
            }
        }
    }

    /// Carry out a remote-control command.
    pub fn apply(&mut self, command: Command) -> Reply {
        // Never act behind a dialog the user is answering; harmless commands still go through.
        let busy = matches!(
            self.mode,
            Mode::Prompt(_) | Mode::Confirm(_) | Mode::Filter | Mode::Options(_)
        );
        if busy && !matches!(command, Command::Message { .. } | Command::Refresh) {
            return Reply::error("rcmd is busy: a dialog is open");
        }
        // Views that cover the panels get out of the way.
        let reveal = |mode: &mut Mode| {
            if matches!(mode, Mode::Viewer(_) | Mode::Diff(_) | Mode::Help) {
                *mode = Mode::Normal;
            }
        };
        let base = self.active_panel().location.clone();
        let loc = |p: &str| self.vfs.normalize(Location::local(p), Some(&base));
        match command {
            // Shows the place in the requested panel without moving the user's focus.
            Command::Goto { path, side } => {
                let side = side.index(self.active);
                let target = loc(&path);
                let Location::Local { path: p } = &target;
                let p = std::path::Path::new(p);
                if p.symlink_metadata().is_err() {
                    return Reply::error(format!("{}: no such file or directory", p.display()));
                }
                reveal(&mut self.mode);
                let ok = if p.is_dir() {
                    self.load(side, target, None)
                } else {
                    let (Some(dir), Some(name)) = (target.parent(), target.file_name().map(str::to_string)) else {
                        return Reply::error(format!("cannot show {path}"));
                    };
                    // A dot-file can only get the cursor if hidden files are shown.
                    if name.starts_with('.') && !self.panels[side].show_hidden {
                        self.panels[side].toggle_hidden();
                    }
                    self.load(side, dir, Some(&name))
                };
                if !ok {
                    return Reply::error(self.message.clone().map_or_else(|| "failed".into(), |m| m.0));
                }
            }
            Command::Diff { left, right } => match self.vfs.diff(Some(&loc(&left)), Some(&loc(&right))) {
                Ok(resp) => self.mode = Mode::Diff(DiffView::new(resp)),
                Err(e) => return Reply::error(format!("{e:#}")),
            },
            Command::View { path } => match self.vfs.read_content(&loc(&path)) {
                Ok(content) => self.mode = Mode::Viewer(Viewer::new(content)),
                Err(e) => return Reply::error(format!("{e:#}")),
            },
            Command::Select { mask, side } => {
                reveal(&mut self.mode);
                let n = self.panels[side.index(self.active)].select_mask(&mask, true);
                self.info(format!("Selected {n} entries matching {mask}"));
            }
            Command::Refresh => self.refresh_all(),
            Command::Message { text } => self.info(text),
        }
        Reply::ok()
    }

    fn tick_job(&mut self) -> bool {
        let Some(job) = &mut self.job else { return false };
        let mut changed = false;
        loop {
            match job.rx.try_recv() {
                Ok(JobMsg::Progress(p)) => {
                    job.progress = Some(p);
                    changed = true;
                }
                Ok(JobMsg::Done(res)) => {
                    let verb = job.verb;
                    self.job = None;
                    self.finish_op(verb, res);
                    return true;
                }
                Err(TryRecvError::Empty) => return changed,
                // The worker died without reporting (it panicked).
                Err(TryRecvError::Disconnected) => {
                    let verb = job.verb;
                    self.job = None;
                    self.error(format!("{verb} aborted unexpectedly; check the destination"));
                    self.refresh_all();
                    return true;
                }
            }
        }
    }

    fn report(&mut self, msg: &str, ok: bool) {
        if ok { self.info(msg) } else { self.error(msg) }
    }

    /// Show the result and refresh. The selection is kept when something failed so the
    /// user can retry.
    fn finish_op(&mut self, verb: &str, res: OpResult) {
        // A cancelled operation keeps the selection too, so the rest can be retried.
        let ok = res.errors.is_empty() && !res.cancelled;
        self.report(&res.summary(verb), ok);
        if ok {
            self.panels.iter_mut().for_each(Panel::clear_selection);
        }
        self.refresh_all();
    }

    /// Quit, unless that would abandon a running job: then ask first.
    fn request_quit(&mut self) -> Effect {
        match &self.job {
            None => Effect::Quit,
            Some(job) => {
                self.mode = Mode::Confirm(Confirm {
                    text: format!("{} is still running. Quit anyway and leave it incomplete?", job.verb),
                    action: ConfirmAction::Quit,
                    enter_confirms: false,
                });
                Effect::None
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Effect {
        let ctrl_c = key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal if ctrl_c => return self.request_quit(),
            Mode::Normal => return self.normal_key(key),
            // Ctrl+C backs out of whatever is open.
            Mode::Options(o) if ctrl_c => self.revert_options(o),
            _ if ctrl_c => {}
            Mode::Help => {}
            Mode::Filter => return self.filter_key(key),
            Mode::Prompt(p) => self.prompt_key(p, key),
            Mode::Confirm(c) => match key.code {
                KeyCode::Char('y' | 'Y') => return self.confirm(c.action),
                KeyCode::Enter if c.enter_confirms => return self.confirm(c.action),
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
            Mode::Options(o) => self.options_key(o, key),
            Mode::Hotlist(cursor) => self.hotlist_key(cursor, key),
            Mode::Conflict(c) => self.conflict_key(c, key),
        }
        Effect::None
    }

    fn options_key(&mut self, mut o: Options, key: KeyEvent) {
        if let Some(text) = &mut o.editing {
            match key.code {
                KeyCode::Enter => {
                    self.settings.editor = text.trim().to_string();
                    o.editing = None;
                }
                KeyCode::Esc => o.editing = None,
                KeyCode::Backspace => drop(text.pop()),
                KeyCode::Char(c) if !has_command_modifier(key) => text.push(c),
                _ => {}
            }
            self.mode = Mode::Options(o);
            return;
        }
        // Same model as the browser dialog: changes preview live, Enter saves, Esc reverts.
        match key.code {
            KeyCode::Esc => return self.revert_options(o),
            KeyCode::Enter if o.cursor != EDITOR_ROW => {
                if self.settings != o.original {
                    self.save_settings();
                }
                return;
            }
            KeyCode::Up => o.cursor = o.cursor.saturating_sub(1),
            KeyCode::Down => o.cursor = (o.cursor + 1).min(EDITOR_ROW),
            KeyCode::Char(' ') | KeyCode::Enter => match o.cursor {
                EDITOR_ROW => o.editing = Some(self.settings.editor.clone()),
                HIDDEN_ROW => {
                    self.settings.show_hidden = !self.settings.show_hidden;
                    // Apply right away so the effect is visible.
                    for p in &mut self.panels {
                        if p.show_hidden != self.settings.show_hidden {
                            p.toggle_hidden();
                        }
                    }
                }
                i => self.settings.toggle_column(Column::ALL[i]),
            },
            _ => {}
        }
        self.mode = Mode::Options(o);
    }

    fn revert_options(&mut self, o: Options) {
        self.settings = o.original;
        for p in &mut self.panels {
            if p.show_hidden != self.settings.show_hidden {
                p.toggle_hidden();
            }
        }
    }

    fn hotlist_key(&mut self, mut cursor: usize, key: KeyEvent) {
        let marks = &mut self.settings.bookmarks;
        let here = self.panels[self.active].location.to_string();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return,
            KeyCode::Up => cursor = cursor.saturating_sub(1),
            KeyCode::Down => cursor = (cursor + 1).min(marks.len().saturating_sub(1)),
            KeyCode::Char('a') if !marks.contains(&here) => {
                marks.push(here);
                cursor = marks.len() - 1;
                self.save_settings();
            }
            KeyCode::Char('d') | KeyCode::Delete if cursor < marks.len() => {
                marks.remove(cursor);
                cursor = cursor.min(marks.len().saturating_sub(1));
                self.save_settings();
            }
            KeyCode::Enter | KeyCode::Char('1'..='9') => {
                let index = match key.code {
                    KeyCode::Char(c) => c as usize - '1' as usize,
                    _ => cursor,
                };
                if let Some(path) = marks.get(index).cloned() {
                    // Bookmarks may be hand-edited: allow ~ and relative paths.
                    let base = self.panels[self.active].location.clone();
                    if let Some(loc) = Location::resolve_input(&path, &base) {
                        let loc = self.vfs.normalize(loc, Some(&base));
                        self.load(self.active, loc, None);
                    }
                    return;
                }
            }
            _ => {}
        }
        self.mode = Mode::Hotlist(cursor);
    }

    fn save_settings(&mut self) {
        let Some(path) = &self.settings_path else { return };
        match rc_vfs::settings::save_to(path, &self.settings) {
            Ok(()) => self.info(format!("Options saved to {}", path.display())),
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    fn confirm(&mut self, action: ConfirmAction) -> Effect {
        match action {
            ConfirmAction::Run(op) => self.run_job(op),
            ConfirmAction::Quit => return Effect::Quit,
        }
        Effect::None
    }

    /// Quick filter: typing narrows the list. Any other key (F-keys, Enter, Tab, ...) ends
    /// filter mode, keeps the filter, and acts normally, so "type, then F5" works.
    fn filter_key(&mut self, key: KeyEvent) -> Effect {
        let mut filter = self.active_panel().filter.clone();
        match key.code {
            KeyCode::Esc => filter.clear(),
            KeyCode::Backspace => {
                filter.pop();
                self.mode = Mode::Filter;
            }
            KeyCode::Char(c) if !has_command_modifier(key) => {
                filter.push(c);
                self.mode = Mode::Filter;
            }
            KeyCode::Up | KeyCode::Down => {
                let delta = if key.code == KeyCode::Up { -1 } else { 1 };
                self.active_mut().move_cursor(delta);
                self.mode = Mode::Filter;
                return Effect::None;
            }
            _ => return self.normal_key(key),
        }
        self.active_mut().set_filter(&filter);
        Effect::None
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
            KeyCode::Char(c) if !has_command_modifier(key) => {
                p.input.push(c);
                self.mode = Mode::Prompt(p);
            }
            _ => self.mode = Mode::Prompt(p),
        }
    }

    fn normal_key(&mut self, key: KeyEvent) -> Effect {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let page = self.page.max(1) as isize;
        // Digits stand in for function keys: Mac laptop keyboards need fn for F-keys.
        let code = match key.code {
            KeyCode::Char(c @ '1'..='9') if !ctrl => KeyCode::F(c as u8 - b'0'),
            other => other,
        };
        match code {
            KeyCode::Char('q') | KeyCode::F(10) => return self.request_quit(),
            KeyCode::Up => self.active_mut().move_cursor(-1),
            KeyCode::Down => self.active_mut().move_cursor(1),
            KeyCode::PageUp => self.active_mut().move_cursor(-page),
            KeyCode::PageDown => self.active_mut().move_cursor(page),
            KeyCode::Home => self.active_mut().cursor_home(),
            KeyCode::End => self.active_mut().cursor_end(),
            KeyCode::Enter => self.activate(),
            KeyCode::Left if alt => self.history_step(false),
            KeyCode::Right if alt => self.history_step(true),
            KeyCode::Backspace | KeyCode::Left if !ctrl => self.go_up(),
            KeyCode::Right => {
                if self.active_panel().current().is_some_and(|e| e.is_dir) {
                    self.activate();
                }
            }
            KeyCode::Tab | KeyCode::BackTab => self.active = self.other(),
            KeyCode::Char(' ') | KeyCode::Insert => self.active_mut().toggle_current(),
            KeyCode::Char('+') => self.prompt(
                "Select files matching".into(),
                "*".into(),
                PromptAction::SelectMask { select: true },
            ),
            KeyCode::Char('-') => self.prompt(
                "Deselect files matching".into(),
                "*".into(),
                PromptAction::SelectMask { select: false },
            ),
            KeyCode::Char('*') => self.active_mut().invert_selection(),
            // Esc stops a running copy/move first.
            KeyCode::Esc if self.job.is_some() => {
                if let Some(job) = &self.job {
                    job.cancel.store(true, Ordering::Relaxed);
                }
                self.info("Cancelling…");
            }
            // First Esc clears the filter, the next one the selection.
            KeyCode::Esc => {
                let p = self.active_mut();
                if p.filter.is_empty() {
                    p.clear_selection();
                } else {
                    p.set_filter("");
                }
            }
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Char('f') if ctrl => self.mode = Mode::Filter,
            KeyCode::Char('.') => self.active_mut().toggle_hidden(),
            KeyCode::Char('r') if ctrl => self.refresh_all(),
            KeyCode::Char('u') if ctrl => {
                self.panels.swap(LEFT, RIGHT);
                self.scroll.swap(LEFT, RIGHT);
                self.history.swap(LEFT, RIGHT);
            }
            KeyCode::Char('s') if ctrl => self.active_mut().cycle_sort(),
            KeyCode::F(n @ 3..=6) if ctrl => {
                let key = [SortKey::Name, SortKey::Ext, SortKey::Modified, SortKey::Size][n as usize - 3];
                self.active_mut().set_sort(key);
            }
            KeyCode::Char('g') => self.start_goto(),
            KeyCode::Char('l') if ctrl => self.start_goto(),
            KeyCode::F(1) | KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('[') => self.history_step(false),
            KeyCode::Char(']') => self.history_step(true),
            KeyCode::Char('d') if ctrl => self.mode = Mode::Hotlist(0),
            KeyCode::Char('z') => self.prompt("Jump to (zoxide query):".into(), String::new(), PromptAction::Zoxide),
            KeyCode::Char('o') => {
                self.mode = Mode::Options(Options {
                    cursor: 0,
                    editing: None,
                    original: self.settings.clone(),
                })
            }
            KeyCode::F(2) if shift => self.diff_head(),
            KeyCode::F(2) => self.diff(),
            KeyCode::F(3) => self.view(),
            KeyCode::F(4) => {
                let p = self.active_panel();
                if let Some(e) = p.current().filter(|e| !e.is_dir) {
                    let Location::Local { path } = p.location.join(&e.name);
                    let editor = self.settings.editor.clone();
                    return Effect::Edit { path, editor };
                }
            }
            KeyCode::F(5) => self.start_copy_move(false),
            KeyCode::F(6) => self.start_copy_move(true),
            KeyCode::F(7) => self.prompt("New directory".into(), String::new(), PromptAction::Mkdir),
            KeyCode::F(8) | KeyCode::Delete => self.start_delete(shift),
            KeyCode::F(9) => self.compare_dirs(),
            _ => {}
        }
        Effect::None
    }

    /// Status line for the job in progress, if any.
    pub fn job_status(&self) -> Option<String> {
        let job = self.job.as_ref()?;
        if job.cancel.load(Ordering::Relaxed) {
            return Some(format!("Cancelling {}…", job.verb.to_lowercase()));
        }
        let Some(p) = &job.progress else {
            return Some(format!("{}…  (Esc cancels)", job.verb));
        };
        let name = p.path.rsplit('/').next().unwrap_or(&p.path);
        if p.total == 0 {
            return Some(format!("{}: {name}  (Esc cancels)", job.verb));
        }
        let filled = usize::from(p.percent()) / 5;
        Some(format!(
            "{} {:>3}% {}{} {} of {} · {name}  (Esc cancels)",
            job.verb,
            p.percent(),
            "█".repeat(filled),
            "░".repeat(20 - filled),
            human_size(p.done.min(p.total)),
            human_size(p.total),
        ))
    }
}

/// Best-effort: tell zoxide (if installed) that we visited `path`, like its shell hook.
fn zoxide_add(path: &str) {
    static HAS_ZOXIDE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *HAS_ZOXIDE.get_or_init(|| std::process::Command::new("zoxide").arg("--version").output().is_ok()) {
        let path = path.to_string();
        std::thread::spawn(move || std::process::Command::new("zoxide").args(["add", "--", &path]).output());
    }
}

/// Ask zoxide for the best match for `query` (space-separated keywords).
fn zoxide_query(query: &str, current: &str) -> Result<String, String> {
    let words: Vec<&str> = query.split_whitespace().collect();
    let out = std::process::Command::new("zoxide")
        // Like the shell's `z`: never "jump" to where we already are.
        .args(["query", "--exclude", current, "--"])
        .args(&words)
        .output()
        .map_err(|_| "zoxide is not installed (https://github.com/ajeetdsouza/zoxide)".to_string())?;
    if !out.status.success() {
        return Err(format!("zoxide: no match for “{query}”"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Ctrl/Alt chords are commands, never text.
fn has_command_modifier(key: KeyEvent) -> bool {
    key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_core::control::Side;
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
    fn quitting_during_a_job_asks_first() {
        let (_tmp, mut app) = setup();
        let (_tx, rx) = mpsc::channel();
        app.job = Some(Job {
            verb: "Copy",
            progress: None,
            rx,
            cancel: Arc::default(),
        });
        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Effect::None);
        assert!(matches!(app.mode, Mode::Confirm(_)));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Effect::None,
            "Enter does not confirm"
        );
        assert_eq!(app.handle_key(key(KeyCode::Char('y'))), Effect::Quit);
    }

    #[test]
    fn dead_worker_clears_the_job() {
        let (_tmp, mut app) = setup();
        let (tx, rx) = mpsc::channel::<JobMsg>();
        drop(tx);
        app.job = Some(Job {
            verb: "Copy",
            progress: None,
            rx,
            cancel: Arc::default(),
        });
        assert!(app.tick());
        assert!(app.job.is_none());
        assert!(app.message.as_ref().unwrap().1);
    }

    #[test]
    fn function_keys_work_from_the_filter() {
        let (tmp, mut app) = setup();
        app.handle_key(key(KeyCode::Char('/')));
        typed(&mut app, "b.");
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::F(8)));
        let Mode::Confirm(c) = &app.mode else {
            panic!("expected confirm")
        };
        assert!(c.text.contains("b.txt"), "{}", c.text);
        assert_eq!(app.panels[LEFT].filter, "b.", "filter is kept");
        app.handle_key(key(KeyCode::Char('n')));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(tmp.path().join("left/b.txt").exists());
    }

    #[test]
    fn digits_are_text_in_prompts_and_ctrl_chords_are_not() {
        let (tmp, mut app) = setup();
        app.handle_key(key(KeyCode::Char('7')));
        typed(&mut app, "2024");
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Enter));
        assert!(tmp.path().join("left/2024").is_dir());
    }

    #[test]
    fn selection_beats_cursor_and_parent_row_is_inert() {
        let (_tmp, mut app) = setup();
        app.panels[LEFT].cursor_home();
        app.handle_key(key(KeyCode::F(5)));
        assert!(matches!(app.mode, Mode::Normal), "no copy of '..'");
        app.panels[LEFT].select_mask("*.txt", true);
        app.panels[LEFT].focus_name("sub");
        app.handle_key(key(KeyCode::F(8)));
        let Mode::Confirm(c) = &app.mode else {
            panic!("expected confirm")
        };
        assert!(c.text.contains("2 items"), "{}", c.text);
    }

    #[test]
    fn esc_clears_filter_before_selection() {
        let (_tmp, mut app) = setup();
        app.panels[LEFT].select_mask("*", true);
        app.panels[LEFT].set_filter("a");
        app.handle_key(key(KeyCode::Esc));
        assert!(app.panels[LEFT].filter.is_empty() && !app.panels[LEFT].selected.is_empty());
        app.handle_key(key(KeyCode::Esc));
        assert!(app.panels[LEFT].selected.is_empty());
    }

    #[test]
    fn options_toggle_columns_hidden_and_editor_and_save() {
        let (tmp, mut app) = setup();
        let cfg = tmp.path().join("cfg/settings.toml");
        app.settings_path = Some(cfg.clone());
        app.handle_key(key(KeyCode::Char('o')));
        let perms = Column::ALL.iter().position(|c| *c == Column::Permissions).unwrap();
        for _ in 0..perms {
            app.handle_key(key(KeyCode::Down));
        }
        app.handle_key(key(KeyCode::Char(' ')));
        assert!(app.settings.shows(Column::Permissions));
        // Hidden files row applies immediately.
        app.panels[LEFT].show_hidden = true;
        for _ in perms..HIDDEN_ROW {
            app.handle_key(key(KeyCode::Down));
        }
        app.handle_key(key(KeyCode::Char(' ')));
        assert!(!app.settings.show_hidden && !app.panels[LEFT].show_hidden);
        // Editor row: typing '1' is text, not a function key.
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Char(' ')));
        typed(&mut app, "nano1");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.settings.editor, "nano1");
        app.handle_key(key(KeyCode::Up));
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(rc_vfs::settings::load_from(&cfg).unwrap(), app.settings);

        // Esc reverts whatever changed since the dialog opened, including hidden files.
        app.handle_key(key(KeyCode::Char('o')));
        for _ in 0..HIDDEN_ROW {
            app.handle_key(key(KeyCode::Down));
        }
        app.handle_key(key(KeyCode::Char(' ')));
        assert!(app.panels[LEFT].show_hidden);
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.settings.show_hidden && !app.panels[LEFT].show_hidden);

        app.panels[LEFT].focus_name("a.txt");
        let Effect::Edit { editor, .. } = app.handle_key(key(KeyCode::F(4))) else {
            panic!("expected edit")
        };
        assert_eq!(editor, "nano1");
    }

    #[test]
    fn remote_commands() {
        let (tmp, mut app) = setup();
        let p = |s: &str| tmp.path().join(s).to_string_lossy().into_owned();
        // A file path shows its directory with the cursor on it, in the requested panel.
        let r = app.apply(Command::Goto {
            path: p("right/a.txt"),
            side: Side::Left,
        });
        assert!(r.ok);
        assert_eq!(app.panels[LEFT].location, Location::local(p("right")));
        assert_eq!(app.panels[LEFT].current().unwrap().name, "a.txt");
        assert!(
            app.apply(Command::Diff {
                left: p("left/a.txt"),
                right: p("right/a.txt")
            })
            .ok
        );
        assert!(matches!(app.mode, Mode::Diff(_)));
        // Goto closes the diff so the panels are visible.
        assert!(
            app.apply(Command::Goto {
                path: p("left"),
                side: Side::Right
            })
            .ok
        );
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.active, LEFT, "remote navigation doesn't move the user's focus");
        assert!(
            app.apply(Command::Select {
                mask: "*.txt".into(),
                side: Side::Right
            })
            .ok
        );
        assert_eq!(app.panels[RIGHT].selected.len(), 2);
        assert!(
            !app.apply(Command::Goto {
                path: p("missing/x"),
                side: Side::Active
            })
            .ok
        );
        assert!(!app.apply(Command::View { path: p("nope") }).ok);
        assert!(
            !app.apply(Command::Goto {
                path: p("left/missing.txt"),
                side: Side::Active
            })
            .ok
        );
        // A dialog the user is answering is never replaced.
        app.handle_key(key(KeyCode::F(7)));
        assert!(!app.apply(Command::View { path: p("left/a.txt") }).ok);
        assert!(matches!(app.mode, Mode::Prompt(_)));
        assert!(app.apply(Command::Message { text: "hi".into() }).ok);
        assert_eq!(app.message.as_ref().unwrap().0, "hi");
    }

    #[test]
    fn live_refresh_picks_up_new_files_and_flashes_them() {
        let (tmp, mut app) = setup();
        app.enable_live_refresh().unwrap();
        app.panels[LEFT].focus_name("b.txt");
        app.tick();
        std::thread::sleep(Duration::from_millis(100));
        std::fs::write(tmp.path().join("left/new.txt"), "x").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !app.panels[LEFT].entries().iter().any(|e| e.name == "new.txt") {
            app.tick();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(app.panels[LEFT].entries().iter().any(|e| e.name == "new.txt"));
        assert!(app.flash[LEFT].contains_key("new.txt"));
        assert!(!app.flash[LEFT].contains_key("a.txt"));
        assert_eq!(app.panels[LEFT].current().unwrap().name, "b.txt", "cursor kept");
        // Leaving the directory drops its highlights.
        app.panels[LEFT].focus_name("sub");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.flash[LEFT].is_empty());
    }

    #[test]
    fn f2_falls_back_to_git_head() {
        let (tmp, mut app) = setup();
        let left = tmp.path().join("left");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&left)
                .args(args)
                .output()
        };
        if git(&["init", "-q"]).is_err() {
            return; // no git here
        }
        git(&["-c", "user.name=t", "-c", "user.email=t@e", "add", "."]).unwrap();
        git(&["-c", "user.name=t", "-c", "user.email=t@e", "commit", "-qm", "x"]).unwrap();
        fs::write(left.join("b.txt"), "changed\n").unwrap();
        app.refresh(LEFT);
        assert_eq!(
            app.panels[LEFT]
                .entries()
                .iter()
                .find(|e| e.name == "b.txt")
                .unwrap()
                .git,
            Some(rc_core::GitStatus::Modified)
        );
        // The right panel has no b.txt, so F2 compares with HEAD.
        app.panels[LEFT].focus_name("b.txt");
        app.panels[RIGHT].focus_name("..");
        app.handle_key(key(KeyCode::F(2)));
        let Mode::Diff(d) = &app.mode else {
            panic!("expected diff, got message {:?}", app.message)
        };
        assert_eq!(d.response.left_title, "HEAD:b.txt");
    }

    #[test]
    fn history_back_and_forward() {
        let (_tmp, mut app) = setup();
        let start = app.panels[LEFT].location.clone();
        app.panels[LEFT].focus_name("sub");
        app.handle_key(key(KeyCode::Enter));
        let sub = app.panels[LEFT].location.clone();
        assert_ne!(sub, start);
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(app.panels[LEFT].location, start);
        assert_eq!(
            app.panels[LEFT].current().unwrap().name,
            "sub",
            "cursor on the dir we came from"
        );
        app.handle_key(key(KeyCode::Char(']')));
        assert_eq!(app.panels[LEFT].location, sub);
    }

    #[test]
    fn history_skips_deleted_places() {
        let (tmp, mut app) = setup();
        let start = app.panels[LEFT].location.clone();
        fs::create_dir(tmp.path().join("left/sub/gone")).unwrap();
        app.panels[LEFT].focus_name("sub");
        app.handle_key(key(KeyCode::Enter));
        app.panels[LEFT].focus_name("gone");
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Char('g')));
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        typed(&mut app, &tmp.path().join("right").to_string_lossy());
        app.handle_key(key(KeyCode::Enter));
        fs::remove_dir(tmp.path().join("left/sub/gone")).unwrap();
        // Back: "gone" no longer exists, so it lands on sub; back again reaches the start.
        app.handle_key(key(KeyCode::Char('[')));
        assert!(
            app.panels[LEFT].location.to_string().ends_with("/left/sub"),
            "{}",
            app.panels[LEFT].location
        );
        app.handle_key(key(KeyCode::Char('[')));
        assert_eq!(app.panels[LEFT].location, start);
    }

    #[test]
    fn hotlist_add_jump_delete() {
        let (_tmp, mut app) = setup();
        let left = app.panels[LEFT].location.to_string();
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('a')));
        assert_eq!(app.settings.bookmarks, std::slice::from_ref(&left));
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('1')));
        assert_eq!(app.panels[RIGHT].location.to_string(), left);
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('d')));
        assert!(app.settings.bookmarks.is_empty());
    }

    #[test]
    fn copy_asks_about_existing_files() {
        let (tmp, mut app) = setup();
        // a.txt exists on both sides with different content.
        app.panels[LEFT].focus_name("a.txt");
        app.handle_key(key(KeyCode::F(5)));
        app.handle_key(key(KeyCode::Enter));
        let Mode::Conflict(c) = &app.mode else {
            panic!("expected a conflict prompt")
        };
        assert_eq!(c.existing.len(), 1);
        // 'n' means no, as in every other dialog.
        app.handle_key(key(KeyCode::Char('n')));
        assert!(matches!(app.mode, Mode::Normal) && app.job.is_none());
        app.handle_key(key(KeyCode::F(5)));
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Char('s')));
        wait_job(&mut app);
        assert_eq!(
            fs::read_to_string(tmp.path().join("right/a.txt")).unwrap(),
            "one\nTWO\n",
            "skipped"
        );
        app.panels[LEFT].focus_name("a.txt");
        app.handle_key(key(KeyCode::F(5)));
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Char('o')));
        wait_job(&mut app);
        assert_eq!(
            fs::read_to_string(tmp.path().join("right/a.txt")).unwrap(),
            "one\ntwo\n",
            "overwritten"
        );
        // No conflict: runs straight away.
        app.panels[LEFT].focus_name("b.txt");
        app.handle_key(key(KeyCode::F(5)));
        app.handle_key(key(KeyCode::Enter));
        assert!(app.job.is_some());
        wait_job(&mut app);
    }

    #[test]
    fn esc_cancels_a_running_job() {
        let (_tmp, mut app) = setup();
        let (_tx, rx) = mpsc::channel();
        app.job = Some(Job {
            verb: "Copy",
            progress: None,
            rx,
            cancel: Arc::default(),
        });
        app.panels[LEFT].select_mask("*", true);
        app.handle_key(key(KeyCode::Esc));
        assert!(app.job.as_ref().unwrap().cancel.load(Ordering::Relaxed));
        assert!(app.job_status().unwrap().starts_with("Cancelling"));
        assert!(
            !app.panels[LEFT].selected.is_empty(),
            "Esc cancelled instead of clearing the selection"
        );
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
