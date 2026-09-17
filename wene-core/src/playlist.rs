//! Slideshow playlist: ordering, stepping, loop, and shuffle.
//! Shuffle keeps two permutation maps so stepping in the original
//! order still works while shuffled (like the original app's
//! DYRandomizableArray, much reduced).

use std::path::PathBuf;

/// Small xorshift PRNG; enough for shuffling a slideshow.
/// ponytail: not cryptographic, fine for shuffle, swap for a rand
/// crate if quality ever matters.
struct XorShift(u64);

impl XorShift {
    fn new() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E3779B97F4A7C15)
            | 1;
        XorShift(seed)
    }

    fn next_below(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % bound as u64) as usize
    }
}

pub struct Playlist {
    files: Vec<PathBuf>,
    /// Position in play order (an index into `order` when shuffled,
    /// into `files` otherwise).
    position: usize,
    /// Shuffled play order: order[play position] = file index.
    /// Empty when not shuffled.
    order: Vec<usize>,
    pub looping: bool,
}

impl Playlist {
    pub fn new(files: Vec<PathBuf>, start: usize) -> Self {
        let start = start.min(files.len().saturating_sub(1));
        Playlist {
            files,
            position: start,
            order: Vec::new(),
            looping: false,
        }
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn shuffled(&self) -> bool {
        !self.order.is_empty()
    }

    /// Index of the current file in the ORIGINAL (sorted) order.
    pub fn current_index(&self) -> usize {
        if self.shuffled() {
            self.order[self.position]
        } else {
            self.position
        }
    }

    pub fn current(&self) -> Option<&PathBuf> {
        self.files.get(self.current_index())
    }

    /// The file at a play-order offset from here (for precaching),
    /// respecting loop mode.
    pub fn peek(&self, delta: i64) -> Option<&PathBuf> {
        let target = self.target_position(delta)?;
        let index = if self.shuffled() {
            self.order[target]
        } else {
            target
        };
        self.files.get(index)
    }

    /// Step in play order. Returns false when the edge is hit and
    /// loop is off.
    pub fn step(&mut self, delta: i64) -> bool {
        match self.target_position(delta) {
            Some(position) => {
                self.position = position;
                true
            }
            None => false,
        }
    }

    fn target_position(&self, delta: i64) -> Option<usize> {
        let len = self.files.len() as i64;
        if len == 0 {
            return None;
        }
        let next = self.position as i64 + delta;
        if next >= 0 && next < len {
            Some(next as usize)
        } else if self.looping {
            Some(next.rem_euclid(len) as usize)
        } else {
            None
        }
    }

    pub fn jump_first(&mut self) {
        self.position = 0;
    }

    pub fn jump_last(&mut self) {
        self.position = self.files.len().saturating_sub(1);
    }

    /// Step relative to the ORIGINAL order while shuffled (the
    /// original app's option-arrow behavior). Plain step otherwise.
    pub fn step_original(&mut self, delta: i64) -> bool {
        if !self.shuffled() {
            return self.step(delta);
        }
        let len = self.files.len() as i64;
        let mut target = self.current_index() as i64 + delta;
        if self.looping {
            target = target.rem_euclid(len);
        } else if target < 0 || target >= len {
            return false;
        }
        // Move play position to wherever that file sits in the
        // shuffled order.
        // ponytail: O(n) scan; keep an inverse map if lists get huge.
        if let Some(position) = self.order.iter().position(|&i| i == target as usize) {
            self.position = position;
            true
        } else {
            false
        }
    }

    /// Turn shuffle on (current file becomes position 0) or off (the
    /// current file keeps its place in the original order).
    pub fn set_shuffled(&mut self, shuffled: bool) {
        if shuffled == self.shuffled() || self.files.is_empty() {
            return;
        }
        if shuffled {
            let current = self.current_index();
            let mut order: Vec<usize> = (0..self.files.len()).collect();
            let mut rng = XorShift::new();
            // Fisher-Yates.
            for i in (1..order.len()).rev() {
                let j = rng.next_below(i + 1);
                order.swap(i, j);
            }
            // Current file plays first.
            if let Some(at) = order.iter().position(|&i| i == current) {
                order.swap(0, at);
            }
            self.order = order;
            self.position = 0;
        } else {
            self.position = self.current_index();
            self.order.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playlist(n: usize) -> Playlist {
        Playlist::new((0..n).map(|i| PathBuf::from(format!("/f/{i}.jpg"))).collect(), 0)
    }

    fn name(p: &PathBuf) -> String {
        p.file_name().unwrap().to_string_lossy().into_owned()
    }

    #[test]
    fn stepping_and_edges() {
        let mut pl = playlist(3);
        assert_eq!(name(pl.current().unwrap()), "0.jpg");
        assert!(pl.step(1));
        assert!(pl.step(1));
        assert_eq!(name(pl.current().unwrap()), "2.jpg");
        assert!(!pl.step(1)); // edge, no loop
        pl.looping = true;
        assert!(pl.step(1)); // wraps
        assert_eq!(name(pl.current().unwrap()), "0.jpg");
        assert!(pl.step(-1));
        assert_eq!(name(pl.current().unwrap()), "2.jpg");
    }

    #[test]
    fn peek_respects_loop() {
        let mut pl = playlist(3);
        pl.jump_last();
        assert!(pl.peek(1).is_none());
        pl.looping = true;
        assert_eq!(name(pl.peek(1).unwrap()), "0.jpg");
    }

    #[test]
    fn shuffle_keeps_current_and_covers_all() {
        let mut pl = playlist(10);
        pl.step(1);
        pl.step(1); // current = 2.jpg
        pl.set_shuffled(true);
        assert_eq!(name(pl.current().unwrap()), "2.jpg"); // plays first
        let mut seen = vec![false; 10];
        seen[pl.current_index()] = true;
        while pl.step(1) {
            seen[pl.current_index()] = true;
        }
        assert!(seen.iter().all(|&s| s)); // every file exactly reachable
    }

    #[test]
    fn unshuffle_keeps_current() {
        let mut pl = playlist(10);
        pl.set_shuffled(true);
        pl.step(1);
        let current = name(pl.current().unwrap());
        pl.set_shuffled(false);
        assert_eq!(name(pl.current().unwrap()), current);
    }

    #[test]
    fn step_original_while_shuffled() {
        let mut pl = playlist(5);
        pl.step(1); // current = 1.jpg
        pl.set_shuffled(true);
        assert!(pl.step_original(1));
        assert_eq!(name(pl.current().unwrap()), "2.jpg");
        assert!(pl.step_original(-1));
        assert!(pl.step_original(-1));
        assert_eq!(name(pl.current().unwrap()), "0.jpg");
        assert!(!pl.step_original(-1)); // edge, no loop
    }
}
