//! How large host copies of the textures the title streams are, by how much
//! video memory is left: all of them whole did not fit, and paging them cost
//! seconds a frame, or the driver failed.
//!
//! The largest side is 1024, 512 and then 256 texels as video memory fills.
//! A texture made when there was little stays at that size only until there
//! is room again: some of them are made again every second
//! ([`crate::resource::care`]), by a thread of their own, as few as the
//! thread that uploads them does not notice.
//!
//! `VICTORY_TEXTURE_MAX=<texels>` sets one size for all (0: no limit).
//! `VICTORY_TEXTURE_CAPS=<a>,<b>`: from `a` percent of video memory in use
//! 512, from `b` percent 256; `0` is as before (70 and 85 of what the driver
//! says is used). `VICTORY_TEXTURE_UPGRADE_MB=<MiB>` a second are made again
//! (16 by default; 0: none, as before). `VICTORY_TEXTURE_SWEEP=0` keeps the
//! copies of textures nothing shows any more, as before.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::memstat;
use crate::resource::{Care, Cared, now_ms};

/// The largest sides of streamed textures, in texels (0: any): of those made
/// now, and of those made again because there is room.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Caps {
    pub(crate) new: u64,
    pub(crate) again: u64,
}

impl Caps {
    const NONE: Caps = Caps { new: 0, again: 0 };
}

/// Video memory, in bytes.
#[derive(Clone, Copy, Default, Debug)]
struct Memory {
    /// What the driver says is used, and may be.
    used: u64,
    budget: u64,
    /// Of the used: textures the host renders to or writes.
    targets: u64,
    /// Such textures nothing has used yet, which take none so far.
    unmade: u64,
    /// Of the used: on its way out, or free in heaps of textures.
    back: u64,
}

impl Memory {
    fn now() -> Option<Memory> {
        let [(used, budget), _] = crate::host::video_memory()?;
        Some(Memory {
            used,
            budget,
            targets: memstat::TARGETS[0].load(Relaxed),
            unmade: memstat::UNMADE[0].load(Relaxed),
            back: memstat::LEAVING.load(Relaxed) + crate::texpool::spare_bytes(),
        })
    }

    /// The percentage in use, as far as textures to come are concerned: room
    /// is kept for some of the targets nothing has used yet, and what is on
    /// its way out counts as free (both within reason: how much of either
    /// comes true is not known).
    fn percent(&self) -> u64 {
        let kept = self.unmade.min(self.budget / 32);
        let back = self.back.min(self.budget / 16);
        (self.used + kept).saturating_sub(back) * 100 / self.budget.max(1)
    }

    /// The percentage the driver says is used, without what is leaving.
    fn plain_percent(&self) -> u64 {
        self.used * 100 / self.budget.max(1)
    }
}

/// Where the sizes change, in percent of video memory.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Limits {
    /// By what the driver says alone.
    AsBefore,
    Set([u64; 2]),
    Tuned,
}

/// Made again larger only where that size is new ones' with so much more
/// percent in use: what is made again does not bring the next size down.
const MARGIN: u64 = 6;

impl Limits {
    /// The limits `VICTORY_TEXTURE_CAPS` names.
    fn parse(set: Option<&str>) -> Limits {
        let set = set.map(str::trim).unwrap_or_default();
        if set == "0" {
            return Limits::AsBefore;
        }
        let parsed = set
            .split_once(',')
            .and_then(|(a, b)| Some([a.trim().parse().ok()?, b.trim().parse().ok()?]));
        parsed.map_or(Limits::Tuned, Limits::Set)
    }

    /// The sizes that go with `m`.
    ///
    /// Tuned for a card of 8 GB (7.1 GB of which are ours) whose targets are
    /// made on first use: with less than 2.5 GB of them, textures may fill it
    /// further than when 4.2 GB of targets were there before any of them.
    fn caps(self, m: &Memory) -> Caps {
        let (limits, percent) = match self {
            Limits::AsBefore => ([70, 85], m.plain_percent()),
            Limits::Set(limits) => (limits, m.percent()),
            Limits::Tuned if m.targets < m.budget / 100 * 36 => ([78, 90], m.percent()),
            Limits::Tuned => ([70, 85], m.percent()),
        };
        let at = |percent: u64| match percent {
            p if p < limits[0] => 1024,
            p if p < limits[1] => 512,
            _ => 256,
        };
        Caps { new: at(percent), again: at(percent + MARGIN) }
    }
}

