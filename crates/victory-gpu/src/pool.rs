//! Threads kept for work that is cut into pieces (`host::in_parallel`).
//!
//! A thread started for each such piece of work is dear in this process:
//! every thread start and end runs the title's own thread callbacks (its
//! TLS callbacks and its DLLs' `DllMain`), under the loader lock, while the
//! submission's locks are held. These are started once and sleep between
//! their pieces of work.
//!
//! The idea of threads kept for what shaders wrote is from
//! github.com/DexNom/VIctory and github.com/fxceless/victory-fork (`wide`
//! in their shadow.rs); the code here is written anew for closures that
//! borrow from the caller.

use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};

/// Threads kept besides the one that asks.
const KEPT: usize = 5;

/// Whether threads are kept (`VYCE_KEPT_POOL=0`: threads are started for
/// each piece of work, as before).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VYCE_KEPT_POOL").is_none_or(|v| v != "0"))
}

/// Work some threads do together.
struct Job {
    id: u64,
    /// What each of them runs. Borrowed for longer than its lifetime says:
    /// [`Leave`] takes the job off the list, and waits until no kept thread
    /// is in it, before [`join`] returns to the caller that lent it.
    work: *const (dyn Fn() + Sync),
    /// Kept threads that may still come in, and those that are in.
    want: usize,
    inside: usize,
    /// One of them panicked.
    panicked: bool,
}

// SAFETY: the pointer is to a `Sync` closure, alive while the job is listed.
unsafe impl Send for Job {}

struct Pool {
    /// The jobs under way, and the number of the last one.
    jobs: Mutex<(Vec<Job>, u64)>,
    /// Told when a job wants threads.
    more: Condvar,
    /// Told when the last kept thread left a job.
    left: Condvar,
    /// How many threads there are.
    threads: OnceLock<usize>,
}

static POOL: Pool = Pool {
    jobs: Mutex::new((Vec::new(), 0)),
    more: Condvar::new(),
    left: Condvar::new(),
    threads: OnceLock::new(),
};

impl Pool {
    fn jobs(&self) -> MutexGuard<'_, (Vec<Job>, u64)> {
        self.jobs.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The kept threads, started the first time.
    fn threads(&'static self) -> usize {
        *self.threads.get_or_init(|| {
            (0..KEPT)
                .filter(|i| {
                    let started = std::thread::Builder::new()
                        .name(format!("victory-pool-{i}"))
                        .spawn(move || {
                            urgent();
                            self.work()
                        });
                    if let Err(error) = &started {
                        tracing::warn!(%error, "no thread kept for parallel work");
                    }
                    started.is_ok()
                })
                .count()
        })
    }

    /// A kept thread's life.
    fn work(&self) {
        let mut jobs = self.jobs();
        loop {
            let Some(job) = jobs.0.iter_mut().find(|j| j.want > 0) else {
                jobs = self.more.wait(jobs).unwrap_or_else(|e| e.into_inner());
                continue;
            };
            job.want -= 1;
            job.inside += 1;
            let (id, work) = (job.id, job.work);
            drop(jobs);
            // SAFETY: the job stays listed, and what it points to alive,
            // until `inside` is back to zero.
            let ran = catch_unwind(AssertUnwindSafe(|| unsafe { (*work)() }));
            jobs = self.jobs();
            if let Some(job) = jobs.0.iter_mut().find(|j| j.id == id) {
                job.inside -= 1;
                job.panicked |= ran.is_err();
                if job.inside == 0 {
                    self.left.notify_all();
                }
            }
        }
    }
}

/// The calling thread runs before the title's ordinary ones: the submitting
/// thread waits for it.
pub(crate) fn urgent() {
    #[cfg(windows)]
    // SAFETY: a plain call for the calling thread.
    unsafe {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
        };
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
}

/// Takes a job off the list once no kept thread is in it, however the
/// caller's own part of it ended.
struct Leave(u64);

impl Leave {
    /// Whether a kept thread panicked in the job.
    fn leave(&self) -> bool {
        let mut jobs = POOL.jobs();
        loop {
            let Some(at) = jobs.0.iter().position(|j| j.id == self.0) else {
                return false;
            };
            // No more come in.
            jobs.0[at].want = 0;
            if jobs.0[at].inside == 0 {
                return jobs.0.swap_remove(at).panicked;
            }
            jobs = POOL.left.wait(jobs).unwrap_or_else(|e| e.into_inner());
        }
    }
}

impl Drop for Leave {
    fn drop(&mut self) {
        // Nothing to do if `join` left already.
        self.leave();
    }
}

