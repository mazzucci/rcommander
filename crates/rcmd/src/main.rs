//! `rcmd`: a Total Commander-style dual-pane file manager.
//!
//! `rcmd [LEFT] [RIGHT]` starts the terminal UI; `rcmd web [LEFT] [RIGHT]` serves the
//! browser UI on 127.0.0.1 and opens it.

mod server;
mod tui;

use clap::{Parser, Subcommand};
use rc_core::Location;
use rc_vfs::Vfs;
use std::sync::Arc;

#[derive(Parser)]
#[command(version, about, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    dirs: Dirs,
}

#[derive(clap::Args)]
struct Dirs {
    /// Directory for the left panel (default: current directory).
    left: Option<String>,
    /// Directory for the right panel (default: home directory).
    right: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the browser UI on 127.0.0.1 and open it.
    Web {
        #[command(flatten)]
        dirs: Dirs,
        #[arg(long, default_value_t = 7878)]
        port: u16,
        /// Don't open a browser, just print the URL.
        #[arg(long)]
        no_open: bool,
        /// Browser application to use, e.g. "Google Chrome" (default: system browser).
        #[arg(long)]
        browser: Option<String>,
    },
}

impl Dirs {
    fn locations(self) -> (Location, Location) {
        let (left, right) = Vfs::default_locations();
        (
            self.left.map(Location::local).unwrap_or(left),
            self.right.map(Location::local).unwrap_or(right),
        )
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let vfs = Arc::new(Vfs::new());
    match cli.command {
        None => {
            let (left, right) = cli.dirs.locations();
            let settings = rc_vfs::settings::load()?;
            let path = rc_vfs::settings::path();
            tui::run(tui::App::with_settings(vfs, left, right, settings, Some(path)))
        }
        Some(Command::Web {
            dirs,
            port,
            no_open,
            browser,
        }) => {
            let (left, right) = dirs.locations();
            let settings = rc_vfs::settings::load()?;
            let settings_path = Some(rc_vfs::settings::path());
            server::run(
                vfs,
                server::Config {
                    port,
                    open_browser: !no_open,
                    browser,
                    left,
                    right,
                    settings,
                    settings_path,
                },
            )
        }
    }
}
