//! Side-by-side text diff with inline (character level) highlighting.

use serde::{Deserialize, Serialize};
use similar::{Algorithm, ChangeTag, DiffOp};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RowKind {
    Equal,
    Delete,
    Insert,
    Replace,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub text: String,
    pub changed: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Side {
    /// 1-based line number.
    pub line_no: usize,
    pub segments: Vec<Segment>,
}

impl Side {
    pub fn text(&self) -> String {
        self.segments.iter().map(|s| s.text.as_str()).collect()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiffRow {
    pub kind: RowKind,
    pub left: Option<Side>,
    pub right: Option<Side>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DiffStats {
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiffResult {
    pub rows: Vec<DiffRow>,
    /// Row index where each block of changes starts (for next/previous navigation).
    pub hunks: Vec<usize>,
    pub stats: DiffStats,
}

/// What the UI should draw when unchanged regions are folded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visible {
    Row(usize),
    /// `n` unchanged rows hidden, starting at row index `start`.
    Fold {
        start: usize,
        n: usize,
    },
}

impl DiffResult {
    pub fn identical(&self) -> bool {
        self.hunks.is_empty()
    }

    /// Rows to display, keeping `context` unchanged lines around each change.
    pub fn visible(&self, context: usize) -> Vec<Visible> {
        let n = self.rows.len();
        let mut keep = vec![false; n];
        for (i, row) in self.rows.iter().enumerate() {
            if row.kind != RowKind::Equal {
                let lo = i.saturating_sub(context);
                let hi = (i + context + 1).min(n);
                keep[lo..hi].iter_mut().for_each(|k| *k = true);
            }
        }
        let mut out = Vec::new();
        let mut i = 0;
        while i < n {
            if keep[i] {
                out.push(Visible::Row(i));
                i += 1;
            } else {
                let start = i;
                while i < n && !keep[i] {
                    i += 1;
                }
                // Folding a single line saves nothing; show it instead.
                if i - start == 1 {
                    out.push(Visible::Row(start));
                } else {
                    out.push(Visible::Fold { start, n: i - start });
                }
            }
        }
        out
    }
}

pub fn diff_text(left: &str, right: &str) -> DiffResult {
    let l: Vec<&str> = left.lines().collect();
    let r: Vec<&str> = right.lines().collect();
    let ops = similar::capture_diff_slices(Algorithm::Patience, &l, &r);

    let mut rows = Vec::new();
    let mut hunks = Vec::new();
    let mut stats = DiffStats::default();
    let plain = |line_no: usize, text: &str, changed: bool| Side {
        line_no: line_no + 1,
        segments: vec![Segment {
            text: text.to_string(),
            changed,
        }],
    };
    let deleted = |k: usize| DiffRow {
        kind: RowKind::Delete,
        left: Some(plain(k, l[k], true)),
        right: None,
    };
    let inserted = |k: usize| DiffRow {
        kind: RowKind::Insert,
        left: None,
        right: Some(plain(k, r[k], true)),
    };

    for op in ops {
        if !matches!(op, DiffOp::Equal { .. }) {
            hunks.push(rows.len());
        }
        match op {
            DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => {
                rows.extend((0..len).map(|k| DiffRow {
                    kind: RowKind::Equal,
                    left: Some(plain(old_index + k, l[old_index + k], false)),
                    right: Some(plain(new_index + k, r[new_index + k], false)),
                }));
            }
            DiffOp::Delete { old_index, old_len, .. } => {
                stats.removed += old_len;
                rows.extend((old_index..old_index + old_len).map(deleted));
            }
            DiffOp::Insert { new_index, new_len, .. } => {
                stats.added += new_len;
                rows.extend((new_index..new_index + new_len).map(inserted));
            }
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                // Pair lines up one-to-one for inline highlighting; leftovers are plain deletes/inserts.
                let paired = old_len.min(new_len);
                stats.changed += paired;
                stats.removed += old_len - paired;
                stats.added += new_len - paired;
                rows.extend((0..paired).map(|k| {
                    let (a, b) = inline(l[old_index + k], r[new_index + k]);
                    DiffRow {
                        kind: RowKind::Replace,
                        left: Some(Side {
                            line_no: old_index + k + 1,
                            segments: a,
                        }),
                        right: Some(Side {
                            line_no: new_index + k + 1,
                            segments: b,
                        }),
                    }
                }));
                rows.extend((old_index + paired..old_index + old_len).map(deleted));
                rows.extend((new_index + paired..new_index + new_len).map(inserted));
            }
        }
    }
    DiffResult { rows, hunks, stats }
}

/// Character-level diff of two lines. If they share too little, mark both entirely changed
/// rather than producing confetti.
fn inline(a: &str, b: &str) -> (Vec<Segment>, Vec<Segment>) {
    let changes = similar::utils::diff_chars(Algorithm::Myers, a, b);
    let common: usize = changes
        .iter()
        .filter(|(t, _)| *t == ChangeTag::Equal)
        .map(|(_, s)| s.len())
        .sum();
    if common * 2 < a.len().max(b.len()) {
        let whole = |s: &str| {
            vec![Segment {
                text: s.to_string(),
                changed: true,
            }]
        };
        return (whole(a), whole(b));
    }
    let (mut left, mut right) = (Vec::new(), Vec::new());
    for (tag, text) in changes {
        match tag {
            ChangeTag::Equal => {
                push(&mut left, text, false);
                push(&mut right, text, false);
            }
            ChangeTag::Delete => push(&mut left, text, true),
            ChangeTag::Insert => push(&mut right, text, true),
        }
    }
    (left, right)
}

fn push(segs: &mut Vec<Segment>, text: &str, changed: bool) {
    match segs.last_mut() {
        Some(last) if last.changed == changed => last.text.push_str(text),
        _ => segs.push(Segment {
            text: text.to_string(),
            changed,
        }),
    }
}

/// Heuristic used by both back ends: NUL bytes in the first 8 KiB means binary.
pub fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|&b| b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical() {
        let d = diff_text("a\nb\n", "a\nb\n");
        assert!(d.identical());
        assert_eq!(d.rows.len(), 2);
    }

    #[test]
    fn replace_insert_delete() {
        let d = diff_text("one\ntwo\nthree\nfour\n", "one\ntwo!\nthree\nfour\nfive\n");
        assert_eq!(d.hunks.len(), 2);
        assert_eq!(
            d.stats,
            DiffStats {
                added: 1,
                removed: 0,
                changed: 1
            }
        );
        let row = &d.rows[1];
        assert_eq!(row.kind, RowKind::Replace);
        let right = row.right.as_ref().unwrap();
        assert_eq!(right.text(), "two!");
        assert_eq!(
            right.segments.last().unwrap(),
            &Segment {
                text: "!".into(),
                changed: true
            }
        );
        assert_eq!(d.rows.last().unwrap().kind, RowKind::Insert);
    }

    #[test]
    fn folding() {
        let left: String = (0..20).map(|i| format!("{i}\n")).collect();
        let right = left.replace("10\n", "ten\n");
        let v = diff_text(&left, &right).visible(2);
        assert_eq!(v[0], Visible::Fold { start: 0, n: 8 });
        assert_eq!(v.len(), 1 + 5 + 1);
        // A one-line gap is shown, not folded.
        let right = left.replace("3\n", "three\n").replace("11\n", "eleven\n");
        let v = diff_text(&left, &right).visible(3);
        assert!(v.iter().all(|x| !matches!(x, Visible::Fold { n: 1, .. })));
    }
}
