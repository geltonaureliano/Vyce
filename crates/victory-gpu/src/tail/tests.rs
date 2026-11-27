//! The tail drain against the old drain: the same submissions, the same
//! writes of the title's CPU and the same work of the host GPU leave the
//! same title memory and reads either way, and the same copies (all their
//! bytes where no gate opens after a while and the prewarm is kept, see
//! [`timed`]); brought up to date at the end, the copies are title memory.
//!
//! The memory is private memory the kernel watches writes to, with a sparse
//! copy of all of it and a copy that is not sparse of a part, and a section
//! of physical pages shown at two views, each with a copy that is not
//! sparse (as xmem's sections are): what a landing writes through one view
//! changes the other, and a refresh through that one waits for the landing
//! too (`crate::watch::other_views`).
//!
//! The host GPU's part of each title submission is done by the test, on a
//! thread of its own, when the gate in front of the submission opens: what
//! its shaders read is looked at, what they write is written into the
//! copies (and into memory taken for used in place), what its read-backs
//! bring is put where the landing takes it from. The submissions, their
//! preparation, the host queue's fences and the landings are the real ones
//! (on WARP, and on the machine's own GPU). The gate opens when the next
//! submission's preparation comes to wait for it (the latest the flight can
//! be: everything before the wait was prepared under it), or after a short
//! time; the work is done when the gate opens, or as soon as the submission
//! was sent.

use super::*;
use crate::host::{Host, HostList};
use crate::queue::Written;
use crate::shadow::Shadow;
use crate::watch::PAGE;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use windows::Win32::Graphics::Direct3D12::ID3D12Fence;

const TILES: usize = 8;
const SIZE: usize = TILES * PAGE;
/// The part of the memory with a copy that is not sparse, besides the
/// sparse one of all of it.
const DENSE: (usize, usize) = (2 * PAGE + 4096, 5 * PAGE);
/// Memory taken for used in place by the host GPU.
const IN_PLACE: (usize, usize) = (6 * PAGE, 7 * PAGE);
/// The memory past `SIZE`: a section of two pages shown twice, at the
/// first view and then at the second (the same bytes).
const VIEW: usize = 2 * PAGE;
const VIEW_A: (usize, usize) = (SIZE, SIZE + VIEW);
const VIEW_B: (usize, usize) = (SIZE + VIEW, SIZE + 2 * VIEW);
/// Of the first view, memory taken for used in place: at the second view the
/// same bytes are not, but refreshes of them wait as for it.
const VIEW_IN_PLACE: (usize, usize) = (SIZE + PAGE, SIZE + VIEW);

/// Where the bytes at `at` are shown at the first view (the private memory
/// and the first view: there).
fn same(at: usize) -> usize {
    if at >= VIEW_B.0 { at - VIEW } else { at }
}

/// The part of the memory `at` is in.
fn part_of(at: usize) -> (usize, usize) {
    match at {
        a if a < SIZE => (0, SIZE),
        a if a < VIEW_B.0 => VIEW_A,
        _ => VIEW_B,
    }
}

/// Whether `[at, at + len)` is memory used in place, at either view.
fn in_place(at: usize, len: usize) -> bool {
    let (from, to) = (same(at), same(at) + len);
    [IN_PLACE, VIEW_IN_PLACE].iter().any(|&(a, b)| from < b && a < to)
}

/// Title memory the kernel watches writes to.
struct Region(usize);

impl Region {
    fn new() -> Region {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
        };
        const MEM_WRITE_WATCH: u32 = 0x20_0000;
        let kind = MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH;
        // SAFETY: a fresh private region of the test's.
        let base = unsafe { VirtualAlloc(std::ptr::null(), SIZE, kind, PAGE_READWRITE) } as usize;
        assert_ne!(base, 0);
        Region(base)
    }

    fn at(&self, offset: usize) -> usize {
        self.0 + offset
    }

    fn write(&self, offset: usize, bytes: &[u8]) {
        assert!(offset + bytes.len() <= SIZE);
        let to = (self.0 + offset) as *mut u8;
        // SAFETY: inside the region, which only the test and what it runs
        // touch.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), to, bytes.len()) };
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
        // SAFETY: our region.
        unsafe { VirtualFree(self.0 as *mut _, 0, MEM_RELEASE) };
    }
}

/// A section of physical pages of the test's, two blocks, shown at two
/// views as the runtime tells of them (`crate::watch::title_memory_view`).
struct Section {
    handle: usize,
    a: usize,
    b: usize,
}

impl Section {
    fn new() -> Section {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, PAGE_READWRITE,
        };
        // SAFETY: a pagefile section of the test's, mapped read-write twice.
        unsafe {
            let handle = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                VIEW as u32,
                std::ptr::null(),
            );
            assert!(!handle.is_null());
            let a = MapViewOfFile(handle, FILE_MAP_WRITE, 0, 0, VIEW).Value as usize;
            let b = MapViewOfFile(handle, FILE_MAP_WRITE, 0, 0, VIEW).Value as usize;
            assert!(a != 0 && b != 0);
            for at in [a, b] {
                crate::watch::title_memory_view(at, VIEW, handle as usize, 0);
                // As the runtime tells of a mapping (another test's memory
                // may have been there, guarded).
                crate::watch::title_memory_remapped(at, VIEW);
            }
            Section { handle: handle as usize, a, b }
        }
    }
}

impl Drop for Section {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
        for at in [self.a, self.b] {
            crate::watch::title_memory_remapped(at, VIEW);
            crate::watch::title_memory_view_gone(at, VIEW);
            // SAFETY: our view, which nothing uses any more.
            unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: at as *mut _ }) };
        }
        // SAFETY: our section.
        unsafe { CloseHandle(self.handle as _) };
    }
}

/// Where the memory's offsets are: the private memory's, then the two
/// views'.
#[derive(Clone, Copy)]
struct Space {
    base: usize,
    a: usize,
    b: usize,
}

impl Space {
    fn at(self, offset: usize) -> usize {
        match offset {
            o if o < SIZE => self.base + o,
            o if o < VIEW_B.0 => self.a + (o - VIEW_A.0),
            o => self.b + (o - VIEW_B.0),
        }
    }

    /// Writes `bytes` at `offset` as the title's CPU does (a guarded view
    /// block opens on the fault).
    fn write(self, offset: usize, bytes: &[u8]) {
        assert!(offset + bytes.len() <= part_of(offset).1, "{offset:#x}+{:#x}", bytes.len());
        // SAFETY: inside one part of the test's memory, which only the test
        // and what it runs touch.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.at(offset) as *mut u8, bytes.len()) };
    }

    fn bytes(self, offset: usize, len: usize) -> Vec<u8> {
        // SAFETY: as above.
        unsafe { std::slice::from_raw_parts(self.at(offset) as *const u8, len) }.to_vec()
    }

    /// The title memory: the private memory, then the section's bytes.
    fn title(self) -> Vec<u8> {
        let mut all = self.bytes(0, SIZE);
        all.extend(self.bytes(VIEW_A.0, VIEW));
        all
    }
}

/// Opens a gate of the host queue, at its drop at the latest.
struct Gate(ID3D12Fence);

// SAFETY: D3D12 fences are free-threaded.
unsafe impl Send for Gate {}

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

/// Which copy of the memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Which {
    Sparse,
    Dense,
    /// The copies of the section at its first view and at its second.
    ViewA,
    ViewB,
}

/// The copies of a run.
#[derive(Clone)]
struct Copies {
    sparse: Arc<Shadow>,
    dense: Arc<Shadow>,
    view_a: Arc<Shadow>,
    view_b: Arc<Shadow>,
}

impl Copies {
    fn of(&self, which: Which) -> &Arc<Shadow> {
        match which {
            Which::Sparse => &self.sparse,
            Which::Dense => &self.dense,
            Which::ViewA => &self.view_a,
            Which::ViewB => &self.view_b,
        }
    }
}

