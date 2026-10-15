//! One file panel: path bar, column header, file list and status line.

use crate::app::{App, ROW_HEIGHT};
use leptos::ev;
use leptos::html::{Div, Input, Span};
use leptos::prelude::*;
use rc_core::dircmp::CmpMark;
use rc_core::format::human_size;
use rc_core::{Column, Entry, Panel, Row, SortKey};
use wasm_bindgen::JsValue;
use web_sys::KeyboardEvent;

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
    format!(
        "{}{}",
        column_class(c),
        if c.right_aligned() { " align-right" } else { "" }
    )
}

/// Narrowest the name column gets before optional columns are dropped, in characters.
const MIN_NAME: u16 = 12;
/// Characters each row needs besides the name and the optional columns: the mark column,
/// the icon and paddings.
const FIXED: u16 = 6;
/// Rows rendered above and below the visible ones, so scrolling stays smooth.
const OVERSCAN: usize = 20;

/// Grid track list for the mark, name and the given optional columns. Each optional
/// column is its `width()` in characters plus padding, about one character, which is
/// what `Settings::fit_columns` assumes.
fn grid_columns(columns: &[Column]) -> String {
    let widths: String = columns
        .iter()
        .map(|c| format!(" calc({}ch + 8px)", c.width()))
        .collect();
    format!("--cols: 14px minmax({MIN_NAME}ch, 1fr){widths}")
}

