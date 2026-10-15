//! Application state, the actions behind every key and button, and the top-level layout.

use crate::diff_view::DiffView;
use crate::find::{FindForm, FindView};
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
use rc_core::api::{
    BrowserEvent, ChangesDirs, Conflicts, DiffResponse, FileContent, FileOp, OnConflict, OpResult, Progress,
};
use rc_core::dircmp::{self, CmpMark};
use rc_core::format::human_size;
use rc_core::history::History;
use rc_core::{Location, Panel, Row, Settings, SortKey};
use std::collections::HashMap;
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

/// A copy, move or delete the server is running, followed in the footer.
#[derive(Clone)]
pub struct Job {
    pub id: String,
    /// "Copying", "Moving", "Deleting".
    pub verb: &'static str,
    /// Latest report from the server; `None` until the first one.
    pub progress: Option<Progress>,
    /// Shown once the operation has run for a moment (quick ones never flash up).
    pub shown: bool,
    pub cancelling: bool,
}

/// How long an operation runs before its progress shows, and how often it is polled.
const JOB_POLL: Duration = Duration::from_millis(250);

/// Views that cover the panels and take over the keyboard while open.
#[derive(Clone)]
pub enum Overlay {
    /// A file, and a 1-based line to scroll to and highlight.
    View(Arc<FileContent>, Option<usize>),
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
    /// Find files (Alt+F7) is open, and its form and last results.
    pub find_open: RwSignal<bool>,
    pub find: FindForm,
    /// A hit is being viewed from Find files: closing the viewer goes back to it.
    find_return: StoredValue<bool>,
    /// Saved preferences, from `init` and the options dialog.
    pub settings: RwSignal<Settings>,
    /// Where "Claude's changes" lives, from `init`.
    pub changes: RwSignal<Option<ChangesDirs>>,
    pub overlay: RwSignal<Option<Overlay>>,
    pub message: RwSignal<Option<Message>>,
    /// Number of API requests in flight.
    pub busy: RwSignal<u32>,
    /// Running copies, moves and deletes, oldest first.
    pub jobs: RwSignal<Vec<Job>>,
    /// Number of jobs started, for their ids.
    job_seq: StoredValue<u64>,
    /// Per panel: entries that just appeared or changed on disk, highlighted briefly,
    /// with the refresh that highlighted them (a later one restarts the highlight).
    pub flash: [RwSignal<HashMap<String, u64>>; 2],
    /// Per panel: id of the latest navigation, so a slow, older reply can't overwrite
    /// a newer one.
    load_seq: [StoredValue<u64>; 2],
    /// Per panel: id of the latest refresh; only its reply is applied.
    refresh_seq: [StoredValue<u64>; 2],
    history: [StoredValue<History>; 2],
    /// Per panel: the navigation id of a back/forward step still loading (key repeat
    /// must not stack steps). Stale once any newer navigation starts.
    history_busy: [StoredValue<Option<u64>>; 2],
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
            find_open: RwSignal::new(false),
            find: FindForm::new(),
            find_return: StoredValue::new(false),
            settings: RwSignal::new(Settings::default()),
            changes: RwSignal::new(None),
            overlay: RwSignal::new(None),
            message: RwSignal::new(None),
            busy: RwSignal::new(0),
            jobs: RwSignal::new(Vec::new()),
            job_seq: StoredValue::new(0),
            flash: [RwSignal::new(HashMap::new()), RwSignal::new(HashMap::new())],
            load_seq: [StoredValue::new(0), StoredValue::new(0)],
            refresh_seq: [StoredValue::new(0), StoredValue::new(0)],
            history_busy: [StoredValue::new(None), StoredValue::new(None)],
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
            || self.find_open.get()
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
            self.changes.set(init.changes);
            self.load(LEFT, init.left, None);
            self.load(RIGHT, init.right, None);
            events::connect(self);
        });
    }

    // ---- remote control and live refresh ----------------------------------------

    /// Carry out an event from the server, like the terminal UI's `App::apply`.
    pub fn apply(self, event: BrowserEvent) {
        let active = self.active_side();
        // Like the terminal UI: never act behind a dialog the user is answering (the
        // browser can't report that back to `rcmd ctl`, so say it here).
        let dialog_open = self.modal.with_untracked(Option::is_some)
            || self.options_open.get_untracked()
            || self.hotlist_open.get_untracked()
            || self.find_open.get_untracked();
        let acts = matches!(
            event,
            BrowserEvent::Navigate { .. }
                | BrowserEvent::Diff { .. }
                | BrowserEvent::View { .. }
                | BrowserEvent::Select { .. }
        );
        if dialog_open && acts {
            self.info("Ignored a remote command while a dialog is open");
            return;
        }
        // Views that cover the panels get out of the way.
        let reveal = || self.overlay.set(None);
        match event {
            // Shows the place without moving the user's focus, as in the terminal UI.
            BrowserEvent::Navigate { side, location, focus } => {
                reveal();
                self.show(side.index(active), location, focus, Nav::Visit, |_| {});
            }
            BrowserEvent::Diff { left, right } => self.open_diff(api::diff(left, right)),
            BrowserEvent::View { location, line } => self.view_at(location, line),
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
            let generation = self.refresh_seq[side].get_value();
            flash.update(|f| f.extend(changed.iter().map(|n| (n.clone(), generation))));
            set_timeout(
                move || {
                    // Only end the highlights this refresh started.
                    flash.try_update(|f| f.retain(|_, g| *g != generation));
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
        // Refreshes are ordered among themselves: only the latest one's reply counts.
        let refreshes = self.refresh_seq[side];
        if nav == Nav::Refresh {
            refreshes.update_value(|n| *n += 1);
        }
        let my_refresh = refreshes.get_value();
        let (panel, history) = (self.panels[side], self.history[side]);
        self.request_then(api::list(location), move |result| {
            if seq.get_value() != mine || (nav == Nav::Refresh && refreshes.get_value() != my_refresh) {
                return; // a newer navigation or refresh owns the panel now
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
        let busy = self.history_busy[side];
        // A step is still loading (e.g. key repeat): taking another now would push the
        // same place twice and lose the one in between.
        if busy.get_value() == Some(self.load_seq[side].get_value()) {
            return;
        }
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
            busy.set_value(None);
            if !ok {
                // That place is gone: forget it and try the next one, as the terminal UI does.
                history.update_value(|h| if forward { h.discard_forward() } else { h.discard_back() });
                self.history_step(forward);
            }
        });
        busy.set_value(Some(self.load_seq[side].get_value()));
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

    /// Alt+F7: find files under the active panel's folder.
    fn open_find(self) {
        let (location, hidden) =
            self.panels[self.active_side()].with_untracked(|p| (p.location.clone(), p.show_hidden));
        self.find.open(location, hidden);
        self.find_open.set(true);
    }

    /// Show `name` in `folder` in the active panel, as Find's "go to" does. Hidden files
    /// are switched on if the target is one, and a quick filter is cleared.
    pub fn go_to(self, folder: Location, name: String) {
        let side = self.active_side();
        if name.starts_with('.') && !self.panels[side].with_untracked(|p| p.show_hidden) {
            self.panels[side].update(Panel::toggle_hidden);
        }
        // A quick filter could hide it.
        if self.panels[side].with_untracked(|p| !p.filter.is_empty()) {
            self.panels[side].update(|p| p.set_filter(""));
            self.filtering.set(None);
        }
        self.load(side, folder, Some(name));
    }

    /// View a hit from Find files; closing the viewer reopens the dialog.
    pub fn view_from_find(self, location: Location, line: Option<usize>) {
        self.request_then(api::read(location), move |result| match result {
            Ok(content) => {
                self.find_return.set_value(true);
                dom::blur_active();
                self.overlay.set(Some(Overlay::View(Arc::new(content), line)));
            }
            Err(e) => {
                self.error(e);
                self.find_open.set(true);
            }
        });
    }

    fn view(self, location: Location) {
        self.view_at(location, None);
    }

    /// Open the viewer, optionally at a line.
    fn view_at(self, location: Location, line: Option<usize>) {
        self.request(api::read(location), move |content| {
            dom::blur_active();
            self.overlay.set(Some(Overlay::View(Arc::new(content), line)));
        });
    }

    fn open_diff(self, fut: impl Future<Output = Result<DiffResponse, String>> + 'static) {
        self.open_diff_titled(fut, None);
    }

    /// Like `open_diff`, replacing the left side's title.
    fn open_diff_titled(self, fut: impl Future<Output = Result<DiffResponse, String>> + 'static, left: Option<String>) {
        self.request(fut, move |mut diff: DiffResponse| {
            if let Some(title) = left {
                diff.left_title = title;
            }
            dom::blur_active();
            self.overlay.set(Some(Overlay::Diff(Arc::new(diff))));
        });
    }

    /// Whether the active panel shows "Claude's changes".
    fn in_changes(self) -> bool {
        let here = self.panels[self.active_side()].with_untracked(|p| p.location.clone());
        self.changes
            .with_untracked(|c| c.as_ref().is_some_and(|c| c.links == here))
    }

    /// `c`: show "Claude's changes" (files an agent edited) in the active panel.
    fn show_changes(self) {
        match self.changes.get_untracked() {
            Some(c) => {
                // Edited dot-files (.env, .gitignore…) belong in the list too.
                let side = self.active_side();
                if !self.panels[side].with_untracked(|p| p.show_hidden) {
                    self.panels[side].update(Panel::toggle_hidden);
                }
                self.load(side, c.links, None);
                self.info("Claude's changes: F2 diff against the original · F3 view · F8 forget");
            }
            None => self.error("Claude's changes are not available"),
        }
    }

    /// F2 in "Claude's changes": the file against its content before the agent's edits.
    fn diff_change(self) {
        let Some(c) = self.changes.get_untracked() else { return };
        let entry = self.panels[self.active_side()].with_untracked(|p| p.current().cloned());
        let Some(e) = entry.filter(|e| e.is_link) else { return };
        let Some(target) = e.link_target.clone() else { return };
        let original = c.originals.join(&e.name);
        let current = Location::local(&target);
        let title = format!("{target} (before)");
        // A file deleted (or moved) since shows as "does not exist" on the right.
        let fut = async move {
            match api::diff(original.clone(), current).await {
                Ok(diff) => Ok(diff),
                Err(_) => api::diff_sides(Some(original), None).await,
            }
        };
        self.open_diff_titled(fut, Some(title));
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
        if self.in_changes() {
            return self.diff_change();
        }
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
        if self.refuse_second_job() {
            return;
        }
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
    /// F8 in "Claude's changes": forget entries, never touching the edited files.
    fn ask_forget(self) {
        let names: Vec<String> =
            self.panels[self.active_side()].with_untracked(|p| p.targets().iter().map(|e| e.name.clone()).collect());
        let title = match &names[..] {
            [] => return,
            [one] => format!("Forget \"{one}\" from Claude's changes? The file stays as it is."),
            many => format!(
                "Forget {} entries from Claude's changes? The files stay as they are.",
                many.len()
            ),
        };
        self.modal.set(Some(Modal {
            title,
            input: None,
            kind: ModalKind::Forget { names },
        }));
    }

    fn ask_delete(self, permanent: bool) {
        if self.refuse_second_job() {
            return;
        }
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
            // Forget entries: remove the log's link and saved original; the file stays.
            ModalKind::Forget { names } => {
                let side = self.active_side();
                self.request(api::forget_changes(names), move |res: OpResult| {
                    let text = if res.errors.is_empty() {
                        format!("Forgot {} from Claude's changes; the files are untouched", res.done)
                    } else {
                        format!("Forgot {}; {}", res.done, res.errors[0])
                    };
                    if res.errors.is_empty() {
                        self.info(text)
                    } else {
                        self.error(text)
                    }
                    self.reload(side, None);
                });
            }
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
        if self.job_kind(&op).is_some() && self.refuse_second_job() {
            return;
        }
        let job = self.start_job(&op);
        let fut = api::op(op, job.clone());
        self.request(
            async move {
                let result = fut.await;
                if let Some(id) = job {
                    self.jobs.update(|jobs| jobs.retain(|j| j.id != id));
                }
                result
            },
            move |result: OpResult| {
                let text = result.summary(verb);
                // As in the terminal UI: keep the selection when something failed or the
                // operation was cancelled, so the rest can be retried.
                if result.errors.is_empty() && !result.cancelled {
                    self.info(text);
                    self.panels.iter().for_each(|p| p.update(|p| p.clear_selection()));
                } else {
                    self.error(text);
                }
                self.reload(side, focus);
                self.reload(1 - side, None);
            },
        );
    }

    /// Copies, moves and deletes run as jobs (with progress, one at a time, as in the
    /// terminal UI); other operations are quick and run without one.
    fn job_kind(self, op: &FileOp) -> Option<&'static str> {
        match op {
            FileOp::Copy { .. } => Some("Copying"),
            FileOp::Move { .. } => Some("Moving"),
            FileOp::Delete { .. } => Some("Deleting"),
            FileOp::Mkdir { .. } | FileOp::Rename { .. } => None,
        }
    }

    /// True (with a message) while a copy, move or delete is still running.
    fn refuse_second_job(self) -> bool {
        let running = self.jobs.with_untracked(|jobs| !jobs.is_empty());
        if running {
            self.error("Another operation is still running");
        }
        running
    }

    /// Register a copy, move or delete as a job and start following its progress.
    fn start_job(self, op: &FileOp) -> Option<String> {
        let verb = self.job_kind(op)?;
        let seq = self.job_seq.get_value() + 1;
        self.job_seq.set_value(seq);
        // Unique across tabs and reloads talking to the same server.
        let id = format!("{seq}-{:x}", (js_sys::Math::random() * u32::MAX as f64) as u32);
        self.jobs.update(|jobs| {
            jobs.push(Job {
                id: id.clone(),
                verb,
                progress: None,
                shown: false,
                cancelling: false,
            })
        });
        self.poll_job(id.clone());
        Some(id)
    }

    /// Ask the server where the job is, again and again until it finishes.
    fn poll_job(self, id: String) {
        set_timeout(
            move || {
                if !self.jobs.with_untracked(|jobs| jobs.iter().any(|j| j.id == id)) {
                    return;
                }
                spawn_local(async move {
                    let status = api::job(id.clone()).await;
                    self.jobs.update(|jobs| {
                        if let Some(job) = jobs.iter_mut().find(|j| j.id == id) {
                            job.shown = true;
                            if let Ok(status) = status
                                && status.progress.is_some()
                            {
                                job.progress = status.progress;
                            }
                        }
                    });
                    self.poll_job(id);
                });
            },
            JOB_POLL,
        );
    }

    /// The footer's Cancel button: the server stops at the next file or chunk.
    pub fn cancel_job(self, id: String) {
        let set_cancelling = move |id: &str, on: bool| {
            self.jobs.update(|jobs| {
                if let Some(job) = jobs.iter_mut().find(|j| j.id == id) {
                    job.cancelling = on;
                }
            })
        };
        set_cancelling(&id, true);
        spawn_local(async move {
            match api::cancel(id.clone()).await {
                Ok(status) if status.running => {}
                // Not running on the server (just finished, or not started yet): if it
                // is still listed here, let the user try again.
                Ok(_) => set_cancelling(&id, false),
                Err(e) => {
                    set_cancelling(&id, false);
                    self.error(e);
                }
            }
        });
    }

    /// Esc with nothing else to clear: stop the running operation.
    fn cancel_running_job(self) -> bool {
        let running = self
            .jobs
            .with_untracked(|jobs| jobs.last().filter(|j| !j.cancelling).map(|j| j.id.clone()));
        let Some(id) = running else { return false };
        self.cancel_job(id);
        true
    }

    /// F9: mark files by comparing both panels; pressing again clears the marks.
    fn compare_dirs(self) {
        let [left, right] = self.panels;
        if left.with_untracked(|p| !p.marks.is_empty()) || right.with_untracked(|p| !p.marks.is_empty()) {
            left.update(|p| p.set_marks(HashMap::new()));
            right.update(|p| p.set_marks(HashMap::new()));
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
        left.update(|p| p.set_marks(lm));
        right.update(|p| p.set_marks(rm));
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
            8 if self.in_changes() => self.ask_forget(),
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
                "F7" => self.open_find(),
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
            (false, "Delete") if self.in_changes() => self.ask_forget(),
            (false, "Delete") => self.ask_delete(shift),
            (false, "c") => self.show_changes(),
            (false, "F") => self.open_find(),
            (false, "Escape") => {
                let idle = panel.with_untracked(|p| p.filter.is_empty() && p.selected.is_empty());
                if !(idle && self.cancel_running_job()) {
                    panel.update(|p| {
                        if p.filter.is_empty() {
                            p.clear_selection();
                        } else {
                            p.set_filter("");
                        }
                    })
                }
            }
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
    // Closing or reloading the page loses sight of a running operation: ask first, as
    // the terminal UI does before quitting.
    let unload = window_event_listener(ev::beforeunload, move |ev| {
        if app.jobs.with_untracked(|jobs| !jobs.is_empty()) {
            ev.prevent_default();
        }
    });
    on_cleanup(move || {
        keys.remove();
        unload.remove();
    });
    // The tab title is the active panel's location.
    Effect::new(move |_| {
        let path = app.panels[app.active.get()].with(|p| p.location.to_string());
        document().set_title(if path.is_empty() { "rcommander" } else { &path });
    });
    app.start();
    // Closing a viewer opened from Find files goes back to the results.
    Effect::new(move |_| {
        if app.overlay.with(Option::is_none) && app.find_return.get_value() {
            app.find_return.set_value(false);
            app.find_open.set(true);
        }
    });

    // Footer buttons must not take focus, or Enter/Space would click them again.
    let key_button = move |key: String, label: &'static str, action: Box<dyn Fn(bool)>| {
        view! {
            <button class="key" tabindex="-1" title=label on:mousedown=|ev| ev.prevent_default() on:click=move |ev| action(ev.shift_key())>
                <kbd>{key}</kbd>
                <span class="key-label">{label}</span>
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
                <For each=move || app.jobs.with(|jobs| jobs.iter().filter(|j| j.shown).map(|j| j.id.clone()).collect::<Vec<_>>()) key=String::clone let:id>
                    <JobView app id/>
                </For>
                // A job shows its own progress.
                <Show when=move || { app.busy.get() > 0 && app.jobs.with(|jobs| !jobs.iter().any(|j| j.shown)) }>
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
        <Show when=move || app.find_open.get()>
            <FindView app/>
        </Show>
        {move || {
            app.overlay
                .get()
                .map(|overlay| match overlay {
                    Overlay::View(content, line) => view! { <Viewer app content line/> }.into_any(),
                    Overlay::Diff(diff) => view! { <DiffView app diff/> }.into_any(),
                    Overlay::Help => view! { <HelpView app/> }.into_any(),
                })
        }}
    }
}

/// A running operation in the footer: what, which file, how far, and a way to stop it.
#[component]
fn JobView(app: App, id: String) -> impl IntoView {
    let job = {
        let id = id.clone();
        Memo::new(move |_| {
            app.jobs.with(|jobs| {
                jobs.iter()
                    .find(|j| j.id == id)
                    .map(|j| (j.verb, j.progress.clone(), j.cancelling))
            })
        })
    };
    let progress = move || job.get().and_then(|(_, p, _)| p);
    let cancelling = move || job.get().is_some_and(|(_, _, c)| c);
    // Deletes report files, not bytes: no bar for them.
    let percent = move || progress().filter(|p| p.total > 0).map(|p| p.percent());
    let title = move || {
        progress().map(|p| {
            let bytes = if p.total > 0 {
                format!("\n{} of {}", human_size(p.done.min(p.total)), human_size(p.total))
            } else {
                String::new()
            };
            format!("{}{bytes}", p.path)
        })
    };
    view! {
        <span class="job" title=title>
            <span class="job-verb">{move || if cancelling() { "Cancelling…" } else { job.get().map_or("", |(v, _, _)| v) }}</span>
            <span class="job-file">{move || progress().map(|p| p.path.rsplit('/').next().unwrap_or_default().to_string())}</span>
            <Show when=move || percent().is_some()>
                <span class="job-bar" role="progressbar" aria-valuemin="0" aria-valuemax="100" aria-valuenow=move || percent().unwrap_or(0)>
                    <span class="job-fill" style:width=move || format!("{}%", percent().unwrap_or(0))></span>
                </span>
                <span class="job-pct">{move || format!("{}%", percent().unwrap_or(0))}</span>
            </Show>
            <button class="job-cancel" tabindex="-1" title="Cancel (Esc)" disabled=cancelling
                on:mousedown=|ev| ev.prevent_default()
                on:click=move |_| app.cancel_job(id.clone())>
                "Cancel"
            </button>
        </span>
    }
}
