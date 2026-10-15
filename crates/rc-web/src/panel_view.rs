//! One file panel: path bar, column header, file list and status line.

use crate::app::App;
use crate::dom;
use leptos::html::{Div, Input};
use leptos::prelude::*;
use rc_core::dircmp::CmpMark;
use rc_core::format::{format_time, human_size};
use rc_core::{Panel, Row, SortKey};
use web_sys::{KeyboardEvent, ScrollLogicalPosition};

/// Everything a list row displays, precomputed so the list only re-renders when one of
/// these changes (not on every cursor move).
#[derive(Clone, PartialEq)]
struct RowData {
    name: String,
    label: String,
    ext: String,
    size: String,
    date: String,
    is_dir: bool,
    hidden: bool,
    link: bool,
    mark: Option<CmpMark>,
}

/// Display data for row `i`; the synthetic ".." row has no entry.
fn row_data(panel: &Panel, i: usize, tz_offset: i64) -> RowData {
    let Some(e) = panel.row_entry(i) else {
        return RowData {
            name: String::new(),
            label: "[..]".into(),
            ext: String::new(),
            size: "<UP>".into(),
            date: String::new(),
            is_dir: true,
            hidden: false,
            link: false,
            mark: None,
        };
    };
    RowData {
        name: e.name.clone(),
        label: if e.is_dir {
            format!("[{}]", e.name)
        } else {
            e.name.clone()
        },
        ext: e.extension().to_owned(),
        size: match (e.is_dir, e.size) {
            (true, _) => "<DIR>".into(),
            (false, Some(size)) => human_size(size),
            (false, None) => String::new(),
        },
        date: e.modified.map(|t| format_time(t, tz_offset)).unwrap_or_default(),
        is_dir: e.is_dir,
        hidden: e.hidden,
        link: e.is_link,
        mark: panel.marks.get(&e.name).copied(),
    }
}

fn mark_class(mark: Option<CmpMark>) -> &'static str {
    match mark {
        None => "",
        Some(CmpMark::Orphan) => "m-orphan",
        Some(CmpMark::Newer) => "m-newer",
        Some(CmpMark::Older) => "m-older",
        Some(CmpMark::Different) => "m-diff",
        Some(CmpMark::Same) => "m-same",
    }
}

#[component]
pub fn PanelView(app: App, side: usize) -> impl IntoView {
    let panel = app.panels[side];
    let is_active = Memo::new(move |_| app.active.get() == side);
    let cursor = Memo::new(move |_| panel.with(|p| p.cursor));
    let selected = Memo::new(move |_| panel.with(|p| p.selected.clone()));
    let rows =
        Memo::new(move |_| panel.with(|p| (0..p.len()).map(|i| row_data(p, i, app.tz_offset)).collect::<Vec<_>>()));

    // Keep the cursor row visible. Effects run after the DOM update, so the row exists.
    let list_ref = NodeRef::<Div>::new();
    Effect::new(move |_| {
        let c = cursor.get();
        rows.track();
        if let Some(row) = list_ref.get().and_then(|list| list.children().item(c as u32)) {
            dom::scroll_into_view(&row, ScrollLogicalPosition::Nearest);
        }
    });

    let row_view = move |i: usize, r: RowData| {
        let name = r.name.clone();
        view! {
            <div
                class=format!("row {}", mark_class(r.mark))
                class:dir=r.is_dir
                class:hidden=r.hidden
                class:link=r.link
                class:cursor=move || cursor.get() == i
                class:selected=move || selected.with(|s| s.contains(&name))
                on:mousedown=move |_| panel.update(|p| p.cursor = i)
                on:dblclick=move |_| app.enter(side)
            >
                <span class="c-mark">{r.mark.map(|m| m.symbol().to_string())}</span>
                <span class="c-name">{r.label}</span>
                <span class="c-ext">{r.ext}</span>
                <span class="c-size">{r.size}</span>
                <span class="c-date">{r.date}</span>
            </div>
        }
    };

    let header = move |key: SortKey, label: &'static str, class: &'static str| {
        view! {
            <span class=class on:click=move |_| panel.update(|p| p.set_sort(key))>
                {label}
                <span class="sort-arrow">
                    {move || panel.with(|p| (p.sort == key).then_some(if p.sort_desc { "▼" } else { "▲" }))}
                </span>
            </span>
        }
    };

    view! {
        <div class="panel" class:active=is_active on:mousedown=move |_| app.active.set(side)>
            <PathBar app side/>
            <div class="colhdr">
                <span class="c-mark"></span>
                {header(SortKey::Name, "Name", "c-name")}
                {header(SortKey::Ext, "Ext", "c-ext")}
                {header(SortKey::Size, "Size", "c-size")}
                {header(SortKey::Modified, "Date", "c-date")}
            </div>
            <div class="list" id=format!("list-{side}") node_ref=list_ref>
                {move || rows.get().into_iter().enumerate().map(|(i, r)| row_view(i, r)).collect_view()}
            </div>
            <StatusLine app side/>
        </div>
    }
}

