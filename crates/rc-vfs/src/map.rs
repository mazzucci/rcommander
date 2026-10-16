//! Claude maps: a summary of a file or folder with links into it, written by the
//! user's own Claude Code CLI (`claude -p`) with read-only tools, and cached per version
//! of the file or folder.

use crate::local::ProgressFn;
use anyhow::{Context, Result, bail};
use rc_core::Location;
use rc_core::api::{FileMap, MapAnswer, MapExchange, MapItem, MapSection, Progress};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Model used when none is configured: good maps at a few cents each.
/// Model used when none is configured: a strong one, for a first map that is right.
pub const DEFAULT_MODEL: &str = "opus";
/// Folders below the mapped one Claude may read when none is configured.
pub const DEFAULT_DEPTH: usize = 4;
/// Tool steps for a first map (a rough overview) and for a question (details).
const MAP_TURNS: u32 = 16;
const ASK_TURNS: u32 = 30;
/// Folders never read: dependencies, build output, caches, version control.
const SKIPPED: [&str; 12] = [
    "node_modules",
    ".git",
    "target",
    "dist",
    "build",
    "vendor",
    ".venv",
    "venv",
    "__pycache__",
    ".next",
    ".cache",
    ".gradle",
];
/// Files and folders never read because they usually hold credentials.
const SECRETS: [&str; 9] = [
    ".env",
    ".env.*",
    ".ssh/**",
    ".gnupg/**",
    ".aws/**",
    "*.pem",
    "*.key",
    "id_rsa*",
    "id_ed25519*",
];
/// Lines of the listing given to Claude at most.
const LISTING_LINES: usize = 250;
/// Spending cap per map, passed to `claude --max-budget-usd`.
const BUDGET_USD: &str = "0.50";
/// Spending cap per question (they may read more than a first map).
const ASK_BUDGET_USD: &str = "1.00";
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
binary or vendored files. Be concrete and brief; no marketing words. Never quote passwords, keys, tokens \
or other credentials you come across, not even partly. Your answer must follow the JSON schema you were \
given.";

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

/// What Claude must answer to a question about a map.
const ASK_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["answer","links"],"properties":{"answer":{"type":"string","description":"The answer: short paragraphs or '- ' lists, `code` for names."},"links":{"type":"array","maxItems":20,"items":{"type":"object","additionalProperties":false,"required":["text","path","line"],"properties":{"text":{"type":"string","description":"One short line about the target."},"path":{"type":"string","description":"Path relative to the base folder."},"line":{"type":["integer","null"],"description":"1-based line, for files."}}}}}}"#;

#[derive(Deserialize)]
struct AskAnswer {
    answer: String,
    links: Vec<AnswerItem>,
}

/// Longest question accepted, and how many earlier exchanges go along as context.
pub const MAX_QUESTION: usize = 2000;
const THREAD_CONTEXT: usize = 6;

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
    /// `success`, `error_max_turns`, `error_max_budget_usd`…
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    errors: Vec<String>,
}

/// Makes maps with the configured CLI, and keeps them in `cache` (if any).
pub struct Mapper {
    /// Command (program and arguments); empty finds `claude`.
    pub command: String,
    pub model: String,
    /// Folders below the mapped one Claude may read; 0 means [`DEFAULT_DEPTH`].
    pub depth: usize,
    pub cache: Option<PathBuf>,
}

/// A cache file: the map and the version of the target it describes.
#[derive(Serialize, Deserialize)]
struct Cached {
    key: String,
    map: FileMap,
}

