//! Host pipelines are made by workers, not by the title thread that first
//! records a draw with them. Translating a shader and the host driver's
//! compile take from milliseconds to seconds each (structurisation of the
//! biggest compute programs ~0.4 s, some driver compiles 3.4 s), and Grand
//! Theft Auto VI's first world frame wants over 1,200 new pipelines: made
//! on its recording threads, that frame took more than 30 s and the
//! title's GPU watchdog ended the run (decision 0007, "Pipelines made off
//! the title's threads").
//!
//! First binding can start compilation. A draw promotes its pipeline and may
//! wait within a wall-clock budget shared by recording threads. A pipeline
//! given a substantial wait is polled later while its worker finishes.
//!
//! Passes over the screen (no vertex input, no depth) have workers and a
//! longer budget of their own: the title fills its lighting tables with
//! such draws once, at the world's first frame, and behind a world's worth
//! of pipelines they waited minutes; a run that lost them had no diffuse
//! light at all.
//!
//! A one-off pass with a pipeline that misses its budget can still be lost.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::Shader;
use super::host::{GraphicsState, HostPipeline};

/// What a host pipeline is made from, and the host pipeline once made.
pub(crate) struct Pending {
    shaders: Vec<Arc<Shader>>,
    pub(crate) graphics: Option<Box<GraphicsState>>,
    queue_state: AtomicU8,
    made: OnceLock<Option<HostPipeline>>,
    /// Its worker was lost to it: it will not be made.
    lost: AtomicBool,
    /// Recording already gave this in-flight job time to finish.
    wait_expired: AtomicBool,
    /// A pass over the screen (see [`GraphicsState::screen_pass`]).
    screen: bool,
    /// What identifies it across runs (shader containers and state).
    key: String,
    /// Listed for the next run's early start once a draw asked for it
    /// ([`early_at_once`]), and as before once it is also made.
    prewarm: AtomicU8,
    /// Taken by a worker from [`Queue::slow`].
    was_slow: AtomicBool,
}

// SAFETY: D3D12 objects are free-threaded; the graphics state (whose input
// elements point into its own names) is only read after creation.
unsafe impl Send for Pending {}
// SAFETY: as above.
unsafe impl Sync for Pending {}

impl Pending {
    pub(crate) fn new(shaders: Vec<Arc<Shader>>, graphics: Option<Box<GraphicsState>>) -> Pending {
        let mut key = blake3::Hasher::new();
        for s in &shaders {
            key.update(s.stage.as_bytes());
            key.update(s.hash.as_bytes());
        }
        key.update(graphics.as_ref().map(|g| g.describe()).unwrap_or_default().as_bytes());
        Pending {
            screen: graphics.as_ref().is_some_and(|g| g.screen_pass()),
            shaders,
            graphics,
            queue_state: AtomicU8::new(NEW),
            made: OnceLock::new(),
            lost: AtomicBool::new(false),
            wait_expired: AtomicBool::new(false),
            key: key.finalize().to_hex()[..32].to_owned(),
            prewarm: AtomicU8::new(0),
            was_slow: AtomicBool::new(false),
        }
    }

    #[cfg(test)]
    pub(super) fn key(&self) -> &str {
        &self.key
    }

    fn make(&self) {
        let began = busy_ms();
        // Not `get_or_init`: a worker lost in it would leave it locked.
        let made = HostPipeline::new(&self.shaders, self.graphics.as_deref());
        if let (Some(began), Some(ended)) = (began, busy_ms()) {
            super::library::note_time(&self.key, ended.saturating_sub(began));
        }
        let compiled = made.is_some();
        let _ = self.made.set(made);
        wake_waiters();
        if compiled {
            self.mark_prewarm(COMPILED);
        } else if early_at_once() {
            // Listed when asked for, or by a run before: no translation or
            // refusal is kept, so it would be tried at every start.
            super::library::note_unused(&self.key);
        }
    }