/// A step of a list, with what the host GPU does for it. Ranges are offsets
/// in the memory and lengths, each in one part of it.
#[derive(Clone, Debug)]
enum Op {
    /// The copy is brought up to date there.
    Refresh(Which, usize, usize),
    /// Shaders write in the range (declared), the bytes of the part given
    /// (offsets in the range), of the byte given on.
    Write(Which, usize, usize, (usize, usize), u8),
    /// Shaders read the copy there.
    Read(Which, usize, usize),
    /// Shaders write the memory used in place there.
    PlaceWrite(usize, usize, u8),
    /// Shaders read the memory used in place there.
    PlaceRead(usize, usize),
    /// Title memory into the list's upload memory, which shaders read.
    Fill(usize, usize),
    /// The runtime's own work reads title memory into memory of its own,
    /// which shaders read.
    Host(usize, usize),
    /// A title buffer copy: destination, source, bytes.
    Copy(usize, usize, usize),
    /// `CopyMemoryToMemoryX`: the same as work that may write title memory.
    Run(usize, usize, usize),
    /// A bottom-of-pipe write of a value.
    Bottom(usize, u64),
    /// A read-back into title memory, of the byte given on.
    Readback(usize, usize, u8),
}

/// A title submission.
#[derive(Clone, Debug)]
struct Sub {
    /// The title's CPU writes before it is submitted.
    title: Vec<(usize, Vec<u8>)>,
    lists: Vec<Vec<Op>>,
    /// The host GPU does its work as soon as it is sent (else when its
    /// gate opens).
    early: bool,
    /// Its gate opens after this many microseconds (else when the next
    /// submission comes to wait for it).
    delay: Option<u64>,
    /// It submits the lists of the one before again, which may be in the air.
    again: bool,
}

#[derive(Clone, Debug)]
struct Case {
    /// The memory's bytes at the start: this one at offset 0, then on.
    initial: u8,
    subs: Vec<Sub>,
}

/// What a run of a case leaves: what the shaders read, the memory and the
/// copies.
#[derive(PartialEq, Eq)]
struct Outcome {
    reads: Vec<Vec<Vec<u8>>>,
    /// The private memory, then the section's bytes.
    title: Vec<u8>,
    /// The copies as left, unless the run did not keep them or the case is
    /// [`timed`] (then empty).
    sparse: Vec<u8>,
    dense: Vec<u8>,
    view_a: Vec<u8>,
    view_b: Vec<u8>,
    /// Where a copy brought up to date at the end is not title memory.
    stale: Option<String>,
}

/// Whether a gate of `case` but the last one opens after a while. Whether
/// the flight before has landed when a submission's preparation begins is
/// then a matter of timing, with either drain, and so is what the prewarm
/// (which runs only under a flight) brings up to date between the pieces
/// it joins: bytes of the copies no step asks for. The copies of such a
/// case are compared brought up to date only. So are those of a run of the
/// tail drain without the prewarm (its default), which leaves such bytes
/// as they were.
fn timed(case: &Case) -> bool {
    case.subs.iter().rev().skip(1).any(|s| s.delay.is_some())
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    /// A place in `[from, to)`, near a tile's edge more often than not.
    fn at(&mut self, (from, to): (usize, usize)) -> usize {
        if self.chance(60) {
            let tile = from.div_ceil(PAGE) + self.below((to - from) / PAGE + 1);
            let near = (tile * PAGE + self.below(512)).saturating_sub(256);
            if (from..to).contains(&near) {
                return near;
            }
        }
        from + self.below(to - from)
    }

    /// A range inside `within`: small more often than not.
    fn range(&mut self, within: (usize, usize)) -> (usize, usize) {
        let at = self.at((within.0, within.1 - 1));
        let most = within.1 - at;
        let len = match self.below(10) {
            0..=4 => 1 + self.below(most.min(512)),
            5..=7 => 1 + self.below(most.min(3 * PAGE / 2)),
            _ => 1 + self.below(most.min(3 * PAGE)),
        };
        (at, len)
    }

    fn copy(&mut self) -> (Which, (usize, usize)) {
        match self.below(100) {
            0..=44 => (Which::Sparse, (0, SIZE)),
            45..=64 => (Which::Dense, DENSE),
            65..=82 => (Which::ViewA, VIEW_A),
            _ => (Which::ViewB, VIEW_B),
        }
    }

    /// A part of the memory: the private memory, or one of the views.
    fn part(&mut self) -> (usize, usize) {
        match self.below(10) {
            0..=5 => (0, SIZE),
            6 | 7 => VIEW_A,
            _ => VIEW_B,
        }
    }

    /// Memory used in place.
    fn place(&mut self) -> (usize, usize) {
        if self.chance(50) { IN_PLACE } else { VIEW_IN_PLACE }
    }

    /// Steps of a list: a range shaders read or write is brought up to date
    /// first, as draws do.
    fn steps(&mut self, ops: &mut Vec<Op>) {
        match self.below(20) {
            0..=3 => {
                let (copy, within) = self.copy();
                let (at, len) = self.range(within);
                ops.push(Op::Refresh(copy, at, len));
            }
            4..=8 => {
                let (copy, within) = self.copy();
                let (at, len) = self.range(within);
                let from = self.below(len);
                let to = from + 1 + self.below(len - from);
                ops.push(Op::Refresh(copy, at, len));
                ops.push(Op::Write(copy, at, len, (from, to), self.next() as u8));
            }
            9..=10 => {
                // Not of memory used in place: the old drain's prewarm copies
                // what the flight in the air writes there into the copy it
                // reads (copies of such memory are views off their elements,
                // which the world's sessions had none of).
                let (copy, within) = self.copy();
                let (at, len) = self.range(within);
                let len = len.min(4096);
                ops.push(Op::Refresh(copy, at, len));
                if !in_place(at, len) {
                    ops.push(Op::Read(copy, at, len));
                }
            }
            11 => {
                let place = self.place();
                let (at, len) = self.range(place);
                ops.push(Op::PlaceWrite(at, len.min(8192), self.next() as u8));
            }
            12 => {
                let place = self.place();
                let (at, len) = self.range(place);
                ops.push(Op::PlaceRead(at, len.min(4096)));
            }
            13 => {
                let part = self.part();
                let (at, len) = self.range(part);
                ops.push(Op::Fill(at, len.min(8192)));
            }
            14 => {
                let part = self.part();
                let (at, len) = self.range(part);
                ops.push(Op::Host(at, len.min(4096)));
            }
            15 | 16 => {
                // Two ranges apart (in the same bytes too).
                let len = 1 + self.below(8192);
                let (into, out) = (self.part(), self.part());
                let to = into.0 + self.below(into.1 - into.0 - len);
                let from = out.0 + self.below(out.1 - out.0 - len);
                let (t, f) = (same(to), same(from));
                if t + len <= f || f + len <= t {
                    let copy = self.chance(50);
                    ops.push(if copy { Op::Copy(to, from, len) } else { Op::Run(to, from, len) });
                }
            }
            17 => {
                let part = self.part();
                let at = part.0 + self.below((part.1 - part.0) / 8) * 8;
                ops.push(Op::Bottom(at, self.next() << 7 | 5));
            }
            _ => {
                let part = self.part();
                let (at, len) = self.range(part);
                ops.push(Op::Readback(at, len.min(4096), self.next() as u8));
            }
        }
    }

    fn case(&mut self) -> Case {
        let mut subs: Vec<Sub> = Vec::new();
        for _ in 0..2 + self.below(5) {
            // The title's CPU writes nothing the flight in the air uses (at
            // either view): on the console that is a race with its GPU.
            let busy = subs.last().map(touched).unwrap_or_default();
            let mut title = Vec::new();
            for _ in 0..self.below(4) {
                let part = self.part();
                let (at, len) = self.range(part);
                let len = len.min(2048);
                let (a, b) = (same(at), same(at) + len);
                if busy.iter().any(|&(x, y)| x < b && a < y) {
                    continue;
                }
                let byte = self.next() as u8;
                title.push((at, (0..len).map(|i| byte ^ (i as u8)).collect()));
            }
            let again = !subs.is_empty() && self.chance(10);
            let lists = match subs.last() {
                Some(last) if again => last.lists.clone(),
                _ => (0..1 + self.below(2))
                    .map(|_| {
                        let mut ops = Vec::new();
                        for _ in 0..1 + self.below(6) {
                            self.steps(&mut ops);
                        }
                        ops
                    })
                    .collect(),
            };
            let early = self.chance(30);
            let delay = self.chance(25).then(|| self.next() % 2000);
            subs.push(Sub { title, lists, early, delay, again });
        }
        Case { initial: self.next() as u8, subs }
    }
}

