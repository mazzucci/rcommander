//! F2 side-by-side diff overlay with folding of unchanged regions, change navigation and
//! an overview ruler.

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
    let hunks = StoredValue::new(diff.result.as_ref().map(|r| r.hunks.clone()).unwrap_or_default());

    let goto = Callback::new(move |h: usize| {
        current.set(Some(h));
        let row = hunks.with_value(|hunks| hunks.get(h).copied());
        if let Some(row) = row.and_then(|r| dom::by_id(&format!("drow-{r}"))) {
            dom::scroll_into_view(&row, ScrollLogicalPosition::Center);
        }
    });
    let step = move |delta: isize| {
        let Some(last) = hunks.with_value(Vec::len).checked_sub(1) else {
            return;
        };
        goto.run(match current.get_untracked() {
            None if delta > 0 => 0,
            None => last,
            Some(h) => h.saturating_add_signed(delta).min(last),
        });
    };
    let close = move || app.overlay.set(None);
    let keys = window_event_listener(ev::keydown, move |ev| {
        let key = ev.key();
        match key.as_str() {
            "Escape" | "q" => close(),
            "f" => fold.update(|f| *f = !*f),
            "n" => step(1),
            "p" => step(-1),
            "ArrowDown" if ev.alt_key() => step(1),
            "ArrowUp" if ev.alt_key() => step(-1),
            k if dom::is_function_key(k) => {}
            _ if body.get_untracked().is_some_and(|el| dom::scroll_by_key(&el, &key)) => {}
            _ => return,
        }
        ev.prevent_default();
    });
    on_cleanup(move || keys.remove());

    let total = hunks.with_value(Vec::len);
    let content = match diff.result.clone() {
        None => view! {
            <div class="diff-note">{diff.note.clone().unwrap_or_else(|| "Files cannot be compared".into())}</div>
        }
        .into_any(),
        Some(result) => {
            let result = Arc::new(result);
            let items = Memo::new({
                let result = Arc::clone(&result);
                move |_| visible_items(&result, fold.get(), &expanded.get())
            });
            view! {
                <div class="diff-main">
                    <div class="overlay-body" node_ref=body>
                        <DiffTable result=Arc::clone(&result) items expanded current/>
                    </div>
                    <Ruler result items body current goto/>
                </div>
            }
            .into_any()
        }
    };

    view! {
        <div class="overlay diff" role="dialog" aria-label="Diff">
            <header class="diff-header">
                {path_title(&diff.left_title)}
                <span class="diff-arrow" aria-hidden="true">"↔"</span>
                {path_title(&diff.right_title)}
                <span class="spacer"></span>
                <DiffSummary diff=Arc::clone(&diff)/>
                {(total > 0)
                    .then(|| {
                        view! {
                            <span class="hunk-pos">
                                {move || match current.get() {
                                    Some(h) => format!("{} / {total}", h + 1),
                                    None => format!("{total} changes"),
                                }}
                            </span>
                            <button class="icon" title="Previous change (p)" on:click=move |_| step(-1)>"↑"</button>
                            <button class="icon" title="Next change (n)" on:click=move |_| step(1)>"↓"</button>
                        }
                    })}
                <button class="ghost" title="Fold unchanged lines (f)" on:click=move |_| fold.update(|f| *f = !*f)>
                    {move || if fold.get() { "Unfold" } else { "Fold" }}
                    <kbd>"f"</kbd>
                </button>
                <button class="icon" title="Close (Esc)" on:click=move |_| close()>"✕"</button>
            </header>
            {content}
        </div>
    }
}

/// A path as "directory/" (dimmed) and "name" (bold).
fn path_title(path: &str) -> impl IntoView + use<> {
    let (dir, name) = match path.rsplit_once('/') {
        Some((dir, name)) => (format!("{dir}/"), name.to_string()),
        None => (String::new(), path.to_string()),
    };
    view! {
        <span class="diff-title" title=path.to_string()>
            <span class="dir">{dir}</span>
            <span class="name">{name}</span>
        </span>
    }
}

/// Change counts as pills, or the note ("Binary files differ", "Files are identical", …).
#[component]
fn DiffSummary(diff: Arc<DiffResponse>) -> impl IntoView {
    let note = diff.note.clone().or_else(|| {
        diff.result
            .as_ref()
            .filter(|r| r.identical())
            .map(|_| "Files are identical".to_string())
    });
    let stats = diff.result.as_ref().filter(|r| !r.identical()).map(|r| {
        let s = &r.stats;
        view! {
            <span class="pill add" title="added lines">"+" {s.added}</span>
            <span class="pill del" title="removed lines">"−" {s.removed}</span>
            <span class="pill chg" title="changed lines">"~" {s.changed}</span>
        }
    });
    view! {
        <span class="diff-stats">
            {note.map(|n| view! { <span class="pill note">{n}</span> })}
            {stats}
        </span>
    }
}

