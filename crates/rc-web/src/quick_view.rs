//! Ctrl+Q: Quick View. The panel that is not active previews the entry under the
//! active panel's cursor: the beginning of a file, or what a folder holds.

use crate::api;
use crate::app::App;
use leptos::prelude::*;
use leptos::task::spawn_local;
use rc_core::Location;
use rc_core::api::Preview;
use rc_core::format::human_size;
use std::time::Duration;

/// Wait this long after the cursor stops before asking the server, so holding an
/// arrow key does not send a request per row.
const SETTLE: Duration = Duration::from_millis(60);
/// Lines shown of a text file (the rest is a keypress away with F3).
const MAX_LINES: usize = 500;

/// The entry being previewed; a change on disk (size or time) makes a new key.
type Key = (Location, Option<u64>, Option<i64>);

#[component]
pub fn QuickView(app: App, side: usize) -> impl IntoView {
    let source = move || app.panels[1 - side];
    let target =
        Memo::new(move |_| source().with(|p| p.current().map(|e| (p.location.join(&e.name), e.size, e.modified))));
    let preview = RwSignal::new(None::<(Key, Result<Preview, String>)>);
    let seq = StoredValue::new(0u64);

    Effect::new(move |_| {
        let Some(key) = target.get() else {
            preview.set(None);
            return;
        };
        if preview.with_untracked(|p| p.as_ref().is_some_and(|(k, _)| *k == key)) {
            return;
        }
        let n = seq.get_value() + 1;
        seq.set_value(n);
        set_timeout(
            move || {
                if seq.try_get_value() != Some(n) {
                    return;
                }
                spawn_local(async move {
                    let result = api::preview(key.0.clone()).await;
                    // Only the latest request counts.
                    if seq.try_get_value() == Some(n) {
                        preview.try_set(Some((key, result)));
                    }
                });
            },
            SETTLE,
        );
    });

    let name = move || {
        target.with(|t| {
            t.as_ref()
                .and_then(|(loc, ..)| loc.file_name().map(|n| format!("Quick view: {n}")))
                .unwrap_or_else(|| "Quick view".into())
        })
    };
    let body = move || {
        preview.with(|p| match p.as_ref().map(|(_, r)| r) {
            None => view! { <div class="qv-msg">"Nothing to preview" <span class="qv-sub">"Ctrl+Q closes Quick View"</span></div> }
                .into_any(),
            Some(Err(e)) => view! { <div class="qv-msg error">{e.clone()}</div> }.into_any(),
            Some(Ok(Preview::Folder(sum))) => {
                let plural = |n: usize, w: &str| format!("{n} {w}{}", if n == 1 { "" } else { "s" });
                view! {
                    <div class="qv-folder">
                        <div class="qv-kind">"Folder"</div>
                        <dl>
                            <dt>"Folders"</dt><dd>{sum.folders}</dd>
                            <dt>"Files"</dt><dd>{sum.files}</dd>
                            <dt>"Size"</dt><dd>{human_size(sum.size)}</dd>
                            <dt>"Hidden"</dt><dd>{sum.hidden}</dd>
                        </dl>
                        <div class="qv-sub">
                            {if sum.partial {
                                "Very large folder: counting stopped early".to_string()
                            } else {
                                format!("{} directly inside; subfolders not included", plural(sum.folders + sum.files, "item"))
                            }}
                        </div>
                    </div>
                }
                .into_any()
            }
            Some(Ok(Preview::File(c))) => match &c.text {
                None => view! {
                    <div class="qv-msg">"Binary file · " {human_size(c.size)} <span class="qv-sub">"F3 to view"</span></div>
                }
                .into_any(),
                Some(text) => {
                    let lines: Vec<&str> = text.lines().take(MAX_LINES).collect();
                    let gutter: String = (1..=lines.len().max(1)).map(|n| format!("{n}\n")).collect();
                    view! {
                        <div class="qv-text">
                            <pre class="gutter">{gutter}</pre>
                            <pre class="text">{lines.join("\n")}</pre>
                        </div>
                    }
                    .into_any()
                }
            },
        })
    };
    let footer = move || {
        preview.with(|p| match p.as_ref().map(|(_, r)| r) {
            Some(Ok(Preview::File(c))) if c.text.is_some() => {
                let cut = c.truncated || c.text.as_ref().is_some_and(|t| t.lines().nth(MAX_LINES).is_some());
                format!(
                    "{}{}",
                    human_size(c.size),
                    if cut {
                        " · beginning shown · F3 to view all"
                    } else {
                        ""
                    }
                )
            }
            _ => String::new(),
        })
    };

    view! {
        <div class="panel quick-view" aria-label="Quick view">
            <div class="pathbar"><span class="path">{name}</span></div>
            <div class="qv-body">{body}</div>
            <div class="status">{footer}</div>
        </div>
    }
}