impl Mapper {
    fn depth(&self) -> usize {
        if self.depth == 0 {
            DEFAULT_DEPTH
        } else {
            self.depth.min(20)
        }
    }

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
        if !meta.is_file() && !meta.is_dir() {
            bail!("{path} is not a regular file or folder");
        }
        let model = self.model();
        if model.starts_with('-') || !model.chars().all(|c| c.is_ascii_alphanumeric() || "._:-[]".contains(c)) {
            bail!("invalid model name {model:?} (see Options)");
        }
        let key = version_key(path, &meta, model);
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
        let rules = format!(
            "Rules: only name files and folders that appear in the listing below or that you opened; give a \
             line number only when you saw that line in a file you read, else null; if you are not sure, leave \
             it out. Paths are relative to {}.",
            base.display()
        );
        let prompt = if is_dir {
            format!(
                "Make a rough first map of the folder {path}: what it is and what it is for, its main parts, and \
                 where to start (sections such as Entry points, Main parts, Configuration, Tests, Docs). Open at \
                 most a few key files (README, manifest, entry points) to confirm what things are; the user can \
                 ask for details later, so do not try to be complete. Leave out build output, dependencies and \
                 other noise.\n\n{rules}\n\nListing (two levels; deeper folders show only how many entries \
                 they hold):\n{}",
                listing(&base, 2)
            )
        } else {
            format!(
                "Make a rough first map of the file {path} ({} lines): what the file is and does, and its main \
                 parts (types, functions, headings, settings blocks…), each with one short line and the line where \
                 it starts; the path of these items is the file name. Read the file (for a long one, the parts you \
                 need). Add a few related files nearby if useful.\n\n{rules}\n\nThe file's folder:\n{}",
                line_count(Path::new(path)).map_or("?".into(), |n| n.to_string()),
                listing(&base, 1)
            )
        };
        let (value, cost) = self.run(&prompt, &base, SCHEMA, path, (MAP_TURNS, BUDGET_USD), progress)?;
        let answer: Answer = serde_json::from_value(value).context("Claude's answer was not a map")?;
        let mut lines = LineCounts::default();
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
                    items: s
                        .items
                        .into_iter()
                        .take(MAX_ITEMS)
                        .map(|i| resolve(&base, i, &mut lines))
                        .collect(),
                })
                .filter(|s| !s.items.is_empty())
                .collect(),
            model: self.model().to_string(),
            generated: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64),
            cost_usd: cost,
            cached: false,
        })
    }

    /// Ask Claude a question about `map` (after the exchanges in `thread`). Claude can
    /// look at the files again, with the same read-only tools as for the map.
    pub fn ask(
        &self,
        map: &FileMap,
        thread: &[MapExchange],
        question: &str,
        progress: &mut ProgressFn,
    ) -> Result<MapAnswer> {
        let question = question.trim();
        if question.is_empty() {
            bail!("empty question");
        }
        if question.chars().count() > MAX_QUESTION {
            bail!("the question is longer than {MAX_QUESTION} characters");
        }
        let Location::Local { path } = &map.location;
        let meta = fs::metadata(path).with_context(|| format!("cannot read {path}"))?;
        let base = if meta.is_dir() {
            PathBuf::from(path)
        } else {
            Path::new(path)
                .parent()
                .map_or_else(|| PathBuf::from("/"), Path::to_path_buf)
        };
        let model = self.model();
        if model.starts_with('-') || !model.chars().all(|c| c.is_ascii_alphanumeric() || "._:-[]".contains(c)) {
            bail!("invalid model name {model:?} (see Options)");
        }
        let rel = |p: &str| {
            let prefix = format!("{}/", base.display().to_string().trim_end_matches('/'));
            p.strip_prefix(&prefix).unwrap_or(p).to_string()
        };
        let item = |i: &MapItem| match (&i.path, i.line) {
            (Some(p), Some(l)) => format!("- {}:{l}: {}", rel(p), i.text),
            (Some(p), None) => format!("- {}: {}", rel(p), i.text),
            (None, _) => format!("- {}", i.text),
        };
        let mut context = format!("Summary: {}\n", map.summary);
        for section in &map.sections {
            context += &format!("\n{}\n", section.heading);
            for i in &section.items {
                context += &item(i);
                context += "\n";
            }
        }
        let earlier: String = thread
            .iter()
            .rev()
            .take(THREAD_CONTEXT)
            .rev()
            .map(|x| {
                format!(
                    "Question: {}\nYour answer: {}\n\n",
                    x.question,
                    clip(&x.answer.text, 1500)
                )
            })
            .collect();
        let what = if meta.is_dir() { "folder" } else { "file" };
        let mut prompt = format!("You made this map of the {what} {path}:\n\n{context}\n");
        if !earlier.is_empty() {
            prompt += &format!("The user already asked about it:\n\n{earlier}");
        }
        prompt += &format!(
            "The user asks: {question}\n\nAnswer, looking at the files as needed (you can read up to {} folders \
             down). Base what you say on files you read; if you could not find out, say so. In `answer` write plain \
             text (short paragraphs or '- ' lists, `code` for names), concise. In `links` list the files, folders \
             and lines your answer refers to, paths relative to {}; give a line only if you saw it.",
            self.depth(),
            base.display()
        );
        let (value, cost) = self.run(&prompt, &base, ASK_SCHEMA, path, (ASK_TURNS, ASK_BUDGET_USD), progress)?;
        let answer: AskAnswer =
            serde_json::from_value(value).context("Claude's answer was not in the expected form")?;
        Ok(MapAnswer {
            text: clip(&answer.answer, 6000),
            links: answer
                .links
                .into_iter()
                .take(20)
                .map(|i| resolve(&base, i, &mut LineCounts::default()))
                .collect(),
            cost_usd: cost,
        })
    }

    /// Run Claude Code on `prompt` in `base` (read-only, restricted, see the module
    /// docs) and return its answer, which follows `schema`, and the cost. `path` labels
    /// the progress reports.
    fn run(
        &self,
        prompt: &str,
        base: &Path,
        schema: &str,
        path: &str,
        (max_turns, budget): (u32, &str),
        progress: &mut ProgressFn,
    ) -> Result<(serde_json::Value, Option<f64>)> {
        let path = path.to_string();
        // Hard limits Claude Code enforces (Read rules also filter Glob and Grep): nothing
        // deeper than `depth` folders below `base`, nothing in dependency or build folders.
        let mut deny = vec![format!("Read(./{}**)", "*/".repeat(self.depth() + 2))];
        deny.extend(SKIPPED.iter().map(|d| format!("Read(./**/{d}/**)")));
        // Obvious credentials stay out of the prompt whatever Claude decides.
        deny.extend(SECRETS.iter().map(|p| format!("Read(./**/{p})")));
        let (program, args) = self.program()?;
        let mut cmd = Command::new(&program);
        cmd.args(args)
            // The prompt goes in on stdin: as an argument it could pass the system's
            // limit for one argument (128 KiB on Linux).
            .arg("-p")
            .args(["--system-prompt", SYSTEM_PROMPT])
            .args(["--model", self.model()])
            .args(["--tools", "Read,Glob,Grep"])
            .args(["--setting-sources", ""])
            // Restricted: no code-running tools or settings files (so no hooks from the
            // folder), file tools confined to the working folder; no MCP servers either.
            .args([
                "--restricted",
                "--strict-mcp-config",
                "--disable-slash-commands",
                "--no-session-persistence",
            ])
            .args(["--max-budget-usd", budget])
            .args(["--max-turns", &max_turns.to_string()])
            .args(["--disallowedTools", &deny.join(",")])
            .args(["--output-format", "json"])
            .args(["--json-schema", schema])
            .current_dir(base)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        // Linux: if rcommander dies (even killed), Claude is stopped too.
        #[cfg(target_os = "linux")]
        // SAFETY: prctl is async-signal-safe and touches only this (new) process.
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut cmd, || {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("cannot run {}", program.display()))?;
        // Write the prompt and read both pipes on threads, so neither side can block.
        let mut input = child.stdin.take().expect("piped stdin");
        let prompt_bytes = prompt.as_bytes().to_vec();
        std::thread::spawn(move || {
            use std::io::Write;
            let _ = input.write_all(&prompt_bytes);
        });
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
            match cli.subtype.as_deref() {
                Some("error_max_turns") => bail!(
                    "Claude ran out of steps ({max_turns}) before answering: try again, or ask something narrower"
                ),
                Some("error_max_budget_usd") => bail!("Claude reached the ${budget} spending cap before answering"),
                _ => {}
            }
            let why = cli
                .result
                .filter(|r| !r.trim().is_empty())
                .or_else(|| (!cli.errors.is_empty()).then(|| cli.errors.join("; ")));
            bail!("Claude: {}", first_line(why.as_deref().unwrap_or("error")));
        }
        let value = match cli.structured_output {
            Some(v) => v,
            // Older CLIs put the JSON text in `result`.
            None => serde_json::from_str(cli.result.as_deref().unwrap_or_default())
                .context("Claude's answer was not in the expected form")?,
        };
        Ok((value, cli.total_cost_usd))
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
fn resolve(base: &Path, item: AnswerItem, lines: &mut LineCounts) -> MapItem {
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
        // A line past the end of the file is a guess: drop it.
        line: line.filter(|&l| match (&meta, &target) {
            (Some(m), Some(t)) if m.is_file() => lines.fits(t, m.len(), l),
            _ => false,
        }),
        // Only paths that exist and survive as text (locations are UTF-8) are links.
        path: target
            .filter(|_| meta.is_some())
            .and_then(|t| t.to_str().map(str::to_string)),
    }
}

