//! Compiled host pipelines kept on disk, and the title pipelines worth
//! starting early (decision 0007, "Pipeline cache").
//!
//! 1. Every host pipeline goes into the host's pipeline library, named by
//!    a hash of all that its host description holds (the translated
//!    shaders' code, which changes when a translation does, our root
//!    signature, the graphics state). The library is saved under the cache
//!    folder; the host refuses a library of another adapter or driver, and
//!    an entry whose description differs, so neither is ever used.
//! 2. The title pipelines a run drew or dispatched with are listed by a key
//!    known when the title creates them (their shader containers and
//!    graphics state); the next run hands those to the workers at
//!    creation, during loading.
//!
//! All library calls are serialised: the interface does not promise that
//! stores, loads and serialising may overlap.
//!
//! The library is one file of 150 MB and more, on the disk the title
//! streams from, and is written whole. So a thread of its own writes it,
//! slowly and at the lowest priority for the disk: some minutes after the
//! first pipeline that is not in the file, when a load of the title's ends,
//! and (at once) when the process does. A crash loses what those minutes
//! made.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{HSTRING, Interface, PCWSTR};

static DIR: OnceLock<PathBuf> = OnceLock::new();

/// Keeps the pipeline library and the early list in `dir` from now on.
pub fn use_pipeline_cache(dir: PathBuf) {
    let _ = DIR.set(dir);
}

/// Host pipelines created, loaded from the library, and title pipelines
/// started at creation, since the start.
pub(crate) static MADE: AtomicU64 = AtomicU64::new(0);
pub(crate) static LOADED: AtomicU64 = AtomicU64::new(0);
pub(crate) static EARLY: AtomicU64 = AtomicU64::new(0);

/// The library held nothing for this run: the first one, or another
/// driver's.
static EMPTY: AtomicBool = AtomicBool::new(false);

/// Whether pipelines are made anew in this run (a new driver or build).
pub(crate) fn cold() -> bool {
    EMPTY.load(Ordering::Relaxed)
        || MADE.load(Ordering::Relaxed) > LOADED.load(Ordering::Relaxed)
}

struct Library {
    library: ID3D12PipelineLibrary1,
    /// The blob the library was made from: it must outlive the library.
    _blob: Vec<u8>,
    /// Pipelines stored since the last save.
    added: u64,
}

// SAFETY: D3D12 objects are free-threaded; every call is made under the
// mutex that holds the library.
unsafe impl Send for Library {}

const LIBRARY_FILE: &str = "host-pipelines.bin";
const EARLY_FILE: &str = "early-pipelines.txt";
/// Title pipelines listed for an early start, at most.
const EARLY_MAX: usize = 8192;

fn library() -> Option<&'static Mutex<Library>> {
    static LIBRARY: OnceLock<Option<Mutex<Library>>> = OnceLock::new();
    LIBRARY
        .get_or_init(|| {
            let device = crate::host::host()?.device.cast::<ID3D12Device1>().ok()?;
            let blob = DIR
                .get()
                .and_then(|d| std::fs::read(d.join(LIBRARY_FILE)).ok())
                .unwrap_or_default();
            let library = open(&device, blob);
            match &library {
                Some(l) => EMPTY.store(l._blob.is_empty(), Ordering::Relaxed),
                None => tracing::warn!("no host pipeline library: pipelines are not kept"),
            }
            if library.is_some()
                && let Some(dir) = DIR.get()
            {
                remove_left_over(dir);
                let thread = std::thread::Builder::new().name("victory-library".into()).spawn(saver);
                if let Err(e) = thread {
                    tracing::warn!(error = %e, "pipelines are kept only as the run ends");
                }
            }
            library.map(Mutex::new)
        })
        .as_ref()
}

/// The library in `blob`, or an empty one if the host rejects the blob (an
/// adapter or driver change, damage).
fn open(device: &ID3D12Device1, blob: Vec<u8>) -> Option<Library> {
    let make = |bytes: &[u8]| -> windows::core::Result<ID3D12PipelineLibrary1> {
        // SAFETY: the bytes outlive the library (kept beside it).
        unsafe {
            device.CreatePipelineLibrary::<ID3D12PipelineLibrary>(bytes)?.cast()
        }
    };
    if !blob.is_empty() {
        match make(&blob) {
            Ok(library) => {
                tracing::info!(bytes = blob.len(), "host pipeline library loaded");
                return Some(Library { library, _blob: blob, added: 0 });
            }
            Err(e) => tracing::info!(error = %e, "host pipeline library rejected; starting an empty one"),
        }
    } else {
        tracing::info!("no host pipeline library yet; starting an empty one");
    }
    let library = make(&[]).ok()?;
    Some(Library { library, _blob: Vec::new(), added: 0 })
}