    /// Notes `flag`, and lists the pipeline for the next run's early start
    /// when what that asks for is there.
    fn mark_prewarm(&self, flag: u8) {
        if self.prewarm.load(Ordering::Relaxed) & flag != 0 {
            return;
        }
        let wants = if early_at_once() { WANTED } else { WANTED | COMPILED };
        let before = self.prewarm.fetch_or(flag, Ordering::AcqRel);
        // One made without a host pipeline is not listed again.
        let refused = matches!(self.made.get(), Some(None));
        if !refused && before & wants != wants && (before | flag) & wants == wants {
            super::library::note_used(&self.key);
        }
    }
}

const WANTED: u8 = 1;
const COMPILED: u8 = 2;

/// Whether a pipeline a draw or dispatch asks for is listed for the next
/// run's early start at once, made or not: a short session ended with
/// hundreds still queued (373 to 436 in three logs), and the next one did
/// not start them while it loaded. From github.com/avatarmanz/VIctory-7900XTX
/// (d279e69); one made without a host pipeline is taken off the list here.
/// `VYCE_EARLY_AT_ONCE=0`: listed once it is made too, as before.
fn early_at_once() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VYCE_EARLY_AT_ONCE").map_or(true, |v| v != "0"))
}

/// The processor time this thread has had, in milliseconds: what making a
/// pipeline takes, without the time it waited for a processor.
fn busy_ms() -> Option<u64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let mut times = [FILETIME::default(); 4];
    let [created, exited, kernel, user] = &mut times;
    // SAFETY: a plain query on our own thread, into times of ours.
    let known = unsafe { GetThreadTimes(GetCurrentThread(), created, exited, kernel, user) } != 0;
    // In 100 ns.
    let ms = |t: &FILETIME| {
        (u64::from(t.dwHighDateTime) << 32 | u64::from(t.dwLowDateTime)) / 10_000
    };
    known.then(|| ms(&times[2]) + ms(&times[3]))
}

/// Why a pipeline has no host pipeline (diagnostics): its shaders that do
/// not translate, else "refused" or "being made"; `None` if it has one.
#[cfg(feature = "diagnostics")]
pub(crate) fn missing(p: &Pending) -> Option<String> {
    match p.made.get() {
        Some(Some(_)) => None,
        None if p.lost.load(Ordering::Acquire) => Some("compiler worker lost".into()),
        None => Some("being made".into()),
        Some(None) => {
            let failed: Vec<String> = p
                .shaders
                .iter()
                .filter(|s| matches!(s.translated.get(), Some(None)))
                .map(|s| format!("{} {}", s.stage, s.hash))
                .collect();
            Some(if failed.is_empty() {
                format!("refused {:?}", p.shaders)
            } else {
                failed.join(" + ")
            })
        }
    }
}

#[derive(Default)]
pub(crate) struct WaitStats {
    pub(crate) micros: u64,
    pub(crate) calls: u64,
    pub(crate) timeouts: u64,
}

/// Time during which at least one recorder is waiting, by presentation.
/// Overlapping waiters spend the same interval only once.
#[derive(Default)]
struct WaitWindow {
    frame: Option<u64>,
    limit: Duration,
    spent: Duration,
    active_since: Option<Instant>,
    waiters: usize,
}

impl WaitWindow {
    fn remaining(&self, now: Instant) -> Duration {
        let active = self.active_since.map_or(Duration::ZERO, |start| now - start);
        self.limit.saturating_sub(self.spent + active)
    }

    fn enter(&mut self, frame: u64, now: Instant, limit: Duration) -> Option<Instant> {
        if self.frame.is_some_and(|current| frame < current) {
            return None;
        }
        if self.frame != Some(frame) {
            *self = Self { frame: Some(frame), limit, ..Self::default() };
        }
        let remaining = self.remaining(now);
        if remaining.is_zero() {
            return None;
        }
        self.active_since.get_or_insert(now);
        self.waiters += 1;
        Some(now + remaining)
    }

