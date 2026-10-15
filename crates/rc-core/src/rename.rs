//! Multi-rename (Shift+F6): new names for many entries of one folder from a mask, a
//! search-and-replace and a case rule, as in Total Commander's multi-rename tool.
//! Pure functions, so both UIs preview exactly what will happen.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NameCase {
    #[default]
    Keep,
    Lower,
    Upper,
    /// First letter of each word upper case, the rest lower case (the extension too).
    Title,
}

impl NameCase {
    pub const ALL: [NameCase; 4] = [NameCase::Keep, NameCase::Lower, NameCase::Upper, NameCase::Title];

    pub fn label(self) -> &'static str {
        match self {
            NameCase::Keep => "unchanged",
            NameCase::Lower => "lower case",
            NameCase::Upper => "UPPER CASE",
            NameCase::Title => "Title Case",
        }
    }

    pub fn next(self) -> NameCase {
        let i = Self::ALL.iter().position(|c| *c == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    fn apply(self, s: &str) -> String {
        match self {
            NameCase::Keep => s.to_string(),
            NameCase::Lower => s.to_lowercase(),
            NameCase::Upper => s.to_uppercase(),
            NameCase::Title => {
                let mut out = String::with_capacity(s.len());
                let mut start = true;
                for c in s.chars() {
                    if start {
                        out.extend(c.to_uppercase());
                    } else {
                        out.extend(c.to_lowercase());
                    }
                    // A dot does not start a word: "Pic.jpg", not "Pic.Jpg".
                    start = !(c.is_alphanumeric() || c == '.' || c == '\'');
                }
                out
            }
        }
    }
}

/// How to build the new names.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RenameRule {
    /// `[N]` name without extension, `[E]` extension, `[C]` counter, `[P]` parent
    /// folder's name; anything else is copied. A `.` right before an empty `[E]` is
    /// dropped, so `[N].[E]` keeps names without an extension as they are.
    pub mask: String,
    /// Replaced everywhere in the result (case-sensitive). Empty: no replacing.
    pub search: String,
    pub replace: String,
    pub case: NameCase,
    pub counter_start: u64,
    pub counter_step: u64,
    /// Zero-padded to at least this many digits.
    pub counter_digits: usize,
}

impl Default for RenameRule {
    fn default() -> Self {
        RenameRule {
            mask: "[N].[E]".into(),
            search: String::new(),
            replace: String::new(),
            case: NameCase::Keep,
            counter_start: 1,
            counter_step: 1,
            counter_digits: 1,
        }
    }
}

/// One entry and what it would become.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Planned {
    pub from: String,
    pub to: String,
    /// Why this rename cannot be done; nothing is renamed while any entry has one.
    pub problem: Option<String>,
}

impl Planned {
    pub fn changes(&self) -> bool {
        self.from != self.to
    }
}

/// `name` split into stem and extension: "a.tar.gz" → ("a.tar", "gz"); a leading dot is
/// part of the name (".bashrc" has none); folders have no extension.
pub fn split_ext(name: &str, is_dir: bool) -> (&str, &str) {
    match name.rfind('.') {
        // A trailing dot is part of the name ("notes." has no extension).
        Some(i) if i > 0 && i + 1 < name.len() && !is_dir => (&name[..i], &name[i + 1..]),
        _ => (name, ""),
    }
}

