//! Find files: walk a folder tree for names matching a mask and, optionally, files
//! containing a text.

use crate::local::ProgressFn;
use anyhow::{Result, bail};
use rc_core::Location;
use rc_core::api::{FindHit, FindRequest, FindResult, Progress};
use rc_core::panel::{natural_cmp, wildcard_match};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

/// Most hits a search returns.
pub const MAX_HITS: usize = 5000;
/// Matching lines are cut to this many characters for display.
const MAX_TEXT: usize = 200;
/// Bytes looked at to tell binary files (skipped by content search) from text.
const SNIFF: usize = 8192;
/// Content search reports progress (and can be cancelled) every this many bytes.
const REPORT_EVERY: u64 = 4 << 20;

/// Search `req.root` (not following symlinked folders). `progress` hears about every
/// folder and every searched file; answering `Break` stops with what was found so far.
pub fn find(req: &FindRequest, progress: &mut ProgressFn) -> Result<FindResult> {
    find_limited(req, MAX_HITS, progress)
}

pub fn find_limited(req: &FindRequest, limit: usize, progress: &mut ProgressFn) -> Result<FindResult> {
    let Location::Local { path: root } = &req.root;
    if !Path::new(root).is_dir() {
        bail!("{root} is not a folder");
    }
    let mask = name_mask(&req.name);
    let needle = Needle::new(&req.contains);
    let mut res = FindResult::default();
    let mut stack = vec![PathBuf::from(root)];
    while let Some(dir) = stack.pop() {
        let report = Progress {
            path: dir.to_string_lossy().into_owned(),
            done: res.hits.len() as u64,
            total: 0,
        };
        if progress(&report).is_break() {
            res.cancelled = true;
            break;
        }
        let Ok(read) = fs::read_dir(&dir) else {
            res.unreadable += 1;
            continue;
        };
        let mut entries: Vec<(String, bool)> = read
            .filter_map(|e| {
                let e = e.ok()?;
                // file_type does not follow symlinks: linked folders are not entered.
                let is_dir = e.file_type().ok()?.is_dir();
                Some((e.file_name().to_string_lossy().into_owned(), is_dir))
            })
            .filter(|(name, _)| req.hidden || !name.starts_with('.'))
            .collect();
        entries.sort_by(|a, b| natural_cmp(&a.0, &b.0));
        let mut subdirs = Vec::new();
        for (name, is_dir) in entries {
            let path = dir.join(&name);
            if is_dir && name != ".git" {
                subdirs.push(path.clone());
            }
            if !mask.as_ref().is_none_or(|m| wildcard_match(m, &name)) {
                continue;
            }
            let hit = match &needle {
                None => Some(FindHit {
                    path: path.to_string_lossy().into_owned(),
                    is_dir,
                    line: None,
                    text: None,
                }),
                Some(_) if is_dir => None,
                Some(needle) => match search_file(&path, needle, &mut res, progress) {
                    ControlFlow::Break(()) => {
                        res.cancelled = true;
                        return Ok(res);
                    }
                    ControlFlow::Continue(found) => found.map(|(line, text)| FindHit {
                        path: path.to_string_lossy().into_owned(),
                        is_dir: false,
                        line: Some(line),
                        text: Some(text),
                    }),
                },
            };
            if let Some(hit) = hit {
                if res.hits.len() == limit {
                    res.truncated = true;
                    return Ok(res);
                }
                res.hits.push(hit);
            }
        }
        // Popped in name order: a folder's own hits come before those of its subfolders.
        stack.extend(subdirs.into_iter().rev());
    }
    Ok(res)
}

