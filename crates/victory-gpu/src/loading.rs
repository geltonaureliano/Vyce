//! How far a load has come, for the picture and the session panel
//! (`VYCE_LOADING=0` draws nothing over the picture).
//!
//! The title shows its loading screen for minutes and says nothing. What it
//! does meanwhile is read files. So a load is followed along one like it
//! before, kept in the cache folder (`loads.txt`), by two guesses: the
//! clock, and where that load was when it had read as much. The history
//! says which of the two has been right more often. Without a history the
//! clock goes along the length of the first runs seen, and what is shown
//! says that it is a guess.
//!
//! A thread of ours looks at counters twice a second, so it goes on while
//! the title presents nothing (frames of seconds, a frozen screen). The
//! title's own threads only add to relaxed atomics.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// What the title reads, counted where it is read and never reset.
static REQUESTED: AtomicU64 = AtomicU64::new(0);
static FINISHED: AtomicU64 = AtomicU64::new(0);
static GIVEN_UP: AtomicU64 = AtomicU64::new(0);
static READS: AtomicU64 = AtomicU64::new(0);
static READ: AtomicU64 = AtomicU64::new(0);
static WRITTEN: AtomicU64 = AtomicU64::new(0);
/// Nanoseconds workers waited for files.
static WAITED: AtomicU64 = AtomicU64::new(0);
/// Files opened, again or not.
static FILES: AtomicU64 = AtomicU64::new(0);

/// The title queued a read.
pub fn requested() {
    REQUESTED.fetch_add(1, Relaxed);
}

/// `n` queued reads will never run (their queue was closed).
pub fn given_up(n: u64) {
    GIVEN_UP.fetch_add(n, Relaxed);
}

/// A queued read is done, for `written` bytes.
pub fn done(written: u64) {
    WRITTEN.fetch_add(written, Relaxed);
    FINISHED.fetch_add(1, Relaxed);
}

/// The title read `bytes` of a file without a queue (`ReadFile`).
pub fn read_plainly(bytes: u64) {
    READ.fetch_add(bytes, Relaxed);
    READS.fetch_add(1, Relaxed);
}

/// The title read `bytes` of a file through the C library's buffer
/// (`fread`): a few at a time are no read of the disk's.
pub fn read_buffered(bytes: u64) {
    const BUFFER: u64 = 4096;
    READ.fetch_add(bytes, Relaxed);
    if bytes >= BUFFER {
        READS.fetch_add(1, Relaxed);
    }
}

/// A file opened for queued reads, and what was read of it.
pub struct FileReads {
    name: String,
    reads: AtomicU64,
    bytes: AtomicU64,
}

impl FileReads {
    const fn new(name: String) -> FileReads {
        FileReads {
            name,
            reads: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        }
    }

    /// How many reads of it there were, and of how many bytes.
    pub fn so_far(&self) -> (u64, u64) {
        (self.reads.load(Relaxed), self.bytes.load(Relaxed))
    }

    /// A worker read `bytes` of it and waited `took` for them.
    pub fn read(&self, bytes: u64, took: Duration) {
        self.reads.fetch_add(1, Relaxed);
        self.bytes.fetch_add(bytes, Relaxed);
        READ.fetch_add(bytes, Relaxed);
        READS.fetch_add(1, Relaxed);
        WAITED.fetch_add(took.as_nanos() as u64, Relaxed);
    }
}

/// Every file opened so far, never let go (the title opens about a hundred).
static OPENED: Mutex<Vec<&'static FileReads>> = Mutex::new(Vec::new());
/// Files told apart; what is past them counts as one.
const FILES_MAX: usize = 1024;
static OTHERS: FileReads = FileReads::new(String::new());

/// The title opened a file for queued reads.
pub fn opened(name: &str) -> &'static FileReads {
    FILES.fetch_add(1, Relaxed);
    let mut all = OPENED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&known) = all.iter().find(|f| f.name == name) {
        return known;
    }
    if all.len() >= FILES_MAX {
        return &OTHERS;
    }
    let file: &'static FileReads = Box::leak(Box::new(FileReads::new(name.to_owned())));
    all.push(file);
    file
}

/// Name, reads and bytes of files.
type PerFile = Vec<(&'static str, u64, u64)>;

/// What was read of every file so far.
fn files() -> PerFile {
    let all = OPENED.lock().unwrap_or_else(|e| e.into_inner()).clone();
    all.into_iter()
        .map(|f| (f.name.as_str(), f.so_far().0, f.so_far().1))
        .collect()
}

/// The files most was read of since `before`: what a faster disk is worth
/// most for.
fn most_read(before: &PerFile) -> String {
    let mut now = files();
    for f in &mut now {
        if let Some(b) = before.iter().find(|b| b.0 == f.0) {
            (f.1, f.2) = (f.1.saturating_sub(b.1), f.2.saturating_sub(b.2));
        }
    }
    now.sort_unstable_by_key(|f| std::cmp::Reverse(f.2));
    let lines: Vec<String> = now
        .iter()
        .take(8)
        .filter(|f| f.1 > 0)
        .map(|(name, reads, bytes)| format!("{name} {reads}x {} MiB", bytes >> 20))
        .collect();
    lines.join(", ")
}

/// The counters at one moment. All but `queued`, `waiting` and `cold` only
/// grow.
#[derive(Clone, Copy, Debug, Default)]
struct Signals {
    /// Frames presented.
    frames: u64,
    /// Draws run; draws, dispatches and indirect executions dropped.
    work: u64,
    /// Reads of files, bytes taken from them, bytes written of queued reads,
    /// time waited for files (ns).
    reads: u64,
    bytes: u64,
    written: u64,
    waited: u64,
    /// Reads queued and not done.
    queued: u64,
    /// Files opened.
    files: u64,
    /// Host pipelines waited for, and done with.
    waiting: u64,
    pipelines: u64,
    shaders: u64,
    /// Most shaders or pipelines are made anew (a new build or driver).
    cold: bool,
}

impl Signals {
    fn now() -> Signals {
        let get = |c: &AtomicU64| c.load(Relaxed);
        let done = get(&FINISHED);
        let (hits, misses) = crate::shader_cache::counts();
        Signals {
            frames: crate::queue::presented(),
            work: crate::draw::drawn()
                + crate::draw::dropped_so_far()
                + crate::list::indirect_so_far(),
            reads: get(&READS),
            bytes: get(&READ),
            written: get(&WRITTEN),
            waited: get(&WAITED),
            queued: get(&REQUESTED).saturating_sub(done + get(&GIVEN_UP)),
            files: get(&FILES),
            waiting: crate::pipeline::making(),
            pipelines: crate::pipeline::finished(),
            shaders: hits + misses,
            cold: misses > hits || crate::pipeline::cold(),
        }
    }

    /// What the reads so far cost a disk, in KiB: a hard disk reads a few
    /// hundred in the time of a seek. The same reads give the same sum on
    /// any disk, which is all that is asked of it.
    fn cost(&self) -> u64 {
        self.reads * 256 + (self.bytes >> 10)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// The process's start to the title's first menu.
    Start,
    /// The first loading screen to the world.
    World,
    /// A later loading screen (a place travelled to).
    Again,
}

impl Kind {
    const ALL: [Kind; 3] = [Kind::Start, Kind::World, Kind::Again];

    /// Its name in the history.
    fn name(self) -> &'static str {
        match self {
            Kind::Start => "start",
            Kind::World => "world",
            Kind::Again => "again",
        }
    }

    /// Its name in the session panel's line.
    fn stage(self) -> &'static str {
        match self {
            Kind::Start => "start",
            Kind::World => "load",
            Kind::Again => "again",
        }
    }

    /// Seconds one takes where none is known: about what eight runs took on
    /// an i7-8700K with the title on a hard disk (the start 91 to 151, the
    /// first loading screen 234 to 252 and 451 and 596; no later one was
    /// seen).
    fn guess(self) -> f64 {
        match self {
            Kind::Start => 110.0,
            Kind::World => 300.0,
            Kind::Again => 60.0,
        }
    }
}