/// Runs `work` on this thread and on up to `extra` of the kept ones at the
/// same time, and returns when all that came have returned from it: `work`
/// takes its pieces in turn, so how many come only changes how long it
/// takes. Several callers may be in here at once; they share the threads.
pub(crate) fn join(extra: usize, work: &(dyn Fn() + Sync)) {
    let extra = if extra == 0 { 0 } else { extra.min(POOL.threads()) };
    if extra == 0 {
        return work();
    }
    // SAFETY: only the lifetime changes; `Leave` below ends every use of the
    // pointer before this function returns or unwinds.
    let lent = unsafe {
        std::mem::transmute::<&(dyn Fn() + Sync), &'static (dyn Fn() + Sync)>(work)
    };
    let leave = {
        let mut jobs = POOL.jobs();
        jobs.1 += 1;
        let id = jobs.1;
        jobs.0.push(Job {
            id,
            work: std::ptr::from_ref(lent),
            want: extra,
            inside: 0,
            panicked: false,
        });
        Leave(id)
    };
    for _ in 0..extra {
        POOL.more.notify_one();
    }
    let ran = catch_unwind(AssertUnwindSafe(work));
    let panicked = leave.leave();
    if let Err(panic) = ran {
        resume_unwind(panic);
    }
    assert!(!panicked, "a kept thread panicked");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// What `host::in_parallel` does with it: pieces taken in turn.
    fn pieces(n: usize, extra: usize, f: impl Fn(usize) + Sync) {
        let next = AtomicUsize::new(0);
        join(extra, &|| {
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
                f(i);
            }
        });
    }

    /// Every piece is done once, and `join` returns only when all are,
    /// however many there are and however few threads come.
    #[test]
    fn every_piece_is_done_before_join_returns() {
        for n in [0usize, 1, 2, 7, 300, 5000] {
            for extra in [0, 1, 3, 64] {
                let done: Vec<AtomicUsize> = (0..n).map(|_| AtomicUsize::new(0)).collect();
                pieces(n, extra, |i| {
                    done[i].fetch_add(1, Ordering::Relaxed);
                });
                let once = done.iter().all(|d| d.load(Ordering::Relaxed) == 1);
                assert!(once, "{n} pieces, {extra} more threads");
            }
        }
    }

    /// Work borrowed from the caller is not touched after `join` returned:
    /// the closure's own memory is taken away at once, again and again.
    #[test]
    fn nothing_lent_is_used_after_join() {
        for round in 0..2000usize {
            let mine = Box::new([round; 64]);
            let sum = AtomicUsize::new(0);
            join(3, &|| {
                sum.fetch_add(mine[round % 64], Ordering::Relaxed);
            });
            let sum = sum.into_inner();
            assert!(sum.is_multiple_of(round.max(1)) && (round..=4 * round).contains(&sum), "{round}: {sum}");
            drop(mine);
        }
    }

    /// Two callers at once, and one inside the other's work: nobody waits
    /// for threads that never come.
    #[test]
    fn callers_share_the_threads() {
        let total = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    for _ in 0..50 {
                        pieces(16, 5, |_| {
                            pieces(4, 5, |_| {
                                total.fetch_add(1, Ordering::Relaxed);
                            });
                        });
                    }
                });
            }
        });
        assert_eq!(total.into_inner(), 4 * 50 * 16 * 4);
    }

    /// A panic in a piece reaches the caller, whichever thread had it, and
    /// the threads go on.
    #[test]
    fn a_panic_reaches_the_caller() {
        for _ in 0..8 {
            let went = catch_unwind(|| {
                pieces(64, 5, |i| {
                    std::thread::sleep(std::time::Duration::from_micros(200));
                    assert_ne!(i, 40, "this panic is the test's own");
                });
            });
            assert!(went.is_err());
        }
        let done = AtomicUsize::new(0);
        pieces(64, 5, |_| {
            done.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(done.into_inner(), 64);
    }

    /// What a piece of work costs on kept threads and on threads started for
    /// it (in the game the latter also run the title's callbacks):
    /// `cargo test --release -p victory-gpu -- --ignored --nocapture pool_against`.
    #[test]
    #[ignore = "benchmark"]
    fn pool_against_started_threads() {
        const ROUNDS: u32 = 1000;
        let work = || {
            std::hint::black_box(0u64);
        };
        join(5, &work);
        let started = std::time::Instant::now();
        for _ in 0..ROUNDS {
            join(5, &work);
        }
        let kept = started.elapsed() / ROUNDS;
        let started = std::time::Instant::now();
        for _ in 0..ROUNDS {
            std::thread::scope(|s| {
                for _ in 0..5 {
                    s.spawn(work);
                }
                work();
            });
        }
        println!("an empty piece of work: {kept:?} kept, {:?} started", started.elapsed() / ROUNDS);
        // 48 pieces of 20 us: a submission's copy-back.
        let spin = |_: usize| {
            let until = std::time::Instant::now() + std::time::Duration::from_micros(20);
            while std::time::Instant::now() < until {
                std::hint::spin_loop();
            }
        };
        let started = std::time::Instant::now();
        for _ in 0..ROUNDS / 10 {
            pieces(48, 5, spin);
        }
        let kept = started.elapsed() / (ROUNDS / 10);
        let started = std::time::Instant::now();
        for _ in 0..ROUNDS / 10 {
            (0..48).for_each(spin);
        }
        let one = started.elapsed() / (ROUNDS / 10);
        println!("48 pieces of 20 us: {kept:?} on six threads, {one:?} on one");
    }
}
