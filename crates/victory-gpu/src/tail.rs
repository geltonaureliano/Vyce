//! The tail drain: a title submission is prepared while the one before it
//! is still in the air, and waits for it to land only just before its own
//! lists go to the host queue (stage B of the flights, step 1).
//!
//! Without it every title submission waits for everything in flight before
//! it is prepared (`crate::host::Host::execute_title`): its refreshes, its
//! title copies, its snapshots and its uploads then see what the flights'
//! landings wrote, and touch nothing they still use. The host GPU stands
//! idle for the whole preparation, some 40 ms in 23 submissions a world
//! frame on the RTX 2070 SUPER (131 ms of idle a frame, log 2026-11-27).
//!
//! With it, at most one title flight is in the air while a submission is
//! prepared (the one before; flights of ours with no work of the CPU may be
//! behind it), and that flight lands before this submission's lists are
//! given to the host queue. Each step of the preparation is one of three
//! kinds:
//!
//! - done at once, as before: what neither reads what a landing in the air
//!   writes nor writes what the flight still uses (refreshes of memory no
//!   landing writes, ranges given to shaders, bindless slots the title wrote
//!   since the plan before);
//! - left for after the wait, in the order the steps came (late): the
//!   refreshes of memory a landing in the air writes, that is used in place
//!   by the host GPU, or that the flight's shaders write through directly
//!   indexed heaps (those writes stay in the copy); and every step that
//!   reads title memory into memory of ours or writes what a flight may
//!   still read (fills, the runtime's own work, textures and their uploads,
//!   preludes, bounces of constant buffers, snapshots of copies that are not
//!   sparse);
//! - a wait where it stands (a flush point): title copies and work that may
//!   write title memory, a declaration of shader writes on a copy that is
//!   not sparse which a flight in the air writes too (the copy has one
//!   snapshot), and slots of a directly indexed heap the title did not write
//!   since the plan before (those that plan left unmade over its time
//!   budget, and those the plan's look finds): the flight may read them, and
//!   a descriptor is not to change while the host GPU may read it. From
//!   there on the preparation is the old one.
//!
//! What a landing writes is known by address, and the title shows one
//! physical page at several addresses (views of xmem's sections): a refresh
//! of the same bytes at another view, of a page whose views are guarded
//! together, waits for the landing too ([`crate::watch::other_views`]), as
//! does one of another view of memory used in place. The old drain saw those
//! writes through the guards: our writes into title memory open every view
//! of the page ([`crate::watch::opening`]), and the refresh came after them.
//!
//! A refresh left for later goes around the ranges the submission had given
//! its shaders by its own step, and no others: the old drain had landed the
//! flight before it, so those were all a refresh found closed then. Ranges
//! shaders write carry the fence value of their submission, so a landing
//! opens only its own ([`crate::shadow::Shadow::copy_back`]). The prewarm
//! (the same refreshes done early while the flight before runs) is left
//! out: the preparation itself runs under the flight now
//! (`VYCE_TAIL_PREWARM=1` keeps it). A submission with a list still in the
//! air, or with shaders writing buffers on the card, takes the old drain.
//!
//! shortcut: shader writes nobody declares (through pointers; through
//! unordered access slots over their plan's cap or made since it was built
//! whole) are not waited for: where the title writes the same page, the next
//! submission's refresh may now come before them (they stay in the copy)
//! instead of always after; `VYCE_TAIL_DRAIN_STRICT=1` waits for every
//! refresh.
//!
//! The design is step 1 of the judge's stage B plan in the phase 2 design
//! (lab/fase2-desenho.json, "judge:stage-b"), whose three designs list the
//! hazards; written for this tree, no fork's code.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crate::flight::{Line, Marks};
use crate::telemetry::{Count, count};

/// What a title submission does about the flights in the air before it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    /// It waits for them before it is prepared (`VYCE_TAIL_DRAIN=0`).
    Old,
    /// It is prepared under them and waits at its tail.
    Tail,
    /// As `Tail`, with every refresh left for after the wait
    /// (`VYCE_TAIL_DRAIN_STRICT=1`): a picture that this mends had a
    /// landing's write or a range used in place missed.
    Strict,
}

