//! Local control socket. Each running `rcmd` (terminal or web) listens on
//! `<runtime dir>/rcommander/<pid>.sock`; `rcmd ctl` connects to the newest one.
//! The directory is 0700 and sockets are 0600, so only the same user can connect.

use anyhow::{Context, Result, bail};
use rc_core::control::{Command, Reply};
use std::io::{BufRead, BufReader, Read, Write};
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

/// This user's private directory for rcommander's sockets and scratch files:
/// `$XDG_RUNTIME_DIR/rcommander`, or `<temp>/rcommander-<uid>/rcommander` without one.
/// Every directory we create or reuse must be a real directory (not a symlink), owned by
/// us and closed to everyone else; anything else is refused, since other users can plant
/// things in a shared temp dir.
pub fn private_dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => {
            // SAFETY: getuid has no preconditions and cannot fail.
            let fallback = std::env::temp_dir().join(format!("rcommander-{}", unsafe { libc::getuid() }));
            secure_dir(&fallback)?;
            fallback
        }
    };
    let dir = base.join("rcommander");
    secure_dir(&dir)?;
    Ok(dir)
}

/// Create `dir` as 0700 if missing, then insist it is ours and private.
pub fn secure_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("cannot create {}", dir.display())),
    }
    let meta = std::fs::symlink_metadata(dir)?;
    // SAFETY: getuid has no preconditions and cannot fail.
    let me = unsafe { libc::getuid() };
    if !meta.is_dir() || meta.uid() != me {
        bail!("{} is not a directory owned by you; refusing to use it", dir.display());
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Start listening. Requests arrive on the returned channel; the caller must answer each
/// one (dropping the reply sender answers with an error).
pub fn listen() -> Result<(Listener, Receiver<Request>)> {
    listen_in(&private_dir()?)
}

fn listen_in(dir: &Path) -> Result<(Listener, Receiver<Request>)> {
    secure_dir(dir)?;
    let path = dir.join(format!("{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    // The private directory is the access control; also bind with a strict umask so the
    // socket never exists with looser permissions.
    // SAFETY: umask only swaps the process file-mode mask.
    let old = unsafe { libc::umask(0o077) };
    let bound = UnixListener::bind(&path);
    // SAFETY: as above.
    unsafe { libc::umask(old) };
    let listener = bound.with_context(|| format!("cannot listen on {}", path.display()))?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let tx = tx.clone();
                    std::thread::spawn(move || serve(stream, tx));
                }
                // e.g. out of file descriptors: back off instead of spinning.
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    });
    Ok((Listener { path }, rx))
}

/// Answer one connection: read a command line, hand it to the app, write the reply.
fn serve(stream: UnixStream, tx: Sender<Request>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    // One command per connection, and no command needs more than this.
    let mut line = String::new();
    if BufReader::new((&stream).take(64 * 1024)).read_line(&mut line).is_err() {
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
/// one that still answers. `timeout` bounds the wait for the reply.
pub fn send(command: &Command, pid: Option<u32>, timeout: Duration) -> Result<Reply> {
    send_in(&private_dir()?, command, pid, timeout)
}

fn send_in(dir: &Path, command: &Command, pid: Option<u32>, timeout: Duration) -> Result<Reply> {
    let candidates = match pid {
        Some(pid) => vec![dir.join(format!("{pid}.sock"))],
        None => instances(dir),
    };
    for path in candidates {
        let mut stream = match UnixStream::connect(&path) {
            Ok(s) => s,
            // Left behind by an instance that was killed: clean it up.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            Err(_) => continue,
        };
        stream.set_read_timeout(Some(timeout))?;
        writeln!(stream, "{}", serde_json::to_string(command)?)?;
        let mut line = String::new();
        BufReader::new(&stream)
            .read_line(&mut line)
            .map_err(|_| anyhow::anyhow!("rcmd did not answer within {}s", timeout.as_secs()))?;
        return serde_json::from_str(&line).context("bad reply from rcmd");
    }
    match pid {
        Some(pid) => bail!("no running rcmd with pid {pid}"),
        None => bail!("no running rcmd found (start `rcmd` or `rcmd web` first)"),
    }
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
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        // Answer one request on another thread, like the app's event loop would.
        let app = std::thread::spawn(move || {
            let req = rx.recv().unwrap();
            assert_eq!(req.command, Command::Refresh);
            req.reply.send(Reply::error("nope")).unwrap();
        });
        let reply = send_in(&dir, &Command::Refresh, None, Duration::from_secs(5)).unwrap();
        assert_eq!(reply, Reply::error("nope"));
        app.join().unwrap();
        let path = listener.path.clone();
        drop(listener);
        assert!(!path.exists(), "socket removed on drop");
        assert!(send_in(&dir, &Command::Refresh, None, Duration::from_secs(5)).is_err());
    }

    #[test]
    fn refuses_symlinked_or_foreign_dirs_and_cleans_stale_sockets() {
        let tmp = tempfile::tempdir().unwrap();
        let victim = tmp.path().join("victim");
        std::fs::create_dir(&victim).unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o755)).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        assert!(listen_in(&link).is_err());
        let mode = std::fs::metadata(&victim).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "the symlink target was not touched");

        // A socket left by a dead instance is removed, and the live one still answers.
        let dir = tmp.path().join("rc");
        let (_live, rx) = listen_in(&dir).unwrap();
        let stale = dir.join("99999999.sock");
        drop(UnixListener::bind(&stale).unwrap());
        let app = std::thread::spawn(move || {
            let req = rx.recv().unwrap();
            req.reply.send(Reply::ok()).unwrap();
        });
        assert!(
            send_in(&dir, &Command::Refresh, None, Duration::from_secs(5))
                .unwrap()
                .ok
        );
        app.join().unwrap();
        assert!(!stale.exists(), "stale socket removed");
        assert!(
            send_in(&dir, &Command::Refresh, Some(1), Duration::from_secs(1))
                .unwrap_err()
                .to_string()
                .contains("pid 1")
        );
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