/// Line counts of the files links point at, each file read once per answer.
#[derive(Default)]
struct LineCounts(std::collections::HashMap<PathBuf, Option<usize>>);

impl LineCounts {
    /// Whether `line` can be in the file. Files over 8 MB are not read: their lines stay.
    fn fits(&mut self, path: &Path, size: u64, line: usize) -> bool {
        if size > 8 << 20 {
            return true;
        }
        let n = *self.0.entry(path.to_path_buf()).or_insert_with(|| line_count(path));
        n.is_some_and(|n| line <= n.max(1))
    }
}

/// Lines in a text file (up to 8 MB read); `None` if it cannot be read.
fn line_count(path: &Path) -> Option<usize> {
    let mut bytes = Vec::new();
    fs::File::open(path).ok()?.take(8 << 20).read_to_end(&mut bytes).ok()?;
    let n = bytes.iter().filter(|&&b| b == b'\n').count();
    Some(if bytes.last().is_some_and(|&b| b != b'\n') {
        n + 1
    } else {
        n
    })
}

/// A listing of `dir` for the prompt, `levels` (1 or 2) deep: every entry of the folder
/// first (folders marked with `/`, dependency and build folders noted but not entered),
/// then, while [`LISTING_LINES`] allows, what each folder holds; folders left over show
/// how many entries they hold.
pub fn listing(dir: &Path, levels: usize) -> String {
    let entries = |dir: &Path| -> Vec<(String, bool)> {
        let Ok(read) = fs::read_dir(dir) else { return Vec::new() };
        let mut v: Vec<(String, bool)> = read
            .flatten()
            .map(|e| {
                (
                    e.file_name().to_string_lossy().into_owned(),
                    // Does not follow links: no loops.
                    e.file_type().is_ok_and(|t| t.is_dir()),
                )
            })
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| rc_core::panel::natural_cmp(&a.0, &b.0)));
        v
    };
    let count = |dir: &Path| {
        let n = fs::read_dir(dir).map_or(0, |r| r.take(10_000).count());
        if n == 10_000 {
            "10000+".to_string()
        } else {
            n.to_string()
        }
    };
    let top = entries(dir);
    if top.is_empty() {
        return "(empty)".into();
    }
    let shown = top.len().min(LISTING_LINES);
    let mut budget = LISTING_LINES.saturating_sub(shown);
    let mut out = Vec::new();
    for (name, is_dir) in &top[..shown] {
        let path = dir.join(name);
        if !is_dir {
            out.push(name.clone());
        } else if SKIPPED.contains(&name.as_str()) {
            out.push(format!("{name}/ (not read)"));
        } else {
            let inner = if levels > 1 { entries(&path) } else { Vec::new() };
            if levels > 1 && !inner.is_empty() && inner.len() <= budget {
                budget -= inner.len();
                out.push(format!("{name}/"));
                for (child, child_dir) in inner {
                    let note = match (child_dir, SKIPPED.contains(&child.as_str())) {
                        (true, true) => " (not read)".to_string(),
                        (true, false) => format!(" ({} entries)", count(&path.join(&child))),
                        _ => String::new(),
                    };
                    out.push(format!("  {child}{}{note}", if child_dir { "/" } else { "" }));
                }
            } else {
                out.push(format!("{name}/ ({} entries)", count(&path)));
            }
        }
    }
    if shown < top.len() {
        out.push(format!("… and {} more", top.len() - shown));
    }
    out.join("\n")
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
/// For a folder, the newest modification among its direct entries counts too, so
/// editing a file in it makes a new map (deeper changes do not; `I` regenerates).
fn version_key(path: &str, meta: &fs::Metadata, model: &str) -> String {
    let nanos = |m: &fs::Metadata| {
        m.modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos())
    };
    let mut mtime = nanos(meta);
    if meta.is_dir()
        && let Ok(entries) = fs::read_dir(path)
    {
        for entry in entries.flatten().take(20_000) {
            if let Ok(m) = entry.metadata() {
                mtime = mtime.max(nanos(&m));
            }
        }
    }
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
    static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = file.with_extension(format!("tmp{}-{n}", std::process::id()));
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
                "#!/bin/sh\n{{ printf '%s\\n' \"$@\"; cat; }} > {}\nsleep {delay}\ncat {}\n",
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
            depth: 0,
            cache: None,
        };
        let loc = Location::local(root.to_string_lossy());
        let map = mapper.map(&loc, false, &mut go).unwrap();
        assert_eq!(map.summary, "A small project.");
        assert_eq!(
            (map.model.as_str(), map.cost_usd, map.cached),
            ("opus", Some(0.031), false)
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
            "--restricted",
            "--strict-mcp-config",
            "--disable-slash-commands",
            "--json-schema",
            "opus",
            "--max-turns",
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
            depth: 0,
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
        // A folder's map is new once a file directly in it changes.
        let dir = Location::local(tmp.path().to_string_lossy());
        assert!(!mapper.map(&dir, false, &mut go).unwrap().cached);
        assert!(mapper.map(&dir, false, &mut go).unwrap().cached);
        std::thread::sleep(Duration::from_millis(20));
        fs::write(&file, "# Notes, edited again\n").unwrap();
        assert!(!mapper.map(&dir, false, &mut go).unwrap().cached);
    }

    #[test]
    fn questions_carry_the_map_and_the_thread() {
        let tmp = tempfile::tempdir().unwrap();
        let tools = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/config.rs"), "a\nb\nfn load() {}\n").unwrap();
        let reply = r#"{"is_error":false,"total_cost_usd":0.02,"structured_output":{"answer":"Config is loaded in `load`.","links":[{"text":"load()","path":"src/config.rs","line":3},{"text":"made up","path":"nope.rs","line":1}]}}"#;
        let mapper = Mapper {
            command: fake_claude(tools.path(), reply, 0).to_string_lossy().into(),
            model: String::new(),
            depth: 0,
            cache: None,
        };
        let map = FileMap {
            location: Location::local(tmp.path().to_string_lossy()),
            is_dir: true,
            summary: "A tiny app.".into(),
            sections: vec![MapSection {
                heading: "Code".into(),
                items: vec![MapItem {
                    text: "config".into(),
                    path: Some(tmp.path().join("src/config.rs").to_string_lossy().into()),
                    is_dir: false,
                    line: Some(3),
                }],
            }],
            model: "sonnet".into(),
            generated: 0,
            cost_usd: None,
            cached: false,
        };
        let earlier = MapExchange {
            question: "What is this?".into(),
            answer: MapAnswer {
                text: "An app.".into(),
                links: vec![],
                cost_usd: None,
            },
        };
        let answer = mapper
            .ask(&map, &[earlier], "Where is the config loaded?", &mut go)
            .unwrap();
        assert_eq!(answer.text, "Config is loaded in `load`.");
        assert_eq!(answer.links[0].line, Some(3));
        assert!(answer.links[1].path.is_none(), "made-up links stay text");
        let args = fs::read_to_string(tools.path().join("args")).unwrap();
        for part in [
            "A tiny app.",
            "src/config.rs:3: config",
            "What is this?",
            "An app.",
            "Where is the config loaded?",
        ] {
            assert!(args.contains(part), "prompt lacks {part:?}");
        }
        assert!(args.lines().any(|l| l == "--restricted") && args.lines().any(|l| l == "Read,Glob,Grep"));
        assert!(mapper.ask(&map, &[], "   ", &mut go).is_err());
        assert!(mapper.ask(&map, &[], &"x".repeat(MAX_QUESTION + 1), &mut go).is_err());
    }

    #[test]
    fn listing_is_shallow_and_skips_noise() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path();
        for d in ["src/deep/deeper", "node_modules/x", "docs"] {
            fs::create_dir_all(p.join(d)).unwrap();
        }
        fs::write(p.join("README.md"), "").unwrap();
        fs::write(p.join("src/main.rs"), "").unwrap();
        fs::write(p.join("src/deep/a.rs"), "").unwrap();
        let text = listing(p, 2);
        assert_eq!(
            text, "docs/ (0 entries)\nnode_modules/ (not read)\nsrc/\n  deep/ (2 entries)\n  main.rs\nREADME.md",
            "{text}"
        );
        assert_eq!(listing(p, 1).lines().count(), 4);
    }

    #[test]
    fn listing_shows_every_top_level_entry_before_going_deeper() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..40 {
            let d = tmp.path().join(format!("dir{i:02}"));
            fs::create_dir(&d).unwrap();
            for j in 0..20 {
                fs::write(d.join(format!("f{j}")), "").unwrap();
            }
        }
        fs::write(tmp.path().join("README.md"), "").unwrap();
        let text = listing(tmp.path(), 2);
        assert!(text.lines().count() <= LISTING_LINES + 1);
        assert!(
            text.contains("\nREADME.md") && text.contains("dir39/ (20 entries)"),
            "{text}"
        );
    }

    #[test]
    fn running_out_of_steps_or_budget_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let tools = tempfile::tempdir().unwrap();
        let loc = Location::local(tmp.path().to_string_lossy());
        for (out, expected) in [
            (
                r#"{"subtype":"error_max_turns","is_error":true,"result":null,"errors":["Reached maximum number of turns (16)"]}"#,
                "ran out of steps",
            ),
            (
                r#"{"subtype":"error_max_budget_usd","is_error":true,"result":null}"#,
                "spending cap",
            ),
            (
                r#"{"subtype":"error_during_execution","is_error":true,"result":null,"errors":["boom"]}"#,
                "Claude: boom",
            ),
        ] {
            let mapper = Mapper {
                command: fake_claude(tools.path(), out, 0).to_string_lossy().into(),
                model: String::new(),
                depth: 0,
                cache: None,
            };
            let err = mapper.map(&loc, true, &mut go).unwrap_err().to_string();
            assert!(err.contains(expected), "{err}");
        }
    }

    #[test]
    fn guardrails_are_passed_and_bad_lines_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let tools = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("one.txt"), "only line\n").unwrap();
        let sections = r#"[{"heading":"H","items":[{"text":"real","path":"one.txt","line":1},{"text":"guessed","path":"one.txt","line":99}]}]"#;
        let mapper = Mapper {
            command: fake_claude(tools.path(), &answer(sections), 0).to_string_lossy().into(),
            model: String::new(),
            depth: 2,
            cache: None,
        };
        let map = mapper
            .map(&Location::local(tmp.path().to_string_lossy()), false, &mut go)
            .unwrap();
        let lines: Vec<_> = map.sections[0].items.iter().map(|i| i.line).collect();
        assert_eq!(lines, [Some(1), None], "a line past the end is dropped");
        let args: Vec<String> = fs::read_to_string(tools.path().join("args"))
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        let after = |flag: &str| {
            args.iter()
                .position(|a| a == flag)
                .map(|i| args[i + 1].clone())
                .unwrap()
        };
        assert_eq!(after("--max-turns"), MAP_TURNS.to_string());
        let deny = after("--disallowedTools");
        assert!(
            deny.starts_with("Read(./*/*/*/*/**),"),
            "depth 2 allows files in a/b/: {deny}"
        );
        assert!(deny.contains("Read(./**/node_modules/**)") && deny.contains("Read(./**/.git/**)"));
        assert!(
            deny.contains("Read(./**/.env)") && deny.contains("Read(./**/.ssh/**)"),
            "{deny}"
        );
        assert_eq!(after("-p"), "--system-prompt", "the prompt is not an argument");
        // The prompt spans lines in the log: look at all of it.
        let all = args.join("\n");
        assert!(all.contains("rough first map") && all.contains("only name files") && all.contains("\none.txt"));
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
            depth: 0,
            cache: None,
        };
        let err = mapper.map(&loc, false, &mut go).unwrap_err().to_string();
        assert!(err.contains("Not logged in"), "{err}");

        let mapper = Mapper {
            command: fake_claude(tools.path(), "garbage", 0).to_string_lossy().into(),
            model: String::new(),
            depth: 0,
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
            depth: 0,
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

        // Special files and odd model names are refused before anything runs.
        let fifo = tmp.path().join("pipe");
        let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let err = mapper
            .map(&Location::local(fifo.to_string_lossy()), false, &mut go)
            .unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");
        let odd = Mapper {
            command: mapper.command.clone(),
            model: "--dangerously".into(),
            depth: 0,
            cache: None,
        };
        assert!(
            odd.map(&loc, false, &mut go)
                .unwrap_err()
                .to_string()
                .contains("invalid model")
        );

        let mapper = Mapper {
            command: "/no/such/claude".into(),
            model: String::new(),
            depth: 0,
            cache: None,
        };
        assert!(mapper.map(&loc, false, &mut go).is_err());
    }
}
