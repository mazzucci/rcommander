//! `i`: the Claude map panel. The panel that is not active shows Claude's summary of a
//! file or folder with links into it: click (or Enter) shows the target in the panel,
//! the line badge (or F3) opens the viewer there. Tab moves the keyboard into the map.

use crate::api;
use crate::app::App;
use leptos::ev;
use leptos::html::{Div, Input};
use leptos::prelude::*;
use leptos::task::spawn_local;
use rc_core::Location;
use rc_core::api::{FileMap, MapExchange, MapItem, MapQuestion};
use std::time::Duration;

/// What the map panel shows: a target and a request number, so `I` can ask again for
/// the same target.
#[derive(Clone, PartialEq)]
pub struct MapTarget {
    pub location: Location,
    pub refresh: bool,
    pub seq: u64,
}

#[derive(Clone)]
enum State {
    Loading(u64),
    Ready(FileMap),
    Failed(String),
}

const POLL: Duration = Duration::from_millis(500);

#[component]
pub fn MapView(app: App, side: usize, target: MapTarget) -> impl IntoView {
    let state = RwSignal::new(State::Loading(0));
    let cursor = RwSignal::new(0usize);
    let body = NodeRef::<Div>::new();
    let job = format!(
        "map-{}-{:x}",
        target.seq,
        (js_sys::Math::random() * u32::MAX as f64) as u32
    );
    let location = target.location.clone();

    // Ask the server; poll for the seconds spent; stop Claude if the panel goes away.
    let done = StoredValue::new(false);
    {
        let (job, location) = (job.clone(), location.clone());
        spawn_local(async move {
            let result = api::map(location, target.refresh, job).await;
            done.try_set_value(true);
            // Showing it again (e.g. on the other side) must not ask Claude again.
            app.map.try_update_untracked(|m| {
                if let Some(m) = m {
                    m.refresh = false;
                }
            });
            state.try_set(match result {
                Ok(map) => State::Ready(map),
                Err(e) => State::Failed(e),
            });
        });
    }
    fn tick(state: RwSignal<State>, done: StoredValue<bool>, job: String) {
        set_timeout(
            move || {
                if done.try_get_value() != Some(false) {
                    return;
                }
                spawn_local(async move {
                    if let Ok(status) = api::job(job.clone()).await
                        && let Some(p) = status.progress
                    {
                        state.try_update(|s| {
                            if let State::Loading(secs) = s {
                                *secs = p.done;
                            }
                        });
                    }
                    tick(state, done, job);
                });
            },
            POLL,
        );
    }
    tick(state, done, job.clone());
    {
        let job = job.clone();
        on_cleanup(move || {
            if done.try_get_value() == Some(false) {
                spawn_local(async move {
                    let _ = api::cancel(job).await;
                });
            }
        });
    }

    // Questions about the map: the thread so far, the one Claude is answering (with
    // the seconds spent), and the last error.
    let thread = RwSignal::new(Vec::<MapExchange>::new());
    let pending = RwSignal::new(None::<(String, u64)>);
    let ask_error = RwSignal::new(None::<String>);
    let ask_input = NodeRef::<Input>::new();
    let ask_job = StoredValue::new(None::<String>);
    let ask_seq = StoredValue::new(0u64);

    // Tab from the panels (App::handle_key) focuses `.map-body`.
    let map_links = Memo::new(move |_| {
        state.with(|s| match s {
            State::Ready(m) => m
                .sections
                .iter()
                .flat_map(|s| &s.items)
                .filter(|i| i.path.is_some())
                .cloned()
                .collect(),
            _ => Vec::<MapItem>::new(),
        })
    });
    let links = Memo::new(move |_| {
        let mut all = map_links.get();
        thread.with(|t| {
            all.extend(
                t.iter()
                    .flat_map(|x| &x.answer.links)
                    .filter(|i| i.path.is_some())
                    .cloned(),
            )
        });
        all
    });
    let follow = move |item: &MapItem, view: bool| {
        let Some(path) = &item.path else { return };
        let loc = Location::local(path);
        match (view && !item.is_dir, loc.parent(), loc.file_name()) {
            (true, ..) => app.view_at(loc.clone(), item.line),
            (false, Some(parent), Some(name)) => app.go_to(parent, name.to_string()),
            _ => {}
        }
    };
    let on_key = move |ev: ev::KeyboardEvent| {
        let n = links.with_untracked(Vec::len);
        let current = move || links.with_untracked(|l| l.get(cursor.get_untracked()).cloned());
        match ev.key().as_str() {
            "ArrowDown" => cursor.update(|c| *c = (*c + 1).min(n.saturating_sub(1))),
            "ArrowUp" => cursor.update(|c| *c = c.saturating_sub(1)),
            "PageDown" => cursor.update(|c| *c = (*c + 10).min(n.saturating_sub(1))),
            "PageUp" => cursor.update(|c| *c = c.saturating_sub(10)),
            "Home" => cursor.set(0),
            "End" => cursor.set(n.saturating_sub(1)),
            "Enter" => {
                if let Some(item) = current() {
                    follow(&item, false);
                }
            }
            "F3" | "3" => {
                if let Some(item) = current() {
                    follow(&item, true);
                }
            }
            "a" => {
                if let Some(input) = ask_input.get_untracked() {
                    let _ = input.focus();
                }
            }
            // Back to the panels (focus may be on the map or on a clicked link in it).
            "Tab" | "Escape" => crate::dom::blur_active(),
            _ => return,
        }
        ev.prevent_default();
        // The panels' key handler must not act on these too.
        ev.stop_propagation();
        request_animation_frame(|| {
            if let Some(el) = crate::dom::by_id("map-cursor") {
                crate::dom::scroll_into_view(&el, web_sys::ScrollLogicalPosition::Nearest);
            }
        });
    };

    let name = location.file_name().unwrap_or("/").to_string();
    let title = format!("Claude map: {name}");
    let base = location.to_string();
    let content = move || {
        state.with(|s| match s {
            State::Loading(secs) => view! {
                <div class="qv-msg">
                    <span class="map-loading">{format!("Claude is reading {name}… {secs} s")}</span>
                    <span class="qv-sub">"i closes (and stops it)"</span>
                </div>
            }
            .into_any(),
            State::Failed(e) => view! {
                <div class="qv-msg">
                    <span class="error">{e.clone()}</span>
                    <span class="qv-sub">"I tries again · i closes"</span>
                </div>
            }
            .into_any(),
            State::Ready(m) => {
                // Links show paths relative to the folder mapped (or the file's folder).
                let base = if m.is_dir {
                    base.clone()
                } else {
                    location.parent().map_or(base.clone(), |p| p.to_string())
                };
                let mut k = 0;
                let sections = m
                    .sections
                    .iter()
                    .map(|s| {
                        let items = s
                            .items
                            .iter()
                            .map(|item| {
                                let at = item.path.is_some().then(|| {
                                    k += 1;
                                    k - 1
                                });
                                item_view(item, at, &base, &name, cursor, follow)
                            })
                            .collect_view();
                        view! {
                            <section>
                                <h3>{s.heading.clone()}</h3>
                                <ul>{items}</ul>
                            </section>
                        }
                    })
                    .collect_view();
                let (tbase, tname) = (base.clone(), name.clone());
                view! {
                    <div class="map-content">
                        <p class="map-summary">{inline_code(&m.summary)}</p>
                        {sections}
                        {move || thread_view(thread, map_links.with(Vec::len), &tbase, &tname, cursor, follow)}
                        {move || pending.get().map(|(q, secs)| view! {
                            <div class="map-qa">
                                <div class="map-q">{q}</div>
                                <div class="map-loading">{format!("Claude is looking… {secs} s")}</div>
                            </div>
                        })}
                        {move || ask_error.get().map(|e| view! { <div class="map-qa error">{e}</div> })}
                    </div>
                }
                .into_any()
            }
        })
    };
    // Send a question with the map and the thread so far; poll for the seconds spent.
    let ask = move || {
        let Some(input) = ask_input.get_untracked() else { return };
        let question = input.value().trim().to_string();
        let map = state.with_untracked(|s| match s {
            State::Ready(m) => Some(m.clone()),
            _ => None,
        });
        let (Some(map), false) = (map, question.is_empty() || pending.with_untracked(Option::is_some)) else {
            return;
        };
        input.set_value("");
        let n = ask_seq.get_value() + 1;
        ask_seq.set_value(n);
        let job = format!("ask-{n}-{:x}", (js_sys::Math::random() * u32::MAX as f64) as u32);
        ask_job.set_value(Some(job.clone()));
        ask_error.set(None);
        pending.set(Some((question.clone(), 0)));
        scroll_to_end(body);
        fn tick(pending: RwSignal<Option<(String, u64)>>, job: String) {
            set_timeout(
                move || {
                    if pending.try_with_untracked(Option::is_some) != Some(true) {
                        return;
                    }
                    spawn_local(async move {
                        if let Ok(status) = api::job(job.clone()).await
                            && let Some(p) = status.progress
                        {
                            pending.try_update(|x| {
                                if let Some((_, secs)) = x {
                                    *secs = p.done;
                                }
                            });
                        }
                        tick(pending, job);
                    });
                },
                POLL,
            );
        }
        tick(pending, job.clone());
        let request = MapQuestion {
            map,
            thread: thread.get_untracked(),
            question: question.clone(),
        };
        spawn_local(async move {
            let result = api::map_ask(request, job.clone()).await;
            // Only the question still in flight counts (Stop drops it).
            if ask_job.try_get_value().flatten().as_deref() != Some(job.as_str()) {
                return;
            }
            ask_job.set_value(None);
            pending.set(None);
            match result {
                Ok(answer) => {
                    let first = links.with_untracked(Vec::len);
                    let has_links = answer.links.iter().any(|l| l.path.is_some());
                    thread.update(|t| t.push(MapExchange { question, answer }));
                    if has_links {
                        cursor.set(first);
                    }
                    scroll_to_end(body);
                }
                Err(e) => ask_error.set(Some(e)),
            }
        });
    };
    let stop_ask = move || {
        if let Some(job) = ask_job.try_get_value().flatten() {
            ask_job.set_value(None);
            pending.set(None);
            spawn_local(async move {
                let _ = api::cancel(job).await;
            });
        }
    };
    on_cleanup(move || {
        if let Some(job) = ask_job.try_get_value().flatten() {
            spawn_local(async move {
                let _ = api::cancel(job).await;
            });
        }
    });
    let ready = move || state.with(|s| matches!(s, State::Ready(_)));

    let footer = move || {
        state.with(|s| match s {
            State::Ready(m) => {
                let cost = m
                    .cost_usd
                    .filter(|_| !m.cached)
                    .map(|c| format!(" · ${c:.2}"))
                    .unwrap_or_default();
                let when = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(m.generated as f64 * 1000.0));
                let when: String = when
                    .to_locale_string("default", &wasm_bindgen::JsValue::UNDEFINED)
                    .into();
                format!(
                    "{} · {when}{cost}{} · a ask · I regenerate",
                    m.model,
                    if m.cached { " · cached" } else { "" }
                )
            }
            _ => String::new(),
        })
    };

    view! {
        <div class="panel map-view" class:active=move || app.map_focused.get() aria-label="Claude map">
            <div class="pathbar"><span class="path">{title}</span></div>
            <div
                class="map-body"
                id=format!("map-body-{side}")
                tabindex="0"
                node_ref=body
                on:keydown=on_key
                on:focusin=move |_| app.map_focused.set(true)
                on:focusout=move |_| { app.map_focused.try_set(false); }
            >
                {content}
            </div>
            <Show when=ready>
                <form class="map-ask" on:submit=move |ev| { ev.prevent_default(); ask(); }>
                    <input
                        class="modal-input"
                        node_ref=ask_input
                        placeholder="Ask Claude about this…"
                        maxlength="2000"
                        spellcheck="false"
                        autocomplete="off"
                        on:keydown=move |ev| {
                            if ev.key() == "Escape" {
                                ev.prevent_default();
                                ev.stop_propagation();
                                crate::dom::blur_active();
                            }
                        }
                        on:focusin=move |_| app.map_focused.set(true)
                        on:focusout=move |_| { app.map_focused.try_set(false); }
                    />
                    <Show
                        when=move || pending.with(Option::is_some)
                        fallback=|| view! { <button type="submit" tabindex="-1">"Ask"</button> }
                    >
                        <button type="button" tabindex="-1" on:click=move |_| stop_ask()>"Stop"</button>
                    </Show>
                </form>
            </Show>
            <div class="status">{footer}</div>
        </div>
    }
}