/// Reads over a load's time: (milliseconds, cost), neither ever less than
/// before; at least a first and a last point.
#[derive(Clone, Debug, PartialEq)]
struct Curve(Vec<(u64, u64)>);

impl Curve {
    const POINTS: usize = 96;
    /// The share of all reads by which two loads of one place differ.
    const SLACK: f64 = 0.04;

    fn flat(seconds: f64) -> Curve {
        Curve(vec![(0, 0), ((seconds * 1e3) as u64, 0)])
    }

    fn seconds(&self) -> f64 {
        self.0.last().map_or(0.0, |p| p.0 as f64 / 1e3)
    }

    fn cost(&self) -> f64 {
        self.0.last().map_or(0.0, |p| p.1 as f64)
    }

    /// When (seconds) the reads passed `cost`, between points `i - 1` and `i`.
    fn between(&self, i: usize, cost: f64) -> f64 {
        let ((t0, c0), (t1, c1)) = (self.0[i - 1], self.0[i]);
        let part = (cost - c0 as f64) / (c1 as f64 - c0 as f64).max(1.0);
        (t0 as f64 + part.clamp(0.0, 1.0) * (t1 as f64 - t0 as f64)) / 1e3
    }

    /// The earliest and the latest second at which it had read about `cost`.
    fn stretch(&self, cost: f64) -> (f64, f64) {
        let slack = self.cost() * Self::SLACK;
        if slack == 0.0 {
            return (0.0, self.seconds());
        }
        let (low, high) = ((cost - slack).max(0.0), cost + slack);
        let at = |i: usize, cost: f64| match i {
            0 => 0.0,
            i if i >= self.0.len() => self.seconds(),
            i => self.between(i, cost),
        };
        let from = at(self.0.partition_point(|p| (p.1 as f64) < low), low);
        let to = at(self.0.partition_point(|p| p.1 as f64 <= high), high);
        (from, to.max(from))
    }

    /// As many points as are kept, evenly spaced in time.
    fn thinned(&self) -> Curve {
        let whole = self.0.last().map_or(0, |p| p.0);
        let time = |k: usize| whole * k as u64 / (Self::POINTS as u64 - 1);
        let mut points: Vec<(u64, u64)> = (0..Self::POINTS)
            .filter_map(|k| self.0.get(self.0.partition_point(|p| p.0 < time(k))))
            .copied()
            .collect();
        points.dedup();
        Curve(points)
    }
}

/// Follows one load along another.
struct Follow {
    along: Curve,
    /// Its length is a guess: it goes slower the nearer the end is.
    guess: bool,
    /// Seconds of `along` that are done.
    place: f64,
    /// Seconds this load takes for a second of `along`.
    pace: f64,
    /// (seconds, place) of the last minute.
    lately: VecDeque<(f64, f64)>,
}

impl Follow {
    /// Seconds of `along` in a second, at most, when the reads pull it on.
    const PULL: f64 = 6.0;

    fn new(along: Curve, guess: bool) -> Follow {
        Follow { along, guess, place: 0.0, pace: 1.0, lately: VecDeque::new() }
    }

    /// `dt` seconds later, `now` seconds into the load.
    fn step(&mut self, dt: f64, now: f64, cost: f64) {
        let whole = self.along.seconds();
        let (from, to) = self.along.stretch(cost);
        let ease = if self.guess {
            (1.0 - (self.place / whole.max(1.0)).powi(3)).max(0.05)
        } else {
            1.0
        };
        let mut next = (self.place + dt / self.pace * ease).min(to);
        if next < from {
            next = from.min(self.place + Self::PULL * dt);
        }
        self.place = next.max(self.place).min(whole);
        self.lately.push_back((now, self.place));
        while self.lately.front().is_some_and(|p| p.0 < now - 60.0) {
            self.lately.pop_front();
        }
        if self.guess {
            return;
        }
        let (then, there) = self.lately[0];
        let passed = now - then;
        let recent = if passed > 5.0 {
            passed / (self.place - there).max(passed / 4.0)
        } else {
            1.0
        };
        let all = now / self.place.max(now / 4.0).max(0.1);
        let seen = (0.5 * recent + 0.5 * all).clamp(0.3, 4.0);
        // Of the first seconds little follows.
        let target = 1.0 + (seen - 1.0) * (now / 40.0).min(1.0);
        self.pace += (target - self.pace) * (dt / 10.0).min(1.0);
    }

    fn share(&self) -> f64 {
        (self.place / self.along.seconds().max(0.1)).clamp(0.0, 0.99)
    }

    /// Seconds to go; `None` past the end of `along`.
    fn left(&self) -> Option<f64> {
        let rest = self.along.seconds() - self.place;
        (rest >= 0.5).then(|| rest * self.pace.powf(0.8))
    }
}

/// The clock's guess and the reads' put together.
struct Guess {
    clock: Follow,
    reads: Follow,
    /// How much the reads count at least, as the history has it (0 to 1).
    trust: f64,
    /// (seconds, the reads' place) so far.
    way: Vec<(f64, f64)>,
    shown: f64,
}

impl Guess {
    fn new(along: Curve, guess: bool, trust: f64) -> Guess {
        let clock = Follow::new(Curve::flat(along.seconds()), guess);
        Guess { clock, reads: Follow::new(along, guess), trust, way: Vec::new(), shown: 0.0 }
    }

    fn step(&mut self, dt: f64, now: f64, cost: f64) {
        self.clock.step(dt, now, 0.0);
        self.reads.step(dt, now, cost);
        self.way.push((now, self.reads.place));
        let trust = self.trust + (0.95 - self.trust) * self.steady(now);
        let share = trust * self.reads.share() + (1.0 - trust) * self.clock.share();
        self.shown = self.shown.max(share);
    }

    /// How evenly the reads have come: 1 is the load before at one pace.
    fn steady(&self, now: f64) -> f64 {
        let place = self.reads.place.max(1e-3);
        let straight = place / now.max(1e-3);
        let off: f64 = self.way.iter().map(|&(t, p)| (p - t * straight).abs()).sum();
        let off = off / self.way.len().max(1) as f64 / place;
        (1.0 - off / 0.1).max(0.0) * (now / 30.0).min(1.0)
    }

    /// Seconds to go; `None` where the load before would be over.
    fn left(&self) -> Option<f64> {
        let rest = self.reads.along.seconds() * (1.0 - self.shown);
        (self.shown < 0.985).then(|| rest * self.reads.pace.clamp(0.5, 2.0).powf(0.8))
    }

