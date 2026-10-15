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
                Effect::Edit(path) => {
                    ratatui::restore();
                    let editor = std::env::var("VISUAL")
                        .or_else(|_| std::env::var("EDITOR"))
                        .unwrap_or("vi".into());
                    // Allow editors with arguments, e.g. EDITOR="code --wait".
                    let mut parts = editor.split_whitespace();
                    let status = std::process::Command::new(parts.next().unwrap_or("vi"))
                        .args(parts)
                        .arg(&path)
                        .status();
                    // A fresh alternate screen is blank and the next draw repaints everything.
                    *terminal = ratatui::init();
                    if let Err(e) = status {
                        app.message = Some((format!("cannot run {editor}: {e}"), true));
                    }
                    app.refresh(0);
                    app.refresh(1);
                }
            },
            _ => {}
        }
    }
}
