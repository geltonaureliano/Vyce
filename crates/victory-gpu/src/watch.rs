//! Which title memory the title wrote since a host copy took it (decision
//! 0007, "Copies of title memory"). The runtime reserves title GPU memory
//! with write watching (`victory-hle` xmem), so the kernel keeps a dirty bit
//! per 4 KiB page; asking for a range's written pages also clears them. Host
//! copies of the same memory can overlap (nested buffers, textures in
//! buffers), so what one asks is kept here for the others: a journal of
//! each page's last write, numbered by the asking.
//!
//! Physical-page views (xmem's sections) cannot be write-watched. There a
//! copy leaves each 64 KiB block it took read-only, and our vectored handler
//! takes the first write to it: it notes the block as written and gives the
//! write access back. The runtime tells us when it maps or unmaps views
//! ([`title_memory_remapped`]), which resets their protection.
//!
//! The title shows one physical page at several addresses. A write of its
//! own through any of them is seen: the runtime tells us the views it maps
//! ([`title_memory_view`]), guarding a block makes every other view of its
//! page read-only too, and a fault at one of them opens them all. Without
//! this a copy taken at one address stayed as it was for the rest of the
//! run when the title wrote through another (parts of moving vehicles left
//! behind and stretched across the screen, 2026-11-25). The idea is
//! victory-fork #115's (Spexivus, joroyt) as carried by victory-amd; the
//! code here is written anew. `VICTORY_VIEW_GUARD=0` turns it off.
//!
//! shortcut: the runtime's own writes into physical pages through a view of
//! its own (xmem's `write_physical`, DirectStorage's page destinations) are
//! told by address ([`title_memory_written`]), not through this. A page
//! shown at more than [`MANY_VIEWS`] addresses is guarded one view at a
//! time. A system call writing into a guarded block fails (none is known
//! to).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Per-64-MiB-region epoch table. Incremented for only the region(s) touched
/// whenever a guarded block is opened (VEH guard fault or `writing`),
/// remapped (`title_memory_remapped`), or rewritten (`title_memory_written`).
/// While a span's region epoch stays unchanged, any range already guarded and
/// copied at that epoch cannot have been modified by the title — and
/// `copy_back` on one buffer no longer invalidates clean spans in other regions.
const REGION_SHIFT: usize = 26; // 64 MiB per bucket
const REGION_BUCKETS: usize = 1024;
static REGION_EPOCHS: [AtomicU64; REGION_BUCKETS] =
    [const { AtomicU64::new(1) }; REGION_BUCKETS];

#[inline]
fn bump_region_epochs(start: usize, end: usize) {
    if start >= end {
        return;
    }
    let first = start >> REGION_SHIFT;
    let last = (end - 1) >> REGION_SHIFT;
    let count = last - first + 1;
    if count >= REGION_BUCKETS {
        for slot in &REGION_EPOCHS {
            slot.fetch_add(1, Ordering::Release);
        }
    } else {
        for r in first..=last {
            REGION_EPOCHS[r & (REGION_BUCKETS - 1)].fetch_add(1, Ordering::Release);
        }
    }
}

#[inline]
pub(crate) fn region_epoch(start: usize, end: usize) -> u64 {
    if start >= end {
        return 0;
    }
    let first = start >> REGION_SHIFT;
    let last = (end - 1) >> REGION_SHIFT;
    let count = last - first + 1;
    let mut hash = 0u64;
    if count >= REGION_BUCKETS {
        for slot in &REGION_EPOCHS {
            hash = hash
                .wrapping_mul(0x9e37_79b9_7f4a_7c15)
                .wrapping_add(slot.load(Ordering::Acquire));
        }
    } else {
        for r in first..=last {
            hash = hash
                .wrapping_mul(0x9e37_79b9_7f4a_7c15)
                .wrapping_add(REGION_EPOCHS[r & (REGION_BUCKETS - 1)].load(Ordering::Acquire));
        }
    }
    hash
}

/// The unit tracked and copied: 64 KiB, the console's page (the kernel's
/// 4 KiB pages fold into it). Finer units made the checks of the world's
/// big buffers cost more than the copies they saved.
pub(crate) const PAGE: usize = 64 << 10;
/// The kernel's page, the unit of its answers.
const KERNEL_PAGE: usize = 4096;

struct Journal {
    /// Numbers the askings; 0 is "never".
    asked: u64,
    /// Page number → the asking that last found it written.
    written: BTreeMap<usize, u64>,
    /// Room for the kernel's answer.
    found: Vec<usize>,
    /// Page ranges asked about ahead of a group of refreshes
    /// ([`ask_ahead`]): until it is done, [`writes`] answers for a range
    /// inside one from the journal, without another kernel call.
    ahead: Vec<(usize, usize)>,
}

impl Journal {
    fn pages(&self, first: usize, last: usize) -> Vec<(usize, u64)> {
        self.written
            .range(first..last)
            .map(|(&page, &when)| (page, when))
            .collect()
    }
}

static JOURNAL: Mutex<Journal> = Mutex::new(Journal {
    asked: 0,
    written: BTreeMap::new(),
    found: Vec::new(),
    ahead: Vec::new(),
});

/// The pages of `[start, end)` (whole pages) as last written, for pages
/// written since tracking began: `(page number, asking)` in page order, and
/// this asking's number, which is higher than any in the list but those the
/// kernel just reported. `None` if the memory is not write-watched (nor,
/// with `guarded`, view memory, which is then guarded).
pub(crate) fn writes(start: usize, end: usize, guarded: bool) -> Option<(u64, Vec<(usize, u64)>)> {
    observe_writes(start, end, guarded, true, Journal::pages)
}

/// Records writes and returns only their generation. Copy-back already knows
/// which pages it synchronized, so it does not need a snapshot of the journal.
/// Always asks the kernel: what it finds written are our own writes too,
/// which must not count as the title's later.
pub(crate) fn copy_epoch(start: usize, end: usize, known_guarded: bool) -> Option<u64> {
    let epoch_only = |_: &Journal, _: usize, _: usize| ();
    if known_guarded {
        observe_guarded_writes(start, end, epoch_only)
    } else {
        observe_writes(start, end, true, false, epoch_only)
    }
    .map(|(now, ())| now)
}

/// Whether a group of refreshes asks the kernel once for all of their
/// write-watched memory ([`ask_ahead`]; `VYCE_ASK_AHEAD=0`: each refresh
/// asks for its own range, as before).
pub(crate) fn ask_ahead_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VYCE_ASK_AHEAD").is_none_or(|v| v != "0"))
}

/// Ranges of one group closer than this (in pages) are asked about
/// together: walking the pages between them costs less than another kernel
/// call (one that resets written pages costs tens of microseconds whatever
/// the range).
const AHEAD_GAP: usize = (8 << 20) / PAGE;
/// The most one asking covers (its answer needs 8 bytes per 4 KiB page).
const AHEAD_MOST: usize = (1 << 30) / PAGE;

/// Until dropped, refreshes of what [`ask_ahead`] asked are answered from
/// the journal.
pub(crate) struct Ahead(());

impl Drop for Ahead {
    fn drop(&mut self) {
        JOURNAL.lock().unwrap_or_else(|e| e.into_inner()).ahead.clear();
    }
}

