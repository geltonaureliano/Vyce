//! What a frame's submissions cost, for the session panel
//! (`VYCE_TELEMETRY=1`): how many ran and how many were waited for, the
//! parts of their preparation, who waited for the host GPU, and how long it
//! worked on them and stood idle between them (its own timestamps, not our
//! wait for it). Off, nothing is timed here.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

#[cfg(windows)]
use windows::Win32::Graphics::Direct3D12::*;

#[cfg(windows)]
use crate::host::Host;

/// Whether the session panel asked for measurements.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VYCE_TELEMETRY").is_some_and(|v| v == "1"))
}

/// What is counted between two samples. Times are in microseconds.
#[derive(Clone, Copy)]
pub(crate) enum Count {
    /// Submissions, the title's and ours.
    Submissions,
    /// Those of the title that returned before the host GPU was done.
    Unwaited,
    /// Those of the title waited for only because shaders write buffers on
    /// the card.
    HeldByCard,
    /// Keeping the bytes of ranges shaders will write.
    Snapshots,
    /// Work of the runtime's own the lists left for their submission.
    Steps,
    /// Recording the uploads and compares of buffers on the card.
    Card,
    /// Giving the lists to the host queue.
    Submit,
    /// The host GPU from a submission's start to its end.
    Gpu,
    /// The submissions timed so.
    GpuTimed,
    /// The host GPU from a submission's end to the next one's start: it had
    /// nothing to do.
    GpuIdle,
    /// Waits for the host GPU (and for what its work left to do), by what
    /// waited: a submission before it is prepared, ...
    WaitExecute,
    /// ... a queue's signal, ...
    WaitSignal,
    /// ... other work on a queue (page mappings; a present, unless it goes
    /// behind the work in flight), ...
    WaitWork,
    /// ... a queue at a wait that is not met, ...
    WaitHeld,
    /// ... a present, ...
    WaitPresent,
    /// ... a title thread blocked on a fence, ...
    WaitFence,
    /// ... a list being reset or let go of.
    WaitList,
    /// Calls on the title's queues: submissions, ...
    Executes,
    /// ... signals, ...
    Signals,
    /// ... waits for a fence, ...
    QueueWaits,
    /// ... presents, ...
    Presents,
    /// ... and page mappings.
    Mappings,
    /// The time between two such calls of one thread: the title's own work.
    TitleGap,
    /// Signals put behind a submission in flight, ...
    LateSignals,
    /// ... waits met by one ahead in the host queue, ...
    LateWaits,
    /// ... and submissions not waited for that landed (were finished once
    /// the host GPU had run them), on any thread.
    Landed,
    /// Lists given to the host queue, and the calls that gave them.
    HostLists,
    Ecls,
    /// The title's buffer copies done at submission: how many, bytes, time.
    Copies,
    CopyBytes,
    CopyTime,
    /// Title memory copied into the lists' upload memory at submission.
    Fills,
    FillBytes,
    FillTime,
    /// Refreshes of buffer copies, after joining, and their time.
    Updates,
    UpdateTime,
    /// Ranges shaders write, as the submissions' lists declared them.
    Declared,
    /// Pieces of copies kept before a submission and compared after it, with
    /// their bytes.
    Kept,
    KeptBytes,
    Compared,
    ComparedBytes,
    /// Misaligned constant buffers of directly indexed heaps copied.
    Bounces,
    BounceBytes,
    BounceTime,
    /// Ranges directly indexed heaps read, brought up to date.
    PlanReads,
    PlanReadTime,
    /// State preludes recorded, and the time.
    Preludes,
    PreludeTime,
    /// Work cut into pieces for several threads (`host::in_parallel`).
    Parallel,
    /// Title memory opened before a write of ours, and how often there was
    /// nothing to open.
    Openings,
    OpeningsIdle,
    /// How many of the waits above had to wait, by the same reasons.
    DrainsExecute,
    DrainsSignal,
    DrainsWork,
    DrainsHeld,
    DrainsPresent,
    DrainsFence,
    DrainsList,
    /// Submissions landed by a thread that needed them sooner than the
    /// landing thread got to them.
    LandedHere,
    /// Askings of the kernel's write watch, their time, and refreshes
    /// answered by a submission's asking ahead instead
    /// (`crate::watch::ask_ahead`).
    WatchAsks,
    WatchTime,
    WatchAhead,
    /// Title buffer copies that went on where the one before ended, made one
    /// with it.
    CopiesJoined,
    /// Dynamic descriptor slots given back when the submission of their
    /// list landed (`crate::host::free_tables_at_landing`).
    SlotsFreedAtLanding,
    /// Tables that found no free dynamic slots and waited for a landing,
    /// and the time they waited.
    TableRetries,
    TableRetryTime,
    /// Submissions of lists the title submitted again after other tables
    /// took their slots: their commands were not run.
    TablesLost,
    /// Lent dynamic descriptor slots other tables took, when no run of
    /// other free slots fitted them
    /// (`crate::descriptor::host::HostHeap::give_back`).
    LentTaken,
}

