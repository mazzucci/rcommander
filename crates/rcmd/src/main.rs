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
        /// Seconds to wait for the instance to answer.
        #[arg(long, global = true, default_value_t = 15)]
        timeout: u64,
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
    /// Open the side-by-side diff of two files. A left side of `-` reads the "before"
    /// content from stdin (for showing an edit without writing a temp file yourself).
    Diff { left: String, right: String },
    /// Open a file in the viewer.
    View {
        path: String,
        /// Scroll to this line (1-based) and highlight it.
        #[arg(long)]
        line: Option<usize>,
    },
    /// Record that FILE is about to be edited, reading its current content from stdin.
    /// The first recording of a file keeps its original for "Claude's changes".
    Track { file: String },
    /// Show "Claude's changes", the files an agent edited, in a panel.
    Changes {
        #[arg(long, value_enum, default_value_t = SideArg::Active)]
        side: SideArg,
        /// Forget all recorded changes instead (the files stay as they are).
        #[arg(long)]
        clear: bool,
    },
    /// Select entries matching a mask such as '*.rs'.
    Select {
        mask: String,
        #[arg(long, value_enum, default_value_t = SideArg::Active)]
        side: SideArg,
    },
    /// Re-read both panels.
    Refresh,
    /// Show a message in the status bar.
    Message {
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
    },
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

/// Save stdin as the "before" copy of `file` in the private scratch dir and return its
/// path. Named after a hash of the full path, so same-named files don't collide.
fn snapshot_from_stdin(file: &str) -> anyhow::Result<String> {
    use std::hash::{Hash, Hasher};
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = control::private_dir()?.join("snapshots");
    control::secure_dir(&dir)?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    file.hash(&mut hasher);
    let name = file.rsplit('/').next().unwrap_or("file");
    let path = dir.join(format!("{:016x}-{name}", hasher.finish()));
    let mut content = Vec::new();
    std::io::stdin().read_to_end(&mut content)?;
    let _ = std::fs::remove_file(&path);
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    std::io::Write::write_all(&mut out, &content)?;
    Ok(path.to_string_lossy().into_owned())
}

fn ctl(action: CtlAction, pid: Option<u32>, timeout: u64) -> anyhow::Result<()> {
    let command = match action {
        CtlAction::Goto { path, side } => RemoteCommand::Goto {
            path: absolute(&path),
            side: side.into(),
        },
        CtlAction::Diff { left, right } => {
            let right = absolute(&right);
            let left = if left == "-" {
                snapshot_from_stdin(&right)?
            } else {
                absolute(&left)
            };
            RemoteCommand::Diff { left, right }
        }
        CtlAction::View { path, line } => RemoteCommand::View {
            path: absolute(&path),
            line,
        },
        CtlAction::Track { file } => {
            use std::io::Read;
            // One byte over the cap is enough to know it is too large.
            let mut original = Vec::new();
            std::io::stdin()
                .take(rc_vfs::changes::MAX_ORIGINAL + 1)
                .read_to_end(&mut original)?;
            control::changes_log()?.record(std::path::Path::new(&absolute(&file)), &original)?;
            // Panels showing the log pick the new entry up; no running rcmd is fine.
            let _ = control::send(&RemoteCommand::Refresh, pid, std::time::Duration::from_secs(timeout));
            return Ok(());
        }
        CtlAction::Changes { side, clear: true } => {
            let _ = side;
            control::changes_log()?.clear()?;
            let _ = control::send(&RemoteCommand::Refresh, pid, std::time::Duration::from_secs(timeout));
            return Ok(());
        }
        CtlAction::Changes { side, clear: false } => RemoteCommand::Goto {
            path: control::changes_log()?.links_dir().to_string_lossy().into_owned(),
            side: side.into(),
        },
        CtlAction::Select { mask, side } => RemoteCommand::Select {
            mask,
            side: side.into(),
        },
        CtlAction::Refresh => RemoteCommand::Refresh,
        CtlAction::Message { text } => RemoteCommand::Message { text: text.join(" ") },
    };
    let reply = control::send(&command, pid, std::time::Duration::from_secs(timeout))?;
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
        Some(Command::Ctl { pid, timeout, action }) => ctl(action, pid, timeout),
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
