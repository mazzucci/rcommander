//! `rcmd`: a Total Commander-style dual-pane file manager.
//!
//! `rcmd [LEFT] [RIGHT]` starts the terminal UI; `rcmd web [LEFT] [RIGHT]` serves the
//! browser UI on 127.0.0.1 and opens it.

mod control;
mod server;
mod tui;

use clap::{Parser, Subcommand};
use rc_core::Location;
use rc_core::control::{Command as RemoteCommand, Side};
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
    /// Control a running rcmd (terminal or web), e.g. from scripts or Claude Code.
    Ctl {
        /// Target a specific instance (default: the most recently started).
        #[arg(long, global = true)]
        pid: Option<u32>,
        #[command(subcommand)]
        action: CtlAction,
    },
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

#[derive(Subcommand)]
enum CtlAction {
    /// Show a directory, or a file's directory with the cursor on the file.
    Goto {
        path: String,
        #[arg(long, value_enum, default_value_t = SideArg::Active)]
        side: SideArg,
    },
    /// Open the side-by-side diff of two files.
    Diff { left: String, right: String },
    /// Open a file in the viewer.
    View { path: String },
    /// Select entries matching a mask such as '*.rs'.
    Select {
        mask: String,
        #[arg(long, value_enum, default_value_t = SideArg::Active)]
        side: SideArg,
    },
    /// Re-read both panels.
    Refresh,
    /// Show a message in the status bar.
    Message { text: String },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum SideArg {
    Active,
    Other,
    Left,
    Right,
}

impl From<SideArg> for Side {
    fn from(s: SideArg) -> Side {
        match s {
            SideArg::Active => Side::Active,
            SideArg::Other => Side::Other,
            SideArg::Left => Side::Left,
            SideArg::Right => Side::Right,
        }
    }
}

/// Paths are resolved against the caller's directory, not the instance's.
fn absolute(path: &str) -> String {
    let Location::Local { path } = Vfs::new().normalize(Location::local(path), None);
    path
}

fn ctl(action: CtlAction, pid: Option<u32>) -> anyhow::Result<()> {
    let command = match action {
        CtlAction::Goto { path, side } => RemoteCommand::Goto {
            path: absolute(&path),
            side: side.into(),
        },
        CtlAction::Diff { left, right } => RemoteCommand::Diff {
            left: absolute(&left),
            right: absolute(&right),
        },
        CtlAction::View { path } => RemoteCommand::View { path: absolute(&path) },
        CtlAction::Select { mask, side } => RemoteCommand::Select {
            mask,
            side: side.into(),
        },
        CtlAction::Refresh => RemoteCommand::Refresh,
        CtlAction::Message { text } => RemoteCommand::Message { text },
    };
    let reply = control::send(&command, pid)?;
    match reply.error {
        None if reply.ok => Ok(()),
        err => anyhow::bail!(err.unwrap_or_else(|| "failed".into())),
    }
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
        Some(Command::Ctl { pid, action }) => ctl(action, pid),
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
