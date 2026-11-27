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
            if library.is_none() {
                tracing::warn!("no host pipeline library: pipelines are not kept");
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

/// The machine's physical memory not in use, if Windows tells.
fn available_memory() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: a structure of the size it says it is.
    unsafe {
        let mut status: MEMORYSTATUSEX = std::mem::zeroed();
        status.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
        (GlobalMemoryStatusEx(&mut status) != 0).then_some(status.ullAvailPhys)
    }
}

/// How long the driver's compiler may work on a pipeline before
/// [`endless`] stops it: past `endless` it is stopped once the process has
/// grown by `growth` bytes since, or once the machine has less than
/// `least_free` bytes of memory free (0: never for that); past `even_so`
/// in any case.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Hold {
    endless: Duration,
    even_so: Duration,
    growth: u64,
    least_free: u64,
}

/// Not sooner, nor for less memory (upstream Belobug's 7263339): while the
/// world loads the process takes gigabytes by itself and every core is busy,
/// and four pipelines the driver makes in seconds were left out for good
/// with 20 s and 3 GiB (run amd1, 2026-11-25). A compile that never ends
/// takes 8 GiB a minute; with 32 GB and the title's 13 to 19 GB beside it,
/// one is also stopped once less than 2 GiB are free, before the machine
/// pages.
const HOLD: Hold = Hold {
    endless: Duration::from_secs(60),
    even_so: Duration::from_secs(300),
    growth: 8 << 30,
    least_free: 2 << 30,
};

/// `VICTORY_COMPILE_HOLD=<s>,<s>,<GiB>,<GiB>`: [`Hold`]'s four, in that order;
/// those left out keep [`HOLD`]'s. `20,150,3,0` is the hold before upstream's
/// 7263339. Pure, for testing.
fn parse_hold(value: Option<&str>) -> Option<Hold> {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Some(HOLD);
    };
    let mut hold = HOLD;
    for (i, part) in value.split(',').enumerate() {
        let n: f64 = part.trim().parse().ok().filter(|n: &f64| n.is_finite() && *n >= 0.0)?;
        match i {
            0 => hold.endless = Duration::try_from_secs_f64(n).ok()?,
            1 => hold.even_so = Duration::try_from_secs_f64(n).ok()?,
            2 => hold.growth = (n * f64::from(1u32 << 30)) as u64,
            3 => hold.least_free = (n * f64::from(1u32 << 30)) as u64,
            _ => return None,
        }
    }
    Some(hold)
}