/// Text with `code` spans (Claude often writes names that way) shown as code.
fn inline_code(text: &str) -> impl IntoView + use<> {
    text.split('`')
        .enumerate()
        .map(|(i, part)| {
            if i % 2 == 1 && !part.is_empty() {
                view! { <code>{part.to_string()}</code> }.into_any()
            } else {
                part.to_string().into_any()
            }
        })
        .collect_view()
}

/// A map link as shown: relative to `base` when inside it (the base itself by `name`),
/// else absolute.
fn relative(path: &str, base: &str, name: &str) -> String {
    if path == base {
        return name.to_string();
    }
    let prefix = format!("{}/", base.trim_end_matches('/'));
    path.strip_prefix(&prefix).unwrap_or(path).to_string()
}

/// One map link: "path:line" (the line opens the viewer) and its text; items without a
/// path are plain text. `at` is its index among the links.
fn item_view(
    item: &MapItem,
    at: Option<usize>,
    base: &str,
    name: &str,
    cursor: RwSignal<usize>,
    follow: impl Fn(&MapItem, bool) + Copy + 'static,
) -> AnyView {
    let (Some(path), Some(i)) = (item.path.clone(), at) else {
        return view! { <li class="map-item plain">{inline_code(&item.text)}</li> }.into_any();
    };
    let mut shown = relative(&path, base, name);
    if item.is_dir {
        shown.push('/');
    }
    let selected = move || cursor.get() == i;
    let (go, view_it) = (item.clone(), item.clone());
    view! {
        <li class="map-item" class:cursor=selected id=move || selected().then_some("map-cursor")>
            <a class="map-link" class:dir=item.is_dir href="#" title=path tabindex="-1"
                on:click=move |ev| { ev.prevent_default(); cursor.set(i); follow(&go, false); }>
                {shown}
            </a>
            {item.line.map(|n| view! {
                <a class="map-line" href="#" tabindex="-1" title="View at this line (F3)"
                    on:click=move |ev| { ev.prevent_default(); cursor.set(i); follow(&view_it, true); }>
                    {format!(":{n}")}
                </a>
            })}
            <div class="map-text">{inline_code(&item.text)}</div>
        </li>
    }
    .into_any()
}

