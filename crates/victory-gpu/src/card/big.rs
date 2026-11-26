//! Big buffers the title made in its private memory, as cards.
//!
//! A buffer is noted when the title makes it ([`registered`]) and asked for
//! when something is given its memory in place ([`note`]). It is made a card
//! between two submissions ([`settle`]), when the host GPU is idle and title
//! memory holds all it wrote, if there is room: of several the ones shaders
//! write first, as a write to system memory costs them the most.
//!
//! The memory has been used in place until then, and may be again (another
//! view reaches past the buffer, the video memory is full), so both ways in
//! must agree:
//!
//! - Views of it are made anew on either side: draws ask for their memory
//!   each time, and descriptor slots made over it are made again before the
//!   next lists run ([`super::moved_since`], `HostHeap::forget_moved`).
//! - A list recorded before goes on using the memory in place when it runs.
//!   What it reads there is what earlier submissions left (cards write back
//!   what shaders wrote before the next submission); what it declares to
//!   write there ([`concerns`]) is looked at before the next submission
//!   ([`super::wrote_in_place`]), as the kernel does not tell of it.
//! - A view that reaches past the buffer ([`super::find`]) ends its card: the
//!   buffer stays in place for good.
//!
//! shortcut: between lists of one submission that use the memory in the two
//! ways, only title memory as it was before the submission is shared; that
//! is the submission a buffer becomes a card in, or stops being one.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use super::*;

/// Not asked for yet.
const IDLE: u8 = 0;
/// Something was given its memory in place.
const ASKED: u8 = 1;
/// It is (in) a card.
const CARD: u8 = 2;
/// There was no room: asked again later.
const LATER: u8 = 3;
/// It stays in place.
const NEVER: u8 = 4;

/// A buffer there was no room for is looked at again this much later.
const RETRY: Duration = Duration::from_secs(10);
/// A card whose buffer the title released goes after nothing named it for
/// this long: the title makes buffers anew at the same place.
const ORPHAN: Duration = Duration::from_secs(10);
/// Cards are made from this much of the video memory's budget at most.
const BUDGET_PERCENT: u64 = 88;
/// Bytes of cards made before one submission (one at least): making one
/// takes the processor some milliseconds a megabyte.
const AT_ONCE: u64 = 32 << 20;

struct Big {
    from: u64,
    to: u64,
    /// The title's buffers with this range.
    refs: AtomicU32,
    state: AtomicU8,
    /// Shaders write it.
    wrote: AtomicBool,
}

/// By start.
static BIGS: RwLock<Vec<Arc<Big>>> = RwLock::new(Vec::new());
/// Of them, those still to be asked for, and those that wait for room.
static OPEN: AtomicUsize = AtomicUsize::new(0);
/// Something was asked for, or is to be looked at again, at this time (in
/// milliseconds since the first) at the earliest.
static DUE: AtomicU64 = AtomicU64::new(u64::MAX);

fn bigs() -> std::sync::RwLockReadGuard<'static, Vec<Arc<Big>>> {
    BIGS.read().unwrap_or_else(|e| e.into_inner())
}

fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Counts the buffers [`note`] has something to do for.
fn count_open(all: &[Arc<Big>]) {
    let open = all.iter().filter(|b| matches!(b.state.load(Ordering::Acquire), IDLE | ASKED | LATER)).count();
    OPEN.store(open, Ordering::Release);
}

/// The title made a buffer at `start`.
pub(crate) fn registered(start: u64, len: u64) {
    let (least, _) = limits();
    if !buffers_enabled() || STOPPED.load(Ordering::Acquire) || len < least || len >= 1 << 30 {
        return;
    }
    let end = start + len;
    let mut all = BIGS.write().unwrap_or_else(|e| e.into_inner());
    if let Some(b) = all.iter().find(|b| (b.from, b.to) == (start, end)) {
        b.refs.fetch_add(1, Ordering::AcqRel);
        return;
    }
    let at = all.partition_point(|b| b.from <= start);
    all.insert(
        at,
        Arc::new(Big {
            from: start,
            to: end,
            refs: AtomicU32::new(1),
            state: AtomicU8::new(IDLE),
            wrote: AtomicBool::new(false),
        }),
    );
    count_open(&all);
}

/// The title released a buffer.
pub(crate) fn unregistered(start: u64, len: u64) {
    let (least, _) = limits();
    if !buffers_enabled() || len < least || len >= 1 << 30 {
        return;
    }
    let end = start + len;
    let mut all = BIGS.write().unwrap_or_else(|e| e.into_inner());
    let Some(i) = all.iter().position(|b| (b.from, b.to) == (start, end)) else { return };
    let left = all[i].refs.fetch_sub(1, Ordering::AcqRel) - 1;
    // One that is a card, or was found not to be one, is remembered: the
    // title makes buffers anew at the same place.
    if left == 0 && matches!(all[i].state.load(Ordering::Acquire), IDLE | ASKED | LATER) {
        all.remove(i);
        count_open(&all);
    }
}

