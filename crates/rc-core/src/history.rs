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

    /// Undo a `back`/`forward` whose target could not be shown.
    pub fn undo_back(&mut self, target: Location) {
        self.forward.pop();
        self.back.push(target);
    }

    pub fn undo_forward(&mut self, target: Location) {
        self.back.pop();
        self.forward.push(target);
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
    fn undo_restores_state() {
        let l = |s: &str| Location::local(s);
        let mut h = History::default();
        h.visit(l("/gone"));
        let to = h.back(l("/here")).unwrap();
        h.undo_back(to);
        assert_eq!(h.back(l("/here")), Some(l("/gone")));
    }
}