fn limits() -> Limits {
    static LIMITS: OnceLock<Limits> = OnceLock::new();
    *LIMITS.get_or_init(|| Limits::parse(std::env::var("VICTORY_TEXTURE_CAPS").ok().as_deref()))
}

/// One size for all, if the user set one.
fn fixed() -> Option<u64> {
    static SET: OnceLock<Option<u64>> = OnceLock::new();
    *SET.get_or_init(|| std::env::var("VICTORY_TEXTURE_MAX").ok()?.trim().parse().ok())
}

/// The sizes last worked out, and when ([`now_ms`] and one; 0: never).
static NEW: AtomicU64 = AtomicU64::new(1024);
static AGAIN: AtomicU64 = AtomicU64::new(1024);
static ASKED: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    /// The sizes in this test.
    pub(crate) static FORCED: std::cell::Cell<Caps> = const { std::cell::Cell::new(Caps::NONE) };
}

/// The sizes tests run with: none, unless one sets some.
fn forced() -> Caps {
    #[cfg(test)]
    return FORCED.get();
    #[cfg(not(test))]
    Caps::NONE
}

/// The sizes in force, as of a moment ago.
pub(crate) fn caps() -> Caps {
    if cfg!(test) {
        return forced();
    }
    if ASKED.load(Relaxed) == 0 {
        return caps_now();
    }
    Caps { new: NEW.load(Relaxed), again: AGAIN.load(Relaxed) }
}

/// The sizes in force, the driver asked if that is a little while ago.
pub(crate) fn caps_now() -> Caps {
    const OLD_MS: u64 = 50;
    if cfg!(test) {
        return forced();
    }
    let asked = ASKED.load(Relaxed);
    let now = now_ms() + 1;
    // One thread asks; the others go by what is known.
    if (asked == 0 || now > asked + OLD_MS)
        && ASKED.compare_exchange(asked, now, Relaxed, Relaxed).is_ok()
    {
        let caps = match (fixed(), Memory::now()) {
            (Some(max), _) => Caps { new: max, again: max },
            (None, Some(m)) => limits().caps(&m),
            (None, None) => Caps { new: 1024, again: 1024 },
        };
        NEW.store(caps.new, Relaxed);
        AGAIN.store(caps.again, Relaxed);
    }
    Caps { new: NEW.load(Relaxed), again: AGAIN.load(Relaxed) }
}

/// The size new streamed textures are cut to, for the session panel (0: none).
pub(crate) fn in_force() -> u64 {
    if crate::resource::textures() == crate::resource::Textures::Stream { caps().new } else { 0 }
}

/// Bytes a second made again with more mips.
fn better_bytes() -> u64 {
    static SET: OnceLock<u64> = OnceLock::new();
    *SET.get_or_init(|| {
        let set = std::env::var("VICTORY_TEXTURE_UPGRADE_MB").ok();
        set.and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(16) << 20
    })
}

fn sweeps() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_SWEEP").is_none_or(|v| v != "0"))
}

/// How many are made again at once: each is a texture to detile and upload.
const BETTER_MOST: u64 = 64;

/// Percent of video memory from which textures are let go for smaller ones,
/// and how many bytes of them at once: past the budget the system moves what
/// it picks into the PC's memory.
const FULL: u64 = 97;
const CUT: u64 = 64 << 20;

/// What is to be done about the placed textures, with `m` of memory.
fn plan(m: &Memory, caps: Caps, better: u64, sweeps: bool) -> Care {
    /// Percent from which textures nothing shows are let go, and how long
    /// nothing did.
    const SWEPT: u64 = 60;
    const IDLE_MS: u64 = 30_000;
    let real = m.used.saturating_sub(m.back.min(m.budget / 16)) * 100 / m.budget.max(1);
    Care {
        caps,
        better: if real < FULL { [better, BETTER_MOST] } else { [0, 0] },
        cut: if better > 0 && real >= FULL { CUT } else { 0 },
        idle_ms: (sweeps && real >= SWEPT).then_some(IDLE_MS),
    }
}

/// What [`crate::resource::care`] did since the start (of the capped ones,
/// since they were last logged).
static CARED: Mutex<Cared> =
    Mutex::new(Cared { better: [0; 2], twice: [0; 2], cut: [0; 2], gone: [0; 2], capped: [0; 2] });

/// The host made no texture lately.
static REFUSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The host did not make a texture: it may be out of memory.
pub(crate) fn refused() {
    REFUSED.store(true, Relaxed);
}