const COUNTS: usize = Count::LentTaken as usize + 1;

/// The count of waits of the kind a wait's time is counted as.
pub(crate) fn drains_of(why: Count) -> Option<Count> {
    Some(match why {
        Count::WaitExecute => Count::DrainsExecute,
        Count::WaitSignal => Count::DrainsSignal,
        Count::WaitWork => Count::DrainsWork,
        Count::WaitHeld => Count::DrainsHeld,
        Count::WaitPresent => Count::DrainsPresent,
        Count::WaitFence => Count::DrainsFence,
        Count::WaitList => Count::DrainsList,
        _ => return None,
    })
}
static TOTALS: [AtomicU64; COUNTS] = [const { AtomicU64::new(0) }; COUNTS];

/// What the sample shows the highest value of, not a sum.
#[derive(Clone, Copy)]
pub(crate) enum Gauge {
    /// Blocks left writable for a kernel write (`crate::watch`).
    Pins,
    /// Dynamic descriptor slots taken in the fullest view heap.
    Slots,
    /// Submissions in flight at once.
    Flights,
}

const GAUGES: usize = Gauge::Flights as usize + 1;
static HIGHEST: [AtomicU64; GAUGES] = [const { AtomicU64::new(0) }; GAUGES];

/// The counts the panel does not draw, by their name in the sample, with
/// what they are divided by (microseconds and bytes become milliseconds and
/// megabytes).
const MORE: &[(&str, Count, f64)] = &[
    ("gpu_idle_ms", Count::GpuIdle, 1e3),
    ("wait_execute_ms", Count::WaitExecute, 1e3),
    ("wait_signal_ms", Count::WaitSignal, 1e3),
    ("wait_work_ms", Count::WaitWork, 1e3),
    ("wait_held_ms", Count::WaitHeld, 1e3),
    ("wait_present_ms", Count::WaitPresent, 1e3),
    ("wait_fence_ms", Count::WaitFence, 1e3),
    ("wait_list_ms", Count::WaitList, 1e3),
    ("executes", Count::Executes, 1.0),
    ("signals", Count::Signals, 1.0),
    ("queue_waits", Count::QueueWaits, 1.0),
    ("presents", Count::Presents, 1.0),
    ("mappings", Count::Mappings, 1.0),
    ("title_gap_ms", Count::TitleGap, 1e3),
    ("late_signals", Count::LateSignals, 1.0),
    ("late_waits", Count::LateWaits, 1.0),
    ("landed", Count::Landed, 1.0),
    ("host_lists", Count::HostLists, 1.0),
    ("ecl_calls", Count::Ecls, 1.0),
    ("copies", Count::Copies, 1.0),
    ("copies_mb", Count::CopyBytes, 1e6),
    ("copies_ms", Count::CopyTime, 1e3),
    ("fills", Count::Fills, 1.0),
    ("fills_mb", Count::FillBytes, 1e6),
    ("fills_ms", Count::FillTime, 1e3),
    ("updates", Count::Updates, 1.0),
    ("updates_ms", Count::UpdateTime, 1e3),
    ("declared", Count::Declared, 1.0),
    ("kept", Count::Kept, 1.0),
    ("kept_mb", Count::KeptBytes, 1e6),
    ("compared", Count::Compared, 1.0),
    ("compared_mb", Count::ComparedBytes, 1e6),
    ("bounces", Count::Bounces, 1.0),
    ("bounces_mb", Count::BounceBytes, 1e6),
    ("bounces_ms", Count::BounceTime, 1e3),
    ("plan_reads", Count::PlanReads, 1.0),
    ("plan_reads_ms", Count::PlanReadTime, 1e3),
    ("preludes", Count::Preludes, 1.0),
    ("preludes_ms", Count::PreludeTime, 1e3),
    ("parallel_calls", Count::Parallel, 1.0),
    ("openings", Count::Openings, 1.0),
    ("openings_idle", Count::OpeningsIdle, 1.0),
    ("drains_execute", Count::DrainsExecute, 1.0),
    ("drains_signal", Count::DrainsSignal, 1.0),
    ("drains_work", Count::DrainsWork, 1.0),
    ("drains_held", Count::DrainsHeld, 1.0),
    ("drains_present", Count::DrainsPresent, 1.0),
    ("drains_fence", Count::DrainsFence, 1.0),
    ("drains_list", Count::DrainsList, 1.0),
    ("landed_here", Count::LandedHere, 1.0),
    ("watch_asks", Count::WatchAsks, 1.0),
    ("watch_ms", Count::WatchTime, 1e3),
    ("watch_ahead", Count::WatchAhead, 1.0),
    ("copies_joined", Count::CopiesJoined, 1.0),
    ("slots_freed_at_landing", Count::SlotsFreedAtLanding, 1.0),
    ("table_retries", Count::TableRetries, 1.0),
    ("table_retry_ms", Count::TableRetryTime, 1e3),
    ("tables_lost", Count::TablesLost, 1.0),
    ("lent_taken", Count::LentTaken, 1.0),
];