/// Asks the kernel about many ranges `(start, end)` of write-watched memory
/// at once, before the [`writes`] of each: ranges close together are one
/// asking (halved where one fails: across two reservations, or into memory
/// that is not write-watched), and until the result is dropped `writes`
/// answers for them from the journal. A range alone is left to `writes`.
///
/// No less right than an asking per range, as long as nothing of ours
/// writes title memory meanwhile (refreshes write their copies): the
/// journal keeps what each asking found for every asker, and a page the
/// title writes after this asking is still written for the kernel, so the
/// next asking finds it. What changes: such a write is seen by the next
/// group of refreshes, not by this one. `ASK_ONCE`/`ask_ahead` of
/// github.com/avatarmanz/VIctory-7900XTX (0a43566, from fxceless's
/// 578d459), here for a group of refreshes rather than a whole submission.
pub(crate) fn ask_ahead(ranges: &[(usize, usize)]) -> Option<Ahead> {
    if !ask_ahead_on() {
        return None;
    }
    let mut pieces: Vec<(usize, usize)> = ranges
        .iter()
        .filter(|r| r.0 < r.1)
        .map(|&(start, end)| (start / PAGE, end.div_ceil(PAGE)))
        .collect();
    if pieces.len() < 2 {
        return None;
    }
    pieces.sort_unstable();
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    // Another group's asking stands (one thread refreshes at a time).
    if !j.ahead.is_empty() {
        return None;
    }
    let (mut asked, mut written) = (Vec::new(), Vec::new());
    let mut i = 0;
    while i < pieces.len() {
        let first = pieces[i].0;
        let mut last = pieces[i].1;
        let mut k = i + 1;
        while k < pieces.len() && pieces[k].0 <= last + AHEAD_GAP && last.max(pieces[k].1) - first <= AHEAD_MOST {
            last = last.max(pieces[k].1);
            k += 1;
        }
        ask_run(&mut j, &pieces[i..k], &mut asked, &mut written);
        i = k;
    }
    if asked.is_empty() {
        return None;
    }
    j.asked += 1;
    let now = j.asked;
    for page in written {
        j.written.insert(page, now);
    }
    j.ahead = asked;
    Some(Ahead(()))
}

/// One kernel asking for `pieces` (page ranges in order); by halves if it
/// fails. A piece alone is left to [`writes`]. Adds what was asked to
/// `asked` and the pages found written to `written`.
fn ask_run(j: &mut Journal, pieces: &[(usize, usize)], asked: &mut Vec<(usize, usize)>, written: &mut Vec<usize>) {
    if pieces.len() < 2 {
        return;
    }
    let first = pieces[0].0;
    let last = pieces.iter().map(|p| p.1).max().unwrap_or(first);
    if let Some(count) = kernel_written(j, first, last) {
        written.extend(j.found[..count].chunk_by(|a, b| *a / PAGE == *b / PAGE).map(|hits| hits[0] / PAGE));
        asked.push((first, last));
        return;
    }
    let (a, b) = pieces.split_at(pieces.len() / 2);
    ask_run(j, a, asked, written);
    ask_run(j, b, asked, written);
}

/// Asks the kernel which 4 KiB pages of the pages `[first, last)` were
/// written since it was last asked (and forgets that): how many it put into
/// `j.found`, `None` if the range is not write-watched memory of one
/// reservation. Under the journal's lock.
fn kernel_written(j: &mut Journal, first: usize, last: usize) -> Option<usize> {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    let mut count = (last - first) * PAGE / KERNEL_PAGE;
    j.found.resize(count.max(j.found.len()), 0);
    let mut granularity = 0;
    let asking = crate::telemetry::start();
    // SAFETY: the answer buffer holds `count` entries.
    let failed = unsafe {
        GetWriteWatch(
            WRITE_WATCH_FLAG_RESET,
            (first * PAGE) as *const _,
            (last - first) * PAGE,
            j.found.as_mut_ptr().cast(),
            &mut count,
            &mut granularity,
        )
    } != 0;
    crate::telemetry::count(crate::telemetry::Count::WatchAsks, 1);
    crate::telemetry::stop(crate::telemetry::Count::WatchTime, asking);
    (!failed).then_some(count)
}

/// `ahead`: a range a group's asking holds may be answered from the journal.
fn observe_writes<T>(
    start: usize,
    end: usize,
    guarded: bool,
    ahead: bool,
    report: impl FnOnce(&Journal, usize, usize) -> T,
) -> Option<(u64, T)> {
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    let j = &mut *j;
    if ahead && j.ahead.iter().any(|&(a, b)| a <= first && last <= b) {
        crate::telemetry::count(crate::telemetry::Count::WatchAhead, 1);
        // Every asking so far is in the journal: a copy taken from now on
        // holds what they found.
        return Some((j.asked, report(j, first, last)));
    }
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    let asked = kernel_written(j, first, last);
    let count = asked.unwrap_or(0);
    let guarded = if asked.is_none() {
        if !guarded {
            return None;
        }
        guard(first * PAGE, last * PAGE)?
    } else {
        Vec::new()
    };
    #[cfg(feature = "diagnostics")]
    {
        use crate::copystat::{C, add};
        add(C::Watches, 1);
        add(C::WatchUs, started.elapsed().as_micros() as u64);
    }
    j.asked += 1;
    let now = j.asked;
    let Journal { written, found, .. } = j;
    // The kernel reports 4 KiB addresses. Adjacent hits in the same journal
    // page mark the same generation and only need one insertion.
    let pages = found[..count]
        .chunk_by(|a, b| *a / PAGE == *b / PAGE)
        .map(|hits| hits[0] / PAGE);
    for page in pages.chain(guarded) {
        written.insert(page, now);
    }
    Some((now, report(j, first, last)))
}

/// Same as [`writes`] for memory already known to be guarded `MEM_MAPPED`
/// view memory: skips the `GetWriteWatch` syscall (which always fails on
/// mapped views).
pub(crate) fn writes_guarded(start: usize, end: usize) -> Option<(u64, Vec<(usize, u64)>)> {
    observe_guarded_writes(start, end, Journal::pages)
}

fn observe_guarded_writes<T>(
    start: usize,
    end: usize,
    report: impl FnOnce(&Journal, usize, usize) -> T,
) -> Option<(u64, T)> {
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    let guarded = guard(first * PAGE, last * PAGE)?;
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    let j = &mut *j;
    #[cfg(feature = "diagnostics")]
    {
        use crate::copystat::{C, add};
        add(C::Watches, 1);
        add(C::WatchUs, started.elapsed().as_micros() as u64);
    }
    j.asked += 1;
    let now = j.asked;
    for page in guarded {
        j.written.insert(page, now);
    }
    Some((now, report(&j, first, last)))
}

/// Guarded blocks of view memory.
const BLOCK: usize = PAGE;

struct Guards {
    /// Blocks we made read-only.
    protected: BTreeSet<usize>,
    /// Of those, the ones not written since a copy took them.
    clean: BTreeSet<usize>,
    /// Unmapped blocks (`VirtualProtect` failed) within guarded ranges;
    /// cleared by [`title_memory_remapped`] when the runtime maps them.
    holes: BTreeSet<usize>,
    /// Blocks whose page was guarded together with its other views (those
    /// views' blocks are in it too): a write to one opens them all.
    aliased: BTreeSet<usize>,
    /// Blocks the kernel is about to write for the runtime ([`writing`]) and
    /// until when they are left writable: a read may be asynchronous and
    /// land after the call that asked for it returned, and the kernel does
    /// not fault into our handler, it fails. Until then they are not made
    /// read-only, and every check reports them written.
    pins: BTreeMap<usize, std::time::Instant>,
    /// When the pins that ended were last taken out.
    pruned: Option<std::time::Instant>,
}

static GUARDS: Mutex<Guards> = Mutex::new(Guards {
    protected: BTreeSet::new(),
    clean: BTreeSet::new(),
    holes: BTreeSet::new(),
    aliased: BTreeSet::new(),
    pins: BTreeMap::new(),
    pruned: None,
});

/// How long blocks stay writable after [`writing`].
const PIN: std::time::Duration = std::time::Duration::from_secs(2);