/// Textures made again larger, for the session panel.
pub(crate) static BETTER: AtomicU64 = AtomicU64::new(0);

/// Starts the thread that looks after textures, once.
pub(crate) fn start() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    if cfg!(test) {
        return;
    }
    ONCE.call_once(|| {
        let made = std::thread::Builder::new().name("victory-textures".into()).spawn(|| {
            loop {
                std::thread::sleep(Duration::from_secs(1));
                tick();
            }
        });
        if let Err(e) = made {
            tracing::warn!(error = %e, "no thread to look after textures");
        }
    });
}

/// Once a second.
fn tick() {
    crate::list::sweep();
    if better_bytes() == 0 && !sweeps() {
        return;
    }
    let caps = caps_now();
    let (Some(m), Some(host)) = (Memory::now(), crate::host::host()) else {
        return;
    };
    // SAFETY: a plain query.
    if unsafe { host.device.GetDeviceRemovedReason() }.is_err() {
        return;
    }
    let mut what = plan(&m, caps, better_bytes(), sweeps());
    if REFUSED.swap(false, Relaxed) && better_bytes() > 0 {
        // Whatever the driver says is used, there was no memory.
        what = Care { caps: Caps { new: 256, again: 256 }, better: [0, 0], cut: CUT, ..what };
    }
    let cared = crate::resource::care(&what);
    BETTER.fetch_add(cared.better[0], Relaxed);
    let mut all = CARED.lock().unwrap_or_else(|e| e.into_inner());
    let all = &mut *all;
    for (sum, n) in [
        (&mut all.better, cared.better),
        (&mut all.twice, cared.twice),
        (&mut all.cut, cared.cut),
        (&mut all.gone, cared.gone),
        (&mut all.capped, cared.capped),
    ] {
        sum[0] += n[0];
        sum[1] += n[1];
    }
}

/// The textures the host renders to or writes the title created so far, when
/// their number last changed, and when their memory was last logged.
static NOTED: Mutex<(u64, Option<Instant>, Option<Instant>)> = Mutex::new((0, None, None));

/// With the session panel: every second, to log what takes the video memory
/// once the title has created its targets and every minute from then on.
pub(crate) fn note() {
    const FEW: u64 = 64;
    const SETTLED: Duration = Duration::from_secs(10);
    const EVERY: Duration = Duration::from_secs(60);
    let count = memstat::TARGETS[1].load(Relaxed) + memstat::UNMADE[1].load(Relaxed);
    let mut noted = NOTED.lock().unwrap_or_else(|e| e.into_inner());
    if noted.0 != count || noted.1.is_none() {
        noted.0 = count;
        noted.1 = Some(Instant::now());
    }
    let due = match noted.2 {
        None => count >= FEW && noted.1.is_some_and(|at| at.elapsed() >= SETTLED),
        Some(at) => at.elapsed() >= EVERY,
    };
    if due {
        let first = noted.2.replace(Instant::now()).is_none();
        drop(noted);
        log(if first { "after the title created its targets" } else { "now" });
    }
}