const MORE_GAUGES: [(&str, Gauge); GAUGES] = [
    ("pins_max", Gauge::Pins),
    ("slots_max", Gauge::Slots),
    ("flights_max", Gauge::Flights),
];

pub(crate) fn add(what: Count, n: u64) {
    TOTALS[what as usize].fetch_add(n, Ordering::Relaxed);
}

/// [`add`], if measurements are on.
pub(crate) fn count(what: Count, n: u64) {
    if on() {
        add(what, n);
    }
}

/// Notes a value of which the highest is shown, if measurements are on.
pub(crate) fn note(what: Gauge, n: u64) {
    if on() {
        HIGHEST[what as usize].fetch_max(n, Ordering::Relaxed);
    }
}

/// Takes what was counted since the last time.
pub(crate) fn take(what: Count) -> u64 {
    TOTALS[what as usize].swap(0, Ordering::Relaxed)
}

/// The counts since the last time that the sample has no place of its own
/// for, per frame of `frames`, as the sample's last fields.
pub(crate) fn more(frames: f64) -> String {
    use std::fmt::Write;
    let mut text = String::with_capacity(1200);
    for &(name, what, unit) in MORE {
        let _ = write!(text, ",\"{name}\":{:.2}", take(what) as f64 / unit / frames);
    }
    for (name, what) in MORE_GAUGES {
        let _ = write!(text, ",\"{name}\":{}", HIGHEST[what as usize].swap(0, Ordering::Relaxed));
    }
    text
}

/// The time now, if measurements are on.
pub(crate) fn start() -> Option<Instant> {
    on().then(Instant::now)
}

/// Adds the time since `started`.
pub(crate) fn stop(what: Count, started: Option<Instant>) {
    if let Some(started) = started {
        add(what, started.elapsed().as_micros() as u64);
    }
}

thread_local! {
    /// When the thread's last call on a queue returned.
    static LEFT: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}

/// A call of the title's on one of its queues, until it is dropped.
pub(crate) struct Call(());