impl Guards {
    /// Whether `block` is left writable for a kernel write.
    fn pinned(&self, block: usize) -> bool {
        self.pins.get(&block).is_some_and(|&until| until > std::time::Instant::now())
    }

    /// Leaves the blocks `[from, to)` writable until `until`.
    fn pin(&mut self, from: usize, to: usize, until: std::time::Instant) {
        if to - from < 256 {
            self.pins.extend((from..to).map(|block| (block, until)));
        } else {
            // Merged in one go: a window of a gigabyte has 16,384 blocks.
            let mut more: BTreeMap<_, _> = (from..to).map(|block| (block, until)).collect();
            self.pins.append(&mut more);
        }
        crate::telemetry::note(crate::telemetry::Gauge::Pins, self.pins.len() as u64);
    }

    /// Takes out pins that ended, now and then.
    fn prune(&mut self) {
        const OFTEN: std::time::Duration = std::time::Duration::from_millis(100);
        let now = std::time::Instant::now();
        if self.pins.is_empty() || self.pruned.is_some_and(|at| now - at < OFTEN) {
            return;
        }
        self.pruned = Some(now);
        self.pins.retain(|_, until| *until > now);
    }
}

/// Title memory some block of which was made read-only, one bit for every
/// 2 MiB of the first TiB (the title's GPU addresses have 40 bits, its
/// memory lies far below that): set before the block is, never cleared.
/// Where no bit is set there is nothing to open before a write of ours.
static TOUCHED: [AtomicU64; TOUCHED_WORDS] = [const { AtomicU64::new(0) }; TOUCHED_WORDS];
const TOUCHED_SHIFT: usize = 21;
const TOUCHED_WORDS: usize = (1 << (40 - TOUCHED_SHIFT)) / 64;

/// Notes that blocks of `[from, to)` (block numbers) may be read-only from
/// now on.
fn touch(from: usize, to: usize) {
    let (first, last) = (from * BLOCK >> TOUCHED_SHIFT, (to * BLOCK - 1) >> TOUCHED_SHIFT);
    for bit in first..=last.min(TOUCHED_WORDS * 64 - 1) {
        let (word, bit) = (&TOUCHED[bit / 64], 1u64 << (bit % 64));
        if word.load(Ordering::Relaxed) & bit == 0 {
            word.fetch_or(bit, Ordering::Release);
        }
    }
}

/// Whether a block of `[at, at + len)` may be read-only (`len` not 0).
fn touched(at: usize, len: usize) -> bool {
    let (first, last) = (at >> TOUCHED_SHIFT, (at + len - 1) >> TOUCHED_SHIFT);
    last >= TOUCHED_WORDS * 64
        || (first..=last).any(|bit| TOUCHED[bit / 64].load(Ordering::Acquire) >> (bit % 64) & 1 != 0)
}

/// Whether a check that finds its range guarded already honours pins too
/// (`VYCE_PIN_ALWAYS=0`: as before, only a range's first check did, and a
/// block waiting for a kernel read could be made read-only under it).
fn pin_always() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VYCE_PIN_ALWAYS").is_none_or(|v| v != "0"))
}

/// Whether our own copies open title memory without pinning it
/// (`VYCE_OPENING=0`: they go through [`writing`], as before).
pub(crate) fn opens_only() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VYCE_OPENING").is_none_or(|v| v != "0"))
}

/// Views of one page past which its views are not guarded together (the
/// page the title shows wherever it has nothing has thousands).
pub(crate) const MANY_VIEWS: usize = 64;

/// The views of physical pages the runtime mapped: by address (length,
/// section, offset in it), and the addresses each page of a section is
/// shown at.
#[derive(Default)]
struct Views {
    by_addr: BTreeMap<usize, (usize, usize, u64)>,
    pages: std::collections::HashMap<(usize, u64), Vec<usize>>,
}

static VIEWS: Mutex<Option<Views>> = Mutex::new(None);

/// The longest stretch of views around `[at, end)` that follow one another
/// in the address space and in one section alike, within `reach` bytes of
/// `at` rounded down to it: its start, its end, the section and the offset
/// in it. `None` if the range is not in one such stretch.
pub(crate) fn stretch(at: usize, end: usize, reach: usize) -> Option<(usize, usize, usize, u64)> {
    let views = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
    let views = views.as_ref()?;
    let (&first, &(n, section, offset)) = views.by_addr.range(..=at).next_back()?;
    if first + n <= at {
        return None;
    }
    let (low, high) = (at - at % reach, (at - at % reach).checked_add(reach)?);
    if end > high {
        return None;
    }
    // Up from the view holding `at`.
    let (mut stop, mut next) = (first + n, offset + n as u64);
    for (&a, &(n, s, o)) in views.by_addr.range(first + 1..high) {
        if a != stop || s != section || o != next {
            break;
        }
        (stop, next) = (a + n, o + n as u64);
    }
    if stop < end {
        return None;
    }
    // And down from it.
    let (mut start, mut from) = (first, offset);
    for (&a, &(n, s, o)) in views.by_addr.range(low..first).rev() {
        if a + n != start || s != section || o + n as u64 != from {
            break;
        }
        (start, from) = (a, o);
    }
    let start_in = start.max(low);
    Some((start_in, stop.min(high), section, from + (start_in - start) as u64))
}

/// Diagnostics: how `[at, at + len)` is made of views of physical pages:
/// the views, their sections, the longest view, and the runs of views that
/// follow one another in their section too (one run: the range is one
/// stretch of a section).
pub(crate) fn layout(at: usize, len: usize) -> (usize, usize, usize, usize) {
    let views = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(views) = views.as_ref() else { return (0, 0, 0, 0) };
    let end = at.saturating_add(len);
    let first = views
        .by_addr
        .range(..=at)
        .next_back()
        .filter(|(a, (n, _, _))| *a + n > at)
        .map(|(a, _)| *a)
        .unwrap_or(at);
    let (mut count, mut longest, mut runs) = (0, 0, 0);
    let mut sections = std::collections::BTreeSet::new();
    let mut before: Option<(usize, usize, u64)> = None;
    for (&a, &(n, section, offset)) in views.by_addr.range(first..end) {
        count += 1;
        longest = longest.max(n);
        sections.insert(section);
        let follows = before.is_some_and(|(pa, ps, po)| pa == a && ps == section && po == offset);
        if !follows {
            runs += 1;
        }
        before = Some((a + n, section, offset + n as u64));
    }
    (count, sections.len(), longest, runs)
}

/// Whether all the views of a guarded page are guarded together.
fn view_guard() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_VIEW_GUARD").is_none_or(|v| v != "0"))
}

impl Views {
    /// Forgets the views overlapping `[at, at + len)`, whole.
    fn forget(&mut self, at: usize, len: usize) {
        let end = at.saturating_add(len);
        let first = self
            .by_addr
            .range(..=at)
            .next_back()
            .filter(|(a, (n, _, _))| *a + n > at)
            .map(|(a, _)| *a);
        let hit: Vec<usize> = first
            .into_iter()
            .chain(self.by_addr.range(at.saturating_add(1)..end).map(|(a, _)| *a))
            .collect();
        for a in hit {
            let Some((n, section, offset)) = self.by_addr.remove(&a) else {
                continue;
            };
            for k in 0..n / BLOCK {
                let key = (section, offset / BLOCK as u64 + k as u64);
                if let Some(shown) = self.pages.get_mut(&key) {
                    shown.retain(|&x| x != a + k * BLOCK);
                    if shown.is_empty() {
                        self.pages.remove(&key);
                    }
                }
            }
        }
    }

