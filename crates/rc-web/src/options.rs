//! The options dialog (`o`): visible columns, hidden files and the F4 editor command.

use crate::app::App;
use crate::dom;
use leptos::ev;
use leptos::html::Div;
use leptos::prelude::*;
use rc_core::{Column, Panel};

/// Focusable controls of the dialog, in order, for ↑/↓ navigation.
const CONTROLS: &str = "input";

#[component]
pub fn OptionsView(app: App) -> impl IntoView {
    // Edits go to a draft; only Save publishes them. "Show hidden files" is the exception:
    // like the terminal UI it applies at once, and Cancel puts it back.
    let draft = RwSignal::new(app.settings.get_untracked());
    let hidden_before = app.panels.map(|p| p.with_untracked(|p| p.show_hidden));
    let dialog = NodeRef::<Div>::new();
    // Why the server refused to save (e.g. an editor that is not a program), shown here.
    let error = RwSignal::new(None::<String>);
    Effect::new(move |_| {
        if let Some(dialog) = dialog.get() {
            dom::move_focus(&dialog, CONTROLS, 0);
        }
    });

    let save = move || {
        let mut settings = draft.get_untracked();
        settings.gui_editor = settings.gui_editor.trim().to_string();
        app.save_settings(settings, move |result| match result {
            Ok(()) => {
                app.options_open.set(false);
                app.info("Options saved");
            }
            Err(e) => error.set(Some(e)),
        });
    };
    let cancel = move || {
        for (panel, show) in app.panels.into_iter().zip(hidden_before) {
            if panel.with_untracked(|p| p.show_hidden) != show {
                panel.update(Panel::toggle_hidden);
            }
        }
        app.options_open.set(false);
    };
    let keys = window_event_listener(ev::keydown, move |ev| {
        if ev.is_composing() {
            return;
        }
        let key = ev.key();
        match key.as_str() {
            // A held Enter must save once, not again on the panel behind.
            "Enter" if ev.repeat() => {}
            // Let a focused button take Enter as a click.
            "Enter" if dom::is_button(ev.target()) => return,
            "Enter" => save(),
            "Escape" => cancel(),
            "Tab" => dom::trap_tab(dialog, &ev),
            "ArrowDown" | "ArrowUp" => {
                if let Some(dialog) = dialog.get_untracked() {
                    dom::move_focus(&dialog, CONTROLS, if key == "ArrowDown" { 1 } else { -1 });
                }
            }
            k if dom::is_function_key(k) => {}
            _ => return,
        }
        ev.prevent_default();
    });
    on_cleanup(move || keys.remove());

    let column_box = |c: Column| {
        view! {
            <label class="opt-check">
                <input
                    type="checkbox"
                    prop:checked=move || draft.with(|s| s.shows(c))
                    on:change=move |_| draft.update(|s| s.toggle_column(c))
                />
                {c.label()}
            </label>
        }
    };

    view! {
        <div class="backdrop">
            <div class="modal options" node_ref=dialog role="dialog" aria-label="Options">
                <div class="modal-title">"Options"</div>
                <fieldset class="opt-columns">
                    <legend>"Columns"</legend>
                    {Column::ALL.map(column_box).collect_view()}
                </fieldset>
                <label class="opt-check">
                    <input
                        type="checkbox"
                        prop:checked=move || draft.with(|s| s.show_hidden)
                        on:change=move |ev| {
                            let show = event_target_checked(&ev);
                            draft.update(|s| s.show_hidden = show);
                            app.show_hidden(show);
                        }
                    />
                    "Show hidden files"
                </label>
                <label class="opt-field">
                    "Editor (F4)"
                    <input
                        type="text"
                        class="modal-input"
                        spellcheck="false"
                        placeholder="Empty: open with the system default app"
                        prop:value=move || draft.with(|s| s.gui_editor.clone())
                        on:input=move |ev| draft.update(|s| s.gui_editor = event_target_value(&ev))
                    />
                </label>
                <div class="opt-note">
                    "The browser UI starts the editor without a terminal, so use a GUI command such as "
                    <code>"code"</code> " or " <code>"open -t"</code> ", not " <code>"vim"</code> "."
                </div>
                <div class="opt-row">
                    <label class="opt-field">
                        "Claude map model (i)"
                        <input
                            type="text"
                            class="modal-input"
                            spellcheck="false"
                            placeholder="sonnet"
                            prop:value=move || draft.with(|s| s.claude_model.clone())
                            on:input=move |ev| draft.update(|s| s.claude_model = event_target_value(&ev))
                        />
                    </label>
                    <label class="opt-field">
                        "Claude command"
                        <input
                            type="text"
                            class="modal-input"
                            spellcheck="false"
                            placeholder="claude"
                            prop:value=move || draft.with(|s| s.claude_command.clone())
                            on:input=move |ev| draft.update(|s| s.claude_command = event_target_value(&ev))
                        />
                    </label>
                </div>
                <div class="opt-note">
                    "Maps run your Claude Code with read-only tools; the file contents they read go to Claude. "
                    "Model: " <code>"haiku"</code> ", " <code>"sonnet"</code> " or " <code>"opus"</code> "."
                </div>
                {move || error.get().map(|e| view! { <div class="modal-error" role="alert">{e}</div> })}
                <div class="modal-buttons">
                    <button class="primary" on:click=move |_| save()>
                        "Save"
                        <kbd>"Enter"</kbd>
                    </button>
                    <button on:click=move |_| cancel()>"Cancel" <kbd>"Esc"</kbd></button>
                </div>
            </div>
        </div>
    }
}
