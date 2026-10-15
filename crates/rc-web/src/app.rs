//! Application state, the actions behind every key and button, and the top-level layout.

use crate::diff_view::DiffView;
use crate::help::HelpView;
use crate::hotlist::HotlistView;
use crate::modal::{Modal, ModalKind, ModalView};
use crate::options::OptionsView;
use crate::panel_view::PanelView;
use crate::viewer::Viewer;
use crate::{api, dom, events};
use leptos::ev;
use leptos::prelude::*;
use leptos::task::spawn_local;
use rc_core::api::{BrowserEvent, Conflicts, DiffResponse, FileContent, FileOp, OnConflict, OpResult};
use rc_core::dircmp::{self, CmpMark};
use rc_core::history::History;
use rc_core::{Location, Panel, Row, Settings, SortKey};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use web_sys::KeyboardEvent;

pub const LEFT: usize = 0;
pub const RIGHT: usize = 1;
/// Height of a panel row in pixels; must match `.row` in style.css.
pub const ROW_HEIGHT: i32 = 22;
/// How long rows that changed on disk stay highlighted; matches the `flash` animation.
const FLASH: Duration = Duration::from_millis(1500);

#[derive(Clone)]
pub struct Message {
    pub text: String,
    pub error: bool,
}

/// Views that cover the panels and take over the keyboard while open.
#[derive(Clone)]
pub enum Overlay {
    View(Arc<FileContent>),
    Diff(Arc<DiffResponse>),
    Help,
}

/// How a listing gets into a panel.
#[derive(Clone, Copy, PartialEq)]
enum Nav {
    /// Going somewhere: the place we leave goes on the back history.
    Visit,
    /// Back or forward: the history is already updated.
    History,
    /// Re-reading what the panel shows. Dropped if the panel moved on meanwhile.
    Refresh,
}

/// All UI state. Every field is a signal handle, so `App` is `Copy` and can be moved
/// into any number of closures.
#[derive(Clone, Copy)]
pub struct App {
    pub panels: [RwSignal<Panel>; 2],
    pub active: RwSignal<usize>,
    /// Panel whose path bar is being edited.
    pub editing_path: RwSignal<Option<usize>>,
    /// Panel whose quick-filter input is open.
    pub filtering: RwSignal<Option<usize>>,
    pub modal: RwSignal<Option<Modal>>,
    pub options_open: RwSignal<bool>,
    pub hotlist_open: RwSignal<bool>,
    /// Saved preferences, from `init` and the options dialog.
    pub settings: RwSignal<Settings>,
    pub overlay: RwSignal<Option<Overlay>>,
    pub message: RwSignal<Option<Message>>,
    /// Number of API requests in flight.
    pub busy: RwSignal<u32>,
    /// Per panel: entries that just appeared or changed on disk, highlighted briefly.
    pub flash: [RwSignal<HashSet<String>>; 2],
    /// Per panel: id of the latest navigation, so a slow, older reply can't overwrite
    /// a newer one.
    load_seq: [StoredValue<u64>; 2],
    history: [StoredValue<History>; 2],
}

/// Increments the busy counter for as long as it lives.
struct BusyGuard(RwSignal<u32>);

impl BusyGuard {
    fn new(busy: RwSignal<u32>) -> Self {
        busy.update(|n| *n += 1);
        BusyGuard(busy)
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.try_update(|n| *n -= 1);
    }
}

impl App {
    fn new() -> Self {
        let empty = || RwSignal::new(Panel::new(Location::local("")));
        App {
            panels: [empty(), empty()],
            active: RwSignal::new(LEFT),
            editing_path: RwSignal::new(None),
            filtering: RwSignal::new(None),
            modal: RwSignal::new(None),
            options_open: RwSignal::new(false),
            hotlist_open: RwSignal::new(false),
            settings: RwSignal::new(Settings::default()),
            overlay: RwSignal::new(None),
            message: RwSignal::new(None),
            busy: RwSignal::new(0),
            flash: [RwSignal::new(HashSet::new()), RwSignal::new(HashSet::new())],
            load_seq: [StoredValue::new(0), StoredValue::new(0)],
            history: [
                StoredValue::new(History::default()),
                StoredValue::new(History::default()),
            ],
        }
    }