/// `[address, end)` is given in place; `writes`: shaders may write it.
pub(crate) fn note(address: u64, end: u64, writes: bool) {
    if OPEN.load(Ordering::Relaxed) == 0 {
        return;
    }
    for b in over(&bigs(), address, end) {
        // Inside it: what reaches past it has no use for its card.
        if b.from > address || b.to < end {
            continue;
        }
        if writes {
            b.wrote.store(true, Ordering::Relaxed);
        }
        if b.state.compare_exchange(IDLE, ASKED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            DUE.fetch_min(now_ms(), Ordering::AcqRel);
        }
    }
}

/// Whether what the host GPU writes in place at `[address, address + len)`
/// is to be told ([`super::wrote_in_place`]): a buffer there is a card, or
/// may become one before the list runs, or was one that lists recorded
/// before still use.
pub(crate) fn concerns(address: u64, len: u64) -> bool {
    let end = address.saturating_add(len);
    {
        let all = bigs();
        if !all.is_empty() && over(&all, address, end).any(|b| b.state.load(Ordering::Acquire) != NEVER) {
            return true;
        }
    }
    !buffers_over(address as usize, end as usize).is_empty()
}

/// The buffers that meet `[address, end)`.
fn over(all: &[Arc<Big>], address: u64, end: u64) -> impl Iterator<Item = &Arc<Big>> {
    let last = all.partition_point(|b| b.from < end);
    all[..last].iter().filter(move |b| b.to > address)
}

/// The card `shadow` is ended for good: buffers in it stay in place.
pub(super) fn never(shadow: &Shadow) {
    let Some((from, to)) = shadow.card().map(|c| c.span()) else { return };
    let all = bigs();
    for b in over(&all, from, to) {
        b.state.store(NEVER, Ordering::Release);
    }
    count_open(&all);
}

/// No more title buffers on cards ([`super::by_address`]): those that are
/// go back in place.
pub(super) fn stop() {
    STOPPED.store(true, Ordering::Release);
    {
        let all = bigs();
        for b in all.iter() {
            b.state.store(NEVER, Ordering::Release);
        }
        count_open(&all);
    }
    let mut owned = OWNED.lock().unwrap_or_else(|e| e.into_inner());
    let keys: Vec<u64> = owned
        .iter()
        .filter(|(_, s)| s.card().is_some_and(|c| c.is_buffer()))
        .map(|(k, _)| *k)
        .collect();
    for key in keys {
        if let Some(shadow) = owned.remove(&key) {
            tracing::info!(start = format_args!("{key:#x}"), "a title buffer off the card: memory by address");
            let_go(&owned, shadow);
        }
    }
}

/// Set by [`stop`].
static STOPPED: AtomicBool = AtomicBool::new(false);

/// Before a submission is prepared, with the host GPU idle: makes cards of
/// buffers that were asked for, as far as there is room, and lets go of
/// those nothing needs any more.
pub(crate) fn settle(host: &Host) {
    if !buffers_enabled() || STOPPED.load(Ordering::Acquire) {
        return;
    }
    let now = now_ms();
    orphans(now);
    if DUE.load(Ordering::Acquire) > now {
        return;
    }
    DUE.store(u64::MAX, Ordering::Release);
    let mut asked: Vec<Arc<Big>> = {
        let all = bigs();
        all.iter()
            .filter(|b| matches!(b.state.load(Ordering::Acquire), ASKED | LATER))
            .cloned()
            .collect()
    };
    // Those shaders write first, of those the largest: one inside another
    // is then in its card.
    asked.sort_by_key(|b| (!b.wrote.load(Ordering::Relaxed), std::cmp::Reverse(b.to - b.from)));
    let mut made = 0;
    let mut left = false;
    for b in &asked {
        if made >= AT_ONCE {
            left = true;
            break;
        }
        match make(host, b) {
            Outcome::Made => made += b.to - b.from,
            Outcome::Inside => b.state.store(CARD, Ordering::Release),
            Outcome::Never => b.state.store(NEVER, Ordering::Release),
            Outcome::NoRoom => {
                b.state.store(LATER, Ordering::Release);
                DUE.fetch_min(now + RETRY.as_millis() as u64, Ordering::AcqRel);
            }
        }
    }
    if left {
        DUE.fetch_min(now, Ordering::AcqRel);
    }
    count_open(&bigs());
}

enum Outcome {
    Made,
    /// A card has its memory already.
    Inside,
    Never,
    NoRoom,
}

