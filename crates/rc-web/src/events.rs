//! Server-sent events from `rcmd web`: remote-control commands (`rcmd ctl`) and live
//! refresh notices, applied as they arrive.

use crate::api;
use crate::app::App;
use rc_core::api::BrowserEvent;
use std::cell::Cell;
use std::rc::Rc;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::{EventSource, MessageEvent};

/// Open the event stream for the lifetime of the page. The browser reconnects by itself
/// when the connection drops; after a reconnect both panels are re-read, since change
/// notices may have been missed meanwhile.
pub fn connect(app: App) {
    let source = match EventSource::new(&api::events_url()) {
        Ok(source) => source,
        Err(_) => return app.error("Live updates unavailable: cannot open the event stream"),
    };
    let on_message = Closure::<dyn Fn(MessageEvent)>::new(move |ev: MessageEvent| {
        let Some(json) = ev.data().as_string() else { return };
        match serde_json::from_str::<BrowserEvent>(&json) {
            Ok(event) => app.apply(event),
            Err(e) => app.error(format!("Bad event from rcmd: {e}")),
        }
    });
    let dropped = Rc::new(Cell::new(false));
    let on_error = Closure::<dyn Fn()>::new({
        let dropped = Rc::clone(&dropped);
        move || dropped.set(true)
    });
    let on_open = Closure::<dyn Fn()>::new(move || {
        if dropped.replace(false) {
            app.refresh_both();
        }
    });
    source.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    source.set_onerror(Some(on_error.as_ref().unchecked_ref()));
    source.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    // The stream and its handlers live as long as the page.
    on_message.forget();
    on_error.forget();
    on_open.forget();
    std::mem::forget(source);
}
