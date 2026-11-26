//! Which 64 KiB blocks of one view of title memory the title wrote since a
//! card took them ([`super`]): blocks taken are left read-only, and the
//! vectored handler of [`crate::watch`] gives the first write to one its
//! access back and notes it here, without a lock.
//!
//! The order that makes it safe: a block is made read-only before it is
//! read, and noted written before it is made writable. A block's state is
//! taken ([`BUSY`]) by whoever changes its protection, so it is never
//! writable and thought guarded.
//!
//! Blocks that cannot be told by a guard are looked at every time instead:
//! those shown at another address too (a write there does not fault here)
//! and those the kernel may be writing (it does not fault into our handler,
//! it fails). Blocks written at every look are left open, too: a fault costs
//! the title's thread 5 to 7 us and the comparison of a block as much of
//! ours (measured on an i7-8700K, 2026-11-25).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::watch::PAGE as BLOCK;

/// Writable, not guarded.
const OPEN: u8 = 0;
/// Read-only: a write faults.
const GUARDED: u8 = 1;
/// A thread is changing the protection.
const BUSY: u8 = 2;

/// How long a block stays open after the runtime said the kernel will write
/// it (as [`crate::watch`]'s pins: the write may land after the call that
/// asked for it returned).
const PIN: Duration = Duration::from_secs(2);

/// Looks in a row that found a block written, after which it is left open.
const HOT: u8 = 8;
/// Looks in a row that found an open block unchanged, after which it is
/// guarded again.
const CALM: u8 = 8;

pub(crate) struct Track {
    base: usize,
    blocks: usize,
    live: AtomicBool,
    state: Box<[AtomicU8]>,
    /// A bit per block: written since it was last taken.
    dirty: Box<[AtomicU64]>,
    /// Looked at every time, never guarded.
    each: Box<[AtomicU64]>,
    /// Never looked at, never guarded ([`skip`]).
    skipped: Box<[AtomicU64]>,
    /// Blocks the kernel may write: from, to, until.
    pins: Mutex<Vec<(usize, usize, Instant)>>,
    pinned: AtomicBool,
    heat: Mutex<Heat>,
}

/// How often blocks were written, for [`HOT`].
#[derive(Default)]
struct Heat {
    look: u32,
    /// Per block: the last look it was written at, how many in a row.
    written: Vec<(u32, u8)>,
    /// Blocks left open, with the looks in a row they were unchanged.
    open: std::collections::BTreeMap<usize, u8>,
}

/// The tracks there are. Never freed: the handler reads them at any time.
static TRACKS: [AtomicPtr<Track>; 4] = [const { AtomicPtr::new(std::ptr::null_mut()) }; 4];

/// The track of the title address `at`.
fn over(at: usize) -> Option<&'static Track> {
    TRACKS.iter().find_map(|t| {
        // SAFETY: tracks are never freed.
        let t = unsafe { t.load(Ordering::Acquire).as_ref() }?;
        (t.live.load(Ordering::Acquire) && at >= t.base && at - t.base < t.blocks * BLOCK).then_some(t)
    })
}

/// The tracks with blocks in `[from, to)`.
fn within(from: usize, to: usize) -> impl Iterator<Item = &'static Track> {
    TRACKS.iter().filter_map(move |t| {
        // SAFETY: tracks are never freed.
        let t = unsafe { t.load(Ordering::Acquire).as_ref() }?;
        (t.live.load(Ordering::Acquire) && from < t.base + t.blocks * BLOCK && t.base < to).then_some(t)
    })
}

/// Whether a track guards the block `at` is in.
pub(crate) fn tracked(at: usize) -> bool {
    over(at).is_some()
}

/// What a track makes of a fault.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Fault {
    /// No track is over it, or the block could not be opened.
    No,
    /// The block is open: the write is done again.
    Opened,
    /// As above, and its page is shown at another address too, where other
    /// guards may be over it.
    Shared,
}

/// A write to `at` faulted.
pub(crate) fn fault(at: usize) -> Fault {
    let Some(t) = over(at) else { return Fault::No };
    crate::telemetry::add(crate::telemetry::Count::CardFaults, 1);
    let block = (at - t.base) / BLOCK;
    match (t.open(block, true), Track::bit(&t.each, block)) {
        (false, _) => Fault::No,
        (true, false) => Fault::Opened,
        (true, true) => Fault::Shared,
    }
}