/// A pipeline's name in the library: the hash of what its host
/// description holds.
pub(crate) fn name(kind: &str, root: &[u8], code: &[&[u8]], state: &str) -> String {
    let mut h = blake3::Hasher::new();
    for part in [kind.as_bytes(), root].into_iter().chain(code.iter().copied()).chain([state.as_bytes()]) {
        h.update(&(part.len() as u64).to_le_bytes());
        h.update(part);
    }
    h.finalize().to_hex()[..32].to_owned()
}

/// The file of the pipelines the driver's compiler crashed on (names, one per
/// line). One for each driver: another may make them (NVIDIA 621.40 makes
/// the 160 that 610.88 crashed on).
fn crashed_file(driver: &str) -> String {
    let driver: String = driver
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' { c } else { '-' })
        .collect();
    format!("crashed-{driver}.txt")
}

fn crashed_path() -> Option<&'static std::path::Path> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| Some(DIR.get()?.join(crashed_file(crate::host::driver()))))
        .as_deref()
}

fn crashed_names() -> &'static Mutex<HashSet<String>> {
    static NAMES: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    NAMES.get_or_init(|| {
        let names: HashSet<String> = crashed_path()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .map(|t| t.lines().map(str::to_owned).collect())
            .unwrap_or_default();
        if !names.is_empty() {
            tracing::warn!(
                pipelines = names.len(),
                driver = crate::host::driver(),
                "pipelines this driver's compiler crashed on before are left out"
            );
        }
        Mutex::new(names)
    })
}

/// Remember failures in this run immediately. Persist each new name once.
fn remember_crash(names: &Mutex<HashSet<String>>, file: Option<&std::path::Path>, name: &str) {
    use std::io::Write;
    let mut names = names.lock().unwrap_or_else(|e| e.into_inner());
    if !names.insert(name.to_owned()) {
        return;
    }
    let Some(file) = file else { return };
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file);
    if let Ok(mut f) = file {
        let _ = writeln!(f, "{name}");
    }
}

fn note_crashed(name: &str) {
    remember_crash(crashed_names(), crashed_path(), name);
}

/// Whether the driver crashed making the pipeline named `name` before.
fn crashed(name: &str) -> bool {
    crashed_names()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(name)
}

struct CompileReport {
    name: String,
    shaders: String,
}

thread_local! {
    /// The pipeline the driver makes on this thread.
    static MAKING: std::cell::RefCell<Option<Arc<CompileReport>>> = const { std::cell::RefCell::new(None) };
}

/// The driver makes a pipeline on this thread. If its compiler crashes there
/// (AMD's does on a few of the world's compute shaders), the pipeline is
/// noted and left out from then on, and the thread parked: the others, and
/// the title, go on.
struct Making;

impl Making {
    fn new(name: &str, shaders: &[Arc<super::Shader>]) -> Making {
        use windows_sys::Win32::System::Diagnostics::Debug::AddVectoredExceptionHandler;
        static HANDLER: std::sync::Once = std::sync::Once::new();
        // SAFETY: a handler that lives as long as the process.
        HANDLER.call_once(|| unsafe {
            AddVectoredExceptionHandler(1, Some(compiler_crashed));
        });
        let report = Arc::new(CompileReport {
            name: name.to_owned(),
            shaders: format!("{shaders:?}"),
        });
        MAKING.with(|m| *m.borrow_mut() = Some(report.clone()));
        endless(&report, true);
        Making
    }
}

/// The process's private bytes.
fn committed() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: a structure of the size passed.
    unsafe {
        let mut info: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        let size = size_of_val(&info) as u32;
        if GetProcessMemoryInfo(GetCurrentProcess(), &mut info, size) == 0 {
            return 0;
        }
        info.PagefileUsage as u64
    }
}

/// Workers (threads) lost to compiles that crashed, not replaced yet.
static LOST: LostWorkers = LostWorkers::new();

struct LostWorkers {
    pending: Mutex<Vec<u32>>,
    ready: Condvar,
}

impl LostWorkers {
    const fn new() -> Self {
        Self {
            pending: Mutex::new(Vec::new()),
            ready: Condvar::new(),
        }
    }

    fn enqueue(&self, id: u32) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(id);
        self.ready.notify_one();
    }

    fn take_until(&self, deadline: Instant) -> Vec<u32> {
        let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let wait = deadline.saturating_duration_since(Instant::now());
        let (mut pending, _) = self
            .ready
            .wait_timeout_while(pending, wait, |ids| ids.is_empty())
            .unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *pending)
    }
}