    /// Every address the page shown at `at` (a block's start) is shown at,
    /// `at` included; empty if `at` is in no view we were told of.
    fn shown(&self, at: usize) -> &[usize] {
        let Some((&base, &(n, section, offset))) = self.by_addr.range(..=at).next_back() else {
            return &[];
        };
        if at >= base + n {
            return &[];
        }
        let key = (section, offset / BLOCK as u64 + ((at - base) / BLOCK) as u64);
        self.pages.get(&key).map_or(&[], Vec::as_slice)
    }
}

/// Every address the page of the block at `at` is shown at (see
/// [`Views::shown`]).
fn aliases(at: usize) -> Vec<usize> {
    let views = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
    views.as_ref().map_or(Vec::new(), |v| v.shown(at - at % BLOCK).to_vec())
}

/// The runtime mapped a view at `[at, at + len)` of the bytes from `offset`
/// on of the physical-page section `section`. The view is writable, so the
/// same pages guarded at another view are opened: their copies check them
/// again, which also guards the new view with them.
pub fn title_memory_view(at: usize, len: usize, section: usize, offset: u64) {
    if !view_guard() || len == 0 || !at.is_multiple_of(BLOCK) || !offset.is_multiple_of(BLOCK as u64) {
        return;
    }
    let mut others = Vec::new();
    {
        let mut views = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
        let views = views.get_or_insert_with(Views::default);
        views.forget(at, len);
        views.by_addr.insert(at, (len, section, offset));
        for k in 0..len / BLOCK {
            let shown = views.pages.entry((section, offset / BLOCK as u64 + k as u64)).or_default();
            if shown.len() < MANY_VIEWS {
                others.extend_from_slice(shown);
            }
            shown.push(at + k * BLOCK);
        }
    }
    if !others.is_empty() {
        let mut g = guards();
        for a in others {
            open_here(&mut g, a, a + BLOCK);
        }
    }
}

/// The runtime unmapped the views in `[at, at + len)`.
pub fn title_memory_view_gone(at: usize, len: usize) {
    if let Some(views) = VIEWS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        views.forget(at, len);
    }
}

fn guards() -> std::sync::MutexGuard<'static, Guards> {
    GUARDS.lock().unwrap_or_else(|e| e.into_inner())
}

/// For view memory: the pages of `[start, end)`'s blocks written since
/// they were last guarded (all of a block's pages), guarding them again
/// before the caller copies them. `None` if the memory is not a view.
fn guard(start: usize, end: usize) -> Option<Vec<usize>> {
    let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
    // Fast path: if we already guard or recorded a hole at `first`, we already
    // know this is a mapped view and can skip `QueryVirtualMemoryInformation`.
    {
        let mut g = guards();
        if g.protected.contains(&first) || g.holes.contains(&first) {
            return Some(guard_blocks(&mut g, first, last, false, pin_always()));
        }
    }
    // Slow path (first encounter): `start` is itself a mapped view, or if
    // `start` is an unmapped hole (common in 2 GiB sparse shadows), jump past
    // leading holes via `VirtualQuery` to find the first committed page.
    let probe = if mapped(start) {
        start
    } else {
        first_mapped(start, end)?
    };
    install();
    Some(guard_blocks(&mut guards(), first, last, probe != start, true))
}

/// [`guard`] for the blocks `[first, last)`: the pages written. `check_each`:
/// some of them may be no view memory. `pins`: blocks waiting for a kernel
/// write are left alone.
fn guard_blocks(g: &mut Guards, first: usize, last: usize, check_each: bool, pins: bool) -> Vec<usize> {
    g.prune();
    let pins = pins && !g.pins.is_empty();
    let mut written = Vec::new();
    let mut run: Option<usize> = None;
    for block in first..=last {
        let open = block < last
            && !g.clean.contains(&block)
            && !g.holes.contains(&block)
            && (!check_each || mapped(block * BLOCK));
        if open {
            if pins && g.pinned(block) {
                // The kernel may be writing it: left writable, reported
                // written at every check until the pin ends.
                written.extend(block * BLOCK / PAGE..(block + 1) * BLOCK / PAGE);
                // No range holding it is clean meanwhile (`region_epoch`).
                bump_region_epochs(block * BLOCK, (block + 1) * BLOCK);
            } else if g.protected.contains(&block) {
                // Written through the runtime, still read-only.
                g.clean.insert(block);
                written.extend(block * BLOCK / PAGE..(block + 1) * BLOCK / PAGE);
            } else {
                run.get_or_insert(block);
                continue;
            }
        }
        if let Some(from) = run.take() {
            protect(g, from, block, &mut written);
        }
    }
    written
}

/// Scans `[start, end)` jumping over unmapped holes; returns the first
/// committed address if it is a mapped view, or `None` if `[start, end)` has
/// no committed mapped view (or starts with committed non-view memory).
fn first_mapped(start: usize, end: usize) -> Option<usize> {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_MAPPED, MEMORY_BASIC_INFORMATION, VirtualQuery,
    };
    let mut at = start;
    while at < end {
        let mut info = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: queries our own address space without dereferencing `at`.
        let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
        if n == 0 || info.RegionSize == 0 {
            return None;
        }
        if info.State == MEM_COMMIT {
            return (info.Type == MEM_MAPPED).then_some(at);
        }
        let next = (info.BaseAddress as usize).saturating_add(info.RegionSize);
        if next <= at {
            return None;
        }
        at = next;
    }
    None
}

/// Whether the current allocation at `at` is a mapped view. Asking only for
/// the allocation type avoids enumerating pages with identical protection,
/// which can span an entire large view after its blocks were guarded.
fn mapped(at: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_MAPPED, MEMORY_BASIC_INFORMATION, MemoryRegionInfo, QueryVirtualMemoryInformation,
        VirtualQuery, WIN32_MEMORY_REGION_INFORMATION,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let mut region = WIN32_MEMORY_REGION_INFORMATION::default();
    // SAFETY: both queries describe an address without dereferencing it. The
    // output buffers match their information classes.
    let queried = unsafe {
        QueryVirtualMemoryInformation(
            GetCurrentProcess(),
            at as *const _,
            MemoryRegionInfo,
            (&raw mut region).cast(),
            size_of_val(&region),
            std::ptr::null_mut(),
        )
    };
    if queried != 0 {
        // These are the Windows SDK's named bitfields in declaration order.
        // windows-sys exposes only the union's raw bits.
        const PRIVATE: u32 = 1 << 0;
        const MAPPED_DATA_FILE: u32 = 1 << 1;
        const MAPPED_IMAGE: u32 = 1 << 2;
        const MAPPED_PAGE_FILE: u32 = 1 << 3;
        const MAPPED_PHYSICAL: u32 = 1 << 4;
        // SAFETY: this union was filled by the successful query above.
        match unsafe { region.Anonymous.Flags } {
            MAPPED_DATA_FILE | MAPPED_PAGE_FILE | MAPPED_PHYSICAL => return true,
            PRIVATE | MAPPED_IMAGE => return false,
            _ => {}
        }
    }
    // Unknown flag combinations and failed allocation queries keep the
    // VirtualQuery type check, including for a free address.
    let mut info = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: the output buffer has the required layout and size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    n != 0 && info.Type == MEM_MAPPED
}

