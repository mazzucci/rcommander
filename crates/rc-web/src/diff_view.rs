//! F2 side-by-side diff overlay with folding of unchanged regions and hunk navigation.

use crate::app::App;
use crate::dom;
use leptos::ev;
use leptos::html::Div;
use leptos::prelude::*;
use rc_core::DiffResult;
use rc_core::api::DiffResponse;
use rc_core::diff::{DiffRow, RowKind, Side, Visible};
use std::collections::HashSet;
use std::sync::Arc;
use web_sys::ScrollLogicalPosition;

/// Unchanged lines kept around each change when folding.
const CONTEXT: usize = 3;

#[component]
pub fn DiffView(app: App, diff: Arc<DiffResponse>) -> impl IntoView {
    let fold = RwSignal::new(true);
    // Folds the user clicked open, by their first row.
    let expanded = RwSignal::new(HashSet::<usize>::new());
    // Index into `hunks` of the highlighted change.
    let current = RwSignal::new(None::<usize>);
    let body = NodeRef::<Div>::new();

    let hunks = diff.result.as_ref().map(|r| r.hunks.clone()).unwrap_or_default();
    let goto = move |delta: isize| {
        let Some(last) = hunks.len().checked_sub(1) else { return };
        let next = match current.get_untracked() {
            None if delta > 0 => 0,
            None => last,
            Some(h) => h.saturating_add_signed(delta).min(last),
        };
        current.set(Some(next));
        if let Some(row) = dom::by_id(&format!("drow-{}", hunks[next])) {
            dom::scroll_into_view(&row, ScrollLogicalPosition::Center);
        }
    };
    let keys = window_event_listener(ev::keydown, move |ev| {
        let key = ev.key();
        match key.as_str() {
            "Escape" | "q" => app.overlay.set(None),
            "f" => fold.update(|f| *f = !*f),
            "n" => goto(1),
            "p" => goto(-1),
            "ArrowDown" if ev.alt_key() => goto(1),
            "ArrowUp" if ev.alt_key() => goto(-1),
            k if dom::is_function_key(k) => {}
            _ if body.get_untracked().is_some_and(|el| dom::scroll_by_key(&el, &key)) => {}
            _ => return,
        }
        ev.prevent_default();
    });
    on_cleanup(move || keys.remove());

    let content = match diff.result.clone() {
        None => view! { <div class="diff-note">{diff.note.clone().unwrap_or_else(|| "Files cannot be compared".into())}</div> }
            .into_any(),
        Some(result) => view! { <DiffTable result=Arc::new(result) fold expanded current/> }.into_any(),
    };

    view! {
        <div class="overlay diff">
            <div class="diff-titles">
                <span class="diff-title" title=diff.left_title.clone()>{diff.left_title.clone()}</span>
                <span class="diff-title" title=diff.right_title.clone()>{diff.right_title.clone()}</span>
            </div>
            <div class="diff-meta">
                <DiffSummary diff=diff.clone() current/>
                <span class="hint">
                    "n/p next/prev change · f " {move || if fold.get() { "unfold" } else { "fold" }} " · Esc close"
                </span>
            </div>
            <div class="overlay-body" node_ref=body>
                {content}
            </div>
        </div>
    }
}

/// Change counts, or the note ("Binary files differ", "Files are identical", …).
#[component]
fn DiffSummary(diff: Arc<DiffResponse>, current: RwSignal<Option<usize>>) -> impl IntoView {
    let note = diff.note.clone().or_else(|| {
        diff.result
            .as_ref()
            .filter(|r| r.identical())
            .map(|_| "Files are identical".to_string())
    });
    let stats = diff.result.as_ref().filter(|r| !r.identical()).map(|r| {
        let total = r.hunks.len();
        let s = &r.stats;
        view! {
            <span class="stat add">"+" {s.added}</span>
            <span class="stat del">"−" {s.removed}</span>
            <span class="stat chg">"~" {s.changed}</span>
            <span class="stat">
                {move || match current.get() {
                    Some(h) => format!("change {} of {total}", h + 1),
                    None => format!("{total} changes"),
                }}
            </span>
        }
    });
    view! {
        <span class="diff-stats">
            {note.map(|n| view! { <span class="stat note">{n}</span> })}
            {stats}
        </span>
    }
}

#[component]
fn DiffTable(
    result: Arc<DiffResult>,
    fold: RwSignal<bool>,
    expanded: RwSignal<HashSet<usize>>,
    current: RwSignal<Option<usize>>,
) -> impl IntoView {
    // Which hunk each changed row belongs to, for highlighting the current one.
    let hunk_of: Arc<Vec<Option<usize>>> = Arc::new({
        let mut hunk = None;
        let mut next = 0;
        result
            .rows
            .iter()
            .enumerate()
            .map(|(i, row)| {
                if result.hunks.get(next) == Some(&i) {
                    hunk = Some(next);
                    next += 1;
                }
                (row.kind != RowKind::Equal).then_some(hunk).flatten()
            })
            .collect()
    });

    let items = {
        let result = result.clone();
        move || -> Vec<Visible> {
            if !fold.get() {
                return (0..result.rows.len()).map(Visible::Row).collect();
            }
            expanded.with(|open| {
                result
                    .visible(CONTEXT)
                    .into_iter()
                    .flat_map(|v| match v {
                        Visible::Fold { start, n } if open.contains(&start) => {
                            (start..start + n).map(Visible::Row).collect()
                        }
                        v => vec![v],
                    })
                    .collect()
            })
        }
    };

    view! {
        <table class="diff-table">
            <colgroup>
                <col class="ln-col"/>
                <col/>
                <col class="ln-col"/>
                <col/>
            </colgroup>
            <tbody>
                {move || {
                    items()
                        .into_iter()
                        .map(|v| match v {
                            Visible::Row(i) => row_view(&result.rows[i], i, hunk_of[i], current).into_any(),
                            Visible::Fold { start, n } => {
                                view! {
                                    <tr class="fold" on:click=move |_| expanded.update(|s| { s.insert(start); })>
                                        <td colspan="4">"⋯ " {n} " unchanged lines"</td>
                                    </tr>
                                }
                                    .into_any()
                            }
                        })
                        .collect_view()
                }}
            </tbody>
        </table>
    }
}

fn row_view(row: &DiffRow, i: usize, hunk: Option<usize>, current: RwSignal<Option<usize>>) -> impl IntoView {
    let kind = match row.kind {
        RowKind::Equal => "eq",
        RowKind::Delete => "del",
        RowKind::Insert => "ins",
        RowKind::Replace => "rep",
    };
    view! {
        <tr id=format!("drow-{i}") class=kind class:current=move || hunk.is_some() && current.get() == hunk>
            {side_cells(row.left.as_ref(), "left")}
            {side_cells(row.right.as_ref(), "right")}
        </tr>
    }
}

fn side_cells(side: Option<&Side>, class: &'static str) -> impl IntoView {
    match side {
        Some(side) => {
            let segments = side
                .segments
                .iter()
                .map(|s| view! { <span class:chg=s.changed>{s.text.clone()}</span> })
                .collect_view();
            view! {
                <td class="ln">{side.line_no}</td>
                <td class=format!("code {class}")>{segments}</td>
            }
            .into_any()
        }
        None => view! {
            <td class="ln"></td>
            <td class=format!("code {class} empty")></td>
        }
        .into_any(),
    }
}
