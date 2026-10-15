//! F3 file viewer overlay.

use crate::app::App;
use crate::dom;
use leptos::ev;
use leptos::html::Div;
use leptos::prelude::*;
use rc_core::api::FileContent;
use rc_core::format::human_size;
use std::sync::Arc;

#[component]
pub fn Viewer(app: App, content: Arc<FileContent>) -> impl IntoView {
    let body = NodeRef::<Div>::new();
    let keys = window_event_listener(ev::keydown, move |ev| {
        let key = ev.key();
        if matches!(key.as_str(), "Escape" | "F3" | "3" | "q") {
            app.overlay.set(None);
        } else if !dom::is_function_key(&key) && !body.get_untracked().is_some_and(|el| dom::scroll_by_key(&el, &key)) {
            return;
        }
        ev.prevent_default();
    });
    on_cleanup(move || keys.remove());

    let size = human_size(content.size);
    let body_view = match &content.text {
        None => view! { <div class="viewer-msg">"Binary file (" {size.clone()} "), not shown."</div> }.into_any(),
        Some(text) => {
            // Line numbers live in their own <pre> so the text stays one cheap text node.
            let lines = text.lines().count().max(1);
            let gutter: String = (1..=lines).map(|n| format!("{n}\n")).collect();
            view! {
                <div class="viewer-text">
                    <pre class="gutter">{gutter}</pre>
                    <pre class="text">{text.clone()}</pre>
                </div>
            }
            .into_any()
        }
    };

    view! {
        <div class="overlay viewer">
            <div class="overlay-title">
                <span class="title-path">{content.location.to_string()}</span>
                <span class="title-meta">
                    {size}
                    {content.truncated.then_some(" · truncated, showing the beginning only")}
                </span>
                <span class="hint">"Esc / F3 / q close"</span>
            </div>
            <div class="overlay-body" node_ref=body>
                {body_view}
            </div>
        </div>
    }
}