/// Suspends thread `id`, at work in the driver's compiler (never in a heap
/// function, whose lock it would keep); false if it was not caught there.
fn suspend(id: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, HMODULE};
    use windows_sys::Win32::System::Diagnostics::Debug::{CONTEXT, GetThreadContext};
    use windows::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        GetModuleHandleExW, GetModuleHandleW,
    };
    use windows::Win32::System::Threading::{
        OpenThread, ResumeThread, SuspendThread, THREAD_GET_CONTEXT, THREAD_SUSPEND_RESUME,
    };
    const CONTEXT_CONTROL_AMD64: u32 = 0x10_0001;
    #[repr(align(16))]
    struct Aligned(CONTEXT);
    // SAFETY: plain calls on a thread of ours; the context is whole and
    // aligned; nothing that may need the suspended thread's locks is called
    // while it is suspended (the modules are asked about after it runs on).
    unsafe {
        let Ok(thread) = OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT, false, id) else {
            return false;
        };
        let module = |at: u64| {
            let mut m = HMODULE::default();
            let _ = GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                PCWSTR(at as *const u16),
                &mut m,
            );
            m.0 as usize
        };
        let named = |s: &str| GetModuleHandleW(&HSTRING::from(s)).map_or(0, |m| m.0 as usize);
        let (ntdll, ucrt) = (named("ntdll.dll"), named("ucrtbase.dll"));
        let mut caught = false;
        let mut last = Vec::new();
        for _ in 0..500 {
            if SuspendThread(thread) == u32::MAX {
                break;
            }
            let mut context: Aligned = std::mem::zeroed();
            context.0.ContextFlags = CONTEXT_CONTROL_AMD64;
            let known = GetThreadContext(thread.0, &mut context.0) != 0;
            let at = context.0.Rip >> 12;
            if known && last.contains(&at) {
                caught = true;
                break;
            }
            ResumeThread(thread);
            // A page of the driver's own code: caught there next time.
            let m = module(at << 12);
            if known && m != 0 && m != ntdll && m != ucrt {
                last.push(at);
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let _ = CloseHandle(thread);
        caught
    }
}

/// Notes pipelines the driver's compiler has worked on for long
/// ([`ENDLESS`]): they are left out from the next run on. AMD's never ends
/// some of the world's ray tracing programs, and takes memory all the while
/// (16 GB in two minutes), so their threads are stopped.
fn endless(report: &Arc<CompileReport>, begins: bool) {
    const SCAN_INTERVAL: Duration = Duration::from_secs(2);
    // Some long programs take a while (the upscaler's 10,000 instructions);
    // what takes memory all along goes first.
    const ENDLESS: Duration = Duration::from_secs(20);
    const EVEN_SO: Duration = Duration::from_secs(150);
    const GROWTH: u64 = 3 << 30;
    static AT_WORK: Mutex<Vec<(Arc<CompileReport>, Instant, u32)>> = Mutex::new(Vec::new());
    static WATCH: std::sync::Once = std::sync::Once::new();
    {
        // SAFETY: a plain call.
        let id = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
        let mut at_work = AT_WORK.lock().unwrap_or_else(|e| e.into_inner());
        if begins {
            at_work.push((report.clone(), Instant::now(), id));
        } else if let Some(i) = at_work.iter().position(|w| Arc::ptr_eq(&w.0, report) && w.2 == id) {
            at_work.swap_remove(i);
        }
    }
    WATCH.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("victory-compiles".into())
            .spawn(|| {
                let mut noted: Vec<String> = Vec::new();
                // The process's commit when a compile was first seen late.
                let mut late: Option<u64> = None;
                let mut next_scan = Instant::now() + SCAN_INTERVAL;
                loop {
                    for id in LOST.take_until(next_scan) {
                        super::make::worker_lost(id);
                    }
                    // Crash notifications do not reset the hang scan timer.
                    let now = Instant::now();
                    if now < next_scan {
                        continue;
                    }
                    next_scan = now + SCAN_INTERVAL;
                    let long: Vec<(Arc<CompileReport>, u32)> = AT_WORK
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .iter()
                        .filter(|w| w.1.elapsed() > ENDLESS && !noted.contains(&w.0.name))
                        .map(|w| (w.0.clone(), w.2))
                        .collect();
                    if long.is_empty() {
                        late = None;
                        continue;
                    }
                    let now = committed();
                    let grown = now.saturating_sub(*late.get_or_insert(now)) > GROWTH;
                    let long: Vec<(Arc<CompileReport>, u32)> = if grown {
                        late = None;
                        long
                    } else {
                        AT_WORK
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .iter()
                            .filter(|w| w.1.elapsed() > EVEN_SO && !noted.contains(&w.0.name))
                            .map(|w| (w.0.clone(), w.2))
                            .collect()
                    };
                    for (report, id) in long {
                        let stopped = suspend(id);
                        if stopped {
                            super::make::worker_lost(id);
                        }
                        tracing::error!(
                            name = report.name.as_str(),
                            shaders = %report.shaders,
                            stopped,
                            "the driver's compiler does not end a pipeline: it is left out from the next run on"
                        );
                        note_crashed(&report.name);
                        noted.push(report.name.clone());
                    }
                }
            });
    });
}

