//! Alt+F7 / Shift+F: find files under the active panel's folder by name mask and,
//! optionally, text inside. The form and the last results live in [`App::find`], so
//! viewing a hit (F3) and coming back keeps them.
//!
//! Each part of the form is its own signal: typing never rebuilds the results, and a
//! cursor move only touches the two rows it leaves and enters.

use crate::app::App;
use crate::{api, dom};
use leptos::ev;
use leptos::html::{Div, Input};
use leptos::prelude::*;
use leptos::task::spawn_local;
use rc_core::Location;
use rc_core::api::{FindHit, FindRequest, FindResult, Progress};
use std::time::Duration;

/// The search form and its last results.
#[derive(Clone, Copy)]
pub struct FindForm {
    /// Folder searched; the active panel's when the dialog opened.
    pub root: RwSignal<Option<Location>>,
    pub name: RwSignal<String>,
    pub contains: RwSignal<String>,
    pub hidden: RwSignal<bool>,
    pub result: RwSignal<Option<FindResult>>,
    pub error: RwSignal<Option<String>>,
    pub cursor: RwSignal<usize>,
    /// The search in flight. Owned by the app, not the dialog, so its reply and the
    /// progress timer can outlive a closed dialog.
    running: RwSignal<Option<Running>>,
    /// What the shown results were searched with.
    searched: StoredValue<Option<FindRequest>>,
}

impl FindForm {
    pub fn new() -> Self {
        FindForm {
            root: RwSignal::new(None),
            name: RwSignal::new(String::new()),
            contains: RwSignal::new(String::new()),
            hidden: RwSignal::new(false),
            result: RwSignal::new(None),
            error: RwSignal::new(None),
            cursor: RwSignal::new(0),
            running: RwSignal::new(None),
            searched: StoredValue::new(None),
        }
    }

    /// Open on `root`: the fields stay, results of another folder go.
    pub fn open(self, root: Location, hidden: bool) {
        if self.root.get_untracked().as_ref() != Some(&root) {
            self.root.set(Some(root));
            self.hidden.set(hidden);
            self.result.set(None);
            self.error.set(None);
            self.cursor.set(0);
        }
    }
}

/// How often a running search is asked for progress.
const POLL: Duration = Duration::from_millis(250);

/// A search in flight: its job id and the latest progress.
#[derive(Clone)]
struct Running {
    job: String,
    progress: Option<Progress>,
}

