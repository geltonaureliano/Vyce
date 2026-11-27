//! Submissions sent to the host GPU and not waited for ("flights"), in the
//! order they were sent, and what is left to do once each has run (its
//! landing): what its shaders wrote goes back into title memory, then its
//! read-backs, then its bottom-of-pipe writes, then what the title's queues
//! put behind it (fence signals).
//!
//! All the title's queues run on one host queue, in submission order. So a
//! queue's signal need not wait for the host GPU: it goes behind the newest
//! flight and is done when that one lands, later than before and never
//! earlier. And a wait for a fence, or for a value in memory, that a flight
//! in the air will signal or write is met: what follows it goes to the host
//! queue behind that flight, which is the order the wait asks for. The
//! title's CPU still sees a value or a fence only once the flight has
//! landed.
//!
//! The CPU's part of a submission (the title's copies, refreshes of what
//! shaders may be writing, snapshots) still begins only when every flight
//! before it has landed (`Host::execute_title`): snapshots and the ranges
//! shaders write are kept per host copy, not per submission.
//!
//! Flights land as soon as the host GPU has run them, on a thread of their
//! own, or on a thread that needs it sooner: one that is about to prepare a
//! submission, resets a list, or stands at a fence. One thread lands at a
//! time, in the order of the line, under no lock that a thread waiting for a
//! landing may hold: not the submission's, no queue's, no list's.
//!
//! The rule for signals and waits is that of `late_signal` and `late_met` in
//! github.com/DexNom/VIctory and github.com/fxceless/victory-fork (their
//! host.rs), written anew: here the queues have timelines, nothing is
//! prepared under a flight, and no switch of theirs is needed.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D12::*;

use crate::fence::Held;
use crate::host::HostList;
use crate::queue::Written;
use crate::telemetry::{Count, Gauge, Pair};

/// Whether signals and waits of the title's queues go by the order of the
/// host queue instead of waiting for the host GPU (`VYCE_LATE_SIGNAL=0`:
/// each signal, and each queue held back by a wait, waits for everything in
/// flight, as before).
pub(crate) fn late_signal() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VYCE_LATE_SIGNAL").is_none_or(|v| v != "0"))
}

/// A bottom-of-pipe write: address, what, bytes of it.
pub(crate) type Write = (usize, Written, usize);

/// A range shaders write: host copy, title address, bytes.
pub(crate) type Range = (Arc<crate::shadow::Shadow>, usize, usize);

/// What is done behind a flight, once it has landed.
pub(crate) enum After {
    /// A fence is signalled.
    Signal(Held, u64),
    /// A list's write that comes before any work of the list, while the list
    /// itself waits.
    Write(usize, Written, usize),
}

impl After {
    pub(crate) fn run(self) {
        match self {
            After::Signal(fence, value) => fence.signal(value),
            After::Write(at, what, bytes) => what.write(at as *mut u8, bytes),
        }
    }
}

/// A submission in flight.
pub(crate) struct Flight {
    /// The value signalled behind it on the line's fence.
    pub(crate) fence: u64,
    /// Its number among the submissions started.
    pub(crate) submission: u64,
    /// Its lists, kept alive (and from being recorded anew) until it lands.
    pub(crate) lists: Vec<Arc<HostList>>,
    /// The ranges its shaders write.
    pub(crate) back: Vec<Range>,
    pub(crate) writes: Vec<Write>,
    /// What takes the host GPU's time around it.
    pub(crate) timed: Option<Pair>,
    /// When it was sent.
    pub(crate) sent: Instant,
    /// What its commands use besides.
    pub(crate) _used: Box<dyn std::any::Any>,
    after: Vec<After>,
}

// SAFETY: D3D12 objects are free-threaded, host lists are run and let go of
// by one thread at a time, and the rest is ours and locked.
unsafe impl Send for Flight {}

impl Flight {
    pub(crate) fn new(submission: u64, lists: Vec<Arc<HostList>>) -> Flight {
        Flight {
            fence: 0,
            submission,
            lists,
            back: Vec::new(),
            writes: Vec::new(),
            timed: None,
            sent: Instant::now(),
            _used: Box::new(()),
            after: Vec::new(),
        }
    }
}

/// The flight being landed: what of it is not done yet.
struct Landing {
    fence: u64,
    writes: Vec<Write>,
    after: Vec<After>,
}