/// Before a write of ours into title memory `[at, at + len)` that may go
/// through the kernel: its tracked blocks are opened and noted written.
/// `pin`: the kernel may write them for a while yet. Whether that is all
/// there is to do: the range is a track's, and shown at no other address.
pub(crate) fn opening(at: usize, len: usize, pin: bool) -> bool {
    let end = at.saturating_add(len);
    let mut whole = false;
    for t in within(at, end) {
        let (from, to) = t.span(at, end);
        if pin {
            let mut pins = t.pins.lock().unwrap_or_else(|e| e.into_inner());
            pins.push((from, to, Instant::now() + PIN));
            t.pinned.store(true, Ordering::Release);
        }
        whole = at >= t.base && end <= t.base + t.blocks * BLOCK;
        for block in from..to {
            t.open(block, false);
            whole &= !Track::bit(&t.each, block);
        }
    }
    whole
}

/// Title memory `[at, at + len)` was written behind the guards (through
/// another view): its blocks are looked at next time.
pub(crate) fn written(at: usize, len: usize) {
    let end = at.saturating_add(len);
    for t in within(at, end) {
        let (from, to) = t.span(at, end);
        for block in from..to {
            t.mark(block);
        }
    }
}

/// The pages shown at `at` (a block's start) are shown elsewhere too: the
/// block is looked at every time from now on.
pub(crate) fn shown_elsewhere(at: usize) {
    if let Some(t) = over(at) {
        let block = (at - t.base) / BLOCK;
        t.each[block / 64].fetch_or(1 << (block % 64), Ordering::AcqRel);
        t.open(block, false);
    }
}

/// Memory no shader reads as data (`skip`: the title's descriptors, which
/// its threads write all over every frame), or no longer (`!skip`).
pub(crate) fn skip(from: usize, to: usize, skip: bool) {
    for t in within(from, to) {
        t.skip(from, to, skip);
    }
}