/// Makes blocks `[from, to)` read-only and notes them guarded and clean;
/// block by block if they span views, recording unmapped blocks in `g.holes`
/// so subsequent checks neither retry `VirtualProtect` nor fault copying them.
fn protect(g: &mut Guards, from: usize, to: usize, written: &mut Vec<usize>) {
    use windows_sys::Win32::System::Memory::{PAGE_READONLY, VirtualProtect};
    touch(from, to);
    let set = |from: usize, to: usize| {
        let mut old = 0;
        // SAFETY: title view memory; only its protection changes.
        unsafe {
            VirtualProtect(
                (from * BLOCK) as *const _,
                (to - from) * BLOCK,
                PAGE_READONLY,
                &mut old,
            ) != 0
        }
    };
    let note = |g: &mut Guards, written: &mut Vec<usize>, from: usize, to: usize| {
        g.protected.extend(from..to);
        g.clean.extend(from..to);
        written.extend(from * BLOCK / PAGE..to * BLOCK / PAGE);
    };
    if set(from, to) {
        note(g, written, from, to);
    } else {
        for block in from..to {
            if set(block, block + 1) {
                note(g, written, block, block + 1);
            } else {
                g.holes.insert(block);
            }
        }
    }
    if !view_guard() {
        return;
    }
    // The same pages at their other views: read-only too, so that a write
    // through one of those faults. They are not noted clean: the copies
    // taken at those views have not taken the pages, and their next check
    // must report them (a block read-only and not clean is reported).
    // One look at the views for the whole run, and no list made for a page
    // with one view (most of them): this runs under the guards' lock, which
    // every copy's check waits for (6 % of the submitting thread's time when
    // it asked block by block, run p1).
    let views = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(views) = views.as_ref() else { return };
    for block in from..to {
        let shown = views.shown(block * BLOCK);
        if shown.len() < 2 || shown.len() > MANY_VIEWS || !g.protected.contains(&block) {
            continue;
        }
        for &at in shown {
            let alias = at / BLOCK;
            // Also one that cannot be made read-only: a write of ours there
            // must open the page's other views.
            touch(alias, alias + 1);
            g.aliased.insert(alias);
            if alias != block && !g.protected.contains(&alias) && !g.pinned(alias) {
                let mut old = 0;
                // SAFETY: title view memory; only its protection changes.
                if unsafe { VirtualProtect(at as *const _, BLOCK, PAGE_READONLY, &mut old) } != 0 {
                    g.protected.insert(alias);
                }
            }
        }
    }
}

/// Gives write access back to the guarded blocks of `[start, end)` and
/// notes them written; true if any was guarded. The same pages at their
/// other views are opened too: every guard over them must copy them again.
fn open(g: &mut Guards, start: usize, end: usize) -> bool {
    let any = open_here(g, start, end);
    if start >= end || g.aliased.is_empty() {
        return any;
    }
    let aliased: Vec<usize> = g.aliased.range(start / BLOCK..end.div_ceil(BLOCK)).copied().collect();
    for block in aliased {
        g.aliased.remove(&block);
        for at in aliases(block * BLOCK) {
            if at / BLOCK != block {
                open_here(g, at, at + BLOCK);
            }
            g.aliased.remove(&(at / BLOCK));
        }
    }
    any
}

/// [`open`] for one range, without the other views of its pages.
fn open_here(g: &mut Guards, start: usize, end: usize) -> bool {
    use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
    let blocks: Vec<usize> = g
        .protected
        .range(start / BLOCK..end.div_ceil(BLOCK))
        .copied()
        .collect();
    if blocks.is_empty() {
        return false;
    }
    for &block in &blocks {
        g.protected.remove(&block);
        g.clean.remove(&block);
    }
    // Runs of blocks next to each other in one call (fewer kernel
    // transitions and TLB shootdowns). One call cannot span two views: the
    // title's page mappings are one view a page, and a run over several
    // failed whole, leaving blocks read-only that were no longer noted
    // guarded (a kernel read into them then failed and the title stopped).
    // So a run that fails is given back block by block.
    let give = |from: usize, to: usize| {
        let mut old = 0;
        // SAFETY: blocks we made read-only; only their protection changes.
        let whole =
            unsafe { VirtualProtect((from * BLOCK) as *const _, (to - from) * BLOCK, PAGE_READWRITE, &mut old) } != 0;
        if !whole && to - from > 1 {
            for block in from..to {
                // SAFETY: as above, one block.
                unsafe { VirtualProtect((block * BLOCK) as *const _, BLOCK, PAGE_READWRITE, &mut old) };
            }
        }
    };
    let mut run_start = blocks[0];
    let mut run_end = run_start + 1;
    for &block in &blocks[1..] {
        if block == run_end {
            run_end = block + 1;
        } else {
            give(run_start, run_end);
            run_start = block;
            run_end = block + 1;
        }
    }
    give(run_start, run_end);
    bump_region_epochs(start, end);
    true
}

/// Before our own copy into title memory `[at, at + len)` on this thread:
/// opens its guarded blocks (and the same pages at their other views), so
/// that the copy does not fault block by block and the blocks count as
/// written. Nothing is pinned: the copy is over when its call returns, and a
/// block guarded again in between faults into our handler. A copy that goes
/// through the kernel after all is told of by [`writing`].
pub(crate) fn opening(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    if !opens_only() {
        return writing(at, len);
    }
    if !touched(at, len) {
        crate::telemetry::count(crate::telemetry::Count::OpeningsIdle, 1);
        return;
    }
    crate::telemetry::count(crate::telemetry::Count::Openings, 1);
    // `open` knows the other views: those of a page guarded together with
    // its views are all noted (`Guards::aliased`).
    open(&mut guards(), at, at + len);
}

/// Before a write into title memory `[at, at + len)` through the kernel
/// (which does not fault into our handler): opens its guarded blocks, and
/// leaves them so for a while. The runtime calls it (as `kernel_will_write`)
/// before the title's kernel reads into its own memory.
pub fn writing(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    let (first, last) = (at / BLOCK, (at + len).div_ceil(BLOCK));
    let mut g = guards();
    g.prune();
    let until = std::time::Instant::now() + PIN;
    g.pin(first, last, until);
    open(&mut g, at, at + len);
    if !view_guard() {
        return;
    }
    // The same pages at their other views: the write lands in them too, so
    // their copies must take them again, now and when it has landed.
    for block in first..last {
        let shown = aliases(block * BLOCK);
        if shown.len() < 2 || shown.len() > MANY_VIEWS {
            continue;
        }
        for alias in shown {
            if alias / BLOCK != block {
                g.pin(alias / BLOCK, alias / BLOCK + 1, until);
                open_here(&mut g, alias, alias + BLOCK);
            }
        }
    }
}

/// The runtime mapped or unmapped title memory `[at, at + len)`: what it
/// shows is new and read-write, so it is no longer guarded, and any placed
/// textures backed by that range are evicted.
pub fn title_memory_remapped(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    let end = at.saturating_add(len);
    {
        let mut g = guards();
        let blocks: Vec<usize> = g
            .protected
            .range(at / BLOCK..end.div_ceil(BLOCK))
            .copied()
            .collect();
        let holes: Vec<usize> = g
            .holes
            .range(at / BLOCK..end.div_ceil(BLOCK))
            .copied()
            .collect();
        let changed = !holes.is_empty() || !blocks.is_empty();
        for block in holes {
            g.holes.remove(&block);
        }
        for block in blocks {
            g.protected.remove(&block);
            g.clean.remove(&block);
            // A view mapped a moment ago may already have been made
            // read-only as another view of a guarded page (between the
            // runtime's `title_memory_view` and this call): it is given
            // back, or the title's next write there would fault with no
            // guard to answer for it.
            let mut old = 0;
            // SAFETY: only the protection of title view memory changes; the
            // call fails harmlessly on memory that is no longer a view.
            unsafe {
                windows_sys::Win32::System::Memory::VirtualProtect(
                    (block * BLOCK) as *const _,
                    BLOCK,
                    windows_sys::Win32::System::Memory::PAGE_READWRITE,
                    &mut old,
                )
            };
        }
        let aliased: Vec<usize> = g.aliased.range(at / BLOCK..end.div_ceil(BLOCK)).copied().collect();
        for block in aliased {
            g.aliased.remove(&block);
        }
        // What a kernel write was to land in is not there any more.
        let pinned: Vec<usize> =
            g.pins.range(at / BLOCK..end.div_ceil(BLOCK)).map(|(&block, _)| block).collect();
        for block in pinned {
            g.pins.remove(&block);
        }
        if changed {
            bump_region_epochs(at, end);
        }
    }
    crate::shadow::remapped(at as u64, end as u64);
    crate::resource::evict_placed_textures(at as u64, end as u64);
    crate::descriptor::host::bindless_memory_changed(at as u64, end as u64);
}

