//! One file panel: path bar, column header, file list and status line.

use crate::app::App;
use crate::dom;
use leptos::html::{Div, Input};
use leptos::prelude::*;
use rc_core::dircmp::CmpMark;
use rc_core::format::human_size;
use rc_core::{Column, Entry, Panel, Row, SortKey};
use wasm_bindgen::JsValue;
use web_sys::{KeyboardEvent, ScrollLogicalPosition};

/// What a list row shows, so the list only re-renders when this changes (not on every
/// cursor move). `entry` is `None` for the synthetic ".." row.
#[derive(Clone, PartialEq)]
struct RowData {
    entry: Option<Entry>,
    mark: Option<CmpMark>,
}

fn row_data(panel: &Panel, i: usize) -> RowData {
    let entry = panel.row_entry(i).cloned();
    let mark = entry.as_ref().and_then(|e| panel.marks.get(&e.name).copied());
    RowData { entry, mark }
}

/// Local UTC offset in seconds at the moment `secs` (DST-aware, like the terminal UI).
fn utc_offset(secs: i64) -> i64 {
    // getTimezoneOffset() is "UTC minus local" in minutes.
    -(js_sys::Date::new(&JsValue::from_f64(secs as f64 * 1000.0)).get_timezone_offset() as i64) * 60
}

/// CSS class of a column's cells and header.
fn column_class(c: Column) -> &'static str {
    match c {
        Column::Git => "c-git",
        Column::Ext => "c-ext",
        Column::Size => "c-size",
        Column::Modified => "c-modified",
        Column::Created => "c-created",
        Column::Permissions => "c-perms",
        Column::Owner => "c-owner",
        Column::Group => "c-group",
    }
}

fn cell_class(c: Column) -> String {
    format!("{}{}", column_class(c), if c.right_aligned() { " right" } else { "" })
}

/// Grid track list for the mark, name and the given optional columns.
fn grid_columns(columns: &[Column]) -> String {
    let widths: String = columns
        .iter()
        .map(|c| format!(" calc({}ch + 8px)", c.width()))
        .collect();
    format!("--cols: 14px minmax(8ch, 1fr){widths}")
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
    let columns = Memo::new(move |_| app.settings.with(|s| s.visible_columns().collect::<Vec<_>>()));
    let rows = Memo::new(move |_| panel.with(|p| (0..p.len()).map(|i| row_data(p, i)).collect::<Vec<_>>()));

    // Keep the cursor row visible. Effects run after the DOM update, so the row exists.
    let list_ref = NodeRef::<Div>::new();
    Effect::new(move |_| {
        let c = cursor.get();
        rows.track();
        if let Some(row) = list_ref.get().and_then(|list| list.children().item(c as u32)) {
            dom::scroll_into_view(&row, ScrollLogicalPosition::Nearest);
        }
    });

    let row_view = move |i: usize, r: RowData, columns: &[Column]| {
        let RowData { entry, mark } = r;
        let name = entry.as_ref().map(|e| e.name.clone()).unwrap_or_default();
        let label = match &entry {
            None => "[..]".to_string(),
            Some(e) if e.is_dir => format!("[{}]", e.name),
            Some(e) => e.name.clone(),
        };
        // Symlinks show where they point, as in the terminal UI.
        let link = entry
            .as_ref()
            .filter(|e| e.is_link)
            .map(|e| e.link_target.clone().unwrap_or_else(|| "?".into()));
        let cells = columns
            .iter()
            .map(|&c| {
                let text = match &entry {
                    Some(e) => c.text(e, utc_offset),
                    None => String::new(),
                };
                // Git status letters use the shared palette.
                let color = match (c, entry.as_ref().and_then(|e| e.git)) {
                    (Column::Git, Some(g)) => format!("color:{};font-weight:600", g.color().0),
                    _ => String::new(),
                };
                view! { <span class=cell_class(c) style=color>{text}</span> }
            })
            .collect_view();
        view! {
            <div
                class=format!("row {}", mark_class(mark))
                class:dir=entry.as_ref().is_none_or(|e| e.is_dir)
                class:hidden=entry.as_ref().is_some_and(|e| e.hidden)
                class:cursor=move || cursor.get() == i
                class:selected=move || selected.with(|s| s.contains(&name))
                on:mousedown=move |_| panel.update(|p| p.cursor = i)
                on:dblclick=move |_| app.enter(side)
            >
                <span class="c-mark">{mark.map(|m| m.symbol().to_string())}</span>
                <span class="c-name">
                    {label}
                    {link.map(|target| view! { <span class="link-target">" → " {target}</span> })}
                </span>
                {cells}
            </div>
        }
    };

    // Name, Ext, Size and Modified headers sort; the other columns have no sort order.
    let header = move |key: Option<SortKey>, title: &'static str, class: String| {
        let arrow = move || panel.with(|p| (Some(p.sort) == key).then_some(if p.sort_desc { "▼" } else { "▲" }));
        view! {
            <span
                class=class
                class:sortable=key.is_some()
                on:click=move |_| {
                    if let Some(key) = key {
                        panel.update(|p| p.set_sort(key));
                    }
                }
            >
                {title}
                <span class="sort-arrow">{arrow}</span>
            </span>
        }
    };

    view! {
        <div
            class="panel"
            class:active=is_active
            style=move || columns.with(|c| grid_columns(c))
            on:mousedown=move |_| app.active.set(side)
        >
            <PathBar app side/>
            <div class="colhdr">
                <span class="c-mark"></span>
                {header(Some(SortKey::Name), "Name", "c-name".into())}
                {move || columns.get().into_iter().map(|c| header(c.sort_key(), c.title(), cell_class(c))).collect_view()}
            </div>
            <div class="list" id=format!("list-{side}") node_ref=list_ref>
                {move || {
                    let columns = columns.get();
                    rows.get().into_iter().enumerate().map(|(i, r)| row_view(i, r, &columns)).collect_view()
                }}
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

    // The input is re-created each time the editor opens; focus it whenever it appears.
    // (`NodeRef::on_load` fires only once and panics on a second mount.)
    let input_ref = NodeRef::<Input>::new();
    Effect::new(move |_| {
        if let Some(input) = input_ref.get() {
            let _ = input.focus();
            input.select();
        }
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
    Effect::new(move |_| {
        if let Some(input) = input_ref.get() {
            let _ = input.focus();
        }
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
                (0, _) if total == 1 => "1 item".to_string(),
                (0, _) => format!("{total} items"),
                (n, size) => format!("{n} of {total} selected · {}", human_size(size)),
            }
        })
    };
    let flags = move || {
        panel.with(|p| {
            let mut flags = Vec::new();
            if !p.filter.is_empty() {
                flags.push(format!("filter: {}", p.filter));
            }
            // Only mention what differs from the user's default.
            let default_hidden = app.settings.with(|s| s.show_hidden);
            if p.show_hidden != default_hidden {
                flags.push(if p.show_hidden { "hidden shown" } else { "hidden off" }.into());
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
