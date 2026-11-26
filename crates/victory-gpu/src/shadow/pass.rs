//! What a submission's shaders write to sparse copies, as a whole: the bytes
//! kept before it and the compare after it are one pass each over the union
//! of the ranges its lists declared, instead of one for each declaration
//! (the world declares the same memory dozens of times, through views that
//! overlap almost entirely).
//!
//! One pass per submission and the joined ranges are the idea of
//! `VICTORY_WIDE_BACK` in github.com/DexNom/VIctory and
//! github.com/fxceless/victory-fork (`wide_back`, `take_snapshots`); the
//! code here is written anew for tiles.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, MutexGuard};

use super::{Mix, PAGE, Shadow, Sparse};
use crate::telemetry::Count;

/// Whether submissions do it in one pass each (`VYCE_ONE_PASS=0`: a pass for
/// every declared range, as before).
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VYCE_ONE_PASS").is_none_or(|v| v != "0"))
}

/// A range shaders write: host copy, title address, bytes.
type Range = (Arc<Shadow>, usize, usize);

/// `ranges` sorted, joined where they overlap or touch. Not over gaps: what
/// lies between two ranges was not given to shaders.
pub(crate) fn join(ranges: &mut Vec<(usize, usize)>) {
    ranges.retain(|r| r.0 < r.1);
    ranges.sort_unstable();
    ranges.dedup_by(|next, last| {
        let joins = next.0 <= last.1;
        if joins {
            last.1 = last.1.max(next.1);
        }
        joins
    });
}

/// A copy all of whose aliases share its tiles, with ranges of it.
struct Root<'a> {
    copy: &'a Shadow,
    sparse: &'a Sparse,
    /// As declared, then joined.
    ranges: Vec<(usize, usize)>,
}

/// The sparse copies of `ranges`, in the order of their addresses (the order
/// in which the locks of several are taken).
fn roots(ranges: &[Range]) -> BTreeMap<usize, Root<'_>> {
    let mut roots = BTreeMap::new();
    for (shadow, address, len) in ranges {
        if let Some((copy, sparse)) = shadow.sparse_root() {
            let root = roots
                .entry(std::ptr::from_ref(copy) as usize)
                .or_insert_with(|| Root { copy, sparse, ranges: Vec::new() });
            root.ranges.push((*address, *address + *len));
        }
    }
    roots
}

impl Root<'_> {
    fn before(&self) -> MutexGuard<'_, std::collections::HashMap<u32, Vec<u8>, Mix>> {
        self.sparse.before.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The CPU's views of the tiles' parts of the joined ranges, in address
    /// order: title address, view, length.
    fn views(&self) -> Vec<(usize, usize, usize)> {
        let mut views = Vec::new();
        for &(from, to) in &self.ranges {
            views.extend(self.copy.tiles_of(self.sparse, from, to));
        }
        views
    }

    fn tile(&self, at: usize) -> u32 {
        ((at - self.copy.start as usize) / PAGE) as u32
    }
}

impl Shadow {
    /// [`Shadow::writing`] of a sparse copy without the snapshot, which
    /// [`keep_before`] takes for the whole submission: no refresh touches the
    /// range from here on. The refreshes before it must be done.
    pub(crate) fn close_to_refreshes(&self, address: usize, len: usize) {
        if let Some((root, _)) = self.sparse_root() {
            let range = (address, address + len);
            root.pending.lock().unwrap_or_else(|e| e.into_inner()).push(range);
        }
    }
}

