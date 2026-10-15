//! Terminal front end (ratatui + crossterm).

mod app;
mod ui;
mod views;

use anyhow::Result;
pub use app::App;
use app::Effect;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use std::time::Duration;

pub fn run(mut app: App) -> Result<()> {
    // Remote control is optional: without a socket the app still works.
    let control = crate::control::listen();
    let _listener = match control {
        Ok((listener, rx)) => {
            app.control = Some(rx);
            Some(listener)
        }
        Err(e) => {
            app.message = Some((format!("remote control unavailable: {e:#}"), true));
            None
        }
    };
    if let Err(e) = app.enable_live_refresh() {
        app.message = Some((format!("live refresh unavailable: {e:#}"), true));
    }
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    let mut dirty = true;
    loop {
        if dirty {
            terminal.draw(|f| ui::draw(f, app))?;
        }
        // Poll with a timeout so a running copy/move can report progress.
        dirty = app.tick();
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        dirty = true;
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => match app.handle_key(key) {
                Effect::None => {}
                Effect::Quit => return Ok(()),
                Effect::Edit { path, editor } => {
                    ratatui::restore();
                    let editor = editor_command(&editor);
                    let status = run_editor(&editor, &path);
                    // A fresh alternate screen is blank and the next draw repaints everything.
                    *terminal = ratatui::init();
                    if let Err(e) = status {
                        app.message = Some((format!("cannot run `{editor}`: {e}"), true));
                    }
                    app.refresh(0);
                    app.refresh(1);
                }
            },
            _ => {}
        }
    }
}

/// The configured editor, else `$VISUAL`, `$EDITOR`, `vi`.
fn editor_command(configured: &str) -> String {
    [
        configured.to_string(),
        std::env::var("VISUAL").unwrap_or_default(),
        std::env::var("EDITOR").unwrap_or_default(),
    ]
    .into_iter()
    .find(|e| !e.trim().is_empty())
    .unwrap_or_else(|| "vi".into())
}

/// Run an editor command line such as `code --wait` on `path` and wait for it.
fn run_editor(editor: &str, path: &str) -> std::io::Result<std::process::ExitStatus> {
    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap_or("vi");
    std::process::Command::new(program).args(parts).arg(path).status()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_editor_wins() {
        assert_eq!(editor_command("nano -w"), "nano -w");
        assert!(!editor_command("  ").is_empty());
    }
}