#[component]
pub fn PanelView(app: App, side: usize) -> impl IntoView {
    let panel = app.panels[side];
    let flash = app.flash[side];
    let is_active = Memo::new(move |_| app.active.get() == side);
    let cursor = Memo::new(move |_| panel.with(|p| p.cursor));
    // A cursor move only re-renders the two rows it leaves and enters.
    let on_cursor = Selector::new(move || cursor.get());
    let selected = Memo::new(move |_| panel.with(|p| p.selected.clone()));
    // Rebuild the row list only when rows change, not on cursor moves or selection.
    let version = Memo::new(move |_| panel.with(|p| p.version()));
    let rows = Memo::new(move |_| {
        let _ = version.get(); // `get`, not `track`: a lazy memo must be pulled to update
        panel.with_untracked(|p| (0..p.len()).map(|i| row_data(p, i)).collect::<Vec<_>>())
    });

    // Optional columns that fit the panel's width, least important dropped first.
    let list_ref = NodeRef::<Div>::new();
    let probe = NodeRef::<Span>::new();
    let width_chars = RwSignal::new(None::<u16>);
    // Only the rows in view (plus a margin) are in the DOM, so huge folders stay fast.
    let scroll_top = RwSignal::new(0i32);
    let view_height = RwSignal::new(0i32);
    let window = Memo::new(move |_| {
        let n = rows.with(Vec::len);
        let first = (scroll_top.get().max(0) / ROW_HEIGHT) as usize;
        // Before the first measurement, render a screenful.
        let visible = (view_height.get().max(ROW_HEIGHT * 40) / ROW_HEIGHT) as usize + 1;
        let start = first.saturating_sub(OVERSCAN).min(n);
        (start, (first + visible + OVERSCAN).min(n))
    });

    let measure = move || {
        let (Some(list), Some(probe)) = (list_ref.get_untracked(), probe.get_untracked()) else {
            return;
        };
        view_height.set(list.client_height());
        // The probe holds ten digits in the row font: one `ch` is a tenth of it.
        let ch = f64::from(probe.offset_width()) / 10.0;
        if ch > 0.0 {
            width_chars.set(Some((f64::from(list.client_width()) / ch) as u16));
        }
    };
    Effect::new(move |_| {
        if list_ref.get().is_some() {
            measure();
        }
    });
    let resize = window_event_listener(ev::resize, move |_| measure());
    on_cleanup(move || resize.remove());
    let columns = Memo::new(move |_| {
        let width = width_chars.get();
        app.settings.with(|s| match width {
            Some(width) => s.fit_columns(width, MIN_NAME, FIXED),
            None => s.visible_columns().collect(),
        })
    });

    // Keep the cursor row visible (by position: off-screen rows are not in the DOM).
    Effect::new(move |_| {
        let c = cursor.get() as i32;
        let _ = rows.get();
        let Some(list) = list_ref.get() else { return };
        let (top, height, scrolled) = (c * ROW_HEIGHT, list.client_height(), list.scroll_top());
        if top < scrolled {
            list.set_scroll_top(top);
        } else if top + ROW_HEIGHT > scrolled + height {
            list.set_scroll_top(top + ROW_HEIGHT - height);
        }
    });

    let row_view = move |i: usize, r: RowData, columns: &[Column]| {
        let RowData { entry, mark } = r;
        let name = entry.as_ref().map(|e| e.name.clone()).unwrap_or_default();
        let label = entry.as_ref().map_or_else(|| "..".to_string(), |e| e.name.clone());
        let kind = match &entry {
            None => "up",
            Some(e) if e.is_dir => "dir",
            Some(_) => "file",
        };
        // Symlinks show where they point, as in the terminal UI.
        let link = entry
            .as_ref()
            .filter(|e| e.is_link)
            .map(|e| e.link_target.clone().unwrap_or_else(|| "?".into()));
        let cells = columns
            .iter()
            .map(|&c| {
                let text = entry.as_ref().map(|e| c.text(e, utc_offset)).unwrap_or_default();
                // Git status letters use the palette shared with the terminal UI.
                let color = match (c, entry.as_ref().and_then(|e| e.git)) {
                    (Column::Git, Some(g)) => format!("color:{}", g.color().0),
                    _ => String::new(),
                };
                view! { <span class=cell_class(c) style=color>{text}</span> }
            })
            .collect_view();
        let flashing = {
            let name = name.clone();
            move || flash.with(|f| f.contains_key(&name))
        };
        view! {
            <div
                class=format!("row {kind}")
                // Compare marks color the mark and, unless "same", the name.
                class:marked=mark.is_some_and(|m| m != CmpMark::Same)
                style=mark.map(|m| format!("--mark:{}", m.color().0))
                class:hidden=entry.as_ref().is_some_and(|e| e.hidden)
                class:cursor={
                    let on_cursor = on_cursor.clone();
                    move || on_cursor.selected(&i)
                }
                class:selected=move || selected.with(|s| s.contains(&name))
                class:flash=flashing
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

    // Shown in place of rows when there is nothing but "..".
    let empty = move || {
        panel.with(|p| {
            let nothing = !p.location.to_string().is_empty() && p.rows().iter().all(|r| matches!(r, Row::Parent));
            nothing.then_some(if p.filter.is_empty() {
                "Empty folder · F7 new folder"
            } else {
                "Nothing matches the filter"
            })
        })
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
            <div
                class="list"
                id=format!("list-{side}")
                node_ref=list_ref
                on:scroll=move |_| {
                    if let Some(list) = list_ref.get_untracked() {
                        scroll_top.set(list.scroll_top());
                    }
                }
            >
                <div style=move || format!("height:{}px", window.get().0 as i32 * ROW_HEIGHT)></div>
                {move || {
                    let (start, end) = window.get();
                    let columns = columns.get();
                    rows.with(|r| {
                        r[start..end]
                            .iter()
                            .cloned()
                            .enumerate()
                            .map(|(k, row)| row_view(start + k, row, &columns))
                            .collect_view()
                    })
                }}
                <div style=move || {
                    let (_, end) = window.get();
                    format!("height:{}px", (rows.with(Vec::len) - end) as i32 * ROW_HEIGHT)
                }></div>
                {move || empty().map(|text| view! { <div class="list-empty">{text}</div> })}
            </div>
            <span class="ch-probe" node_ref=probe aria-hidden="true">"0000000000"</span>
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
                    // Set once when the editor opens: live refreshes must not overwrite
                    // what the user is typing.
                    prop:value=panel.with_untracked(|p| p.location.to_string())
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