/// Location display; click (or Ctrl+L / g) turns it into an input.
#[component]
fn PathBar(app: App, side: usize) -> impl IntoView {
    let panel = app.panels[side];
    let editing = Memo::new(move |_| app.editing_path.get() == Some(side));
    let close = move || app.editing_path.set(None);
    let location = move || panel.with(|p| p.location.to_string());

    let input_ref = NodeRef::<Input>::new();
    input_ref.on_load(|input| {
        let _ = input.focus();
        input.select();
    });
    let on_key = move |ev: KeyboardEvent| match ev.key().as_str() {
        "Enter" => {
            let text = input_ref.get_untracked().map(|i| i.value()).unwrap_or_default();
            close();
            app.navigate_input(side, &text);
        }
        "Escape" => close(),
        _ => {}
    };

    view! {
        <div class="pathbar" on:click=move |_| app.editing_path.set(Some(side))>
            <Show when=move || editing.get() fallback=move || view! { <span class="path" title=location>{location}</span> }>
                <input
                    class="path-input"
                    node_ref=input_ref
                    prop:value=location
                    spellcheck="false"
                    on:keydown=on_key
                    on:blur=move |_| close()
                />
            </Show>
        </div>
    }
}

/// Selection summary, or the quick-filter input while it is open.
#[component]
fn StatusLine(app: App, side: usize) -> impl IntoView {
    let panel = app.panels[side];
    let filtering = Memo::new(move |_| app.filtering.get() == Some(side));
    let close = move || app.filtering.set(None);

    let input_ref = NodeRef::<Input>::new();
    input_ref.on_load(|input| {
        let _ = input.focus();
    });
    let on_key = move |ev: KeyboardEvent| match ev.key().as_str() {
        "Enter" => close(),
        "Escape" => {
            panel.update(|p| p.set_filter(""));
            close();
        }
        // Let the cursor move while filtering, so Enter can then open the match.
        "ArrowUp" | "ArrowDown" => {
            ev.prevent_default();
            panel.update(|p| p.move_cursor(if ev.key() == "ArrowUp" { -1 } else { 1 }));
        }
        _ => {}
    };

    let summary = move || {
        panel.with(|p| {
            let total = p.rows().iter().filter(|r| matches!(r, Row::Entry(_))).count();
            match p.selection_summary() {
                (0, _) => format!("{total} items"),
                (n, size) => format!("{n} of {total} selected, {}", human_size(size)),
            }
        })
    };
    let flags = move || {
        panel.with(|p| {
            let mut flags = Vec::new();
            if !p.filter.is_empty() {
                flags.push(format!("filter: {}", p.filter));
            }
            if p.show_hidden {
                flags.push("hidden shown".into());
            }
            flags.join(" · ")
        })
    };

    view! {
        <div class="status">
            <Show
                when=move || filtering.get()
                fallback=move || {
                    view! {
                        <span class="summary">{summary}</span>
                        <span class="flags">{flags}</span>
                    }
                }
            >
                <span class="filter-label">"Filter:"</span>
                <input
                    class="filter-input"
                    node_ref=input_ref
                    prop:value=move || panel.with(|p| p.filter.clone())
                    spellcheck="false"
                    on:input=move |ev| panel.update(|p| p.set_filter(&event_target_value(&ev)))
                    on:keydown=on_key
                    on:blur=move |_| close()
                />
            </Show>
        </div>
    }
}
