//! What the app did to files, so it can be taken back.
//!
//! One entry per batch: a cull of six images is one undo, not six.
//! The stack holds what happened, never how to reverse it — the
//! reversal is read off the entry when it is needed, which keeps undo
//! and redo the same code path in opposite directions.

use std::path::PathBuf;

/// How many batches are remembered. Older ones fall off the bottom:
/// the files are still on disk, they just stop being one keystroke
/// away.
const DEPTH: usize = 50;

/// One batch, as it happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Files that went from one place to another. The trash is one of
    /// those places. Taking it back moves them the other way.
    Moved(Vec<(PathBuf, PathBuf)>),
    /// Files that appeared as copies. Taking it back trashes them.
    Copied(Vec<PathBuf>),
}

impl Step {
    pub fn is_empty(&self) -> bool {
        match self {
            Step::Moved(items) => items.is_empty(),
            Step::Copied(items) => items.is_empty(),
        }
    }
}

/// One batch with the name the menu shows for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub label: String,
    pub step: Step,
}

#[derive(Default)]
pub struct History {
    done: Vec<Entry>,
    undone: Vec<Entry>,
}

impl History {
    /// Record a batch. Anything that was waiting to be redone is gone:
    /// the timeline it belonged to no longer exists.
    pub fn record(&mut self, label: &str, step: Step) {
        if step.is_empty() {
            return;
        }
        self.undone.clear();
        self.done.push(Entry {
            label: label.to_owned(),
            step,
        });
        if self.done.len() > DEPTH {
            self.done.remove(0);
        }
    }

    /// Take the next batch off the undo side. The caller reverses it
    /// and hands back what that reversal did, which becomes the redo
    /// entry.
    pub fn take_undo(&mut self) -> Option<Entry> {
        self.done.pop()
    }

    pub fn take_redo(&mut self) -> Option<Entry> {
        self.undone.pop()
    }

    /// Put a reversed batch on the redo side, in the form it now has
    /// on disk.
    pub fn push_undone(&mut self, entry: Entry) {
        self.undone.push(entry);
    }

    /// Put a redone batch back on the undo side.
    pub fn push_done(&mut self, entry: Entry) {
        self.done.push(entry);
    }

    /// What the Edit menu says, and whether the item is live.
    pub fn undo_title(&self) -> String {
        match self.done.last() {
            Some(entry) => format!("Undo {}", entry.label),
            None => "Undo".to_owned(),
        }
    }

    pub fn redo_title(&self) -> String {
        match self.undone.last() {
            Some(entry) => format!("Redo {}", entry.label),
            None => "Redo".to_owned(),
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.done.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.undone.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn moved(from: &str, to: &str) -> Step {
        Step::Moved(vec![(PathBuf::from(from), PathBuf::from(to))])
    }

    #[test]
    fn a_batch_goes_back_and_forward() {
        let mut history = History::default();
        assert!(!history.can_undo());
        history.record("move to trash", moved("/a/x.heic", "/trash/x.heic"));
        assert_eq!(history.undo_title(), "Undo move to trash");
        assert!(history.can_undo() && !history.can_redo());

        let entry = history.take_undo().expect("one batch to take back");
        history.push_undone(entry);
        assert!(!history.can_undo() && history.can_redo());
        assert_eq!(history.redo_title(), "Redo move to trash");

        let entry = history.take_redo().expect("one batch to redo");
        history.push_done(entry);
        assert!(history.can_undo() && !history.can_redo());
    }

    #[test]
    fn a_new_batch_ends_the_redo_timeline() {
        let mut history = History::default();
        history.record("move", moved("/a/x.heic", "/b/x.heic"));
        let entry = history.take_undo().unwrap();
        history.push_undone(entry);
        assert!(history.can_redo());
        history.record("copy", Step::Copied(vec![PathBuf::from("/b/y.heic")]));
        assert!(!history.can_redo(), "the old future is gone");
    }

    #[test]
    fn an_empty_batch_is_not_worth_remembering() {
        let mut history = History::default();
        history.record("move", Step::Moved(Vec::new()));
        history.record("copy", Step::Copied(Vec::new()));
        assert!(!history.can_undo());
        assert_eq!(history.undo_title(), "Undo");
    }

    #[test]
    fn the_stack_stops_growing() {
        let mut history = History::default();
        for index in 0..DEPTH + 10 {
            history.record(&format!("move {index}"), moved("/a/x.heic", "/b/x.heic"));
        }
        let mut taken = 0;
        while history.take_undo().is_some() {
            taken += 1;
        }
        assert_eq!(taken, DEPTH);
    }
}
