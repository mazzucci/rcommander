//! Application state, the actions behind every key and button, and the top-level layout.

use crate::diff_view::DiffView;
use crate::modal::{Modal, ModalKind, ModalView};
use crate::panel_view::PanelView;
use crate::viewer::Viewer;
use crate::{api, dom};
use leptos::ev;
use leptos::prelude::*;
use leptos::task::spawn_local;
use rc_core::api::{DiffResponse, FileContent, FileOp, OpResult};
use rc_core::dircmp::{self, CmpMark};
use rc_core::{Location, Panel, Row};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use web_sys::KeyboardEvent;

pub const LEFT: usize = 0;
pub const RIGHT: usize = 1;
/// Height of a panel row in pixels; must match `.row` in style.css.
pub const ROW_HEIGHT: i32 = 22;

#[derive(Clone)]
pub struct Message {
    pub text: String,
    pub error: bool,
}

/// Full-window views that take over the keyboard while open.
#[derive(Clone)]
pub enum Overlay {
    View(Arc<FileContent>),
    Diff(Arc<DiffResponse>),
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
    pub overlay: RwSignal<Option<Overlay>>,
    pub message: RwSignal<Option<Message>>,
    /// Number of API requests in flight.
    pub busy: RwSignal<u32>,
    /// Local UTC offset in seconds, for `format_time`.
    pub tz_offset: i64,
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
            overlay: RwSignal::new(None),
            message: RwSignal::new(None),
            busy: RwSignal::new(0),
            // getTimezoneOffset() is "UTC minus local" in minutes.
            tz_offset: -(js_sys::Date::new_0().get_timezone_offset() as i64) * 60,
        }
    }

    fn active_side(self) -> usize {
        self.active.get_untracked()
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

    /// Run an API request in the background with the busy indicator on. Errors go to the
    /// message bar; a successful result is passed to `done`.
    fn request<T: 'static>(
        self,
        fut: impl Future<Output = Result<T, String>> + 'static,
        done: impl FnOnce(T) + 'static,
    ) {
        let guard = BusyGuard::new(self.busy);
        spawn_local(async move {
            let result = fut.await;
            drop(guard);
            match result {
                Ok(value) => done(value),
                Err(e) => self.error(e),
            }
        });
    }

    fn start(self) {
        if !api::has_token() {
            self.error("No access token: open the URL printed by `rcmd web`.");
            return;
        }
        self.request(api::init(), move |init| {
            self.load(LEFT, init.left, None);
            self.load(RIGHT, init.right, None);
        });
    }

    // ---- navigation -------------------------------------------------------------

    /// List `location` into a panel, optionally putting the cursor on the entry `focus`.
    pub fn load(self, side: usize, location: Location, focus: Option<String>) {
        self.request(api::list(location), move |listing| {
            self.panels[side].update(|p| p.set_listing(listing, focus.as_deref()));
        });
    }

    fn reload(self, side: usize, focus: Option<String>) {
        let location = self.panels[side].with_untracked(|p| p.location.clone());
        self.load(side, location, focus);
    }

    fn refresh_both(self) {
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
    }

    fn set_hidden_everywhere(self) {
        let show = !self.panels[self.active_side()].with_untracked(|p| p.show_hidden);
        for panel in self.panels {
            if panel.with_untracked(|p| p.show_hidden) != show {
                panel.update(Panel::toggle_hidden);
            }
        }
    }

    // ---- file actions -----------------------------------------------------------

    fn view(self, location: Location) {
        self.request(api::read(location), move |content| {
            self.overlay.set(Some(Overlay::View(Arc::new(content))));
        });
    }

    /// F4: open the entry under the cursor with the system default application.
    fn open_external(self) {
        let target = self.panels[self.active_side()]
            .with_untracked(|p| p.current().map(|e| (p.location.join(&e.name), e.name.clone())));
        let Some((location, name)) = target else { return };
        self.request(api::open(location), move |()| self.info(format!("Opened {name}")));
    }

    /// F2: diff two selected files, or the cursor file against its counterpart in the
    /// other panel (the cursor file there, else the same name). Left panel = left side.
    fn diff(self) {
        let side = self.active_side();
        let pair = self.panels[LEFT]
            .with_untracked(|l| self.panels[RIGHT].with_untracked(|r| rc_core::diff_targets(l, r, side == LEFT)));
        match pair {
            Some((left, right)) => self.request(api::diff(left, right), move |diff| {
                self.overlay.set(Some(Overlay::Diff(Arc::new(diff))));
            }),
            None => self.error("Put the cursor on a file (or select two files) to compare."),
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
            title: "Create directory:".into(),
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
            input: Some("*.*".into()),
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
                let op = if is_move {
                    FileOp::Move {
                        sources,
                        dest,
                        overwrite: false,
                    }
                } else {
                    FileOp::Copy {
                        sources,
                        dest,
                        overwrite: false,
                    }
                };
                self.run_op(op, None);
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
        }
    }

    /// Perform a file operation, then refresh both panels (cursor on `focus` in the active one).
    fn run_op(self, op: FileOp, focus: Option<String>) {
        let verb = op.verb();
        self.request(api::op(op), move |result: OpResult| {
            let text = result.summary(verb);
            if result.errors.is_empty() {
                self.info(text);
            } else {
                self.error(text);
            }
            let side = self.active_side();
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

    /// F2–F9, from the keyboard or the button bar.
    pub fn function_key(self, n: u8, shift: bool) {
        match n {
            2 => self.diff(),
            3 => self.enter(self.active_side()),
            4 => self.open_external(),
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
        if self.modal.with_untracked(Option::is_some)
            || self.overlay.with_untracked(Option::is_some)
            || dom::is_text_field(ev.target())
            || ev.alt_key()
            || ev.meta_key()
        {
            return;
        }
        let (ctrl, shift) = (ev.ctrl_key(), ev.shift_key());
        let key = ev.key();
        let side = self.active_side();
        let panel = self.panels[side];

        // F2–F9, with plain digits as an alias for keyboards where F-keys need Fn.
        let fkey = key
            .strip_prefix('F')
            .unwrap_or(&key)
            .parse::<u8>()
            .ok()
            .filter(|n| (2..=9).contains(n));
        if let Some(n) = fkey.filter(|_| !ctrl) {
            ev.prevent_default();
            self.function_key(n, shift);
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
            (false, "Backspace") => self.go_parent(side),
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
            (false, ".") => self.set_hidden_everywhere(),
            (false, "g") | (true, "l") => self.editing_path.set(Some(side)),
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

const FKEYS: [(u8, &str); 8] = [
    (2, "Diff"),
    (3, "View"),
    (4, "Open"),
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
    app.start();

    view! {
        <div class="panels">
            <PanelView app side=LEFT/>
            <PanelView app side=RIGHT/>
        </div>
        <div class="msgbar">
            {move || {
                app.message
                    .get()
                    .map(|Message { text, error }| {
                        let title = text.clone();
                        view! { <span class="msg" class:error=error title=title>{text}</span> }
                    })
            }}
            <Show when=move || { app.busy.get() > 0 }>
                <span class="busy">"Working…"</span>
            </Show>
        </div>
        <div class="fkeys">
            {FKEYS
                .into_iter()
                .map(|(n, label)| {
                    view! {
                        // mousedown default would move focus to the button, where Enter/Space
                        // would then click it again.
                        <button on:mousedown=|ev| ev.prevent_default() on:click=move |ev| app.function_key(n, ev.shift_key())>
                            <b>"F"{n}</b>
                            " "
                            {label}
                        </button>
                    }
                })
                .collect_view()}
        </div>
        {move || app.modal.get().map(|modal| view! { <ModalView app modal/> })}
        {move || {
            app.overlay
                .get()
                .map(|overlay| match overlay {
                    Overlay::View(content) => view! { <Viewer app content/> }.into_any(),
                    Overlay::Diff(diff) => view! { <DiffView app diff/> }.into_any(),
                })
        }}
    }
}