/// The title committed memory `[at, at + len)` it had reserved.
pub fn title_memory_committed(at: usize, len: usize) {
    if len != 0 {
        crate::shadow::filled(at as u64, (at + len) as u64);
    }
}

/// The runtime wrote new contents into title memory `[at, at + len)` (a
/// streamed file arrived): textures placed there are uploaded again at their
/// next use, not when their turn to be looked at comes.
pub fn title_memory_written(at: usize, len: usize) {
    if len != 0 {
        bump_region_epochs(at, at.saturating_add(len));
        // The runtime writes through a view of its own, which neither the
        // kernel's write watch nor our guards see: the pages are noted
        // written here, or host copies of buffers made before the file
        // arrived kept what the memory held then for good (meshes read as
        // zeros: walls, floors and bodies missing, others each launch).
        {
            let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
            j.asked += 1;
            let now = j.asked;
            for page in at / PAGE..(at + len).div_ceil(PAGE) {
                j.written.insert(page, now);
            }
        }
        crate::resource::textures_rewritten(at as u64, (at + len) as u64);
    }
}

/// Opens a guarded block at `at` (or returns `true` if another thread already
/// made it read-write). Called both by our own VEH handler and by
/// `host::safe_copy`'s VEH handler on write faults.
pub(crate) fn try_open_guarded(at: usize) -> bool {
    let mut g = guards();
    if open(&mut g, at, at + 1) {
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::GuardFaults, 1);
        return true;
    }
    if writable(at) {
        return true;
    }
    // Read-only, in a view we were told of, and not noted guarded: only we
    // make title views read-only, so it is ours to give back.
    if view_guard() && !aliases(at).is_empty() {
        let mut old = 0;
        let block = at - at % BLOCK;
        // SAFETY: only the protection of title view memory changes.
        let opened = unsafe {
            windows_sys::Win32::System::Memory::VirtualProtect(
                block as *const _,
                BLOCK,
                windows_sys::Win32::System::Memory::PAGE_READWRITE,
                &mut old,
            )
        } != 0;
        if opened {
            bump_region_epochs(block, block + BLOCK);
            tracing::warn!(at = format_args!("{at:#x}"), "a read-only view block that no guard knew was opened");
        }
        return opened;
    }
    false
}