fn hold() -> Hold {
    static HOLDS: std::sync::OnceLock<Hold> = std::sync::OnceLock::new();
    *HOLDS.get_or_init(|| {
        let value = std::env::var("VICTORY_COMPILE_HOLD").ok();
        parse_hold(value.as_deref()).unwrap_or_else(|| {
            tracing::warn!(value, "VICTORY_COMPILE_HOLD: expected up to four numbers (s,s,GiB,GiB); the default is kept");
            HOLD
        })
    })
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
/// ([`hold`]): they are left out from the next run on. AMD's never ends
/// some of the world's ray tracing programs, and takes memory all the while
/// (16 GB in two minutes), so their threads are stopped.
fn endless(report: &Arc<CompileReport>, begins: bool) {
    const SCAN_INTERVAL: Duration = Duration::from_secs(2);
    // Some long programs take a while (the upscaler's 10,000 instructions);
    // what takes memory all along goes first.
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
                let hold = hold();
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
                        .filter(|w| w.1.elapsed() > hold.endless && !noted.contains(&w.0.name))
                        .map(|w| (w.0.clone(), w.2))
                        .collect();
                    if long.is_empty() {
                        late = None;
                        continue;
                    }
                    let now = committed();
                    let grown = now.saturating_sub(*late.get_or_insert(now)) > hold.growth;
                    let short =
                        hold.least_free > 0 && available_memory().is_some_and(|free| free < hold.least_free);
                    let why = if grown {
                        "the process grew"
                    } else if short {
                        "memory runs short"
                    } else {
                        "too long"
                    };
                    let long: Vec<(Arc<CompileReport>, u32)> = if grown || short {
                        late = None;
                        long
                    } else {
                        AT_WORK
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .iter()
                            .filter(|w| w.1.elapsed() > hold.even_so && !noted.contains(&w.0.name))
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
                            why,
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
    load: impl FnOnce(&ID3D12PipelineLibrary1, PCWSTR) -> windows::core::Result<ID3D12PipelineState>,
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
    if unsafe { l.library.StorePipeline(PCWSTR(wide.as_ptr()), &pso) }.is_ok() {
        l.added += 1;
    }
    Ok(pso)
}

/// Writes the library and the early list if pipelines were added since the
/// last save and `SAVE_EVERY` has passed (or `now`).
pub(crate) fn save(now: bool) {
    // Held over serialisation and both writes, so overlapping saves cannot
    // replace files under each other.
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    const SAVE_EVERY: std::time::Duration = std::time::Duration::from_secs(20);
    let (Some(dir), Some(library)) = (DIR.get(), library()) else { return };
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if !now && last.is_some_and(|t| t.elapsed() < SAVE_EVERY) {
        return;
    }
    *last = Some(std::time::Instant::now());
    let (blob, saved_additions) = {
        let l = library.lock().unwrap_or_else(|e| e.into_inner());
        if l.added == 0 && !USED_CHANGED.load(Ordering::Relaxed) {
            return;
        }
        // SAFETY: a buffer of the size the library asks for.
        let bytes = unsafe {
            let mut bytes = vec![0u8; l.library.GetSerializedSize()];
            l.library.Serialize(&mut bytes).map(|()| bytes)
        };
        match bytes {
            Ok(bytes) => (bytes, l.added),
            Err(e) => {
                tracing::warn!(error = %e, "host pipeline library not serialised");
                return;
            }
        }
    };
    let used_changed = USED_CHANGED.swap(false, Ordering::Relaxed);
    let used: Vec<String> = USED.lock().unwrap_or_else(|e| e.into_inner()).iter().flatten().cloned().collect();
    let written = std::fs::create_dir_all(dir)
        .and_then(|()| write_atomically(&dir.join(LIBRARY_FILE), &blob))
        .and_then(|()| write_atomically(&dir.join(EARLY_FILE), used.join("\n").as_bytes()));
    match written {
        Ok(()) => {
            let mut l = library.lock().unwrap_or_else(|e| e.into_inner());
            // Keep pipelines stored while the files were being written.
            l.added = l.added.saturating_sub(saved_additions);
            tracing::info!(
                bytes = blob.len(),
                early = used.len(),
                made = MADE.load(Ordering::Relaxed),
                loaded = LOADED.load(Ordering::Relaxed),
                started_early = EARLY.load(Ordering::Relaxed),
                "host pipeline library saved"
            );
        }
        Err(e) => {
            // At least one cache file was not published. Keep the counters
            // dirty for the next save.
            if used_changed {
                USED_CHANGED.store(true, Ordering::Relaxed);
            }
            tracing::warn!(error = %e, "host pipeline library not saved");
        }
    }
}

/// Writes `bytes` to a unique temporary file beside `path`, then renames it
/// over `path`: a reader never sees a part, and overlapping saves do not
/// share a temporary file.
fn write_atomically(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let extension = format!(
        "tmp-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let tmp = path.with_extension(extension);
    let written = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
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
        USED_CHANGED.store(true, Ordering::Relaxed);
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

    /// The compile hold: upstream's by default, the one before it on
    /// demand, nothing taken from a malformed value.
    #[test]
    fn the_compile_hold_is_set_by_its_switch() {
        assert_eq!(parse_hold(None), Some(HOLD));
        assert_eq!(parse_hold(Some(" ")), Some(HOLD));
        assert_eq!(HOLD.endless, Duration::from_secs(60));
        assert_eq!((HOLD.even_so, HOLD.growth, HOLD.least_free), (Duration::from_secs(300), 8 << 30, 2 << 30));
        let before = Hold {
            endless: Duration::from_secs(20),
            even_so: Duration::from_secs(150),
            growth: 3 << 30,
            least_free: 0,
        };
        assert_eq!(parse_hold(Some("20,150,3,0")), Some(before));
        let some = parse_hold(Some("90, 600")).unwrap();
        assert_eq!((some.endless, some.even_so, some.growth), (Duration::from_secs(90), Duration::from_secs(600), HOLD.growth));
        assert_eq!(parse_hold(Some("1.5")).unwrap().endless, Duration::from_millis(1500));
        assert_eq!(parse_hold(Some("60,300,0.5")).unwrap().growth, 1 << 29);
        for bad in ["sixty", "60,-1", "1,2,3,4,5", "60,,8", "inf"] {
            assert_eq!(parse_hold(Some(bad)), None, "{bad}");
        }
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
}