/// The new name of one entry; `counter` is its number in the batch.
pub fn new_name(rule: &RenameRule, name: &str, is_dir: bool, counter: u64, parent: &str) -> String {
    let (stem, ext) = split_ext(name, is_dir);
    let mut out = String::new();
    let mut rest = rule.mask.as_str();
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after = &rest[open..];
        let Some(close) = after.find(']') else {
            out.push_str(after);
            rest = "";
            break;
        };
        match &after[1..close] {
            "N" => out.push_str(stem),
            "E" if ext.is_empty() => {
                if out.ends_with('.') {
                    out.pop();
                }
            }
            "E" => out.push_str(ext),
            "C" => out.push_str(&format!("{counter:0width$}", width = rule.counter_digits.min(20))),
            "P" => out.push_str(parent),
            // Not a placeholder: keep it as typed.
            _ => out.push_str(&after[..=close]),
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    if !rule.search.is_empty() {
        out = out.replace(&rule.search, &rule.replace);
    }
    rule.case.apply(&out)
}

/// New names for `entries` (name, is directory) of the folder named `parent`, in order,
/// with what would go wrong. `existing` is every name in the folder.
pub fn plan(rule: &RenameRule, entries: &[(String, bool)], parent: &str, existing: &[String]) -> Vec<Planned> {
    let mut planned: Vec<Planned> = entries
        .iter()
        .enumerate()
        .map(|(i, (name, is_dir))| {
            let counter = rule
                .counter_start
                .saturating_add(rule.counter_step.saturating_mul(i as u64));
            Planned {
                from: name.clone(),
                to: new_name(rule, name, *is_dir, counter, parent),
                problem: None,
            }
        })
        .collect();
    let renamed: HashSet<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
    let mut targets: HashMap<String, usize> = HashMap::new();
    for p in &planned {
        *targets.entry(p.to.clone()).or_default() += 1;
    }
    for p in &mut planned {
        p.problem = if p.to.is_empty() {
            Some("empty name".into())
        } else if p.to.contains('/') {
            Some("contains /".into())
        } else if p.to.chars().any(char::is_control) {
            Some("contains a control character".into())
        } else if p.to.len() > 255 {
            Some("name too long".into())
        } else if p.to == "." || p.to == ".." {
            Some("not a valid name".into())
        } else if targets[&p.to] > 1 {
            Some("same name as another".into())
        } else if p.changes() && !renamed.contains(p.to.as_str()) && existing.contains(&p.to) {
            Some("already exists".into())
        } else {
            None
        };
    }
    planned
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(rule: &RenameRule, list: &[&str]) -> Vec<String> {
        let entries: Vec<(String, bool)> = list.iter().map(|n| (n.to_string(), false)).collect();
        plan(rule, &entries, "photos", &[]).into_iter().map(|p| p.to).collect()
    }

    #[test]
    fn default_mask_keeps_names() {
        let rule = RenameRule::default();
        let same = ["a.txt", "Makefile", ".bashrc", "x.tar.gz", "notes."];
        assert_eq!(names(&rule, &same), same);
    }

    #[test]
    fn placeholders_counter_and_literal_brackets() {
        let rule = RenameRule {
            mask: "[P]_[C]_[N] [x].[E]".into(),
            counter_start: 9,
            counter_step: 2,
            counter_digits: 3,
            ..RenameRule::default()
        };
        assert_eq!(
            names(&rule, &["IMG.JPG", "b.png"]),
            ["photos_009_IMG [x].JPG", "photos_011_b [x].png"]
        );
        let rule = RenameRule {
            mask: "[N".into(),
            ..RenameRule::default()
        };
        assert_eq!(names(&rule, &["a.txt"]), ["[N"], "an unclosed bracket is literal");
    }

    #[test]
    fn replace_then_case() {
        let rule = RenameRule {
            search: "_".into(),
            replace: " ".into(),
            case: NameCase::Title,
            ..RenameRule::default()
        };
        assert_eq!(names(&rule, &["my_HOLIDAY_pic.JPG"]), ["My Holiday Pic.jpg"]);
        let rule = RenameRule {
            case: NameCase::Lower,
            ..RenameRule::default()
        };
        assert_eq!(names(&rule, &["ÄBC.TXT"]), ["äbc.txt"]);
    }

    #[test]
    fn folders_have_no_extension() {
        let rule = RenameRule {
            mask: "[N]-old".into(),
            ..RenameRule::default()
        };
        let p = plan(&rule, &[("v1.2".into(), true)], "p", &[]);
        assert_eq!(p[0].to, "v1.2-old");
    }

    #[test]
    fn problems_are_reported() {
        let entries: Vec<(String, bool)> = ["a.txt", "b.txt", "c.txt"]
            .iter()
            .map(|n| (n.to_string(), false))
            .collect();
        let existing: Vec<String> = ["a.txt", "b.txt", "c.txt", "taken.txt"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        // Duplicates.
        let rule = RenameRule {
            mask: "same.[E]".into(),
            ..RenameRule::default()
        };
        assert!(
            plan(&rule, &entries, "p", &existing)
                .iter()
                .all(|p| p.problem.as_deref() == Some("same name as another"))
        );

        // Onto a file that stays.
        let rule = RenameRule {
            search: "a.".into(),
            replace: "taken.".into(),
            ..RenameRule::default()
        };
        let p = plan(&rule, &entries, "p", &existing);
        assert_eq!(p[0].problem.as_deref(), Some("already exists"));
        assert_eq!(p[1].problem, None);

        // Onto a name that is itself being renamed away: fine (a → b, b → c, c → d).
        let rule = RenameRule {
            mask: "[C].txt".into(),
            counter_start: 0,
            ..RenameRule::default()
        };
        let shifted = [("0.txt".to_string(), false), ("1.txt".to_string(), false)];
        let p = plan(&rule, &shifted, "p", &["0.txt".into(), "1.txt".into()]);
        assert_eq!((p[0].to.as_str(), p[1].to.as_str()), ("0.txt", "1.txt"));
        let rule = RenameRule {
            mask: "[C].txt".into(),
            counter_start: 1,
            ..RenameRule::default()
        };
        let p = plan(&rule, &shifted, "p", &["0.txt".into(), "1.txt".into()]);
        assert!(p.iter().all(|p| p.problem.is_none()), "{p:?}");

        // Bad names.
        let long = "x".repeat(256);
        for (mask, why) in [
            ("", "empty name"),
            ("a/[N]", "contains /"),
            ("..", "not a valid name"),
            ("a\nb", "contains a control character"),
            (long.as_str(), "name too long"),
        ] {
            let rule = RenameRule {
                mask: mask.into(),
                ..RenameRule::default()
            };
            let p = plan(&rule, &entries[..1], "p", &existing);
            assert_eq!(p[0].problem.as_deref(), Some(why), "{mask}");
        }
    }
}
