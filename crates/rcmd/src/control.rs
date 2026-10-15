//! Local control socket. Each running `rcmd` (terminal or web) listens on
//! `<runtime dir>/rcommander/<pid>.sock`; `rcmd ctl` connects to the newest one.
//! The directory is 0700 and sockets are 0600, so only the same user can connect.

use anyhow::{Context, Result, bail};
use rc_core::control::{Command, Reply};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

/// A command waiting to be applied, with the channel its reply goes to.
pub struct Request {
    pub command: Command,
    pub reply: Sender<Reply>,
}

/// Keeps the socket alive; removes the socket file when dropped.
pub struct Listener {
    path: PathBuf,
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn socket_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // SAFETY: getuid has no preconditions and cannot fail.
            std::env::temp_dir().join(format!("rcommander-{}", unsafe { libc::getuid() }))
        });
    base.join("rcommander")
}

/// Start listening. Requests arrive on the returned channel; the caller must answer each
/// one (dropping the reply sender answers with an error).
pub fn listen() -> Result<(Listener, Receiver<Request>)> {
    listen_in(&socket_dir())
}

fn listen_in(dir: &Path) -> Result<(Listener, Receiver<Request>)> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let path = dir.join(format!("{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).with_context(|| format!("cannot listen on {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let tx = tx.clone();
            std::thread::spawn(move || serve(stream, tx));
        }
    });
    Ok((Listener { path }, rx))
}

/// Answer one connection: read a command line, hand it to the app, write the reply.
fn serve(stream: UnixStream, tx: Sender<Request>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut line = String::new();
    if BufReader::new(&stream).read_line(&mut line).is_err() {
        return;
    }
    let reply = match serde_json::from_str::<Command>(&line) {
        Err(e) => Reply::error(format!("bad command: {e}")),
        Ok(command) => {
            let (reply_tx, reply_rx) = mpsc::channel();
            if tx
                .send(Request {
                    command,
                    reply: reply_tx,
                })
                .is_err()
            {
                Reply::error("rcmd is shutting down")
            } else {
                reply_rx
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap_or_else(|_| Reply::error("rcmd did not answer"))
            }
        }
    };
    let mut stream = stream;
    let _ = writeln!(stream, "{}", serde_json::to_string(&reply).unwrap_or_default());
}

/// Send a command to a running instance: the given pid, or else the most recently started
/// one that still answers.
pub fn send(command: &Command, pid: Option<u32>) -> Result<Reply> {
    send_in(&socket_dir(), command, pid)
}

fn send_in(dir: &Path, command: &Command, pid: Option<u32>) -> Result<Reply> {
    let candidates = match pid {
        Some(pid) => vec![dir.join(format!("{pid}.sock"))],
        None => instances(dir),
    };
    for path in candidates {
        // Sockets left behind by a crashed instance refuse connections; skip them.
        let Ok(mut stream) = UnixStream::connect(&path) else {
            continue;
        };
        stream.set_read_timeout(Some(Duration::from_secs(15)))?;
        writeln!(stream, "{}", serde_json::to_string(command)?)?;
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line)?;
        return serde_json::from_str(&line).context("bad reply from rcmd");
    }
    bail!("no running rcmd found (start `rcmd` or `rcmd web` first)")
}

/// Sockets in `dir`, newest first.
fn instances(dir: &Path) -> Vec<PathBuf> {
    let mut socks: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "sock"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    socks.sort_by_key(|s| std::cmp::Reverse(s.0));
    socks.into_iter().map(|(_, p)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_permissions() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("rc");
        let (listener, rx) = listen_in(&dir).unwrap();
        let mode = std::fs::metadata(&listener.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // Answer one request on another thread, like the app's event loop would.
        let app = std::thread::spawn(move || {
            let req = rx.recv().unwrap();
            assert_eq!(req.command, Command::Refresh);
            req.reply.send(Reply::error("nope")).unwrap();
        });
        let reply = send_in(&dir, &Command::Refresh, None).unwrap();
        assert_eq!(reply, Reply::error("nope"));
        app.join().unwrap();
        let path = listener.path.clone();
        drop(listener);
        assert!(!path.exists(), "socket removed on drop");
        assert!(send_in(&dir, &Command::Refresh, None).is_err());
    }

    #[test]
    fn rejects_malformed_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let (_listener, _rx) = listen_in(tmp.path()).unwrap();
        let sock = instances(tmp.path()).remove(0);
        let mut s = UnixStream::connect(sock).unwrap();
        writeln!(s, "{{\"cmd\":\"explode\"}}").unwrap();
        let mut line = String::new();
        BufReader::new(&s).read_line(&mut line).unwrap();
        assert!(line.contains("bad command"), "{line}");
    }
}
