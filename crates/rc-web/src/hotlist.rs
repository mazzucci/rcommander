//! Ctrl+D: the hotlist of bookmarked folders, saved with the settings.

use crate::app::App;
use crate::dom;
use leptos::ev;
use leptos::html::Div;
use leptos::prelude::*;
use rc_core::Location;

#[component]
pub fn HotlistView(app: App) -> impl IntoView {
    let cursor = RwSignal::new(0usize);
    let dialog = NodeRef::<Div>::new();
    let marks = move || app.settings.with(|s| s.bookmarks.clone());
    let count = move || app.settings.with_untracked(|s| s.bookmarks.len());
    Effect::new(move |_| {
        if let Some(dialog) = dialog.get() {
            dom::move_focus(&dialog, ".hotlist-items", 0);
        }
    });

    let close = move || app.hotlist_open.set(false);
    let jump = move |index: usize| {
        let Some(path) = app.settings.with_untracked(|s| s.bookmarks.get(index).cloned()) else {
            return;
        };
        close();
        app.load(app.active.get_untracked(), Location::local(path), None);
    };
    let save = move |bookmarks: Vec<String>, what: &'static str| {
        let mut settings = app.settings.get_untracked();
        settings.bookmarks = bookmarks;
        app.save_settings(settings, move |result| match result {
            Ok(()) => app.info(what),
            Err(e) => app.error(e),
        });
    };
    let add = move || {
        let here = app.panels[app.active.get_untracked()].with_untracked(|p| p.location.to_string());
        let mut marks = app.settings.get_untracked().bookmarks;
        if !here.is_empty() && !marks.contains(&here) {
            marks.push(here);
            cursor.set(marks.len() - 1);
            save(marks, "Bookmark added");
        }
    };
    let remove = move || {
        let mut marks = app.settings.get_untracked().bookmarks;
        let i = cursor.get_untracked();
        if i < marks.len() {
            marks.remove(i);
            cursor.set(i.min(marks.len().saturating_sub(1)));
            save(marks, "Bookmark removed");
        }
    };

    let keys = window_event_listener(ev::keydown, move |ev| {
        if ev.repeat() && ev.key() == "Enter" {
            ev.prevent_default();
            return;
        }
        let key = ev.key();
        // 1–9 jump straight to that bookmark.
        if let Some(n) = key.parse::<usize>().ok().filter(|n| (1..=9).contains(n)) {
            ev.prevent_default();
            return jump(n - 1);
        }
        match key.as_str() {
            "Escape" | "q" => close(),
            "Tab" => dom::trap_tab(dialog, &ev),
            "Enter" | " " if dom::is_button(ev.target()) => return,
            "ArrowUp" => cursor.update(|c| *c = c.saturating_sub(1)),
            "ArrowDown" => cursor.update(|c| *c = (*c + 1).min(count().saturating_sub(1))),
            "Enter" => jump(cursor.get_untracked()),
            "a" => add(),
            "d" | "Delete" => remove(),
            k if dom::is_function_key(k) => {}
            _ => return,
        }
        ev.prevent_default();
    });
    on_cleanup(move || keys.remove());

    view! {
        <div class="backdrop">
            <div class="modal hotlist" node_ref=dialog role="dialog" aria-label="Hotlist">
                <div class="modal-title">"Hotlist"</div>
                <ol class="hotlist-items" tabindex="0" role="listbox" aria-label="Bookmarks">
                    {move || {
                        marks()
                            .into_iter()
                            .enumerate()
                            .map(|(i, path)| {
                                view! {
                                    <li
                                        role="option"
                                        class:cursor=move || cursor.get() == i
                                        aria-selected=move || (cursor.get() == i).to_string()
                                        on:click=move |_| jump(i)
                                    >
                                        <span class="hk">{if i < 9 { (i + 1).to_string() } else { String::new() }}</span>
                                        {path}
                                    </li>
                                }
                            })
                            .collect_view()
                    }}
                </ol>
                <Show when=move || app.settings.with(|s| s.bookmarks.is_empty())>
                    <div class="modal-note">"No bookmarks yet. Press " <kbd>"a"</kbd> " to add the current folder."</div>
                </Show>
                <div class="modal-buttons">
                    <span class="modal-hint">"Enter / 1–9 go · a add · d remove"</span>
                    <button on:click=move |_| add()>"Add current" <kbd>"a"</kbd></button>
                    <button on:click=move |_| close()>"Close" <kbd>"Esc"</kbd></button>
                </div>
            </div>
        </div>
    }
}
