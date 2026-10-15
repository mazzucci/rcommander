//! Small DOM helpers shared by the views.

use wasm_bindgen::JsCast;
use web_sys::{Element, EventTarget, HtmlElement, ScrollIntoViewOptions, ScrollLogicalPosition};

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