    fn active_side(self) -> usize {
        self.active.get_untracked()
    }

    /// A dialog or overlay has the keyboard (tracked, for the view).
    pub fn covered(self) -> bool {
        self.modal.with(Option::is_some)
            || self.options_open.get()
            || self.hotlist_open.get()
            || self.overlay.with(Option::is_some)
    }

    pub fn info(self, text: impl Into<String>) {
        self.message.set(Some(Message {
            text: text.into(),
            error: false,
        }));
    }

    pub fn error(self, text: impl Into<String>) {
        self.message.set(Some(Message {
            text: text.into(),
            error: true,
        }));
    }

    /// Run an API request in the background with the busy indicator on and hand the
    /// result to `done`.
    fn request_then<T: 'static>(
        self,
        fut: impl Future<Output = Result<T, String>> + 'static,
        done: impl FnOnce(Result<T, String>) + 'static,
    ) {
        let guard = BusyGuard::new(self.busy);
        spawn_local(async move {
            let result = fut.await;
            drop(guard);
            done(result);
        });
    }

    /// Like [`App::request_then`], with errors going to the message bar.
    fn request<T: 'static>(
        self,
        fut: impl Future<Output = Result<T, String>> + 'static,
        done: impl FnOnce(T) + 'static,
    ) {
        self.request_then(fut, move |result| match result {
            Ok(value) => done(value),
            Err(e) => self.error(e),
        });
    }

    fn start(self) {
        if !api::has_token() {
            self.error("No access token: open the URL printed by `rcmd web`.");
            return;
        }
        self.request(api::init(), move |init| {
            for panel in self.panels {
                panel.update(|p| p.show_hidden = init.settings.show_hidden);
            }
            self.settings.set(init.settings);
            self.load(LEFT, init.left, None);
            self.load(RIGHT, init.right, None);
            events::connect(self);
        });
    }

    // ---- remote control and live refresh ----------------------------------------

    /// Carry out an event from the server, like the terminal UI's `App::apply`.
    pub fn apply(self, event: BrowserEvent) {
        let active = self.active_side();
        // Views that cover the panels get out of the way; dialogs the user is typing in stay.
        let reveal = || self.overlay.set(None);
        match event {
            BrowserEvent::Navigate { side, location, focus } => {
                reveal();
                let side = side.index(active);
                self.show(side, location, focus, Nav::Visit, move |ok| {
                    if ok {
                        self.active.set(side);
                    }
                });
            }
            BrowserEvent::Diff { left, right } => self.open_diff(api::diff(left, right)),
            BrowserEvent::View { location } => self.view(location),
            BrowserEvent::Select { side, mask } => {
                reveal();
                let n = self.panels[side.index(active)]
                    .try_update(|p| p.select_mask(&mask, true))
                    .unwrap_or(0);
                self.info(format!("Selected {n} entries matching {mask}"));
            }
            BrowserEvent::Refresh => self.refresh_both(),
            BrowserEvent::Message { text } => self.info(text),
            BrowserEvent::Changed { location } => {
                for side in [LEFT, RIGHT] {
                    if self.panels[side].with_untracked(|p| p.location == location) {
                        self.live_refresh(side);
                    }
                }
            }
        }
    }

    /// Re-read a panel whose directory changed on disk and highlight what is new or
    /// different. Cursor and selection stay, as for any refresh.
    fn live_refresh(self, side: usize) {
        let panel = self.panels[side];
        let (location, before) = panel.with_untracked(|p| (p.location.clone(), p.entries().to_vec()));
        self.show(side, location, None, Nav::Refresh, move |ok| {
            if !ok {
                return;
            }
            let changed = panel.with_untracked(|p| dircmp::changed_since(&before, p.entries()));
            if changed.is_empty() {
                return;
            }
            let flash = self.flash[side];
            flash.update(|f| f.extend(changed.iter().cloned()));
            set_timeout(
                move || {
                    flash.try_update(|f| changed.iter().for_each(|n| _ = f.remove(n)));
                },
                FLASH,
            );
        });
    }

    // ---- navigation -------------------------------------------------------------

    /// List `location` into panel `side`, with the cursor on `focus` if given. `then`
    /// hears whether it worked; it is not called for a reply that arrived too late.
    fn show(self, side: usize, location: Location, focus: Option<String>, nav: Nav, then: impl FnOnce(bool) + 'static) {
        let seq = self.load_seq[side];
        if nav != Nav::Refresh {
            seq.update_value(|n| *n += 1);
        }
        let mine = seq.get_value();
        let (panel, history) = (self.panels[side], self.history[side]);
        self.request_then(api::list(location), move |result| {
            if seq.get_value() != mine {
                return; // a newer navigation owns the panel now
            }
            match result {
                Ok(listing) => {
                    let here = panel.with_untracked(|p| p.location.clone());
                    if listing.location != here {
                        match nav {
                            Nav::Refresh => return,
                            Nav::Visit if !here.to_string().is_empty() => history.update_value(|h| h.visit(here)),
                            _ => {}
                        }
                    }
                    panel.update(|p| p.set_listing(listing, focus.as_deref()));
                    then(true);
                }
                Err(e) => {
                    self.error(e);
                    then(false);
                }
            }
        });
    }

    /// Go to `location` (recorded in the back history), cursor on `focus` if given.
    pub fn load(self, side: usize, location: Location, focus: Option<String>) {
        self.show(side, location, focus, Nav::Visit, |_| {});
    }

    fn reload(self, side: usize, focus: Option<String>) {
        let location = self.panels[side].with_untracked(|p| p.location.clone());
        self.show(side, location, focus, Nav::Refresh, |_| {});
    }

    pub fn refresh_both(self) {
        self.reload(LEFT, None);
        self.reload(RIGHT, None);
    }

    /// Go up one level and put the cursor on the directory we came from.
    pub fn go_parent(self, side: usize) {
        let (parent, focus) =
            self.panels[side].with_untracked(|p| (p.location.parent(), p.location.file_name().map(str::to_owned)));
        if let Some(parent) = parent {
            self.load(side, parent, focus);
        }
    }

    /// Alt+← / Alt+→ / `[` `]`: step through the active panel's history. Coming back out
    /// of a subdirectory puts the cursor on it.
    fn history_step(self, forward: bool) {
        let side = self.active_side();
        let history = self.history[side];
        let current = self.panels[side].with_untracked(|p| p.location.clone());
        let target = history
            .try_update_value(|h| {
                if forward {
                    h.forward(current.clone())
                } else {
                    h.back(current.clone())
                }
            })
            .flatten();
        let Some(target) = target else { return };
        let focus = (current.parent().as_ref() == Some(&target))
            .then(|| current.file_name().map(str::to_owned))
            .flatten();
        self.show(side, target, focus, Nav::History, move |ok| {
            if !ok {
                // That place is gone: forget it and try the next one, as the terminal UI does.
                history.update_value(|h| if forward { h.discard_forward() } else { h.discard_back() });
                self.history_step(forward);
            }
        });
    }

    /// Enter / double click / F3: go up on "..", into a directory, or view a file.
    pub fn enter(self, side: usize) {
        let (on_parent, target) = self.panels[side].with_untracked(|p| {
            let target = p.current().map(|e| (p.location.join(&e.name), e.is_dir));
            (p.current_row() == Some(Row::Parent), target)
        });
        match (on_parent, target) {
            (true, _) => self.go_parent(side),
            (false, Some((location, true))) => self.load(side, location, None),
            (false, Some((location, false))) => self.view(location),
            (false, None) => {}
        }
    }

    pub fn navigate_input(self, side: usize, input: &str) {
        let target = self.panels[side].with_untracked(|p| Location::resolve_input(input, &p.location));
        if let Some(location) = target {
            self.load(side, location, None);
        }
    }

    fn swap_panels(self) {
        let [left, right] = self.panels;
        let l = left.get_untracked();
        left.set(right.get_untracked());
        right.set(l);
        let [lh, rh] = self.history;
        let h = lh.get_value();
        lh.set_value(rh.get_value());
        rh.set_value(h);
    }

    /// Show or hide dot-files in both panels.
    pub fn show_hidden(self, show: bool) {
        for panel in self.panels {
            if panel.with_untracked(|p| p.show_hidden) != show {
                panel.update(Panel::toggle_hidden);
            }
        }
    }

    /// Save settings on the server and adopt what it stored. `done` hears the outcome;
    /// errors are left to it (the options dialog shows them in place).
    pub fn save_settings(self, settings: Settings, done: impl FnOnce(Result<(), String>) + 'static) {
        self.request_then(api::save_settings(settings), move |result| {
            done(result.map(|saved| self.settings.set(saved)));
        });
    }

    // ---- file actions -----------------------------------------------------------

    fn view(self, location: Location) {
        self.request(api::read(location), move |content| {
            dom::blur_active();
            self.overlay.set(Some(Overlay::View(Arc::new(content))));
        });
    }

    fn open_diff(self, fut: impl Future<Output = Result<DiffResponse, String>> + 'static) {
        self.request(fut, move |diff| {
            dom::blur_active();
            self.overlay.set(Some(Overlay::Diff(Arc::new(diff))));
        });
    }

    /// F4: open the file under the cursor in the configured editor (server side), or
    /// the system default application if none is set. Directories are ignored.
    fn edit(self) {
        let target = self.panels[self.active_side()].with_untracked(|p| {
            p.current()
                .filter(|e| !e.is_dir)
                .map(|e| (p.location.join(&e.name), e.name.clone()))
        });
        let Some((location, name)) = target else { return };
        self.request(api::edit(location), move |()| self.info(format!("Editing {name}")));
    }

    /// Whether `location` is in the listing of a panel showing its directory. Locations
    /// in directories no panel shows count as existing.
    fn listed(self, location: &Location) -> bool {
        let (dir, name) = (location.parent(), location.file_name());
        self.panels
            .iter()
            .find_map(|p| {
                p.with_untracked(|p| {
                    (Some(&p.location) == dir.as_ref())
                        .then(|| name.is_some_and(|n| p.entries().iter().any(|e| e.name == n)))
                })
            })
            .unwrap_or(true)
    }

    /// F2: diff two selected files, or the cursor file against its counterpart in the
    /// other panel (the cursor file there, else the same name). Left panel = left side.
    /// If the other panel has no such file, diff against git HEAD as Shift+F2 does.
    fn diff(self) {
        let side = self.active_side();
        let pair = self.panels[LEFT]
            .with_untracked(|l| self.panels[RIGHT].with_untracked(|r| rc_core::diff_targets(l, r, side == LEFT)));
        match pair {
            Some((left, right)) if self.listed(&left) && self.listed(&right) => self.open_diff(api::diff(left, right)),
            Some(_) => self.diff_head(),
            None => self.error("Diff: put the cursor on a file (or select two files)"),
        }
    }

    /// Shift+F2: diff the file under the cursor against its committed version.
    fn diff_head(self) {
        let file = self.panels[self.active_side()]
            .with_untracked(|p| p.current().filter(|e| !e.is_dir).map(|e| p.location.join(&e.name)));
        match file {
            Some(location) => self.open_diff(api::diff_head(location)),
            None => self.error("Diff against HEAD: put the cursor on a file"),
        }
    }

    /// Locations of the selection (or cursor entry) of the active panel, plus a label
    /// like `"notes.txt"` or `3 items` for dialogs.
    fn targets(self) -> Option<(Vec<Location>, String)> {
        self.panels[self.active_side()].with_untracked(|p| {
            let targets = p.targets();
            let label = match targets[..] {
                [] => return None,
                [one] => format!("\"{}\"", one.name),
                _ => format!("{} items", targets.len()),
            };
            Some((targets.iter().map(|e| p.location.join(&e.name)).collect(), label))
        })
    }

    /// F5 / F6: ask for the destination, prefilled with the other panel's location.
    fn ask_transfer(self, is_move: bool) {
        let Some((sources, label)) = self.targets() else { return };
        let dest = self.panels[1 - self.active_side()].with_untracked(|p| p.location.to_string());
        let verb = if is_move { "Move" } else { "Copy" };
        self.modal.set(Some(Modal {
            title: format!("{verb} {label} to:"),
            input: Some(dest),
            kind: ModalKind::Transfer { is_move, sources },
        }));
    }

    fn ask_mkdir(self) {
        self.modal.set(Some(Modal {
            title: "New folder:".into(),
            input: Some(String::new()),
            kind: ModalKind::Mkdir,
        }));
    }

    fn ask_mask(self, select: bool) {
        let title = if select {
            "Select files matching:"
        } else {
            "Deselect files matching:"
        };
        self.modal.set(Some(Modal {
            title: title.into(),
            input: Some("*".into()),
            kind: ModalKind::Mask { select },
        }));
    }

    /// F8: trash, Shift+F8: permanent delete.
    fn ask_delete(self, permanent: bool) {
        let Some((targets, label)) = self.targets() else { return };
        let title = if permanent {
            format!("Permanently delete {label}? This cannot be undone.")
        } else {
            format!("Move {label} to Trash?")
        };
        self.modal.set(Some(Modal {
            title,
            input: None,
            kind: ModalKind::Delete { targets, permanent },
        }));
    }

    /// Carry out a confirmed dialog. `input` is the text field's value (empty if none).
    pub fn confirm(self, kind: ModalKind, input: String) {
        let side = self.active_side();
        let base = self.panels[side].with_untracked(|p| p.location.clone());
        match kind {
            ModalKind::Transfer { is_move, sources } => {
                let Some(dest) = Location::resolve_input(&input, &base) else {
                    return;
                };
                let on_conflict = OnConflict::Skip;
                self.transfer(if is_move {
                    FileOp::Move {
                        sources,
                        dest,
                        on_conflict,
                    }
                } else {
                    FileOp::Copy {
                        sources,
                        dest,
                        on_conflict,
                    }
                });
            }
            ModalKind::Mkdir => {
                let Some(location) = Location::resolve_input(&input, &base) else {
                    return;
                };
                let focus = location.file_name().map(str::to_owned);
                self.run_op(FileOp::Mkdir { location }, focus);
            }
            ModalKind::Mask { select } => {
                let n = self.panels[side]
                    .try_update(|p| p.select_mask(&input, select))
                    .unwrap_or(0);
                self.info(format!(
                    "{} {n} entries matching {input}",
                    if select { "Selected" } else { "Deselected" }
                ));
            }
            ModalKind::Delete { targets, permanent } => self.run_op(FileOp::Delete { targets, permanent }, None),
            // Answered with a policy through `resolve_conflict`, never plain confirm.
            ModalKind::Conflict { .. } => {}
        }
    }

    /// Run a copy or move, first asking what to do if it would meet existing files.
    fn transfer(self, op: FileOp) {
        let check = op.clone();
        self.request(async move { api::conflicts(&check).await }, move |found: Conflicts| {
            if found.existing.is_empty() {
                return self.run_op(op, None);
            }
            let title = match &found.existing[..] {
                [one] => format!(
                    "{} already exists (folders are merged).",
                    one.rsplit('/').next().unwrap_or(one)
                ),
                many => format!(
                    "{} items already exist at the destination (folders are merged).",
                    many.len()
                ),
            };
            self.modal.set(Some(Modal {
                title,
                input: None,
                kind: ModalKind::Conflict {
                    op,
                    existing: found.existing,
                },
            }));
        });
    }

    /// The answer to the "already exists" dialog.
    pub fn resolve_conflict(self, op: FileOp, policy: OnConflict) {
        self.run_op(op.with_on_conflict(policy), None);
    }

    /// Perform a file operation, then refresh both panels (cursor on `focus` in the active one).
    fn run_op(self, op: FileOp, focus: Option<String>) {
        let verb = op.verb();
        // The panel the operation started from, even if the user switches meanwhile.
        let side = self.active_side();
        self.request(api::op(op), move |result: OpResult| {
            let text = result.summary(verb);
            if result.errors.is_empty() {
                self.info(text);
                // As in the terminal UI: keep the selection only when something failed.
                self.panels.iter().for_each(|p| p.update(|p| p.clear_selection()));
            } else {
                self.error(text);
            }
            self.reload(side, focus);
            self.reload(1 - side, None);
        });
    }

    /// F9: mark files by comparing both panels; pressing again clears the marks.
    fn compare_dirs(self) {
        let [left, right] = self.panels;
        if left.with_untracked(|p| !p.marks.is_empty()) || right.with_untracked(|p| !p.marks.is_empty()) {
            left.update(|p| p.marks.clear());
            right.update(|p| p.marks.clear());
            self.info("Directory compare marks cleared");
            return;
        }
        let (lm, rm) = left.with_untracked(|l| right.with_untracked(|r| dircmp::compare(l.entries(), r.entries())));
        let count = |m: &HashMap<String, CmpMark>, mark| m.values().filter(|&&v| v == mark).count();
        self.info(format!(
            "Compared: {} only left, {} only right, {} newer left, {} newer right, {} differ, {} same",
            count(&lm, CmpMark::Orphan),
            count(&rm, CmpMark::Orphan),
            count(&lm, CmpMark::Newer),
            count(&rm, CmpMark::Newer),
            count(&lm, CmpMark::Different),
            count(&lm, CmpMark::Same),
        ));
        left.update(|p| p.marks = lm);
        right.update(|p| p.marks = rm);
    }

    /// F1–F9, from the keyboard or the footer.
    pub fn function_key(self, n: u8, shift: bool) {
        match n {
            1 => self.overlay.set(Some(Overlay::Help)),
            2 if shift => self.diff_head(),
            2 => self.diff(),
            3 => self.enter(self.active_side()),
            4 => self.edit(),
            5 => self.ask_transfer(false),
            6 => self.ask_transfer(true),
            7 => self.ask_mkdir(),
            8 => self.ask_delete(shift),
            9 => self.compare_dirs(),
            _ => {}
        }
    }

    // ---- keyboard ---------------------------------------------------------------

    /// Global key handler for the panels. Overlays, dialogs and text fields handle their
    /// own keys, so this backs off while any of them has the keyboard.
    fn handle_key(self, ev: &KeyboardEvent) {
        if untrack(|| self.covered()) || dom::is_text_field(ev.target()) || ev.meta_key() {
            return;
        }
        let (ctrl, shift) = (ev.ctrl_key(), ev.shift_key());
        let key = ev.key();
        // Auto-repeat only for movement; a held Enter or F-key must act once.
        let movement = key.starts_with("Arrow") || matches!(key.as_str(), "PageUp" | "PageDown");
        if ev.repeat() && !movement {
            ev.prevent_default();
            return;
        }
        if ev.alt_key() {
            // Alt+←/→ is history, as in browsers; preventDefault keeps the page itself.
            match key.as_str() {
                "ArrowLeft" => self.history_step(false),
                "ArrowRight" => self.history_step(true),
                _ => return,
            }
            ev.prevent_default();
            return;
        }
        let side = self.active_side();
        let panel = self.panels[side];

        // F1–F9, with plain digits as an alias for keyboards where F-keys need Fn.
        let fkey = key
            .strip_prefix('F')
            .unwrap_or(&key)
            .parse::<u8>()
            .ok()
            .filter(|n| (1..=9).contains(n));
        if let Some(n) = fkey.filter(|_| !ctrl) {
            ev.prevent_default();
            self.function_key(n, shift);
            return;
        }
        // Ctrl+F3..F6 sort by name, extension, date, size (as in Total Commander).
        if let Some(n @ 3..=6) = fkey.filter(|_| ctrl && key.starts_with('F')) {
            ev.prevent_default();
            let sort = [SortKey::Name, SortKey::Ext, SortKey::Modified, SortKey::Size][usize::from(n - 3)];
            panel.update(|p| p.set_sort(sort));
            return;
        }

        match (ctrl, key.as_str()) {
            (false, "ArrowUp") => panel.update(|p| p.move_cursor(-1)),
            (false, "ArrowDown") => panel.update(|p| p.move_cursor(1)),
            (false, "PageUp") => panel.update(|p| p.move_cursor(-page_rows(side))),
            (false, "PageDown") => panel.update(|p| p.move_cursor(page_rows(side))),
            (false, "Home") => panel.update(Panel::cursor_home),
            (false, "End") => panel.update(Panel::cursor_end),
            (false, "Enter") => self.enter(side),
            (false, "Backspace" | "ArrowLeft") => self.go_parent(side),
            (false, "ArrowRight") => {
                if panel.with_untracked(|p| p.current().is_some_and(|e| e.is_dir)) {
                    self.enter(side);
                }
            }
            (false, "[") => self.history_step(false),
            (false, "]") => self.history_step(true),
            (true, "s") => panel.update(Panel::cycle_sort),
            (false, "Tab") => self.active.set(1 - side),
            (false, " " | "Insert") => panel.update(Panel::toggle_current),
            (false, "+") => self.ask_mask(true),
            (false, "-") => self.ask_mask(false),
            (false, "*") => panel.update(Panel::invert_selection),
            (false, "Delete") => self.ask_delete(shift),
            (false, "Escape") => panel.update(|p| {
                if p.filter.is_empty() {
                    p.clear_selection();
                } else {
                    p.set_filter("");
                }
            }),
            (false, "/") | (true, "f") => self.filtering.set(Some(side)),
            (false, ".") => panel.update(Panel::toggle_hidden),
            (false, "g") | (true, "l") => self.editing_path.set(Some(side)),
            (false, "o") => self.options_open.set(true),
            (false, "?") => self.overlay.set(Some(Overlay::Help)),
            (true, "d") => self.hotlist_open.set(true),
            (true, "r") => self.refresh_both(),
            (true, "u") => self.swap_panels(),
            _ => return,
        }
        ev.prevent_default();
    }
}