impl Drop for Making {
    fn drop(&mut self) {
        if let Some(report) = MAKING.with(|m| m.borrow_mut().take()) {
            endless(&report, false);
        }
    }
}

unsafe extern "system" fn compiler_crashed(
    info: *mut windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS,
) -> i32 {
    const ACCESS_VIOLATION: i32 = 0xc000_0005_u32 as i32;
    // SAFETY: the system passes a live record.
    let record = unsafe { &*(*info).ExceptionRecord };
    // Only reads or writes of the first pages: our own guards' faults are
    // elsewhere.
    if record.ExceptionCode != ACCESS_VIOLATION || record.ExceptionInformation[1] >= 0x10000 {
        return 0; // not ours
    }
    // The report was formatted before entering the driver; cloning the Arc
    // lets us log it without holding the TLS borrow or allocating a string.
    let Ok(Some(report)) = MAKING.try_with(|m| m.try_borrow().ok()?.as_ref().cloned()) else {
        return 0; // no compiler call to attribute it to
    };
    note_crashed(&report.name);
    tracing::error!(
        name = report.name.as_str(),
        shaders = %report.shaders,
        "the driver's compiler crashed making a pipeline: it is left out"
    );
    endless(&report, false);
    // SAFETY: a plain call.
    let id = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
    LOST.enqueue(id);
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

/// The pipeline named `name`: loaded from the library by `load` if it is
/// there with the same description, else made by `create` and stored.
pub(crate) fn pipeline(
    name: &str,
    shaders: &[Arc<super::Shader>],
    load: impl Fn(&ID3D12PipelineLibrary1, PCWSTR) -> windows::core::Result<ID3D12PipelineState>,
    create: impl FnOnce() -> windows::core::Result<ID3D12PipelineState>,
) -> windows::core::Result<ID3D12PipelineState> {
    let reject = || windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL);
    if crashed(name) {
        return Err(reject());
    }
    let wide = HSTRING::from(name);
    let create = || {
        // Another worker may have hit the failure while we looked in the library.
        if crashed(name) {
            return Err(reject());
        }
        let _making = Making::new(name, shaders);
        create()
    };
    let Some(library) = library() else {
        MADE.fetch_add(1, Ordering::Relaxed);
        return create();
    };
    if let Ok(pso) = load(&library.lock().unwrap_or_else(|e| e.into_inner()).library, PCWSTR(wide.as_ptr())) {
        LOADED.fetch_add(1, Ordering::Relaxed);
        return Ok(pso);
    }
    // Compiled outside the lock: compiles take up to seconds.
    let pso = create()?;
    MADE.fetch_add(1, Ordering::Relaxed);
    let mut l = library.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: a live pipeline; a name already stored (a description that
    // differs) is refused, which leaves the library as it was.
    let stored = unsafe { l.library.StorePipeline(PCWSTR(wide.as_ptr()), &pso) };
    match &stored {
        Ok(()) => l.added += 1,
        // Another worker stored it first (two title pipelines alike), or
        // the library does not take it: then it is made in every run.
        Err(e) if load(&l.library, PCWSTR(wide.as_ptr())).is_err() => not_kept(name, e),
        Err(_) => {}
    }
    drop(l);
    if stored.is_ok() {
        changed();
    }
    Ok(pso)
}

/// Logs a pipeline the library does not take, once per reason.
fn not_kept(name: &str, error: &windows::core::Error) {
    static SEEN: Mutex<Vec<i32>> = Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if !seen.contains(&error.code().0) {
        seen.push(error.code().0);
        tracing::warn!(
            error = %error,
            name,
            "the host pipeline library does not take a pipeline: it is made in every run (logged once per reason)"
        );
    }
}

/// How long what is not on disk waits for more to come, at most.
const SAVE_EVERY: Duration = Duration::from_secs(180);

/// When the cache is to be written.
struct Due {
    /// Since when something is not on disk.
    since: Option<Instant>,
    /// It is to be there soon.
    soon: bool,
}

impl Due {
    const NOTHING: Due = Due { since: None, soon: false };

    fn changed(&mut self, now: Instant) {
        self.since.get_or_insert(now);
    }

    fn soon(&mut self) {
        self.soon = self.since.is_some();
    }

    /// How long until it is written; `None` with nothing to write.
    fn wait(&self, now: Instant) -> Option<Duration> {
        let waited = now.saturating_duration_since(self.since?);
        Some(if self.soon { Duration::ZERO } else { SAVE_EVERY.saturating_sub(waited) })
    }
}

static DUE: Mutex<Due> = Mutex::new(Due::NOTHING);
static DUE_CHANGED: Condvar = Condvar::new();

fn due() -> std::sync::MutexGuard<'static, Due> {
    DUE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Something is not on disk.
fn changed() {
    let mut due = due();
    if due.since.is_none() {
        due.changed(Instant::now());
        DUE_CHANGED.notify_one();
    }
}

/// Has what is not on disk written soon (a load of the title's ended).
pub(crate) fn save_soon() {
    due().soon();
    DUE_CHANGED.notify_one();
}

/// The process ends: a write under way is given up, and no other begun.
static ENDING: AtomicBool = AtomicBool::new(false);

/// Writes what is not on disk at once (the process ends).
pub(crate) fn save_now() {
    ENDING.store(true, Ordering::Relaxed);
    // A run that made nothing does not open the library for this.
    if MADE.load(Ordering::Relaxed) > 0 || USED_CHANGED.load(Ordering::Relaxed) {
        save(|path, bytes| std::fs::write(path, bytes));
    }
}

/// Writes the cache when it is due, as long as the process runs.
fn saver() {
    // The workers that make pipelines wait while the library serialises:
    // at their priority.
    // SAFETY: a plain call on our own thread.
    unsafe {
        use windows::Win32::System::Threading::*;
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
    let mut waiting = due();
    while !ENDING.load(Ordering::Relaxed) {
        waiting = match waiting.wait(Instant::now()) {
            None => DUE_CHANGED.wait(waiting).unwrap_or_else(|e| e.into_inner()),
            Some(Duration::ZERO) => {
                // What is added from here on is for the next write.
                *waiting = Due::NOTHING;
                drop(waiting);
                if !save(|path, bytes| write_slowly(path, bytes, &ENDING)) {
                    changed();
                }
                due()
            }
            Some(rest) => {
                let woken = DUE_CHANGED.wait_timeout(waiting, rest);
                woken.unwrap_or_else(|e| e.into_inner()).0
            }
        };
    }
}

/// The library's bytes, and how many pipelines they hold that its file does
/// not; `None` if there are none, or they do not serialise.
fn serialized(library: &Mutex<Library>) -> Option<(Vec<u8>, u64)> {
    /// Room for what is added while the buffer is made.
    const SPARE: usize = 4 << 20;
    let lock = || library.lock().unwrap_or_else(|e| e.into_inner());
    let size = {
        let l = lock();
        if l.added == 0 {
            return None;
        }
        // SAFETY: a plain query.
        unsafe { l.library.GetSerializedSize() }
    };
    // Made and touched outside the lock: the page faults of 150 MB.
    let mut bytes: Vec<u8> = Vec::new();
    if let Err(e) = bytes.try_reserve_exact(size + SPARE) {
        tracing::warn!(error = %e, size, "no memory to serialise the host pipeline library");
        return None;
    }
    bytes.resize(size + SPARE, 0);
    let l = lock();
    // SAFETY: a buffer of the size the library asks for.
    let done = unsafe {
        bytes.resize(l.library.GetSerializedSize(), 0);
        l.library.Serialize(&mut bytes)
    };
    match done {
        Ok(()) => Some((bytes, l.added)),
        Err(e) => {
            tracing::warn!(error = %e, "host pipeline library not serialised");
            None
        }
    }
}

/// Writes the library if pipelines were added (with `write`, to a file it is
/// given) and the early list if that changed; false if one was not written.
fn save(write: impl Fn(&std::path::Path, &[u8]) -> std::io::Result<()>) -> bool {
    // Held over serialisation and both writes, so overlapping saves cannot
    // replace files under each other.
    static ONE: Mutex<()> = Mutex::new(());
    let (Some(dir), Some(library)) = (DIR.get(), library()) else { return true };
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let started = Instant::now();
    let blob = serialized(library);
    let used_changed = USED_CHANGED.swap(false, Ordering::Relaxed);
    if blob.is_none() && !used_changed {
        return true;
    }
    let used: Vec<String> = USED.lock().unwrap_or_else(|e| e.into_inner()).iter().flatten().cloned().collect();
    let written = std::fs::create_dir_all(dir)
        .and_then(|()| match &blob {
            Some((bytes, _)) => publish(&dir.join(LIBRARY_FILE), |file| write(file, bytes)),
            None => Ok(()),
        })
        .and_then(|()| match used_changed {
            true => write_atomically(&dir.join(EARLY_FILE), used.join("\n").as_bytes()),
            false => Ok(()),
        });
    match (written, blob) {
        (Ok(()), Some((bytes, added))) => {
            let mut l = library.lock().unwrap_or_else(|e| e.into_inner());
            // Keep pipelines stored while the files were being written.
            l.added = l.added.saturating_sub(added);
            drop(l);
            tracing::info!(
                bytes = bytes.len(),
                added,
                early = used.len(),
                made = MADE.load(Ordering::Relaxed),
                loaded = LOADED.load(Ordering::Relaxed),
                started_early = EARLY.load(Ordering::Relaxed),
                seconds = started.elapsed().as_secs_f64(),
                "host pipeline library saved"
            );
            true
        }
        (Ok(()), None) => {
            tracing::info!(early = used.len(), "title pipelines to start early saved");
            true
        }
        (Err(e), _) => {
            // At least one cache file was not published. Keep the counters
            // dirty for the next save.
            if used_changed {
                USED_CHANGED.store(true, Ordering::Relaxed);
            }
            if e.kind() == std::io::ErrorKind::Interrupted {
                tracing::info!("a slow write of the host pipeline library given up");
            } else {
                tracing::warn!(error = %e, "host pipeline library not saved");
            }
            false
        }
    }
}

/// Writes `bytes` to a new file a piece at a time, at the lowest priority
/// for the disk (each piece is there before the next) and the processor: a
/// disk that is read all the while takes two of them a second (measured, a
/// hard disk). What is left after a minute goes at the thread's own
/// priority. Given up (`Interrupted`) as soon as `stop` is set.
fn write_slowly(path: &std::path::Path, bytes: &[u8], stop: &AtomicBool) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::System::Threading::*;
    const WRITE_THROUGH: u32 = 0x8000_0000;
    const PIECE: usize = 1 << 20;
    const SLOW: Duration = Duration::from_secs(60);
    /// This thread at background priority, until dropped.
    struct Background;
    impl Background {
        fn begin() -> Background {
            // SAFETY: a plain call on our own thread.
            let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
            Background
        }
    }
    impl Drop for Background {
        fn drop(&mut self) {
            // SAFETY: a plain call on our own thread.
            let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_END) };
        }
    }
    let started = Instant::now();
    let mut background = Some(Background::begin());
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(WRITE_THROUGH)
        .open(path)?;
    // The file's place on the disk, in one piece.
    file.set_len(bytes.len() as u64)?;
    for piece in bytes.chunks(PIECE) {
        if stop.load(Ordering::Relaxed) {
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        if started.elapsed() > SLOW {
            background = None;
        }
        file.write_all(piece)?;
    }
    drop(background);
    Ok(())
}

