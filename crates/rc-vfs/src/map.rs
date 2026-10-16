//! Claude maps: a summary of a file or folder with links into it, written by the
//! user's own Claude Code CLI (`claude -p`) with read-only tools, and cached per version
//! of the file or folder.

use crate::local::ProgressFn;
use anyhow::{Context, Result, bail};
use rc_core::Location;
use rc_core::api::{FileMap, MapItem, MapSection, Progress};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Model used when none is configured: good maps at a few cents each.
pub const DEFAULT_MODEL: &str = "sonnet";
/// Spending cap per map, passed to `claude --max-budget-usd`.
const BUDGET_USD: &str = "0.50";
/// A map that takes longer than this is abandoned.
const TIMEOUT: Duration = Duration::from_secs(300);
/// How often a running map reports progress (and can be cancelled).
const TICK: Duration = Duration::from_millis(200);
/// Limits on what is kept of Claude's answer.
const MAX_SECTIONS: usize = 8;
const MAX_ITEMS: usize = 16;
const MAX_TEXT: usize = 600;
const MAX_SUMMARY: usize = 2000;

const SYSTEM_PROMPT: &str = "You write short, accurate maps of files and folders for the side panel of a \
file manager. Look with the tools you have and read only what you need: never read large generated, \
binary or vendored files. Be concrete and brief; no marketing words. Your answer must follow the JSON \
schema you were given.";

/// What Claude must answer.
const SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["summary","sections"],"properties":{"summary":{"type":"string","description":"2-4 plain sentences: what this is and what it is for."},"sections":{"type":"array","maxItems":8,"items":{"type":"object","additionalProperties":false,"required":["heading","items"],"properties":{"heading":{"type":"string"},"items":{"type":"array","maxItems":12,"items":{"type":"object","additionalProperties":false,"required":["text","path","line"],"properties":{"text":{"type":"string","description":"One short line about the target."},"path":{"type":"string","description":"Path relative to the base folder, or empty."},"line":{"type":["integer","null"],"description":"1-based line where it starts, for files."}}}}}}}}}"#;

#[derive(Deserialize)]
struct Answer {
    summary: String,
    sections: Vec<AnswerSection>,
}

#[derive(Deserialize)]
struct AnswerSection {
    heading: String,
    items: Vec<AnswerItem>,
}

#[derive(Deserialize)]
struct AnswerItem {
    text: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    line: Option<i64>,
}

/// The part of `claude -p --output-format json` output we use.
#[derive(Deserialize)]
struct CliOutput {
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    structured_output: Option<serde_json::Value>,
    #[serde(default)]
    total_cost_usd: Option<f64>,
}

/// Makes maps with the configured CLI, and keeps them in `cache` (if any).
pub struct Mapper {
    /// Command (program and arguments); empty finds `claude`.
    pub command: String,
    pub model: String,
    pub cache: Option<PathBuf>,
}

/// A cache file: the map and the version of the target it describes.
#[derive(Serialize, Deserialize)]
struct Cached {
    key: String,
    map: FileMap,
}

impl Mapper {
    fn model(&self) -> &str {
        if self.model.is_empty() {
            DEFAULT_MODEL
        } else {
            &self.model
        }
    }

    /// The map of `loc`, from the cache when it describes the current version (unless
    /// `refresh`), else from Claude. `progress` hears every [`TICK`] with the seconds
    /// spent in `done`; answering `Break` stops Claude.
    pub fn map(&self, loc: &Location, refresh: bool, progress: &mut ProgressFn) -> Result<FileMap> {
        let Location::Local { path } = loc;
        let target = Path::new(path);
        let meta = fs::metadata(target).with_context(|| format!("cannot read {path}"))?;
        let key = version_key(path, &meta, self.model());
        let cache_file = self
            .cache
            .as_ref()
            .map(|dir| dir.join(format!("{:016x}.json", fnv1a(path.as_bytes()))));
        if !refresh
            && let Some(file) = &cache_file
            && let Ok(text) = fs::read_to_string(file)
            && let Ok(cached) = serde_json::from_str::<Cached>(&text)
            && cached.key == key
        {
            return Ok(FileMap {
                cached: true,
                ..cached.map
            });
        }
        let map = self.generate(loc, meta.is_dir(), progress)?;
        if let Some(file) = &cache_file {
            // A cache that cannot be written only costs a new request next time.
            let _ = write_private(file, &serde_json::to_vec(&Cached { key, map: map.clone() })?);
        }
        Ok(map)
    }

