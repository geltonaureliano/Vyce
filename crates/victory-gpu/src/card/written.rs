//! Which pages of write-watched memory the title wrote since a card took
//! them ([`super`]): the kernel's answer, by 4 KiB page.
//!
//! Asking takes 1.8 us a megabyte whether or not anything was written
//! (measured, 2026-11-25), so not all of a card is asked about before each
//! submission: all of it before the first one of a frame, then what the
//! submission's lists are known to read, and what the title was seen to
//! change lately.
//!
//! shortcut: what the title changes for the first time in a while, in the
//! middle of a frame, and shaders read only through heaps they index
//! directly, arrives with the next frame.
//!
//! What the GPU writes into the same memory in place (lists recorded before
//! there was a card) no one reports: [`Written::force`].

use std::time::{Duration, Instant};

use super::{LEAF, PIECE};

/// Frames a piece is asked about before each submission after the title
/// changed it.
const LATELY: u64 = 240;
/// Pieces asked about for nothing at most to join two spans (the kernel is
/// asked once for each).
const GAP: usize = 8;
/// As [`super::track`]'s.
const PIN: Duration = Duration::from_secs(2);
const ALL: u16 = u16::MAX;

pub(super) struct Written {
    /// By piece: the asking after which it was last looked at.
    taken: Vec<u64>,
    /// By piece: the frame (plus one) it was last found changed in.
    changed: Vec<u64>,
    /// Pieces to look at whole next time, whatever the kernel says.
    forced: Vec<usize>,
    /// Pieces the kernel may write for a while: from, to, until.
    pins: Vec<(usize, usize, Instant)>,
    /// The frame (plus one) all of it was last asked about.
    asked: u64,
}

impl Written {
    /// For `pieces` pieces from `base` on, all to be looked at. `None` if the
    /// memory is not write-watched.
    pub(super) fn new(base: usize, pieces: usize) -> Option<Written> {
        let (now, ..) = crate::watch::watched(base, base + pieces * PIECE)?;
        Some(Written {
            taken: vec![now; pieces],
            changed: vec![crate::queue::presented() + 1; pieces],
            forced: (0..pieces).collect(),
            pins: Vec::new(),
            asked: 0,
        })
    }

    /// Pieces `[from, to)` are looked at whole next time.
    pub(super) fn force(&mut self, from: usize, to: usize) {
        self.forced.extend(from..to.min(self.taken.len()));
    }

    /// The kernel may write pieces `[from, to)` for a while.
    pub(super) fn pin(&mut self, from: usize, to: usize) {
        self.pins.push((from, to.min(self.taken.len()), Instant::now() + PIN));
    }

    /// What may have changed of the memory from `base` on: pieces, in order,
    /// with a bit for each page. `reads`: pieces (from, to) the submission's
    /// lists read. `None`: the memory is not watched any more.
    pub(super) fn take(&mut self, base: usize, reads: &[(usize, usize)], frame: u64) -> Option<Vec<(usize, u16)>> {
        let mut masks = std::collections::BTreeMap::<usize, u16>::new();
        for (from, to) in self.spans(reads, frame) {
            let (now, leaves, others) = crate::watch::watched(base + from * PIECE, base + to * PIECE)?;
            for at in leaves {
                let leaf = (at - base) / LEAF;
                *masks.entry(leaf / 16).or_default() |= 1 << (leaf % 16);
            }
            // What others asked about meanwhile they were told of, not we.
            for (page, when) in others {
                let piece = page - base / PIECE;
                if when > self.taken[piece] {
                    masks.insert(piece, ALL);
                }
            }
            self.taken[from..to].fill(now);
        }
        for piece in self.forced.drain(..) {
            masks.insert(piece, ALL);
        }
        let now = Instant::now();
        self.pins.retain(|p| p.2 > now);
        for &(from, to, _) in &self.pins {
            masks.extend((from..to).map(|piece| (piece, ALL)));
        }
        Some(masks.into_iter().collect())
    }

    /// The pieces to ask about, as spans.
    fn spans(&mut self, reads: &[(usize, usize)], frame: u64) -> Vec<(usize, usize)> {
        let pieces = self.taken.len();
        if self.asked != frame + 1 {
            self.asked = frame + 1;
            return vec![(0, pieces)];
        }
        let mut ask = vec![false; pieces];
        for &(from, to) in reads {
            ask[from.min(pieces)..to.min(pieces)].fill(true);
        }
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for (piece, &read) in ask.iter().enumerate() {
            if !read && self.changed[piece] + LATELY < frame + 1 {
                continue;
            }
            match spans.last_mut() {
                Some((_, to)) if piece <= *to + GAP => *to = piece + 1,
                _ => spans.push((piece, piece + 1)),
            }
        }
        spans
    }