fn make(host: &Host, b: &Arc<Big>) -> Outcome {
    let (from, to, len) = (b.from, b.to, b.to - b.from);
    let mut owned = OWNED.lock().unwrap_or_else(|e| e.into_inner());
    match held(from, to) {
        Held::Not => {}
        Held::By(_) => return Outcome::Inside,
        // What uses the one card would end the other's.
        Held::Across(_) => return Outcome::Never,
    }
    if !host.card_ready() {
        return Outcome::Never;
    }
    if !private(from as usize, to as usize) {
        return Outcome::Never;
    }
    let mine: u64 = owned.values().filter_map(|s| s.card()).map(|c| c.size as u64).sum();
    let (_, most) = limits();
    let room = crate::host::video_memory()
        .is_none_or(|[(used, budget), _]| budget == 0 || used + len <= budget * BUDGET_PERCENT / 100);
    if mine + len > most || !room {
        static COUNT: AtomicU64 = AtomicU64::new(0);
        let count = COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if count <= 16 || count.is_power_of_two() {
            let used = crate::host::video_memory().map(|[(used, budget), _]| (used >> 20, budget >> 20));
            tracing::info!(
                count,
                start = format_args!("{from:#x}"),
                len,
                cards_mb = mine >> 20,
                video_memory_mb = ?used,
                "no room on the card for a title buffer"
            );
        }
        return Outcome::NoRoom;
    }
    let started = Instant::now();
    let base = from as usize & !(PIECE - 1);
    let pieces = (to as usize).next_multiple_of(PIECE) / PIECE - base / PIECE;
    let Some(watch) = written::Written::new(base, pieces) else {
        return Outcome::Never;
    };
    let place = Place { back: base, own_view: false, told: Told::Written(Mutex::new(watch)) };
    let Some(card) = Card::new(host, from as usize, to as usize, place) else {
        return Outcome::NoRoom;
    };
    let shadow = Shadow::on_card(card);
    owned.insert(from, shadow);
    publish(&owned);
    b.state.store(CARD, Ordering::Release);
    // Smaller ones inside it.
    for other in over(&bigs(), from, to) {
        if other.from >= from && other.to <= to {
            other.state.store(CARD, Ordering::Release);
        }
    }
    moved((from, to));
    tracing::info!(
        start = format_args!("{from:#x}"),
        len,
        written = b.wrote.load(Ordering::Relaxed),
        cards_mb = (mine + len) >> 20,
        ms = started.elapsed().as_millis() as u64,
        "a title buffer on the card"
    );
    Outcome::Made
}

/// Whether `[from, to)` is read-write private memory of one allocation.
fn private(from: usize, to: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_PRIVATE, MEMORY_BASIC_INFORMATION, PAGE_READWRITE, VirtualQuery,
    };
    let (mut at, mut allocation) = (from, None);
    while at < to {
        let mut info = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: a query of our own address space.
        if unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) } == 0
            || info.State != MEM_COMMIT
            || info.Type != MEM_PRIVATE
            || info.Protect != PAGE_READWRITE
            || *allocation.get_or_insert(info.AllocationBase as usize) != info.AllocationBase as usize
        {
            return false;
        }
        at = info.BaseAddress as usize + info.RegionSize;
    }
    true
}

/// Lets go of cards whose buffers the title released and nothing names any
/// more, and of those whose writes cannot be told.
fn orphans(now: u64) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    static IDLE_SINCE: Mutex<BTreeMap<u64, Instant>> = Mutex::new(BTreeMap::new());
    const EVERY: u64 = 1000;
    if NEXT.load(Ordering::Relaxed) > now {
        return;
    }
    NEXT.store(now + EVERY, Ordering::Relaxed);
    let mut idle = IDLE_SINCE.lock().unwrap_or_else(|e| e.into_inner());
    let mut owned = OWNED.lock().unwrap_or_else(|e| e.into_inner());
    let mut gone: Vec<u64> = Vec::new();
    {
        let all = bigs();
        for (&key, shadow) in owned.iter() {
            let Some(card) = shadow.card().filter(|c| c.is_buffer()) else { continue };
            if card.untold.load(Ordering::Acquire) {
                tracing::warn!(start = format_args!("{key:#x}"), "no one tells what the title writes to a buffer on the card");
                gone.push(key);
                continue;
            }
            let (from, to) = card.span();
            let held = over(&all, from, to)
                .any(|b| b.from >= from && b.to <= to && b.refs.load(Ordering::Acquire) > 0);
            // Only the table's reference: no list, no slot.
            if held || Arc::strong_count(shadow) > 1 {
                idle.remove(&key);
                continue;
            }
            if idle.entry(key).or_insert_with(Instant::now).elapsed() >= ORPHAN {
                gone.push(key);
            }
        }
    }
    for key in gone {
        idle.remove(&key);
        let Some(shadow) = owned.remove(&key) else { continue };
        let untold = shadow.card().is_some_and(|c| c.untold.load(Ordering::Acquire));
        if let Some((from, to)) = shadow.card().map(|c| c.span()) {
            let mut all = BIGS.write().unwrap_or_else(|e| e.into_inner());
            if untold {
                for b in over(&all, from, to) {
                    b.state.store(NEVER, Ordering::Release);
                }
            } else {
                all.retain(|b| !(b.from >= from && b.to <= to && b.refs.load(Ordering::Acquire) == 0));
            }
            count_open(&all);
        }
        tracing::info!(start = format_args!("{key:#x}"), "a title buffer off the card");
        let_go(&owned, shadow);
    }
}