/// Counts a call of the title's on a queue, and the time since the thread's
/// last one (a second and more is not the frame's: the thread did something
/// else); `None` with measurements off.
pub(crate) fn call(what: Count) -> Option<Call> {
    if !on() {
        return None;
    }
    add(what, 1);
    if let Some(left) = LEFT.take() {
        let gap = left.elapsed().as_micros() as u64;
        if gap < 1_000_000 {
            add(Count::TitleGap, gap);
        }
    }
    Some(Call(()))
}

impl Drop for Call {
    fn drop(&mut self) {
        LEFT.set(Some(Instant::now()));
    }
}

/// Submissions timed at once: those in flight.
#[cfg(windows)]
const PAIRS: usize = 8;

/// Pairs of lists that take the host GPU's time around a submission, one
/// pair for each submission in flight.
#[cfg(windows)]
pub(crate) struct Clock {
    pairs: Vec<[ID3D12CommandList; 2]>,
    /// The pairs' timestamps two by two, in mapped read-back memory.
    times: *const u64,
    /// Ticks per second.
    frequency: u64,
    /// Which pairs submissions have (by bit), and the last one's end.
    used: std::sync::Mutex<(u32, u64)>,
    /// What the lists were recorded in and name.
    _allocators: Vec<ID3D12CommandAllocator>,
    _heap: ID3D12QueryHeap,
    _buffer: ID3D12Resource,
}

// SAFETY: D3D12 objects are free-threaded; the mapped words are read only
// once the submission that has the pair ran.
#[cfg(windows)]
unsafe impl Send for Clock {}
// SAFETY: as above.
#[cfg(windows)]
unsafe impl Sync for Clock {}

/// A pair of [`Clock`]'s lists a submission has.
#[cfg(windows)]
#[derive(Clone, Copy)]
pub(crate) struct Pair(usize);

#[cfg(windows)]
impl Clock {
    pub(crate) fn new(host: &Host) -> Option<Clock> {
        let (buffer, times) = host.readback_buffer(16 * PAIRS as u64)?;
        let made = Self::record(&host.device, host.queue(), buffer, times.cast());
        made.inspect_err(|e| {
            tracing::warn!(error = %e, "no host GPU timestamps for the session panel");
        })
        .ok()
    }

    fn record(
        device: &ID3D12Device,
        queue: &ID3D12CommandQueue,
        buffer: ID3D12Resource,
        times: *const u64,
    ) -> windows::core::Result<Clock> {
        use windows::core::Interface;
        // SAFETY: plain creation; the recorded lists name objects the clock
        // keeps.
        unsafe {
            let mut heap: Option<ID3D12QueryHeap> = None;
            device.CreateQueryHeap(
                &D3D12_QUERY_HEAP_DESC {
                    Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                    Count: 2 * PAIRS as u32,
                    NodeMask: 0,
                },
                &mut heap,
            )?;
            let heap = heap.ok_or_else(windows::core::Error::empty)?;
            let mut allocators = Vec::new();
            let mut list = || -> windows::core::Result<ID3D12GraphicsCommandList> {
                let allocator: ID3D12CommandAllocator =
                    device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
                let list =
                    device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
                allocators.push(allocator);
                Ok(list)
            };
            let mut pairs = Vec::new();
            for k in 0..PAIRS as u32 {
                let (begin, end) = (list()?, list()?);
                begin.EndQuery(&heap, D3D12_QUERY_TYPE_TIMESTAMP, 2 * k);
                begin.Close()?;
                end.EndQuery(&heap, D3D12_QUERY_TYPE_TIMESTAMP, 2 * k + 1);
                let at = 16 * u64::from(k);
                end.ResolveQueryData(&heap, D3D12_QUERY_TYPE_TIMESTAMP, 2 * k, 2, &buffer, at);
                end.Close()?;
                pairs.push([begin.cast()?, end.cast()?]);
            }
            Ok(Clock {
                pairs,
                times,
                frequency: queue.GetTimestampFrequency()?.max(1),
                used: std::sync::Mutex::new((0, 0)),
                _allocators: allocators,
                _heap: heap,
                _buffer: buffer,
            })
        }
    }