    /// How far off the clock's and the reads' times to go were over the load
    /// `real` had it been followed along `along`, as shares of its length.
    fn errors(along: &Curve, real: &Curve) -> (f64, f64) {
        let mut g = Guess::new(along.clone(), false, 0.5);
        let whole = real.seconds();
        let (mut clock, mut reads, mut before) = (0.0, 0.0, 0.0);
        for &(ms, cost) in &real.0 {
            let now = ms as f64 / 1e3;
            g.step(now - before, now, cost as f64);
            before = now;
            let off = |f: &Follow| (f.left().unwrap_or(5.0) - (whole - now)).abs();
            clock += off(&g.clock);
            reads += off(&g.reads);
        }
        let points = (real.0.len() as f64).max(1.0) * whole.max(1.0);
        (clock / points, reads / points)
    }
}

/// A load that reached its end.
#[derive(Clone, Debug, PartialEq)]
struct Record {
    kind: Kind,
    /// Seconds since 1970.
    when: u64,
    cold: bool,
    /// Reads of files, and KiB taken from them.
    reads: u64,
    kib: u64,
    /// What the clock and the reads got wrong, followed along a load before.
    errors: Option<(f64, f64)>,
    curve: Curve,
}

fn pair<T: std::str::FromStr>(s: &str) -> Option<(T, T)> {
    let (a, b) = s.split_once(':')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

impl Record {
    /// Milliseconds a load lasts, at least and at most: what is past them in
    /// the file is damage.
    const LENGTHS: std::ops::RangeInclusive<u64> = 1_000..=6 * 3_600_000;
    /// Points a load in the file has, at most.
    const POINTS_MAX: usize = 4096;

    fn line(&self) -> String {
        let points: Vec<String> = self.curve.0.iter().map(|(t, c)| format!("{t}:{c}")).collect();
        let errors = self.errors.map_or(String::new(), |(c, r)| format!(" errors={c:.3}:{r:.3}"));
        format!(
            "load 1 kind={} when={} cold={} reads={} kib={}{errors} curve={}\n",
            self.kind.name(),
            self.when,
            u8::from(self.cold),
            self.reads,
            self.kib,
            points.join(",")
        )
    }

    fn parse(line: &str) -> Option<Record> {
        let rest = line.strip_prefix("load 1 ")?;
        let field = |name: &str| {
            rest.split_ascii_whitespace()
                .find_map(|w| w.strip_prefix(name)?.strip_prefix('='))
        };
        let curve: Vec<(u64, u64)> = field("curve")?
            .split(',')
            .take(Self::POINTS_MAX + 1)
            .map(pair::<u64>)
            .collect::<Option<_>>()?;
        let grows = curve.windows(2).all(|w| w[0].0 <= w[1].0 && w[0].1 <= w[1].1);
        let fits = (2..=Self::POINTS_MAX).contains(&curve.len())
            && curve.last().is_some_and(|p| Self::LENGTHS.contains(&p.0));
        if !grows || !fits {
            return None;
        }
        let kind = field("kind")?;
        let sound = |e: &(f64, f64)| [e.0, e.1].iter().all(|v| (0.0..=100.0).contains(v));
        Some(Record {
            kind: Kind::ALL.into_iter().find(|k| k.name() == kind)?,
            when: field("when")?.parse().ok()?,
            cold: field("cold")? == "1",
            reads: field("reads")?.parse().ok()?,
            kib: field("kib")?.parse().ok()?,
            errors: field("errors").and_then(pair::<f64>).filter(sound),
            curve: Curve(curve),
        })
    }
}

/// The loads of this machine, oldest first.
#[derive(Default)]
struct History(Vec<Record>);

impl History {
    /// Loads kept of each kind.
    const KEEP: usize = 6;
    /// Bytes of the file that are read: far more than it ever has.
    const FILE_MAX: u64 = 1 << 20;

    fn parse(text: &str) -> History {
        History(text.lines().filter_map(Record::parse).collect())
    }

    /// The loads in `file`; none if there is no such file, and all that can
    /// be read of a damaged one.
    fn load(file: &Path) -> History {
        use std::io::Read as _;
        let mut bytes = Vec::new();
        let read = std::fs::File::open(file)
            .and_then(|f| f.take(Self::FILE_MAX).read_to_end(&mut bytes));
        if let Err(e) = read
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(error = %e, "the loads before were not read");
        }
        History::parse(&String::from_utf8_lossy(&bytes))
    }

    /// Writes the loads to a file beside `file`, then over it.
    fn save(&self, file: &Path) -> std::io::Result<()> {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let beside = file.with_extension("tmp");
        std::fs::write(&beside, self.text()).and_then(|()| std::fs::rename(&beside, file))
    }

    fn text(&self) -> String {
        self.0.iter().map(Record::line).collect()
    }

    /// The load to follow: of the last three of its kind, with caches as
    /// cold if there are any, the one of middle length (a machine busy with
    /// something else makes one far longer); of fewer the newest.
    fn reference(&self, kind: Kind, cold: bool) -> Option<&Record> {
        let of_kind = || self.0.iter().rev().filter(move |r| r.kind == kind);
        let mut last: Vec<&Record> = of_kind().filter(|r| r.cold == cold).take(3).collect();
        if last.is_empty() {
            last = of_kind().take(3).collect();
        }
        if last.len() == 3 {
            last.sort_by(|a, b| a.curve.seconds().total_cmp(&b.curve.seconds()));
            return Some(last[1]);
        }
        last.first().copied()
    }

    /// How much the reads' guess counts beside the clock's: half where
    /// nothing is known, else by what the two got wrong lately.
    fn trust(&self, kind: Kind) -> f64 {
        let (mut clock, mut reads) = (0.0, 0.0);
        let known = self.0.iter().rev().filter(|r| r.kind == kind).filter_map(|r| r.errors);
        for (c, r) in known.take(3) {
            clock += c;
            reads += r;
        }
        0.5 + 0.35 * (clock - reads) / f64::max(clock + reads, 1e-3)
    }

    fn add(&mut self, record: Record) {
        let kind = record.kind;
        self.0.push(record);
        let of_kind = self.0.iter().filter(|r| r.kind == kind).count();
        let mut spare = of_kind.saturating_sub(Self::KEEP);
        self.0.retain(|r| {
            let gone = r.kind == kind && spare > 0;
            spare -= usize::from(gone);
            !gone
        });
    }
}

/// What a second of frames looks like.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Look {
    /// The title's loading screen: about a dozen draws at its own 30 a second.
    Loading,
    /// A menu: as many, as fast as they come.
    Menu,
    Other,
}

impl Look {
    fn of(fps: f64, work: f64) -> Look {
        if (20.0..=34.0).contains(&fps) && (5.0..=30.0).contains(&work) {
            Look::Loading
        } else if fps >= 45.0 && (9.0..=100.0).contains(&work) {
            Look::Menu
        } else {
            Look::Other
        }
    }
}

/// A load under way.
struct Load {
    kind: Kind,
    base: Signals,
    files: PerFile,
    guess: Guess,
    /// Whether it is followed along a load of this machine that says
    /// something about it.
    known: bool,
    /// KiB the followed load took from files.
    kib: u64,
    /// Seconds into it at the last look.
    at: f64,
    points: Vec<(u64, u64)>,
}