/// After a submission's refreshes and before its lists run: keeps the bytes
/// of the sparse copies' `ranges`, which its shaders write
/// ([`Shadow::close_to_refreshes`]). Other copies' are kept as they are
/// declared ([`Shadow::writing`]).
///
/// Later than at each declaration, and no less right: a range declared is
/// left alone by every refresh, title copies write title memory and not the
/// copies, and the host GPU runs the lists afterwards. What else lies in the
/// tile may be refreshed meanwhile, and is not compared.
pub(crate) fn keep_before(ranges: &[Range]) {
    let mut roots = roots(ranges);
    for root in roots.values_mut() {
        join(&mut root.ranges);
    }
    // (the copy's bytes, where they are kept, how many): apart from each
    // other, as the joined ranges are.
    let mut copies: Vec<(usize, usize, usize)> = Vec::new();
    // The snapshots stay where they are while they are filled.
    let mut held = Vec::with_capacity(roots.len());
    for root in roots.values() {
        let views = root.views();
        let mut before = root.before();
        let mut new: Vec<u32> = views.iter().map(|v| root.tile(v.0)).collect();
        new.dedup();
        new.retain(|tile| !before.contains_key(tile));
        for (tile, mut kept) in new.iter().zip(super::pooled_tiles(new.len())) {
            // SAFETY: room for a tile; only bytes put there below are read.
            unsafe { kept.set_len(PAGE) };
            before.insert(*tile, kept);
        }
        for (at, view, n) in views {
            if let Some(kept) = before.get_mut(&root.tile(at)) {
                copies.push((view, kept.as_mut_ptr() as usize + at % PAGE, n));
            }
        }
        held.push(before);
    }
    if copies.is_empty() {
        return;
    }
    let bytes: usize = copies.iter().map(|c| c.2).sum();
    crate::telemetry::count(Count::Kept, copies.len() as u64);
    crate::telemetry::count(Count::KeptBytes, bytes as u64);
    #[cfg(feature = "diagnostics")]
    super::SPARSE_WRITES[1].fetch_add(bytes as u64, Ordering::Relaxed);
    crate::host::in_parallel(&copies, 16, |chunk| {
        for &(view, kept, n) in chunk {
            // SAFETY: a part of the tile's mapping; a part of its snapshot,
            // each written by one thread.
            unsafe { std::ptr::copy_nonoverlapping(view as *const u8, kept as *mut u8, n) };
        }
    });
    drop(held);
}

/// After a submission ran: [`Shadow::copy_back`] of all the ranges its
/// shaders write, each byte once. Returns the bytes written to title memory.
pub(crate) fn copy_back(ranges: &[Range]) -> usize {
    let mut written = dense(ranges);
    let mut roots = roots(ranges);
    if roots.is_empty() {
        return written;
    }
    // (title address, the copy's bytes, how many, the bytes kept): one
    // tile's part each, so no two touch the same memory.
    let mut work: Vec<(usize, usize, usize, usize)> = Vec::new();
    let mut held = Vec::with_capacity(roots.len());
    let mut declared: Vec<HashSet<(usize, usize)>> = Vec::with_capacity(roots.len());
    for root in roots.values_mut() {
        #[cfg(feature = "diagnostics")]
        {
            let bytes: usize = root.ranges.iter().map(|r| r.1 - r.0).sum();
            super::SPARSE_WRITES[0].fetch_add(bytes as u64, Ordering::Relaxed);
        }
        declared.push(root.ranges.iter().copied().collect());
        join(&mut root.ranges);
    }
    for root in roots.values() {
        let views = root.views();
        let mut before = root.before();
        work.extend(views.into_iter().filter_map(|(at, view, n)| {
            let kept = before.get_mut(&root.tile(at))?;
            Some((at, view, n, kept.as_mut_ptr() as usize + at % PAGE))
        }));
        held.push(before);
    }
    let bytes: usize = work.iter().map(|w| w.2).sum();
    crate::telemetry::count(Count::Compared, work.len() as u64);
    crate::telemetry::count(Count::ComparedBytes, bytes as u64);
    #[cfg(feature = "diagnostics")]
    super::SPARSE_WRITES[2].fetch_add(bytes as u64, Ordering::Relaxed);
    let put = AtomicUsize::new(0);
    crate::host::in_parallel(&work, 8, |chunk| {
        let mut title: Option<Vec<u8>> = None;
        for &(at, view, n, kept) in chunk {
            // SAFETY: a part of the tile's mapping and of its snapshot (kept
            // under the lock held above, its bytes put there before the
            // submission); this part is this thread's alone.
            let (host, old) = unsafe {
                (
                    std::slice::from_raw_parts_mut(view as *mut u8, n),
                    std::slice::from_raw_parts_mut(kept as *mut u8, n),
                )
            };
            if host == old {
                continue;
            }
            #[cfg(feature = "diagnostics")]
            super::SPARSE_WRITES[3].fetch_add(n as u64, Ordering::Relaxed);
            let buf = title.get_or_insert_with(|| {
                let mut v = super::pooled(PAGE);
                v.resize(PAGE, 0);
                v
            });
            if !crate::host::read_into(at as *const u8, &mut buf[..n]) {
                continue;
            }
            if let Some((from, to)) = super::merge_gpu_writes_mut(&mut buf[..n], host, old) {
                crate::watch::opening(at + from, to - from);
                crate::host::write_mapped((at + from) as *mut u8, buf[from..].as_ptr(), to - from);
                put.fetch_add(to - from, Ordering::Relaxed);
            }
        }
        if let Some(buf) = title {
            super::give_back(buf);
        }
    });
    written += put.into_inner();
    // Every list that declared them is done with the ranges (one submission
    // is prepared at a time, and only when all before it have come here),
    // and the snapshots with the last range.
    for ((root, mut before), declared) in roots.values().zip(held).zip(declared) {
        let mut pending = root.copy.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.retain(|r| !declared.contains(r));
        if pending.is_empty() {
            super::give_back_tiles(before.drain().map(|(_, kept)| kept));
        }
    }
    written
}