/// What a submission's host GPU work and landing touch in title memory, as
/// the bytes at the first view.
fn touched(sub: &Sub) -> Vec<(usize, usize)> {
    let mut all = Vec::new();
    for op in sub.lists.iter().flatten() {
        match *op {
            Op::Read(_, at, len)
            | Op::Write(_, at, len, ..)
            | Op::PlaceRead(at, len)
            | Op::PlaceWrite(at, len, _)
            | Op::Readback(at, len, _) => all.push((same(at), same(at) + len)),
            Op::Bottom(at, _) => all.push((same(at), same(at) + 8)),
            _ => {}
        }
    }
    all
}

/// The memory and its copies, made once for every case of a test (tiles of
/// sparse copies are never given back).
struct Bench {
    host: &'static Host,
    region: Region,
    section: Section,
    sparse: Arc<Shadow>,
    lists: Vec<Arc<HostList>>,
}

impl Drop for Bench {
    fn drop(&mut self) {
        let places = self.in_place();
        crate::shadow::IN_PLACE_FOR_TEST.lock().unwrap().retain(|r| !places.contains(r));
    }
}

impl Bench {
    fn new(host: &'static Host) -> Option<Bench> {
        let region = Region::new();
        let Some(sparse) = crate::shadow::sparse_for_test(region.0 as u64, SIZE as u64) else {
            eprintln!("no sparse D3D12 buffer; skipped");
            return None;
        };
        let lists = (0..12)
            .map(|_| {
                let list = host.new_list().unwrap();
                list.close();
                Arc::new(list)
            })
            .collect();
        let bench = Bench { host, region, section: Section::new(), sparse, lists };
        crate::shadow::IN_PLACE_FOR_TEST.lock().unwrap().extend(bench.in_place());
        Some(bench)
    }

    fn space(&self) -> Space {
        Space { base: self.region.0, a: self.section.a, b: self.section.b }
    }

    /// The memory taken for used in place, by address.
    fn in_place(&self) -> [(usize, usize); 2] {
        let space = self.space();
        [IN_PLACE, VIEW_IN_PLACE].map(|(a, b)| (space.at(a), space.at(a) + (b - a)))
    }

    /// Whether nothing is pending in the copies after a run.
    fn settled(&self) -> bool {
        self.sparse.pending_for_test().is_empty() && self.sparse.kept_for_test() == 0
    }

    /// Runs `case` in `mode` with `rule` off; `raw`: the copies as left are
    /// compared too, where the case is not [`timed`].
    fn run(&self, case: &Case, mode: Mode, rule: Option<Mutation>, raw: bool) -> Outcome {
        force(Some(mode));
        mutate(rule);
        let outcome = self.run_here(case, raw);
        force(None);
        mutate(None);
        outcome
    }

    fn run_here(&self, case: &Case, raw: bool) -> Outcome {
        let host = self.host;
        let space = self.space();
        let line = host.line();
        let initial: Vec<u8> =
            (0..SIZE + VIEW).map(|i| case.initial.wrapping_add((i / 61) as u8)).collect();
        space.write(0, &initial[..SIZE]);
        space.write(VIEW_A.0, &initial[SIZE..]);
        self.sparse.reset_for_test();
        let fresh = |(from, to): (usize, usize)| Shadow::for_test(host, space.at(from), (to - from) as u64).0;
        let copies = Copies {
            sparse: self.sparse.clone(),
            dense: fresh(DENSE),
            view_a: fresh(VIEW_A),
            view_b: fresh(VIEW_B),
        };
        // What the shaders' work needs of each submission: where fills and
        // the runtime's work put title memory, and where read-backs come
        // from (kept until the end).
        let mut kept: Vec<Box<[u8]>> = Vec::new();
        let go = Arc::new(AtomicBool::new(false));
        let reads: Arc<Mutex<Vec<Vec<Vec<u8>>>>> =
            Arc::new(Mutex::new(vec![Vec::new(); case.subs.len()]));
        // A submission's number, its gate, its work, when it is done and its
        // gate opens, and the waits begun when it was sent.
        type Job = (usize, Gate, Vec<(Op, usize)>, bool, Option<u64>, u64);
        let (jobs, take) = mpsc::channel::<Job>();
        let (done, finished) = mpsc::channel::<usize>();
        let (mut ops, mut used): (Vec<(Op, usize)>, Vec<Arc<HostList>>) = (Vec::new(), Vec::new());
        let gpu = {
            let (go, reads, line, copies) = (go.clone(), reads.clone(), line.clone(), copies.clone());
            std::thread::spawn(move || {
                for (k, gate, ops, early, delay, waits) in take {
                    let run = |ops: &[(Op, usize)]| {
                        let mut seen = Vec::new();
                        for (op, at) in ops {
                            shaders(op, *at, space, &copies, &mut seen);
                        }
                        reads.lock().unwrap()[k].extend(seen);
                    };
                    if early {
                        run(&ops);
                        done.send(k).unwrap();
                    }
                    match delay {
                        Some(us) => std::thread::sleep(std::time::Duration::from_micros(us)),
                        None => until(|| line.waits_begun() > waits || go.load(Ordering::Acquire)),
                    }
                    if !early {
                        run(&ops);
                    }
                    gate.open();
                }
            })
        };
        for (k, sub) in case.subs.iter().enumerate() {
            for (at, bytes) in &sub.title {
                space.write(*at, bytes);
            }
            let gate = Gate(host.gate());
            if !sub.again {
                ops.clear();
                used.clear();
                for (i, steps) in sub.lists.iter().enumerate() {
                    let list = &self.lists[2 * k + i];
                    list.reset();
                    for op in steps {
                        let at = record(list, op, space, &copies, &mut kept);
                        ops.push((op.clone(), at));
                    }
                    list.count_work();
                    list.close();
                    used.push(list.clone());
                }
            }
            host.execute_title(&used);
            jobs.send((k, gate, ops.clone(), sub.early, sub.delay, line.waits_begun())).unwrap();
            if sub.early {
                assert_eq!(finished.recv().unwrap(), k);
            }
        }
        go.store(true, Ordering::Release);
        drop(jobs);
        gpu.join().unwrap();
        line.drain(Count::WaitHeld);
        let held = |shadow: &Shadow, (from, to): (usize, usize), step: usize| -> Vec<u8> {
            (from..to)
                .step_by(step)
                .flat_map(|at| {
                    let n = step.min(to - at);
                    let bytes = shadow.view_for_test(space.at(at));
                    // SAFETY: the copy's mapping (of a sparse one, a tile,
                    // whole); nothing runs.
                    unsafe { std::slice::from_raw_parts(bytes, n) }.to_vec()
                })
                .collect()
        };
        let all = |copies: &Copies| {
            (
                held(&copies.sparse, (0, SIZE), PAGE),
                held(&copies.dense, DENSE, DENSE.1 - DENSE.0),
                held(&copies.view_a, VIEW_A, VIEW),
                held(&copies.view_b, VIEW_B, VIEW),
            )
        };
        let (mut sparse, mut dense, mut view_a, mut view_b) = all(&copies);
        let title = space.title();
        // Brought up to date, the copies are title memory: no part of them
        // is taken for up to date that is not.
        copies.sparse.update_lazily(space.at(0), SIZE, false);
        copies.dense.update_lazily(space.at(DENSE.0), DENSE.1 - DENSE.0, false);
        copies.view_a.update_lazily(space.at(VIEW_A.0), VIEW, false);
        copies.view_b.update_lazily(space.at(VIEW_B.0), VIEW, false);
        let now = all(&copies);
        let first = |x: &[u8], y: &[u8]| x.iter().zip(y).position(|(x, y)| x != y);
        // Each copy against the title memory it holds: where it starts in
        // the memory, and in `title` (the section's bytes once).
        let held_now = [
            ("sparse", &now.0, 0, 0),
            ("dense", &now.1, DENSE.0, DENSE.0),
            ("first view's", &now.2, VIEW_A.0, SIZE),
            ("second view's", &now.3, VIEW_B.0, SIZE),
        ];
        let stale = held_now.iter().find_map(|&(name, copy, at, from)| {
            first(copy, &title[from..from + copy.len()]).map(|i| {
                let (x, y) = (copy[i], title[from + i]);
                format!("the {name} copy brought up to date at {:#x}: {x:#x}, title {y:#x}", at + i)
            })
        });
        if !raw || timed(case) {
            (sparse, dense, view_a, view_b) = Default::default();
        }
        let reads = std::mem::take(&mut *reads.lock().unwrap());
        drop(kept);
        Outcome { reads, title, sparse, dense, view_a, view_b, stale }
    }
}