/// Writes `bytes` to a unique temporary file beside `path`, then renames it
/// over `path`: a reader never sees a part, and overlapping saves do not
/// share a temporary file.
fn write_atomically(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    publish(path, |file| std::fs::write(file, bytes))
}

/// As [`write_atomically`], with the temporary file written by `write`.
fn publish(
    path: &std::path::Path,
    write: impl FnOnce(&std::path::Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let extension = format!(
        "{TEMPORARY}{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let tmp = path.with_extension(extension);
    let written = write(&tmp).and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// What the extension of a temporary file starts with.
const TEMPORARY: &str = "tmp-";

/// Removes the temporary files of runs that ended while they wrote one.
fn remove_left_over(dir: &std::path::Path) {
    /// No write takes as long: one of this age is nobody's.
    const OLD: Duration = Duration::from_secs(3600);
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let temporary = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.starts_with(TEMPORARY));
        let old = || {
            let written = entry.metadata().and_then(|m| m.modified()).ok()?;
            Some(written.elapsed().ok()? > OLD)
        };
        if temporary && old() == Some(true) {
            let removed = std::fs::remove_file(&path);
            tracing::info!(path = %path.display(), removed = removed.is_ok(), "a left over temporary file");
        }
    }
}

/// Title pipeline keys drawn or dispatched with (this run and the last).
static USED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
static USED_CHANGED: AtomicBool = AtomicBool::new(false);

fn used() -> std::sync::MutexGuard<'static, Option<HashSet<String>>> {
    let mut used = USED.lock().unwrap_or_else(|e| e.into_inner());
    if used.is_none() {
        let listed = DIR
            .get()
            .and_then(|d| std::fs::read_to_string(d.join(EARLY_FILE)).ok())
            .unwrap_or_default();
        let set: HashSet<String> = listed.lines().filter(|l| !l.is_empty()).map(str::to_owned).collect();
        tracing::info!(pipelines = set.len(), "title pipelines to start early");
        *used = Some(set);
    }
    used
}