    fn leave(&mut self, frame: u64, now: Instant) {
        if self.frame == Some(frame) {
            self.waiters -= 1;
            if self.waiters == 0 {
                self.spent += now - self.active_since.take().expect("active waiter");
            }
        }
    }
}

struct WaitState {
    windows: [WaitWindow; 2],
    stats: [WaitStats; 2],
}

/// Signalled whenever a worker has made a pipeline or been lost.
static DONE: std::sync::LazyLock<(Mutex<WaitState>, Condvar)> = std::sync::LazyLock::new(|| (
    Mutex::new(WaitState { windows: Default::default(), stats: Default::default() }),
    Condvar::new(),
));

/// Completed waits for ordinary and screen passes. Times on different
/// threads sum in telemetry even though their budget is shared.
pub(crate) fn take_wait_stats() -> [WaitStats; 2] {
    let mut held = DONE.0.lock().unwrap_or_else(|e| e.into_inner());
    std::mem::take(&mut held.stats)
}

fn wake_waiters() {
    let (lock, done) = &*DONE;
    let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
    done.notify_all();
}

const STARTUP_BUDGET: Duration = Duration::from_millis(500);
const ORDINARY_BUDGET: Duration = Duration::from_millis(32);
const STREAMING_BUDGET: Duration = Duration::from_millis(12);
const BACKLOG: u64 = 32;
/// Screen passes include one-off lighting bakes.
const SCREEN_BUDGET: Duration = Duration::from_secs(20);

fn budget(screen: bool, frame: u64, queued: u64) -> Duration {
    if screen {
        SCREEN_BUDGET
    } else if queued >= BACKLOG {
        STREAMING_BUDGET
    } else if frame == 0 {
        STARTUP_BUDGET
    } else {
        ORDINARY_BUDGET
    }
}

/// Pipelines queued and not made yet.
static WAITING: AtomicU64 = AtomicU64::new(0);

/// Pipelines queued and not made yet (for the frame line).
pub(crate) fn waiting() -> u64 {
    WAITING.load(Ordering::Relaxed)
}

/// Pipelines that were queued and are done with: made, refused or lost.
static FINISHED: AtomicU64 = AtomicU64::new(0);

pub(crate) fn finished() -> u64 {
    FINISHED.load(Ordering::Relaxed)
}

/// The host pipeline if it is made (or is made within the frame's budget);
/// queues it on first use.
pub(crate) fn ready(p: &Arc<Pending>) -> Option<&HostPipeline> {
    p.mark_prewarm(WANTED);
    if p.made.get().is_none() {
        start(p, REQUESTED, false);
        wait(p);
    }
    peek(p)
}

pub(crate) fn peek(p: &Pending) -> Option<&HostPipeline> {
    p.made.get()?.as_ref()
}

/// Let the first bind overlap compilation with recording. Limit speculative
/// ordinary work to a short queue. Actual draws can always promote it.
pub(crate) fn bound(p: &Arc<Pending>) {
    if !p.shaders.is_empty() {
        start(p, EARLY, true);
    }
}

/// At the title's creation of a pipeline: starts it at once if an earlier
/// run drew with it (during loading, so the world's first frames find it).
pub(crate) fn early(p: &Arc<Pending>) {
    if !p.shaders.is_empty() && super::library::wanted_early(&p.key) {
        super::library::EARLY.fetch_add(1, Ordering::Relaxed);
        start(p, EARLY, false);
    }
}