/// [`copy_back`] for copies that are not sparse: their ranges one by one,
/// different copies on several threads.
fn dense(ranges: &[Range]) -> usize {
    let mut copies: Vec<Vec<&Range>> = Vec::new();
    for range in ranges.iter().filter(|r| r.0.sparse.is_none()) {
        match copies.iter_mut().find(|c| Arc::ptr_eq(&c[0].0, &range.0)) {
            Some(copy) => copy.push(range),
            None => copies.push(vec![range]),
        }
    }
    let written = AtomicUsize::new(0);
    crate::host::in_parallel(&copies, 8, |chunk| {
        for (shadow, address, len) in chunk.iter().flatten().copied() {
            written.fetch_add(shadow.copy_back(*address, *len), Ordering::Relaxed);
        }
    });
    written.into_inner()
}

thread_local! {
    /// See [`early`].
    static EARLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `f`, which refreshes copies before their turn, while a submission
/// before may still change title memory: memory that is copied once a frame
/// for want of a watch on it is left for the refresh in its turn, or that
/// one would find it copied already.
pub(crate) fn early<R>(f: impl FnOnce() -> R) -> R {
    struct Back(bool);
    impl Drop for Back {
        fn drop(&mut self) {
            EARLY.set(self.0);
        }
    }
    let _back = Back(EARLY.replace(true));
    f()
}

/// Whether this thread is in [`early`].
pub(super) fn is_early() -> bool {
    EARLY.get()
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };

    const TILES: usize = 12;
    const SIZE: usize = TILES * PAGE;

    /// Title memory with a sparse copy of it.
    struct Copied {
        base: usize,
        shadow: Arc<Shadow>,
    }

    impl Copied {
        fn new() -> Option<Copied> {
            // SAFETY: a private, page-aligned writable region for the test.
            let base = unsafe {
                VirtualAlloc(std::ptr::null(), SIZE, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE)
            } as usize;
            assert_ne!(base, 0);
            let Some(shadow) = super::super::sparse_buffer(base as u64, SIZE as u64, None) else {
                eprintln!("no sparse D3D12 buffer; skipped");
                return None;
            };
            let copied = Copied { base, shadow: Arc::new(shadow) };
            for at in 0..SIZE {
                // SAFETY: inside the region.
                unsafe { ((base + at) as *mut u8).write((at * 7 + at / 251) as u8) };
            }
            for tile in 0..TILES {
                // SAFETY: the tile's mapping and its title memory.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        (base + tile * PAGE) as *const u8,
                        copied.host(tile * PAGE),
                        PAGE,
                    )
                };
            }
            Some(copied)
        }

        /// The copy's bytes at `offset`.
        fn host(&self, offset: usize) -> *mut u8 {
            let sparse = self.shadow.sparse.as_ref().unwrap();
            let tile = self.shadow.tile(sparse, self.base + offset).expect("a mapped tile");
            (tile + offset % PAGE) as *mut u8
        }

        fn title(&self) -> Vec<u8> {
            // SAFETY: the region, which only the test writes.
            unsafe { std::slice::from_raw_parts(self.base as *const u8, SIZE) }.to_vec()
        }

        fn range(&self, from: usize, to: usize) -> Range {
            (self.shadow.clone(), self.base + from, to - from)
        }

        /// The host GPU and the title's CPU each write some bytes.
        fn write(&self) {
            for at in (5..SIZE).step_by(997) {
                // SAFETY: inside the copy.
                unsafe { self.host(at).write(0xa0 | (at % 13) as u8) };
            }
            for at in (11..SIZE).step_by(1499) {
                // SAFETY: inside the region.
                unsafe { ((self.base + at) as *mut u8).write(0x50 | (at % 11) as u8) };
            }
        }
    }

    impl Drop for Copied {
        fn drop(&mut self) {
            // SAFETY: our region.
            unsafe { VirtualFree(self.base as *mut _, 0, MEM_RELEASE) };
        }
    }

    /// Overlapping, touching, repeated and unaligned ranges.
    const RANGES: [(usize, usize); 9] = [
        (3, PAGE + 100),
        (PAGE - 9, 3 * PAGE + 5),
        (3, PAGE + 100),
        (3 * PAGE + 5, 3 * PAGE + 64),
        (5 * PAGE + 17, 5 * PAGE + 18),
        (5 * PAGE + 4000, 5 * PAGE + 9000),
        (7 * PAGE, 9 * PAGE),
        (7 * PAGE + 1, 8 * PAGE),
        (11 * PAGE, SIZE),
    ];

    #[test]
    fn ranges_join_where_they_meet_and_not_over_gaps() {
        let mut ranges = vec![(40, 50), (10, 20), (20, 25), (45, 60), (7, 7), (61, 62)];
        join(&mut ranges);
        assert_eq!(ranges, [(10, 25), (40, 60), (61, 62)]);
        join(&mut ranges);
        assert_eq!(ranges, [(10, 25), (40, 60), (61, 62)]);
        let mut none = Vec::new();
        join(&mut none);
        assert!(none.is_empty());
    }

    /// One pass over a submission's ranges leaves the same title bytes as a
    /// pass for each, and nothing behind: what shaders wrote is in title
    /// memory inside the ranges only, what the title wrote meanwhile stays.
    #[test]
    fn one_pass_puts_back_what_a_pass_per_range_does() {
        let (Some(one), Some(each)) = (Copied::new(), Copied::new()) else { return };
        let before = one.title();
        assert_eq!(before, each.title());

        let all: Vec<Range> = RANGES.iter().map(|r| one.range(r.0, r.1)).collect();
        for (shadow, address, len) in &all {
            shadow.close_to_refreshes(*address, *len);
        }
        keep_before(&all);
        one.write();
        let written = copy_back(&all);

        for &(from, to) in &RANGES {
            each.shadow.writing(each.base + from, to - from);
        }
        each.write();
        let mut passes = 0;
        for &(from, to) in &RANGES {
            passes += each.shadow.copy_back(each.base + from, to - from);
        }
        assert!(one.title() == each.title(), "the same title bytes");
        assert!(written > 0 && passes > 0, "{written} bytes in one pass, {passes} in a pass each");
        let declared = |at: usize| RANGES.iter().any(|&(from, to)| (from..to).contains(&at));
        for at in (5..SIZE).step_by(997) {
            let now = one.title()[at];
            if declared(at) {
                assert_eq!(now, 0xa0 | (at % 13) as u8, "written by a shader at {at:#x}");
            } else if at < 11 || !(at - 11).is_multiple_of(1499) {
                assert_eq!(now, before[at], "not declared at {at:#x}");
            }
        }
        for copied in [&one, &each] {
            assert!(copied.shadow.pending.lock().unwrap().is_empty());
            assert!(copied.shadow.sparse.as_ref().unwrap().before.lock().unwrap().is_empty());
        }
    }

    /// Two copies' ranges in one pass: each goes into its own title memory,
    /// and a range of one that another submission still has closed stays.
    #[test]
    fn copies_share_a_pass() {
        let (Some(a), Some(b)) = (Copied::new(), Copied::new()) else { return };
        b.shadow.close_to_refreshes(b.base + 10 * PAGE, 64);
        let all = [a.range(0, 2 * PAGE), b.range(PAGE, 4 * PAGE), a.range(PAGE, 3 * PAGE)];
        for (shadow, address, len) in &all {
            shadow.close_to_refreshes(*address, *len);
        }
        keep_before(&all);
        for (copied, at) in [(&a, PAGE + 9), (&b, PAGE + 9), (&a, 3 * PAGE + 1), (&b, 3 * PAGE + 1)] {
            // SAFETY: inside the copies.
            unsafe { copied.host(at).write(copied.title()[at] ^ 0xff) };
        }
        let before = [a.title(), b.title()];
        copy_back(&all);
        let changed = [vec![PAGE + 9], vec![PAGE + 9, 3 * PAGE + 1]];
        for ((copied, before), changed) in [&a, &b].into_iter().zip(&before).zip(changed) {
            let now = copied.title();
            let differ: Vec<usize> = (0..SIZE).filter(|&at| now[at] != before[at]).collect();
            assert_eq!(differ, changed);
        }
        assert!(a.shadow.pending.lock().unwrap().is_empty());
        assert_eq!(*b.shadow.pending.lock().unwrap(), [(b.base + 10 * PAGE, b.base + 10 * PAGE + 64)]);
    }

    /// Refreshed early, memory nothing watches is refreshed again in its
    /// turn.
    #[test]
    fn early_refreshes_leave_unwatched_memory_for_its_turn() {
        assert!(!is_early());
        early(|| {
            assert!(is_early());
            early(|| assert!(is_early()));
            assert!(is_early());
        });
        assert!(!is_early());
        let Some(copied) = Copied::new() else { return };
        let at = PAGE + 4;
        // SAFETY: inside the region.
        unsafe { ((copied.base + at) as *mut u8).write(0xee) };
        assert_eq!(early(|| copied.shadow.update(copied.base + at, 8)), 0);
        // SAFETY: inside the copy.
        assert_ne!(unsafe { copied.host(at).read() }, 0xee);
        assert!(copied.shadow.update(copied.base + at, 8) > 0);
        // SAFETY: as above.
        assert_eq!(unsafe { copied.host(at).read() }, 0xee);
    }

    /// What one pass saves where views of the same memory are declared over
    /// and over: `cargo test --release -p victory-gpu -- --ignored --nocapture passes_over`.
    #[test]
    #[ignore = "benchmark"]
    fn passes_over_overlapping_ranges() {
        let Some(copied) = Copied::new() else { return };
        let all: Vec<Range> = (0..48).map(|i| copied.range(i * 64, SIZE - i * 256)).collect();
        let rounds = 50;
        let mut took = [std::time::Duration::ZERO; 4];
        for _ in 0..rounds {
            let t = std::time::Instant::now();
            for (shadow, address, len) in &all {
                shadow.writing(*address, *len);
            }
            took[0] += t.elapsed();
            let t = std::time::Instant::now();
            for (shadow, address, len) in &all {
                shadow.copy_back(*address, *len);
            }
            took[1] += t.elapsed();
            let t = std::time::Instant::now();
            for (shadow, address, len) in &all {
                shadow.close_to_refreshes(*address, *len);
            }
            keep_before(&all);
            took[2] += t.elapsed();
            let t = std::time::Instant::now();
            copy_back(&all);
            took[3] += t.elapsed();
        }
        println!(
            "48 ranges over {SIZE} bytes: kept {:?} -> {:?}, compared {:?} -> {:?}",
            took[0] / rounds,
            took[2] / rounds,
            took[1] / rounds,
            took[3] / rounds
        );
    }
}
