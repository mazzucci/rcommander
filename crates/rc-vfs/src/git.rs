//! Git awareness via the `git` command line: per-entry status for a directory listing and
//! the committed (HEAD) content of a file. Silent no-ops outside a repository or without git.

use rc_core::GitStatus;
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("git")
        // Never take the index lock: a user's own `git commit` must not collide with us.
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(dir)
        .args(args)
        // Variables from the user's shell would point git at a different repository.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_COMMON_DIR")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

/// Status of each entry of `dir` that git reports, by entry name. A directory gets the
/// most significant status of anything inside it.
pub fn statuses(dir: &Path) -> HashMap<String, GitStatus> {
    let mut map = HashMap::new();
    let Some(prefix) = git(dir, &["rev-parse", "--show-prefix"]) else {
        return map;
    };
    let prefix = String::from_utf8_lossy(&prefix).trim_end().to_string();
    // Paths in porcelain output are relative to the repository root.
    let Some(out) = git(
        dir,
        &["status", "--porcelain=v1", "-z", "--untracked-files=normal", "--", "."],
    ) else {
        return map;
    };
    let mut records = out.split(|&b| b == 0).filter(|r| !r.is_empty());
    while let Some(rec) = records.next() {
        if rec.len() < 4 {
            continue;
        }
        let (x, y) = (rec[0], rec[1]);
        let path = String::from_utf8_lossy(&rec[3..]);
        // A rename or copy, staged (X) or detected in the worktree (Y), is followed by its
        // source path as a separate record.
        if matches!(x, b'R' | b'C') || matches!(y, b'R' | b'C') {
            records.next();
        }
        let Some(rest) = path.strip_prefix(&prefix) else {
            continue;
        };
        let (name, inside) = match rest.split_once('/') {
            Some((dir_name, _)) => (dir_name, true),
            None => (rest, false),
        };
        let status = classify(x, y);
        // A folder with something deleted or newly added inside is "modified" as a whole.
        let status = if inside && matches!(status, GitStatus::Deleted | GitStatus::Added) {
            GitStatus::Modified
        } else {
            status
        };
        map.entry(name.to_string())
            .and_modify(|s: &mut GitStatus| *s = (*s).max(status))
            .or_insert(status);
    }
    map
}

fn classify(x: u8, y: u8) -> GitStatus {
    match (x, y) {
        (b'?', _) => GitStatus::Untracked,
        (b'U', _) | (_, b'U') | (b'A', b'A') | (b'D', b'D') => GitStatus::Conflicted,
        (b'D', _) | (_, b'D') => GitStatus::Deleted,
        (b'A', _) => GitStatus::Added,
        (b'R' | b'C', _) | (_, b'R' | b'C') => GitStatus::Renamed,
        _ => GitStatus::Modified,
    }
}

/// The committed content of `file` at HEAD, or `None` if it isn't tracked there.
pub fn head_content(file: &Path) -> Option<Vec<u8>> {
    let dir = file.parent()?;
    let name = file.file_name()?.to_str()?;
    git(dir, &["show", &format!("HEAD:./{name}")])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn statuses_and_head_content() {
        if Command::new("git").arg("--version").output().is_err() {
            return; // no git on this machine
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        sh(repo, &["init", "-q"]);
        sh(repo, &["config", "user.email", "t@example.com"]);
        sh(repo, &["config", "user.name", "t"]);
        std::fs::create_dir(repo.join("sub")).unwrap();
        std::fs::write(repo.join("kept.txt"), "one\n").unwrap();
        std::fs::write(repo.join("changed.txt"), "old\n").unwrap();
        std::fs::write(repo.join("sub/inner.txt"), "x\n").unwrap();
        sh(repo, &["add", "."]);
        sh(repo, &["commit", "-qm", "init"]);
        std::fs::write(repo.join("changed.txt"), "new\n").unwrap();
        std::fs::write(repo.join("sub/inner.txt"), "y\n").unwrap();
        std::fs::write(repo.join("fresh.txt"), "?\n").unwrap();

        let s = statuses(repo);
        assert_eq!(s.get("changed.txt"), Some(&GitStatus::Modified));
        assert_eq!(s.get("fresh.txt"), Some(&GitStatus::Untracked));
        assert_eq!(s.get("sub"), Some(&GitStatus::Modified));
        assert_eq!(s.get("kept.txt"), None);
        assert_eq!(statuses(&repo.join("sub")).get("inner.txt"), Some(&GitStatus::Modified));

        assert_eq!(head_content(&repo.join("changed.txt")).unwrap(), b"old\n");
        assert_eq!(head_content(&repo.join("fresh.txt")), None);

        // An untracked folder shows as one entry without listing everything inside.
        std::fs::create_dir(repo.join("newdir")).unwrap();
        std::fs::write(repo.join("newdir/x"), "x").unwrap();
        assert_eq!(statuses(repo).get("newdir"), Some(&GitStatus::Untracked));

        // A rename git detects in the worktree is reported as one record pair.
        std::fs::rename(repo.join("kept.txt"), repo.join("ab cd.txt")).unwrap();
        std::fs::write(repo.join("cd.txt"), "unrelated\n").unwrap();
        sh(repo, &["add", "-N", "ab cd.txt"]);
        let s = statuses(repo);
        assert_eq!(s.get("ab cd.txt"), Some(&GitStatus::Renamed), "{s:?}");
        assert_eq!(s.get("cd.txt"), Some(&GitStatus::Untracked), "{s:?}");
    }
}
