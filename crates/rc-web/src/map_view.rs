//! `i`: the Claude map panel. The panel that is not active shows Claude's summary of a
//! file or folder with links into it: click (or Enter) shows the target in the panel,
//! the line badge (or F3) opens the viewer there. Tab moves the keyboard into the map.

use crate::api;
use crate::app::App;
use leptos::ev;
use leptos::html::Div;
use leptos::prelude::*;
use leptos::task::spawn_local;
use rc_core::Location;
use rc_core::api::{FileMap, MapItem};
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

    // Tab from the panels (App::handle_key) focuses `.map-body`.
    let links = Memo::new(move |_| {
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
                let base = if m.is_dir { base.clone() } else { location.parent().map_or(base.clone(), |p| p.to_string()) };
                let mut k = 0;
                let sections = m
                    .sections
                    .iter()
                    .map(|s| {
                        let items = s
                            .items
                            .iter()
                            .map(|item| {
                                let Some(path) = item.path.clone() else {
                                    return view! { <li class="map-item plain">{inline_code(&item.text)}</li> }.into_any();
                                };
                                let i = k;
                                k += 1;
                                let rel = path.strip_prefix(&base).map(|p| p.trim_start_matches('/')).unwrap_or(&path);
                                let mut shown = if rel.is_empty() { path.clone() } else { rel.to_string() };
                                if item.is_dir {
                                    shown.push('/');
                                }
                                let at = move || cursor.get() == i;
                                let (go, view_it) = (item.clone(), item.clone());
                                view! {
                                    <li class="map-item" class:cursor=at id=move || at().then_some("map-cursor")>
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
                view! {
                    <div class="map-content">
                        <p class="map-summary">{inline_code(&m.summary)}</p>
                        {sections}
                    </div>
                }
                .into_any()
            }
        })
    };
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
                    "{} · {when}{cost}{} · I regenerate",
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