#[derive(Default)]
struct State {
    /// In the order sent.
    flights: VecDeque<Flight>,
    landing: Option<Landing>,
    /// When the last one was found to have run.
    ran: Option<Instant>,
}

/// The host queue's flights.
pub(crate) struct Line {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    /// Signalled behind each submission.
    fence: ID3D12Fence,
    state: Mutex<State>,
    /// Told when a flight is sent.
    moved: Condvar,
    /// Held by the thread that lands flights.
    landing: Mutex<()>,
    /// The fence value of the newest flight that has landed.
    landed: AtomicU64,
    /// The host GPU's time around submissions, when asked for.
    pub(crate) clock: OnceLock<Option<crate::telemetry::Clock>>,
    lander: std::sync::Once,
}

// SAFETY: D3D12 devices, queues and fences are free-threaded.
unsafe impl Send for Line {}
// SAFETY: as above; the rest is locked.
unsafe impl Sync for Line {}

thread_local! {
    /// This thread is landing a flight.
    static LANDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// This is the landing thread ([`land_all`]).
    static BEHIND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl Line {
    pub(crate) fn new(device: ID3D12Device, queue: ID3D12CommandQueue, fence: ID3D12Fence) -> Arc<Line> {
        Arc::new(Line {
            device,
            queue,
            fence,
            state: Mutex::default(),
            moved: Condvar::new(),
            landing: Mutex::new(()),
            landed: AtomicU64::new(0),
            clock: OnceLock::new(),
            lander: std::sync::Once::new(),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `flight` joins the line: its lists were given to the host queue, which
    /// signals its fence value behind them. The caller holds the submission's
    /// lock, so flights join in the order of their values.
    pub(crate) fn send(self: &Arc<Self>, flight: Flight) {
        let fence = flight.fence;
        let mut s = self.state();
        // Under the lock: who finds the value reached finds the flight too.
        // SAFETY: our queue and fence.
        if let Err(e) = unsafe { self.queue.Signal(&self.fence, fence) } {
            let runs: Vec<&HostList> = flight.lists.iter().map(|l| &**l).collect();
            self.check(&runs);
            crate::host::fail(format_args!("a host submission could not be followed by a signal ({e})"));
        }
        for list in &flight.lists {
            list.sent(fence);
        }
        s.flights.push_back(flight);
        crate::telemetry::note(Gauge::Flights, s.flights.len() as u64);
        drop(s);
        if late_signal() {
            self.lander.call_once(|| {
                let line = Arc::downgrade(self);
                let started = std::thread::Builder::new()
                    .name("victory-landing".into())
                    .spawn(move || land_all(&line));
                if let Err(e) = started {
                    // Nothing would land a flight the title only looks at.
                    crate::host::fail(format_args!(
                        "no thread to land submissions ({e}); VYCE_LATE_SIGNAL=0 does without"
                    ));
                }
            });
        }
        self.moved.notify_all();
    }

    /// Puts `what` behind everything in flight; it comes back if nothing is.
    pub(crate) fn after(&self, what: After) -> Result<(), After> {
        let mut s = self.state();
        let State { flights, landing, .. } = &mut *s;
        match (flights.back_mut(), landing) {
            (Some(flight), _) => flight.after.push(what),
            (None, Some(landing)) => landing.after.push(what),
            (None, None) => return Err(what),
        }
        Ok(())
    }

    /// Whether `fence` reaches `value` once everything in flight has landed.
    pub(crate) fn signals(&self, fence: &Held, value: u64) -> bool {
        {
            let s = self.state();
            let landing = s.landing.iter().flat_map(|l| &l.after);
            let mut after = s.flights.iter().flat_map(|f| &f.after).chain(landing);
            if after.any(|a| matches!(a, After::Signal(f, v) if *v >= value && f.same(fence))) {
                return true;
            }
        }
        // The fence's value is memory, which lists write too.
        self.value(fence.address(), 8).is_some_and(|v| v >= value)
    }

    /// The value the `bytes` at `address` have once everything in flight has
    /// landed, if that is one of the flights' bottom-of-pipe writes; `None`
    /// if none writes there, or not in a way known before it has run.
    pub(crate) fn value(&self, address: usize, bytes: usize) -> Option<u64> {
        let s = self.state();
        // The newest write there.
        let newest = |writes: &[Write], after: &[After]| -> Option<Option<u64>> {
            let look = |&(at, what, n): &Write| {
                if at >= address + bytes || address >= at + n {
                    return None;
                }
                Some(match what {
                    Written::Value(v) if at == address && n >= bytes => {
                        Some(if bytes == 4 { v & u64::from(u32::MAX) } else { v })
                    }
                    _ => None,
                })
            };
            after
                .iter()
                .rev()
                .find_map(|a| match a {
                    After::Write(at, what, n) => look(&(*at, *what, *n)),
                    After::Signal(..) => None,
                })
                .or_else(|| writes.iter().rev().find_map(look))
        };
        s.flights
            .iter()
            .rev()
            .find_map(|f| newest(&f.writes, &f.after))
            .or_else(|| s.landing.as_ref().and_then(|l| newest(&l.writes, &l.after)))
            .flatten()
    }

    /// Whether a flight is still to land (or being landed).
    pub(crate) fn busy(&self) -> bool {
        let s = self.state();
        !s.flights.is_empty() || s.landing.is_some()
    }

    /// Waits until the flight with fence value `target` has landed. The time
    /// counts for `why`, and the wait among those of its kind
    /// ([`crate::telemetry::drains_of`]).
    pub(crate) fn wait(&self, target: u64, why: Count) {
        if target == 0 || self.landed.load(Ordering::Acquire) >= target {
            return;
        }
        if let Some(kind) = crate::telemetry::drains_of(why) {
            crate::telemetry::count(kind, 1);
        }
        let began = Instant::now();
        let mut standing = Duration::ZERO;
        loop {
            let stood = Instant::now();
            self.reached(target);
            standing += stood.elapsed();
            self.land();
            // From inside a landing (a list let go of by it): the host GPU is
            // done with the list, the rest is not waited for.
            if self.landed.load(Ordering::Acquire) >= target || LANDING.get() {
                break;
            }
            std::thread::yield_now();
        }
        crate::host::waited(standing.as_micros() as u64);
        crate::telemetry::count(why, began.elapsed().as_micros() as u64);
    }

    /// Waits until everything in flight at the call has landed: not for what
    /// other threads send meanwhile, which a thread that keeps submitting
    /// would make a wait without end. Inside a landing it returns at once:
    /// nothing behind the flight being landed can land before it.
    pub(crate) fn drain(&self, why: Count) {
        if LANDING.get() {
            return;
        }
        let newest = {
            let s = self.state();
            s.flights.back().map(|f| f.fence).or(s.landing.as_ref().map(|l| l.fence))
        };
        if let Some(target) = newest {
            // Flights land in the order of their values.
            self.wait(target, why);
        }
    }

    /// Blocks until the host GPU has reached `target`.
    fn reached(&self, target: u64) {
        // SAFETY: our fence; without an event the call itself blocks, on any
        // thread.
        unsafe {
            if self.fence.GetCompletedValue() < target
                && let Err(e) = self.fence.SetEventOnCompletion(target, HANDLE::default())
            {
                tracing::warn!(error = %e, "host fence wait failed");
                // Not a loop at full speed around a wait that fails.
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    /// Ends the run if the device is gone; `runs` ran last.
    fn check(&self, runs: &[&HostList]) {
        // SAFETY: a plain query.
        if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
            crate::host::removed(&self.device, runs);
        }
    }

    /// Lands the flights that have run, oldest first; how many that were.
    pub(crate) fn land(&self) -> usize {
        // A list dropped by a landing waits for flights that ran before.
        if LANDING.get() {
            return 0;
        }
        struct Here;
        impl Drop for Here {
            fn drop(&mut self) {
                LANDING.set(false);
                // A landing that did not end leaves its signals and values
                // undone: whoever waits for them would wait for good.
                if std::thread::panicking() {
                    crate::host::fail(format_args!(
                        "landing a host submission panicked: what it was to tell the title never comes"
                    ));
                }
            }
        }
        let _one = self.landing.lock().unwrap_or_else(|e| e.into_inner());
        LANDING.set(true);
        let _here = Here;
        let (mut count, mut last) = (0, 0);
        loop {
            // SAFETY: a plain query.
            let completed = unsafe { self.fence.GetCompletedValue() };
            let (flight, took) = {
                let mut s = self.state();
                if s.flights.front().is_none_or(|f| f.fence > completed) {
                    break;
                }
                let Some(mut flight) = s.flights.pop_front() else { break };
                s.landing = Some(Landing {
                    fence: flight.fence,
                    writes: flight.writes.clone(),
                    after: std::mem::take(&mut flight.after),
                });
                // Behind the one before, and no longer than since then.
                let now = Instant::now();
                let began = s.ran.replace(now).map_or(flight.sent, |ran| ran.max(flight.sent));
                (flight, now - began)
            };
            last = last.max(flight.submission);
            count += 1;
            let runs: Vec<&HostList> = flight.lists.iter().map(|l| &**l).collect();
            self.check(&runs);
            if let (Some(pair), Some(Some(clock))) = (flight.timed, self.clock.get()) {
                clock.ran(pair);
            }
            crate::host::ran(runs.len(), took);
            crate::host::finish(&self.queue, &runs, &flight.back, &flight.writes);
            {
                // Then what was put behind it, under the lock: who looks
                // finds a signal still to come, or the fence at its value.
                let mut s = self.state();
                if let Some(landing) = s.landing.take() {
                    landing.after.into_iter().for_each(After::run);
                }
                // Before its lists go: one that goes with it waits for this
                // value.
                self.landed.fetch_max(flight.fence, Ordering::AcqRel);
            }
            drop(runs);
            drop(flight);
        }
        if count > 0 {
            crate::host::finished(last);
            // Values written into fences' memory.
            crate::fence::wake();
            crate::telemetry::count(Count::Landed, count as u64);
            if !BEHIND.get() {
                crate::telemetry::count(Count::LandedHere, count as u64);
            }
        }
        count
    }
}

/// The landing thread: lands each flight when the host GPU has run it. It
/// runs nothing of the queues': what they hold is looked at by whoever
/// submits next, and all the time by a thread of theirs
/// (`crate::timeline`). Ends with its line (tests').
fn land_all(line: &Weak<Line>) {
    crate::pool::urgent();
    BEHIND.set(true);
    loop {
        let Some(line) = line.upgrade() else { return };
        let next = {
            let s = line.state();
            match s.flights.front() {
                Some(f) => Some(f.fence),
                None => {
                    let rest = Duration::from_secs(1);
                    let (s, _) =
                        line.moved.wait_timeout(s, rest).unwrap_or_else(|e| e.into_inner());
                    s.flights.front().map(|f| f.fence)
                }
            }
        };
        let Some(target) = next else { continue };
        line.reached(target);
        line.land();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Host;
    use crate::memwait::Wait;
    use crate::object::{This, slot};
    use crate::timeline::{Lists, Op, Timeline};
    use std::sync::atomic::AtomicBool;

    type Memory = &'static [AtomicU64; 4];

    /// A software host of the test's own, and title memory.
    fn host() -> Option<(&'static Host, Memory)> {
        if !late_signal() {
            return None;
        }
        let host = Host::new_warp()?.overlapping();
        Some((host, Box::leak(Box::default())))
    }

    fn at(memory: Memory, i: usize) -> usize {
        (&raw const memory[i]) as usize
    }

    fn read(memory: Memory) -> [u64; 4] {
        [0, 1, 2, 3].map(|i| memory[i].load(Ordering::Acquire))
    }

    /// Opens a gate of the host queue, at the test's end at the latest.
    struct Gate(ID3D12Fence);
    impl Gate {
        fn open(&self) {
            // SAFETY: our fence.
            unsafe { self.0.Signal(1).unwrap() };
        }
    }
    impl Drop for Gate {
        fn drop(&mut self) {
            self.open();
        }
    }

    /// A closed list that waits for values, then does work and writes.
    fn list(host: &Host, waits: &[(usize, u64)], writes: &[(usize, u64)]) -> Arc<HostList> {
        let list = host.new_list().unwrap();
        for &(address, reference) in waits {
            list.note_wait(Wait { address, comparison: 7, reference, bytes: 8 });
        }
        list.count_work();
        for &(at, value) in writes {
            list.defer_write(at as *mut u8, Written::Value(value), 8);
        }
        list.close();
        Arc::new(list)
    }

    struct Runs(&'static Host, VecDeque<Arc<HostList>>);
    impl Lists for Runs {
        fn ready(&self) -> usize {
            self.1.iter().take_while(|l| l.waits_met(Some(self.0.line()))).count()
        }
        fn len(&self) -> usize {
            self.1.len()
        }
        fn early(&mut self) -> bool {
            self.1.front().is_some_and(|l| l.write_early(Some(self.0.line())))
        }
        fn take(&mut self, n: usize) -> Box<dyn FnOnce()> {
            let (host, runs) = (self.0, self.1.drain(..n).collect::<Vec<_>>());
            Box::new(move || host.execute_title(&runs))
        }
        fn waits(&self) -> String {
            String::new()
        }
    }

    fn submission(host: &'static Host, lists: Vec<Arc<HostList>>) -> Op {
        Op::Lists(Box::new(Runs(host, lists.into())))
    }

    /// A fence of the title's (its address).
    fn fence() -> usize {
        let mut f = std::ptr::null();
        let iid = &crate::fence::FENCE_IIDS[0];
        assert_eq!(crate::fence::create(std::ptr::null(), 0, 0, iid, &mut f), 0);
        f as usize
    }

    fn held(fence: usize) -> Held {
        Held::new(fence as This)
    }

    fn completed(fence: usize) -> u64 {
        slot::<extern "system" fn(This) -> u64>(fence as This, 0x40)(fence as This)
    }

    fn until(what: impl Fn() -> bool) {
        let started = Instant::now();
        while !what() {
            assert!(started.elapsed() < Duration::from_secs(20), "never came");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// A list with a bottom-of-pipe write, a signal behind it, and a second
    /// queue that waits for both and goes on: nothing waits for the host GPU,
    /// nothing is told before it has run, and then everything in order.
    #[test]
    fn signals_and_waits_go_by_the_order_of_the_host_queue() {
        use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
        let Some((host, memory)) = host() else { return };
        let line = host.line();
        let gate = Gate(host.gate());
        let (one, two) = (Timeline::on(line), Timeline::on(line));
        let (f, g) = (fence(), fence());
        // SAFETY: a plain auto-reset event.
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        let set = |ms| {
            // SAFETY: our event.
            unsafe { WaitForSingleObject(event, ms) }
        };

        one.submit(submission(host, vec![list(host, &[], &[(at(memory, 0), 5)])]));
        one.submit(Op::Signal(held(f), 1));
        // (Another test's thread may be the one that runs it.)
        until(|| one.idle());
        assert!(line.busy());
        // What is on its way is known, and only what is exactly known.
        assert_eq!(line.value(at(memory, 0), 8), Some(5));
        assert_eq!(line.value(at(memory, 0), 4), Some(5));
        assert_eq!(line.value(at(memory, 0) + 4, 4), None);
        assert_eq!(line.value(at(memory, 1), 8), None);
        assert!(line.signals(&held(f), 1) && !line.signals(&held(f), 2) && !line.signals(&held(g), 1));
        let on_completion =
            slot::<extern "system" fn(This, u64, *mut std::ffi::c_void) -> i32>(f as This, 0x48);
        assert_eq!(on_completion(f as This, 1, event), 0);

        // The second queue passes its fence wait and its memory wait; its
        // list is prepared when the first has landed.
        let met = list(host, &[(at(memory, 0), 5)], &[(at(memory, 1), 7)]);
        assert!(met.waits_met(Some(line)) && !met.waits_met(None));
        let other = std::thread::spawn(move || {
            two.submit(Op::Wait(held(f), 1));
            two.submit(submission(host, vec![met]));
            two.submit(Op::Signal(held(g), 3));
            until(|| two.idle());
        });
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!((read(memory), completed(f), completed(g)), ([0; 4], 0, 0), "told before it ran");
        assert_eq!(set(0), WAIT_TIMEOUT);

        gate.open();
        other.join().unwrap();
        // The title's threads only look; the landing thread does the rest.
        until(|| completed(g) == 3);
        assert_eq!((read(memory), completed(f)), ([5, 7, 0, 0], 1));
        assert_eq!(set(0), WAIT_OBJECT_0);
        until(|| !line.busy());
        // Nothing in flight: at once, as ever.
        one.submit(Op::Signal(held(f), 2));
        assert_eq!(completed(f), 2);
    }

    /// A queue at a wait nothing in flight meets is held, whatever is in
    /// flight, and goes on when the value comes.
    #[test]
    fn a_wait_nothing_in_flight_meets_holds_its_queue() {
        let Some((host, memory)) = host() else { return };
        let line = host.line();
        let gate = Gate(host.gate());
        let queue = Timeline::on(line);
        queue.submit(submission(host, vec![list(host, &[], &[(at(memory, 0), 1)])]));
        until(|| queue.idle());
        let waits = list(host, &[(at(memory, 0), 1), (at(memory, 2), 9)], &[(at(memory, 1), 4)]);
        assert!(!waits.waits_met(Some(line)));
        queue.submit(submission(host, vec![waits]));
        assert!(!queue.idle(), "held");
        gate.open();
        until(|| !line.busy());
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(read(memory), [1, 0, 0, 0]);
        assert!(!queue.idle(), "held still");
        memory[2].store(9, Ordering::Release);
        until(|| memory[1].load(Ordering::Acquire) == 4);
        until(|| queue.idle() && !line.busy());
    }

    /// A list's writes before its first wait come after the queue's earlier
    /// work, which is in flight, and another queue's wait for them is met.
    #[test]
    fn early_writes_stay_behind_earlier_work() {
        let Some((host, memory)) = host() else { return };
        let line = host.line();
        let gate = Gate(host.gate());
        host.execute_title(&[list(host, &[], &[(at(memory, 0), 1)])]);
        let meets = host.new_list().unwrap();
        meets.defer_write(at(memory, 1) as *mut u8, Written::Value(2), 8);
        meets.note_wait(Wait { address: at(memory, 2), comparison: 7, reference: 1, bytes: 8 });
        meets.count_work();
        meets.defer_write(at(memory, 3) as *mut u8, Written::Value(4), 8);
        meets.close();
        let meets = Arc::new(meets);
        assert!(!meets.waits_met(Some(line)));
        assert!(meets.write_early(Some(line)) && !meets.write_early(Some(line)));
        assert_eq!(read(memory), [0; 4], "before the work it follows");
        assert_eq!(line.value(at(memory, 1), 8), Some(2));
        assert!(list(host, &[(at(memory, 1), 2)], &[]).waits_met(Some(line)));
        gate.open();
        line.drain(Count::WaitHeld);
        assert_eq!(read(memory), [1, 2, 0, 0]);
        // Met, it writes the rest, and nothing twice.
        memory[1].store(0, Ordering::Release);
        memory[2].store(1, Ordering::Release);
        assert!(meets.waits_met(Some(line)));
        host.execute_title(&[meets]);
        line.drain(Count::WaitHeld);
        assert_eq!(read(memory), [1, 0, 1, 4]);
    }

    /// Of several flights the newest write of a value counts, and of what
    /// follows a flight that comes after its own.
    #[test]
    fn the_newest_write_in_flight_counts() {
        let Some((host, memory)) = host() else { return };
        let line = host.line();
        let gate = Gate(host.gate());
        let (x, y, z) = (at(memory, 0), at(memory, 1), at(memory, 2));
        let first = list(host, &[], &[(x, 1), (z, 0x1_0000_0009)]);
        let second = host.new_list().unwrap();
        second.defer_write(x as *mut u8, Written::Value(2), 8);
        second.defer_write(y as *mut u8, Written::Value(9), 4);
        second.defer_write(z as *mut u8, Written::GpuTimestamp, 8);
        second.close();
        host.send_behind(&first);
        assert_eq!((line.value(x, 8), line.value(z, 4), line.value(y, 4)), (Some(1), Some(9), None));
        host.send_behind(&Arc::new(second));
        assert_eq!(line.value(x, 8), Some(2));
        assert_eq!((line.value(y, 4), line.value(y, 8)), (Some(9), None), "four bytes of eight");
        assert_eq!(line.value(z, 8), None, "a time is not known before");
        assert!(line.after(After::Write(x, Written::Value(3), 8)).is_ok());
        assert_eq!(line.value(x, 8), Some(3));
        assert_eq!(line.value(x + 8 * 4, 8), None);
        gate.open();
        line.drain(Count::WaitHeld);
        let [a, b, c, _] = read(memory);
        assert!((a, b) == (3, 9) && c != 0x1_0000_0009);
        assert_eq!(line.value(x, 8), None);
        let After::Write(..) = line.after(After::Write(x, Written::Value(4), 8)).unwrap_err() else {
            panic!("nothing is in flight")
        };
    }

    /// A list is not recorded anew under its flight, and ours that are not
    /// waited for land like the title's.
    #[test]
    fn a_list_in_flight_is_waited_for_before_its_reset() {
        let Some((host, memory)) = host() else { return };
        let line = host.line();
        let gate = Gate(host.gate());
        let ours = list(host, &[], &[]);
        host.execute_title(&[list(host, &[], &[(at(memory, 0), 1)])]);
        host.send_behind(&ours);
        static RESET: AtomicBool = AtomicBool::new(false);
        let again = ours.clone();
        let other = std::thread::spawn(move || {
            again.reset();
            RESET.store(true, Ordering::Release);
            again.close();
        });
        std::thread::sleep(Duration::from_millis(50));
        assert!(!RESET.load(Ordering::Acquire), "reset under the host GPU");
        gate.open();
        other.join().unwrap();
        assert!(RESET.load(Ordering::Acquire));
        assert_eq!(memory[0].load(Ordering::Acquire), 1);
        line.drain(Count::WaitList);
        until(|| Arc::strong_count(&ours) == 1);
    }

    /// Submissions and signals by one thread, waits of all kinds by others:
    /// a value never goes back, a fence never comes before the value of the
    /// work it stands behind, and everything comes.
    #[test]
    fn landings_keep_their_order_whoever_does_them() {
        if let Some((host, memory)) = host() {
            in_order(host, memory);
        }
        if let Some(host) = Host::new_hardware() {
            in_order(host.overlapping(), Box::leak(Box::default()));
        }
    }

    fn in_order(host: &'static Host, memory: Memory) {
        const N: u64 = 400;
        let line = host.line();
        let f = fence();
        let done = AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                let mut last = 0;
                while last < N {
                    let told = completed(f);
                    let value = memory[0].load(Ordering::Acquire);
                    assert!(value >= told, "the fence says {told}, the work before it {value}");
                    assert!(value >= last, "{value} after {last}");
                    last = value;
                    std::hint::spin_loop();
                }
            });
            for k in 0..2u64 {
                s.spawn(move || {
                    let mut target = k;
                    while completed(f) < N {
                        line.wait(target, Count::WaitFence);
                        if k == 1 {
                            line.drain(Count::WaitHeld);
                        }
                        // Only what was sent: others never come.
                        target = line.landed.load(Ordering::Acquire);
                        std::thread::yield_now();
                    }
                });
            }
            let mut old: VecDeque<Arc<HostList>> = VecDeque::new();
            for i in 1..=N {
                let next = list(host, &[], &[(at(memory, 0), i)]);
                host.execute_title(&[next.clone()]);
                // As a queue's signal does.
                line.after(After::Signal(held(f), i)).unwrap_or_else(After::run);
                if i.is_multiple_of(5) {
                    host.send_behind(&list(host, &[], &[]));
                }
                old.push_back(next);
                if old.len() > 3 {
                    let list = old.pop_front().unwrap();
                    list.reset();
                    list.close();
                }
            }
            until(|| completed(f) == N);
            done.store(true, Ordering::Release);
        });
        line.drain(Count::WaitList);
        assert_eq!((memory[0].load(Ordering::Acquire), completed(f)), (N, N));
    }

    /// A drain waits for what was in flight when it began, not for what is
    /// sent afterwards: behind a second gate a later flight stays in the
    /// air, and the drain returns all the same. On WARP and on the machine's
    /// own GPU.
    #[test]
    fn a_drain_does_not_wait_for_flights_sent_after_it_began() {
        if let Some((host, memory)) = host() {
            drain_alone(host, memory);
        }
        if late_signal()
            && let Some(host) = Host::new_hardware()
        {
            drain_alone(host.overlapping(), Box::leak(Box::default()));
        }
    }

    fn drain_alone(host: &'static Host, memory: Memory) {
        let line = host.line();
        let first_gate = Gate(host.gate());
        host.execute_title(&[list(host, &[], &[(at(memory, 0), 1)])]);
        let drained = Arc::new(AtomicBool::new(false));
        let drainer = {
            let (line, drained) = (line.clone(), drained.clone());
            std::thread::spawn(move || {
                line.drain(Count::WaitFence);
                drained.store(true, Ordering::Release);
            })
        };
        // The drain has taken its target and stands at the host GPU.
        std::thread::sleep(Duration::from_millis(150));
        assert!(!drained.load(Ordering::Acquire), "drained before the host GPU ran");
        // (A submission of the title's would wait for the first to land
        // before it is prepared.)
        let second_gate = Gate(host.gate());
        host.send_behind(&list(host, &[], &[(at(memory, 1), 2)]));
        first_gate.open();
        until(|| drained.load(Ordering::Acquire));
        drainer.join().unwrap();
        assert_eq!(memory[0].load(Ordering::Acquire), 1, "the first flight landed");
        assert!(line.busy() && memory[1].load(Ordering::Acquire) == 0, "the second is still in the air");
        second_gate.open();
        line.drain(Count::WaitFence);
        assert_eq!(read(memory)[..2], [1, 2]);
    }

    /// Inside a landing (a list it lets go of, say) a drain cannot wait for
    /// what lies behind the flight being landed: it returns at once.
    #[test]
    fn a_drain_inside_a_landing_returns_at_once() {
        let Some((host, memory)) = host() else { return };
        let line = host.line();
        let gate = Gate(host.gate());
        host.execute_title(&[list(host, &[], &[(at(memory, 0), 1)])]);
        LANDING.set(true);
        let started = Instant::now();
        line.drain(Count::WaitList);
        LANDING.set(false);
        assert!(started.elapsed() < Duration::from_secs(5) && line.busy());
        gate.open();
        line.drain(Count::WaitList);
        assert_eq!(memory[0].load(Ordering::Acquire), 1);
    }

    /// Work of a queue that needs everything finished, behind a flight, runs
    /// on the thread that submits it once that flight has landed, and finds
    /// what the flight left: the queue's thread lands it itself if it has to
    /// (it holds no lock the landing needs).
    #[test]
    fn work_behind_a_flight_runs_once_it_has_landed() {
        let Some((host, memory)) = host() else { return };
        let line = host.line();
        let gate = Gate(host.gate());
        let queue = Timeline::on(line);
        queue.submit(submission(host, vec![list(host, &[], &[(at(memory, 0), 6)])]));
        until(|| queue.idle());
        let seen = Arc::new(AtomicU64::new(0));
        let worker = {
            let (queue, seen) = (queue.clone(), seen.clone());
            std::thread::spawn(move || {
                queue.submit(Op::Work(Box::new(move || {
                    seen.store(1 + memory[0].load(Ordering::Acquire), Ordering::Release);
                })));
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(seen.load(Ordering::Acquire), 0, "ran under the host GPU");
        gate.open();
        // (Another test's thread may be the one that runs it.)
        until(|| seen.load(Ordering::Acquire) != 0);
        assert_eq!(seen.load(Ordering::Acquire), 7, "ran before the value it follows");
        worker.join().unwrap();
        until(|| queue.idle() && !line.busy());
    }

    /// Shaders' writes go into title memory when their flight lands, before
    /// the value that tells of them, and not over what the title wrote.
    #[test]
    fn what_shaders_wrote_comes_before_the_value_that_tells_of_it() {
        let Some((host, memory)) = host() else { return };
        let line = host.line();
        let mut title = vec![7u8; 4 << 16];
        let base = (title.as_mut_ptr() as usize).next_multiple_of(1 << 16);
        let (shadow, copy) = crate::shadow::Shadow::for_test(host, base, 2 << 16);
        let gate = Gate(host.gate());
        let written = list(host, &[], &[(at(memory, 0), 1)]);
        written.defer_gpu_write(&shadow, base + 16, 1 << 16);
        host.execute_title(&[written]);
        // The host GPU and the title's CPU write meanwhile.
        // SAFETY: the copy's mapping; the test's own memory.
        unsafe {
            (copy as *mut u8).add(100).write(13);
            ((base + 101) as *mut u8).write(29);
            assert_eq!(((base + 100) as *const u8).read_volatile(), 7);
        }
        assert_eq!(shadow.update(base + 64, 64), 0, "left to the shaders");
        gate.open();
        until(|| memory[0].load(Ordering::Acquire) == 1);
        // SAFETY: the test's own memory.
        let now = |at: usize| unsafe { ((base + at) as *const u8).read_volatile() };
        assert_eq!([now(100), now(101)], [13, 29]);
        line.drain(Count::WaitHeld);
        drop(title);
    }
}
