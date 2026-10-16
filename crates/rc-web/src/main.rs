//! Browser front end of rcommander, served by `rcmd web`.

mod api;
mod app;
mod diff_view;
mod dom;
mod events;
mod find;
mod help;
mod hotlist;
mod map_view;
mod modal;
mod options;
mod panel_view;
mod quick_view;
mod rename;
mod viewer;

fn main() {
    console_error_panic_hook::set_once();
    api::init_token();
    leptos::mount::mount_to_body(app::Root);
}