thread_local! {
    /// Tests': the mode of the submissions this thread prepares.
    #[cfg(test)]
    static FORCED: std::cell::Cell<Option<Mode>> = const { std::cell::Cell::new(None) };
    /// Tests': whether their dry run is on.
    #[cfg(test)]
    static DRY: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    /// Tests': whether those prepared under a flight are prewarmed.
    #[cfg(test)]
    static PREWARM: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// The mode the switches ask for (`VYCE_TAIL_DRAIN`, on unless 0;
/// `VYCE_TAIL_DRAIN_STRICT`). The host turns it off where it cannot apply
/// (`crate::host`'s `tail_drain`).
pub(crate) fn mode() -> Mode {
    #[cfg(test)]
    if let Some(mode) = FORCED.get() {
        return mode;
    }
    static MODE: OnceLock<Mode> = OnceLock::new();
    *MODE.get_or_init(|| {
        let on = std::env::var_os("VYCE_TAIL_DRAIN").is_none_or(|v| v != "0");
        let strict = std::env::var_os("VYCE_TAIL_DRAIN_STRICT").is_some_and(|v| v == "1");
        match (on, strict) {
            (false, _) => Mode::Old,
            (true, false) => Mode::Tail,
            (true, true) => Mode::Strict,
        }
    })
}

/// Tests': the submissions this thread prepares take `mode` (`None`: the
/// switches').
#[cfg(test)]
pub(crate) fn force(mode: Option<Mode>) {
    FORCED.set(mode);
}

/// Whether submissions that wait before they are prepared count what the
/// tail drain (not the strict one) would have done instead (the `b_dry_*`
/// fields of `VYCE_TELEMETRY`; `VYCE_TAIL_DRAIN_DRY=0`: not). Only with
/// measurements on.
pub(crate) fn dry() -> bool {
    #[cfg(test)]
    if let Some(on) = DRY.get() {
        return on;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    crate::telemetry::on()
        && *ON.get_or_init(|| std::env::var_os("VYCE_TAIL_DRAIN_DRY").is_none_or(|v| v != "0"))
}

/// Tests': the dry run of the submissions this thread prepares is `on`
/// (`None`: as the switches say).
#[cfg(test)]
pub(crate) fn force_dry(on: Option<bool>) {
    DRY.set(on);
}

/// Whether a title submission prepared under a flight is prewarmed first,
/// as with the old drain (`VYCE_TAIL_PREWARM=1`; by default not: the
/// prewarm brings the copies its steps ask for up to date while the flight
/// before runs, which the preparation under the flight does itself, so it
/// was the same refreshes twice, some 6 ms a world frame on the title's
/// submitting thread).
pub(crate) fn prewarm() -> bool {
    #[cfg(test)]
    if let Some(on) = PREWARM.get() {
        return on;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VYCE_TAIL_PREWARM").is_some_and(|v| v == "1"))
}

/// Tests': the submissions this thread prepares under a flight are
/// prewarmed (`None`: as the switch says).
#[cfg(test)]
pub(crate) fn force_prewarm(on: Option<bool>) {
    PREWARM.set(on);
}

/// Why a preparation waits for the flights before its tail.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Why {
    /// A title buffer copy (`CopyBufferRegion`, `CopyResource`).
    Copy,
    /// Work that may write title memory (`CopyMemoryToMemoryX`).
    Run,
    /// Shaders write a copy that is not sparse, which a flight in the air
    /// writes too: its one snapshot would be theirs both.
    Dense,
    /// Slots of a directly indexed heap the title did not write since the
    /// plan before, which the flight in the air may read
    /// (`crate::descriptor::host::HostHeap::bindless_plan_with`).
    Slots,
    /// Its lists go to the host queue.
    Tail,
}

/// Why a title submission takes the old drain with the tail drain on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Old {
    /// One of its lists is still in the air: what the list holds (its
    /// prelude, its fills' memory) is to be made anew.
    List,
    /// Its shaders write buffers on the card, which waits for them.
    Card,
}

/// Counts an old drain taken for `why`, in the real counts or in the dry
/// run's.
pub(crate) fn old(why: Old, dry: bool) {
    #[cfg(test)]
    if !dry {
        LOG.with_borrow_mut(|log| log.push(Event::Old(why)));
    }
    let what = match (why, dry) {
        (Old::List, false) => Count::TailOldList,
        (Old::Card, false) => Count::TailOldCard,
        (Old::List, true) => Count::DryOldList,
        (Old::Card, true) => Count::DryOldCard,
    };
    count(what, 1);
}

/// What the flights in the air when a preparation began will still write
/// in title memory.
pub(crate) struct Air {
    marks: Vec<Arc<Marks>>,
    line: Arc<Line>,
    /// The dry run's: they count as in the air although the old drain
    /// landed them before the preparation.
    dry: bool,
}

impl Air {
    pub(crate) fn new(marks: Vec<Arc<Marks>>, line: Arc<Line>, dry: bool) -> Air {
        Air { marks, line, dry }
    }

    /// The flights not landed yet.
    fn up(&self) -> impl Iterator<Item = &Arc<Marks>> {
        self.marks.iter().filter(|m| self.dry || !self.line.landed_up_to(m.fence))
    }

    /// Whether a landing still to come writes title memory `[from, to)`.
    pub(crate) fn meets(&self, from: usize, to: usize) -> bool {
        from < to && self.up().any(|m| m.meets(from, to))
    }

    /// [`Air::meets`] of `[from, to)` or of the same bytes at another view.
    pub(crate) fn meets_here_or_shown(&self, from: usize, to: usize) -> bool {
        self.meets(from, to) || shown(from, to, |a, b| self.meets(a, b))
    }

    /// Whether a flight still to land has shaders writing the copy at
    /// `copy` (one that is not sparse).
    pub(crate) fn holds(&self, copy: usize) -> bool {
        self.up().any(|m| m.holds(copy))
    }

    /// How many flights' landings write memory the host GPU uses in place.
    pub(crate) fn lands_in_place(&self) -> usize {
        self.up().filter(|m| m.lands_in_place()).count()
    }
}

/// Whether a refresh of `[from, to)` waits for the tail: a landing in the
/// air writes there, or the host GPU uses it in place (its writes are known
/// nowhere), there or at another view of its pages; or `strict`.
pub(crate) fn late(air: &Air, strict: bool, from: usize, to: usize) -> bool {
    if strict {
        return true;
    }
    let meets = |a: usize, b: usize| air.meets(a, b) || crate::shadow::meets_in_place(a, b);
    if meets(from, to) {
        return true;
    }
    if !shown(from, to, meets) {
        return false;
    }
    count(if air.dry { Count::DryLateViews } else { Count::TailLateViews }, 1);
    true
}

/// Whether `f` holds for the same bytes as `[from, to)` at another address
/// their pages are shown at, where those views are guarded together
/// ([`crate::watch::other_views`]).
pub(crate) fn shown(from: usize, to: usize, f: impl Fn(usize, usize) -> bool) -> bool {
    #[cfg(test)]
    if mutated(Mutation::Views) {
        return false;
    }
    crate::watch::other_views(from, to).into_iter().any(|(a, b)| f(a, b))
}

/// A refresh left for after the wait.
pub(crate) struct Late {
    pub(crate) shadow: Arc<crate::shadow::Shadow>,
    pub(crate) address: usize,
    pub(crate) len: usize,
    /// Tiles not looked at in their turn are skipped (plan reads).
    pub(crate) lazy: bool,
    /// Of a directly indexed heap's plan (counted as such).
    pub(crate) plan: bool,
    /// The ranges the submission had given its shaders by the refresh's
    /// own step: what the refresh then went around.
    pub(crate) around: Vec<(usize, usize)>,
}

/// The tail drain's state of a preparation under flights.
pub(crate) struct Deferring {
    pub(crate) air: Air,
    pub(crate) strict: bool,
    /// The preparation's fence value: its own ranges carry it.
    pub(crate) fence: u64,
    /// Refreshes left for after the wait, in order.
    pub(crate) late: Vec<Late>,
    /// The flights have landed (a flush point came): the rest is the old
    /// preparation.
    pub(crate) drained: bool,
    /// When the preparation began, and when it first waited.
    began: Instant,
    pub(crate) waited_at: Option<Instant>,
    /// Time waited for the flights in all, and in the wait at the tail.
    pub(crate) waited: Duration,
}

impl Deferring {
    pub(crate) fn new(air: Air, strict: bool, fence: u64, began: Instant) -> Deferring {
        count(Count::TailPrepared, 1);
        #[cfg(test)]
        LOG.with_borrow_mut(|log| log.push(Event::Under));
        Deferring {
            air,
            strict,
            fence,
            late: Vec::new(),
            drained: false,
            began,
            waited_at: None,
            waited: Duration::ZERO,
        }
    }

    /// Whether a refresh of `[address, address + len)` is left for later.
    pub(crate) fn is_late(&self, address: usize, len: usize) -> bool {
        #[cfg(test)]
        if mutated(Mutation::Late) {
            return false;
        }
        !self.drained && late(&self.air, self.strict, address, address.saturating_add(len))
    }

    /// Leaves a refresh for after the wait.
    pub(crate) fn defer(
        &mut self,
        shadow: &Arc<crate::shadow::Shadow>,
        address: usize,
        len: usize,
        lazy: bool,
        plan: bool,
    ) {
        count(Count::TailLate, 1);
        count(Count::TailLateBytes, len as u64);
        #[cfg(test)]
        LOG.with_borrow_mut(|log| log.push(Event::Late));
        let around = shadow.own_pending(address, len, self.fence);
        self.late.push(Late { shadow: shadow.clone(), address, len, lazy, plan, around });
    }

    /// The wait for the flights in the air, at a flush point or the tail:
    /// how long it took.
    pub(crate) fn wait(&mut self, why: Why) -> Duration {
        let now = Instant::now();
        if self.waited_at.is_none() {
            self.waited_at = Some(now);
            count(Count::TailOverlap, (now - self.began).as_micros() as u64);
        }
        let flush = match why {
            Why::Copy => Some(Count::TailFlushCopy),
            Why::Run => Some(Count::TailFlushRun),
            Why::Dense => Some(Count::TailFlushDense),
            Why::Slots => Some(Count::TailFlushSlots),
            Why::Tail => None,
        };
        if let Some(flush) = flush {
            count(flush, 1);
        }
        #[cfg(test)]
        LOG.with_borrow_mut(|log| log.push(Event::Waited(why)));
        self.air.line.about_to_wait();
        let what = if why == Why::Tail { Count::WaitTail } else { Count::WaitFlush };
        self.air.line.drain(what);
        self.drained = true;
        let took = now.elapsed();
        self.waited += took;
        took
    }

    /// At the end of the preparation: the time after its first wait, which
    /// the host GPU stood idle for.
    pub(crate) fn finish(&self) {
        if let Some(at) = self.waited_at {
            let after = at.elapsed().saturating_sub(self.waited);
            count(Count::TailAfterFlush, after.as_micros() as u64);
        }
    }
}

/// The dry run of a preparation that waited before it began: what the tail
/// drain would have left for later and where it would have waited.
pub(crate) struct Dry {
    pub(crate) air: Air,
    began: Instant,
    /// The old drain's wait.
    pub(crate) waited: Duration,
    /// Where the tail drain would first have waited, and why.
    flushed: Option<Duration>,
    /// Time of steps it would have left for later, before that.
    queued: Duration,
    late: Duration,
}

impl Dry {
    pub(crate) fn new(air: Air) -> Dry {
        count(Count::DryPrepared, 1);
        count(Count::DryLandsInPlace, air.lands_in_place() as u64);
        #[cfg(test)]
        LOG.with_borrow_mut(|log| log.push(Event::DryUnder));
        Dry {
            air,
            began: Instant::now(),
            waited: Duration::ZERO,
            flushed: None,
            queued: Duration::ZERO,
            late: Duration::ZERO,
        }
    }

    /// The preparation proper begins (after the old drain).
    pub(crate) fn begin(&mut self, waited: Duration) {
        self.waited = waited;
        self.began = Instant::now();
    }

    /// Whether the tail drain would still be under the flights.
    pub(crate) fn under(&self) -> bool {
        self.flushed.is_none()
    }

    /// The refreshes of `work` (title ranges), done in `took`: those the tail
    /// drain would have left for later.
    /// shortcut: their time is the group's, in the share of their bytes
    /// (the group is done as before, all together).
    pub(crate) fn refreshes(&mut self, work: impl Iterator<Item = (usize, usize)>, took: Duration) {
        if !self.under() {
            return;
        }
        let (mut all, mut waiting) = (0u64, 0u64);
        for (address, len) in work {
            all += len as u64;
            if late(&self.air, false, address, address.saturating_add(len)) {
                waiting += len as u64;
                count(Count::DryLate, 1);
                #[cfg(test)]
                LOG.with_borrow_mut(|log| log.push(Event::DryLate));
            }
        }
        count(Count::DryLateBytes, waiting);
        if all > 0 && waiting > 0 {
            self.late += took.mul_f64(waiting as f64 / all as f64);
        }
    }

    /// A step the tail drain would have left for later, done in `took`.
    pub(crate) fn queued(&mut self, took: Duration) {
        if self.under() {
            count(Count::DryQueued, 1);
            count(Count::DryQueuedTime, took.as_micros() as u64);
            self.queued += took;
        }
    }

    /// A step the tail drain would have waited at.
    pub(crate) fn flush(&mut self, why: Why) {
        if !self.under() {
            return;
        }
        self.flushed = Some(self.began.elapsed());
        #[cfg(test)]
        LOG.with_borrow_mut(|log| log.push(Event::DryFlush(why)));
        let what = match why {
            Why::Copy => Count::DryFlushCopy,
            Why::Run => Count::DryFlushRun,
            Why::Dense => Count::DryFlushDense,
            Why::Slots => Count::DryFlushSlots,
            Why::Tail => return,
        };
        count(what, 1);
    }

    /// Shaders of the submission write `[address, address + len)`: whether a
    /// flight in the air writes there too, or at another view of the same
    /// pages (step 3 of the plan waits there).
    pub(crate) fn declared(&self, address: usize, len: usize) {
        if self.under() && self.air.meets_here_or_shown(address, address.saturating_add(len)) {
            count(Count::DryDeclaredOver, 1);
        }
    }

    /// At the preparation's tail: what would have overlapped the host GPU.
    /// shortcut: what it saves is the least of that and the old drain's
    /// wait (with the prewarm): the host GPU's work left when the
    /// preparation began is not known, nor how long the landing takes.
    pub(crate) fn finish(&self) {
        let all = self.began.elapsed();
        let before = self.flushed.unwrap_or(all);
        let overlap = before.saturating_sub(self.queued + self.late);
        count(Count::DryWait, self.waited.as_micros() as u64);
        count(Count::DryOverlap, overlap.as_micros() as u64);
        count(Count::DrySaved, overlap.min(self.waited).as_micros() as u64);
        if self.flushed.is_some() {
            count(Count::DryAfterFlush, (all - before).as_micros() as u64);
        }
    }
}

/// What a preparation did about the flights before it, as tests see it.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Event {
    /// Prepared under flights.
    Under,
    /// It took the old drain for this.
    Old(Old),
    /// It left a refresh for after the wait.
    Late,
    /// It waited, for this.
    Waited(Why),
    /// The old drain's dry run: the tail drain would have prepared it under
    /// flights, left a refresh for after the wait, first waited for this.
    DryUnder,
    DryLate,
    DryFlush(Why),
}

