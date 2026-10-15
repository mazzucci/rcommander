//! `rcmd`: a Total Commander-style dual-pane file manager.

mod tui;

use clap::Parser;
use rc_core::Location;
use rc_vfs::Vfs;
use std::sync::Arc;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Directory for the left panel (default: current directory).
    left: Option<String>,
    /// Directory for the right panel (default: home directory).
    right: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let (default_left, default_right) = Vfs::default_locations();
    let pick = |arg: Option<String>, default| arg.map(Location::local).unwrap_or(default);
    let app = tui::App::new(
        Arc::new(Vfs::new()),
        pick(cli.left, default_left),
        pick(cli.right, default_right),
    );
    tui::run(app)
}