enum Stage {
    Load(Load),
    /// Menus, films.
    Idle,
    World,
}

/// What to show now.
#[derive(Clone, Debug, Default, PartialEq)]
struct Report {
    stage: &'static str,
    share: Option<f64>,
    left: Option<f64>,
    known: bool,
    /// Seconds in this stage.
    elapsed: f64,
    /// MiB taken from files in this load, of how many, and in a second.
    read: u64,
    of: u64,
    rate: f64,
    /// Workers waiting for files, on average over ten seconds.
    readers: f64,
    /// Seconds the world's load took before, while it has not begun.
    next: Option<f64>,
}

struct Loads {
    history: History,
    stage: Stage,
    /// When the stage began.
    began: f64,
    /// The world has been reached before.
    been: bool,
    /// Where the second of frames now looked at began.
    window: (f64, Signals),
    /// What the seconds looked like, how many in a row, since when.
    row: (Look, u32, (f64, Signals)),
    /// (seconds, signals) of the last ten seconds.
    lately: VecDeque<(f64, Signals)>,
    /// A load ended since this was last taken, and the history changed.
    ended: bool,
    changed: bool,
}

impl Loads {
    fn new(history: History) -> Loads {
        let zero = (0.0, Signals::default());
        let mut loads = Loads {
            history,
            stage: Stage::Idle,
            began: 0.0,
            been: false,
            window: zero,
            row: (Look::Other, 0, zero),
            lately: VecDeque::new(),
            ended: false,
            changed: false,
        };
        loads.begin(Kind::Start, zero);
        loads
    }

    fn begin(&mut self, kind: Kind, (began, base): (f64, Signals)) {
        let trust = self.history.trust(kind);
        let before = self.history.reference(kind, base.cold);
        let along = before.map_or_else(|| Curve::flat(kind.guess()), |r| r.curve.clone());
        // With caches colder than then its length says less.
        let known = before.is_some_and(|r| r.cold || !base.cold);
        let (guess, kib) = (before.is_none(), before.map_or(0, |r| r.kib));
        tracing::info!(
            kind = kind.name(),
            cold = base.cold,
            known,
            expected_s = along.seconds(),
            trust,
            "a load begins"
        );
        self.began = began;
        self.stage = Stage::Load(Load {
            kind,
            base,
            files: files(),
            guess: Guess::new(along, guess, trust),
            known,
            kib,
            at: 0.0,
            points: vec![(0, 0)],
        });
    }

    /// The load under way, if there is one, has reached its end.
    fn finish(&mut self, now: f64, s: &Signals) {
        let Stage::Load(load) = std::mem::replace(&mut self.stage, Stage::Idle) else { return };
        let seconds = now - self.began;
        self.began = now;
        self.ended = true;
        let (reads, kib) = (s.reads - load.base.reads, (s.bytes - load.base.bytes) >> 10);
        tracing::info!(
            kind = load.kind.name(),
            seconds,
            shown = load.guess.shown,
            reads,
            mib = kib >> 10,
            written_mib = (s.written - load.base.written) >> 20,
            readers = (s.waited - load.base.waited) as f64 / 1e9 / seconds.max(1.0),
            files = s.files - load.base.files,
            pipelines = s.pipelines - load.base.pipelines,
            shaders = s.shaders - load.base.shaders,
            cold = s.cold,
            most_read = %most_read(&load.files),
            "a load is done"
        );
        let curve = Curve(load.points).thinned();
        let length = curve.0.last().map_or(0, |p| p.0);
        if seconds < 5.0 || curve.0.len() < 2 || !Record::LENGTHS.contains(&length) {
            return;
        }
        let before = self.history.reference(load.kind, s.cold);
        let errors = before.map(|along| Guess::errors(&along.curve, &curve));
        let when = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        self.history.add(Record { kind: load.kind, when, cold: s.cold, reads, kib, errors, curve });
        self.changed = true;
    }

    fn step(&mut self, now: f64, s: Signals) -> Report {
        self.lately.push_back((now, s));
        while self.lately.front().is_some_and(|p| p.0 < now - 10.0) {
            self.lately.pop_front();
        }
        let (then, before) = self.lately[0];
        let passed = (now - then).max(0.5);
        // Reading, or files were opened to be read.
        let busy = s.reads - before.reads >= 40 || s.files > before.files;

        let (from, w) = self.window;
        let (frames, work) = (s.frames - w.frames, s.work - w.work);
        if work >= 300 && work / frames.max(1) >= 150 {
            // A frame of the world, also one still being recorded.
            self.window = (now, s);
            self.row = (Look::Other, 0, (now, s));
            self.been = true;
            if !matches!(self.stage, Stage::World) {
                self.finish(now, &s);
                (self.stage, self.began) = (Stage::World, now);
            }
        } else if now - from >= 1.0 && frames > 0 {
            self.window = (now, s);
            let look = Look::of(frames as f64 / (now - from), work as f64 / frames as f64);
            self.row = match self.row {
                (l, n, at) if l == look => (l, n + 1, at),
                _ => (look, 1, (from, w)),
            };
            self.looked(now, &s, busy);
        }

        let mut report = Report {
            elapsed: now - self.began,
            rate: (s.bytes - before.bytes) as f64 / passed / 1048576.0,
            readers: (s.waited - before.waited) as f64 / 1e9 / passed,
            ..Report::default()
        };
        match &mut self.stage {
            Stage::Idle => {
                report.stage = "idle";
                if !self.been {
                    let next = self.history.reference(Kind::World, s.cold);
                    report.next = next.map(|r| r.curve.seconds());
                }
            }
            Stage::World => report.stage = "world",
            Stage::Load(load) => {
                let into = report.elapsed;
                let cost = s.cost().saturating_sub(load.base.cost());
                load.guess.step(into - load.at, into, cost as f64);
                load.at = into;
                load.points.push(((into * 1e3) as u64, cost));
                report.stage = load.kind.stage();
                report.share = Some(load.guess.shown);
                report.left = load.guess.left();
                report.known = load.known;
                report.read = (s.bytes - load.base.bytes) >> 20;
                report.of = load.kib >> 10;
                if load.kind == Kind::Start {
                    let next = self.history.reference(Kind::World, s.cold);
                    report.next = next.map(|r| r.curve.seconds());
                }
            }
        }
        report
    }

    /// A second of frames was looked at: begins or ends a load.
    fn looked(&mut self, now: f64, s: &Signals, busy: bool) {
        let (look, n, at) = self.row;
        let under_way = match &self.stage {
            Stage::Load(l) => Some(l.kind),
            _ => None,
        };
        let in_a_row = if matches!(self.stage, Stage::World) { 4 } else { 2 };
        match (under_way, look) {
            // The first menu (or at once a loading screen) ends the start.
            (Some(Kind::Start), Look::Menu | Look::Loading) if n >= 2 => {
                self.finish(now, s);
                if look == Look::Loading && busy {
                    self.begin(Kind::World, at);
                }
            }
            // Back in a menu: given up.
            (Some(_), Look::Menu) if n >= 4 => (self.stage, self.began) = (Stage::Idle, now),
            (None, Look::Loading) if busy && n >= in_a_row => {
                self.begin(if self.been { Kind::Again } else { Kind::World }, at);
            }
            _ => {}
        }
    }
}