/// Whether the title pipeline with `key` was drawn with in an earlier run.
pub(crate) fn wanted_early(key: &str) -> bool {
    used().as_ref().is_some_and(|s| s.contains(key))
}

/// The title pipeline with `key` was drawn or dispatched with.
pub(crate) fn note_used(key: &str) {
    let mut used = used();
    let set = used.get_or_insert_with(HashSet::new);
    if set.len() < EARLY_MAX && set.insert(key.to_owned()) {
        drop(used);
        USED_CHANGED.store(true, Ordering::Relaxed);
        changed();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_worker_losses_are_taken_without_a_second_notification() {
        let lost = LostWorkers::new();
        lost.enqueue(4);
        lost.enqueue(9);

        assert_eq!(lost.take_until(Instant::now()), [4, 9]);
        assert!(lost.take_until(Instant::now()).is_empty());
    }

    #[test]
    fn remembered_failure_is_visible_immediately_and_appended_once() {
        let dir = tempfile::tempdir().unwrap();
        let names = Mutex::new(HashSet::new());
        let file = dir.path().join(crashed_file("10de 32.0.16.2140"));

        remember_crash(&names, None, "no-disk-cache");
        remember_crash(&names, Some(&file), "failed-pipeline");
        remember_crash(&names, Some(&file), "failed-pipeline");

        let known = names.into_inner().unwrap();
        assert!(known.contains("no-disk-cache"));
        assert!(known.contains("failed-pipeline"));
        let contents = std::fs::read_to_string(&file).unwrap();
        assert_eq!(contents.lines().collect::<Vec<_>>(), ["failed-pipeline"]);
    }

    #[test]
    fn each_driver_has_its_own_list_of_crashes() {
        assert_eq!(crashed_file("10de 32.0.16.2140"), "crashed-10de-32.0.16.2140.txt");
        assert_ne!(crashed_file("10de 32.0.16.2140"), crashed_file("10de 32.0.16.1088"));
        assert!(!crashed_file("a/b\\c:d").contains(['/', '\\', ':']));
    }

    #[test]
    fn names_change_with_what_goes_into_the_pipeline() {
        let a = name("graphics", b"root", &[b"vs", b"ps"], "state");
        assert_eq!(a, name("graphics", b"root", &[b"vs", b"ps"], "state"));
        assert_ne!(a, name("graphics", b"root", &[b"vs", b"ps2"], "state"));
        assert_ne!(a, name("graphics", b"root", &[b"vs", b"ps"], "state2"));
        assert_ne!(a, name("graphics", b"root2", &[b"vs", b"ps"], "state"));
        assert_ne!(a, name("mesh", b"root", &[b"vs", b"ps"], "state"));
        // Parts do not run into each other.
        assert_ne!(a, name("graphics", b"root", &[b"vsp", b"s"], "state"));
    }

    #[test]
    fn a_rejected_blob_gives_an_empty_library() {
        let Some(device) = crate::host::host().and_then(|h| h.device.cast::<ID3D12Device1>().ok()) else {
            return; // no host GPU here
        };
        let library = open(&device, b"not a pipeline library".repeat(16)).expect("an empty library");
        // SAFETY: a plain query on a live library.
        assert!(unsafe { library.library.GetSerializedSize() } > 0);
        assert!(library._blob.is_empty());
    }

    #[test]
    fn concurrent_atomic_writes_publish_one_complete_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LIBRARY_FILE);
        let start = std::sync::Arc::new(std::sync::Barrier::new(8));
        let mut threads = Vec::new();
        for thread in 0..8 {
            let path = path.clone();
            let start = start.clone();
            threads.push(std::thread::spawn(move || {
                start.wait();
                let byte = thread as u8 + 1;
                for _ in 0..32 {
                    write_atomically(&path, &[byte; 64 * 1024]).unwrap();
                }
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }

        let published = std::fs::read(&path).unwrap();
        assert_eq!(published.len(), 64 * 1024);
        assert!(published.iter().all(|&byte| byte == published[0]));
        assert!((1..=8).contains(&published[0]));
        assert!(!std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().contains(".tmp-")));
    }

    #[test]
    fn failed_atomic_writes_remove_their_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LIBRARY_FILE);
        std::fs::create_dir(&path).unwrap();

        assert!(write_atomically(&path, b"not published").is_err());
        assert!(!std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().contains(".tmp-")));
    }

    #[test]
    fn what_is_not_on_disk_is_written_after_minutes_or_soon_if_asked() {
        let began = Instant::now();
        let at = |s: u64| began + Duration::from_secs(s);
        let mut due = Due::NOTHING;
        assert_eq!(due.wait(at(0)), None);
        // Asked for with nothing to write, it is not kept for later.
        due.soon();
        assert_eq!(due.wait(at(1)), None);
        due.changed(at(10));
        assert_eq!(due.wait(at(10)), Some(SAVE_EVERY));
        // More of the same does not put it off.
        due.changed(at(100));
        assert_eq!(due.wait(at(100)), Some(SAVE_EVERY - Duration::from_secs(90)));
        assert_eq!(due.wait(at(10) + SAVE_EVERY), Some(Duration::ZERO));
        assert_eq!(due.wait(at(10_000)), Some(Duration::ZERO));
        // A load ended.
        due = Due::NOTHING;
        due.changed(at(200));
        due.soon();
        assert_eq!(due.wait(at(201)), Some(Duration::ZERO));
    }

    #[test]
    fn a_slow_write_is_whole_or_given_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LIBRARY_FILE);
        let bytes: Vec<u8> = (0..1 << 20).map(|i: u32| (i >> 8) as u8).chain([1, 2, 3]).collect();
        let go_on = AtomicBool::new(false);
        publish(&path, |file| write_slowly(file, &bytes, &go_on)).unwrap();
        assert!(std::fs::read(&path).unwrap() == bytes);
        // Given up, the file before stays and nothing else does.
        let stop = AtomicBool::new(true);
        let given_up = publish(&path, |file| write_slowly(file, b"other", &stop));
        assert_eq!(given_up.unwrap_err().kind(), std::io::ErrorKind::Interrupted);
        assert!(std::fs::read(&path).unwrap() == bytes);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        // The thread has its own priority again.
        // SAFETY: a plain query.
        let priority = unsafe {
            use windows::Win32::System::Threading::*;
            GetThreadPriority(GetCurrentThread())
        };
        assert_eq!(priority, 0);
    }

    #[test]
    fn only_old_temporary_files_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str, old: bool| {
            let path = dir.path().join(name);
            let file = std::fs::File::create(&path).unwrap();
            if old {
                let then = std::time::SystemTime::now() - Duration::from_secs(2 * 3600);
                file.set_modified(then).unwrap();
            }
            path
        };
        let gone = file("host-pipelines.tmp-1234-0", true);
        let kept = [
            file("host-pipelines.tmp-1234-1", false),
            file("host-pipelines.bin", true),
            file("early-pipelines.txt", true),
            file("crashed-10de-32.0.16.2140.txt", true),
        ];
        remove_left_over(dir.path());
        assert!(!gone.exists());
        assert!(kept.iter().all(|k| k.exists()));
        remove_left_over(&dir.path().join("none"));
    }

    /// From a pipeline made to the next run, with the thread that writes the
    /// file. The folder and the end of the run are the process's: alone.
    #[test]
    #[ignore = "sets the process's cache folder and ends its saving: run it alone"]
    fn what_was_made_is_there_in_the_next_run() {
        use windows::Win32::Graphics::Direct3D::{Fxc::D3DCompile, ID3DBlob};
        use windows::core::{PCSTR, s};
        let Some(host) = crate::host::host() else { return };
        let dir = tempfile::tempdir().unwrap();
        use_pipeline_cache(dir.path().join("pipelines"));
        let source = b"RWByteAddressBuffer b : register(u0); [numthreads(1, 1, 1)] void main() { b.Store(0, 7); }";
        let signature = b"UAV(u0)";
        // SAFETY: the arguments are live and the blobs the output pointers.
        let blobs = unsafe {
            let compile = |source: &[u8], entry, target| {
                let mut code: Option<ID3DBlob> = None;
                D3DCompile(
                    source.as_ptr().cast(), source.len(), PCSTR::null(), None, None,
                    entry, target, 0, 0, &mut code, None,
                )
                .unwrap();
                code.unwrap()
            };
            let define = [b"#define RS \"", &signature[..], b"\"\n"].concat();
            [compile(source, s!("main"), s!("cs_5_1")), compile(&define, s!("RS"), s!("rootsig_1_1"))]
        };
        // SAFETY: the blobs own these bytes.
        let [code, root] = blobs.each_ref().map(|b| unsafe {
            std::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize())
        });
        // SAFETY: a serialized root signature.
        let root: ID3D12RootSignature = unsafe { host.device.CreateRootSignature(0, root) }.unwrap();
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            // SAFETY: a borrowed pointer the description never releases.
            pRootSignature: unsafe { std::mem::transmute_copy(&root) },
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: code.as_ptr().cast(),
                BytecodeLength: code.len(),
            },
            ..Default::default()
        };
        let make = |name: &str| {
            pipeline(
                name,
                &[],
                // SAFETY: the description points at live data.
                |library, name| unsafe { library.LoadComputePipeline(name, &desc) },
                // SAFETY: as above.
                || unsafe { host.device.CreateComputePipelineState(&desc) },
            )
            .unwrap()
        };
        make("first");
        assert!(cold());
        let file = dir.path().join("pipelines").join(LIBRARY_FILE);
        std::thread::sleep(Duration::from_secs(1));
        assert!(!file.exists(), "not written before it is due");
        save_soon();
        let asked = Instant::now();
        while !file.exists() {
            assert!(asked.elapsed() < Duration::from_secs(60), "never written");
            std::thread::sleep(Duration::from_millis(50));
        }
        eprintln!("written {:?} after it was asked for", asked.elapsed());
        // The end of the run writes what came since, and the early list.
        make("second");
        note_used("a title pipeline");
        save_now();
        let device = host.device.cast::<ID3D12Device1>().unwrap();
        let next = open(&device, std::fs::read(&file).unwrap()).unwrap();
        assert!(!next._blob.is_empty());
        for name in ["first", "second"] {
            let wide = HSTRING::from(name);
            // SAFETY: as above.
            let loaded: windows::core::Result<ID3D12PipelineState> =
                unsafe { next.library.LoadComputePipeline(PCWSTR(wide.as_ptr()), &desc) };
            loaded.unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        let early = std::fs::read_to_string(file.with_file_name(EARLY_FILE)).unwrap();
        assert_eq!(early, "a title pipeline");
    }
}