/// Rows per page in a panel's list, for PageUp/PageDown.
fn page_rows(side: usize) -> isize {
    let height = dom::by_id(&format!("list-{side}")).map_or(0, |el| el.client_height());
    (height / ROW_HEIGHT - 1).max(1) as isize
}

/// Footer keys: function key number and label.
const FKEYS: [(u8, &str); 9] = [
    (1, "Help"),
    (2, "Diff"),
    (3, "View"),
    (4, "Edit"),
    (5, "Copy"),
    (6, "Move"),
    (7, "MkDir"),
    (8, "Delete"),
    (9, "CmpDirs"),
];

#[component]
pub fn Root() -> impl IntoView {
    let app = App::new();
    let keys = window_event_listener(ev::keydown, move |ev| app.handle_key(&ev));
    on_cleanup(move || keys.remove());
    // The tab title is the active panel's location.
    Effect::new(move |_| {
        let path = app.panels[app.active.get()].with(|p| p.location.to_string());
        document().set_title(if path.is_empty() { "rcommander" } else { &path });
    });
    app.start();

    // Footer buttons must not take focus, or Enter/Space would click them again.
    let key_button = move |key: String, label: &'static str, action: Box<dyn Fn(bool)>| {
        view! {
            <button class="key" tabindex="-1" on:mousedown=|ev| ev.prevent_default() on:click=move |ev| action(ev.shift_key())>
                <kbd>{key}</kbd>
                {label}
            </button>
        }
    };

    view! {
        // Everything behind a dialog or overlay is inert: no focus, no clicks.
        <div class="app" inert=move || app.covered()>
            <div class="panels">
                <PanelView app side=LEFT/>
                <PanelView app side=RIGHT/>
            </div>
            <footer class="footer">
                <span class="msg-area">
                    {move || {
                        app.message
                            .get()
                            .map(|Message { text, error }| {
                                let title = text.clone();
                                view! { <span class="msg" class:error=error title=title>{text}</span> }
                            })
                    }}
                </span>
                <Show when=move || { app.busy.get() > 0 }>
                    <span class="busy">"Working…"</span>
                </Show>
                <nav class="keys-bar">
                    {FKEYS
                        .into_iter()
                        .map(|(n, label)| key_button(format!("F{n}"), label, Box::new(move |shift| app.function_key(n, shift))))
                        .collect_view()}
                    {key_button("o".into(), "Options", Box::new(move |_| app.options_open.set(true)))}
                </nav>
            </footer>
        </div>
        {move || app.modal.get().map(|modal| view! { <ModalView app modal/> })}
        <Show when=move || app.options_open.get()>
            <OptionsView app/>
        </Show>
        <Show when=move || app.hotlist_open.get()>
            <HotlistView app/>
        </Show>
        {move || {
            app.overlay
                .get()
                .map(|overlay| match overlay {
                    Overlay::View(content) => view! { <Viewer app content/> }.into_any(),
                    Overlay::Diff(diff) => view! { <DiffView app diff/> }.into_any(),
                    Overlay::Help => view! { <HelpView app/> }.into_any(),
                })
        }}
    }
}
