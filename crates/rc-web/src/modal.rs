//! Small dialogs: destination / name / mask prompts and delete confirmation.

use crate::app::App;
use crate::dom;
use leptos::ev;
use leptos::html::Input;
use leptos::prelude::*;
use rc_core::Location;

#[derive(Clone)]
pub enum ModalKind {
    Transfer { is_move: bool, sources: Vec<Location> },
    Mkdir,
    Mask { select: bool },
    Delete { targets: Vec<Location>, permanent: bool },
}

#[derive(Clone)]
pub struct Modal {
    pub title: String,
    /// Initial text of the input field; `None` for a plain yes/no confirmation.
    pub input: Option<String>,
    pub kind: ModalKind,
}

impl Modal {
    fn dangerous(&self) -> bool {
        matches!(self.kind, ModalKind::Delete { permanent: true, .. })
    }
}

#[component]
pub fn ModalView(app: App, modal: Modal) -> impl IntoView {
    let value = RwSignal::new(modal.input.clone().unwrap_or_default());
    let input_ref = NodeRef::<Input>::new();
    Effect::new(move |_| {
        if let Some(input) = input_ref.get() {
            let _ = input.focus();
            input.select();
        }
    });

    let confirm = move || {
        if let Some(modal) = app.modal.get_untracked() {
            app.modal.set(None);
            app.confirm(modal.kind, value.get_untracked());
        }
    };
    let cancel = move || app.modal.set(None);
    // Enter/Esc work whether or not the input has focus (a confirmation has no input).
    let keys = window_event_listener(ev::keydown, move |ev| {
        // A held Enter must not confirm and then act on the panel; IME composition owns Enter.
        if ev.repeat() || ev.is_composing() {
            ev.prevent_default();
            return;
        }
        match ev.key().as_str() {
            "Enter" => confirm(),
            "Escape" => cancel(),
            k if dom::is_function_key(k) => {}
            _ => return,
        }
        ev.prevent_default();
    });
    on_cleanup(move || keys.remove());

    let danger = modal.dangerous();
    view! {
        <div class="backdrop">
            <div class="modal" class:danger=danger>
                <div class="modal-title">{modal.title}</div>
                {modal.input.is_some().then(|| view! { <input class="modal-input" node_ref=input_ref bind:value=value spellcheck="false"/> })}
                <div class="modal-buttons">
                    <button class="primary" on:click=move |_| confirm()>
                        {if danger { "Delete" } else { "OK" }}
                        <kbd>"Enter"</kbd>
                    </button>
                    <button on:click=move |_| cancel()>"Cancel" <kbd>"Esc"</kbd></button>
                </div>
            </div>
        </div>
    }
}