/// Registers the handler that takes writes to guarded blocks, once, ahead
/// of the crash reporter's.
fn install() {
    use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, NTSTATUS};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
        EXCEPTION_POINTERS,
    };
    unsafe extern "system" fn on_write(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        let r = unsafe { &*(*info).ExceptionRecord };
        const WRITE: usize = 1;
        if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION as NTSTATUS
            || r.NumberParameters < 2
            || r.ExceptionInformation[0] != WRITE
        {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let at = r.ExceptionInformation[1];
        if try_open_guarded(at) {
            EXCEPTION_CONTINUE_EXECUTION
        } else {
            EXCEPTION_CONTINUE_SEARCH
        }
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: the handler only touches our guard state and protections.
    ONCE.call_once(|| unsafe {
        AddVectoredExceptionHandler(1, Some(on_write));
    });
}

/// Whether `at` is committed read-write memory now.
fn writable(at: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_READWRITE, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    n != 0 && info.State == MEM_COMMIT && info.Protect == PAGE_READWRITE
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };
    const MEM_WRITE_WATCH: u32 = 0x20_0000;

    /// Refreshes asked about ahead are answered from the journal while the
    /// asking stands: a write after it is seen by the first asking after it
    /// (nothing is lost, it comes a group later). What was not asked ahead is
    /// asked of the kernel as ever, and a range alone is not asked ahead.
    #[test]
    fn an_asking_ahead_answers_for_its_ranges_until_it_is_done() {
        if !ask_ahead_on() {
            return;
        }
        // SAFETY: a private write-watched region of the test's own.
        let p = unsafe {
            VirtualAlloc(std::ptr::null(), 16 * PAGE, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE)
        } as usize;
        assert_ne!(p, 0);
        let page = |i: usize| p / PAGE + i;
        // SAFETY: committed pages of ours.
        let write = |i: usize| unsafe { ((p + i * PAGE + 8) as *mut u8).write(7) };
        // Windows can reuse an address already in the journal: only what
        // was found after this counts.
        let (initial, _) = writes(p, p + 16 * PAGE, false).expect("watched");
        let found = |list: &[(usize, u64)]| list.iter().filter(|w| w.1 > initial).map(|w| w.0).collect::<Vec<_>>();
        write(1);
        write(9);
        let ahead = ask_ahead(&[(p + 8 * PAGE, p + 12 * PAGE), (p, p + 2 * PAGE)]).expect("asked ahead");
        let (now, first) = writes(p, p + 2 * PAGE, false).unwrap();
        assert_eq!(found(&first), [page(1)]);
        assert!(first.iter().all(|&(_, when)| when <= now));
        write(10);
        let (_, second) = writes(p + 8 * PAGE, p + 12 * PAGE, false).unwrap();
        assert_eq!(found(&second), [page(9)], "a write after the asking is the next one's");
        write(14);
        let (_, apart) = writes(p + 14 * PAGE, p + 15 * PAGE, false).unwrap();
        assert_eq!(found(&apart), [page(14)], "asked of the kernel");
        drop(ahead);
        let (later, third) = writes(p + 8 * PAGE, p + 12 * PAGE, false).unwrap();
        assert!(later > now);
        assert!(third.contains(&(page(10), later)), "{third:?}");
        assert!(ask_ahead(&[(p, p + PAGE)]).is_none());
        // SAFETY: our allocation.
        unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) };
    }

    /// Two views of the same two physical pages, told as the runtime tells
    /// them: the two addresses.
    fn two_views() -> (usize, usize) {
        use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile};
        // SAFETY: a pagefile section of two blocks, mapped read-write twice.
        unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let one = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK).Value as usize;
            let two = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK).Value as usize;
            assert!(one != 0 && two != 0);
            title_memory_view(one, 2 * BLOCK, section as usize, 0);
            title_memory_view(two, 2 * BLOCK, section as usize, 0);
            // As the runtime tells of a mapping. Another test's memory was
            // here, perhaps, pinned by a copy into it.
            title_memory_remapped(one, 2 * BLOCK);
            title_memory_remapped(two, 2 * BLOCK);
            (one, two)
        }
    }

    fn drop_views(views: &[usize]) {
        use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
        for &view in views {
            title_memory_remapped(view, 2 * BLOCK);
            title_memory_view_gone(view, 2 * BLOCK);
            // SAFETY: our view.
            unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: view as *mut _ }) };
        }
    }

    /// The title writes the pages of a guarded copy through another view of
    /// them: the guarded view's next check reports the page written.
    #[test]
    fn a_write_through_another_view_is_reported_at_the_guarded_one() {
        let (guarded, other) = two_views();
        let pages = |block: usize| (guarded + block * BLOCK) / PAGE;
        assert_eq!(guard(guarded, guarded + 2 * BLOCK), Some(vec![pages(0), pages(1)]));
        assert_eq!(guard(guarded, guarded + 2 * BLOCK), Some(Vec::new()), "nothing written yet");
        // SAFETY: the second view; the handler opens it on the fault.
        unsafe { ((other + BLOCK + 8) as *mut u8).write(1) };
        assert_eq!(guard(guarded, guarded + 2 * BLOCK), Some(vec![pages(1)]));
        assert_eq!(guard(guarded, guarded + 2 * BLOCK), Some(Vec::new()));
        drop_views(&[guarded, other]);
    }

    /// Views that follow one another in memory and in their section make a
    /// stretch; a gap in either ends it.
    #[test]
    fn a_stretch_is_views_that_follow_in_memory_and_in_the_section() {
        let base = 0x7a00_0000_0000usize;
        let (section, other) = (0x51, 0x52);
        for k in 0..3 {
            title_memory_view(base + k * BLOCK, BLOCK, section, (k * BLOCK) as u64);
        }
        // Next in memory, not in the section.
        title_memory_view(base + 3 * BLOCK, BLOCK, section, (9 * BLOCK) as u64);
        // Next in the section's offsets, of another section.
        title_memory_view(base + 4 * BLOCK, BLOCK, other, (10 * BLOCK) as u64);
        let reach = 1 << 30;
        assert_eq!(
            stretch(base + BLOCK + 8, base + BLOCK + 16, reach),
            Some((base, base + 3 * BLOCK, section, 0))
        );
        assert_eq!(
            stretch(base + 3 * BLOCK, base + 3 * BLOCK + 1, reach),
            Some((base + 3 * BLOCK, base + 4 * BLOCK, section, (9 * BLOCK) as u64))
        );
        // Across the gap: no stretch holds it.
        assert_eq!(stretch(base + 2 * BLOCK, base + 3 * BLOCK + 1, reach), None);
        // Outside every view.
        assert_eq!(stretch(base + 6 * BLOCK, base + 6 * BLOCK + 1, reach), None);
        title_memory_view_gone(base, 5 * BLOCK);
        assert_eq!(stretch(base, base + 1, reach), None);
    }

    /// Both views have copies: a write through either is reported at both.
    #[test]
    fn a_write_is_reported_at_every_checked_view() {
        let (one, two) = two_views();
        let pages = |view: usize, block: usize| (view + block * BLOCK) / PAGE;
        assert_eq!(guard(one, one + 2 * BLOCK), Some(vec![pages(one, 0), pages(one, 1)]));
        // The other view was made read-only with the first, not noted clean.
        assert_eq!(guard(two, two + 2 * BLOCK), Some(vec![pages(two, 0), pages(two, 1)]));
        assert_eq!(guard(one, one + 2 * BLOCK), Some(Vec::new()));
        assert_eq!(guard(two, two + 2 * BLOCK), Some(Vec::new()));
        // SAFETY: the first view; the handler opens it on the fault.
        unsafe { ((one + 16) as *mut u8).write(7) };
        assert_eq!(guard(two, two + 2 * BLOCK), Some(vec![pages(two, 0)]));
        assert_eq!(guard(one, one + 2 * BLOCK), Some(vec![pages(one, 0)]));
        // SAFETY: the same byte through the other view.
        assert_eq!(unsafe { ((two + 16) as *const u8).read() }, 7);
        drop_views(&[one, two]);
    }

    /// A view mapped beside a guarded one is writable: the guarded view
    /// checks its pages again.
    #[test]
    fn a_view_mapped_beside_a_guarded_one_makes_it_check_again() {
        use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile};
        // SAFETY: a pagefile section of two blocks, mapped read-write.
        let (section, guarded) = unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            (section, MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK).Value as usize)
        };
        title_memory_view(guarded, 2 * BLOCK, section as usize, 0);
        // As in `two_views`.
        title_memory_remapped(guarded, 2 * BLOCK);
        let pages = |block: usize| (guarded + block * BLOCK) / PAGE;
        assert_eq!(guard(guarded, guarded + 2 * BLOCK), Some(vec![pages(0), pages(1)]));
        assert_eq!(guard(guarded, guarded + 2 * BLOCK), Some(Vec::new()));
        // SAFETY: a second view of the second page only.
        let other = unsafe { MapViewOfFile(section, FILE_MAP_WRITE, 0, BLOCK as u32, BLOCK).Value as usize };
        assert_ne!(other, 0);
        title_memory_view(other, BLOCK, section as usize, BLOCK as u64);
        assert_eq!(guard(guarded, guarded + 2 * BLOCK), Some(vec![pages(1)]));
        title_memory_remapped(other, BLOCK);
        title_memory_view_gone(other, BLOCK);
        // SAFETY: our view.
        unsafe {
            windows_sys::Win32::System::Memory::UnmapViewOfFile(
                windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS { Value: other as *mut _ },
            )
        };
        drop_views(&[guarded]);
    }

    #[test]
    fn allocation_query_recognizes_file_views_and_rejects_nonviews() {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_READ, MapViewOfFile, PAGE_READONLY, UnmapViewOfFile,
        };
        let file = tempfile::tempfile().unwrap();
        file.set_len(PAGE as u64).unwrap();
        // SAFETY: a valid, open file and a read-only mapping of its contents.
        let section = unsafe {
            CreateFileMappingW(
                file.as_raw_handle(),
                std::ptr::null(),
                PAGE_READONLY,
                0,
                0,
                std::ptr::null(),
            )
        };
        assert!(!section.is_null());
        // SAFETY: an open section containing at least one page.
        let view = unsafe { MapViewOfFile(section, FILE_MAP_READ, 0, 0, PAGE) };
        assert!(!view.Value.is_null());
        assert!(mapped(view.Value as usize + 13));
        // SAFETY: our successful mapping and its section handle.
        unsafe {
            UnmapViewOfFile(view);
            CloseHandle(section);
        }
        assert!(!mapped((&raw const file) as usize)); // private stack allocation
        assert!(!mapped(mapped as *const () as usize)); // executable image
        assert!(!mapped(0)); // no allocation to query
    }

    #[test]
    fn written_pages_are_reported_to_every_asker() {
        // SAFETY: a fresh write-watched allocation, freed at the end.
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                4 * PAGE,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(p, 0);
        let (start, end) = (p, p + 4 * PAGE);
        let (initial, before) = writes(start, end, false).expect("watched");
        // Windows can reuse an address already present in the journal.
        assert!(before.iter().all(|&(_, when)| when < initial));
        // SAFETY: committed pages of ours.
        unsafe {
            ((p + PAGE + 8) as *mut u8).write(1);
            ((p + PAGE + KERNEL_PAGE + 8) as *mut u8).write(2);
            ((p + 2 * PAGE + 8) as *mut u8).write(3);
        }
        let now = copy_epoch(start, end, false).expect("watched");
        // Another asker after the kernel's bit was cleared still sees it.
        let (later, again) = writes(p + PAGE, p + 3 * PAGE, false).expect("watched");
        assert!(later > now);
        assert_eq!(again, [(p / PAGE + 1, now), (p / PAGE + 2, now)]);
        let mut plain = vec![0u8; PAGE];
        assert!(
            writes(
                plain.as_mut_ptr() as usize,
                plain.as_ptr() as usize + PAGE,
                true
            )
            .is_none()
        );
        // SAFETY: our allocation.
        unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) };
    }

    #[test]
    fn copy_epoch_preserves_guarded_writes_for_later_askers() {
        let (guarded, other) = two_views();
        let (start, end) = (guarded + BLOCK, guarded + 2 * BLOCK);
        copy_epoch(guarded, end, true).expect("guard the mapped views");
        // SAFETY: a write in the second view; the guard handler opens it.
        unsafe { ((other + BLOCK + 8) as *mut u8).write(1) };
        let now = copy_epoch(guarded, end, true).expect("record the alias write");
        let (later, pages) = writes_guarded(start, end).expect("read the overlapping history");
        assert!(later > now);
        assert_eq!(pages, [(start / PAGE, now)]);
        drop_views(&[guarded, other]);
    }

    /// A view of two blocks of a pagefile section.
    fn one_view() -> usize {
        use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile};
        // SAFETY: a pagefile section of two blocks, mapped read-write.
        let view = unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK).Value as usize
        };
        assert_ne!(view, 0);
        // Another test's view was here, perhaps.
        title_memory_remapped(view, 2 * BLOCK);
        view
    }

    fn read_only(at: usize) -> bool {
        !writable(at)
    }

    /// A block waiting for a kernel read is left writable, and reported
    /// written, by a check whose first block is guarded already; when the
    /// wait is over it is guarded like the others.
    #[test]
    fn a_check_of_guarded_memory_leaves_pinned_blocks_writable() {
        if !pin_always() {
            return;
        }
        let view = one_view();
        let page = |block: usize| (view + block * BLOCK) / PAGE;
        assert_eq!(guard(view, view + 2 * BLOCK), Some(vec![page(0), page(1)]));
        assert!(read_only(view) && read_only(view + BLOCK));
        writing(view + BLOCK + 100, 13);
        assert!(read_only(view) && !read_only(view + BLOCK));
        for _ in 0..3 {
            assert_eq!(guard(view, view + 2 * BLOCK), Some(vec![page(1)]));
            assert!(read_only(view) && !read_only(view + BLOCK), "the kernel may be writing it");
        }
        // The pin ends.
        guards().pins.insert(view / BLOCK + 1, std::time::Instant::now());
        assert_eq!(guard(view, view + 2 * BLOCK), Some(vec![page(1)]));
        assert!(read_only(view + BLOCK));
        assert_eq!(guard(view, view + 2 * BLOCK), Some(Vec::new()));
        drop_views(&[view]);
    }

    /// Before a copy of ours guarded blocks are opened, at the page's other
    /// views too, and count as written; nothing is pinned, and memory that
    /// was never guarded is left alone.
    #[test]
    fn opening_reports_and_does_not_pin() {
        if !opens_only() {
            return;
        }
        let view = one_view();
        let page = |block: usize| (view + block * BLOCK) / PAGE;
        opening(view + BLOCK, 8);
        assert_eq!(guard(view, view + 2 * BLOCK), Some(vec![page(0), page(1)]));
        assert!(touched(view, 2 * BLOCK));
        opening(view + BLOCK + 8, 16);
        assert!(read_only(view) && !read_only(view + BLOCK));
        assert!(!guards().pins.contains_key(&(view / BLOCK + 1)));
        assert_eq!(guard(view, view + 2 * BLOCK), Some(vec![page(1)]), "written");
        assert!(read_only(view + BLOCK), "and guarded again at once");
        assert_eq!(guard(view, view + 2 * BLOCK), Some(Vec::new()));
        drop_views(&[view]);

        let (one, two) = two_views();
        let page = |view: usize, block: usize| (view + block * BLOCK) / PAGE;
        guard(one, one + 2 * BLOCK).unwrap();
        guard(two, two + 2 * BLOCK).unwrap();
        opening(two + 8, 8);
        assert_eq!(guard(one, one + 2 * BLOCK), Some(vec![page(one, 0)]));
        assert_eq!(guard(two, two + 2 * BLOCK), Some(vec![page(two, 0)]));
        drop_views(&[one, two]);
    }

    /// Ranges are found in the coarse map at their ends too, and addresses
    /// past it count as touched.
    #[test]
    fn the_coarse_map_holds_every_block_made_read_only() {
        const MIB: usize = 1 << 20;
        // Nobody's addresses.
        let base = 0xe3_0000_0000usize;
        assert!(!touched(base, 8 * MIB));
        touch((base + 2 * MIB) / BLOCK, (base + 2 * MIB) / BLOCK + 1);
        assert!(!touched(base, 2 * MIB) && touched(base, 2 * MIB + 1));
        assert!(touched(base + 2 * MIB, 1) && touched(base + 4 * MIB - 1, 1));
        assert!(!touched(base + 4 * MIB, 4 * MIB));
        // Across two bits.
        touch((base + 16 * MIB) / BLOCK - 1, (base + 16 * MIB) / BLOCK + 1);
        assert!(touched(base + 14 * MIB, 1) && touched(base + 17 * MIB, 1));
        assert!(!touched(base + 18 * MIB, 1));
        let past = 1usize << 40;
        assert!(touched(past, 1) && touched(past - 1, 2) && touched(usize::MAX - 16, 8));
        touch(past / BLOCK, past / BLOCK + 1);
    }

    /// What a copy of ours pays before it writes memory no copy of the host
    /// guards: `cargo test --release -p victory-gpu -- --ignored --nocapture opening_against`.
    #[test]
    #[ignore = "benchmark"]
    fn opening_against_writing() {
        const N: u32 = 100_000;
        let mut memory = vec![0u8; 64 << 20];
        let base = memory.as_mut_ptr() as usize;
        let time = |f: &dyn Fn(usize)| {
            let started = std::time::Instant::now();
            for i in 0..N as usize {
                f(base + i * 4096 % (63 << 20));
            }
            started.elapsed() / N
        };
        let idle = time(&|at| opening(at, 256));
        let (view, other) = two_views();
        guard(view, view + 2 * BLOCK).unwrap();
        let guarded = time(&|at| opening(view + at % BLOCK, 256));
        let pinned = time(&|at| writing(at, 256));
        println!(
            "a copy of ours opens its memory in {idle:?}, in guarded memory {guarded:?}, with a pin {pinned:?}"
        );
        drop_views(&[view, other]);
        // The memory goes, and its pins with it.
        let blocks = base / BLOCK..(base + memory.len()).div_ceil(BLOCK);
        guards().pins.retain(|block, _| !blocks.contains(block));
    }

    /// A kernel read into a guarded view block fails; opened first, it
    /// lands and the block counts as written.
    #[test]
    fn kernel_reads_into_guarded_blocks_after_opening() {
        use std::io::{Read, Seek, Write};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, UnmapViewOfFile,
        };
        // SAFETY: a pagefile section of two blocks, mapped read-write.
        let view = unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK).Value as usize
        };
        assert_ne!(view, 0);
        // As in `two_views`.
        title_memory_remapped(view, 2 * BLOCK);
        assert!(guard(view, view + 2 * BLOCK).is_some()); // now read-only
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"from the disk").unwrap();
        // `ReadFile` inside: the kernel writes the view.
        let read = |file: &mut std::fs::File| {
            file.rewind().unwrap();
            // SAFETY: 13 bytes of our view, nothing else refers to them.
            let into = unsafe { std::slice::from_raw_parts_mut((view + 100) as *mut u8, 13) };
            file.read(into).is_ok_and(|n| n == 13)
        };
        assert!(!read(&mut file), "the kernel does not fault into our handler");
        writing(view + 100, 13);
        assert!(read(&mut file));
        // SAFETY: the bytes just read.
        assert_eq!(unsafe { std::slice::from_raw_parts((view + 100) as *const u8, 13) }, b"from the disk");
        assert_eq!(guard(view, view + 2 * BLOCK), Some((view / PAGE..view / PAGE + 1).collect()));
        // SAFETY: our view.
        unsafe { UnmapViewOfFile(windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS { Value: view as *mut _ }) };
        title_memory_remapped(view, 2 * BLOCK);
    }
}