/// Only a presentation event opens a fresh budget. A wait admitted before the
/// rollover keeps its original deadline, but never spends the new budget.
fn wait(p: &Pending) {
    if p.lost.load(Ordering::Acquire) || p.wait_expired.load(Ordering::Relaxed) {
        return;
    }
    let kind = usize::from(p.screen);
    let (lock, done) = &*DONE;
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    if p.made.get().is_some()
        || p.lost.load(Ordering::Acquire)
        || p.wait_expired.load(Ordering::Relaxed)
    {
        return;
    }
    let frame = crate::queue::presented();
    let limit = budget(p.screen, frame, waiting());
    let started = Instant::now();
    let window = &mut held.windows[kind];
    let Some(deadline) = window.enter(frame, started, limit) else { return };

    let mut timed_out = false;
    let mut blocked = false;
    loop {
        if p.made.get().is_some() || p.lost.load(Ordering::Acquire) {
            break;
        }
        let now = Instant::now();
        let rest = deadline.saturating_duration_since(now);
        if rest.is_zero() {
            timed_out = blocked;
            break;
        }
        blocked = true;
        held = done.wait_timeout(held, rest).unwrap_or_else(|e| e.into_inner()).0;
    }
    let ended = Instant::now();
    held.windows[kind].leave(frame, ended);
    if blocked {
        let stats = &mut held.stats[kind];
        stats.micros += (ended - started).as_micros() as u64;
        stats.calls += 1;
        stats.timeouts += u64::from(timed_out);
    }
    let grace = if p.screen { SCREEN_BUDGET } else { STREAMING_BUDGET };
    // A caller that only received the tail of a shared window is still
    // eligible for a meaningful first-use wait in another frame.
    if timed_out && deadline - started >= grace / 2 {
        p.wait_expired.store(true, Ordering::Relaxed);
    }
}

const NEW: u8 = 0;
const EARLY: u8 = 1;
const REQUESTED: u8 = 2;
const CLAIMED: u8 = 3;

/// Whether pipelines known to take long ([`super::library::slow`]) that no
/// draw asked for yet are made by a quarter of a pool's workers at most, so
/// that the quick ones go first. On fresh caches (a new translator or
/// driver) the early list holds programs of tens of seconds each, and all
/// workers were in them through the title's loading (97 made in 20 s), and
/// the world's first minute and a half dropped up to 2,900 draws a frame.
/// From github.com/avatarmanz/VIctory-7900XTX (a30829c).
/// `VICTORY_PIPELINE_SLOW=0`: in the order the title creates them, as before.
fn slow_first_limited() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_PIPELINE_SLOW").map_or(true, |v| v != "0"))
}

#[derive(Default)]
struct Queue {
    early: VecDeque<Arc<Pending>>,
    /// Early ones known to take long, and how many of them workers are
    /// making ([`slow_first_limited`]).
    slow: VecDeque<Arc<Pending>>,
    slow_now: usize,
    requested: VecDeque<Arc<Pending>>,
    workers: usize,
}

impl Queue {
    /// Returns whether this is a new job or a promotion. Once claimed, a job
    /// cannot be queued again even if the driver fails to compile it. `slow`:
    /// it is known to take long (only an early one is kept apart).
    fn push(&mut self, p: &Arc<Pending>, priority: u8, slow: bool) -> Option<bool> {
        let prev = p.queue_state.load(Ordering::Relaxed);
        if prev >= priority {
            return None;
        }
        p.queue_state.store(priority, Ordering::Relaxed);
        match priority {
            REQUESTED => self.requested.push_back(p.clone()),
            EARLY if slow => self.slow.push_back(p.clone()),
            EARLY => self.early.push_back(p.clone()),
            _ => unreachable!("invalid pipeline priority"),
        }
        Some(prev == NEW)
    }

    /// How many slow ones may be made at once: a quarter of the workers,
    /// one at least.
    fn slow_limit(&self) -> usize {
        (self.workers / 4).max(1)
    }

    fn pop(&mut self) -> Option<Arc<Pending>> {
        // A promoted entry's old prewarm reference can still be here.
        let unclaimed = |list: &mut VecDeque<Arc<Pending>>| {
            std::iter::from_fn(|| list.pop_front())
                .find(|p| p.queue_state.load(Ordering::Relaxed) == EARLY)
        };
        let mut next = self.requested.pop_front().or_else(|| unclaimed(&mut self.early));
        if next.is_none() && self.slow_now < self.slow_limit() {
            next = unclaimed(&mut self.slow);
            if let Some(p) = &next {
                p.was_slow.store(true, Ordering::Relaxed);
                self.slow_now += 1;
            }
        }
        let next = next?;
        next.queue_state.store(CLAIMED, Ordering::Relaxed);
        Some(next)
    }

