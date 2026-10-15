//! The multi-rename dialog (Shift+F6 / M): a mask, search and replace, case and
//! counter, with a live preview of every new name.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rc_core::Location;
use rc_core::api::FileOp;
use rc_core::rename::{Planned, RenameRule, plan};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Field {
    Mask,
    Search,
    Replace,
    Case,
    Start,
    Digits,
}

impl Field {
    pub const ALL: [Field; 6] = [
        Field::Mask,
        Field::Search,
        Field::Replace,
        Field::Case,
        Field::Start,
        Field::Digits,
    ];
}

pub struct MultiRename {
    pub folder: Location,
    /// The folder's own name, for `[P]`.
    parent: String,
    /// Entries to rename (name, is directory), in panel order.
    entries: Vec<(String, bool)>,
    /// Every name in the folder, to catch renames onto entries that stay.
    existing: Vec<String>,
    pub rule: RenameRule,
    pub field: Field,
    pub plan: Vec<Planned>,
    /// First visible preview row and rows that fit; kept up to date by the renderer.
    pub scroll: usize,
}

#[derive(Debug, PartialEq)]
pub enum Action {
    None,
    Close,
    Run(FileOp),
}

impl MultiRename {
    pub fn new(folder: Location, entries: Vec<(String, bool)>, existing: Vec<String>) -> Self {
        let parent = folder.file_name().unwrap_or_default().to_string();
        let mut r = MultiRename {
            folder,
            parent,
            entries,
            existing,
            rule: RenameRule::default(),
            field: Field::Mask,
            plan: Vec::new(),
            scroll: 0,
        };
        r.update();
        r
    }

    fn update(&mut self) {
        self.plan = plan(&self.rule, &self.entries, &self.parent, &self.existing);
    }

    pub fn problems(&self) -> usize {
        self.plan.iter().filter(|p| p.problem.is_some()).count()
    }

    pub fn changes(&self) -> usize {
        self.plan.iter().filter(|p| p.changes()).count()
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        let at = Field::ALL.iter().position(|f| *f == self.field).unwrap_or(0);
        let typing = !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let rule = &mut self.rule;
        match (key.code, self.field) {
            (KeyCode::Esc, _) => return Action::Close,
            (KeyCode::Enter, _) => {
                if self.problems() > 0 || self.changes() == 0 {
                    return Action::None;
                }
                let renames = self
                    .plan
                    .iter()
                    .filter(|p| p.changes())
                    .map(|p| (p.from.clone(), p.to.clone()))
                    .collect();
                return Action::Run(FileOp::RenameMany {
                    folder: self.folder.clone(),
                    renames,
                });
            }
            (KeyCode::Tab | KeyCode::Down, _) => self.field = Field::ALL[(at + 1) % Field::ALL.len()],
            (KeyCode::BackTab | KeyCode::Up, _) => {
                self.field = Field::ALL[(at + Field::ALL.len() - 1) % Field::ALL.len()]
            }
            (KeyCode::PageDown, _) => self.scroll += 10,
            (KeyCode::PageUp, _) => self.scroll = self.scroll.saturating_sub(10),
            (KeyCode::Char(' ') | KeyCode::Right, Field::Case) => rule.case = rule.case.next(),
            (KeyCode::Backspace, Field::Mask) => drop(rule.mask.pop()),
            (KeyCode::Backspace, Field::Search) => drop(rule.search.pop()),
            (KeyCode::Backspace, Field::Replace) => drop(rule.replace.pop()),
            (KeyCode::Backspace, Field::Start) => rule.counter_start /= 10,
            (KeyCode::Backspace, Field::Digits) => rule.counter_digits /= 10,
            (KeyCode::Char(c), Field::Start) if typing && c.is_ascii_digit() => {
                rule.counter_start = rule
                    .counter_start
                    .saturating_mul(10)
                    .saturating_add(u64::from(c as u8 - b'0'))
            }
            (KeyCode::Char(c), Field::Digits) if typing && c.is_ascii_digit() => {
                rule.counter_digits = (rule.counter_digits * 10 + usize::from(c as u8 - b'0')).min(20)
            }
            (KeyCode::Char(c), Field::Mask) if typing => rule.mask.push(c),
            (KeyCode::Char(c), Field::Search) if typing => rule.search.push(c),
            (KeyCode::Char(c), Field::Replace) if typing => rule.replace.push(c),
            _ => return Action::None,
        }
        self.update();
        Action::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn typed(r: &mut MultiRename, s: &str) {
        for c in s.chars() {
            r.handle_key(key(KeyCode::Char(c)));
        }
    }

    fn dialog() -> MultiRename {
        let entries = vec![("IMG_1.JPG".into(), false), ("IMG_2.JPG".into(), false)];
        let existing = vec!["IMG_1.JPG".into(), "IMG_2.JPG".into(), "trip-1.jpg".into()];
        MultiRename::new(Location::local("/photos/trip"), entries, existing)
    }

    #[test]
    fn edit_fields_and_preview() {
        let mut r = dialog();
        assert_eq!(r.changes(), 0);
        assert_eq!(r.handle_key(key(KeyCode::Enter)), Action::None, "nothing to rename yet");
        for _ in 0..7 {
            r.handle_key(key(KeyCode::Backspace));
        }
        typed(&mut r, "[P]-[C].[E]");
        r.field = Field::Case;
        r.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(r.plan[0].to, "trip-1.jpg");
        assert_eq!(
            r.plan[0].problem.as_deref(),
            Some("already exists"),
            "trip-1.jpg is another file"
        );
        r.field = Field::Start;
        r.handle_key(key(KeyCode::Backspace));
        typed(&mut r, "7");
        r.field = Field::Digits;
        r.handle_key(key(KeyCode::Backspace));
        typed(&mut r, "2");
        let to: Vec<_> = r.plan.iter().map(|p| p.to.as_str()).collect();
        assert_eq!(to, ["trip-07.jpg", "trip-08.jpg"]);
        let Action::Run(FileOp::RenameMany { folder, renames }) = r.handle_key(key(KeyCode::Enter)) else {
            panic!("no rename");
        };
        assert_eq!(folder, Location::local("/photos/trip"));
        assert_eq!(renames[1], ("IMG_2.JPG".to_string(), "trip-08.jpg".to_string()));
        assert_eq!(r.handle_key(key(KeyCode::Esc)), Action::Close);
    }

    #[test]
    fn problems_block_the_rename() {
        let mut r = dialog();
        r.rule.mask = "trip-1.jpg".into();
        r.update();
        assert!(r.problems() > 0);
        assert_eq!(r.handle_key(key(KeyCode::Enter)), Action::None);
    }
}