    /// Of the pieces taken last, `changed` were found changed.
    pub(super) fn looked(&mut self, changed: &[usize], frame: u64) {
        for &piece in changed {
            self.changed[piece] = frame + 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::*;

    const MEM_WRITE_WATCH: u32 = 0x20_0000;

    /// Write-watched memory, as the runtime gives the title.
    pub(super) struct Watched(pub(super) usize);

    impl Watched {
        pub(super) fn new(len: usize) -> Watched {
            // SAFETY: a plain allocation.
            let at = unsafe {
                VirtualAlloc(std::ptr::null(), len, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE)
            } as usize;
            assert_ne!(at, 0);
            Watched(at)
        }
    }

    impl Drop for Watched {
        fn drop(&mut self) {
            // SAFETY: our allocation.
            unsafe { VirtualFree(self.0 as *mut _, 0, MEM_RELEASE) };
        }
    }

    fn poke(at: usize) {
        // SAFETY: the tests' own memory.
        unsafe { (at as *mut u8).write_volatile((at as *const u8).read_volatile().wrapping_add(1)) };
    }

    #[test]
    fn pages_written_are_told_once_and_others_askings_are_not_missed() {
        let m = Watched::new(16 * PIECE);
        let base = m.0;
        let mut w = Written::new(base, 16).expect("watched");
        let all: Vec<(usize, u16)> = (0..16).map(|p| (p, ALL)).collect();
        assert_eq!(w.take(base, &[], 1), Some(all), "new: everything");
        assert_eq!(w.take(base, &[], 1), Some(vec![]));
        poke(base + 3 * PIECE + 5 * LEAF + 9);
        poke(base + 3 * PIECE + 6 * LEAF);
        poke(base + 9 * PIECE);
        assert_eq!(w.take(base, &[], 1), Some(vec![(3, 0b110_0000), (9, 1)]));
        assert_eq!(w.take(base, &[], 1), Some(vec![]));
        // Another asker takes the kernel's bits: the piece is told whole.
        poke(base + 4 * PIECE + 100);
        crate::watch::writes(base + 4 * PIECE, base + 5 * PIECE, false).expect("watched");
        poke(base + 5 * PIECE + LEAF);
        assert_eq!(w.take(base, &[], 1), Some(vec![(4, ALL), (5, 0b10)]));
        assert_eq!(w.take(base, &[], 1), Some(vec![]));
        // Written in place by the GPU, or by the kernel.
        w.force(7, 8);
        assert_eq!(w.take(base, &[], 1), Some(vec![(7, ALL)]));
        w.pin(2, 3);
        assert_eq!(w.take(base, &[], 1), Some(vec![(2, ALL)]));
        assert_eq!(w.take(base, &[], 1), Some(vec![(2, ALL)]));
        w.pins[0].2 = Instant::now() - Duration::from_millis(1);
        assert_eq!(w.take(base, &[], 1), Some(vec![]));
        // Ordinary memory cannot be told about.
        let plain = vec![0u8; 3 * PIECE];
        assert!(Written::new(plain.as_ptr() as usize & !(PIECE - 1), 2).is_none());
    }

    #[test]
    fn quiet_pieces_are_asked_about_once_a_frame_or_when_read() {
        let m = Watched::new(64 * PIECE);
        let base = m.0;
        let mut w = Written::new(base, 64).expect("watched");
        w.take(base, &[], 1);
        let quiet = 2 + LATELY;
        assert_eq!(w.spans(&[], quiet), [(0, 64)], "the frame's first");
        assert!(w.spans(&[], quiet).is_empty());
        assert_eq!(w.spans(&[(3, 5), (4, 6), (40, 41)], quiet), [(3, 6), (40, 41)]);
        assert_eq!(w.spans(&[(3, 5), (12, 13)], quiet), [(3, 13)], "near ones at once");
        w.looked(&[20], quiet);
        assert_eq!(w.spans(&[(50, 51)], quiet), [(20, 21), (50, 51)]);
        assert_eq!(w.spans(&[], quiet + 1), [(0, 64)]);
        assert_eq!(w.spans(&[], quiet + 1), [(20, 21)]);
        // A write in a piece not asked about is told with the next frame.
        assert_eq!(w.take(base, &[], quiet + 1), Some(vec![]));
        poke(base + 33 * PIECE);
        assert_eq!(w.take(base, &[], quiet + 1), Some(vec![]));
        assert_eq!(w.take(base, &[(33, 34)], quiet + 1), Some(vec![(33, 1)]));
        poke(base + 34 * PIECE);
        assert_eq!(w.take(base, &[], quiet + 2), Some(vec![(34, 1)]));
    }
}
