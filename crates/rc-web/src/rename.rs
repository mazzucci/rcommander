//! Shift+F6 / M / Ctrl+M: rename many entries of a folder by a rule (mask, search and
//! replace, case, counter), previewing every new name before anything is renamed.

use crate::app::App;
use crate::dom;
use leptos::ev;
use leptos::html::{Div, Input};
use leptos::prelude::*;
use rc_core::Location;
use rc_core::api::FileOp;
use rc_core::rename::{NameCase, Planned, RenameRule, plan};

/// What the dialog renames: entries of one folder, and every name in it.
#[derive(Clone)]
pub struct RenameTarget {
    pub folder: Location,
    pub entries: Vec<(String, bool)>,
    pub existing: Vec<String>,
}

/// Placeholders offered as buttons next to the mask.
const TOKENS: [(&str, &str); 4] = [
    ("[N]", "name without extension"),
    ("[E]", "extension"),
    ("[C]", "counter"),
    ("[P]", "folder name"),
];

#[component]
pub fn RenameView(app: App, target: RenameTarget) -> impl IntoView {
    let dialog = NodeRef::<Div>::new();
    let mask_input = NodeRef::<Input>::new();
    let rule = RwSignal::new(RenameRule::default());
    let parent = target.folder.file_name().unwrap_or_default().to_string();
    let planned = Memo::new(move |_| rule.with(|r| plan(r, &target.entries, &parent, &target.existing)));
    let problems = move || planned.with(|p| p.iter().filter(|p| p.problem.is_some()).count());
    let changes = move || planned.with(|p| p.iter().filter(|p| p.changes()).count());
    let ready = move || problems() == 0 && changes() > 0;

    Effect::new(move |_| {
        if let Some(input) = mask_input.get() {
            let _ = input.focus();
            // Cursor before the extension, where most edits go.
            let at = input.value().find(".[E]").unwrap_or(input.value().len()) as u32;
            let _ = input.set_selection_range(at, at);
        }
    });

    let close = move || app.rename.set(None);
    let folder = target.folder.clone();
    let apply = move || {
        if !ready() {
            return;
        }
        let renames = planned.with_untracked(|p| {
            p.iter()
                .filter(|p| p.changes())
                .map(|p| (p.from.clone(), p.to.clone()))
                .collect()
        });
        close();
        app.run_op(
            FileOp::RenameMany {
                folder: folder.clone(),
                renames,
            },
            None,
        );
    };
    let apply_key = apply.clone();
    // Insert a placeholder at the mask's cursor.
    let insert = move |token: &'static str| {
        let Some(input) = mask_input.get_untracked() else {
            return;
        };
        let value = input.value();
        // Selection offsets count UTF-16 units; turn them into byte offsets.
        let byte = |units: Option<u32>| {
            let units = units.map_or(usize::MAX, |n| n as usize);
            let mut seen = 0;
            value
                .char_indices()
                .find(|(_, c)| {
                    let past = seen >= units;
                    seen += c.len_utf16();
                    past
                })
                .map_or(value.len(), |(i, _)| i)
        };
        let at = byte(input.selection_start().ok().flatten());
        let end = byte(input.selection_end().ok().flatten()).max(at);
        let new = format!("{}{token}{}", &value[..at], &value[end..]);
        rule.update(|r| r.mask = new.clone());
        input.set_value(&new);
        let caret = new[..at + token.len()].encode_utf16().count() as u32;
        let _ = input.focus();
        let _ = input.set_selection_range(caret, caret);
    };

    let keys = window_event_listener(ev::keydown, move |ev| {
        if ev.repeat() && ev.key() == "Enter" {
            ev.prevent_default();
            return;
        }
        match ev.key().as_str() {
            "Escape" => close(),
            "Tab" => dom::trap_tab(dialog, &ev),
            "Enter" if dom::is_button(ev.target()) => return,
            "Enter" => apply_key(),
            k if dom::is_function_key(k) => {}
            _ => return,
        }
        ev.prevent_default();
    });
    on_cleanup(move || keys.remove());

    let text_field = move |id: &'static str,
                           label: &'static str,
                           get: fn(&RenameRule) -> &String,
                           set: fn(&mut RenameRule, String)| {
        view! {
            <label for=id>{label}</label>
            <input
                id=id
                class="modal-input"
                spellcheck="false"
                autocomplete="off"
                prop:value=move || rule.with(|r| get(r).clone())
                on:input=move |ev| rule.update(|r| set(r, event_target_value(&ev)))
            />
        }
    };
    let number = move |value: String| value.trim().parse::<u64>().ok();

    let rows = move || {
        planned.with(|p| {
            p.iter()
                .map(|Planned { from, to, problem }| {
                    let same = from == to;
                    view! {
                        <tr class:same=same class:bad=problem.is_some()>
                            <td class="rn-from" title=from.clone()>{from.clone()}</td>
                            <td class="rn-arrow">"→"</td>
                            <td class="rn-to" title=to.clone()>{to.clone()}</td>
                            <td class="rn-problem">{problem.clone()}</td>
                        </tr>
                    }
                })
                .collect_view()
        })
    };
    let status = move || {
        let (n, bad) = (changes(), problems());
        if bad > 0 {
            view! { <span class="error">{format!("{bad} problem{} · nothing will be renamed", if bad == 1 { "" } else { "s" })}</span> }.into_any()
        } else if n == 0 {
            view! { <span>"No name changes yet"</span> }.into_any()
        } else {
            view! { <span class="ok">{format!("{n} to rename")}</span> }.into_any()
        }
    };
    let count = planned.with_untracked(Vec::len);
    let title = format!(
        "Rename {count} {} in {}",
        if count == 1 { "item" } else { "items" },
        target.folder
    );

    view! {
        <div class="backdrop">
            <div class="modal rename" node_ref=dialog role="dialog" aria-label="Multi-rename">
                <div class="modal-title">{title}</div>
                <div class="rn-fields">
                    <label for="rn-mask">"Mask"</label>
                    <div class="rn-mask">
                        <input
                            id="rn-mask"
                            class="modal-input"
                            node_ref=mask_input
                            spellcheck="false"
                            autocomplete="off"
                            prop:value=move || rule.with(|r| r.mask.clone())
                            on:input=move |ev| rule.update(|r| r.mask = event_target_value(&ev))
                        />
                        {TOKENS
                            .into_iter()
                            .map(|(token, what)| {
                                view! {
                                    <button class="token" title=what tabindex="-1" on:mousedown=|ev| ev.prevent_default() on:click=move |_| insert(token)>
                                        {token}
                                    </button>
                                }
                            })
                            .collect_view()}
                    </div>
                    {text_field("rn-search", "Search", |r| &r.search, |r, v| r.search = v)}
                    {text_field("rn-replace", "Replace", |r| &r.replace, |r, v| r.replace = v)}
                    <label for="rn-case">"Case"</label>
                    <div class="rn-row">
                        <select
                            id="rn-case"
                            class="modal-input"
                            on:change=move |ev| {
                                let i: usize = event_target_value(&ev).parse().unwrap_or(0);
                                rule.update(|r| r.case = NameCase::ALL[i.min(NameCase::ALL.len() - 1)]);
                            }
                        >
                            {NameCase::ALL
                                .into_iter()
                                .enumerate()
                                .map(|(i, c)| view! { <option value=i.to_string()>{c.label()}</option> })
                                .collect_view()}
                        </select>
                        <label for="rn-start">"Counter from"</label>
                        <input
                            id="rn-start"
                            class="modal-input num"
                            type="number"
                            min="0"
                            prop:value=move || rule.with(|r| r.counter_start.to_string())
                            on:blur=move |ev| show_value(&ev, rule.try_with_untracked(|r| r.counter_start))
                            on:input=move |ev| {
                                if let Some(n) = number(event_target_value(&ev)) {
                                    rule.update(|r| r.counter_start = n);
                                }
                            }
                        />
                        <label for="rn-digits">"Digits"</label>
                        <input
                            id="rn-digits"
                            class="modal-input num"
                            type="number"
                            min="1"
                            max="20"
                            prop:value=move || rule.with(|r| r.counter_digits.to_string())
                            on:blur=move |ev| show_value(&ev, rule.try_with_untracked(|r| r.counter_digits as u64))
                            on:input=move |ev| {
                                if let Some(n) = number(event_target_value(&ev)) {
                                    rule.update(|r| r.counter_digits = (n as usize).min(20));
                                }
                            }
                        />
                    </div>
                </div>
                <div class="rn-status">{status}</div>
                <div class="rn-preview">
                    <table>
                        <tbody>{rows}</tbody>
                    </table>
                </div>
                <div class="modal-buttons">
                    <span class="modal-hint">"Enter rename · Esc cancel"</span>
                    <button on:click=move |_| close()>"Cancel"</button>
                    <button class="primary" disabled=move || !ready() on:click=move |_| apply()>"Rename" <kbd>"Enter"</kbd></button>
                </div>
            </div>
        </div>
    }
}

/// Leaving a number field shows the value in use (an empty or invalid entry is ignored).
/// `None`: the dialog is closing (removing a focused field blurs it); nothing to do.
fn show_value(ev: &leptos::ev::FocusEvent, value: Option<u64>) {
    if let Some(value) = value {
        event_target::<web_sys::HtmlInputElement>(ev).set_value(&value.to_string());
    }
}