    /// A worker is done with `p`, made or lost.
    fn finished(&mut self, p: &Pending) {
        if p.was_slow.swap(false, Ordering::Relaxed) {
            self.slow_now = self.slow_now.saturating_sub(1);
        }
    }

    fn fail_waiting(&mut self) -> u64 {
        let mut failed = 0;
        let all = std::mem::take(&mut self.requested)
            .into_iter()
            .chain(std::mem::take(&mut self.early))
            .chain(std::mem::take(&mut self.slow));
        for p in all {
            // A promoted entry is in two lists: counted once.
            if p.queue_state.swap(CLAIMED, Ordering::Relaxed) != CLAIMED {
                p.lost.store(true, Ordering::Release);
                failed += 1;
            }
        }
        failed
    }
}

struct Pool {
    queue: Mutex<Queue>,
    available: Condvar,
    started: OnceLock<()>,
}

impl Pool {
    const fn new() -> Self {
        Self {
            queue: Mutex::new(Queue {
                early: VecDeque::new(),
                slow: VecDeque::new(),
                slow_now: 0,
                requested: VecDeque::new(),
                workers: 0,
            }),
            available: Condvar::new(),
            started: OnceLock::new(),
        }
    }

    fn next(&self) -> Arc<Pending> {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(p) = queue.pop() {
                return p;
            }
            queue = self.available.wait(queue).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// Keep the screen passes' workers independent of the ordinary pipelines.
static POOLS: [Pool; 2] = [const { Pool::new() }; 2];

/// Queue a prewarm job or promote a job needed by a draw. Claims and
/// promotions are serialized by the pool's queue lock.
fn start(p: &Arc<Pending>, priority: u8, speculative: bool) {
    if p.queue_state.load(Ordering::Relaxed) >= priority {
        return;
    }
    // Asked before the queue's lock is taken: the list is read from disk
    // the first time.
    let slow = priority == EARLY && slow_first_limited() && super::library::slow(&p.key);
    let pool = &POOLS[usize::from(p.screen)];
    pool.started.get_or_init(|| {
        let mut queue = pool.queue.lock().unwrap_or_else(|e| e.into_inner());
        let count = if p.screen {
            2
        } else {
            std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).clamp(2, 8))
        };
        for i in 0..count {
            queue.workers += usize::from(worker(i + if p.screen { 50 } else { 0 }, p.screen));
        }
    });
    let mut queue = pool.queue.lock().unwrap_or_else(|e| e.into_inner());
    if queue.workers == 0 {
        // The driver's crash handler can park a compiler thread. Compiling
        // here would expose the game's recording thread to that failure.
        p.queue_state.store(CLAIMED, Ordering::Relaxed);
        p.lost.store(true, Ordering::Release);
        drop(queue);
        wake_waiters();
        return;
    }
    let queued = queue.early.len() + queue.slow.len() + queue.requested.len();
    if speculative && queued >= BACKLOG as usize {
        return;
    }
    if let Some(new) = queue.push(p, priority, slow) {
        if new {
            PEAK.fetch_max(WAITING.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
        }
        pool.available.notify_one();
    }
}

/// What the workers (by thread) are making.
static AT_WORK: Mutex<Vec<(u32, Arc<Pending>)>> = Mutex::new(Vec::new());

/// Whether the workers of ordinary pipelines run at the lowest priority:
/// below the title's streaming thread, which runs below normal itself and
/// does all of its loading. From github.com/avatarmanz/VIctory-7900XTX
/// (c278c67). `VICTORY_PIPELINE_LOWEST=0`: below normal, level with it, as
/// before. The workers of passes over the screen stay below normal: the
/// title waits for those.
fn lowest() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_PIPELINE_LOWEST").map_or(true, |v| v != "0"))
}