impl Track {
    /// A track over the view `[base, base + blocks * 64 KiB)`, every block
    /// to be taken. `None` if there are too many already.
    pub(crate) fn new(base: usize, blocks: usize) -> Option<&'static Track> {
        let words = |n: usize| (0..n).map(|_| AtomicU64::new(0)).collect::<Box<[AtomicU64]>>();
        let t: &'static Track = Box::leak(Box::new(Track {
            base,
            blocks,
            live: AtomicBool::new(true),
            state: (0..blocks).map(|_| AtomicU8::new(OPEN)).collect(),
            dirty: words(blocks.div_ceil(64)),
            each: words(blocks.div_ceil(64)),
            skipped: words(blocks.div_ceil(64)),
            pins: Mutex::default(),
            pinned: AtomicBool::new(false),
            heat: Mutex::new(Heat {
                written: vec![(0, 0); blocks],
                ..Heat::default()
            }),
        }));
        for block in 0..blocks {
            t.mark(block);
        }
        crate::watch::install();
        let free = TRACKS.iter().find(|slot| {
            slot.compare_exchange(
                std::ptr::null_mut(),
                std::ptr::from_ref(t).cast_mut(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        });
        if free.is_none() {
            t.live.store(false, Ordering::Release);
            return None;
        }
        Some(t)
    }

    /// The blocks of `[from, to)`, cut to the track.
    fn span(&self, from: usize, to: usize) -> (usize, usize) {
        let end = self.base + self.blocks * BLOCK;
        let (from, to) = (from.max(self.base), to.min(end));
        if from >= to {
            return (0, 0);
        }
        ((from - self.base) / BLOCK, (to - self.base).div_ceil(BLOCK))
    }

    fn bit(words: &[AtomicU64], block: usize) -> bool {
        words[block / 64].load(Ordering::Acquire) >> (block % 64) & 1 != 0
    }

    fn mark(&self, block: usize) {
        self.dirty[block / 64].fetch_or(1 << (block % 64), Ordering::Release);
    }

    fn protect(&self, from: usize, to: usize, protection: u32) -> bool {
        let mut old = 0;
        // SAFETY: only the protection of the title's own view changes.
        unsafe {
            windows_sys::Win32::System::Memory::VirtualProtect(
                (self.base + from * BLOCK) as *const _,
                (to - from) * BLOCK,
                protection,
                &mut old,
            ) != 0
        }
    }

    /// Notes `block` written and makes it writable; `fault`: a write to it
    /// just failed, so it is made writable whatever we think it is.
    fn open(&self, block: usize, fault: bool) -> bool {
        use windows_sys::Win32::System::Memory::PAGE_READWRITE;
        let state = &self.state[block];
        loop {
            let now = state.load(Ordering::Acquire);
            match now {
                BUSY => std::thread::yield_now(),
                OPEN if !fault => {
                    self.mark(block);
                    return true;
                }
                _ => {
                    if state.compare_exchange(now, BUSY, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                        // Noted before it opens: another thread may write it
                        // and submit as soon as it is.
                        self.mark(block);
                        let opened = self.protect(block, block + 1, PAGE_READWRITE);
                        state.store(OPEN, Ordering::Release);
                        return opened;
                    }
                }
            }
        }
    }

    /// Makes blocks `[from, to)` read-only; those that cannot be stay open
    /// and are looked at next time.
    fn guard(&self, from: usize, to: usize) {
        use windows_sys::Win32::System::Memory::PAGE_READONLY;
        let mut ours = Vec::with_capacity(to - from);
        for block in from..to {
            loop {
                match self.state[block].compare_exchange(OPEN, BUSY, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => {
                        ours.push(block);
                        break;
                    }
                    Err(GUARDED) => break,
                    // The handler is opening it: guarded once it has.
                    Err(_) => std::thread::yield_now(),
                }
            }
        }
        // In one call (a view of one allocation); block by block if the
        // system refuses that.
        let whole = self.protect(from, to, PAGE_READONLY);
        for block in ours {
            if whole || self.protect(block, block + 1, PAGE_READONLY) {
                self.state[block].store(GUARDED, Ordering::Release);
            } else {
                self.mark(block);
                self.state[block].store(OPEN, Ordering::Release);
            }
        }
    }

    /// Which blocks the kernel may be writing now.
    fn pinned(&self) -> Vec<(usize, usize)> {
        if !self.pinned.load(Ordering::Acquire) {
            return Vec::new();
        }
        let now = Instant::now();
        let mut pins = self.pins.lock().unwrap_or_else(|e| e.into_inner());
        pins.retain(|p| p.2 > now);
        if pins.is_empty() {
            self.pinned.store(false, Ordering::Release);
        }
        pins.iter().map(|p| (p.0, p.1)).collect()
    }

    /// The blocks that may have changed since the last time, in order, read
    /// only from here on until they are written again (but those left open).
    /// One thread at a time; [`Track::looked`] follows.
    pub(crate) fn take(&self) -> Vec<usize> {
        let pins = self.pinned();
        let mut heat = self.heat.lock().unwrap_or_else(|e| e.into_inner());
        heat.look += 1;
        let look = heat.look;
        let mut out: Vec<usize> = Vec::new();
        let mut guard: Vec<usize> = Vec::new();
        let mut take = |heat: &mut Heat, block: usize, dirty: bool| {
            // Those left open are taken below.
            if heat.open.contains_key(&block) {
                return;
            }
            out.push(block);
            if Self::bit(&self.each, block) || pins.iter().any(|&(a, b)| a <= block && block < b) {
                return;
            }
            let (last, row) = heat.written[block];
            let row = if dirty && last + 1 == look { row.saturating_add(1) } else { 1 };
            heat.written[block] = (look, row);
            if row >= HOT {
                heat.open.insert(block, 0);
            } else {
                guard.push(block);
            }
        };
        for w in 0..self.dirty.len() {
            let each = self.each[w].load(Ordering::Acquire);
            let dirty = if self.dirty[w].load(Ordering::Relaxed) == 0 {
                0
            } else {
                self.dirty[w].swap(0, Ordering::AcqRel)
            };
            let mut bits = (dirty | each) & !self.skipped[w].load(Ordering::Acquire);
            while bits != 0 {
                let b = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let block = w * 64 + b;
                if block < self.blocks {
                    take(&mut heat, block, dirty >> b & 1 != 0);
                }
            }
        }
        // Pinned blocks, whether or not a fault told of them.
        for &(a, b) in &pins {
            out.extend((a..b.min(self.blocks)).filter(|&block| !Self::bit(&self.skipped, block)));
        }
        // Those left open; guarded again once they have calmed down.
        let calm: Vec<usize> = heat.open.iter().filter(|(_, n)| **n >= CALM).map(|(b, _)| *b).collect();
        for block in calm {
            heat.open.remove(&block);
            heat.written[block] = (look, 1);
            out.push(block);
            guard.push(block);
        }
        out.extend(heat.open.keys());
        drop(heat);
        guard.sort_unstable();
        let mut run: Option<(usize, usize)> = None;
        for block in guard {
            match &mut run {
                Some((_, to)) if *to == block => *to += 1,
                _ => {
                    if let Some((from, to)) = run.replace((block, block + 1)) {
                        self.guard(from, to);
                    }
                }
            }
        }
        if let Some((from, to)) = run {
            self.guard(from, to);
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Of the blocks taken last, `changed` (in order) were found changed.
    pub(crate) fn looked(&self, changed: &[usize]) {
        let mut heat = self.heat.lock().unwrap_or_else(|e| e.into_inner());
        for (block, calm) in heat.open.iter_mut() {
            *calm = if changed.binary_search(block).is_ok() { 0 } else { *calm + 1 };
        }
    }

    /// See [`skip`]: whole blocks of the range only.
    fn skip(&self, from: usize, to: usize, skip: bool) {
        let (from, to) = (from.max(self.base), to.min(self.base + self.blocks * BLOCK));
        let first = (from - self.base).div_ceil(BLOCK);
        let last = to.saturating_sub(self.base) / BLOCK;
        for block in first..last.max(first) {
            let bit = 1 << (block % 64);
            if skip {
                self.skipped[block / 64].fetch_or(bit, Ordering::AcqRel);
            } else {
                self.skipped[block / 64].fetch_and(!bit, Ordering::AcqRel);
            }
            // Writable from now on if skipped, looked at anew if not.
            self.open(block, true);
        }
    }

    /// Ends the track: every block is the title's to write, untold.
    pub(crate) fn end(&self) {
        for block in 0..self.blocks {
            self.open(block, true);
        }
        self.live.store(false, Ordering::Release);
        for slot in &TRACKS {
            let _ = slot.compare_exchange(
                std::ptr::from_ref(self).cast_mut(),
                std::ptr::null_mut(),
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::*;

    /// A section of some blocks and views of it.
    pub(in crate::card) struct Views {
        pub(in crate::card) section: usize,
        views: Vec<usize>,
        blocks: usize,
    }

    impl Views {
        pub(in crate::card) fn new(blocks: usize) -> Views {
            // SAFETY: a pagefile section of our own.
            let section = unsafe {
                CreateFileMappingW(
                    -1isize as _,
                    std::ptr::null(),
                    PAGE_READWRITE,
                    0,
                    (blocks * BLOCK) as u32,
                    std::ptr::null(),
                )
            };
            assert!(!section.is_null());
            Views { section: section as usize, views: Vec::new(), blocks }
        }

        pub(in crate::card) fn map(&mut self) -> usize {
            // SAFETY: a view of our own section.
            let view = unsafe { MapViewOfFile(self.section as _, FILE_MAP_WRITE, 0, 0, self.blocks * BLOCK) };
            assert!(!view.Value.is_null());
            self.views.push(view.Value as usize);
            view.Value as usize
        }
    }

    impl Drop for Views {
        fn drop(&mut self) {
            for &at in &self.views {
                // SAFETY: our views.
                unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: at as *mut _ }) };
            }
            // SAFETY: our section.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.section as _) };
        }
    }

    fn poke(at: usize, value: u8) {
        // SAFETY: the tests' own view; the handler opens guarded blocks.
        unsafe { (at as *mut u8).write_volatile(value) };
    }

    /// The tracks are few: the tests that make some run one at a time.
    pub(in crate::card) static ONE: Mutex<()> = Mutex::new(());

    #[test]
    fn a_write_to_a_guarded_block_is_told_once() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let mut views = Views::new(8);
        let at = views.map();
        let t = Track::new(at, 8).expect("a track");
        assert_eq!(t.take(), [0, 1, 2, 3, 4, 5, 6, 7], "new: everything");
        t.looked(&[]);
        assert!(t.take().is_empty());
        poke(at + 2 * BLOCK + 77, 1);
        poke(at + 2 * BLOCK + 78, 1);
        poke(at + 7 * BLOCK, 1);
        assert_eq!(t.take(), [2, 7]);
        assert!(t.take().is_empty());
        // What we write through the kernel is opened first; a write behind
        // the guards is told by address.
        assert!(opening(at + BLOCK, 10, false));
        written(at + 3 * BLOCK - 1, 2);
        assert_eq!(t.take(), [1, 2, 3]);
        assert!(!opening(at + 8 * BLOCK, 10, false), "past the track");
        t.end();
        assert!(!tracked(at));
        poke(at, 9);
    }

    #[test]
    fn blocks_shown_elsewhere_and_pinned_ones_are_looked_at_each_time() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let mut views = Views::new(6);
        let at = views.map();
        let t = Track::new(at, 6).expect("a track");
        t.take();
        shown_elsewhere(at + 4 * BLOCK);
        assert_eq!(t.take(), [4]);
        assert_eq!(t.take(), [4]);
        opening(at + BLOCK + 5, 10, true);
        assert_eq!(t.take(), [1, 4]);
        assert_eq!(t.take(), [1, 4]);
        // The pin ends.
        t.pins.lock().unwrap()[0].2 = Instant::now() - Duration::from_millis(1);
        assert_eq!(t.take(), [4]);
        t.end();
    }

    #[test]
    fn skipped_blocks_are_the_titles_own() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let mut views = Views::new(6);
        let at = views.map();
        let t = Track::new(at, 6).expect("a track");
        t.take();
        // Whole blocks only: 2 and 3.
        skip(at + BLOCK + 1, at + 4 * BLOCK + 100, true);
        poke(at + 2 * BLOCK, 1);
        poke(at + 4 * BLOCK, 1);
        assert_eq!(t.take(), [4]);
        skip(at + BLOCK + 1, at + 4 * BLOCK + 100, false);
        assert_eq!(t.take(), [2, 3]);
        poke(at + 3 * BLOCK, 1);
        assert_eq!(t.take(), [3]);
        t.end();
    }

    #[test]
    fn blocks_written_at_every_look_are_left_open_until_they_calm_down() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let mut views = Views::new(4);
        let at = views.map();
        let t = Track::new(at, 4).expect("a track");
        t.take();
        t.looked(&[]);
        for k in 0..40u8 {
            poke(at + BLOCK, k);
            assert_eq!(t.take(), [1], "look {k}");
            t.looked(&[1]);
        }
        assert!(t.heat.lock().unwrap().open.contains_key(&1));
        assert_eq!(t.state[1].load(Ordering::Relaxed), OPEN);
        for _ in 0..CALM {
            assert_eq!(t.take(), [1]);
            t.looked(&[]);
        }
        assert_eq!(t.take(), [1], "guarded again, looked at once more");
        assert_eq!(t.state[1].load(Ordering::Relaxed), GUARDED);
        t.looked(&[]);
        assert!(t.take().is_empty());
        poke(at + BLOCK, 5);
        assert_eq!(t.take(), [1]);
        t.end();
    }

    /// Writers on several threads beside a thread that takes and reads: at
    /// the end what was read last is what was written last, everywhere.
    #[test]
    fn no_write_is_missed_between_threads() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        const BLOCKS: usize = 32;
        let mut views = Views::new(BLOCKS);
        let at = views.map();
        let t = Track::new(at, BLOCKS).expect("a track");
        let mut copy = vec![0u32; BLOCKS];
        let mut look = || {
            let taken = t.take();
            let mut changed = Vec::new();
            for &block in &taken {
                // SAFETY: the tests' own view.
                let now = unsafe { ((at + block * BLOCK + 128) as *const u32).read_volatile() };
                if copy[block] != now {
                    copy[block] = now;
                    changed.push(block);
                }
            }
            t.looked(&changed);
        };
        std::thread::scope(|s| {
            let writers: Vec<_> = (0..4usize)
                .map(|k| {
                    s.spawn(move || {
                        let mut x = k as u64 + 1;
                        for round in 1..4000u32 {
                            x ^= x << 13;
                            x ^= x >> 7;
                            x ^= x << 17;
                            // Each thread has blocks of its own.
                            let block = (x as usize % (BLOCKS / 4)) * 4 + k;
                            // SAFETY: the tests' own view.
                            unsafe { ((at + block * BLOCK + 128) as *mut u32).write_volatile(round) };
                        }
                        for block in (k..BLOCKS).step_by(4) {
                            // SAFETY: as above.
                            unsafe { ((at + block * BLOCK + 128) as *mut u32).write_volatile(1_000_000 + block as u32) };
                        }
                    })
                })
                .collect();
            while !writers.iter().all(|w| w.is_finished()) {
                look();
            }
        });
        look();
        for (block, v) in copy.iter().enumerate() {
            assert_eq!(*v, 1_000_000 + block as u32, "block {block}");
        }
        t.end();
    }
}