/// The name mask as `wildcard_match` takes it: plain words become `*word*`, so
/// "main" finds "main.rs". `None` matches everything.
fn name_mask(name: &str) -> Option<String> {
    let parts: Vec<String> = name
        .split([';', ' '])
        .filter(|p| !p.is_empty())
        .map(|p| {
            if p.contains(['*', '?']) {
                p.to_string()
            } else {
                format!("*{p}*")
            }
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(";"))
}

/// Text to look for, with smart case: case-sensitive only if it has capitals.
struct Needle {
    text: String,
    ignore_case: bool,
}

impl Needle {
    fn new(text: &str) -> Option<Needle> {
        (!text.is_empty()).then(|| Needle {
            ignore_case: !text.chars().any(char::is_uppercase),
            text: text.to_string(),
        })
    }

    fn is_in(&self, line: &str) -> bool {
        if self.ignore_case {
            line.to_lowercase().contains(&self.text)
        } else {
            line.contains(&self.text)
        }
    }
}

/// The first line of a text file containing `needle`, as (1-based number, text).
/// Binary files, and anything that is not a regular file, never match.
fn search_file(
    path: &Path,
    needle: &Needle,
    res: &mut FindResult,
    progress: &mut ProgressFn,
) -> ControlFlow<(), Option<(usize, String)>> {
    // Follows symlinks; FIFOs and devices would block or never end.
    if !fs::metadata(path).is_ok_and(|m| m.is_file()) {
        return ControlFlow::Continue(None);
    }
    let Ok(file) = fs::File::open(path) else {
        res.unreadable += 1;
        return ControlFlow::Continue(None);
    };
    let mut reader = BufReader::with_capacity(64 << 10, file);
    match reader.fill_buf() {
        Ok(head) if head[..head.len().min(SNIFF)].contains(&0) => return ControlFlow::Continue(None),
        Ok(_) => {}
        Err(_) => {
            res.unreadable += 1;
            return ControlFlow::Continue(None);
        }
    }
    let (mut line, mut number, mut read, mut next_report) = (Vec::new(), 0, 0u64, REPORT_EVERY);
    loop {
        line.clear();
        match reader.by_ref().read_until(b'\n', &mut line) {
            Ok(0) => return ControlFlow::Continue(None),
            Ok(n) => read += n as u64,
            Err(_) => {
                res.unreadable += 1;
                return ControlFlow::Continue(None);
            }
        }
        number += 1;
        let text = String::from_utf8_lossy(&line);
        if needle.is_in(&text) {
            return ControlFlow::Continue(Some((number, display_line(&text))));
        }
        if read >= next_report {
            next_report += REPORT_EVERY;
            let report = Progress {
                path: path.to_string_lossy().into_owned(),
                done: res.hits.len() as u64,
                total: 0,
            };
            progress(&report)?;
        }
    }
}

/// A matching line as shown in the results: trimmed, control characters blanked, cut short.
fn display_line(line: &str) -> String {
    let mut out: String = line
        .trim()
        .chars()
        .take(MAX_TEXT)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if line.trim().chars().nth(MAX_TEXT).is_some() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(root: &Path, name: &str, contains: &str, hidden: bool) -> FindRequest {
        FindRequest {
            root: Location::local(root.to_string_lossy()),
            name: name.into(),
            contains: contains.into(),
            hidden,
        }
    }

    fn run(r: &FindRequest) -> Vec<String> {
        let root = match &r.root {
            Location::Local { path } => path.clone(),
        };
        find(r, &mut |_| ControlFlow::Continue(()))
            .unwrap()
            .hits
            .into_iter()
            .map(|h| h.path.strip_prefix(&format!("{root}/")).unwrap().to_string())
            .collect()
    }

    fn tree() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path();
        for d in ["src/nested", ".hidden", ".git/objects", "docs"] {
            fs::create_dir_all(p.join(d)).unwrap();
        }
        fs::write(p.join("src/main.rs"), "fn main() {\n    println!(\"Hello\");\n}\n").unwrap();
        fs::write(p.join("src/nested/lib.rs"), "pub fn hello() {}\n").unwrap();
        fs::write(p.join("docs/readme.md"), "say hello\n").unwrap();
        fs::write(p.join("docs/blob.bin"), b"hello\0world").unwrap();
        fs::write(p.join(".hidden/secret.rs"), "hello").unwrap();
        fs::write(p.join(".git/objects/x.rs"), "hello").unwrap();
        fs::write(p.join("Cargo.toml"), "[package]\n").unwrap();
        tmp
    }

    #[test]
    fn finds_names_by_mask_or_substring_in_tree_order() {
        let tmp = tree();
        assert_eq!(
            run(&req(tmp.path(), "*.rs", "", false)),
            ["src/main.rs", "src/nested/lib.rs"]
        );
        assert_eq!(
            run(&req(tmp.path(), "main", "", false)),
            ["src/main.rs"],
            "plain words match inside names"
        );
        assert_eq!(
            run(&req(tmp.path(), "*.toml;*.md", "", false)),
            ["Cargo.toml", "docs/readme.md"]
        );
        assert_eq!(
            run(&req(tmp.path(), "NESTED", "", false)),
            ["src/nested"],
            "folders match names too"
        );
        assert_eq!(
            run(&req(tmp.path(), "", "", false)).len(),
            8,
            "empty mask matches everything"
        );
    }

    #[test]
    fn hidden_entries_only_on_request_and_never_inside_git() {
        let tmp = tree();
        assert_eq!(
            run(&req(tmp.path(), "*.rs", "", true)),
            [".hidden/secret.rs", "src/main.rs", "src/nested/lib.rs"]
        );
        assert_eq!(
            run(&req(tmp.path(), ".git", "", true)),
            [".git"],
            "the .git folder itself can match"
        );
    }

    #[test]
    fn content_search_uses_smart_case_and_skips_binaries() {
        let tmp = tree();
        let r = req(tmp.path(), "", "hello", false);
        assert_eq!(run(&r), ["docs/readme.md", "src/main.rs", "src/nested/lib.rs"]);
        let r = req(tmp.path(), "*.rs", "Hello", false);
        assert_eq!(run(&r), ["src/main.rs"], "capitals make it case-sensitive");
        let hit = &find(&r, &mut |_| ControlFlow::Continue(())).unwrap().hits[0];
        assert_eq!((hit.line, hit.text.as_deref()), (Some(2), Some("println!(\"Hello\");")));
    }

    #[test]
    fn symlinked_folders_are_not_followed() {
        let tmp = tree();
        std::os::unix::fs::symlink(tmp.path().join("src"), tmp.path().join("docs/link")).unwrap();
        assert_eq!(
            run(&req(tmp.path(), "*.rs", "", false)),
            ["src/main.rs", "src/nested/lib.rs"]
        );
        assert_eq!(run(&req(tmp.path(), "link", "", false)), ["docs/link"]);
    }

    #[test]
    fn content_search_skips_fifos() {
        let tmp = tree();
        let fifo = tmp.path().join("pipe");
        let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert_eq!(run(&req(tmp.path(), "pipe", "hello", false)), Vec::<String>::new());
    }

    #[test]
    fn stops_at_the_limit_or_when_cancelled() {
        let tmp = tree();
        let r = req(tmp.path(), "", "", false);
        let res = find_limited(&r, 2, &mut |_| ControlFlow::Continue(())).unwrap();
        assert!(res.truncated && res.hits.len() == 2);
        let mut calls = 0;
        let res = find(&r, &mut |_| {
            calls += 1;
            if calls > 1 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .unwrap();
        assert!(res.cancelled && !res.truncated);
        assert_eq!(res.hits.len(), 3, "the root's own entries were found before stopping");
        assert!(
            find(&req(&tmp.path().join("Cargo.toml"), "", "", false), &mut |_| {
                ControlFlow::Continue(())
            })
            .is_err()
        );
    }

    #[test]
    fn long_lines_are_cut_for_display() {
        let long = format!("x{}\ty", "a".repeat(300));
        let shown = display_line(&long);
        assert_eq!(shown.chars().count(), MAX_TEXT + 1);
        assert!(shown.ends_with('…'));
        assert_eq!(display_line("  a\tb \n"), "a b");
    }
}
