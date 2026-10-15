//! Directory comparison ("Synchronize dirs" light): marks each file by how it relates
//! to the file of the same name in the other panel.

use crate::Entry;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CmpMark {
    /// Only exists on this side.
    Orphan,
    /// Exists on both sides and this one is newer.
    Newer,
    /// Exists on both sides and this one is older.
    Older,
    /// Same timestamp but different size.
    Different,
    /// Same size and timestamp.
    Same,
}

impl CmpMark {
    pub fn symbol(self) -> char {
        match self {
            CmpMark::Orphan => '+',
            CmpMark::Newer => '>',
            CmpMark::Older => '<',
            CmpMark::Different => '≠',
            CmpMark::Same => '=',
        }
    }
}

/// Compare files (directories are ignored) by name, size and mtime.
/// Timestamps within 2 seconds count as equal (FAT and zip have 2 s resolution).
pub fn compare(left: &[Entry], right: &[Entry]) -> (HashMap<String, CmpMark>, HashMap<String, CmpMark>) {
    let files = |v: &[Entry]| -> HashMap<String, Entry> {
        v.iter()
            .filter(|e| !e.is_dir)
            .map(|e| (e.name.clone(), e.clone()))
            .collect()
    };
    let (l, r) = (files(left), files(right));
    let mut lm = HashMap::new();
    let mut rm = HashMap::new();
    for (name, le) in &l {
        let Some(re) = r.get(name) else {
            lm.insert(name.clone(), CmpMark::Orphan);
            continue;
        };
        let dt = match (le.modified, re.modified) {
            (Some(a), Some(b)) if (a - b).abs() > 2 => a - b,
            _ => 0,
        };
        let (a, b) = if dt > 0 {
            (CmpMark::Newer, CmpMark::Older)
        } else if dt < 0 {
            (CmpMark::Older, CmpMark::Newer)
        } else if le.size != re.size {
            (CmpMark::Different, CmpMark::Different)
        } else {
            (CmpMark::Same, CmpMark::Same)
        };
        lm.insert(name.clone(), a);
        rm.insert(name.clone(), b);
    }
    for name in r.keys().filter(|n| !l.contains_key(*n)) {
        rm.insert(name.clone(), CmpMark::Orphan);
    }
    (lm, rm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks() {
        let at = |e: Entry, t| Entry { modified: Some(t), ..e };
        let left = [
            at(Entry::file("a", 1), 100),
            at(Entry::file("b", 1), 100),
            Entry::file("only_l", 1),
        ];
        let right = [
            at(Entry::file("a", 1), 101),
            at(Entry::file("b", 1), 500),
            Entry::file("only_r", 1),
        ];
        let (l, r) = compare(&left, &right);
        assert_eq!(l["a"], CmpMark::Same);
        assert_eq!(l["b"], CmpMark::Older);
        assert_eq!(r["b"], CmpMark::Newer);
        assert_eq!(l["only_l"], CmpMark::Orphan);
        assert_eq!(r["only_r"], CmpMark::Orphan);
    }
}