/// Records `op` into `list`: what the host GPU's work for it needs.
fn record(list: &HostList, op: &Op, space: Space, copies: &Copies, kept: &mut Vec<Box<[u8]>>) -> usize {
    let mut keep = |len: usize| {
        kept.push(vec![0; len].into_boxed_slice());
        kept.last_mut().unwrap().as_mut_ptr() as usize
    };
    match *op {
        Op::Refresh(which, at, len) => copies.of(which).refresh(list, space.at(at) as u64, len as u64),
        Op::Write(which, at, len, ..) => {
            copies.of(which).gpu_writes(list, space.at(at) as u64, len as u64);
        }
        Op::Fill(at, len) => {
            let (_, to) = list.scratch(len as u64).expect("upload memory");
            list.defer_fill(to, space.at(at) as *const u8, len);
            return to as usize;
        }
        Op::Host(at, len) => {
            let (to, from) = (keep(len), space.at(at));
            // SAFETY: title memory and the test's buffer, which outlives
            // the submission.
            list.defer_host(move || unsafe {
                std::ptr::copy_nonoverlapping(from as *const u8, to as *mut u8, len);
            });
            return to;
        }
        Op::Copy(to, from, len) => {
            list.defer_copy(space.at(to) as *mut u8, space.at(from) as *const u8, len);
        }
        Op::Run(to, from, len) => {
            let (to, from) = (space.at(to), space.at(from));
            // SAFETY: title memory of the test's, apart (in its bytes too); a
            // guarded view block opens on the fault.
            list.defer(move || unsafe { std::ptr::copy(from as *const u8, to as *mut u8, len) });
        }
        Op::Bottom(at, value) => list.defer_write(space.at(at) as *mut u8, Written::Value(value), 8),
        Op::Readback(at, len, _) => {
            let from = keep(len);
            list.defer_readback(space.at(at) as *mut u8, from as *const u8, len, len, 1, len);
            return from;
        }
        Op::Read(..) | Op::PlaceWrite(..) | Op::PlaceRead(..) => {}
    }
    0
}

/// The host GPU's work for `op` (`at`: what [`record`] gave): what it
/// reads goes into `seen`.
fn shaders(op: &Op, at: usize, space: Space, copies: &Copies, seen: &mut Vec<Vec<u8>>) {
    // SAFETY: the copies' mappings, title memory used in place and the
    // test's buffers, each byte by one side at a time; a guarded view block
    // of memory used in place opens on the fault.
    unsafe {
        match *op {
            Op::Write(which, start, _, (from, to), byte) => {
                let copy = copies.of(which);
                for (i, offset) in (start + from..start + to).enumerate() {
                    copy.view_for_test(space.at(offset)).write_volatile(byte ^ (i as u8));
                }
            }
            Op::Read(which, start, len) => {
                let copy = copies.of(which);
                let span = start..start + len;
                seen.push(span.map(|o| copy.view_for_test(space.at(o)).read_volatile()).collect());
            }
            Op::PlaceWrite(start, len, byte) => {
                for i in 0..len {
                    (space.at(start + i) as *mut u8).write_volatile(byte.wrapping_add(i as u8));
                }
            }
            Op::PlaceRead(start, len) => {
                seen.push(std::slice::from_raw_parts(space.at(start) as *const u8, len).to_vec());
            }
            Op::Fill(_, len) | Op::Host(_, len) => {
                seen.push(std::slice::from_raw_parts(at as *const u8, len).to_vec());
            }
            Op::Readback(_, len, byte) => {
                for i in 0..len {
                    let value = byte.wrapping_mul(3).wrapping_add(i as u8);
                    ((at + i) as *mut u8).write_volatile(value);
                }
            }
            Op::Refresh(..) | Op::Copy(..) | Op::Run(..) | Op::Bottom(..) => {}
        }
    }
}

fn until(what: impl Fn() -> bool) {
    let started = Instant::now();
    while !what() {
        assert!(started.elapsed() < Duration::from_secs(20), "never came");
        std::thread::yield_now();
    }
}

/// The case, one line a step, with the steps that touch `near` (at either
/// view) marked.
fn told(case: &Case, near: Option<usize>) -> String {
    let mut text = format!("initial {}\n", case.initial);
    for (k, sub) in case.subs.iter().enumerate() {
        let titles: Vec<String> =
            sub.title.iter().map(|(at, b)| format!("{at:#x}+{:#x}", b.len())).collect();
        let (early, delay, again, titles) = (sub.early, sub.delay, sub.again, titles.join(" "));
        text += &format!("{k}: early {early} delay {delay:?} again {again} title {titles}\n");
        for (i, list) in sub.lists.iter().enumerate() {
            for op in list {
                let spans = match *op {
                    Op::Refresh(_, at, len)
                    | Op::Write(_, at, len, ..)
                    | Op::Read(_, at, len)
                    | Op::PlaceWrite(at, len, _)
                    | Op::PlaceRead(at, len)
                    | Op::Fill(at, len)
                    | Op::Host(at, len)
                    | Op::Readback(at, len, _) => vec![(at, len)],
                    Op::Copy(to, from, len) | Op::Run(to, from, len) => vec![(to, len), (from, len)],
                    Op::Bottom(at, _) => vec![(at, 8)],
                };
                let near = near.is_some_and(|n| {
                    spans.iter().any(|&(at, len)| same(at) <= same(n) && same(n) < same(at) + len)
                });
                let mark = if near { "* " } else { "  " };
                text += &format!("  {mark}{i} {op:x?}\n");
            }
        }
    }
    text
}

/// Where two outcomes first differ.
fn differ(a: &Outcome, b: &Outcome) -> Option<String> {
    for (k, (x, y)) in a.reads.iter().zip(&b.reads).enumerate() {
        if let Some(i) = x.iter().zip(y).position(|(x, y)| x != y) {
            return Some(format!("submission {k}, read {i}"));
        }
        if x.len() != y.len() {
            return Some(format!("submission {k}: {} reads against {}", x.len(), y.len()));
        }
    }
    let first = |x: &[u8], y: &[u8]| x.iter().zip(y).position(|(x, y)| x != y);
    if let Some(at) = first(&a.title, &b.title) {
        let (x, y) = (a.title[at], b.title[at]);
        return Some(format!("title memory at {at:#x}: {x:#x} against {y:#x}"));
    }
    if let Some(stale) = &a.stale {
        return Some(format!("in the first run, {stale}"));
    }
    if let Some(stale) = &b.stale {
        return Some(stale.clone());
    }
    // Each copy as left: its name, where it starts in the memory and in
    // `title`.
    let copies = [
        ("sparse", &a.sparse, &b.sparse, 0, 0),
        ("dense", &a.dense, &b.dense, DENSE.0, DENSE.0),
        ("first view's", &a.view_a, &b.view_a, VIEW_A.0, SIZE),
        ("second view's", &a.view_b, &b.view_b, VIEW_B.0, SIZE),
    ];
    copies.iter().find_map(|&(name, x, y, at, from)| {
        first(x, y).map(|i| {
            let title = a.title[from + i];
            format!("the {name} copy at {:#x}: {:#x} against {:#x} (title {title:#x})", at + i, x[i], y[i])
        })
    })
}

/// The offset `differ` names, if any.
fn offset(text: &str) -> Option<usize> {
    let at = text.split(" at 0x").nth(1)?;
    usize::from_str_radix(at.split(|c: char| !c.is_ascii_hexdigit()).next()?, 16).ok()
}

