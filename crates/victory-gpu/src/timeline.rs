//! The title's queues: each runs its work in submission order, and holds it
//! back behind a wait (for a fence, or for a value a list waits for in
//! memory) until that is met.
//!
//! Work runs on the thread that submits it, as long as nothing holds the
//! queue back; else the submission returns, and the work runs on the thread
//! that meets the wait. One title thread submits to all queues, waits before
//! what they wait for, so a submission never blocks.
//!
//! A signal goes behind what the host GPU still runs, and a wait that such
//! work will meet is met, as all queues run on one host queue
//! (`crate::flight`).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::fence::Held;

/// Lists of one submission still to run.
pub(crate) trait Lists: Send {
    /// How many of the first may run now: those that wait for nothing (what
    /// an earlier one writes counts once that ran).
    fn ready(&self) -> usize;
    fn len(&self) -> usize;
    /// Writes what the first, not ready, writes before its wait, if it does
    /// no work before that; whether it wrote anything. Two queues may meet
    /// this way halfway through a list each.
    fn early(&mut self) -> bool {
        false
    }
    /// Takes the first `n`, to be run.
    fn take(&mut self, n: usize) -> Box<dyn FnOnce()>;
    /// What the first waits for (for the log).
    fn waits(&self) -> String;
}

/// One step of a queue's work.
pub(crate) enum Op {
    /// Holds back what follows until the fence reaches the value.
    Wait(Held, u64),
    Signal(Held, u64),
    Lists(Box<dyn Lists>),
    /// Work that needs everything the host GPU runs to be finished.
    Work(Box<dyn FnOnce() + Send>),
    /// Work that goes behind it by itself.
    Behind(Box<dyn FnOnce() + Send>),
}

#[derive(Default)]
pub(crate) struct Timeline {
    state: Mutex<State>,
    /// The flights of another host than the process's (tests).
    #[cfg(all(test, windows))]
    line: std::sync::OnceLock<Arc<crate::flight::Line>>,
}

#[derive(Default)]
struct State {
    ops: VecDeque<Op>,
    /// A thread runs the queue's work.
    busy: bool,
    /// Since when the first op is held back.
    held: Option<Instant>,
}

/// Every queue with a timeline.
static QUEUES: Mutex<Vec<Arc<Timeline>>> = Mutex::new(Vec::new());
/// How many queues are held back.
static HELD: AtomicU64 = AtomicU64::new(0);

/// How long queues may be held back while none of them moves. A wait whose
/// value only comes from later work of the same list, or from work that was
/// dropped, is never met: past this its list runs all the same.
const LIMIT: Duration = Duration::from_secs(2);

/// Whether signals and waits go by the host queue's order
/// ([`crate::flight::late_signal`]).
fn late() -> bool {
    #[cfg(windows)]
    return crate::flight::late_signal();
    #[cfg(not(windows))]
    false
}

/// Whether any queue is held back.
pub(crate) fn held() -> bool {
    HELD.load(Ordering::Acquire) > 0
}

/// Whether any queue has work still to run.
pub(crate) fn active() -> bool {
    queues().iter().any(|q| !q.idle())
}

impl Timeline {
    pub(crate) fn new() -> Arc<Timeline> {
        let timeline = Arc::new(Timeline::default());
        queues().push(timeline.clone());
        timeline
    }

