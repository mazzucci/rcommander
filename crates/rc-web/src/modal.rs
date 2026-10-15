//! Small dialogs: destination / name / mask prompts, delete confirmation and the
//! "already exists" question of copy and move.

use crate::app::App;
use crate::dom;
use leptos::ev;
use leptos::html::{Div, Input};
use leptos::prelude::*;
use rc_core::Location;
use rc_core::api::{FileOp, OnConflict};

#[derive(Clone)]
pub enum ModalKind {
    Transfer {
        is_move: bool,
        sources: Vec<Location>,
    },
    Mkdir,
    Mask {
        select: bool,
    },
    Delete {
        targets: Vec<Location>,
        permanent: bool,
    },
    /// A copy or move would meet `existing` destination paths.
    Conflict {
        op: FileOp,
        existing: Vec<String>,
    },
}

#[derive(Clone)]
pub struct Modal {
    pub title: String,
    /// Initial text of the input field; `None` for dialogs without one.
    pub input: Option<String>,
    pub kind: ModalKind,
}

/// Items listed by name in a delete dialog before "and N more".
const LISTED: usize = 6;

/// Choices of the conflict dialog: key, label, policy. Same keys as the terminal UI;
/// 'n' is "no" (cancel), as in every other dialog. Folders are merged either way.
const CONFLICT_CHOICES: [(&str, &str, OnConflict); 3] = [
    ("o", "Replace existing", OnConflict::Overwrite),
    ("u", "Replace older", OnConflict::IfNewer),
    ("s", "Keep existing", OnConflict::Skip),
];

#[component]
pub fn ModalView(app: App, modal: Modal) -> impl IntoView {
    let value = RwSignal::new(modal.input.clone().unwrap_or_default());
    let dialog = NodeRef::<Div>::new();
    let input_ref = NodeRef::<Input>::new();
    let is_conflict = matches!(modal.kind, ModalKind::Conflict { .. });
    // Focus the input; dialogs without one focus their safest button.
    Effect::new(move |_| {
        if let Some(input) = input_ref.get() {
            let _ = input.focus();
            input.select();
        } else if let Some(dialog) = dialog.get() {
            dom::move_focus(&dialog, "button.default", 0);
        }
    });

    let close = move || app.modal.set(None);
    let confirm = move || {
        if let Some(modal) = app.modal.get_untracked() {
            close();
            app.confirm(modal.kind, value.get_untracked());
        }
    };
    let choose = move |policy: OnConflict| {
        if let Some(Modal {
            kind: ModalKind::Conflict { op, .. },
            ..
        }) = app.modal.get_untracked()
        {
            close();
            app.resolve_conflict(op, policy);
        }
    };
    let keys = window_event_listener(ev::keydown, move |ev| {
        // A held Enter must not confirm and then act on the panel; IME composition owns Enter.
        if ev.repeat() || ev.is_composing() {
            ev.prevent_default();
            return;
        }
        let key = ev.key();
        let in_field = dom::is_text_field(ev.target());
        match key.as_str() {
            "Escape" => close(),
            "n" | "N" if is_conflict && !in_field => close(),
            "Tab" => dom::trap_tab(dialog, &ev),
            // A focused button takes Enter (and Space) as a click.
            "Enter" | " " if dom::is_button(ev.target()) => return,
            "Enter" if !is_conflict => confirm(),
            k if is_conflict && !in_field => match CONFLICT_CHOICES.iter().find(|(c, ..)| k.eq_ignore_ascii_case(c)) {
                Some(&(_, _, policy)) => choose(policy),
                None => return,
            },
            k if dom::is_function_key(k) => {}
            _ => return,
        }
        ev.prevent_default();
    });
    on_cleanup(move || keys.remove());

    let danger = matches!(modal.kind, ModalKind::Delete { .. });
    let details = match &modal.kind {
        ModalKind::Delete { targets, .. } => Some(item_list(targets.iter().filter_map(Location::file_name))),
        ModalKind::Conflict { existing, .. } if existing.len() > 1 => {
            Some(item_list(existing.iter().map(|p| p.rsplit('/').next().unwrap_or(p))))
        }
        _ => None,
    };
    let buttons = if is_conflict {
        CONFLICT_CHOICES
            .iter()
            .map(|&(key, label, policy)| {
                view! {
                    <button class:default=policy == OnConflict::Skip on:click=move |_| choose(policy)>
                        {label}
                        <kbd>{key}</kbd>
                    </button>
                }
            })
            .collect_view()
            .into_any()
    } else {
        view! {
            <button class="primary default" on:click=move |_| confirm()>
                {if danger { "Delete" } else { "OK" }}
                <kbd>"Enter"</kbd>
            </button>
        }
        .into_any()
    };

    let label = modal.title.clone();
    view! {
        <div class="backdrop">
            <div class="modal" class:danger=danger node_ref=dialog role="dialog" aria-label=label>
                <div class="modal-title">{modal.title}</div>
                {details}
                {modal.input.is_some().then(|| view! { <input class="modal-input" node_ref=input_ref bind:value=value spellcheck="false"/> })}
                <div class="modal-buttons">
                    {buttons}
                    <button on:click=move |_| close()>"Cancel" <kbd>"Esc"</kbd></button>
                </div>
            </div>
        </div>
    }
}

/// Names of the items a dialog is about, shortened to the first few.
fn item_list<'a>(names: impl Iterator<Item = &'a str>) -> AnyView {
    let names: Vec<String> = names.map(str::to_owned).collect();
    let more = names.len().saturating_sub(LISTED);
    view! {
        <ul class="modal-items">
            {names.into_iter().take(LISTED).map(|n| view! { <li>{n}</li> }).collect_view()}
            {(more > 0).then(|| view! { <li class="more">{format!("and {more} more")}</li> })}
        </ul>
    }
    .into_any()
}