#[cfg(test)]
thread_local! {
    /// What this thread's preparations did, in order.
    static LOG: std::cell::RefCell<Vec<Event>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Tests': what this thread's preparations did since the last call.
#[cfg(test)]
pub(crate) fn events() -> Vec<Event> {
    LOG.with_borrow_mut(std::mem::take)
}

/// A rule of the tail drain turned off, for tests that must see it fail.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mutation {
    /// Refreshes a landing in the air writes are done at once.
    Late,
    /// Steps that read title memory into memory of ours are done at once.
    Queue,
    /// Title copies and work that may write title memory do not wait.
    Copies,
    /// Declarations on a copy that is not sparse which a flight writes too
    /// do not wait.
    Dense,
    /// Ranges shaders write all carry the same fence value, and a landing
    /// opens those of others too (removal by value, as before the tags).
    Tags,
    /// What shaders write unlisted (through directly indexed heaps) is not
    /// waited for.
    Unlisted,
    /// A refresh of the same bytes at another view of their pages is done
    /// at once (the rule looks at its own addresses only).
    Views,
    /// Slots the title did not write since the plan before are made under
    /// the flight.
    Slots,
    /// The title memory of textures a landing copies back
    /// (`crate::texback`) is left out of the flight's marks.
    Textures,
}

#[cfg(test)]
thread_local! {
    static MUTATED: std::cell::Cell<Option<Mutation>> = const { std::cell::Cell::new(None) };
}

/// Tests': turns `rule` off for the submissions this thread prepares.
#[cfg(test)]
pub(crate) fn mutate(rule: Option<Mutation>) {
    MUTATED.set(rule);
}

/// Whether `rule` is off on this thread.
#[cfg(test)]
pub(crate) fn mutated(rule: Mutation) -> bool {
    MUTATED.get() == Some(rule)
}

/// Tests': a landing keeps every tile snapshot of a sparse copy while
/// anything is pending in it (as before the snapshots went back per flight).
#[cfg(test)]
pub(crate) static KEEP_TILES: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The tag of a submission's ranges: its fence value (0 for all of them
/// with [`Mutation::Tags`], in tests).
pub(crate) fn tag(fence: u64) -> u64 {
    #[cfg(test)]
    if mutated(Mutation::Tags) {
        return 0;
    }
    fence
}

#[cfg(test)]
mod tests;