    /// A queue of another host than the process's.
    #[cfg(all(test, windows))]
    pub(crate) fn on(line: &Arc<crate::flight::Line>) -> Arc<Timeline> {
        let timeline = Timeline::new();
        assert!(timeline.line.set(line.clone()).is_ok());
        timeline
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// What is in flight, if there is a host GPU.
    #[cfg(windows)]
    fn line(&self) -> Option<Arc<crate::flight::Line>> {
        #[cfg(test)]
        if let Some(line) = self.line.get() {
            return Some(line.clone());
        }
        crate::host::line().cloned()
    }

    /// Whether a wait for `fence` to reach `value` is met: it has, or it will
    /// when what is in flight, ahead of what follows the wait, has landed.
    fn met(&self, fence: &Held, value: u64) -> bool {
        if fence.reached(value) {
            return true;
        }
        #[cfg(windows)]
        if late() && self.line().is_some_and(|line| line.signals(fence, value)) {
            crate::telemetry::count(crate::telemetry::Count::LateWaits, 1);
            return true;
        }
        false
    }

    /// Whether a queue at a wait that is not met first has what is in flight
    /// finished, which may meet it. (Not with [`late`]: it is looked at again
    /// when that has landed.)
    fn finishing(&self) -> bool {
        #[cfg(windows)]
        return !late() && self.line().is_some_and(|line| line.busy());
        #[cfg(not(windows))]
        false
    }

    /// Waits for what is in flight, for a queue at such a wait.
    fn finish(&self) -> Box<dyn FnOnce()> {
        #[cfg(windows)]
        let line = self.line();
        Box::new(move || {
            #[cfg(windows)]
            if let Some(line) = line {
                line.drain(crate::telemetry::Count::WaitHeld);
            }
        })
    }

    /// A signal of the queue: behind what is in flight, or once that is
    /// finished.
    fn signal(&self, fence: Held, value: u64) -> Box<dyn FnOnce()> {
        #[cfg(windows)]
        let line = self.line();
        Box::new(move || {
            #[cfg(windows)]
            if let Some(line) = line {
                use crate::flight::After;
                if late() {
                    match line.after(After::Signal(fence, value)) {
                        Ok(()) => crate::telemetry::count(crate::telemetry::Count::LateSignals, 1),
                        Err(now) => now.run(),
                    }
                    return;
                }
                line.drain(crate::telemetry::Count::WaitSignal);
            }
            fence.signal(value);
        })
    }

    /// Work of the queue that needs everything before it finished.
    fn work(&self, work: Box<dyn FnOnce() + Send>) -> Box<dyn FnOnce()> {
        #[cfg(windows)]
        let line = self.line();
        Box::new(move || {
            #[cfg(windows)]
            if let Some(line) = line {
                line.drain(crate::telemetry::Count::WaitWork);
            }
            work();
        })
    }

    /// Adds work to the queue and runs what can run, on every queue.
    pub(crate) fn submit(&self, op: Op) {
        self.lock().ops.push_back(op);
        pump();
    }

    /// Whether work submitted now would run at once.
    pub(crate) fn idle(&self) -> bool {
        let s = self.lock();
        s.ops.is_empty() && !s.busy
    }

    /// Runs what can run on this queue; whether anything did. `force`: the
    /// first op runs whether or not its wait is met.
    fn step(&self, mut force: bool) -> bool {
        let mut moved = false;
        loop {
            let mut s = self.lock();
            if s.busy {
                return moved;
            }
            let work: Box<dyn FnOnce()> = match s.ops.front_mut() {
                None => {
                    s.hold(false);
                    return moved;
                }
                Some(Op::Wait(fence, value)) => {
                    if !force && !self.met(fence, *value) {
                        // What it waits for may be what an unwaited
                        // submission still owes (its bottom-of-pipe writes):
                        // that one is finished first, then this is looked
                        // at again. Without this the queue stood until the
                        // wait was forced, two seconds a frame.
                        if !self.finishing() {
                            s.hold(true);
                            return moved;
                        }
                        self.finish()
                    } else {
                        let op = s.ops.pop_front();
                        Box::new(move || drop(op))
                    }
                }
                Some(Op::Lists(lists)) => {
                    let (ready, len) = (lists.ready(), lists.len());
                    let n = if force { ready.max(1) } else { ready }.min(len);
                    if n == 0 && len > 0 {
                        if lists.early() {
                            progress();
                            moved = true;
                            continue;
                        }
                        // As for a fence above.
                        if !self.finishing() {
                            s.hold(true);
                            return moved;
                        }
                        self.finish()
                    } else {
                        let work = lists.take(n);
                        if n == len {
                            s.ops.pop_front();
                        }
                        work
                    }
                }
                Some(Op::Signal(..) | Op::Work(_) | Op::Behind(_)) => match s.ops.pop_front() {
                    Some(Op::Signal(fence, value)) => self.signal(fence, value),
                    Some(Op::Work(work)) => self.work(work),
                    Some(Op::Behind(work)) => work,
                    _ => return moved,
                },
            };
            s.hold(false);
            s.busy = true;
            drop(s);
            work();
            self.lock().busy = false;
            progress();
            (moved, force) = (true, false);
        }
    }
}

impl State {
    fn hold(&mut self, hold: bool) {
        match (hold, self.held) {
            (true, None) => {
                self.held = Some(Instant::now());
                HELD.fetch_add(1, Ordering::AcqRel);
                watch();
            }
            (false, Some(since)) => {
                self.held = None;
                HELD.fetch_sub(1, Ordering::AcqRel);
                static COUNT: AtomicU64 = AtomicU64::new(0);
                let count = COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                if count <= 8 || count.is_power_of_two() {
                    let ms = since.elapsed().as_millis();
                    tracing::info!(count, ms, "a queue waited for another's work");
                }
            }
            _ => {}
        }
    }
}

fn queues() -> MutexGuard<'static, Vec<Arc<Timeline>>> {
    QUEUES.lock().unwrap_or_else(|e| e.into_inner())
}

/// Runs what can run on every queue, until none moves: what one ran may be
/// what another waits for.
pub(crate) fn pump() {
    loop {
        // Queues the title released go once they have run everything.
        let all = {
            let mut queues = queues();
            queues.retain(|q| Arc::strong_count(q) > 1 || !q.idle());
            queues.clone()
        };
        let mut moved = false;
        for queue in &all {
            moved |= queue.step(false);
        }
        if !moved {
            return;
        }
        crate::fence::wake();
    }
}

/// When a queue last ran something.
static MOVED: Mutex<Option<Instant>> = Mutex::new(None);

fn progress() {
    *MOVED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
}

/// Starts the thread that looks at what holds queues back: values the CPU
/// writes to memory wake nothing.
fn watch() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let started = std::thread::Builder::new()
        .name("victory-queues".into())
        .spawn(|| {
            loop {
                if !held() {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                std::thread::sleep(Duration::from_micros(500));
                pump();
                force();
            }
        });
    if let Err(error) = started {
        tracing::warn!(%error, "no thread for held back queues");
    }
}