/// Seeds to try: `VYCE_TAIL_SEEDS`, or `default`.
fn seeds(default: u64) -> u64 {
    std::env::var("VYCE_TAIL_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// The hosts to run on: WARP, and the machine's own GPU when `hardware`.
fn hosts(hardware: bool) -> Vec<&'static Host> {
    if !crate::flight::late_signal() {
        return Vec::new();
    }
    let mut hosts: Vec<&'static Host> =
        Host::new_warp().map(Host::overlapping).into_iter().collect();
    if hardware {
        hosts.extend(Host::new_hardware().map(Host::overlapping));
    }
    hosts
}

/// The tail drain and the strict one leave what the old drain leaves, over
/// thousands of random cases (`VYCE_TAIL_SEEDS` for more), and every
/// submission after the first of a case is prepared under the one before.
/// The tail drains are prewarmed in a case of three (`VYCE_TAIL_PREWARM=1`),
/// whose copies as left are compared too where no gate opens after a while.
/// The old drain's dry run, on every other case, changes nothing, and where
/// no gate opens after a while it foretells what the tail drain does.
#[test]
fn the_tail_drain_leaves_what_the_old_drain_leaves() {
    for (h, host) in hosts(true).into_iter().enumerate() {
        let Some(bench) = Bench::new(host) else { return };
        let n = if h == 0 { seeds(2500) } else { seeds(2500) / 5 };
        events();
        for seed in 0..n {
            let case = Rng(seed * 7919 + 17).case();
            let dry = seed % 2 == 0;
            force_dry(Some(dry));
            let old = bench.run(&case, Mode::Old, None, true);
            force_dry(None);
            assert!(bench.settled(), "seed {seed}: left pending by the old drain");
            let by_old = events();
            assert!(!by_old.contains(&Event::Under));
            let mode = if seed % 5 == 4 { Mode::Strict } else { Mode::Tail };
            let warm = seed % 3 == 0;
            force_prewarm(Some(warm));
            let tail = bench.run(&case, mode, None, warm);
            force_prewarm(None);
            assert!(bench.settled(), "seed {seed}: left pending by the tail drain");
            // (A flight whose gate opens after a while may have landed, and
            // lists submitted again take the old drain.)
            let events = events();
            let under = events.iter().filter(|e| **e == Event::Under).count();
            let held = case.subs.windows(2).filter(|w| w[0].delay.is_none() && !w[1].again).count();
            let most = case.subs.len();
            let text = "prepared under the one before";
            assert!(under >= held && under < most, "seed {seed}: {under} {text}");
            let again = case.subs.windows(2).filter(|w| w[0].delay.is_none() && w[1].again).count();
            let lists = events.iter().filter(|e| **e == Event::Old(Old::List)).count();
            assert!(lists >= again, "seed {seed}: {lists} lists in the air took the old drain");
            if dry {
                let foretold = by_old.iter().filter(|e| **e == Event::DryUnder).count();
                let text = "counted by the dry run";
                assert!(foretold >= held && foretold < most, "seed {seed}: {foretold} {text}");
            }
            if dry && mode == Mode::Tail && !timed(&case) {
                let foretold: Vec<Event> = by_old
                    .iter()
                    .filter_map(|e| match *e {
                        Event::DryUnder => Some(Event::Under),
                        Event::DryLate => Some(Event::Late),
                        Event::DryFlush(why) => Some(Event::Waited(why)),
                        _ => None,
                    })
                    .collect();
                let did: Vec<Event> = events
                    .iter()
                    .copied()
                    .filter(|e| match e {
                        Event::Under | Event::Late => true,
                        Event::Waited(why) => *why != Why::Tail,
                        _ => false,
                    })
                    .collect();
                assert_eq!(foretold, did, "seed {seed}: the dry run against the tail drain");
            }
            if let Some(at) = differ(&old, &tail) {
                let case = told(&case, offset(&at));
                panic!("host {h}, seed {seed}, {mode:?} (prewarm {warm}): not as with the old drain: {at}\n{case}");
            }
        }
    }
}

/// With any one of its rules off, the tail drain leaves something else than
/// the old drain for some case: the test above sees each hazard. (The tail
/// drain is prewarmed here, so that the copies as left are compared too.)
#[test]
fn each_rule_of_the_tail_drain_is_needed() {
    let Some(host) = hosts(false).into_iter().next() else { return };
    let Some(bench) = Bench::new(host) else { return };
    use Mutation::{Copies, Dense, Late, Queue, Tags, Views};
    let tail = |case: &Case, rule: Option<Mutation>| {
        force_prewarm(Some(true));
        let tail = bench.run(case, Mode::Tail, rule, true);
        force_prewarm(None);
        tail
    };
    for rule in [Late, Queue, Copies, Dense, Tags, Views] {
        let found = (0..500u64).find(|&seed| {
            let case = Rng(seed * 104_729 + 3).case();
            let old = bench.run(&case, Mode::Old, None, true);
            differ(&old, &tail(&case, Some(rule))).is_some()
        });
        assert!(found.is_some(), "{rule:?} off: no case of 500 differs");
        let seed = found.unwrap();
        // The rule's doing: with it on the case is as with the old drain.
        let case = Rng(seed * 104_729 + 3).case();
        let old = bench.run(&case, Mode::Old, None, true);
        let at = differ(&old, &tail(&case, None));
        assert!(at.is_none(), "{rule:?}: seed {seed} differs with every rule on: {at:?}");
        eprintln!("{rule:?} off: seed {seed} differs");
    }
    events();
}

/// Marks join what a landing writes and tell what meets it, with what is
/// put behind the flight later and what its shaders write unlisted.
#[test]
fn marks_tell_what_a_landing_writes() {
    let marks = crate::flight::Marks::new(
        7,
        vec![(100, 200), (150, 300), (400, 400), (500, 600)],
        vec![9, 3, 9],
        vec![(1000, 1100)],
    );
    assert!(marks.meets(299, 300) && marks.meets(0, 101) && marks.meets(599, 1000));
    assert!(!marks.meets(300, 400) && !marks.meets(0, 100));
    assert!(!marks.meets(400, 401) && !marks.meets(600, 1000));
    assert!(marks.meets(1050, 1051) && !marks.meets(1100, 1200));
    assert!(marks.holds(3) && marks.holds(9) && !marks.holds(4));
    // What is put behind the newest flight meets too (flights carry marks
    // where the tail drain is on, or counted).
    let Some(host) = hosts(false).into_iter().next() else { return };
    let line = host.line();
    let gate = Gate(host.gate());
    let list = host.new_list().unwrap();
    list.close();
    force(Some(Mode::Tail));
    host.send_behind(&Arc::new(list));
    force(None);
    let value = Box::new(0u64);
    let at = &raw const *value as usize;
    let air = Air::new(line.air(), line.clone(), false);
    assert!(!air.meets(at, at + 8));
    assert!(line.after(crate::flight::After::Write(at, Written::Value(3), 8)).is_ok());
    assert!(air.meets(at, at + 4) && !air.meets(at + 8, at + 16));
    gate.open();
    line.drain(Count::WaitHeld);
    // SAFETY: written by the landing, which is done.
    assert_eq!(unsafe { (at as *const u64).read_volatile() }, 3);
    assert!(!air.meets(at, at + 8), "landed");
    assert!(Air::new(air.marks.clone(), line.clone(), true).meets(at, at + 8), "the dry run's air");
}

/// A copy of `len` bytes of fresh title memory holding `fill`, the memory,
/// and where the copy begins.
fn dense_copy(host: &Host, len: usize, fill: u8) -> (Arc<Shadow>, Vec<u8>, usize) {
    let mut title = vec![fill; len + PAGE];
    let base = (title.as_mut_ptr() as usize).next_multiple_of(PAGE);
    let (shadow, _) = Shadow::for_test(host, base, len as u64);
    (shadow, title, base)
}

/// Ranges shaders write carry their submission's fence value: a landing
/// opens only its own, and a refresh goes around another's still.
#[test]
fn a_landing_opens_only_its_own_ranges() {
    let Some(host) = Host::new_warp() else { return };
    let (shadow, mut title, base) = dense_copy(&host, PAGE, 1);
    let skip = base - title.as_ptr() as usize;
    shadow.writing(base + 100, 200, 5);
    shadow.writing(base + 100, 200, 6);
    shadow.copy_back(base + 100, 200, 5);
    assert_eq!(shadow.pending_for_test(), [(base + 100, base + 300, 6)]);
    title[skip..skip + PAGE].fill(2);
    shadow.update(base, PAGE);
    // SAFETY: the copy's mapping; nothing runs.
    let held = |at: usize| unsafe { shadow.view_for_test(base + at).read() };
    let around = (held(99), held(100), held(299), held(300));
    assert_eq!(around, (2, 1, 1, 2), "around the other's range");
    assert!(shadow.has_other_pending(5) && !shadow.has_other_pending(6));
    shadow.copy_back(base + 100, 200, 6);
    assert!(shadow.pending_for_test().is_empty());
    drop(title);
}

/// A refresh done after the wait goes around the ranges its submission had
/// closed by its own step, and no others of its own.
#[test]
fn a_late_refresh_goes_around_what_was_closed_before_it() {
    let Some(host) = Host::new_warp() else { return };
    let (shadow, mut title, base) = dense_copy(&host, PAGE, 1);
    let skip = base - title.as_ptr() as usize;
    // Closed before the refresh's step, and after it.
    shadow.close(base + 1000, 100, 9);
    let around = shadow.own_pending(base, PAGE, 9);
    assert_eq!(around, [(base + 1000, base + 1100)]);
    shadow.close(base + 3000, 100, 9);
    // Another submission's (none is in the air by then, in the field).
    shadow.close(base + 5000, 100, 8);
    title[skip..skip + PAGE].fill(4);
    shadow.update_around(base, PAGE, false, &around, 9);
    // SAFETY: the copy's mapping; nothing runs.
    let held = |at: usize| unsafe { shadow.view_for_test(base + at).read() };
    assert_eq!([held(999), held(1000), held(1099), held(1100)], [4, 1, 1, 4]);
    let after = [held(3000), held(3099)];
    assert_eq!(after, [4, 4], "closed after the step: refreshed, as the old order did");
    assert_eq!([held(5000), held(5099), held(5100)], [1, 1, 4], "another submission's: left alone");
    drop(title);
}

/// A landing gives back the tile snapshots of its ranges that nothing
/// still pending touches, and the rest once nothing is.
#[test]
fn a_landing_gives_back_the_tiles_no_one_needs() {
    let region = Region::new();
    let Some(root) = crate::shadow::sparse_for_test(region.0 as u64, SIZE as u64) else { return };
    let range = |from: usize, to: usize| (root.clone(), region.at(from), to - from);
    let a = [range(100, PAGE + 100)];
    let b = [range(PAGE + 200, 3 * PAGE)];
    root.close_to_refreshes(a[0].1, a[0].2, 1);
    crate::shadow::pass::keep_before(&a);
    assert_eq!(root.kept_for_test(), 2);
    // The next submission closes its range meanwhile, its snapshot later.
    root.close_to_refreshes(b[0].1, b[0].2, 2);
    crate::shadow::pass::copy_back(&a, 1);
    assert_eq!(root.pending_for_test(), [(b[0].1, b[0].1 + b[0].2, 2)]);
    assert_eq!(root.kept_for_test(), 1, "tile 1, which the next submission's range touches");
    crate::shadow::pass::keep_before(&b);
    assert_eq!(root.kept_for_test(), 2, "tile 1 again, and tile 2");
    crate::shadow::pass::copy_back(&b, 2);
    assert_eq!(root.kept_for_test(), 0);
}

/// A title submission prepared while the one before is held behind a gate:
/// a refresh no landing writes is done before the gate opens, one that the
/// landing writes after it, and the lists go to the host queue only once
/// the flight before has landed.
#[test]
fn a_submission_is_prepared_under_the_flight_before() {
    for host in hosts(true) {
        let Some(bench) = Bench::new(host) else { return };
        let (region, sparse, line) = (&bench.region, &bench.sparse, host.line());
        let (dense, _) = Shadow::for_test(host, region.at(DENSE.0), (DENSE.1 - DENSE.0) as u64);
        sparse.reset_for_test();
        let near = DENSE.0 + 64;
        // The flight: its shaders write `near` in the sparse copy.
        let first = &bench.lists[0];
        first.reset();
        sparse.refresh(first, region.at(near) as u64, 64);
        sparse.gpu_writes(first, region.at(near) as u64, 64);
        first.close();
        let gate = Gate(host.gate());
        force(Some(Mode::Tail));
        host.execute_title(std::slice::from_ref(first));
        // The next: the dense copy there (the landing writes it), and the
        // sparse one far from it, after the title wrote both.
        let far = DENSE.1 - 4096;
        region.write(near, &[0x5a; 64]);
        region.write(far, &[0x6b; 64]);
        let second = &bench.lists[1];
        second.reset();
        dense.refresh(second, region.at(near) as u64, 64);
        sparse.refresh(second, region.at(far) as u64, 64);
        second.count_work();
        second.close();
        let waits = line.waits_begun();
        let (next, done) = (second.clone(), Arc::new(AtomicBool::new(false)));
        let sent = {
            let done = done.clone();
            std::thread::spawn(move || {
                force(Some(Mode::Tail));
                host.execute_title(std::slice::from_ref(&next));
                done.store(true, Ordering::Release);
                events()
            })
        };
        until(|| line.waits_begun() > waits);
        std::thread::sleep(Duration::from_millis(30));
        // SAFETY: the copies' mappings; the GPU's work is the test's.
        let held = |shadow: &Shadow, at: usize| {
            // SAFETY: as above.
            unsafe { shadow.view_for_test(region.at(at)).read_volatile() }
        };
        assert_eq!(held(sparse, far), 0x6b, "refreshed under the flight");
        assert_ne!(held(&dense, near + 8), 0x77, "the flight's write before it ran");
        assert!(!done.load(Ordering::Acquire), "sent before the flight landed");
        // The flight's shaders write, and it lands.
        // SAFETY: as above.
        unsafe { sparse.view_for_test(region.at(near) + 8).write_volatile(0x77) };
        gate.open();
        let seen = sent.join().unwrap();
        assert_eq!(seen, [Event::Under, Event::Late, Event::Waited(Why::Tail)]);
        assert_eq!((held(&dense, near), held(&dense, near + 8)), (0x5a, 0x77), "after its landing");
        line.drain(Count::WaitHeld);
        force(None);
        events();
    }
}

/// The section's bytes at `offset` (at the first view) set to `byte`, and
/// fresh copies of it at its two views.
fn view_copies(bench: &Bench, offset: usize, len: usize, byte: u8) -> (Arc<Shadow>, Arc<Shadow>) {
    let space = bench.space();
    space.write(VIEW_A.0 + offset, &vec![byte; len]);
    let fresh = |at: usize| Shadow::for_test(bench.host, space.at(at), VIEW as u64).0;
    let (a, b) = (fresh(VIEW_A.0), fresh(VIEW_B.0));
    // Copied and guarded, as copies the title's memory has had for a while.
    a.update(space.at(VIEW_A.0), VIEW);
    b.update(space.at(VIEW_B.0), VIEW);
    (a, b)
}

/// What a landing writes through one view of a section, a submission
/// prepared under the flight refreshes through the other after the wait for
/// it, as the old drain did: its shaders read the flight's results. Without
/// the rule (the refresh looks at its own addresses only) they read what
/// was there before, until a later submission's refresh.
#[test]
fn a_refresh_through_another_view_waits_for_the_landing() {
    for host in hosts(true) {
        let Some(bench) = Bench::new(host) else { return };
        assert_eq!(through_views(&bench, None), (0x77, 0x77));
        assert_eq!(through_views(&bench, Some(Mutation::Views)), (0x11, 0x77), "the rule is needed");
    }
}

/// The second view's copy of what the flight's shaders wrote at the first
/// view, after the next submission's refresh, and once brought up to date
/// again, with `rule` off.
fn through_views(bench: &Bench, rule: Option<Mutation>) -> (u8, u8) {
    let (host, space, line) = (bench.host, bench.space(), bench.host.line());
    let at = VIEW_A.0 + PAGE / 2;
    let (view_a, view_b) = view_copies(bench, PAGE / 2, 64, 0x11);
    // The flight: its shaders write the first view's copy.
    let first = &bench.lists[0];
    first.reset();
    view_a.refresh(first, space.at(at) as u64, 64);
    view_a.gpu_writes(first, space.at(at) as u64, 64);
    first.close();
    let gate = Gate(host.gate());
    force(Some(Mode::Tail));
    host.execute_title(std::slice::from_ref(first));
    force(None);
    // SAFETY: the copy's mapping (the flight's shaders').
    unsafe { view_a.view_for_test(space.at(at) + 8).write_volatile(0x77) };
    // The next: the same bytes through the second view.
    let other = at + VIEW;
    let second = &bench.lists[1];
    second.reset();
    view_b.refresh(second, space.at(other) as u64, 64);
    second.count_work();
    second.close();
    let waits = line.waits_begun();
    let next = second.clone();
    let sent = std::thread::spawn(move || {
        force(Some(Mode::Tail));
        mutate(rule);
        host.execute_title(std::slice::from_ref(&next));
        events()
    });
    until(|| line.waits_begun() > waits);
    gate.open();
    let seen = sent.join().unwrap();
    let late = if rule.is_none() { vec![Event::Late] } else { Vec::new() };
    assert_eq!(seen, [vec![Event::Under], late, vec![Event::Waited(Why::Tail)]].concat());
    line.drain(Count::WaitHeld);
    events();
    // SAFETY: the copy's mapping; nothing runs.
    let held = || unsafe { view_b.view_for_test(space.at(other) + 8).read_volatile() };
    let read = held();
    // The landing opened the second view too: the next refresh finds it.
    view_b.update(space.at(other), 64);
    (read, held())
}

/// Memory the host GPU uses in place at one view of a section: a submission
/// prepared under the flight refreshes the copy of the other view after the
/// wait for it, as for the memory itself. Without the rule the copy holds
/// what was there before the flight's shaders wrote.
#[test]
fn a_refresh_of_another_view_of_memory_used_in_place_waits() {
    for host in hosts(true) {
        let Some(bench) = Bench::new(host) else { return };
        assert_eq!(in_place_through_views(&bench, None), 0x66);
        assert_eq!(in_place_through_views(&bench, Some(Mutation::Views)), 0x22, "the rule is needed");
    }
}

/// The second view's copy, after the next submission's refresh, of what the
/// flight's shaders wrote in place at the first view, with `rule` off.
fn in_place_through_views(bench: &Bench, rule: Option<Mutation>) -> u8 {
    let (host, space, line) = (bench.host, bench.space(), bench.host.line());
    let at = VIEW_IN_PLACE.0 + 512;
    let (_, view_b) = view_copies(bench, at - VIEW_A.0, 64, 0x22);
    // The flight: its shaders write the memory in place (not a copy).
    let first = &bench.lists[0];
    first.reset();
    first.count_work();
    first.close();
    let gate = Gate(host.gate());
    force(Some(Mode::Tail));
    host.execute_title(std::slice::from_ref(first));
    force(None);
    let other = at + VIEW;
    let second = &bench.lists[1];
    second.reset();
    view_b.refresh(second, space.at(other) as u64, 64);
    second.count_work();
    second.close();
    let waits = line.waits_begun();
    let next = second.clone();
    let sent = std::thread::spawn(move || {
        force(Some(Mode::Tail));
        mutate(rule);
        host.execute_title(std::slice::from_ref(&next));
        events()
    });
    until(|| line.waits_begun() > waits);
    // The flight's shaders write in place, and it lands.
    // SAFETY: the section's memory, written by the flight's shaders only
    // (a guarded block opens on the fault).
    unsafe { (space.at(at + 8) as *mut u8).write_volatile(0x66) };
    gate.open();
    let seen = sent.join().unwrap();
    let late = if rule.is_none() { vec![Event::Late] } else { Vec::new() };
    assert_eq!(seen, [vec![Event::Under], late, vec![Event::Waited(Why::Tail)]].concat());
    line.drain(Count::WaitHeld);
    events();
    // SAFETY: the copy's mapping; nothing runs.
    unsafe { view_b.view_for_test(space.at(other) + 8).read_volatile() }
}

/// Without the switch a submission prepared under a flight is not
/// prewarmed: bytes between its steps' ranges that no step asks for stay
/// as they were; with `VYCE_TAIL_PREWARM=1` the prewarm brings them up to
/// date first, as before.
#[test]
fn the_tail_drain_does_without_the_prewarm() {
    for host in hosts(true) {
        let Some(bench) = Bench::new(host) else { return };
        assert_eq!(prewarmed(&bench, Some(false)), 0x31, "not prewarmed");
        assert_eq!(prewarmed(&bench, Some(true)), 0x42, "prewarmed");
        // As the switch says (not, unless VYCE_TAIL_PREWARM=1).
        let switch = if prewarm() { 0x42 } else { 0x31 };
        assert_eq!(prewarmed(&bench, None), switch);
    }
}

/// The dense copy's byte between two refreshes of a submission prepared
/// under a flight (a declaration of shader writes between them), after the
/// title wrote it, with the prewarm as `warm` says.
fn prewarmed(bench: &Bench, warm: Option<bool>) -> u8 {
    let (host, region, line) = (bench.host, &bench.region, bench.host.line());
    region.write(DENSE.0, &[0x31; 1024]);
    let (dense, _) = Shadow::for_test(host, region.at(DENSE.0), (DENSE.1 - DENSE.0) as u64);
    let first = &bench.lists[0];
    first.reset();
    first.count_work();
    first.close();
    let gate = Gate(host.gate());
    force(Some(Mode::Tail));
    host.execute_title(std::slice::from_ref(first));
    force(None);
    region.write(DENSE.0, &[0x42; 1024]);
    let second = &bench.lists[1];
    second.reset();
    let at = |offset: usize| region.at(DENSE.0 + offset) as u64;
    dense.refresh(second, at(0), 100);
    dense.gpu_writes(second, at(0), 100);
    dense.refresh(second, at(200), 100);
    second.count_work();
    second.close();
    let waits = line.waits_begun();
    let next = second.clone();
    let sent = std::thread::spawn(move || {
        force(Some(Mode::Tail));
        force_prewarm(warm);
        host.execute_title(std::slice::from_ref(&next));
        events()
    });
    until(|| line.waits_begun() > waits);
    gate.open();
    assert_eq!(sent.join().unwrap(), [Event::Under, Event::Waited(Why::Tail)]);
    line.drain(Count::WaitHeld);
    events();
    // SAFETY: the copy's mapping; nothing runs.
    let held = |offset: usize| unsafe { dense.view_for_test(region.at(DENSE.0 + offset)).read_volatile() };
    assert_eq!((held(0), held(250)), (0x42, 0x42), "the steps' ranges");
    held(150)
}

/// The bounce slices of a directly indexed heap's constant buffers are
/// rewritten only once the flight that reads them has landed: the flight
/// reads its constants, the next submission its own, as with the old drain,
/// even with the title writing them under the flight (a race on the
/// console). Without the rule (steps done at once), the flight reads the
/// next one's.
#[test]
fn bounces_wait_for_the_flight_that_reads_them() {
    for host in hosts(true) {
        let Some(bench) = Bench::new(host) else { return };
        let Some(heap) = crate::descriptor::host::HostHeap::new(false, 16) else { return };
        let heap = Arc::new(heap);
        assert_eq!(bounced(&bench, &heap, None), (0x11, 0x22));
        let off = bounced(&bench, &heap, Some(Mutation::Queue));
        assert_eq!(off, (0x22, 0x22), "the rule is needed");
    }
}

/// What the flight reads of its slice, and what the next submission finds
/// there, with `rule` off.
fn bounced(
    bench: &Bench,
    heap: &Arc<crate::descriptor::host::HostHeap>,
    rule: Option<Mutation>,
) -> (u8, u8) {
    let (host, region, line) = (bench.host, &bench.region, bench.host.line());
    // The slice, as the heap's bounce memory: written by the plan only.
    let slice = Box::leak(vec![0u8; 64].into_boxed_slice()).as_mut_ptr() as usize;
    // SAFETY: the slice's first byte, written by preparations done by then.
    let first_byte = move || unsafe { (slice as *const u8).read_volatile() };
    let constants = PAGE + 512;
    let bounces = vec![(slice, region.at(constants) as u64, 64)];
    crate::descriptor::host::BindlessPlan::for_test(heap, bounces, Vec::new());
    region.write(constants, &[0x11; 64]);
    let first = &bench.lists[0];
    first.reset();
    first.defer_bindless(heap, false);
    first.close();
    let gate = Gate(host.gate());
    force(Some(Mode::Tail));
    host.execute_title(std::slice::from_ref(first));
    assert_eq!(first_byte(), 0x11);
    region.write(constants, &[0x22; 64]);
    let second = &bench.lists[1];
    second.reset();
    second.defer_bindless(heap, false);
    second.count_work();
    second.close();
    let waits = line.waits_begun();
    let next = second.clone();
    let sent = std::thread::spawn(move || {
        force(Some(Mode::Tail));
        mutate(rule);
        host.execute_title(std::slice::from_ref(&next));
        events()
    });
    until(|| line.waits_begun() > waits);
    std::thread::sleep(Duration::from_millis(30));
    // The flight's shaders read the slice.
    let read = first_byte();
    gate.open();
    let seen = sent.join().unwrap();
    assert_eq!(seen, [Event::Under, Event::Waited(Why::Tail)]);
    line.drain(Count::WaitHeld);
    force(None);
    events();
    (read, first_byte())
}

/// Slots a directly indexed heap's plan left unmade (over its time budget)
/// were written for the flight that may read them: a submission prepared
/// under it waits for it before its plan makes them, and makes them once it
/// has landed. Without the rule the plan makes them under the flight.
#[test]
fn slots_a_plan_left_unmade_wait_for_the_flight() {
    for host in hosts(true) {
        let Some(bench) = Bench::new(host) else { return };
        let Some(heap) = crate::descriptor::host::HostHeap::new(false, 16) else { return };
        let heap = Arc::new(heap);
        assert_eq!(carried(&bench, &heap, None), [Event::Under, Event::Waited(Why::Slots)]);
        let off = carried(&bench, &heap, Some(Mutation::Slots));
        assert_eq!(off, [Event::Under, Event::Waited(Why::Tail)], "the rule is needed");
    }
}

/// What the next submission's preparation did, the plan of the flight before
/// having left a slot unmade, with `rule` off.
fn carried(
    bench: &Bench,
    heap: &Arc<crate::descriptor::host::HostHeap>,
    rule: Option<Mutation>,
) -> Vec<Event> {
    let (host, line) = (bench.host, bench.host.line());
    crate::descriptor::host::BindlessPlan::for_test(heap, Vec::new(), Vec::new());
    let first = &bench.lists[0];
    first.reset();
    first.defer_bindless(heap, false);
    first.close();
    let gate = Gate(host.gate());
    force(Some(Mode::Tail));
    host.execute_title(std::slice::from_ref(first));
    force(None);
    assert!(!heap.carried(), "its plan made what was dirty");
    // As if its plan had run out of time before slot 5.
    heap.carry_for_test(5);
    assert!(heap.carried());
    let second = &bench.lists[1];
    second.reset();
    second.defer_bindless(heap, false);
    second.count_work();
    second.close();
    let (waits, plans) = (line.waits_begun(), heap.plans_made());
    let next = second.clone();
    let sent = std::thread::spawn(move || {
        force(Some(Mode::Tail));
        mutate(rule);
        host.execute_title(std::slice::from_ref(&next));
        events()
    });
    until(|| line.waits_begun() > waits);
    // Where the preparation waits, under the flight: with the rule its plan
    // has made no slot yet.
    let under = heap.plans_made() - plans;
    gate.open();
    let seen = sent.join().unwrap();
    line.drain(Count::WaitHeld);
    events();
    assert_eq!(under, u64::from(rule.is_some()), "slots made under the flight");
    assert_eq!(heap.plans_made() - plans, 1);
    assert!(!heap.carried(), "made in the end");
    seen
}

/// A slot whose view the title released is made anew (null) by the plan's
/// look once a frame: a submission prepared under a flight waits for it
/// first, as for slots left unmade (the flight may still read the slot).
#[test]
fn slots_the_look_remakes_wait_for_the_flight() {
    for host in hosts(true) {
        let Some(bench) = Bench::new(host) else { return };
        let Some(heap) = crate::descriptor::host::HostHeap::new(false, 16) else { return };
        let heap = Arc::new(heap);
        let (host, line) = (bench.host, host.line());
        crate::descriptor::host::BindlessPlan::for_test(&heap, Vec::new(), Vec::new());
        let first = &bench.lists[0];
        first.reset();
        first.count_work();
        first.close();
        let gate = Gate(host.gate());
        force(Some(Mode::Tail));
        host.execute_title(std::slice::from_ref(first));
        force(None);
        // The look finds a slot of the heap to make anew.
        heap.look_for_test(7);
        let second = &bench.lists[1];
        second.reset();
        second.defer_bindless(&heap, false);
        second.count_work();
        second.close();
        let waits = line.waits_begun();
        let next = second.clone();
        let sent = std::thread::spawn(move || {
            force(Some(Mode::Tail));
            host.execute_title(std::slice::from_ref(&next));
            events()
        });
        until(|| line.waits_begun() > waits);
        gate.open();
        assert_eq!(sent.join().unwrap(), [Event::Under, Event::Waited(Why::Slots)]);
        line.drain(Count::WaitHeld);
        events();
    }
}

/// What shaders write through a directly indexed heap's unordered access
/// slots stays in the copy (it is not copied back, as in the game): a
/// refresh there by the next submission waits for the flight, and brings
/// the copy up to date after those writes, as the old drain did. Without
/// the rule, the flight's write is left in the copy.
#[test]
fn a_refresh_of_what_a_flight_writes_unlisted_waits_for_it() {
    for host in hosts(true) {
        let Some(bench) = Bench::new(host) else { return };
        let Some(heap) = crate::descriptor::host::HostHeap::new(false, 16) else { return };
        let heap = Arc::new(heap);
        assert_eq!(unlisted(&bench, &heap, None), (0, 0x33), "refreshed after the flight's writes");
        let off = unlisted(&bench, &heap, Some(Mutation::Unlisted));
        assert_eq!(off, (0x44, 0x33), "the rule is needed");
    }
}

/// The copy's bytes the flight wrote unlisted and the title wrote beside
/// them, after the next submission's refresh, with `rule` off.
fn unlisted(
    bench: &Bench,
    heap: &Arc<crate::descriptor::host::HostHeap>,
    rule: Option<Mutation>,
) -> (u8, u8) {
    let (host, region, sparse) = (bench.host, &bench.region, &bench.sparse);
    let line = host.line();
    region.write(0, &vec![0; SIZE]);
    sparse.reset_for_test();
    let unordered = 3 * PAGE + 4096;
    let writes = vec![(sparse.clone(), region.at(unordered), 256)];
    crate::descriptor::host::BindlessPlan::for_test(heap, Vec::new(), writes);
    let first = &bench.lists[0];
    first.reset();
    first.defer_bindless(heap, true);
    first.close();
    let gate = Gate(host.gate());
    force(Some(Mode::Tail));
    mutate(rule);
    crate::descriptor::host::as_in_game(true);
    host.execute_title(std::slice::from_ref(first));
    crate::descriptor::host::as_in_game(false);
    mutate(None);
    // The title writes another part of the page meanwhile.
    region.write(unordered + 1024, &[0x33; 16]);
    let second = &bench.lists[1];
    second.reset();
    sparse.refresh(second, region.at(unordered) as u64, 2048);
    second.count_work();
    second.close();
    let waits = line.waits_begun();
    let next = second.clone();
    let sent = std::thread::spawn(move || {
        force(Some(Mode::Tail));
        host.execute_title(std::slice::from_ref(&next));
        events()
    });
    until(|| line.waits_begun() > waits);
    std::thread::sleep(Duration::from_millis(30));
    // The flight's shaders write the copy through an unordered access slot.
    // SAFETY: the copy's mapping (the flight's shaders').
    unsafe { sparse.view_for_test(region.at(unordered) + 8).write_volatile(0x44) };
    gate.open();
    let seen = sent.join().unwrap();
    let late = if rule.is_none() { vec![Event::Late] } else { Vec::new() };
    assert_eq!(seen, [vec![Event::Under], late, vec![Event::Waited(Why::Tail)]].concat());
    line.drain(Count::WaitHeld);
    force(None);
    events();
    // SAFETY: the copy's mapping; nothing runs.
    let held = |at: usize| unsafe { sparse.view_for_test(region.at(at)).read_volatile() };
    (held(unordered + 8), held(unordered + 1024))
}