/// Host pipelines still to make in the world.
#[derive(Default)]
struct Backlog {
    /// (seconds, pipelines waiting) of the last half minute.
    lately: VecDeque<(f64, u64)>,
    /// Many wait, and have for a while: they are told of until few do.
    many: bool,
}

impl Backlog {
    const MANY: u64 = 16;
    const FEW: u64 = 4;

    /// Seconds to go, if they have been getting fewer for a while.
    fn left(&mut self, now: f64, waiting: u64) -> Option<f64> {
        self.lately.push_back((now, waiting));
        while self.lately.front().is_some_and(|p| p.0 < now - 30.0) {
            self.lately.pop_front();
        }
        // A place's new pipelines come and go in a second or two.
        let while_ago = self.lately.iter().rev().take_while(|p| p.0 > now - 2.0);
        if now - self.lately[0].0 >= 2.0 && while_ago.into_iter().all(|p| p.1 >= Self::MANY) {
            self.many = true;
        } else if waiting < Self::FEW {
            self.many = false;
        }
        let (then, were) = self.lately[0];
        let a_second = were.checked_sub(waiting)? as f64 / (now - then).max(1.0);
        (now - then > 10.0 && a_second > 0.1).then(|| waiting as f64 / a_second)
    }

    /// Forgets what was before the world.
    fn clear(&mut self) {
        self.lately.clear();
        self.many = false;
    }
}

/// A line over the picture, and how much of the bar under it is full.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Shown {
    pub(crate) text: String,
    pub(crate) share: Option<f64>,
}

static SHOWN: Mutex<Option<Arc<Shown>>> = Mutex::new(None);

/// What to draw over the picture now.
pub(crate) fn shown() -> Option<Arc<Shown>> {
    SHOWN.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn show(line: Option<Shown>) {
    let mut line = line.map(Arc::new);
    // What was shown is let go of after the lock.
    std::mem::swap(&mut *SHOWN.lock().unwrap_or_else(|e| e.into_inner()), &mut line);
}

/// `~3 MIN`, `~40 S`.
fn about(seconds: f64) -> String {
    if seconds >= 90.0 {
        format!("~{:.0} MIN", (seconds / 60.0).ceil())
    } else {
        format!("~{:.0} S", (seconds / 10.0).ceil().max(1.0) * 10.0)
    }
}

/// The line for a report, and the pipelines to tell of with the seconds to
/// go. (The readout has no accents: "HA" is "há".)
fn line(r: &Report, pipelines: Option<(u64, Option<f64>)>) -> Option<Shown> {
    let time = |left: Option<f64>| left.map_or(String::new(), |s| format!("  {}", about(s)));
    let what = match r.stage {
        "start" => "INICIANDO",
        "load" | "again" => "CARREGANDO",
        _ => {
            let (waiting, left) = pipelines?;
            let text = format!("FALTAM {waiting} PIPELINES{}", time(left));
            return Some(Shown { text, share: None });
        }
    };
    let share = r.share?;
    let percent = (share * 100.0).floor();
    let text = if r.known {
        format!("{what} {percent:.0}%{}", time(r.left))
    } else {
        // A guess: how long it has been, not how long it will be.
        let seconds = r.elapsed.max(0.0) as u64;
        format!("{what} ~{percent:.0}%?  HA {}:{:02}", seconds / 60, seconds % 60)
    };
    Some(Shown { text, share: Some(share) })
}

static FILE: OnceLock<PathBuf> = OnceLock::new();
static BEGAN: OnceLock<Instant> = OnceLock::new();

/// Seconds since the process started (for the session panel's lines).
pub(crate) fn clock() -> f64 {
    BEGAN.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// Keeps the loads of this machine in `file`. Called as the process starts:
/// the first load is counted from here.
pub fn use_history(file: PathBuf) {
    let _ = BEGAN.set(Instant::now());
    let _ = FILE.set(file);
}

/// Starts looking (once).
pub fn start() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let thread = std::thread::Builder::new().name("victory-loading".into()).spawn(|| {
            // A line is not left standing if this ends.
            let _ = std::panic::catch_unwind(watch);
            show(None);
        });
        if let Err(e) = thread {
            tracing::warn!(error = %e, "no loading progress");
        }
    });
}

fn watch() {
    const STEP: Duration = Duration::from_millis(500);
    let drawn = std::env::var("VYCE_LOADING").map_or(true, |v| v != "0");
    let history = FILE.get().map_or_else(History::default, |f| History::load(f));
    let mut loads = Loads::new(history);
    let mut backlog = Backlog::default();
    let mut told = 0.0;
    loop {
        std::thread::sleep(STEP);
        let (now, s) = (clock(), Signals::now());
        let report = loads.step(now, s);
        let left = match loads.stage {
            Stage::World => backlog.left(now, s.waiting),
            _ => {
                backlog.clear();
                None
            }
        };
        if drawn {
            show(line(&report, backlog.many.then_some((s.waiting, left))));
        }
        if crate::telemetry::on() && now - told >= 0.9 {
            told = now;
            tell(&json(now, &report, &s, left));
        }
        if std::mem::take(&mut loads.ended) {
            crate::pipeline::save_pipeline_cache_soon();
        }
        if std::mem::take(&mut loads.changed)
            && let Some(file) = FILE.get()
            && let Err(e) = loads.history.save(file)
        {
            tracing::warn!(error = %e, "this load was not kept");
        }
    }
}

/// A line for the session panel, and for looking at later, in one piece.
fn tell(line: &str) {
    use std::io::Write as _;
    let _ = std::io::stderr().lock().write_all(line.as_bytes());
}