    fn generate(&self, loc: &Location, is_dir: bool, progress: &mut ProgressFn) -> Result<FileMap> {
        let Location::Local { path } = loc;
        let base = if is_dir {
            PathBuf::from(path)
        } else {
            Path::new(path)
                .parent()
                .map_or_else(|| PathBuf::from("/"), Path::to_path_buf)
        };
        let prompt = if is_dir {
            format!(
                "Make a map of the folder {path}. Summary: what it is and what it is for. Sections (for \
                 example Entry points, Main parts, Configuration, Tests, Docs) list the files and folders worth \
                 knowing, each with one short line; for code, give the line where the important thing starts. \
                 Paths are relative to {path}. Leave out build output, dependencies and other noise."
            )
        } else {
            format!(
                "Make a map of the file {path}. Summary: what the file is and does. Sections list its main parts \
                 (types, functions, headings, settings blocks…), each with one short line and the line where it \
                 starts; the path of these items is the file name. Add a section of related files nearby if \
                 useful. Paths are relative to {}.",
                base.display()
            )
        };
        let (program, args) = self.program()?;
        let mut cmd = Command::new(&program);
        cmd.args(args)
            .args(["-p", &prompt])
            .args(["--system-prompt", SYSTEM_PROMPT])
            .args(["--model", self.model()])
            .args(["--tools", "Read,Glob,Grep"])
            .args(["--setting-sources", ""])
            .args([
                "--strict-mcp-config",
                "--disable-slash-commands",
                "--no-session-persistence",
            ])
            .args(["--max-budget-usd", BUDGET_USD])
            .args(["--output-format", "json"])
            .args(["--json-schema", SCHEMA])
            .current_dir(&base)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("cannot run {}", program.display()))?;
        // Read both pipes on threads so a chatty child never blocks on a full pipe.
        let mut out = child.stdout.take().expect("piped stdout");
        let mut err = child.stderr.take().expect("piped stderr");
        let out = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = out.read_to_string(&mut s);
            s
        });
        let err = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = err.read_to_string(&mut s);
            s
        });
        let start = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            let report = Progress {
                path: path.clone(),
                done: start.elapsed().as_secs(),
                total: 0,
            };
            if progress(&report).is_break() || start.elapsed() > TIMEOUT {
                kill(&mut child);
                if start.elapsed() > TIMEOUT {
                    bail!("Claude took longer than {} s; stopped", TIMEOUT.as_secs());
                }
                bail!("stopped");
            }
            std::thread::sleep(TICK);
        };
        let (out, err) = (out.join().unwrap_or_default(), err.join().unwrap_or_default());
        let parsed: Option<CliOutput> = serde_json::from_str(out.trim()).ok();
        let Some(cli) = parsed else {
            let detail = if err.trim().is_empty() { out.trim() } else { err.trim() };
            bail!("{} failed ({status}): {}", program.display(), first_line(detail));
        };
        if cli.is_error {
            bail!("Claude: {}", first_line(cli.result.as_deref().unwrap_or("error")));
        }
        let value = match cli.structured_output {
            Some(v) => v,
            // Older CLIs put the JSON text in `result`.
            None => serde_json::from_str(cli.result.as_deref().unwrap_or_default())
                .context("Claude's answer was not a map")?,
        };
        let answer: Answer = serde_json::from_value(value).context("Claude's answer was not a map")?;
        Ok(FileMap {
            location: loc.clone(),
            is_dir,
            summary: clip(&answer.summary, MAX_SUMMARY),
            sections: answer
                .sections
                .into_iter()
                .take(MAX_SECTIONS)
                .map(|s| MapSection {
                    heading: clip(&s.heading, 80),
                    items: s.items.into_iter().take(MAX_ITEMS).map(|i| resolve(&base, i)).collect(),
                })
                .filter(|s| !s.items.is_empty())
                .collect(),
            model: self.model().to_string(),
            generated: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64),
            cost_usd: cli.total_cost_usd,
            cached: false,
        })
    }

    /// The program to run and its leading arguments.
    fn program(&self) -> Result<(PathBuf, Vec<String>)> {
        let mut parts = self.command.split_whitespace().map(str::to_string);
        if let Some(program) = parts.next() {
            crate::check_program(&program)?;
            return Ok((PathBuf::from(program), parts.collect()));
        }
        if crate::check_program("claude").is_ok() {
            return Ok((PathBuf::from("claude"), Vec::new()));
        }
        // Apps started from the Dock or a launcher often have a short PATH.
        let home = dirs::home_dir().unwrap_or_default();
        [".local/bin/claude", ".claude/local/claude", ".npm-global/bin/claude"]
            .iter()
            .map(|p| home.join(p))
            .chain(["/opt/homebrew/bin/claude", "/usr/local/bin/claude"].map(PathBuf::from))
            .find(|p| crate::check_program(&p.to_string_lossy()).is_ok())
            .map(|p| (p, Vec::new()))
            .context("Claude Code (`claude`) was not found: install it, or set its command in Options")
    }
}

