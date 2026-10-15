//! F1 / `?`: the key reference.

use crate::app::App;
use crate::dom;
use leptos::ev;
use leptos::html::Div;
use leptos::prelude::*;

/// Keys of the browser UI. Keep in sync with `handle_key` in app.rs and the README.
const KEYS: &[(&str, &str)] = &[
    ("↑ ↓ PgUp PgDn Home End", "move cursor"),
    ("Enter  →", "open directory, view file"),
    ("Backspace  ←", "parent directory"),
    ("Alt+← Alt+→  [  ]", "back / forward"),
    ("Tab", "switch panel"),
    ("Space  Insert", "select / deselect"),
    ("+  -  *", "select by mask, deselect by mask, invert"),
    ("Esc", "clear filter, then selection"),
    ("/  Ctrl+F", "quick filter"),
    (".", "show / hide hidden files"),
    ("g  Ctrl+L", "go to path"),
    ("Ctrl+D", "hotlist (bookmarks)"),
    ("c", "Claude's changes: F2 diff vs original, F8 forget"),
    ("o", "options: columns, hidden files, editor"),
    ("Ctrl+S  Ctrl+F3…F6", "cycle sort / sort by name, ext, date, size"),
    ("Ctrl+R  Ctrl+U", "refresh / swap panels"),
    ("F2", "diff the files under both cursors, or two selected"),
    ("Shift+F2", "diff the file against git HEAD"),
    ("F3  F4", "view / edit with the editor from options"),
    ("F5  F6", "copy / move or rename"),
    ("F7  F8", "new folder / delete to Trash (Shift: permanently)"),
    ("F9", "compare folders (toggle)"),
    ("1 … 9", "same as F1 … F9"),
];

#[component]
pub fn HelpView(app: App) -> impl IntoView {
    let dialog = NodeRef::<Div>::new();
    let keys = window_event_listener(ev::keydown, move |ev| {
        match ev.key().as_str() {
            "Escape" | "F1" | "1" | "?" | "q" | "Enter" => app.overlay.set(None),
            "Tab" => dom::trap_tab(dialog, &ev),
            k if dom::is_function_key(k) => {}
            _ => return,
        }
        ev.prevent_default();
    });
    on_cleanup(move || keys.remove());

    view! {
        <div class="backdrop" on:mousedown=move |_| app.overlay.set(None)>
            <div class="modal help" node_ref=dialog role="dialog" aria-label="Keys" on:mousedown=|ev| ev.stop_propagation()>
                <div class="modal-title">"Keys"</div>
                <dl class="keys">
                    {KEYS
                        .iter()
                        .map(|(k, what)| view! { <dt>{*k}</dt> <dd>{*what}</dd> })
                        .collect_view()}
                </dl>
                <div class="modal-note">"Mouse: click selects the row, double-click opens it, column headers sort."</div>
                <div class="modal-buttons">
                    <button class="primary" on:click=move |_| app.overlay.set(None)>"Close" <kbd>"Esc"</kbd></button>
                </div>
            </div>
        </div>
    }
}
