//! Terminal UI state and key handling. Drawing lives in `ui.rs`; this module has no
//! terminal I/O so it can be unit-tested with plain key events.

use super::views::{DiffView, Viewer};
use crate::control::Request;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rc_core::api::{FileOp, OpResult};
use rc_core::control::{Command, Reply};
use rc_core::panel::SortKey;
use rc_core::{Column, Location, Panel, Row, Settings, dircmp};
use rc_vfs::Vfs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};

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
    pub settings: Settings,
    /// Where settings are saved; `None` keeps them in memory (tests).
    settings_path: Option<PathBuf>,
    /// Remote-control requests from `rcmd ctl` (see `control.rs`).
    pub control: Option<Receiver<Request>>,
}

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
                    self.report(&res.summary("Create directory"), res.errors.is_empty());
                    self.refresh_all();
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

    /// Apply pending remote-control commands and poll the running job. Returns true if
    /// something changed and a redraw is due.
    pub fn tick(&mut self) -> bool {
        let mut changed = false;
        while let Some(req) = self.control.as_ref().and_then(|rx| rx.try_recv().ok()) {
            let reply = self.apply(req.command);
            let _ = req.reply.send(reply);
            changed = true;
        }
        self.tick_job() || changed
    }

    /// Carry out a remote-control command.
    pub fn apply(&mut self, command: Command) -> Reply {
        // Views that cover the panels get out of the way; dialogs the user is typing in stay.
        let reveal = |mode: &mut Mode| {
            if matches!(mode, Mode::Viewer(_) | Mode::Diff(_) | Mode::Help) {
                *mode = Mode::Normal;
            }
        };
        let base = self.active_panel().location.clone();
        let loc = |p: &str| self.vfs.normalize(Location::local(p), Some(&base));
        match command {
            Command::Goto { path, side } => {
                reveal(&mut self.mode);
                let side = side.index(self.active);
                let target = loc(&path);
                let Location::Local { path: p } = &target;
                let ok = if std::path::Path::new(p).is_dir() {
                    self.load(side, target, None)
                } else {
                    let (Some(dir), Some(name)) = (target.parent(), target.file_name().map(str::to_string)) else {
                        return Reply::error(format!("cannot show {path}"));
                    };
                    self.load(side, dir, Some(&name))
                };
                if !ok {
                    return Reply::error(self.message.clone().map_or_else(|| "failed".into(), |m| m.0));
                }
                self.active = side;
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
                    job.current = p;
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
        let ok = res.errors.is_empty();
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
            }
            KeyCode::Char('s') if ctrl => self.active_mut().cycle_sort(),
            KeyCode::F(n @ 3..=6) if ctrl => {
                let key = [SortKey::Name, SortKey::Ext, SortKey::Modified, SortKey::Size][n as usize - 3];
                self.active_mut().set_sort(key);
            }
            KeyCode::Char('g') => self.start_goto(),
            KeyCode::Char('l') if ctrl => self.start_goto(),
            KeyCode::F(1) | KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('o') => {
                self.mode = Mode::Options(Options {
                    cursor: 0,
                    editing: None,
                    original: self.settings.clone(),
                })
            }
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
            current: String::new(),
            rx,
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
            current: String::new(),
            rx,
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
        // Row 4 is Permissions.
        for _ in 0..4 {
            app.handle_key(key(KeyCode::Down));
        }
        app.handle_key(key(KeyCode::Char(' ')));
        assert!(app.settings.shows(Column::Permissions));
        // Hidden files row applies immediately.
        app.panels[LEFT].show_hidden = true;
        for _ in 0..3 {
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
        assert_eq!(app.active, RIGHT);
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
        assert!(app.apply(Command::Message { text: "hi".into() }).ok);
        assert_eq!(app.message.as_ref().unwrap().0, "hi");
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