/// Logs what takes the video memory.
fn log(when: &str) {
    let mib = |v: u64| v >> 20;
    let load = |c: &[AtomicU64; 2]| [c[0].load(Relaxed), c[1].load(Relaxed)];
    let Some(m) = Memory::now() else { return };
    let caps = caps_now();
    let targets = load(&memstat::TARGETS);
    let unmade = load(&memstat::UNMADE);
    let textures = [&memstat::HOST_TEXTURES, &memstat::HOST_TEXTURE_COUNT].map(|c| c.load(Relaxed));
    let heaps = load(&memstat::BUFFERS[4]);
    let unordered = load(&memstat::BUFFERS[3]);
    let used = crate::resource::targets_used(60_000);
    let cared = {
        let mut all = CARED.lock().unwrap_or_else(|e| e.into_inner());
        let cared = *all;
        all.capped = [0; 2];
        cared
    };
    tracing::info!(
        "video memory {when}: {} of {} MiB ({} %, {} % for streamed textures, which are cut to {} and made again for {}); \
         targets made {} of {} MiB (in the last minute used {} of {} MiB), targets never used {} of {} MiB; \
         streamed {} of {} MiB ({} placed, {} % of them cut), their heaps {} MiB ({} MiB free); leaving {} MiB; \
         unordered buffers {} MiB ({}); so far made larger {} of {} MiB, replaced {} of {} MiB, \
         cut {} of {} MiB, let go {} of {} MiB",
        mib(m.used),
        mib(m.budget),
        m.plain_percent(),
        m.percent(),
        caps.new,
        caps.again,
        targets[1],
        mib(targets[0]),
        used[1],
        mib(used[0]),
        unmade[1],
        mib(unmade[0]),
        textures[1].saturating_sub(targets[1]),
        mib(textures[0].saturating_sub(targets[0])),
        memstat::PLACED_COUNT.load(Relaxed),
        cared.capped[0] * 100 / cared.capped[1].max(1),
        mib(heaps[0]),
        mib(crate::texpool::spare_bytes()),
        mib(memstat::LEAVING.load(Relaxed)),
        mib(unordered[0]),
        unordered[1],
        cared.better[0],
        mib(cared.better[1]),
        cared.twice[0],
        mib(cared.twice[1]),
        cared.cut[0],
        mib(cared.cut[1]),
        cared.gone[0],
        mib(cared.gone[1]),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn memory(used_percent: u64, targets: u64) -> Memory {
        let budget = 7114 << 20;
        Memory { used: budget / 100 * used_percent + (1 << 20), budget, targets, ..Memory::default() }
    }

    /// The sizes by the memory in use: later with few targets than with
    /// many, and those made again sooner back to the next smaller one.
    #[test]
    fn sizes_follow_the_memory_in_use() {
        let new = |p, targets| Limits::Tuned.caps(&memory(p, targets)).new;
        for (percent, size) in [(0, 1024), (77, 1024), (78, 512), (89, 512), (90, 256), (120, 256)] {
            assert_eq!(new(percent, GIB), size, "{percent} % with few targets");
        }
        for (percent, size) in [(69, 1024), (70, 512), (84, 512), (85, 256)] {
            assert_eq!(new(percent, 4 * GIB), size, "{percent} % with many targets");
            assert_eq!(Limits::AsBefore.caps(&memory(percent, GIB)).new, size, "{percent} % as before");
        }
        let again = |p| Limits::Tuned.caps(&memory(p, GIB)).again;
        assert_eq!([71, 72, 83, 84].map(again), [1024, 512, 512, 256]);
        assert_eq!(Limits::Set([50, 60]).caps(&memory(55, GIB)), Caps { new: 512, again: 256 });
    }

    /// Room is kept for targets to come, and what is leaving counts as free,
    /// both to a point.
    #[test]
    fn memory_to_come_and_to_go_counts() {
        let mut m = memory(80, GIB);
        assert_eq!(m.percent(), 80);
        m.unmade = 100 << 20;
        assert_eq!(m.percent(), 81);
        m.unmade = 4 * GIB;
        assert_eq!(m.percent(), 83, "a thirty-second of the budget at most");
        m.unmade = 0;
        m.back = 300 << 20;
        assert_eq!(m.percent(), 75);
        m.back = 4 * GIB;
        assert_eq!(m.percent(), 73, "a sixteenth of the budget at most");
        assert_eq!(m.plain_percent(), 80);
    }

    #[test]
    fn limits_are_read_from_the_setting() {
        assert_eq!(Limits::parse(None), Limits::Tuned);
        assert_eq!(Limits::parse(Some("")), Limits::Tuned);
        assert_eq!(Limits::parse(Some(" 0 ")), Limits::AsBefore);
        assert_eq!(Limits::parse(Some("60, 75")), Limits::Set([60, 75]));
        assert_eq!(Limits::parse(Some("60")), Limits::Tuned);
    }

    /// Textures are cut only when memory is about to run out, and made
    /// larger only before.
    #[test]
    fn little_is_done_with_much_memory_left() {
        let at = |p| plan(&memory(p, GIB), Caps::NONE, 16 << 20, true);
        let (free, some, full) = (at(40), at(80), at(98));
        assert_eq!((free.better, free.cut, free.idle_ms), ([16 << 20, BETTER_MOST], 0, None));
        assert_eq!((some.better, some.cut), ([16 << 20, BETTER_MOST], 0));
        assert!(some.idle_ms.is_some());
        assert_eq!(full.better, [0, 0]);
        assert!(full.cut > 0);
        let mut leaving = memory(98, GIB);
        leaving.back = 200 << 20;
        assert_eq!(plan(&leaving, Caps::NONE, 16 << 20, true).cut, 0, "enough is leaving");
        let off = plan(&memory(98, GIB), Caps::NONE, 0, false);
        assert_eq!((off.better[0], off.cut, off.idle_ms), (0, 0, None));
    }
}