/// Rows to show: everything, or with unchanged regions folded except those opened.
fn visible_items(result: &DiffResult, fold: bool, open: &HashSet<usize>) -> Vec<Visible> {
    if !fold {
        return (0..result.rows.len()).map(Visible::Row).collect();
    }
    result
        .visible(CONTEXT)
        .into_iter()
        .flat_map(|v| match v {
            // A one-line fold would take as much room as the line itself.
            Visible::Fold { start, n } if n == 1 || open.contains(&start) => {
                (start..start + n).map(Visible::Row).collect()
            }
            v => vec![v],
        })
        .collect()
}

/// Which hunk each changed row belongs to, for highlighting the current one.
fn hunk_of_rows(result: &DiffResult) -> Vec<Option<usize>> {
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
}

#[component]
fn DiffTable(
    result: Arc<DiffResult>,
    items: Memo<Vec<Visible>>,
    expanded: RwSignal<HashSet<usize>>,
    current: RwSignal<Option<usize>>,
) -> impl IntoView {
    let hunk_of = Arc::new(hunk_of_rows(&result));
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
                    items
                        .get()
                        .into_iter()
                        .map(|v| match v {
                            Visible::Row(i) => row_view(&result.rows[i], i, hunk_of[i], current).into_any(),
                            Visible::Fold { start, n } => {
                                view! {
                                    <tr class="fold" title="Show these lines" on:click=move |_| expanded.update(|s| { s.insert(start); })>
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

/// A change's mark on the ruler, in percent of the ruler's height.
#[derive(Clone, PartialEq)]
struct Tick {
    hunk: usize,
    kind: &'static str,
    top: f64,
    height: f64,
}

/// Overview of where the changes are, along the right edge; click a mark to go there.
/// Positions are measured from the rendered rows (lines wrap, folds differ in height),
/// as a share of the scrollable height.
#[component]
fn Ruler(
    result: Arc<DiffResult>,
    items: Memo<Vec<Visible>>,
    body: NodeRef<Div>,
    current: RwSignal<Option<usize>>,
    goto: Callback<usize>,
) -> impl IntoView {
    let ticks = RwSignal::new(Vec::<Tick>::new());
    let measure = move || {
        let Some(body) = body.get_untracked() else { return };
        let height = f64::from(body.scroll_height().max(body.client_height()).max(1));
        let origin = body.get_bounding_client_rect().top() - f64::from(body.scroll_top());
        let found: Vec<Tick> = result
            .hunks
            .iter()
            .enumerate()
            .filter_map(|(hunk, &start)| {
                let len = result.rows[start..]
                    .iter()
                    .take_while(|r| r.kind != RowKind::Equal)
                    .count();
                let first = dom::by_id(&format!("drow-{start}"))?.get_bounding_client_rect();
                let last = dom::by_id(&format!("drow-{}", start + len.max(1) - 1))?.get_bounding_client_rect();
                let kind = match result.rows[start].kind {
                    RowKind::Insert => "ins",
                    RowKind::Delete => "del",
                    _ => "rep",
                };
                Some(Tick {
                    hunk,
                    kind,
                    top: (first.top() - origin) / height * 100.0,
                    height: (last.bottom() - first.top()) / height * 100.0,
                })
            })
            .collect();
        ticks.set(found);
    };
    // Effects run after the table has re-rendered for the new items.
    let on_resize = measure.clone();
    Effect::new(move |_| {
        items.track();
        measure();
    });
    let resize = window_event_listener(ev::resize, move |_| on_resize());
    on_cleanup(move || resize.remove());

    view! {
        <div class="ruler" aria-hidden="true">
            {move || {
                ticks
                    .get()
                    .into_iter()
                    .map(|t| {
                        let hunk = t.hunk;
                        view! {
                            <span
                                class=format!("tick {}", t.kind)
                                class:current=move || current.get() == Some(hunk)
                                style=format!("top:{:.3}%;height:max(3px,{:.3}%)", t.top, t.height)
                                on:click=move |_| goto.run(hunk)
                            ></span>
                        }
                    })
                    .collect_view()
            }}
        </div>
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
