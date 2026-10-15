//! Per-panel back/forward navigation history, like a browser's.

use crate::Location;

/// Most locations remembered in each direction.
const LIMIT: usize = 100;

#[derive(Clone, Debug, Default)]
pub struct History {
    back: Vec<Location>,
    forward: Vec<Location>,
}

impl History {
    /// Record leaving `from` for a new place (not via back/forward).
    pub fn visit(&mut self, from: Location) {
        if self.back.last() != Some(&from) {
            self.back.push(from);
            if self.back.len() > LIMIT {
                self.back.remove(0);
            }
        }
        self.forward.clear();
    }

    /// Where "back" goes from `current`; `current` becomes the forward step.
    pub fn back(&mut self, current: Location) -> Option<Location> {
        let to = self.back.pop()?;
        self.forward.push(current);
        Some(to)
    }

    /// Where "forward" goes from `current`; `current` becomes the back step.
    pub fn forward(&mut self, current: Location) -> Option<Location> {
        let to = self.forward.pop()?;
        self.back.push(current);
        Some(to)
    }

    /// The target of the last `back` could not be shown (e.g. it was deleted): forget
    /// it, leaving the current place where it was, so the next `back` goes further.
    pub fn discard_back(&mut self) {
        self.forward.pop();
    }

    /// Like [`History::discard_back`], for `forward`.
    pub fn discard_forward(&mut self) {
        self.back.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn back_and_forward() {
        let l = |s: &str| Location::local(s);
        let mut h = History::default();
        h.visit(l("/a"));
        h.visit(l("/b"));
        // now at /c
        assert_eq!(h.back(l("/c")), Some(l("/b")));
        assert_eq!(h.back(l("/b")), Some(l("/a")));
        assert_eq!(h.back(l("/a")), None);
        assert_eq!(h.forward(l("/a")), Some(l("/b")));
        // A new visit drops the forward list.
        h.visit(l("/b"));
        assert_eq!(h.forward(l("/x")), None);
        assert_eq!(h.back(l("/x")), Some(l("/b")));
    }

    #[test]
    fn dead_entries_are_skipped() {
        let l = |s: &str| Location::local(s);
        let mut h = History::default();
        h.visit(l("/older"));
        h.visit(l("/gone"));
        assert_eq!(h.back(l("/here")), Some(l("/gone")));
        h.discard_back();
        assert_eq!(h.back(l("/here")), Some(l("/older")), "goes past the dead entry");
        assert_eq!(h.forward(l("/older")), Some(l("/here")));
    }

    #[test]
    fn limit_keeps_the_newest() {
        let mut h = History::default();
        for i in 0..150 {
            h.visit(Location::local(format!("/{i}")));
        }
        let mut n = 0;
        let mut at = Location::local("/now");
        while let Some(prev) = h.back(at.clone()) {
            at = prev;
            n += 1;
        }
        assert_eq!(n, 100);
        assert_eq!(at, Location::local("/50"));
    }
}