/// Runs the op held back longest, if nothing moved for `LIMIT`.
fn force() {
    // What the host GPU runs may meet the wait when it lands.
    #[cfg(windows)]
    if late() && crate::host::line().is_some_and(|line| line.busy()) {
        return progress();
    }
    let quiet = MOVED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_none_or(|at| at.elapsed() > LIMIT);
    let all = queues().clone();
    if !quiet || all.iter().any(|q| q.lock().busy) {
        return;
    }
    let oldest = all
        .iter()
        .filter_map(|q| Some((q.lock().held?, q)))
        .min_by_key(|(since, _)| *since);
    let Some((since, queue)) = oldest.filter(|(since, _)| since.elapsed() > LIMIT) else {
        return;
    };
    static FORCED: AtomicU64 = AtomicU64::new(0);
    let forced = FORCED.fetch_add(1, Ordering::Relaxed) + 1;
    if forced <= 32 || forced.is_power_of_two() {
        let waits = match queue.lock().ops.front() {
            Some(Op::Lists(lists)) => lists.waits(),
            Some(Op::Wait(_, value)) => format!("a fence to reach {value}"),
            _ => String::new(),
        };
        for (i, q) in all.iter().enumerate() {
            let s = q.lock();
            for (k, op) in s.ops.iter().enumerate().take(12) {
                let text = match op {
                    Op::Lists(lists) => lists.waits(),
                    Op::Wait(_, v) => format!("fence wait {v}"),
                    Op::Signal(_, v) => format!("signal {v}"),
                    Op::Work(_) | Op::Behind(_) => "work".into(),
                };
                tracing::info!("queue {i} op {k}: {text}");
            }
        }
        tracing::warn!(
            forced,
            ms = since.elapsed().as_millis(),
            waits,
            "a wait was never met: its work runs all the same"
        );
    }
    queue.step(true);
    crate::fence::wake();
}

#[cfg(test)]
mod tests {
    use super::*;

    type Log = Arc<Mutex<Vec<(u64, u64)>>>;

    /// Lists as (value to wait for at the address, value to write there).
    struct Fake {
        at: Arc<AtomicU64>,
        lists: VecDeque<(u64, u64)>,
        log: Log,
    }

    impl Lists for Fake {
        fn ready(&self) -> usize {
            let value = self.at.load(Ordering::Acquire);
            self.lists.iter().take_while(|l| value >= l.0).count()
        }
        fn len(&self) -> usize {
            self.lists.len()
        }
        fn take(&mut self, n: usize) -> Box<dyn FnOnce()> {
            let run: Vec<_> = self.lists.drain(..n).collect();
            let (at, log) = (self.at.clone(), self.log.clone());
            Box::new(move || {
                for l in run {
                    log.lock().unwrap().push(l);
                    at.fetch_max(l.1, Ordering::AcqRel);
                }
            })
        }
        fn waits(&self) -> String {
            String::new()
        }
    }

    fn op(at: &Arc<AtomicU64>, log: &Log, lists: &[(u64, u64)]) -> Op {
        Op::Lists(Box::new(Fake {
            at: at.clone(),
            lists: lists.iter().copied().collect(),
            log: log.clone(),
        }))
    }

    fn until(what: impl Fn() -> bool) {
        let started = Instant::now();
        while !what() {
            assert!(started.elapsed() < Duration::from_secs(20), "still held back");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Compute submitted before the graphics it waits for, in two parts
    /// that wait for one another.
    #[test]
    fn work_runs_once_its_wait_is_met() {
        let (compute, graphics) = (Timeline::new(), Timeline::new());
        let at = Arc::new(AtomicU64::new(0));
        let log = Log::default();
        compute.submit(op(&at, &log, &[(1, 2), (3, 4)]));
        let done = Arc::new(AtomicBool::new(false));
        let mark = done.clone();
        compute.submit(Op::Work(Box::new(move || mark.store(true, Ordering::Release))));
        assert!(log.lock().unwrap().is_empty() && !compute.idle());
        graphics.submit(op(&at, &log, &[(0, 1), (2, 3)]));
        // The watching thread may run some of it.
        until(|| compute.idle() && graphics.idle());
        let order = log.lock().unwrap().clone();
        assert_eq!(order, [(0, 1), (1, 2), (2, 3), (3, 4)]);
        assert!(done.load(Ordering::Acquire));
    }

    /// A value the CPU writes wakes the queue, and a wait never met does not
    /// hold it back for good.
    #[test]
    fn queues_move_without_another_submission() {
        let queue = Timeline::new();
        let at = Arc::new(AtomicU64::new(0));
        let log = Log::default();
        queue.submit(op(&at, &log, &[(5, 0)]));
        assert!(log.lock().unwrap().is_empty());
        at.store(5, Ordering::Release);
        until(|| log.lock().unwrap().len() == 1);
        queue.submit(op(&at, &log, &[(6, 0)]));
        until(|| log.lock().unwrap().len() == 2);
        assert_eq!(at.load(Ordering::Acquire), 5);
    }
}