fn json(t: f64, r: &Report, s: &Signals, backlog: Option<f64>) -> String {
    let number = |v: Option<f64>| {
        v.filter(|v| v.is_finite()).map_or("null".to_owned(), |v| format!("{v:.1}"))
    };
    let plain = |v: f64| if v.is_finite() { v } else { 0.0 };
    format!(
        "VYCE_LOADING {{\"schema\":1,\"t_s\":{:.1},\"stage\":\"{}\",\"percent\":{},\"left_s\":{},\
         \"known\":{},\"elapsed_s\":{:.1},\"next_s\":{},\"read_mb\":{},\"read_of_mb\":{},\
         \"read_mb_s\":{:.2},\"readers\":{:.2},\"reads\":{},\"bytes_mb\":{},\"written_mb\":{},\
         \"queued\":{},\"files\":{},\"frames\":{},\"work\":{},\"pipelines_waiting\":{},\
         \"pipelines_done\":{},\"pipelines_left_s\":{},\"shaders\":{},\"cold\":{}}}\n",
        plain(t),
        r.stage,
        number(r.share.map(|v| v * 100.0)),
        number(r.left),
        r.known,
        plain(r.elapsed),
        number(r.next),
        r.read,
        r.of,
        plain(r.rate),
        plain(r.readers),
        s.reads,
        s.bytes >> 20,
        s.written >> 20,
        s.queued,
        s.files,
        s.frames,
        s.work,
        s.waiting,
        s.pipelines,
        number(backlog),
        s.shaders,
        s.cold,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A load of `seconds` that reads at an even rate.
    fn even(seconds: u64, cost: u64) -> Curve {
        Curve((0..=seconds * 2).map(|k| (k * 500, cost * k / (seconds * 2))).collect())
    }

    /// Follows `real` along `along`; the share and the seconds to go at each
    /// half second.
    fn follow(along: &Curve, real: &Curve) -> Vec<(f64, f64, Option<f64>)> {
        let mut g = Guess::new(along.thinned(), false, 0.5);
        let mut before = 0.0;
        real.0
            .iter()
            .map(|&(ms, cost)| {
                let now = ms as f64 / 1e3;
                g.step(now - before, now, cost as f64);
                before = now;
                (now, g.shown, g.left())
            })
            .collect()
    }

    fn record(kind: Kind, when: u64, cold: bool, seconds: f64) -> Record {
        Record { kind, when, cold, reads: 0, kib: 0, errors: None, curve: Curve::flat(seconds) }
    }

    #[test]
    fn a_load_like_the_one_before_is_on_time() {
        let seen = follow(&even(200, 1_000_000), &even(200, 1_000_000));
        for &(now, share, left) in seen.iter().filter(|s| s.0 <= 180.0) {
            assert!((share - now / 200.0).abs() < 0.03, "{share} at {now} s");
            assert!((left.unwrap() - (200.0 - now)).abs() < 8.0, "{left:?} at {now} s");
        }
        assert!(seen.last().unwrap().1 > 0.97);
        // Also where nothing is known to be read.
        let seen = follow(&Curve::flat(200.0), &Curve(even(200, 0).0));
        assert!(seen.iter().all(|s| (s.1 - s.0 / 200.0).abs() < 0.03));
    }

    /// A slower load (a cold disk, a busy machine): never back, never over,
    /// and the time to go between the load before's pace and this one's.
    #[test]
    fn a_slower_load_is_noticed() {
        let seen = follow(&even(200, 1_000_000), &even(360, 1_000_000));
        assert!(seen.windows(2).all(|w| w[0].1 <= w[1].1), "the share went back");
        assert!(seen.iter().all(|s| s.1 <= 0.99));
        let at = |s: f64| seen.iter().find(|x| x.0 >= s).unwrap();
        assert!((0.45..0.66).contains(&at(180.0).1), "{:?}", at(180.0));
        let left = at(180.0).2.unwrap();
        assert!((90.0..200.0).contains(&left), "{left}");
    }

    #[test]
    fn a_faster_load_is_noticed() {
        let seen = follow(&even(400, 1_000_000), &even(240, 1_000_000));
        let at = |s: f64| seen.iter().find(|x| x.0 >= s).unwrap();
        assert!((0.4..0.6).contains(&at(120.0).1), "{:?}", at(120.0));
        assert!(seen.last().unwrap().1 > 0.85, "{:?}", seen.last());
    }

    /// While nothing is read the reads' guess waits.
    #[test]
    fn a_stall_holds_the_reads() {
        let real = even(200, 1_000_000).0;
        let (half, rest) = real.split_at(200);
        let (began, stalled) = *half.last().unwrap();
        let mut all = half.to_vec();
        all.extend((1..=120).map(|k| (began + k * 500, stalled)));
        all.extend(rest.iter().map(|&(t, c)| (t + 60_000, c)));
        let mut f = Follow::new(even(200, 1_000_000).thinned(), false);
        let mut before = 0.0;
        for &(ms, cost) in &all {
            let now = ms as f64 / 1e3;
            f.step(now - before, now, cost as f64);
            before = now;
            if (120.0..158.0).contains(&now) {
                assert!((0.45..0.56).contains(&f.share()), "{} at {now} s", f.share());
            }
        }
        assert!(f.share() > 0.95);
    }

    #[test]
    fn a_record_reads_back_and_a_broken_one_does_not() {
        let record = Record {
            kind: Kind::World,
            when: 1_795_634_630,
            cold: true,
            reads: 38_211,
            kib: 5_021_000,
            errors: Some((0.125, 0.25)),
            curve: even(60, 100_000).thinned(),
        };
        let text = History(vec![record.clone()]).text();
        assert_eq!(text.lines().count(), 1);
        assert_eq!(History::parse(&text).0, [record]);
        let line = |kind: &str, rest: &str, curve: &str| {
            format!("load 1 kind={kind} when=1 cold=0 reads=0 kib=0{rest} curve={curve}")
        };
        assert!(Record::parse(&line("world", "", "0:0,5000:1")).is_some());
        for broken in [
            String::new(),
            "load 1 kind=world".into(),
            "load 2".to_owned() + &line("world", "", "0:0,5000:1")[6..],
            line("x", "", "0:0,5000:1"),
            line("world", "", ""),
            line("world", "", "0:0"),
            line("world", "", "0:0,5000"),
            line("world", "", "0:0,5000:-1"),
            line("world", "", "0:5,5000:1"),
            line("world", "", "5000:0,4000:1"),
            // Shorter than a load is, and longer.
            line("world", "", "0:0,500:1"),
            line("world", "", "0:0,18446744073709551615:1"),
            line("world", "", "0:0,99999999999999999999:1"),
            line("world", "", &vec!["0:0"; 5000].join(",")),
        ] {
            assert!(Record::parse(&broken).is_none(), "{broken:.80}");
        }
        // Errors that are no numbers are as good as none.
        for errors in [" errors=nan:0.1", " errors=0.1:inf", " errors=-1:0", " errors=x"] {
            let r = Record::parse(&line("world", errors, "0:0,5000:1")).unwrap();
            assert_eq!(r.errors, None, "{errors}");
        }
        let clock_only = Record::parse(&line("start", "", "0:0,100000:0")).unwrap();
        assert_eq!(clock_only.curve.seconds(), 100.0);
    }

    #[test]
    fn a_damaged_file_gives_what_can_be_read_of_it() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cache").join("loads.txt");
        assert!(History::load(&file).0.is_empty());
        let mut history = History::default();
        history.add(record(Kind::Start, 1, false, 100.0));
        history.add(record(Kind::World, 2, false, 250.0));
        history.save(&file).unwrap();
        assert_eq!(History::load(&file).0, history.0);
        // Cut short, and bytes that are no text.
        let mut bytes = std::fs::read(&file).unwrap();
        bytes.splice(0..0, [0xff, 0xfe, 0x00, b'\n']);
        bytes.truncate(bytes.len() - 20);
        std::fs::write(&file, &bytes).unwrap();
        assert_eq!(History::load(&file).0, history.0[..1]);
        // It is written over as if it had been sound, beside nothing else.
        history.save(&file).unwrap();
        assert_eq!(History::load(&file).0, history.0);
        assert_eq!(std::fs::read_dir(file.parent().unwrap()).unwrap().count(), 1);
        // A folder of that name.
        assert!(History::load(dir.path()).0.is_empty());
        assert!(history.save(dir.path()).is_err());
    }

    #[test]
    fn the_history_keeps_the_newest_of_each_kind() {
        let mut h = History::default();
        h.add(record(Kind::Start, 0, false, 10.0));
        (1..=9).for_each(|k| h.add(record(Kind::World, k, k % 2 == 0, 10.0)));
        let kept: Vec<u64> = h.0.iter().filter(|r| r.kind == Kind::World).map(|r| r.when).collect();
        assert_eq!(kept, [4, 5, 6, 7, 8, 9]);
        assert_eq!(h.reference(Kind::Start, true).unwrap().when, 0);
        assert!(h.reference(Kind::Again, true).is_none());
        assert!(h.text().len() < History::FILE_MAX as usize / 10);
    }

    /// One load on a machine busy with something else is not what the next
    /// is measured by, but a build that has become slower is.
    #[test]
    fn the_middle_of_the_last_three_is_followed() {
        let of = |lengths: &[f64], cold: bool| {
            let mut h = History::default();
            for (k, &s) in lengths.iter().enumerate() {
                h.add(record(Kind::World, k as u64, false, s));
            }
            h.reference(Kind::World, cold).map(|r| r.curve.seconds())
        };
        assert_eq!(of(&[250.0], false), Some(250.0));
        assert_eq!(of(&[250.0, 596.0], false), Some(596.0));
        assert_eq!(of(&[250.0, 596.0, 240.0], false), Some(250.0));
        assert_eq!(of(&[250.0, 240.0, 451.0, 460.0], false), Some(451.0));
        // Of caches as cold, if there are any.
        assert_eq!(of(&[250.0, 596.0, 240.0], true), Some(250.0));
        let mut h = History::default();
        h.add(record(Kind::World, 0, true, 600.0));
        h.add(record(Kind::World, 1, false, 250.0));
        assert_eq!(h.reference(Kind::World, true).unwrap().when, 0);
        assert_eq!(h.reference(Kind::World, false).unwrap().when, 1);
    }

    #[test]
    fn the_better_guess_counts_more() {
        let with = |errors| {
            let mut r = record(Kind::World, 0, false, 10.0);
            r.errors = errors;
            History(vec![r]).trust(Kind::World)
        };
        assert_eq!(History::default().trust(Kind::World), 0.5);
        assert_eq!(with(Some((0.0, 0.0))), 0.5);
        assert!(with(Some((0.4, 0.1))) > 0.65);
        assert!(with(Some((0.1, 0.4))) < 0.35);
        // Reads of a burst at the start say nothing, the clock is right.
        let mut burst = even(200, 1_000_000).0;
        burst.iter_mut().for_each(|p| p.1 = p.1.saturating_mul(50).min(1_000_000));
        let (clock, reads) = Guess::errors(&even(200, 1_000_000).thinned(), &Curve(burst).thinned());
        assert!(clock < reads, "{clock} {reads}");
    }

    /// A run of (seconds, frames a second, work a frame, reads each half
    /// second): its stages, and the reports at each half second.
    fn run(loads: &mut Loads, parts: &[(u32, u64, u64, u64)]) -> (Vec<&'static str>, Vec<Report>) {
        let (mut s, mut now) = (Signals::default(), 0.0);
        let mut stages: Vec<&str> = Vec::new();
        let mut reports = Vec::new();
        for &(seconds, fps, work, reads) in parts {
            for _ in 0..seconds * 2 {
                now += 0.5;
                s.frames += fps / 2;
                s.work += fps / 2 * work;
                s.reads += reads;
                s.bytes += reads << 18;
                let r = loads.step(now, s);
                if stages.last() != Some(&r.stage) {
                    stages.push(r.stage);
                }
                reports.push(r);
            }
        }
        (stages, reports)
    }

    /// As the first logs show them: the start's three draws a frame, the
    /// menu's thirteen, a film, the loading screen at 30 a second, the
    /// world, its map, and a journey.
    const A_RUN: [(u32, u64, u64, u64); 9] = [
        (120, 200, 3, 20),
        (6, 200, 13, 0),
        (15, 70, 6, 5),
        (250, 30, 11, 60),
        (30, 2, 4000, 30),
        (12, 14, 44, 0),
        (20, 4, 2000, 10),
        (30, 30, 11, 60),
        (20, 4, 2000, 10),
    ];

    #[test]
    fn a_run_is_told_apart() {
        let mut loads = Loads::new(History::default());
        let (stages, reports) = run(&mut loads, &A_RUN);
        assert_eq!(stages, ["start", "idle", "load", "world", "again", "world"]);
        assert!(loads.ended && loads.changed);
        let lengths: Vec<(Kind, f64)> =
            loads.history.0.iter().map(|r| (r.kind, r.curve.seconds().round())).collect();
        assert_eq!(lengths.len(), 3);
        assert!((120.0..125.0).contains(&lengths[0].1), "{lengths:?}");
        assert!((248.0..253.0).contains(&lengths[1].1), "{lengths:?}");
        assert_eq!(lengths[2].0, Kind::Again);
        // The share of a load rises and stays below one; menus have none.
        for stage in ["start", "load", "again"] {
            let of_stage = || reports.iter().filter(|r| r.stage == stage);
            let shares: Vec<f64> = of_stage().map(|r| r.share.unwrap()).collect();
            assert!(shares.windows(2).all(|w| w[0] <= w[1]), "{stage}");
            assert!(shares.iter().all(|v| (0.0..1.0).contains(v)), "{stage}");
            assert!(*shares.last().unwrap() > 0.3, "{stage} {:?}", shares.last());
            // The first of its kind: a guess, and said to be.
            assert!(of_stage().all(|r| !r.known && r.of == 0), "{stage}");
        }
        assert!(reports.iter().filter(|r| r.stage == "idle").all(|r| r.share.is_none()));
    }

    /// The next run follows what this one kept.
    #[test]
    fn the_second_run_is_known_and_on_time() {
        let mut loads = Loads::new(History::default());
        run(&mut loads, &A_RUN);
        let text = loads.history.text();
        let mut loads = Loads::new(History::parse(&text));
        let (_, reports) = run(&mut loads, &A_RUN);
        let menu = reports.iter().find(|r| r.stage == "idle").unwrap();
        assert!((245.0..255.0).contains(&menu.next.unwrap()));
        let load: Vec<&Report> = reports.iter().filter(|r| r.stage == "load").collect();
        for r in &load {
            assert!(r.known);
            let real = r.elapsed / 250.0;
            assert!((r.share.unwrap() - real).abs() < 0.04, "{r:?}");
        }
        assert!((7500..7530).contains(&load[0].of), "{}", load[0].of);
        assert!(load.last().unwrap().left.is_none());
        assert!(reports.iter().filter(|r| r.stage == "start").all(|r| r.known));
        assert!(reports.iter().filter(|r| r.stage == "world").all(|r| r.next.is_none()));
    }

    /// Caches colder than in the loads before (a new build or driver) make
    /// their length say less; warmer ones do not.
    #[test]
    fn colder_caches_are_not_known() {
        for (before, now, known) in [(false, true, false), (true, false, true), (true, true, true)] {
            let mut h = History::default();
            h.add(record(Kind::World, 0, before, 250.0));
            let mut loads = Loads::new(h);
            loads.begin(Kind::World, (100.0, Signals { cold: now, ..Signals::default() }));
            let s = Signals { cold: now, ..Signals::default() };
            assert_eq!(loads.step(101.0, s).known, known, "{before} {now}");
        }
    }

    #[test]
    fn a_load_left_for_the_menu_is_not_kept() {
        let mut loads = Loads::new(History::default());
        let (stages, _) =
            run(&mut loads, &[(60, 200, 3, 20), (6, 200, 13, 0), (30, 30, 11, 60), (10, 200, 13, 0)]);
        assert_eq!(stages, ["start", "idle", "load", "idle"]);
        assert_eq!(loads.history.0.len(), 1);
    }

    /// A menu at 30 frames a second that reads nothing is no loading screen.
    #[test]
    fn a_slow_menu_is_not_a_load() {
        let mut loads = Loads::new(History::default());
        let (stages, _) = run(&mut loads, &[(60, 200, 3, 0), (60, 30, 13, 1)]);
        assert_eq!(stages, ["start", "idle"]);
    }

    /// Frames of seconds and a picture that stands still: the load goes on.
    #[test]
    fn a_frozen_picture_does_not_end_a_load() {
        let mut loads = Loads::new(History::default());
        let mut parts = vec![(60, 200, 3, 20), (6, 200, 13, 0), (30, 30, 11, 60)];
        parts.extend([(40, 0, 0, 60), (30, 12, 11, 60)]);
        let (stages, reports) = run(&mut loads, &parts);
        assert_eq!(stages, ["start", "idle", "load"]);
        let last = reports.last().unwrap();
        assert!((98.0..102.0).contains(&last.elapsed), "{}", last.elapsed);
        assert!(last.rate > 29.0 && last.share.unwrap() > 0.2);
    }

    #[test]
    fn pipelines_are_told_of_while_many_wait() {
        let mut b = Backlog::default();
        // A burst of a second.
        assert_eq!(b.left(0.0, 2), None);
        for (t, n) in [(0.5, 40), (1.0, 40), (1.5, 20), (2.0, 0), (2.5, 0)] {
            assert_eq!(b.left(t, n), None);
            assert!(!b.many, "{t}");
        }
        b.clear();
        // The first minutes after a driver's change, as a log has them: as
        // many arrive as are made, then fewer and fewer.
        let seen = [226, 220, 255, 274, 265, 264, 249, 190, 148, 104, 47, 7, 2];
        let mut lefts = Vec::new();
        for (k, w) in seen.windows(2).enumerate() {
            for half in 0..32 {
                let part = f64::from(half) / 32.0;
                let waiting = f64::from(w[0]) + (f64::from(w[1]) - f64::from(w[0])) * part;
                let now = k as f64 * 16.0 + f64::from(half) / 2.0;
                lefts.push((now, b.left(now, waiting as u64), b.many));
            }
        }
        let at = |s: f64| *lefts.iter().find(|l| l.0 >= s).unwrap();
        assert!(!at(1.0).2 && at(3.0).2);
        // No time while there are no fewer.
        assert_eq!(at(60.0).1, None);
        let left = at(128.0).1.unwrap();
        assert!((20.0..120.0).contains(&left), "{left}");
        assert!(at(172.0).2, "still told of");
        assert!(!lefts.last().unwrap().2, "a few are not told of");
    }

    #[test]
    fn lines_say_what_is_known() {
        let text = |r: &Report, p| line(r, p).map(|s| s.text);
        let of = |stage, share, left, known| Report {
            stage,
            share: Some(share),
            left: Some(left),
            known,
            elapsed: 205.4,
            ..Report::default()
        };
        let load = of("load", 0.437, 170.0, true);
        assert_eq!(text(&load, None).unwrap(), "CARREGANDO 43%  ~3 MIN");
        assert_eq!(line(&load, None).unwrap().share, Some(0.437));
        let again = of("again", 0.999, 4.0, true);
        assert_eq!(text(&again, None).unwrap(), "CARREGANDO 99%  ~10 S");
        assert_eq!(text(&of("start", 0.1, 34.0, true), None).unwrap(), "INICIANDO 10%  ~40 S");
        let end = Report { left: None, ..of("load", 0.99, 0.0, true) };
        assert_eq!(text(&end, None).unwrap(), "CARREGANDO 99%");
        // Not followed along anything: how long it has been, and that the
        // share is a guess.
        let guess = of("load", 0.437, 170.0, false);
        assert_eq!(text(&guess, None).unwrap(), "CARREGANDO ~43%?  HA 3:25");
        // Nothing in menus, and in the world only pipelines.
        let world = Report { stage: "world", ..Report::default() };
        let left = Some((231, Some(118.0)));
        assert_eq!(text(&world, left).unwrap(), "FALTAM 231 PIPELINES  ~2 MIN");
        assert_eq!(text(&world, Some((231, None))).unwrap(), "FALTAM 231 PIPELINES");
        assert!(text(&world, None).is_none());
        assert!(text(&Report { stage: "idle", ..Report::default() }, None).is_none());
    }

    /// The session panel reads this line: its fields, in this order.
    #[test]
    fn the_line_for_the_panel_has_its_fields() {
        let r = Report {
            stage: "load",
            share: Some(0.432),
            left: Some(172.0),
            known: true,
            elapsed: 205.42,
            read: 2112,
            of: 4903,
            rate: 14.208,
            readers: 7.8,
            next: Some(451.0),
        };
        let s = Signals {
            frames: 33012,
            work: 141_200,
            reads: 21034,
            bytes: 2112 << 20,
            written: 5210 << 20,
            waited: 0,
            queued: 37,
            files: 79,
            waiting: 0,
            pipelines: 1764,
            shaders: 1989,
            cold: false,
        };
        assert_eq!(
            json(388.13, &r, &s, None),
            "VYCE_LOADING {\"schema\":1,\"t_s\":388.1,\"stage\":\"load\",\"percent\":43.2,\
             \"left_s\":172.0,\"known\":true,\"elapsed_s\":205.4,\"next_s\":451.0,\"read_mb\":2112,\
             \"read_of_mb\":4903,\"read_mb_s\":14.21,\"readers\":7.80,\"reads\":21034,\
             \"bytes_mb\":2112,\"written_mb\":5210,\"queued\":37,\"files\":79,\"frames\":33012,\
             \"work\":141200,\"pipelines_waiting\":0,\"pipelines_done\":1764,\
             \"pipelines_left_s\":null,\"shaders\":1989,\"cold\":false}\n"
        );
        let idle = Report { stage: "idle", rate: f64::NAN, ..Report::default() };
        let text = json(1.0, &idle, &s, Some(f64::INFINITY));
        assert!(text.contains("\"percent\":null,\"left_s\":null,\"known\":false"));
        assert!(text.contains("\"read_mb_s\":0.00") && text.contains("\"pipelines_left_s\":null"));
    }

    #[test]
    fn files_are_counted_by_name() {
        let before = files();
        let a = opened("G:\\loading test\\a.rpf");
        a.read(3 << 20, Duration::from_millis(5));
        let again = opened("G:\\loading test\\a.rpf");
        assert!(std::ptr::eq(a, again));
        again.read(1 << 20, Duration::ZERO);
        opened("G:\\loading test\\b.rpf").read(9 << 20, Duration::ZERO);
        opened("G:\\loading test\\c.rpf");
        let most = most_read(&before);
        assert!(most.contains("a.rpf 2x 4 MiB") && most.contains("b.rpf 1x 9 MiB"), "{most}");
        assert!(most.find("b.rpf") < most.find("a.rpf") && !most.contains("c.rpf"), "{most}");
    }
}