/// A link of Claude's answer, made absolute and checked: paths that do not exist are
/// dropped (the text stays), so a made-up link is never clickable.
fn resolve(base: &Path, item: AnswerItem) -> MapItem {
    let raw = item.path.trim();
    let target = (!raw.is_empty()).then(|| {
        let p = Path::new(raw);
        clean(&if p.is_absolute() { p.to_path_buf() } else { base.join(p) })
    });
    let meta = target.as_ref().and_then(|t| fs::metadata(t).ok());
    let line = item.line.filter(|&l| l > 0).map(|l| l as usize);
    MapItem {
        text: clip(&item.text, MAX_TEXT),
        is_dir: meta.as_ref().is_some_and(fs::Metadata::is_dir),
        line: line.filter(|_| meta.as_ref().is_some_and(fs::Metadata::is_file)),
        path: target
            .filter(|_| meta.is_some())
            .map(|t| t.to_string_lossy().into_owned()),
    }
}

/// `a/./b/../c` → `a/c`, without touching the disk.
fn clean(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

fn clip(s: &str, max: usize) -> String {
    let s: String = s
        .trim()
        .chars()
        .map(|c| if c.is_control() && c != '\n' { ' ' } else { c })
        .collect();
    if s.chars().count() > max {
        s.chars().take(max - 1).chain(['…']).collect()
    } else {
        s
    }
}

fn first_line(s: &str) -> String {
    clip(s.lines().find(|l| !l.trim().is_empty()).unwrap_or("no output"), 300)
}

fn kill(child: &mut std::process::Child) {
    // The whole process group: claude may have started helpers.
    #[cfg(unix)]
    // SAFETY: kill(2) with a negative pid signals the group we created at spawn.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGTERM);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Identifies a version of the target: path, modification time, size and model.
fn version_key(path: &str, meta: &fs::Metadata, model: &str) -> String {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    format!("{path}\0{mtime}\0{}\0{model}", meta.len())
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}

/// Write `bytes` to `file` readable by this user only, replacing it atomically.
fn write_private(file: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = file.with_extension(format!("tmp{}", std::process::id()));
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(bytes)?;
    fs::rename(&tmp, file)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ops::ControlFlow;
    use std::os::unix::fs::PermissionsExt;

    /// A stand-in for `claude` that prints `output` and logs its arguments.
    fn fake_claude(dir: &Path, output: &str, delay: u32) -> PathBuf {
        let script = dir.join("fake-claude");
        let out = dir.join("out.json");
        fs::write(&out, output).unwrap();
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nsleep {delay}\ncat {}\n",
                dir.join("args").display(),
                out.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    fn answer(sections: &str) -> String {
        format!(
            r#"{{"type":"result","is_error":false,"total_cost_usd":0.031,"result":"","structured_output":{{"summary":"A small project.","sections":{sections}}}}}"#
        )
    }

    fn go(_: &Progress) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    #[test]
    fn maps_resolve_links_and_drop_made_up_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let tools = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        let sections = r#"[{"heading":"Code","items":[
            {"text":"entry point","path":"src/main.rs","line":1},
            {"text":"source","path":"./src/","line":5},
            {"text":"made up","path":"src/nope.rs","line":3},
            {"text":"no link","path":"","line":null}]},
            {"heading":"Empty","items":[]}]"#;
        let mapper = Mapper {
            command: fake_claude(tools.path(), &answer(sections), 0).to_string_lossy().into(),
            model: String::new(),
            cache: None,
        };
        let loc = Location::local(root.to_string_lossy());
        let map = mapper.map(&loc, false, &mut go).unwrap();
        assert_eq!(map.summary, "A small project.");
        assert_eq!(
            (map.model.as_str(), map.cost_usd, map.cached),
            ("sonnet", Some(0.031), false)
        );
        assert_eq!(map.sections.len(), 1, "empty sections are dropped");
        let items = &map.sections[0].items;
        let main = root.join("src/main.rs").to_string_lossy().into_owned();
        assert_eq!(
            (items[0].path.as_deref(), items[0].line, items[0].is_dir),
            (Some(main.as_str()), Some(1), false)
        );
        assert!(items[1].is_dir && items[1].line.is_none(), "folders have no line");
        assert_eq!(
            (items[2].path.as_ref(), items[2].line),
            (None, None),
            "a made-up path is not a link"
        );
        assert!(items[3].path.is_none());

        // Read-only tools, no settings, MCP or skills; run in the folder.
        let args = fs::read_to_string(tools.path().join("args")).unwrap();
        for expected in [
            "Read,Glob,Grep",
            "--strict-mcp-config",
            "--disable-slash-commands",
            "--json-schema",
            "sonnet",
        ] {
            assert!(args.lines().any(|l| l == expected), "missing {expected} in {args}");
        }
        assert!(!args.contains("Bash") && !args.contains("Write"));
    }

    #[test]
    fn cache_serves_the_same_version_until_it_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let tools = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let file = tmp.path().join("notes.md");
        fs::write(&file, "# Notes\n").unwrap();
        let sections = r##"[{"heading":"Parts","items":[{"text":"title","path":"notes.md","line":1}]}]"##;
        let mapper = Mapper {
            command: fake_claude(tools.path(), &answer(sections), 0).to_string_lossy().into(),
            model: "haiku".into(),
            cache: Some(cache.path().to_path_buf()),
        };
        let loc = Location::local(file.to_string_lossy());
        assert!(!mapper.map(&loc, false, &mut go).unwrap().cached);
        let again = mapper.map(&loc, false, &mut go).unwrap();
        assert!(again.cached && again.model == "haiku");
        assert!(!mapper.map(&loc, true, &mut go).unwrap().cached, "refresh asks again");
        let entry = fs::read_dir(cache.path()).unwrap().next().unwrap().unwrap();
        assert_eq!(entry.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        // A new version of the file is not served from the cache.
        fs::write(&file, "# Notes, longer now\n").unwrap();
        assert!(!mapper.map(&loc, false, &mut go).unwrap().cached);
    }

    #[test]
    fn errors_and_cancel() {
        let tmp = tempfile::tempdir().unwrap();
        let tools = tempfile::tempdir().unwrap();
        let loc = Location::local(tmp.path().to_string_lossy());
        let failing = r#"{"type":"result","is_error":true,"result":"Not logged in · Please run /login"}"#;
        let mapper = Mapper {
            command: fake_claude(tools.path(), failing, 0).to_string_lossy().into(),
            model: String::new(),
            cache: None,
        };
        let err = mapper.map(&loc, false, &mut go).unwrap_err().to_string();
        assert!(err.contains("Not logged in"), "{err}");

        let mapper = Mapper {
            command: fake_claude(tools.path(), "garbage", 0).to_string_lossy().into(),
            model: String::new(),
            cache: None,
        };
        assert!(
            mapper
                .map(&loc, false, &mut go)
                .unwrap_err()
                .to_string()
                .contains("failed")
        );

        // Cancel: a slow CLI is stopped at the next tick.
        let mapper = Mapper {
            command: fake_claude(tools.path(), &answer("[]"), 30).to_string_lossy().into(),
            model: String::new(),
            cache: None,
        };
        let start = Instant::now();
        let mut ticks = 0;
        let err = mapper
            .map(&loc, false, &mut |_| {
                ticks += 1;
                if ticks > 2 {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })
            .unwrap_err();
        assert!(err.to_string().contains("stopped") && start.elapsed() < Duration::from_secs(5));

        let mapper = Mapper {
            command: "/no/such/claude".into(),
            model: String::new(),
            cache: None,
        };
        assert!(mapper.map(&loc, false, &mut go).is_err());
    }
}
