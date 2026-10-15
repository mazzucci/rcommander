//! Small DOM helpers shared by the views.

use leptos::html::Div;
use leptos::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{Element, EventTarget, HtmlElement, KeyboardEvent, ScrollIntoViewOptions, ScrollLogicalPosition};

/// Elements that take keyboard focus inside dialogs.
pub const FOCUSABLE: &str = "button, input, [tabindex]:not([tabindex='-1'])";

pub fn by_id(id: &str) -> Option<Element> {
    web_sys::window()?.document()?.get_element_by_id(id)
}

/// Scroll `el` into view with the given vertical alignment, without smooth animation
/// (key repeat would otherwise lag behind).
pub fn scroll_into_view(el: &Element, block: ScrollLogicalPosition) {
    let opts = ScrollIntoViewOptions::new();
    opts.set_block(block);
    el.scroll_into_view_with_scroll_into_view_options(&opts);
}

/// Whether a key event comes from a text field, which then gets the key instead of us.
pub fn is_text_field(target: Option<EventTarget>) -> bool {
    target
        .and_then(|t| t.dyn_into::<HtmlElement>().ok())
        .is_some_and(|el| matches!(el.tag_name().as_str(), "INPUT" | "TEXTAREA" | "SELECT") || el.is_content_editable())
}

pub fn is_button(target: Option<EventTarget>) -> bool {
    target
        .and_then(|t| t.dyn_into::<HtmlElement>().ok())
        .is_some_and(|el| el.tag_name() == "BUTTON")
}

/// Move keyboard focus among the elements in `container` matching `selector`: `delta`
/// steps from the focused one (clamped), or to the first if none of them has focus.
pub fn move_focus(container: &Element, selector: &str, delta: isize) {
    let items = query_all(container, selector);
    let Some(last) = items.len().checked_sub(1) else { return };
    let active = active_element();
    let next = items
        .iter()
        .position(|el| el.is_same_node(active.as_ref().map(|a| a.as_ref())))
        .map_or(0, |i| i.saturating_add_signed(delta).min(last));
    let _ = items[next].focus();
}

fn active_element() -> Option<Element> {
    web_sys::window()?.document()?.active_element()
}

fn query_all(container: &Element, selector: &str) -> Vec<HtmlElement> {
    let Ok(list) = container.query_selector_all(selector) else {
        return Vec::new();
    };
    (0..list.length())
        .filter_map(|i| list.item(i)?.dyn_into().ok())
        .collect()
}

/// Keep Tab / Shift+Tab inside a dialog: wrap around at either end, and pull focus back
/// in if it is somewhere else. (The page behind is `inert`, so nothing there is reachable
/// anyway; this also keeps focus from leaving for the browser's own UI.)
pub fn trap_tab(dialog: NodeRef<Div>, ev: &KeyboardEvent) {
    let Some(dialog) = dialog.get_untracked() else { return };
    let items = query_all(&dialog, FOCUSABLE);
    let (Some(first), Some(last)) = (items.first(), items.last()) else {
        return;
    };
    let active = active_element();
    let inside = active.as_ref().is_some_and(|a| dialog.contains(Some(a)));
    let at = |el: &HtmlElement| active.as_ref().is_some_and(|a| el.is_same_node(Some(a)));
    let target = match (inside, ev.shift_key()) {
        (false, false) => first,
        (false, true) => last,
        (true, false) if at(last) => first,
        (true, true) if at(first) => last,
        // Inside and not at an edge: the browser moves focus normally.
        _ => return,
    };
    ev.prevent_default();
    let _ = target.focus();
}

/// Take focus away from whatever has it (e.g. a text field under a new overlay, which
/// would otherwise keep receiving keys).
pub fn blur_active() {
    if let Some(el) = active_element().and_then(|e| e.dyn_into::<HtmlElement>().ok()) {
        let _ = el.blur();
    }
}

/// Scroll a container in response to viewer-style navigation keys.
/// Returns `false` if the key is not a scrolling key.
pub fn scroll_by_key(el: &Element, key: &str) -> bool {
    const LINE: i32 = 20;
    let page = (el.client_height() - LINE).max(LINE);
    match key {
        "ArrowDown" => el.set_scroll_top(el.scroll_top() + LINE),
        "ArrowUp" => el.set_scroll_top(el.scroll_top() - LINE),
        "ArrowRight" => el.set_scroll_left(el.scroll_left() + LINE),
        "ArrowLeft" => el.set_scroll_left(el.scroll_left() - LINE),
        "PageDown" | " " => el.set_scroll_top(el.scroll_top() + page),
        "PageUp" => el.set_scroll_top(el.scroll_top() - page),
        "Home" => el.set_scroll_top(0),
        "End" => el.set_scroll_top(el.scroll_height()),
        _ => return false,
    }
    true
}

/// "F1" … "F12". Overlays swallow these so the browser's own bindings (F5 reload,
/// F3 find, …) never fire inside the app.
pub fn is_function_key(key: &str) -> bool {
    key.strip_prefix('F').is_some_and(|n| n.parse::<u8>().is_ok())
}