/// The questions asked so far and Claude's answers, their links numbered after the
/// map's `first` links.
fn thread_view(
    thread: RwSignal<Vec<MapExchange>>,
    first: usize,
    base: &str,
    name: &str,
    cursor: RwSignal<usize>,
    follow: impl Fn(&MapItem, bool) + Copy + 'static,
) -> AnyView {
    let mut k = first;
    thread.with(|t| {
        t.iter()
            .map(|x| {
                let items = x
                    .answer
                    .links
                    .iter()
                    .map(|item| {
                        let at = item.path.is_some().then(|| {
                            k += 1;
                            k - 1
                        });
                        item_view(item, at, base, name, cursor, follow)
                    })
                    .collect_view();
                view! {
                    <div class="map-qa">
                        <div class="map-q">{x.question.clone()}</div>
                        <div class="map-a">
                            {x.answer.text.split("\n\n").map(|p| view! { <p>{inline_code(p)}</p> }).collect_view()}
                        </div>
                        <ul>{items}</ul>
                        {x.answer.cost_usd.map(|c| view! { <div class="map-cost">{format!("${c:.2}")}</div> })}
                    </div>
                }
            })
            .collect_view()
            .into_any()
    })
}

/// Show the end of the map (a question was sent or answered).
fn scroll_to_end(body: NodeRef<Div>) {
    request_animation_frame(move || {
        if let Some(el) = body.try_get_untracked().flatten() {
            el.set_scroll_top(el.scroll_height());
        }
    });
}