#[component]
pub fn FindView(app: App) -> impl IntoView {
    let form = app.find;
    let dialog = NodeRef::<Div>::new();
    let name_input = NodeRef::<Input>::new();
    let running = form.running;
    let seq = StoredValue::new(0u64);

    // Start with the name field, or the results if there are some to come back to.
    Effect::new(move |_| {
        if let Some(input) = name_input.get() {
            if form
                .result
                .with_untracked(|r| r.as_ref().is_some_and(|r| !r.hits.is_empty()))
            {
                focus_results();
            } else {
                let _ = input.focus();
                input.select();
            }
        }
    });

    let root_text = move || {
        form.root
            .with(|r| r.as_ref().map(Location::to_string).unwrap_or_default())
    };
    let hits_len = move || form.result.with_untracked(|r| r.as_ref().map_or(0, |r| r.hits.len()));
    let hit_at = move |i: usize| {
        form.result
            .with_untracked(|r| r.as_ref().and_then(|r| r.hits.get(i).cloned()))
    };
    let current = move || hit_at(form.cursor.get_untracked());
    let on_cursor = Selector::new(move || form.cursor.get());

    let cancel = move || {
        if let Some(r) = running.get_untracked() {
            spawn_local(async move {
                let _ = api::cancel(r.job).await;
            });
        }
    };
    // Closing the dialog stops a search still running; its late reply is dropped.
    on_cleanup(move || {
        cancel();
        running.set(None);
    });

    let poll = move |job: String| {
        fn again(running: RwSignal<Option<Running>>, job: String) {
            set_timeout(
                move || {
                    if running.with_untracked(|r| r.as_ref().is_none_or(|r| r.job != job)) {
                        return;
                    }
                    spawn_local(async move {
                        if let Ok(status) = api::job(job.clone()).await {
                            running.try_update(|r| {
                                if let Some(r) = r.as_mut().filter(|r| r.job == job) {
                                    r.progress = status.progress.or(r.progress.take());
                                }
                            });
                        }
                        again(running, job);
                    });
                },
                POLL,
            );
        }
        again(running, job);
    };

    let request = move || {
        Some(FindRequest {
            root: form.root.get_untracked()?,
            name: form.name.get_untracked().trim().to_string(),
            contains: form.contains.get_untracked(),
            hidden: form.hidden.get_untracked(),
        })
    };
    let search = move || {
        if running.with_untracked(Option::is_some) {
            return;
        }
        let Some(req) = request() else { return };
        form.searched.set_value(Some(req.clone()));
        let n = seq.get_value() + 1;
        seq.set_value(n);
        let job = format!("find-{n}-{:x}", (js_sys::Math::random() * u32::MAX as f64) as u32);
        running.set(Some(Running {
            job: job.clone(),
            progress: None,
        }));
        form.result.set(None);
        form.error.set(None);
        form.cursor.set(0);
        poll(job.clone());
        spawn_local(async move {
            let result = api::find(req, job.clone()).await;
            // Only the search in flight counts (not one stopped by closing the dialog).
            if running.with_untracked(|r| r.as_ref().is_none_or(|r| r.job != job)) {
                return;
            }
            running.set(None);
            match result {
                Ok(r) => {
                    let any = !r.hits.is_empty();
                    form.result.try_set(Some(r));
                    if any {
                        focus_results();
                    }
                }
                Err(e) => drop(form.error.try_set(Some(e))),
            }
        });
    };

    let go_to = move |hit: FindHit| {
        let loc = Location::local(&hit.path);
        let (Some(parent), Some(name)) = (loc.parent(), loc.file_name()) else {
            return;
        };
        app.find_open.set(false);
        app.go_to(parent, name.to_string());
    };
    let view_hit = move |hit: FindHit| {
        if !hit.is_dir {
            app.find_open.set(false);
            app.view_from_find(Location::local(&hit.path), hit.line);
        }
    };
    let move_cursor = move |delta: isize| {
        let len = hits_len();
        if len == 0 {
            return;
        }
        form.cursor.update(|c| *c = c.saturating_add_signed(delta).min(len - 1));
        request_animation_frame(|| {
            if let Some(el) = dom::by_id("find-cursor") {
                dom::scroll_into_view(&el, web_sys::ScrollLogicalPosition::Nearest);
            }
        });
    };

    let keys = window_event_listener(ev::keydown, move |ev| {
        let key = ev.key();
        if ev.repeat() && key == "Enter" {
            ev.prevent_default();
            return;
        }
        let in_field = dom::is_text_field(ev.target());
        match key.as_str() {
            "Escape" if running.with_untracked(Option::is_some) => cancel(),
            "Escape" => app.find_open.set(false),
            "Tab" => dom::trap_tab(dialog, &ev),
            "Enter" | " " if dom::is_button(ev.target()) => return,
            // Enter in a field searches, unless these exact results are already shown
            // (e.g. Enter pressed again right after they arrived): then it goes to the hit.
            "Enter" if in_field && (hits_len() == 0 || form.searched.get_value() != request()) => search(),
            "Enter" => {
                if let Some(hit) = current() {
                    go_to(hit);
                }
            }
            "F3" => {
                if let Some(hit) = current() {
                    view_hit(hit);
                }
            }
            "ArrowDown" => move_cursor(1),
            "ArrowUp" => move_cursor(-1),
            "PageDown" => move_cursor(10),
            "PageUp" => move_cursor(-10),
            "Home" if !in_field => move_cursor(isize::MIN / 2),
            "End" if !in_field => move_cursor(isize::MAX / 2),
            k if dom::is_function_key(k) => {}
            _ => return,
        }
        ev.prevent_default();
    });
    on_cleanup(move || keys.remove());

    let status = move || {
        if let Some(r) = running.get() {
            let (dir, n) = r.progress.map_or((String::new(), 0), |p| (p.path, p.done));
            let root = root_text();
            let rel = dir
                .strip_prefix(&root)
                .unwrap_or(&dir)
                .trim_start_matches('/')
                .to_string();
            let at = if rel.is_empty() {
                String::new()
            } else {
                format!(" {rel}")
            };
            return view! { <span class="find-status busy">{format!("Searching{at} · {n} found")}</span> }.into_any();
        }
        if let Some(e) = form.error.get() {
            return view! { <span class="find-status error">{e}</span> }.into_any();
        }
        form.result.with(|r| {
            let Some(r) = r else {
                return view! {
                    <span class="find-status">
                        "Masks like " <code>"*.rs;*.toml"</code> "; a plain word matches inside names. "
                        "Text is case-sensitive only if it has capitals."
                    </span>
                }
                .into_any();
            };
            let mut s = format!("{} found", r.hits.len());
            if r.truncated {
                s += " (limit reached)";
            }
            if r.cancelled {
                s += " · stopped";
            }
            if r.unreadable > 0 {
                s += &format!(" · {} unreadable", r.unreadable);
            }
            view! { <span class="find-status">{s}</span> }.into_any()
        })
    };

    let rows = move || {
        let root = root_text();
        form.result.with(|r| {
            r.as_ref().map(|r| {
                r.hits
                    .iter()
                    .enumerate()
                    .map(|(i, hit)| {
                        let rel = hit
                            .path
                            .strip_prefix(&root)
                            .unwrap_or(&hit.path)
                            .trim_start_matches('/');
                        let rel = if hit.is_dir { format!("{rel}/") } else { rel.to_string() };
                        let go = hit.clone();
                        let sel = on_cursor.clone();
                        let at = Signal::derive(move || sel.selected(&i));
                        view! {
                            <li
                                role="option"
                                id=move || at.get().then_some("find-cursor")
                                class:cursor=at
                                class:dir=hit.is_dir
                                aria-selected=move || at.get().to_string()
                                title=hit.path.clone()
                                on:click=move |_| form.cursor.set(i)
                                on:dblclick=move |_| go_to(go.clone())
                            >
                                <span class="find-path">{rel}</span>
                                {hit.line.map(|n| view! { <span class="find-line">{format!(":{n}")}</span> })}
                                {hit.text.clone().map(|t| view! { <span class="find-text">{t}</span> })}
                            </li>
                        }
                    })
                    .collect_view()
            })
        })
    };

    view! {
        <div class="backdrop">
            <div class="modal find" node_ref=dialog role="dialog" aria-label="Find files">
                <div class="modal-title">"Find files in " <span class="find-root">{root_text}</span></div>
                <div class="find-fields">
                    <label for="find-name">"Name"</label>
                    <input
                        id="find-name"
                        class="modal-input"
                        node_ref=name_input
                        placeholder="*.rs;*.toml — empty: all"
                        spellcheck="false"
                        autocomplete="off"
                        prop:value=form.name
                        on:input=move |ev| form.name.set(event_target_value(&ev))
                    />
                    <label for="find-contains">"Contains"</label>
                    <input
                        id="find-contains"
                        class="modal-input"
                        placeholder="text inside files — empty: names only"
                        spellcheck="false"
                        autocomplete="off"
                        prop:value=form.contains
                        on:input=move |ev| form.contains.set(event_target_value(&ev))
                    />
                    <span></span>
                    <label class="opt-check">
                        <input
                            type="checkbox"
                            prop:checked=form.hidden
                            on:change=move |ev| form.hidden.set(event_target_checked(&ev))
                        />
                        "Search hidden files and folders"
                    </label>
                </div>
                <div class="find-status-row">{status}</div>
                <ol id="find-results" class="find-results" tabindex="0" role="listbox" aria-label="Results">
                    {rows}
                </ol>
                <div class="modal-buttons">
                    <span class="modal-hint">"Enter go to · F3 view · Esc close"</span>
                    <Show
                        when=move || running.with(Option::is_some)
                        fallback=move || view! { <button class="primary" on:click=move |_| search()>"Search" <kbd>"Enter"</kbd></button> }
                    >
                        <button on:click=move |_| cancel()>"Stop" <kbd>"Esc"</kbd></button>
                    </Show>
                    <button on:click=move |_| app.find_open.set(false)>"Close"</button>
                </div>
            </div>
        </div>
    }
}

/// Put the keyboard on the results list.
/// (By id: the dialog may have closed by the time this runs.)
fn focus_results() {
    request_animation_frame(|| {
        if let Some(list) =
            dom::by_id("find-results").and_then(|el| wasm_bindgen::JsCast::dyn_into::<web_sys::HtmlElement>(el).ok())
        {
            let _ = list.focus();
        }
    });
}