    fn used(&self) -> std::sync::MutexGuard<'_, (u32, u64)> {
        self.used.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A pair no submission has, until [`Clock::ran`].
    pub(crate) fn pair(&self) -> Option<Pair> {
        let mut used = self.used();
        let free = (0..self.pairs.len()).find(|k| used.0 >> k & 1 == 0)?;
        used.0 |= 1 << free;
        Some(Pair(free))
    }

    pub(crate) fn begin(&self, pair: Pair) -> ID3D12CommandList {
        self.pairs[pair.0][0].clone()
    }

    pub(crate) fn end(&self, pair: Pair) -> ID3D12CommandList {
        self.pairs[pair.0][1].clone()
    }

    /// The timestamps of the submission that ran with `pair` last.
    fn read(&self, pair: Pair) -> [u64; 2] {
        // SAFETY: both words are mapped; the GPU has written them.
        [0, 1].map(|i| unsafe { self.times.add(2 * pair.0 + i).read_volatile() })
    }

    /// Counts the submission that ran between `pair`'s lists, and how long
    /// the host GPU had nothing to do before it, and takes the pair back.
    /// Submissions come here in the order they ran.
    pub(crate) fn ran(&self, pair: Pair) {
        let [from, to] = self.read(pair);
        let us = |ticks: u64| (u128::from(ticks) * 1_000_000 / u128::from(self.frequency)) as u64;
        let mut used = self.used();
        used.0 &= !(1 << pair.0);
        if to <= from {
            return;
        }
        add(Count::Gpu, us(to - from));
        add(Count::GpuTimed, 1);
        // A second and more is no frame's: the title was not at work.
        let idle = from.saturating_sub(used.1);
        if used.1 != 0 && idle < self.frequency {
            add(Count::GpuIdle, us(idle));
        }
        used.1 = used.1.max(to);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sample's last fields go on in its JSON, each name once.
    #[test]
    fn the_extra_counts_are_fields_of_their_own() {
        let names: std::collections::HashSet<&str> =
            MORE.iter().map(|m| m.0).chain(MORE_GAUGES.iter().map(|m| m.0)).collect();
        assert_eq!(names.len(), MORE.len() + MORE_GAUGES.len());
        let text = more(2.0);
        assert!(text.starts_with(','));
        for field in text[1..].split(',') {
            let (name, value) = field.split_once(':').expect("a name and a value");
            assert!(name.starts_with('"') && name.ends_with('"'), "{field}");
            assert!(value.parse::<f64>().is_ok_and(f64::is_finite), "{field}");
        }
    }

    /// The lists run again and again, several pairs at once, and an ordered
    /// pair of later times comes back each time.
    #[cfg(windows)]
    #[test]
    fn the_clock_times_every_submission_it_goes_around() {
        let Some(host) = crate::host::host() else { return };
        let Some(clock) = Clock::new(host) else { return };
        // SAFETY: our own queue, fence and closed lists; each run is waited
        // for before its pairs are used again.
        unsafe {
            let queue: ID3D12CommandQueue = host
                .device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })
                .unwrap();
            let fence: ID3D12Fence = host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let mut last = 0;
            for run in 1..=3 {
                let pairs: Vec<Pair> = std::iter::from_fn(|| clock.pair()).collect();
                assert_eq!(pairs.len(), PAIRS, "every pair is free again");
                for &pair in &pairs {
                    queue.ExecuteCommandLists(&[Some(clock.begin(pair)), Some(clock.end(pair))]);
                }
                queue.Signal(&fence, run).unwrap();
                while fence.GetCompletedValue() < run {
                    std::thread::yield_now();
                }
                for &pair in &pairs {
                    let [from, to] = clock.read(pair);
                    assert!(to >= from, "run {run}: {from} to {to}");
                    assert!(from > last, "run {run}: {from} after {last}");
                    last = from;
                    clock.ran(pair);
                }
            }
        }
    }
}