/// Starts worker `i`, for screen passes or for the others.
fn worker(i: usize, screen: bool) -> bool {
    let spawned = std::thread::Builder::new()
        .name(format!("victory-pipelines-{i}"))
        .spawn(move || {
            // Below the title's own threads (loading, recording).
            // SAFETY: a plain call on our own thread.
            unsafe {
                use windows::Win32::System::Threading::*;
                let priority = if !screen && lowest() {
                    THREAD_PRIORITY_LOWEST
                } else {
                    THREAD_PRIORITY_BELOW_NORMAL
                };
                let _ = SetThreadPriority(GetCurrentThread(), priority);
            }
            let pool = &POOLS[usize::from(screen)];
            loop {
                let p = pool.next();
                // SAFETY: a plain call.
                let id = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
                AT_WORK.lock().unwrap_or_else(|e| e.into_inner()).push((id, p.clone()));
                p.make();
                let tracked = {
                    let mut working = AT_WORK.lock().unwrap_or_else(|e| e.into_inner());
                    working.iter().position(|w| w.0 == id).map(|i| working.swap_remove(i))
                };
                if tracked.is_none() {
                    // A watchdog already accounted for and replaced this worker.
                    return;
                }
                if p.was_slow.load(Ordering::Relaxed) {
                    // A slow one's place is free for the next.
                    pool.queue.lock().unwrap_or_else(|e| e.into_inner()).finished(&p);
                    pool.available.notify_one();
                }
                FINISHED.fetch_add(1, Ordering::Relaxed);
                if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
                    drained();
                } else {
                    super::library::worker_done();
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "no pipeline worker");
        return false;
    }
    true
}

/// A worker is lost to a compile that crashed or never ends: another takes
/// its place (its pipeline stays unmade). With all of them lost, the rest of
/// a world's pipelines waited forever.
pub(super) fn worker_lost(id: u32) {
    static LOST: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(100);
    let mut lost = None;
    AT_WORK.lock().unwrap_or_else(|e| e.into_inner()).retain(|w| {
        if w.0 == id {
            w.1.lost.store(true, Ordering::Release);
            lost = Some(w.1.clone());
        }
        w.0 != id
    });
    if let Some(p) = lost {
        let screen = p.screen;
        let pool = &POOLS[usize::from(screen)];
        let failed = {
            let mut queue = pool.queue.lock().unwrap_or_else(|e| e.into_inner());
            queue.finished(&p);
            if !worker(LOST.fetch_add(1, Ordering::Relaxed), screen) {
                queue.workers -= 1;
            }
            if queue.workers == 0 { queue.fail_waiting() } else { 0 }
        };
        FINISHED.fetch_add(failed + 1, Ordering::Relaxed);
        let finished = WAITING.fetch_sub(failed + 1, Ordering::Relaxed) == failed + 1;
        wake_waiters();
        if finished {
            drained();
        }
    }
}

/// The most pipelines waiting at once since the workers last ran out.
static PEAK: AtomicU64 = AtomicU64::new(0);

/// The workers ran out of work: logged after a batch of 10 or more.
fn drained() {
    use super::library::{EARLY, LOADED, MADE};
    if PEAK.swap(0, Ordering::Relaxed) >= 10 {
        tracing::info!(
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            "host pipelines all made"
        );
    }
    super::library::worker_done();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A first use queues the pipeline and, within the budget, gets the
    /// worker's answer (here: no host pipeline for no shaders).
    #[test]
    fn first_use_is_made_by_a_worker() {
        let p = Arc::new(Pending::new(Vec::new(), None));
        assert!(ready(&p).is_none());
        assert_ne!(p.queue_state.load(Ordering::Relaxed), NEW);
        let started = Instant::now();
        while p.made.get().is_none() && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        assert!(matches!(p.made.get(), Some(None)));
    }

    #[test]
    fn requested_pipelines_take_priority_and_promotions_compile_once() {
        let warm = Arc::new(Pending::new(Vec::new(), None));
        let promote = Arc::new(Pending::new(Vec::new(), None));
        let requested = Arc::new(Pending::new(Vec::new(), None));
        let mut queue = Queue::default();
        assert_eq!(queue.push(&warm, EARLY, false), Some(true));
        assert_eq!(queue.push(&promote, EARLY, false), Some(true));
        assert_eq!(queue.push(&promote, REQUESTED, false), Some(false));
        assert_eq!(queue.push(&promote, REQUESTED, false), None);
        assert_eq!(queue.push(&requested, REQUESTED, false), Some(true));
        for expected in [&promote, &requested, &warm] {
            assert!(Arc::ptr_eq(&queue.pop().unwrap(), expected));
            assert_eq!(queue.push(expected, REQUESTED, false), None);
        }
        assert!(queue.pop().is_none());
    }

    /// After the fork's test (a30829c): the quick one first, then one slow
    /// one at a time; one a draw asks for waits for nobody.
    #[test]
    fn pipelines_known_to_take_long_leave_workers_for_the_others() {
        let new = || Arc::new(Pending::new(Vec::new(), None));
        let (long, longer, asked, quick) = (new(), new(), new(), new());
        let mut queue = Queue { workers: 6, ..Queue::default() };
        assert_eq!(queue.slow_limit(), 1);
        for p in [&long, &longer, &asked] {
            assert_eq!(queue.push(p, EARLY, true), Some(true));
        }
        assert_eq!(queue.push(&quick, EARLY, false), Some(true));
        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &quick));
        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &long));
        assert!(queue.pop().is_none(), "one slow one at a time");
        assert_eq!(queue.push(&asked, REQUESTED, true), Some(false));
        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &asked));
        assert!(!asked.was_slow.load(Ordering::Relaxed));
        assert!(queue.pop().is_none());
        queue.finished(&long);
        queue.finished(&long);
        assert_eq!(queue.slow_now, 0, "counted once");
        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &longer));
        assert!(queue.pop().is_none());
        // With 16 workers, four at a time.
        let mut wide = Queue { workers: 16, ..Queue::default() };
        let all: Vec<_> = (0..6).map(|_| new()).collect();
        all.iter().for_each(|p| assert_eq!(wide.push(p, EARLY, true), Some(true)));
        assert_eq!(std::iter::from_fn(|| wide.pop()).count(), 4);
        // A queue without workers fails the slow ones too.
        assert_eq!(wide.fail_waiting(), 2);
    }

    /// A pipeline a draw asks for is in the next run's early list before
    /// it is made (the fork's test of d279e69).
    #[test]
    fn a_requested_pipeline_is_listed_for_the_next_run_at_once() {
        let mut p = Pending::new(Vec::new(), None);
        p.key = "a pipeline listed at once (test)".to_owned();
        // As if a worker had taken it and not finished: no wait, no make.
        p.queue_state.store(CLAIMED, Ordering::Relaxed);
        p.wait_expired.store(true, Ordering::Relaxed);
        let p = Arc::new(p);
        assert!(!super::super::library::wanted_early(&p.key));
        assert!(ready(&p).is_none());
        assert!(p.made.get().is_none());
        assert!(super::super::library::wanted_early(&p.key));
    }

    /// One that gets no host pipeline is taken off the list again, and the
    /// draws that ask for it after that do not put it back.
    #[test]
    fn a_pipeline_without_a_host_pipeline_is_taken_off_the_list() {
        let mut p = Pending::new(Vec::new(), None);
        p.key = "a pipeline the host refuses (test)".to_owned();
        let p = Arc::new(p);
        let listed = || super::super::library::wanted_early(&p.key);
        assert!(ready(&p).is_none());
        let started = Instant::now();
        while (p.made.get().is_none() || listed()) && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        assert!(matches!(p.made.get(), Some(None)));
        assert!(!listed());
        assert!(ready(&p).is_none());
        assert!(!listed());
    }

    #[test]
    fn a_worker_is_timed_by_its_own_processor_time() {
        let before = busy_ms().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let slept = busy_ms().unwrap() - before;
        assert!(slept < 100, "a thread that sleeps is not busy: {slept} ms");
        // Work counts, however busy the machine is.
        let began = Instant::now();
        let mut x = 0u64;
        while busy_ms().unwrap() - before - slept < 200 {
            assert!(began.elapsed() < Duration::from_secs(20), "no time counted for work");
            for _ in 0..100_000 {
                x = std::hint::black_box(x.wrapping_mul(31).wrapping_add(7));
            }
        }
    }

    #[test]
    fn lost_pipeline_does_not_spend_the_frame_wait_budget() {
        let p = Pending::new(Vec::new(), None);
        p.lost.store(true, Ordering::Release);
        let start = Instant::now();
        wait(&p);
        assert!(start.elapsed() < STARTUP_BUDGET, "a lost worker cannot finish");
    }

    #[test]
    fn overlapping_recorders_share_a_deadline_and_leave_unused_time() {
        let ms = Duration::from_millis;
        let now = Instant::now();
        let mut window = WaitWindow::default();
        assert_eq!(window.enter(6, now, ms(32)), Some(now + ms(32)));
        assert_eq!(window.enter(6, now + ms(4), ms(32)), Some(now + ms(32)));
        assert_eq!(window.enter(6, now + ms(5), ms(32)), Some(now + ms(32)));
        window.leave(6, now + ms(6));
        window.leave(6, now + ms(8));
        window.leave(6, now + ms(10));
        // Ten milliseconds were shared, independent of three blocked threads.
        assert_eq!(window.remaining(now + ms(50)), ms(22));
        assert_eq!(window.enter(6, now + ms(50), ms(32)), Some(now + ms(72)));
        window.leave(6, now + ms(72));
        assert_eq!(window.enter(6, now + ms(73), ms(32)), None);
    }

    #[test]
    fn pressure_changes_the_next_window_and_stale_waiters_cannot_reset_it() {
        let ms = Duration::from_millis;
        let now = Instant::now();
        let mut window = WaitWindow::default();
        assert_eq!(window.enter(3, now, ms(32)), Some(now + ms(32)));
        assert_eq!(window.enter(3, now + ms(5), ms(12)), Some(now + ms(32)));
        assert_eq!(window.enter(4, now + ms(9), ms(12)), Some(now + ms(21)));
        window.leave(3, now + ms(10));
        assert_eq!(window.enter(3, now + ms(11), ms(32)), None);
        assert_eq!(window.remaining(now + ms(12)), ms(9));
        window.leave(4, now + ms(12));
        assert_eq!(window.remaining(now + ms(30)), ms(9));
    }

    #[test]
    fn loading_and_screen_passes_keep_their_own_wait_budget() {
        assert_eq!(budget(false, 0, BACKLOG - 1), STARTUP_BUDGET);
        assert_eq!(budget(false, 1, BACKLOG - 1), ORDINARY_BUDGET);
        assert_eq!(budget(false, 1, BACKLOG), STREAMING_BUDGET);
        assert_eq!(budget(true, 1, BACKLOG), SCREEN_BUDGET);
    }

    #[test]
    fn failing_a_queue_marks_each_promoted_job_only_once() {
        let early = Arc::new(Pending::new(Vec::new(), None));
        let promoted = Arc::new(Pending::new(Vec::new(), None));
        let mut queue = Queue::default();
        queue.push(&early, EARLY, false);
        queue.push(&promoted, EARLY, false);
        queue.push(&promoted, REQUESTED, false);
        assert_eq!(queue.fail_waiting(), 2);
        assert!(early.lost.load(Ordering::Acquire));
        assert!(promoted.lost.load(Ordering::Acquire));
        assert!(queue.pop().is_none());
    }
}
