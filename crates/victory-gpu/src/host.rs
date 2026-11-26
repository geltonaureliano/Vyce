//! The host GPU: one D3D12 device and direct queue (decision 0007), and a
//! window to present on.
//!
//! The title's submissions return before the host GPU has run them
//! (`crate::flight`).
//! shortcut: the next one is prepared only once they are finished.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, PCWSTR, w};

use crate::flight::{Flight, Line, Range, Write};
use crate::telemetry::Count;

/// Resource states without the console's own bits (compression hints from
/// bit 25 up), which the host rejects.
pub(crate) fn pc_state(state: u32) -> u32 {
    state & 0x01ff_ffff
}

/// A console state as a host texture state: PC bits only, and none of the
/// buffer-only ones the console allows on textures (vertex, constant and
/// index buffer, stream out, indirect arguments, as in "generic read").
pub(crate) fn texture_state(state: u32) -> u32 {
    const BUFFER_ONLY: u32 = 0x1 | 0x2 | 0x100 | 0x200;
    pc_state(state) & !BUFFER_ONLY
}

/// Render target views live only until their command is recorded, so a
/// ring of host descriptors is enough.
const RTV_RING: u32 = 1024;

/// Whether the title's submissions may return before the host GPU is done
/// (`VICTORY_ASYNC_SUBMIT=0` turns it off). At the stilt house with 8,400
/// draws a frame it is 150-186 ms a frame against 200-238 (run as5 against
/// rw1, 2026-11-25): the wait for the host GPU goes from 78-95 to 4-17 ms.
/// It gained nothing while the title's recording threads took turns on the
/// views table's mutex.
fn async_submit() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_ASYNC_SUBMIT").is_none_or(|v| v != "0"))
}

/// Microseconds spent bringing buffer copies up to date while the
/// submission before ran ([`Host::prewarm`]), since the last frame line.
static PREWARM_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Whether a submission is one call of `ExecuteCommandLists`
/// (`VYCE_ONE_ECL=0`: one for each list, as before). Each call is a kernel
/// submission, 20 to 30 us of this thread, and an end of the host GPU's
/// work at which it empties its caches; some 250 a frame were 6 ms, and as
/// many gaps between the lists. What the end of a call gave the next list
/// to rely on, every list now ends with ([`HostList::close`]).
fn one_ecl() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VYCE_ONE_ECL").is_none_or(|v| v != "0"))
}

/// Whether a present goes behind the work in flight instead of waiting for
/// it, so that the host GPU finishes a frame while the title begins the next
/// (`VYCE_FREE_PRESENT=0`: it waits, as before). The title's own fences
/// bound how far it gets ahead. Not while tiles are given back at presents
/// (`VICTORY_SWEEP_TILES`), which work in flight may still read.
pub(crate) fn free_present() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("VYCE_FREE_PRESENT").is_none_or(|v| v != "0")
            && std::env::var("VICTORY_SWEEP_TILES").map_or(true, |v| v.trim() == "0")
    })
}

/// The most lists in one call (`VYCE_ECL_LISTS`, 32).
fn ecl_lists() -> u64 {
    static MOST: OnceLock<u64> = OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("VYCE_ECL_LISTS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(32).max(1)
    })
}

/// How long a list of a submission took lately at most, in microseconds
/// ([`ran`]): the host resets the device when one call runs for about 2 s.
/// Not known at first: five lists to a call, until some have run.
static LIST_US: AtomicU64 = AtomicU64::new(100_000);

/// A submission of `lists` lists ran, `took` after it was sent. A bound, not
/// a measure: the host GPU may have had other work before.
pub(crate) fn ran(lists: usize, took: std::time::Duration) {
    if lists == 0 {
        return;
    }
    let each = took.as_micros() as u64 / lists as u64;
    // Forgotten slowly: the frame's next submissions are not all alike.
    let before = LIST_US.load(Ordering::Relaxed);
    LIST_US.store(each.max(before - before / 8), Ordering::Relaxed);
}

/// The most lists the next call may take: those that ran for half a second
/// lately.
fn lists_of_a_call(list_us: u64, most: u64) -> usize {
    const CALL_US: u64 = 500_000;
    (CALL_US / list_us.max(1)).clamp(1, most) as usize
}

/// The calls a submission is given in: runs of its lists (by index), each of
/// at most `most` and none with a list twice (a list's own buffers start
/// each call anew).
fn calls(lists: &[usize], most: usize) -> Vec<std::ops::Range<usize>> {
    let mut out: Vec<std::ops::Range<usize>> = Vec::new();
    for (i, list) in lists.iter().enumerate() {
        match out.last_mut() {
            Some(call) if call.len() < most && !lists[call.clone()].contains(list) => call.end = i + 1,
            _ => out.push(i..i + 1),
        }
    }
    out
}

/// Time (microseconds) a thread stood waiting for the host GPU.
pub(crate) fn waited(us: u64) {
    SUBMISSION_US[1].fetch_add(us, Ordering::Relaxed);
}

/// Every submission up to number `submission` is finished.
pub(crate) fn finished(submission: u64) {
    FINISHED.fetch_max(submission, Ordering::AcqRel);
    bury(submission);
}

pub(crate) struct Host {
    pub(crate) device: ID3D12Device,
    pub(crate) depth_bounds_supported: bool,
    pub(crate) fully_typed_casts: bool,
    /// Resizable BAR (`D3D12_HEAP_TYPE_GPU_UPLOAD`) supported by device/OS.
    pub(crate) gpu_upload: bool,
    queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    /// The last value signalled on `fence`, behind the submission lock.
    submitted: Mutex<u64>,
    /// Submissions not waited for.
    line: Arc<Line>,
    /// Whether the title's submissions may return before they ran (not in
    /// tests, but those about it).
    overlap: std::sync::atomic::AtomicBool,
    event: HANDLE,
    rtvs: ID3D12DescriptorHeap,
    rtv_size: u32,
    /// A ring of depth views, used as `rtvs`.
    dsvs: ID3D12DescriptorHeap,
    dsv_size: u32,
    next_dsv: AtomicU32,
    /// With `VICTORY_D3D12_DEBUG` set (diagnostics builds): the debug
    /// layer's messages.
    messages: Option<ID3D12InfoQueue>,
    next_rtv: AtomicU32,
    presenter: Mutex<Option<Presenter>>,
    /// Device-local VRAM card buffer engine (Wave64 GPU page hashing), lazily initialized.
    card: OnceLock<Option<Mutex<crate::card::CardEngine>>>,
    /// Immutable source for resetting each device-local indirect budget.
    indirect_zero: OnceLock<Option<ID3D12Resource>>,
}

// SAFETY: D3D12 devices, queues, fences and descriptor heaps are
// free-threaded; the event handle is only waited on under `submitted`.
unsafe impl Send for Host {}
// SAFETY: as above.
unsafe impl Sync for Host {}

/// Host objects the title let go of while a submission may still use them
/// (a texture its descriptors name, a descriptor slot's old resource), by
/// the submission count they wait for. The title's descriptor writes and
/// releases reach the host at once, from its own threads, while the host
/// runs a submission for most of a second in the world: freeing there was
/// a GPU page fault on the freed texture (DXGI_ERROR_DEVICE_HUNG,
/// 2026-11-24, RX 7600).
static RETIRED: Mutex<std::collections::VecDeque<Retired>> =
    Mutex::new(std::collections::VecDeque::new());
/// Submissions started and finished (waited for).
static STARTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static FINISHED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Presented host frames a retired object is also kept for. Worker threads
/// record lists ahead of submission and a title frame spans many
/// submissions, so finishing the submissions running when it was retired
/// is not enough on its own. Ages are in frames presented to the host, not
/// the title's 60 Hz clock (which ticks many times per host frame while
/// the host runs at 1-2 fps).
const KEEP_FRAMES: u64 = 32;

/// And the time a retired object is kept for at least: in the menus 32
/// frames are a third of a second, and lists recorded ahead, or descriptors
/// the title left in a heap, named copies freed by then (with
/// `VICTORY_RAYS=1`: page faults inside destroyed 4 and 10.78 MiB buffers,
/// Radeon GPU Detective, runs rt2 and rt6; with nothing freed the run went
/// on into the world, rt3).
/// shortcut: a time, not a count of what still names the object.
const KEEP_TIME: std::time::Duration = std::time::Duration::from_secs(8);

/// A retired object, dropped on whatever thread finishes the submission.
struct Retired {
    /// The submission count it waits for.
    submission: u64,
    /// The presented frame it was retired in.
    frame: u64,
    /// When.
    at: std::time::Instant,
    #[allow(dead_code)]
    object: Box<dyn std::any::Any>,
}
// SAFETY: what is retired is D3D12 objects (free-threaded) and the
// reference-counted wrappers holding them, dropped once.
unsafe impl Send for Retired {}

/// Drops `object` once every submission running now has finished and
/// [`KEEP_FRAMES`] more frames were presented.
pub(crate) fn retire<T: 'static>(object: T) {
    let submission = STARTED.load(Ordering::Acquire);
    let frame = crate::queue::presented();
    RETIRED.lock().unwrap_or_else(|e| e.into_inner()).push_back(Retired {
        submission,
        frame,
        at: std::time::Instant::now(),
        object: Box::new(object),
    });
}

/// Drops what waited for submissions up to `finished` and is old enough
/// in presented frames.
fn bury(finished: u64) {
    // Diagnostics (`VICTORY_KEEP_RETIRED`): nothing is freed, to tell a use
    // after free from a wild address.
    if cfg!(feature = "diagnostics") && std::env::var_os("VICTORY_KEEP_RETIRED").is_some() {
        return;
    }
    let now = crate::queue::presented();
    let done: Vec<Retired> = {
        let mut retired = RETIRED.lock().unwrap_or_else(|e| e.into_inner());
        // In the order they came: the old ones are at the front.
        let n = retired
            .iter()
            .take_while(|r| {
                r.submission <= finished
                    && now.saturating_sub(r.frame) >= KEEP_FRAMES
                    && r.at.elapsed() >= KEEP_TIME
            })
            .count();
        retired.drain(..n).collect()
    };
    drop(done);
}

/// Time spent in submissions since the last frame-rate line, in
/// microseconds: preparing them on the CPU (title memory copies, texture
/// detiling), then waiting for the GPU.
static SUBMISSION_US: [std::sync::atomic::AtomicU64; 2] =
    [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)];

/// After submissions: shader-written ranges compared and copied back into
/// title memory (microseconds, bytes that differed), since the last
/// frame-rate line.
static COPY_BACK: [std::sync::atomic::AtomicU64; 2] =
    [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)];

/// Bindless heaps' slots made ready (microseconds), since the last
/// frame-rate line: plans at submission (part of the preparing), and
/// placed textures as draws are recorded.
pub(crate) static BINDLESS_US: [std::sync::atomic::AtomicU64; 2] =
    [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)];

/// Of the preparing: title memory copies (microseconds, bytes) and texture
/// refreshes (microseconds, bytes hashed), since the last frame-rate line.
pub(crate) static PREPARING: [std::sync::atomic::AtomicU64; 4] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// What the runtime does when the host GPU fails for good: report it like a
/// crash and end the run. The console has no such state, and carrying on
/// draws nothing at seconds per frame.
static ON_FAILURE: OnceLock<fn(&str) -> !> = OnceLock::new();

/// Sets what a host GPU failure does (the runtime's crash report).
pub fn on_failure(report: fn(&str) -> !) {
    let _ = ON_FAILURE.set(report);
}

/// Ends the run on a host GPU failure: what failed, the removal reason and
/// the debug layer's first error, if any (`VICTORY_D3D12_DEBUG=1` in a
/// diagnostics build names the command).
pub(crate) fn fail(what: std::fmt::Arguments) -> ! {
    let mut text = what.to_string();
    if let Some(host) = host() {
        // SAFETY: a plain query.
        if let Err(e) = unsafe { host.device.GetDeviceRemovedReason() } {
            text.push_str(&format!("; device removed: {e}"));
        }
        match host.log_messages() {
            Some(first) => text.push_str(&format!("; debug layer: {first}")),
            None if host.messages.is_none() => text.push_str(
                "; a diagnostics build with VICTORY_D3D12_DEBUG=1 names the command",
            ),
            None => {}
        }
    }
    tracing::error!("{text}");
    match ON_FAILURE.get() {
        Some(report) => report(&text),
        None => std::process::abort(),
    }
}

/// The host GPU, created on first use; `None` without one (a run then
/// behaves as before: nothing executes).
static HOST: OnceLock<Option<Host>> = OnceLock::new();

/// The host GPU's memory use as its driver reports it, `[(used, budget)]`
/// in video memory and in system memory; `None` before the device is
/// made (asking does not make it).
pub(crate) fn video_memory() -> Option<[(u64, u64); 2]> {
    static ADAPTER: OnceLock<Option<IDXGIAdapter3>> = OnceLock::new();
    let host = HOST.get()?.as_ref()?;
    let adapter = ADAPTER
        .get_or_init(|| {
            // SAFETY: plain DXGI queries on a live device.
            unsafe {
                let luid = host.device.GetAdapterLuid();
                let factory: IDXGIFactory4 =
                    CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
                factory.EnumAdapterByLuid::<IDXGIAdapter3>(luid).ok()
            }
        })
        .as_ref()?;
    let query = |group| {
        let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        // SAFETY: node 0 of the device's own adapter, a sized out structure.
        unsafe { adapter.QueryVideoMemoryInfo(0, group, &mut info) }.ok()?;
        Some((info.CurrentUsage, info.Budget))
    };
    Some([
        query(DXGI_MEMORY_SEGMENT_GROUP_LOCAL)?,
        query(DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL)?,
    ])
}

pub(crate) fn host() -> Option<&'static Host> {
    HOST.get_or_init(|| match create() {
        Ok(host) => Some(host),
        Err(e) => {
            tracing::warn!(error = %e, "no host D3D12 device; command lists will not run");
            None
        }
    })
    .as_ref()
}

/// The host GPU's driver: its maker's PCI id and the driver's version.
pub(crate) fn driver() -> &'static str {
    static DRIVER: OnceLock<String> = OnceLock::new();
    DRIVER.get_or_init(|| host().and_then(|h| h.driver()).unwrap_or_else(|| "unknown".into()))
}

/// The host GPU's submissions in flight, if there is a host GPU (asking does
/// not make it).
pub(crate) fn line() -> Option<&'static Arc<Line>> {
    HOST.get().and_then(Option::as_ref).map(|h| &h.line)
}

fn create() -> windows::core::Result<Host> {
    // SAFETY: plain D3D12 object creation with valid descriptions.
    unsafe {
        // The debug layer only in builds with diagnostics.
        let debug =
            cfg!(feature = "diagnostics") && std::env::var_os("VICTORY_D3D12_DEBUG").is_some();
        if debug {
            let mut layer: Option<ID3D12Debug> = None;
            match D3D12GetDebugInterface(&mut layer) {
                Ok(()) => layer.iter().for_each(|l| l.EnableDebugLayer()),
                Err(e) => tracing::warn!(error = %e, "no D3D12 debug layer"),
            }
            // GPU-based validation (`VICTORY_D3D12_GBV`): out-of-range
            // descriptor heap indices and the like become messages naming
            // the command instead of page faults. Without state tracking
            // (directly indexed textures are not transitioned).
            if std::env::var_os("VICTORY_D3D12_GBV").is_some()
                && let Some(l) = &layer
            {
                if let Ok(d1) = l.cast::<ID3D12Debug1>() {
                    d1.SetEnableGPUBasedValidation(true);
                }
                if let Ok(d2) = l.cast::<ID3D12Debug2>() {
                    d2.SetGPUBasedValidationFlags(
                        D3D12_GPU_BASED_VALIDATION_FLAGS_DISABLE_STATE_TRACKING,
                    );
                }
                tracing::info!("GPU-based validation on");
            }
        }
        // Diagnostics: device-removed data (the command that faulted, the
        // faulting address), read when the device is removed.
        if dred_on() {
            let mut dred: Option<ID3D12DeviceRemovedExtendedDataSettings> = None;
            if D3D12GetDebugInterface(&mut dred).is_ok()
                && let Some(dred) = dred
            {
                dred.SetAutoBreadcrumbsEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
                dred.SetPageFaultEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
            }
        }
        let mut device: Option<ID3D12Device> = None;
        // Diagnostics: the software adapter, on which a shader's wild access
        // is an access violation in this process (a crash report with the
        // address and instruction) instead of a GPU fault and driver reset.
        if cfg!(feature = "diagnostics") && std::env::var_os("VICTORY_D3D12_WARP").is_some() {
            let adapter = warp_adapter()?;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device)?;
            tracing::info!("host device on the software adapter (WARP)");
        } else {
            match fastest_adapter() {
                Ok(adapter) => D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device)?,
                Err(e) => {
                    tracing::warn!(error = %e, "no adapter preference; the default is used");
                    D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device)?;
                }
            }
        }
        let device = device.ok_or_else(windows::core::Error::empty)?;
        let messages = debug
            .then(|| device.cast::<ID3D12InfoQueue>().ok())
            .flatten();
        let host = Host::from_device(device, messages)?;
        let views = host.most_views();
        if views < victory_shader::emit::VIEW_SLOTS {
            // Before any shader is translated: their indices stay inside.
            victory_shader::emit::set_host_views(views);
            tracing::info!(views, "the host's view heaps hold fewer views than the title's");
        }
        tracing::info!("host D3D12 device ready");
        Ok(host)
    }
}

impl Host {
    fn from_device(
        device: ID3D12Device,
        messages: Option<ID3D12InfoQueue>,
    ) -> windows::core::Result<Host> {
        // SAFETY: plain D3D12 feature query and queue, fence, event and
        // descriptor heap creation.
        unsafe {
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS2::default();
            let depth_bounds_supported = device
                .CheckFeatureSupport(
                    D3D12_FEATURE_D3D12_OPTIONS2,
                    (&raw mut options).cast(),
                    size_of_val(&options) as u32,
                )
                .is_ok()
                && options.DepthBoundsTestSupported.as_bool();
            let mut options3 = D3D12_FEATURE_DATA_D3D12_OPTIONS3::default();
            let fully_typed_casts = device
                .CheckFeatureSupport(
                    D3D12_FEATURE_D3D12_OPTIONS3,
                    (&raw mut options3).cast(),
                    size_of_val(&options3) as u32,
                )
                .is_ok()
                && options3.CastingFullyTypedFormatSupported.as_bool();
            let mut options16 = D3D12_FEATURE_DATA_D3D12_OPTIONS16::default();
            let gpu_upload = !cfg!(test)
                && std::env::var_os("VICTORY_GPU_UPLOAD").is_some_and(|v| v == "1")
                && device
                    .CheckFeatureSupport(
                        D3D12_FEATURE_D3D12_OPTIONS16,
                        (&raw mut options16).cast(),
                        size_of_val(&options16) as u32,
                    )
                    .is_ok()
                && options16.GPUUploadHeapSupported.as_bool();
            tracing::info!(gpu_upload, "D3D12 GPU upload heap (Resizable BAR)");
            let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                ..Default::default()
            })?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
            let event = CreateEventW(None, false, false, None)?;
            let rtvs: ID3D12DescriptorHeap =
                device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                    NumDescriptors: RTV_RING,
                    ..Default::default()
                })?;
            let rtv_size = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV);
            let dsvs: ID3D12DescriptorHeap =
                device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_DSV,
                    NumDescriptors: RTV_RING,
                    ..Default::default()
                })?;
            let dsv_size = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_DSV);
            let line = Line::new(device.clone(), queue.clone(), fence.clone());
            Ok(Host {
                device,
                depth_bounds_supported,
                fully_typed_casts,
                gpu_upload,
                queue,
                fence,
                submitted: Mutex::new(0),
                line,
                overlap: std::sync::atomic::AtomicBool::new(!cfg!(test)),
                event,
                rtvs,
                rtv_size,
                dsvs,
                dsv_size,
                next_dsv: AtomicU32::new(0),
                messages,
                next_rtv: AtomicU32::new(0),
                presenter: Mutex::new(None),
                card: OnceLock::new(),
                indirect_zero: OnceLock::new(),
            })
        }
    }

    fn driver(&self) -> Option<String> {
        // SAFETY: plain DXGI queries about the device's own adapter.
        unsafe {
            let factory: IDXGIFactory4 =
                CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
            let adapter: IDXGIAdapter1 =
                factory.EnumAdapterByLuid(self.device.GetAdapterLuid()).ok()?;
            let maker = adapter.GetDesc1().ok()?.VendorId;
            let v = adapter.CheckInterfaceSupport(&IDXGIDevice::IID).unwrap_or_default();
            Some(format!(
                "{maker:04x} {}.{}.{}.{}",
                v >> 48 & 0xffff,
                v >> 32 & 0xffff,
                v >> 16 & 0xffff,
                v & 0xffff
            ))
        }
    }

    /// The most views a shader-visible heap of this host holds, if fewer than
    /// shaders are made for (NVIDIA: 1,000,000).
    fn most_views(&self) -> u32 {
        const WANTED: u32 = victory_shader::emit::VIEW_SLOTS;
        // SAFETY: plain queries and a heap that is let go of at once.
        unsafe {
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS19::default();
            let asked = self
                .device
                .CheckFeatureSupport(
                    D3D12_FEATURE_D3D12_OPTIONS19,
                    (&raw mut options).cast(),
                    size_of_val(&options) as u32,
                )
                .is_ok();
            if asked && options.MaxViewDescriptorHeapSize >= WANTED {
                return WANTED;
            }
            let whole: windows::core::Result<ID3D12DescriptorHeap> =
                self.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: WANTED,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                });
            if whole.is_ok() {
                WANTED
            } else {
                crate::descriptor::host::PORTABLE_VIEWS
            }
        }
    }

    fn card_engine(&self) -> Option<&Mutex<crate::card::CardEngine>> {
        self.card
            .get_or_init(|| match crate::card::CardEngine::new(self) {
                Ok(engine) => {
                    tracing::info!("VRAM card buffer engine ready (Wave64 GPU hashing)");
                    Some(Mutex::new(engine))
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "VRAM card buffer engine unavailable; falling back to host RAM"
                    );
                    None
                }
            })
            .as_ref()
    }

    pub(crate) fn card_ready(&self) -> bool {
        self.card_engine().is_some()
    }

    /// Executes a single command list on the direct queue and waits for it to
    /// finish. Caller MUST hold `self.submitted`.
    pub(crate) fn execute_and_wait_locked(&self, cmd: ID3D12CommandList, submitted: &mut u64) {
        *submitted += 1;
        let fence_val = *submitted;
        // SAFETY: caller holds `self.submitted`; `cmd` is closed; fence & event are ours.
        unsafe {
            self.queue.ExecuteCommandLists(&[Some(cmd)]);
            let waited = self
                .queue
                .Signal(&self.fence, fence_val)
                .and_then(|()| self.fence.SetEventOnCompletion(fence_val, self.event));
            match waited {
                Ok(()) => {
                    WaitForSingleObject(self.event, INFINITE);
                }
                Err(e) => tracing::warn!(error = %e, "card GPU wait failed"),
            }
        }
    }

    /// Creates an isolated WARP-backed `Host` for unit tests without touching
    /// the process-global host singleton.
    #[cfg(test)]
    pub(crate) fn new_warp() -> Option<Host> {
        let adapter = warp_adapter().ok()?;
        let mut device: Option<ID3D12Device> = None;
        // SAFETY: `adapter` is a verified software WARP adapter.
        unsafe { D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()? };
        Host::from_device(device?, None).ok()
    }

    /// The same on the machine's own GPU, with a device of its own.
    #[cfg(test)]
    pub(crate) fn new_hardware() -> Option<Host> {
        let mut device: Option<ID3D12Device> = None;
        // SAFETY: plain device creation.
        unsafe { D3D12CreateDevice(&fastest_adapter().ok()?, D3D_FEATURE_LEVEL_12_0, &mut device).ok()? };
        Host::from_device(device?, None).ok()
    }

    /// This host, whose title submissions may be in flight, for the rest of
    /// the process: other threads use it.
    #[cfg(test)]
    pub(crate) fn overlapping(self) -> &'static Host {
        self.overlap.store(true, Ordering::Relaxed);
        Box::leak(Box::new(self))
    }

    /// What the host queue is given from here on runs only once the fence
    /// returned is signalled 1. (`gate_queue_for_test` of
    /// github.com/DexNom/VIctory.)
    #[cfg(test)]
    pub(crate) fn gate(&self) -> ID3D12Fence {
        // SAFETY: plain creation, and a wait of our queue.
        unsafe {
            let gate: ID3D12Fence = self.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            self.queue.Wait(&gate, 1).unwrap();
            gate
        }
    }

    /// Sends `list` behind what is in flight, with its bottom-of-pipe writes.
    #[cfg(test)]
    pub(crate) fn send_behind(&self, list: &Arc<HostList>) {
        self.send_writing(list, take_writes(&[list]));
    }
}

/// The adapter Windows ranks fastest, so a machine with an integrated GPU
/// too runs on its discrete one.
fn fastest_adapter() -> windows::core::Result<IDXGIAdapter1> {
    // SAFETY: plain DXGI adapter queries.
    unsafe {
        let factory: IDXGIFactory6 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
        let adapter: IDXGIAdapter1 =
            factory.EnumAdapterByGpuPreference(0, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)?;
        let desc = adapter.GetDesc1()?;
        let end = desc.Description.iter().position(|&c| c == 0).unwrap_or(desc.Description.len());
        tracing::info!(
            name = String::from_utf16_lossy(&desc.Description[..end]),
            vram_mb = desc.DedicatedVideoMemory >> 20,
            "host adapter"
        );
        Ok(adapter)
    }
}

/// DXGI may return hardware for `EnumWarpAdapter` (notably under Proton).
/// Reject it before creating a device in diagnostics' software-only mode.
fn warp_adapter() -> windows::core::Result<IDXGIAdapter> {
    // SAFETY: plain DXGI adapter queries.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
        let adapter: IDXGIAdapter = factory.EnumWarpAdapter()?;
        let desc = adapter.cast::<IDXGIAdapter1>()?.GetDesc1()?;
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
            tracing::warn!(
                vendor = format_args!("{:#x}", desc.VendorId),
                flags = format_args!("{:#x}", desc.Flags),
                "DXGI did not identify the WARP adapter as software; refusing device"
            );
            return Err(windows::core::Error::from_hresult(
                windows::Win32::Foundation::E_FAIL,
            ));
        }
        Ok(adapter)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn nested_parallel_work_reuses_the_current_worker() {
        let processed = std::sync::atomic::AtomicUsize::new(0);
        super::in_parallel(&[0; 12], 1, |outer| {
            let worker = std::thread::current().id();
            super::in_parallel(&[0; 4], 1, |inner| {
                assert_eq!(std::thread::current().id(), worker);
                processed.fetch_add(outer.len() * inner.len(), Ordering::Relaxed);
            });
        });
        assert_eq!(processed.into_inner(), 48);
    }

    #[test]
    fn sparse_copies_preserve_islands_and_observe_remaps() {
        use windows_sys::Win32::System::Memory::*;
        const N: usize = 5 << 16;
        // SAFETY: we reserve a private region, commit selected pages inside it,
        // and only directly access the pages while they are writable.
        unsafe {
            let base = VirtualAlloc(std::ptr::null(), N, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            let mut expected = vec![0; N];
            for (offset, value) in [(0, 9), (2 << 16, 37), (N - 4096, 85)] {
                let at = base.add(offset);
                assert_eq!(VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE), at.cast());
                at.write_bytes(value, 4096);
                expected[offset..offset + 4096].fill(value);
            }
            for remap in [false, true] {
                if remap {
                    let mut old = 0;
                    assert_ne!(VirtualProtect(base.cast(), 4096, PAGE_NOACCESS, &mut old), 0);
                    expected[..4096].fill(0);
                    let at = base.add(1 << 16);
                    assert_eq!(VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE), at.cast());
                    at.write_bytes(0xc7, 4096);
                    expected[1 << 16..(1 << 16) + 4096].fill(0xc7);
                }
                let mut out = vec![0xee; N];
                super::copy_mapped(out.as_mut_ptr().add(27), base.add(27), N - 54, false);
                assert_eq!(out[27..N - 27], expected[27..N - 27]);
                assert!(out[..27].iter().chain(&out[N - 27..]).all(|&v| v == 0xee));
            }
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }

    use super::*;

    #[test]
    fn a_call_takes_its_most_lists_and_none_twice() {
        assert_eq!(calls(&[1, 2, 3, 4, 5], 2), [0..2, 2..4, 4..5]);
        assert_eq!(calls(&[1, 2, 3], 32), [0..3]);
        assert_eq!(calls(&[1, 2, 1, 3, 2], 32), [0..2, 2..5]);
        assert_eq!(calls(&[7, 7, 7], 8), [0..1, 1..2, 2..3]);
        assert_eq!(calls(&[1, 2, 3], 1), [0..1, 1..2, 2..3]);
        assert!(calls(&[], 4).is_empty());
    }

    #[test]
    fn a_call_takes_what_ran_for_half_a_second_lately() {
        assert_eq!(lists_of_a_call(0, 32), 32);
        // A steady frame's lists: some hundred microseconds each.
        assert_eq!(lists_of_a_call(800, 32), 32);
        assert_eq!(lists_of_a_call(100_000, 32), 5);
        assert_eq!(lists_of_a_call(600_000, 32), 1);
        assert_eq!(lists_of_a_call(u64::MAX, 32), 1);
    }

    /// Three compute programs over one root signature: one fills a buffer,
    /// one reads it every way shaders read buffers, one is run by arguments
    /// in it.
    const ACROSS: &str = r#"
#define RS "RootConstants(num32BitConstants=1, b0), UAV(u0), SRV(t0), CBV(b1), UAV(u1)"
cbuffer Seed : register(b0) { uint seed; };
cbuffer Same : register(b1) { uint4 same[16]; };
RWByteAddressBuffer filled : register(u0);
ByteAddressBuffer read : register(t0);
RWByteAddressBuffer found : register(u1);
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void fill(uint3 id : SV_DispatchThreadID) {
    // The first three dwords are arguments of a dispatch: seed groups.
    uint v = id.x == 0 ? seed : id.x < 3 ? 1 : seed * 1000 + id.x;
    filled.Store(id.x * 4, v);
}
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void look(uint3 id : SV_DispatchThreadID) {
    uint by_view = read.Load(id.x * 4);
    uint by_constant = id.x < 64 ? same[id.x / 4][id.x % 4] : by_view;
    uint wanted = id.x == 0 ? seed : id.x < 3 ? 1 : seed * 1000 + id.x;
    uint before;
    if (by_view != wanted) found.InterlockedAdd(0, 1, before);
    if (by_constant != wanted) found.InterlockedAdd(4, 1, before);
}
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void count(uint3 id : SV_DispatchThreadID) {
    uint before;
    found.InterlockedAdd(8, 1, before);
}
"#;

    /// Lists of one `ExecuteCommandLists` call, none with a barrier of its
    /// own: what one's shaders wrote, the next ones find as shader reads, as
    /// constants, as arguments and as a copy's source, in the host GPU's
    /// memory and in ours. On the machine's own GPU.
    #[test]
    fn lists_of_one_call_see_what_those_before_wrote() {
        const DWORDS: u32 = 64 << 10;
        let Some(host) = host() else { return };
        if !one_ecl() {
            return;
        }
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::ID3DBlob;
        use windows::core::{PCSTR, s};
        let compile = |entry: PCSTR| {
            let (mut code, mut errors) = (None::<ID3DBlob>, None::<ID3DBlob>);
            // SAFETY: the source and names live through the call; out
            // pointers ours; the blob owns the bytes copied.
            unsafe {
                D3DCompile(
                    ACROSS.as_ptr().cast(),
                    ACROSS.len(),
                    s!("across"),
                    None,
                    None,
                    entry,
                    s!("cs_5_1"),
                    0,
                    0,
                    &mut code,
                    Some(&mut errors),
                )
                .expect("the test's shader compiles");
                let code = code.unwrap();
                let bytes = code.GetBufferPointer().cast::<u8>();
                std::slice::from_raw_parts(bytes, code.GetBufferSize()).to_vec()
            }
        };
        let codes = [s!("fill"), s!("look"), s!("count")].map(compile);
        // SAFETY: the compiled shaders, whose root signature is inside.
        let (root, psos) = unsafe {
            let root: ID3D12RootSignature = host.device.CreateRootSignature(0, &codes[0]).unwrap();
            let psos = codes.each_ref().map(|code| -> ID3D12PipelineState {
                host.device
                    .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                        pRootSignature: std::mem::transmute_copy(&root),
                        CS: D3D12_SHADER_BYTECODE {
                            pShaderBytecode: code.as_ptr().cast(),
                            BytecodeLength: code.len(),
                        },
                        ..Default::default()
                    })
                    .unwrap()
            });
            (root, psos)
        };
        let argument = D3D12_INDIRECT_ARGUMENT_DESC {
            Type: D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH,
            ..Default::default()
        };
        let mut signature: Option<ID3D12CommandSignature> = None;
        // SAFETY: a description of one argument; no root arguments.
        unsafe {
            host.device
                .CreateCommandSignature(
                    &D3D12_COMMAND_SIGNATURE_DESC {
                        ByteStride: 12,
                        NumArgumentDescs: 1,
                        pArgumentDescs: &argument,
                        NodeMask: 0,
                    },
                    None::<&ID3D12RootSignature>,
                    &mut signature,
                )
                .unwrap()
        };
        let signature = signature.unwrap();
        let bytes = u64::from(DWORDS) * 4;
        let (ours, _) = host.shared_buffer(bytes).unwrap();
        let theirs = host.uav_buffer(bytes).unwrap();
        let (found, seen) = host.shared_buffer(256).unwrap();
        let (back, copied) = host.readback_buffer(bytes).unwrap();
        // SAFETY: live buffers.
        let address = |b: &ID3D12Resource| unsafe { b.GetGPUVirtualAddress() };
        let lists: Vec<HostList> = (0..4).map(|_| host.new_list().unwrap()).collect();
        for buffer in [&theirs, &ours] {
            for seed in 1..=40u32 {
                // SAFETY: the mapped buffer, nothing running.
                unsafe { seen.write_bytes(0, 256) };
                for (list, pso) in lists.iter().zip(&psos) {
                    // SAFETY: open lists; our root signature, pipelines and
                    // live buffers.
                    unsafe {
                        let l = &list.list;
                        l.SetComputeRootSignature(&root);
                        l.SetPipelineState(pso);
                        l.SetComputeRoot32BitConstants(0, 1, (&raw const seed).cast(), 0);
                        l.SetComputeRootUnorderedAccessView(1, address(buffer));
                        l.SetComputeRootShaderResourceView(2, address(buffer));
                        l.SetComputeRootConstantBufferView(3, address(buffer));
                        l.SetComputeRootUnorderedAccessView(4, address(&found));
                    }
                }
                // SAFETY: as above.
                unsafe {
                    lists[0].list.Dispatch(DWORDS / 64, 1, 1);
                    lists[1].list.Dispatch(DWORDS / 64, 1, 1);
                    lists[2].list.ExecuteIndirect(&signature, 1, buffer, 0, None::<&ID3D12Resource>, 0);
                    lists[3].list.CopyBufferRegion(&back, 0, buffer, 0, bytes);
                }
                for list in &lists {
                    list.count_work();
                    list.close();
                }
                host.execute(&lists.iter().collect::<Vec<_>>());
                // SAFETY: both mapped; everything has run.
                let (seen, copied) = unsafe {
                    (
                        std::slice::from_raw_parts(seen.cast::<u32>(), 3),
                        std::slice::from_raw_parts(copied.cast::<u32>(), DWORDS as usize),
                    )
                };
                assert_eq!(seen[..2], [0, 0], "wrong as shader reads, as constants (seed {seed})");
                assert_eq!(seen[2], seed, "groups run by arguments");
                let wrong = copied.iter().enumerate().find(|&(i, &v)| {
                    let i = i as u32;
                    v != if i == 0 { seed } else if i < 3 { 1 } else { seed * 1000 + i }
                });
                assert_eq!(wrong, None, "copied (seed {seed})");
                lists.iter().for_each(HostList::reset);
            }
        }
    }

    /// A list with no work before its waits, as the title's queues meet one
    /// another halfway: each write goes as soon as the waits before it are
    /// met, and work holds the later ones back.
    #[test]
    fn writes_before_an_unmet_wait_go_early() {
        let Some(host) = host() else { return };
        let Ok(list) = host.new_list() else { return };
        let mut memory = [0u64; 4];
        let at = memory.as_mut_ptr();
        // SAFETY: inside the array.
        let slot = |i: usize| unsafe { at.add(i) };
        let wait = |i: usize| crate::memwait::Wait {
            address: slot(i) as usize,
            comparison: 7,
            reference: 5,
            bytes: 8,
        };
        let write = |i: usize| list.defer_write(slot(i).cast(), crate::queue::Written::Value(5), 8);
        // SAFETY: as above.
        let value = |i: usize| unsafe { slot(i).read_volatile() };
        list.note_wait(wait(0));
        write(1);
        list.note_wait(wait(2));
        list.count_work();
        write(3);
        assert!(!list.write_early(None) && value(1) == 0);
        // SAFETY: as above.
        unsafe { slot(0).write_volatile(5) };
        assert_eq!(list.unmet(), [wait(2)]);
        assert!(list.write_early(None) && !list.write_early(None));
        assert_eq!((value(1), value(3)), (5, 0));
        // SAFETY: as above.
        unsafe { slot(2).write_volatile(5) };
        assert!(list.unmet().is_empty() && !list.write_early(None));
        // Run: what is left, then all of it anew for another submission.
        list.close();
        host.execute(&[&list]);
        assert_eq!(value(3), 5);
        // SAFETY: as above.
        unsafe { slot(1).write_volatile(0) };
        host.execute(&[&list]);
        assert_eq!(value(1), 5);
    }

    #[test]
    fn warp_adapter_accepts_only_software() {
        // This probes DXGI without creating a D3D12 device or running a title.
        // SAFETY: plain DXGI adapter queries.
        let raw = unsafe {
            let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
            else {
                return;
            };
            factory.EnumWarpAdapter::<IDXGIAdapter>()
        };
        let Ok(raw) = raw else { return };
        // SAFETY: EnumWarpAdapter returns a DXGI adapter, which implements
        // IDXGIAdapter1; `raw` owns that interface.
        let desc = unsafe { raw.cast::<IDXGIAdapter1>().unwrap().GetDesc1().unwrap() };
        eprintln!(
            "EnumWarpAdapter: vendor={:#x}, flags={:#x}",
            desc.VendorId, desc.Flags
        );
        assert_eq!(
            warp_adapter().is_ok(),
            desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
        );
    }
}

/// A host command list and its allocator, always used together.
pub(crate) struct HostList {
    allocator: ID3D12CommandAllocator,
    pub(crate) list: ID3D12GraphicsCommandList,
    pub(crate) depth_bounds: Option<ID3D12GraphicsCommandList1>,
    /// Work for submission time, in recorded order.
    deferred: Mutex<Deferred>,
    /// Upload memory for data the list's draws make (expanded vertices),
    /// reused after a reset.
    scratch: Mutex<Scratch>,
    /// Host texture states the list assumes, by host copy: at its start and
    /// at its end. Lists are recorded on many threads and run in submission
    /// order, so the transitions from a texture's real state are made at
    /// submission, in `prelude`. Holding the copies keeps every texture the
    /// list names alive until it is reset, even if the title releases the
    /// resource first.
    states: Mutex<States>,
    /// The list that runs those transitions just before this one.
    prelude: (ID3D12CommandAllocator, ID3D12GraphicsCommandList),
    /// What the title's list held for this recording, if the title reset or
    /// released it while a submission of it was still to run.
    kept: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
    /// Draws and dispatches recorded.
    work: Mutex<usize>,
    /// The pipelines the list binds, for reports of slow submissions.
    pipelines: Mutex<std::collections::BTreeSet<String>>,
    /// Queries begun in the list and not ended yet: host heap, type, index,
    /// and whether a draw since was not run on the host (`missed_draw`).
    queries: Mutex<Vec<(ID3D12QueryHeap, D3D12_QUERY_TYPE, u32, bool)>>,
    /// The work its indirect executions may still do (`indirect`): a
    /// shader-writable word, zeroed when the list is submitted.
    budget: OnceLock<Option<IndirectBudget>>,
    /// Device-local scratch buffer for clamped indirect arguments (`indirect`),
    /// grown to the largest indirect execution recorded on the list.
    indirect_args: Mutex<Option<(ID3D12Resource, u64)>>,
    /// The submissions in flight of the host whose list it is.
    line: Arc<Line>,
    /// Host fence value signalled behind the last submission of this list
    /// that was not waited for.
    last_fence: std::sync::atomic::AtomicU64,
    /// Dispatches and buffer copies recorded since the reset: what may have
    /// written indirect arguments (`crate::indirect` clamps hoisted before).
    writes: std::sync::atomic::AtomicU64,
    /// Buffers for clamped indirect arguments when this list holds the
    /// clamps of the list after it (`clamp_room`).
    clamp_chunks: Mutex<ClampChunks>,
    /// The texts of the markers `note_op` set, in order (for device-removed
    /// data without marker texts), with the host GPU range a copy reads.
    #[cfg(feature = "diagnostics")]
    notes: Mutex<Vec<Note>>,
    /// With `VICTORY_GPU_TIMING`: a GPU timestamp before each noted command
    /// and at the end, and where they are read back.
    #[cfg(feature = "diagnostics")]
    stamps: OnceLock<Option<(ID3D12QueryHeap, ID3D12Resource)>>,
}

pub(crate) fn warn_depth_bounds() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| tracing::warn!("the host GPU has no depth bounds test; depth bounds are ignored"));
}

/// A list's buffers for hoisted clamped indirect arguments with their
/// states, how many are in use since the reset, the bytes taken of the last
/// of those, whether the list has the barrier that precedes the clamps, and
/// whether it has the clamps' root signature, pipeline and budget bound.
#[derive(Default)]
struct ClampChunks {
    all: Vec<(ID3D12Resource, D3D12_RESOURCE_STATES)>,
    used: usize,
    at: u64,
    barrier: bool,
    bound: bool,
}

/// Timestamps per list (diagnostics); commands past them are not timed.
#[cfg(feature = "diagnostics")]
const STAMPS: u32 = 16384;

impl Drop for HostList {
    fn drop(&mut self) {
        self.line.wait(self.last_fence.load(Ordering::Acquire), Count::WaitList);
        crate::memstat::sub(&crate::memstat::LISTS, 1);
        self.release_kept();
    }
}

// SAFETY: D3D12 lists and allocators may be used from any thread, one at
// a time: the title records a list on one thread at a time, and a
// submission runs it after its recording ended (a reset while one is to
// run moves recording to a new list). The rest is locked.
unsafe impl Send for HostList {}
// SAFETY: as above.
unsafe impl Sync for HostList {}

/// A marker's text and the host range its command reads (diagnostics).
#[cfg(feature = "diagnostics")]
type Note = (String, Option<(u64, u64)>);

type States = std::collections::HashMap<
    usize,
    (u32, u32, std::sync::Arc<crate::resource::HostTexture>),
>;

/// Whether device-removed data is on (`VICTORY_D3D12_DRED`, diagnostics
/// builds).
pub(crate) fn dred_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        cfg!(feature = "diagnostics") && std::env::var_os("VICTORY_D3D12_DRED").is_some()
    })
}

/// Whether each list of a submission runs and is timed on its own, slow
/// ones logged with their commands (`VICTORY_GPU_TIMING`, diagnostics
/// builds; the commands are noted with `VICTORY_D3D12_DRED` on).
#[cfg(feature = "diagnostics")]
fn timing() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_GPU_TIMING").is_some())
}

/// Notes how long the host GPU took for a submission: how many took how
/// long, and of those of 3 ms and more which pipelines they ran.
#[cfg(feature = "diagnostics")]
fn gpu_waits(gpu: std::time::Duration, runs: &[&HostList]) {
    let mut waits = GPU_WAITS.lock().unwrap_or_else(|e| e.into_inner());
    let us = gpu.as_micros() as u64;
    let bucket = [200, 1000, 3000, 10_000, 30_000].iter().position(|&m| us < m).unwrap_or(5);
    waits.0[bucket].0 += 1;
    waits.0[bucket].1 += us;
    if us >= 3000 {
        let mut pipelines: Vec<String> = runs
            .iter()
            .flat_map(|l| l.pipelines.lock().unwrap_or_else(|e| e.into_inner()).clone())
            .collect();
        pipelines.sort_unstable();
        pipelines.dedup();
        let mut key = format!("{} lists {} pipelines:", runs.len(), pipelines.len());
        for p in pipelines.iter().take(12) {
            key.push(' ');
            key.push_str(p.trim_start_matches("[compute ").trim_end_matches(']'));
        }
        let e = waits.1.entry(key).or_insert((0, 0));
        e.0 += 1;
        e.1 += us;
    }
}

#[cfg(feature = "diagnostics")]
#[allow(clippy::type_complexity)]
static GPU_WAITS: Mutex<([(u32, u64); 6], std::collections::BTreeMap<String, (u32, u64)>)> =
    Mutex::new(([(0, 0); 6], std::collections::BTreeMap::new()));

/// Logs and forgets what [`gpu_waits`] noted.
#[cfg(feature = "diagnostics")]
fn log_gpu_waits() {
    let (buckets, slow) = std::mem::take(&mut *GPU_WAITS.lock().unwrap_or_else(|e| e.into_inner()));
    let mut text = String::new();
    for (name, (n, us)) in ["<0.2", "<1", "<3", "<10", "<30", "more"].iter().zip(buckets) {
        text.push_str(&format!(" {name} ms: {n} x, {:.0} ms;", us as f64 / 1e3));
    }
    tracing::info!("GPU waits since:{text}");
    let mut slow: Vec<_> = slow.into_iter().collect();
    slow.sort_unstable_by_key(|(_, (_, us))| std::cmp::Reverse(*us));
    for (key, (n, us)) in slow.into_iter().take(8) {
        tracing::info!("GPU wait {:.0} ms in {n} x: {key}", us as f64 / 1e3);
    }
}

/// With `VICTORY_GPU_TIMING`: GPU milliseconds and commands by what ran
/// (kind and pipeline), and the frames presented meanwhile.
#[cfg(feature = "diagnostics")]
static GPU_TIMES: Mutex<Option<std::collections::HashMap<String, (u32, f64)>>> = Mutex::new(None);
#[cfg(feature = "diagnostics")]
static TIMED_FRAMES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Logs and forgets the GPU time per kind of command, per frame.
#[cfg(feature = "diagnostics")]
fn log_gpu_times() {
    let frames = f64::from(TIMED_FRAMES.swap(0, Ordering::Relaxed).max(1));
    let Some(times) = GPU_TIMES.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return;
    };
    let total: f64 = times.iter().filter(|(k, _)| k.starts_with("all: ")).map(|(_, t)| t.1).sum();
    let mut all: Vec<_> = times.into_iter().collect();
    all.sort_by(|a, b| b.1.1.total_cmp(&a.1.1));
    tracing::info!("GPU time: {:.1} ms/frame in {} kinds of commands", total / frames, all.len());
    for (key, (n, ms)) in all.into_iter().take(45) {
        tracing::info!("GPU time {:7.2} ms/frame, {:7.1} x: {key}", ms / frames, f64::from(n) / frames);
    }
}

/// Whether each noted command waits for those before it (`VICTORY_GPU_STEP`,
/// diagnostics builds), so that a list stops at the command at fault.
#[cfg(feature = "diagnostics")]
fn step() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_GPU_STEP").is_some())
}

/// Whether submitted lists are not run on the host GPU (`VICTORY_NO_GPU_WORK`,
/// diagnostics builds).
#[cfg(feature = "diagnostics")]
fn no_work() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_NO_GPU_WORK").is_some())
}

/// Names a host object for the device-removed report, which names the
/// allocations a page fault hit (only with device-removed data on).
pub(crate) fn name(object: &impl Interface, text: impl FnOnce() -> String) {
    if dred_on()
        && let Ok(object) = object.cast::<ID3D12Object>()
    {
        // SAFETY: a plain call on a live object.
        let _ = unsafe { object.SetName(&windows::core::HSTRING::from(text())) };
    }
}

/// Upload chunks handed out front to back: the chunks, the one in use and
/// the bytes used in it.
struct IndirectBudget {
    buffer: ID3D12Resource,
    /// The fallback in system memory is cleared by the CPU before submission.
    mapped: Option<usize>,
}

#[derive(Default)]
struct Scratch {
    chunks: Vec<(ID3D12Resource, usize, u64)>,
    current: usize,
    used: u64,
}

// SAFETY: the chunk pointers are persistent mappings of upload buffers.
unsafe impl Send for Scratch {}

const SCRATCH_CHUNK: u64 = 512 << 10;

/// Scratch chunks no list uses: lists give theirs back when reset. Each list
/// keeping the chunks of its largest use added up to gigabytes, as the title
/// has hundreds of lists.
static SCRATCH_FREE: Mutex<Vec<(ID3D12Resource, usize, u64)>> = Mutex::new(Vec::new());
/// Free chunks kept; more are released.
const SCRATCH_KEPT: usize = 256;

impl Scratch {
    fn give_back(&mut self) {
        (self.current, self.used) = (0, 0);
        let mut free = SCRATCH_FREE.lock().unwrap_or_else(|e| e.into_inner());
        free.append(&mut self.chunks);
        let over = free.len().saturating_sub(SCRATCH_KEPT);
        let over: Vec<_> = free.drain(..over).collect();
        drop(free);
        drop(over);
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if self.chunks.is_empty() {
            return;
        }
        // A submission may still read them.
        let chunks = std::mem::take(&mut self.chunks);
        retire(chunks);
    }
}

/// One piece of submission-time work, in recorded order.
enum Step {
    /// A copy into title memory (destination, source, bytes): the title's
    /// buffer copies, which later refreshes must see.
    Copy(usize, usize, usize),
    /// A copy of title memory into the list's own upload memory.
    Fill(usize, usize, usize),
    /// Brings a range (address, bytes) of a buffer's host copy up to date:
    /// once per submission, whichever lists ask, until a `Copy` or `Run`
    /// intervenes.
    Refresh(std::sync::Arc<crate::shadow::Shadow>, usize, usize),
    /// Work that may write title memory.
    Run(Box<dyn Fn() + Send>),
    /// Work that writes host memory only.
    Host(Box<dyn Fn() + Send>),
    /// Shaders write a range (address, bytes) of a buffer's host copy.
    GpuWrite(std::sync::Arc<crate::shadow::Shadow>, usize, usize),
    /// Brings a texture's staging buffer up to date and, when dirty, copies
    /// it into the host texture on the list's prelude: once per submission,
    /// whichever lists sample it, until a `Copy` or `Run` intervenes.
    Texture(
        std::sync::Arc<crate::texture::Staging>,
        std::sync::Arc<crate::resource::HostTexture>,
    ),
}

/// A texture-to-buffer readback copied from a mapped readback buffer into
/// title memory once the GPU submission completes.
struct Readback {
    dst: usize,
    src: usize,
    dst_pitch: usize,
    src_pitch: usize,
    lines: usize,
    row_bytes: usize,
}

/// What a list does on the CPU when it is submitted: copies between host
/// memory regions (title buffers, upload buffers and buffer copies), and
/// the upload/readback buffers its GPU commands name, kept until reset.
#[derive(Default)]
struct Deferred {
    /// Destination, source, bytes.
    copies: Vec<Step>,
    /// Post-submission readbacks from GPU textures into title buffers.
    readbacks: Vec<Readback>,
    uploads: Vec<ID3D12Resource>,
    /// Host objects the list's commands name (pipelines, root signatures,
    /// descriptor heaps): the title may release its objects before it
    /// submits the list.
    kept: Vec<windows::core::IUnknown>,
    /// The host pipelines `bind_pipeline` kept (by address), so a pipeline
    /// the list binds again is not kept again.
    bound: std::collections::HashSet<usize>,
    /// Buffer ranges refreshed since the last title copy (host copy,
    /// address, bytes): refreshing them again would find the same bytes.
    refreshed: std::collections::HashSet<(usize, usize, usize)>,
    /// Buffer ranges declared written by shaders since the last title copy
    /// (host copy, address, bytes): declaring them again would change nothing.
    gpu_written: std::collections::HashSet<(usize, usize, usize)>,
    /// Upload memory filled from title memory since the last title write
    /// (source, bytes): its host address, which a second fill would find
    /// holding the same bytes.
    filled: std::collections::HashMap<(usize, usize), u64>,
    /// View heaps the list's draws index directly (`ResourceDescriptorHeap[]`),
    /// and whether any of their shaders may write memory: their slots are
    /// brought up to date once per submission, after every other step.
    bindless: Vec<(std::sync::Arc<crate::descriptor::host::HostHeap>, bool)>,
    /// Textures whose contents the list brings up to date.
    textures: std::collections::HashSet<usize>,
    /// Bottom-of-pipe writes, done once the list has run: address, what,
    /// bytes.
    writes: Vec<(usize, crate::queue::Written, usize)>,
    /// Waits for memory values, with how many `writes` precede them.
    waits: Vec<(usize, crate::memwait::Wait)>,
    /// The work before each of `writes`.
    marks: Vec<usize>,
    /// How many of `writes` this submission has done already.
    early: usize,
    wait_marks: Vec<usize>,
    /// Whether this recording already prepares its indirect-work budget.
    budget_zeroed: bool,
}

impl Host {
    /// Whether the host runs mesh shaders at shader model 6.6 (translated
    /// mesh shaders need both); logged once.
    pub(crate) fn mesh_shaders(&self) -> bool {
        static MESH: OnceLock<bool> = OnceLock::new();
        *MESH.get_or_init(|| {
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
            let mut model = D3D12_FEATURE_DATA_SHADER_MODEL {
                HighestShaderModel: D3D_SHADER_MODEL_6_6,
            };
            // SAFETY: each query writes its own structure of the given size.
            let ok = unsafe {
                self.device
                    .CheckFeatureSupport(
                        D3D12_FEATURE_D3D12_OPTIONS7,
                        (&raw mut options).cast(),
                        size_of_val(&options) as u32,
                    )
                    .is_ok()
                    && self
                        .device
                        .CheckFeatureSupport(
                            D3D12_FEATURE_SHADER_MODEL,
                            (&raw mut model).cast(),
                            size_of_val(&model) as u32,
                        )
                        .is_ok()
            };
            let mesh = ok
                && options.MeshShaderTier != D3D12_MESH_SHADER_TIER_NOT_SUPPORTED
                && model.HighestShaderModel.0 >= D3D_SHADER_MODEL_6_6.0;
            tracing::info!(mesh, "host mesh shader support");
            mesh
        })
    }

    /// The wave sizes the host runs (D3D12 options 1), which translated
    /// compute shaders that declare the console's wave size must fit;
    /// logged once. Unknown (the query fails): any.
    pub(crate) fn target(&self) -> victory_shader::emit::Target {
        static TARGET: OnceLock<victory_shader::emit::Target> = OnceLock::new();
        *TARGET.get_or_init(|| {
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
            // SAFETY: the query writes the structure of the given size.
            let ok = unsafe {
                self.device
                    .CheckFeatureSupport(
                        D3D12_FEATURE_D3D12_OPTIONS1,
                        (&raw mut options).cast(),
                        size_of_val(&options) as u32,
                    )
                    .is_ok()
            };
            let target = if ok && options.WaveOps.as_bool() && options.WaveLaneCountMin > 0 {
                victory_shader::emit::Target {
                    lanes_min: options.WaveLaneCountMin,
                    lanes_max: options.WaveLaneCountMax,
                }
            } else {
                victory_shader::emit::Target::ANY
            };
            tracing::info!(
                lanes_min = target.lanes_min,
                lanes_max = target.lanes_max,
                "host wave sizes"
            );
            if !target.runs(64) {
                tracing::warn!(
                    lanes_min = target.lanes_min,
                    lanes_max = target.lanes_max,
                    "host wave sizes exclude Wave64; compute shaders without a compatible translation are skipped"
                );
            }
            target
        })
    }

    /// A new list, open for recording.
    pub(crate) fn new_list(&self) -> windows::core::Result<HostList> {
        // SAFETY: plain object creation.
        unsafe {
            let allocator: ID3D12CommandAllocator = self
                .device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let list: ID3D12GraphicsCommandList = self.device.CreateCommandList(
                0,
                D3D12_COMMAND_LIST_TYPE_DIRECT,
                &allocator,
                None,
            )?;
            let prelude_allocator: ID3D12CommandAllocator = self
                .device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let prelude: ID3D12GraphicsCommandList = self.device.CreateCommandList(
                0,
                D3D12_COMMAND_LIST_TYPE_DIRECT,
                &prelude_allocator,
                None,
            )?;
            prelude.Close()?;
            crate::memstat::add(&crate::memstat::LISTS, 1);
            Ok(HostList {
                allocator,
                depth_bounds: self.depth_bounds_supported.then(|| list.cast()).transpose()?,
                list,
                deferred: Mutex::default(),
                scratch: Mutex::default(),
                states: Mutex::default(),
                prelude: (prelude_allocator, prelude),
                kept: Mutex::default(),
                work: Mutex::default(),
                writes: Default::default(),
                clamp_chunks: Mutex::default(),
                pipelines: Mutex::default(),
                queries: Mutex::default(),
                budget: OnceLock::new(),
                indirect_args: Mutex::default(),
                line: self.line.clone(),
                last_fence: std::sync::atomic::AtomicU64::new(0),
                #[cfg(feature = "diagnostics")]
                notes: Mutex::default(),
                #[cfg(feature = "diagnostics")]
                stamps: OnceLock::new(),
            })
        }
    }

    /// The submissions in flight.
    pub(crate) fn line(&self) -> &Arc<Line> {
        &self.line
    }

    /// Sends `list`, a closed list of ours that has nothing done before or
    /// after it on the CPU, behind everything in flight, and does not wait.
    fn send(&self, list: &Arc<HostList>) {
        self.send_writing(list, Vec::new());
    }

    /// [`Host::send`], with `writes` done when it has run.
    fn send_writing(&self, list: &Arc<HostList>, writes: Vec<Write>) {
        let mut submitted = self.submitted.lock().unwrap_or_else(|e| e.into_inner());
        let mut flight = Flight::new(STARTED.fetch_add(1, Ordering::AcqRel) + 1, vec![list.clone()]);
        flight.writes = writes;
        let timed = self.telemetry_clock().and_then(|clock| Some((clock, clock.pair()?)));
        let mut lists: Vec<Option<ID3D12CommandList>> = Vec::with_capacity(3);
        lists.extend(timed.map(|(clock, pair)| Some(clock.begin(pair))));
        lists.push(list.list.cast().ok());
        lists.extend(timed.map(|(clock, pair)| Some(clock.end(pair))));
        // SAFETY: closed lists, not running.
        unsafe { self.queue.ExecuteCommandLists(&lists) };
        if crate::telemetry::on() {
            crate::telemetry::add(Count::Submissions, 1);
            crate::telemetry::add(Count::Unwaited, 1);
            crate::telemetry::add(Count::Ecls, 1);
            crate::telemetry::add(Count::HostLists, lists.len() as u64);
        }
        *submitted += 1;
        flight.fence = *submitted;
        flight.timed = timed.map(|(_, pair)| pair);
        self.line.send(flight);
    }

    /// The host GPU's clock, if measurements are on.
    fn telemetry_clock(&self) -> Option<&crate::telemetry::Clock> {
        if !crate::telemetry::on() {
            return None;
        }
        self.line.clock.get_or_init(|| crate::telemetry::Clock::new(self)).as_ref()
    }

    /// Runs closed lists; waits synchronously when CPU copy-back, readbacks, or
    /// bottom-of-pipe writes require GPU completion, or returns asynchronously
    /// so CPU recording overlaps GPU execution.
    pub(crate) fn execute(&self, runs: &[&HostList]) {
        self.execute_inner(runs, None);
    }

    /// As [`Host::execute`] for the title's own submissions, which may return
    /// before the host GPU is done (`VICTORY_ASYNC_SUBMIT=0`: they do not):
    /// the title waits for its fences, which are signalled behind them
    /// (`crate::timeline`). Our own submissions (captures, read-backs,
    /// texture uploads) read or free what they made as soon as the call
    /// returns: unwaited, the present's capture buffer was freed with its
    /// copy still pending, which the debug layer names (#921, object deleted
    /// while still in use) and the AMD driver answered by removing the
    /// device at the first frame.
    ///
    /// Such a submission is finished (its shaders' writes put back, its
    /// read-backs and bottom-of-pipe writes done) once the host GPU has run
    /// it (`crate::flight`), at the latest before the next submission's own
    /// preparation. Meanwhile the next submission's buffer copies are
    /// brought up to date, so the processor and the card work at the same
    /// time for that long.
    pub(crate) fn execute_title(&self, runs: &[std::sync::Arc<HostList>]) {
        let lists: Vec<&HostList> = runs.iter().map(|l| &**l).collect();
        self.execute_inner(&lists, Some(runs));
    }

    /// Brings the buffer copies `runs` will ask for up to date while the
    /// submission before still runs: the same refreshes as the lists'
    /// steps make, early. The ranges that submission's shaders write are
    /// left alone (`Shadow::update` goes around them), and the steps in
    /// their order do it all again after the wait, finding little left.
    fn prewarm(&self, runs: &[&HostList]) {
        let started = std::time::Instant::now();
        let mut batch = Batch::default();
        for list in runs {
            for step in &list.deferred().copies {
                if let Step::Refresh(shadow, address, len) = step {
                    batch.add(shadow, *address, *len);
                }
            }
        }
        crate::shadow::pass::early(|| batch.flush());
        PREWARM_US.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
    }

    fn execute_inner(&self, runs: &[&HostList], keep: Option<&[std::sync::Arc<HostList>]>) {
        #[cfg(feature = "diagnostics")]
        let _inside = crate::profile::here();
        if runs.is_empty() {
            return;
        }
        // One submission at a time from preparation to copy-back: snapshots
        // and the ranges shaders write are per host copy, not per submission.
        let mut submitted = self.submitted.lock().unwrap_or_else(|e| e.into_inner());
        // Wait for any previous async submission before mutating shared Shadow
        // buffers in `prepare`. During the gap between submissions, the GPU
        // ran in parallel with the title's CPU command-list recording, and
        // runs on while this submission's buffer copies are refreshed.
        if keep.is_some() && self.line.busy() {
            self.prewarm(runs);
        }
        self.line.drain(Count::WaitExecute);
        let this_submission = STARTED.fetch_add(1, Ordering::AcqRel) + 1;
        let started = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::Submissions, 1);
        let mut lists: Vec<Option<ID3D12CommandList>> = Vec::with_capacity(2 * runs.len());
        // Where each run's lists start in `lists`.
        let mut firsts = Vec::with_capacity(runs.len());
        let mut batch = Batch::default();
        let mut early = Early::default();
        // The textures waiting are the process's host's.
        let process = line().is_some_and(|l| Arc::ptr_eq(l, &self.line));
        if process {
            early.take(&mut batch, runs, false);
        }
        for list in runs {
            let uploads = list.run_deferred(&mut batch);
            firsts.push(lists.len());
            let recording = crate::telemetry::start();
            if let Some(prelude) = list.prelude(&uploads) {
                lists.push(Some(prelude));
                crate::telemetry::stop(Count::PreludeTime, recording);
                crate::telemetry::count(Count::Preludes, 1);
            }
            lists.push(list.list.cast().ok());
        }
        batch.prepare_bindless();
        if process {
            early.take(&mut batch, runs, true);
        }
        // The textures live (in `early`) until the submission is done.
        let first = early.record(self);
        // The refreshes are done and the lists have not run.
        if crate::shadow::pass::on() {
            let keeping = crate::telemetry::start();
            crate::shadow::pass::keep_before(&batch.writes);
            crate::telemetry::stop(Count::Snapshots, keeping);
        }
        // Each range once, however many lists and draws declared it; VRAM
        // card shadows get pre-uploaded / Wave64-hashed in the same GPU wait.
        // A copy is either a card's or not.
        let (card_writes, cpu_back): (Vec<Range>, Vec<Range>) =
            std::mem::take(&mut batch.writes).into_iter().partition(|w| w.0.is_card());
        let recording = crate::telemetry::start();
        let (card_before, card_plan, mut card_after) =
            if self.card.get().and_then(|o| o.as_ref()).is_some() || !card_writes.is_empty() {
                if let Some(engine_lock) = self.card_engine() {
                    let mut engine = engine_lock.lock().unwrap_or_else(|e| e.into_inner());
                    let (before, plan) = engine.record_before(self, &card_writes);
                    let after = if plan.has_writes() {
                        engine.record_after(self, &plan)
                    } else {
                        None
                    };
                    (before, Some(plan), after)
                } else {
                    (None, None, None)
                }
            } else {
                (None, None, None)
            };
        crate::telemetry::stop(Count::Card, recording);
        let prepared = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        let each = timing();
        #[cfg(not(feature = "diagnostics"))]
        let each = false;
        let can_async = self.overlap.load(Ordering::Relaxed)
            && keep.is_some()
            && !each
            && async_submit()
            && card_writes.is_empty();
        // Diagnostics: everything but the GPU's work, to look at the order of
        // the title's submissions.
        #[cfg(feature = "diagnostics")]
        let works = !no_work();
        #[cfg(not(feature = "diagnostics"))]
        let works = true;
        // Not around runs that are waited for one by one.
        let timed = (works && !each)
            .then(|| self.telemetry_clock())
            .flatten()
            .and_then(|clock| Some((clock, clock.pair()?)));
        if crate::telemetry::on() {
            crate::telemetry::add(Count::Submissions, 1);
            if keep.is_some() && async_submit() && !card_writes.is_empty() {
                crate::telemetry::add(Count::HeldByCard, 1);
            }
        }
        // The lists in as few calls as the host may run for long enough (it
        // resets the device when one call runs for about 2 s; the world's
        // frames once ran a second or more over dozens of lists), or each run
        // (prelude and list) in a call of its own. One wait at the end; with
        // `VICTORY_GPU_TIMING` (diagnostics) each run is waited for and timed
        // on its own.
        let joined = one_ecl() && !each;
        let most = match joined {
            true => lists_of_a_call(LIST_US.load(Ordering::Relaxed), ecl_lists()),
            false => 1,
        };
        let names: Vec<usize> = runs.iter().map(|r| std::ptr::from_ref::<HostList>(r) as usize).collect();
        let calls: Vec<std::ops::Range<usize>> = calls(&names, most)
            .into_iter()
            .map(|c| firsts[c.start]..firsts.get(c.end).copied().unwrap_or(lists.len()))
            .collect();
        let mut ahead: Vec<Option<ID3D12CommandList>> = Vec::new();
        ahead.extend(timed.map(|(clock, pair)| Some(clock.begin(pair))));
        ahead.extend(first.map(Some));
        ahead.extend(card_before.map(Some));
        let mut behind: Vec<Option<ID3D12CommandList>> = Vec::new();
        let mut card_after_ran = false;
        if works && !each {
            if let Some(after) = card_after.take() {
                behind.push(Some(after));
                card_after_ran = true;
            }
            behind.extend(timed.map(|(clock, pair)| Some(clock.end(pair))));
        }
        let give = |lists: &[Option<ID3D12CommandList>]| {
            crate::telemetry::count(Count::Ecls, 1);
            crate::telemetry::count(Count::HostLists, lists.len() as u64);
            // SAFETY: closed lists, none of them running: every submission
            // before this one is finished.
            unsafe { self.queue.ExecuteCommandLists(lists) };
        };
        let submitting = crate::telemetry::start();
        let sent = std::time::Instant::now();
        if works && joined {
            let last = calls.len().saturating_sub(1);
            for (i, call) in calls.into_iter().enumerate() {
                let mut all = if i == 0 { std::mem::take(&mut ahead) } else { Vec::new() };
                all.extend_from_slice(&lists[call]);
                if i == last {
                    all.append(&mut behind);
                }
                give(&all);
            }
        } else if works {
            ahead.iter().for_each(|list| give(std::slice::from_ref(list)));
            for (i, call) in calls.into_iter().enumerate() {
                let began = std::time::Instant::now();
                give(&lists[call]);
                if each {
                    self.wait_locked(&mut submitted);
                    #[cfg(feature = "diagnostics")]
                    {
                        // SAFETY: a plain query.
                        let frequency = unsafe { self.queue.GetTimestampFrequency() }.unwrap_or(1);
                        runs[i].add_times(frequency);
                        if began.elapsed() > std::time::Duration::from_millis(50) {
                            let commands = runs[i].notes.lock().unwrap_or_else(|e| e.into_inner()).len();
                            tracing::warn!(ms = began.elapsed().as_millis(), commands, "slow host list");
                            for (ms, op) in runs[i].slowest(frequency, 6) {
                                tracing::warn!("slow list command, {ms:.1} ms: {op}");
                            }
                        }
                    }
                }
                let _ = (i, began);
            }
            behind.iter().for_each(|list| give(std::slice::from_ref(list)));
        }
        if !each {
            crate::telemetry::stop(Count::Submit, submitting);
        }
        if works && can_async {
            if let Some(plan) = card_plan {
                plan.clear_pending();
            }
            SUBMISSION_US[0].fetch_add((prepared - started).as_micros() as u64, Ordering::Relaxed);
            *submitted += 1;
            let mut flight = Flight::new(this_submission, keep.map(<[_]>::to_vec).unwrap_or_default());
            flight.fence = *submitted;
            flight.back = cpu_back;
            flight.writes = take_writes(runs);
            flight.timed = timed.map(|(_, pair)| pair);
            flight.sent = sent;
            flight._used = Box::new((early.copies, batch.plans));
            crate::telemetry::count(Count::Unwaited, 1);
            self.line.send(flight);
            return;
        }
        if works && !each {
            self.wait_locked(&mut submitted);
            ran(runs.len(), sent.elapsed());
        }
        if works && let Some(after) = card_after.take() {
            self.execute_and_wait_locked(after, &mut submitted);
            card_after_ran = true;
        }
        if let Some((clock, pair)) = timed {
            clock.ran(pair);
        }
        drop(early);
        finished(this_submission);
        let gpu = prepared.elapsed();
        SUBMISSION_US[0].fetch_add((prepared - started).as_micros() as u64, Ordering::Relaxed);
        SUBMISSION_US[1].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        gpu_waits(gpu, runs);
        // The host resets a device whose submission runs for about 2 s.
        if gpu > std::time::Duration::from_millis(250) {
            tracing::warn!(
                ms = gpu.as_millis(),
                prepare_ms = (prepared - started).as_millis(),
                lists = runs.len(),
                "slow host submission"
            );
            if tracing::enabled!(tracing::Level::DEBUG) {
                let pipelines: std::collections::BTreeSet<String> = runs
                    .iter()
                    .flat_map(|l| l.pipelines.lock().unwrap_or_else(|e| e.into_inner()).clone())
                    .collect();
                tracing::debug!(?pipelines, "slow submission pipelines");
            }
        }
        // SAFETY: a plain query.
        if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
            removed(&self.device, runs);
        }
        // What shaders wrote goes back before the bottom-of-pipe writes,
        // which tell the title the GPU is done; each range once per
        // submission, however many dispatches declared it (the world's
        // passes declare the same big ranges dozens of times).
        let copying = std::time::Instant::now();
        if let Some(plan) = card_plan {
            if card_after_ran {
                if let Some(engine_lock) = self.card_engine() {
                    let mut engine = engine_lock.lock().unwrap_or_else(|e| e.into_inner());
                    let written = engine.finish_back(self, plan, &mut submitted);
                    COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
                } else {
                    plan.clear_pending();
                }
            } else {
                plan.clear_pending();
            }
        }
        COPY_BACK[0].fetch_add(copying.elapsed().as_micros() as u64, Ordering::Relaxed);
        finish(&self.queue, runs, &cpu_back, &take_writes(runs));
        self.log_messages();
    }

    /// Waits for what the host queue was given so far. The caller holds
    /// `self.submitted`.
    fn wait_locked(&self, submitted: &mut u64) {
        *submitted += 1;
        // SAFETY: the fence and event are ours, used under `submitted`.
        unsafe {
            let waited = self
                .queue
                .Signal(&self.fence, *submitted)
                .and_then(|()| self.fence.SetEventOnCompletion(*submitted, self.event));
            match waited {
                Ok(()) => {
                    WaitForSingleObject(self.event, INFINITE);
                }
                Err(e) => tracing::warn!(error = %e, "host submission not waited for"),
            }
        }
    }

    /// How host GPU timestamps map onto the title's GPU clock
    /// ([`crate::queue::Written::GpuTimestamp`]): the queue's calibration
    /// of its timestamps against the CPU's performance counter, which
    /// `Instant` reads too.
    fn clock(queue: &ID3D12CommandQueue) -> Option<Clock> {
        use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
        let (mut gpu, mut cpu, mut now, mut cpu_frequency) = (0, 0, 0, 0);
        // SAFETY: plain queries into our locals.
        let frequency = unsafe {
            let frequency = queue.GetTimestampFrequency().ok()?;
            queue.GetClockCalibration(&mut gpu, &mut cpu).ok()?;
            QueryPerformanceCounter(&mut now);
            QueryPerformanceFrequency(&mut cpu_frequency);
            frequency
        };
        let title_now = crate::queue::gpu_clock();
        let ticks = i128::from(crate::queue::TIMESTAMP_FREQUENCY);
        // The title's clock when the host GPU's read `gpu`.
        let since = (i128::from(now) - i128::from(cpu)) * ticks / i128::from(cpu_frequency.max(1));
        (frequency > 0).then_some(Clock { gpu, at: i128::from(title_now) - since, frequency, now: title_now })
    }

    /// With `VICTORY_D3D12_DRED`: the faulting address and the allocations
    /// it lies in (by the names we give them) or the title memory copy just
    /// below it; for each list that did not finish, the command it stopped
    /// at and what the draws and dispatches in flight there run (or, in
    /// lists that never started, the pipelines they bind).
    fn removal_data(device: &ID3D12Device, runs: &[&HostList]) -> String {
        let Ok(dred) = device.cast::<ID3D12DeviceRemovedExtendedData1>() else {
            return String::new();
        };
        let mut text = String::new();
        let mut fault_va = None;
        // SAFETY: the outputs are the runtime's, valid while the device
        // lives; nodes are walked as linked, histories have their counts.
        unsafe {
            if let Ok(fault) = dred.GetPageFaultAllocationOutput1()
                && fault.PageFaultVA != 0
            {
                let va = fault.PageFaultVA;
                fault_va = Some(va);
                text.push_str(&format!("; page fault at {va:#x}"));
                for (what, head) in [
                    ("inside", fault.pHeadExistingAllocationNode),
                    ("inside freed", fault.pHeadRecentFreedAllocationNode),
                ] {
                    let mut names = Vec::new();
                    let mut node = head;
                    while let Some(n) = node.as_ref()
                        && names.len() < 8
                    {
                        let name = if n.ObjectNameW.is_null() {
                            "unnamed".to_owned()
                        } else {
                            n.ObjectNameW.to_string().unwrap_or_default()
                        };
                        names.push(format!("{name} (type {})", n.AllocationType.0));
                        node = n.pNext;
                    }
                    if !names.is_empty() {
                        text.push_str(&format!("; {what} {names:?}"));
                    }
                }
                text.push_str(&crate::shadow::near(va));
                text.push_str(&crate::descriptor::host::near_heaps(va));
            }
            // The plain output: the extended one (with marker texts) came
            // back without stopped lists on this host.
            let crumbs = match dred.GetAutoBreadcrumbsOutput() {
                Ok(crumbs) => crumbs,
                Err(e) => {
                    text.push_str(&format!("; no breadcrumbs ({e})"));
                    return text;
                }
            };
            // Every list's progress (done/count), in the runtime's order.
            let mut progress = Vec::new();
            let mut node = crumbs.pHeadAutoBreadcrumbNode;
            while let Some(p) = node.as_ref() {
                let done = p.pLastBreadcrumbValue.as_ref().copied().unwrap_or(0);
                let ours = p.pCommandList.as_ref().map(|l| l.as_raw());
                let index = runs.iter().position(|r| Some(r.list.as_raw()) == ours);
                progress.push(format!("{done}/{} {index:?}", p.BreadcrumbCount));
                node = p.pNext;
            }
            text.push_str(&format!("; breadcrumbs (done/count, list) {progress:?}"));
            let mut plain = crumbs.pHeadAutoBreadcrumbNode;
            while let Some(p) = plain.as_ref() {
                let done = p.pLastBreadcrumbValue.as_ref().copied().unwrap_or(0);
                if done < p.BreadcrumbCount && !p.pCommandHistory.is_null() {
                    let history =
                        std::slice::from_raw_parts(p.pCommandHistory, p.BreadcrumbCount as usize);
                    text.push_str(&format!(
                        "; a list stopped at command {done} of {} (op {})",
                        p.BreadcrumbCount, history[done as usize].0
                    ));
                    let list = p.pCommandList.as_ref().map(|l| l.as_raw());
                    let run = runs.iter().find(|r| Some(r.list.as_raw()) == list);
                    // The k-th marker command is the k-th note.
                    #[cfg(feature = "diagnostics")]
                    let markers: Vec<(u32, String)> = run.map_or_else(Vec::new, |r| {
                        let notes = r.notes.lock().unwrap_or_else(|e| e.into_inner());
                        history
                            .iter()
                            .enumerate()
                            .filter(|(_, op)| **op == D3D12_AUTO_BREADCRUMB_OP_SETMARKER)
                            .zip(notes.iter())
                            .map(|((at, _), (note, range))| {
                                let place = fault_va.zip(*range).map(|(va, r)| fault_in(va, r));
                                (at as u32, format!("{note}{}", place.unwrap_or_default()))
                            })
                            .collect()
                    });
                    #[cfg(not(feature = "diagnostics"))]
                    let markers: Vec<(u32, String)> = Vec::new();
                    #[cfg(not(feature = "diagnostics"))]
                    let _ = fault_va;
                    if done > 0 {
                        text.push_str(&in_flight(history, &markers, done as usize));
                    } else if let Some(r) = run {
                        let pipelines = r.pipelines.lock().unwrap_or_else(|e| e.into_inner());
                        text.push_str(&format!(", its pipelines {pipelines:?}"));
                    }
                }
                plain = p.pNext;
            }
        }
        text
    }

    /// Moves the debug layer's messages into our log; returns the first
    /// error among them.
    pub(crate) fn log_messages(&self) -> Option<String> {
        let Some(queue) = &self.messages else {
            return None;
        };
        let mut first_error = None;
        // SAFETY: each message is read into a buffer of the size the queue
        // reports, then the queue is emptied.
        unsafe {
            for i in 0..queue.GetNumStoredMessages() {
                let mut size = 0;
                if queue.GetMessage(i, None, &mut size).is_err() || size == 0 {
                    continue;
                }
                let mut buffer = vec![0u64; size.div_ceil(8)];
                let message = buffer.as_mut_ptr().cast::<D3D12_MESSAGE>();
                if queue.GetMessage(i, Some(message), &mut size).is_ok() {
                    let m = &*message;
                    let text = std::slice::from_raw_parts(m.pDescription, m.DescriptionByteLength);
                    let text = String::from_utf8_lossy(text);
                    let text = text.trim_end_matches('\0');
                    tracing::warn!(id = m.ID.0, severity = m.Severity.0, "D3D12: {text}");
                    if m.Severity.0 <= D3D12_MESSAGE_SEVERITY_ERROR.0 && first_error.is_none() {
                        first_error = Some(text.to_owned());
                    }
                }
            }
            queue.ClearStoredMessages();
        }
        first_error
    }

    /// A host render target view of `resource`, valid until it is recorded.
    pub(crate) fn rtv(
        &self,
        resource: &ID3D12Resource,
        desc: Option<&D3D12_RENDER_TARGET_VIEW_DESC>,
    ) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let slot = self.next_rtv.fetch_add(1, Ordering::Relaxed) % RTV_RING;
        // SAFETY: a slot of our heap and a live resource.
        unsafe {
            let mut handle = self.rtvs.GetCPUDescriptorHandleForHeapStart();
            handle.ptr += (slot * self.rtv_size) as usize;
            self.device
                .CreateRenderTargetView(resource, desc.map(|d| d as *const _), handle);
            handle
        }
    }

    /// A host depth view of `resource`, valid until it is recorded.
    pub(crate) fn dsv(
        &self,
        resource: &ID3D12Resource,
        desc: Option<&D3D12_DEPTH_STENCIL_VIEW_DESC>,
    ) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let slot = self.next_dsv.fetch_add(1, Ordering::Relaxed) % RTV_RING;
        // SAFETY: a slot of our heap and a live resource.
        unsafe {
            let mut handle = self.dsvs.GetCPUDescriptorHandleForHeapStart();
            handle.ptr += (slot * self.dsv_size) as usize;
            self.device
                .CreateDepthStencilView(resource, desc.map(|d| d as *const _), handle);
            handle
        }
    }
}

/// Whether a wait memory does not meet is met once what is in flight has
/// landed.
fn met_in_flight(wait: &crate::memwait::Wait, line: Option<&Line>) -> bool {
    line.and_then(|line| line.value(wait.address, wait.bytes)).is_some_and(|value| wait.met(value))
}

/// Ends the run: the device is gone, and `runs` ran last.
pub(crate) fn removed(device: &ID3D12Device, runs: &[&HostList]) -> ! {
    fail(format_args!(
        "host GPU device removed after a submission of {} lists{}{}",
        runs.len(),
        Host::removal_data(device, runs),
        last_notes(runs)
    ))
}

/// The bottom-of-pipe writes of `runs` not done yet, which are this
/// submission's to do: the lists' next one does all of theirs again.
fn take_writes(runs: &[&HostList]) -> Vec<Write> {
    let mut writes = Vec::new();
    for list in runs {
        let mut deferred = list.deferred();
        writes.extend_from_slice(&deferred.writes[deferred.early..]);
        deferred.early = 0;
    }
    writes
}

/// After a submission of `runs` ran on `queue`: what its shaders wrote
/// (`back`) goes back into title memory, then its read-backs, then the
/// bottom-of-pipe writes that tell the title the GPU is done.
pub(crate) fn finish(queue: &ID3D12CommandQueue, runs: &[&HostList], back: &[Range], writes: &[Write]) {
    let copying = std::time::Instant::now();
    if crate::shadow::pass::on() {
        let written = crate::shadow::pass::copy_back(back);
        COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
    } else {
        copy_back_each(back);
    }
    for list in runs {
        for rb in &list.deferred().readbacks {
            let written = if rb.dst_pitch == rb.row_bytes && rb.src_pitch == rb.row_bytes {
                let bytes = rb.row_bytes * rb.lines;
                crate::watch::opening(rb.dst, bytes);
                write_mapped(rb.dst as *mut u8, rb.src as *const u8, bytes);
                bytes
            } else {
                if crate::watch::opens_only() && rb.lines > 0 {
                    crate::watch::opening(rb.dst, (rb.lines - 1) * rb.dst_pitch + rb.row_bytes);
                }
                let mut n = 0;
                for line in 0..rb.lines {
                    let to = rb.dst + line * rb.dst_pitch;
                    let from = rb.src + line * rb.src_pitch;
                    if !crate::watch::opens_only() {
                        crate::watch::writing(to, rb.row_bytes);
                    }
                    write_mapped(to as *mut u8, from as *const u8, rb.row_bytes);
                    n += rb.row_bytes;
                }
                n
            };
            COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
        }
    }
    COPY_BACK[0].fetch_add(copying.elapsed().as_micros() as u64, Ordering::Relaxed);
    // Host timestamps the lists resolved, on the title's GPU clock.
    let mut clock = None;
    for &(at, what, bytes) in writes {
        let what = match what {
            crate::queue::Written::HostTimestamp(raw) => {
                match clock.get_or_insert_with(|| Host::clock(queue)) {
                    // SAFETY: the list's mapped read-back memory, which the
                    // submission that ran resolved into.
                    Some(c) => crate::queue::Written::Value(c.convert(unsafe { (raw as *const u64).read_volatile() })),
                    None => crate::queue::Written::GpuTimestamp,
                }
            }
            what => what,
        };
        what.write(at as *mut u8, bytes);
    }
}

/// [`finish`]'s copy-back with a pass for every range (`VYCE_ONE_PASS=0`).
fn copy_back_each(back: &[Range]) {
    let one = |(shadow, address, len): &Range| {
        let written = shadow.copy_back(*address, *len);
        COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
    };
    // Group by Shadow pointer so distinct shadows run in parallel across CPU
    // cores without lock contention on `before`/`pending`.
    let mut groups: Vec<Vec<&Range>> = Vec::new();
    for item in back {
        match groups.iter_mut().find(|g| Arc::ptr_eq(&g[0].0, &item.0)) {
            Some(g) => g.push(item),
            None => groups.push(vec![item]),
        }
    }
    in_parallel(&groups, 8, |chunk| chunk.iter().flatten().for_each(|r| one(r)));
}

/// A host GPU timestamp `gpu` taken when the title's GPU clock read `at`,
/// the host's ticks per second, and the title's clock when taken.
struct Clock {
    gpu: u64,
    at: i128,
    frequency: u64,
    now: u64,
}

impl Clock {
    /// A host GPU timestamp on the title's clock: never past the time it
    /// was read, so bottom-of-pipe timestamps written after stay later.
    fn convert(&self, raw: u64) -> u64 {
        let ticks = i128::from(crate::queue::TIMESTAMP_FREQUENCY);
        let since = (i128::from(raw) - i128::from(self.gpu)) * ticks / i128::from(self.frequency);
        (self.at + since).clamp(0, i128::from(self.now)) as u64
    }
}

impl HostList {
    /// The timestamp heap and its read-back buffer (diagnostics, timing on).
    #[cfg(feature = "diagnostics")]
    fn stamps(&self) -> Option<&(ID3D12QueryHeap, ID3D12Resource)> {
        if !timing() {
            return None;
        }
        self.stamps
            .get_or_init(|| {
                let device = &host()?.device;
                let mut heap: Option<ID3D12QueryHeap> = None;
                let mut back: Option<ID3D12Resource> = None;
                let readback = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
                let desc = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: u64::from(STAMPS) * 8,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    ..Default::default()
                };
                // SAFETY: plain object creation.
                unsafe {
                    device
                        .CreateQueryHeap(
                            &D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: STAMPS, NodeMask: 0 },
                            &mut heap,
                        )
                        .ok()?;
                    device
                        .CreateCommittedResource(
                            &readback,
                            D3D12_HEAP_FLAG_NONE,
                            &desc,
                            D3D12_RESOURCE_STATE_COPY_DEST,
                            None,
                            &mut back,
                        )
                        .ok()?;
                }
                Some((heap?, back?))
            })
            .as_ref()
    }

    /// The noted commands that took longest on the GPU, longest first, as
    /// (milliseconds, text) (diagnostics, timing on, after the list ran).
    #[cfg(feature = "diagnostics")]
    fn slowest(&self, frequency: u64, n: usize) -> Vec<(f64, String)> {
        let notes = self.notes.lock().unwrap_or_else(|e| e.into_inner());
        let (Some((_, back)), count) = (self.stamps(), notes.len().min(STAMPS as usize - 1)) else {
            return Vec::new();
        };
        let mut data = std::ptr::null_mut();
        let range = D3D12_RANGE { Begin: 0, End: (count + 1) * 8 };
        // SAFETY: a read-back buffer the finished list resolved into.
        let stamps: Vec<u64> = unsafe {
            if back.Map(0, Some(&range), Some(&mut data)).is_err() {
                return Vec::new();
            }
            let s = std::slice::from_raw_parts(data.cast::<u64>(), count + 1).to_vec();
            back.Unmap(0, None);
            s
        };
        let mut times: Vec<(f64, String)> = (0..count)
            .map(|i| {
                let ticks = stamps[i + 1].saturating_sub(stamps[i]);
                (ticks as f64 * 1e3 / frequency.max(1) as f64, notes[i].0.chars().take(300).collect())
            })
            .collect();
        times.sort_by(|a, b| b.0.total_cmp(&a.0));
        times.truncate(n);
        times
    }

    /// Adds the GPU time of each noted command to the totals by kind
    /// (diagnostics, timing on, after the list ran).
    #[cfg(feature = "diagnostics")]
    fn add_times(&self, frequency: u64) {
        let notes = self.notes.lock().unwrap_or_else(|e| e.into_inner());
        let (Some((_, back)), count) = (self.stamps(), notes.len().min(STAMPS as usize - 1)) else {
            return;
        };
        if count == 0 {
            return;
        }
        let mut data = std::ptr::null_mut();
        let range = D3D12_RANGE { Begin: 0, End: (count + 1) * 8 };
        // SAFETY: a read-back buffer the finished list resolved into.
        let stamps: Vec<u64> = unsafe {
            if back.Map(0, Some(&range), Some(&mut data)).is_err() {
                return;
            }
            let s = std::slice::from_raw_parts(data.cast::<u64>(), count + 1).to_vec();
            back.Unmap(0, None);
            s
        };
        let mut times = GPU_TIMES.lock().unwrap_or_else(|e| e.into_inner());
        let times = times.get_or_insert_with(Default::default);
        for i in 0..count {
            let ms = stamps[i + 1].saturating_sub(stamps[i]) as f64 * 1e3 / frequency.max(1) as f64;
            // The kind without its counts: "dispatch [compute ...]".
            let text = &notes[i].0;
            let kind = text.split(' ').next().unwrap_or("");
            let rest = text.find('[').map_or("", |at| &text[at..]);
            let key: String = format!("{kind} {rest}").chars().take(150).collect();
            let k = times.entry(format!("all: {kind}")).or_insert((0, 0.0));
            k.0 += 1;
            k.1 += ms;
            let e = times.entry(key).or_insert((0, 0.0));
            e.0 += 1;
            e.1 += ms;
        }
    }

    pub(crate) fn close(&self) {
        // The host closes no list with queries running; their title ends,
        // in a later list, are dropped (`end_query`).
        for (heap, kind, index, _) in self.queries.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
            // SAFETY: an open list and a query begun in it.
            unsafe { self.list.EndQuery(&heap, kind, index) };
        }
        #[cfg(feature = "diagnostics")]
        if let Some((heap, back)) = self.stamps() {
            let count = (self.notes.lock().unwrap_or_else(|e| e.into_inner()).len() as u32).min(STAMPS - 1);
            // SAFETY: an open list, our heap and read-back buffer.
            unsafe {
                self.list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, count);
                self.list.ResolveQueryData(heap, D3D12_QUERY_TYPE_TIMESTAMP, 0, count + 1, back, 0);
            }
        }
        if one_ecl() && self.worked() {
            // What follows in the same call sees all this list wrote, through
            // whatever it reads it: shader writes end, and every cache is
            // emptied (any memory may be used as anything else from here on).
            let ends = |kind| D3D12_RESOURCE_BARRIER { Type: kind, ..Default::default() };
            // SAFETY: an open list; the barriers name no resource.
            unsafe {
                self.list.ResourceBarrier(&[
                    ends(D3D12_RESOURCE_BARRIER_TYPE_UAV),
                    ends(D3D12_RESOURCE_BARRIER_TYPE_ALIASING),
                ])
            };
        }
        // SAFETY: our list.
        if let Err(e) = unsafe { self.list.Close() } {
            fail(format_args!(
                "a host command list did not close ({e}): a command recorded into it was invalid"
            ));
        }
    }

    /// The list was sent and not waited for, with `value` signalled behind it
    /// on the host's fence.
    pub(crate) fn sent(&self, value: u64) {
        self.last_fence.store(value, Ordering::Release);
    }

    pub(crate) fn keep_until_run(&self, what: Box<dyn std::any::Any + Send>) {
        self.kept.lock().unwrap_or_else(|e| e.into_inner()).push(what);
    }

    pub(crate) fn release_kept(&self) {
        let kept = std::mem::take(&mut *self.kept.lock().unwrap_or_else(|e| e.into_inner()));
        drop(kept);
    }

    /// Only after the list's last submission finished.
    pub(crate) fn reset(&self) {
        self.line.wait(self.last_fence.load(Ordering::Acquire), Count::WaitList);
        self.release_kept();
        *self.deferred() = Deferred::default();
        *self.work.lock().unwrap_or_else(|e| e.into_inner()) = 0;
        self.writes.store(0, Ordering::Relaxed);
        {
            let mut chunks = self.clamp_chunks.lock().unwrap_or_else(|e| e.into_inner());
            chunks.used = 0;
            chunks.at = 0;
            chunks.barrier = false;
            chunks.bound = false;
        }
        self.states.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.pipelines.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.queries.lock().unwrap_or_else(|e| e.into_inner()).clear();
        #[cfg(feature = "diagnostics")]
        self.notes.lock().unwrap_or_else(|e| e.into_inner()).clear();
        {
            let mut scratch = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
            scratch.give_back();
        }
        // SAFETY: the list is not executing.
        let reset = unsafe {
            self.allocator
                .Reset()
                .and_then(|()| self.list.Reset(&self.allocator, None))
        };
        if let Err(e) = reset {
            fail(format_args!("a host command list did not reset ({e})"));
        }
    }

    /// Moves a title texture's host copy to `wanted` (a host state) in this
    /// list: the host texture and the state it leaves, if that is a change.
    /// The list's first use of a texture records nothing: its prelude brings
    /// the texture there at submission.
    pub(crate) fn transition(
        &self,
        resource: crate::object::This,
        wanted: u32,
    ) -> Option<(&'static ID3D12Resource, u32)> {
        let texture = crate::resource::host_resource(resource)?;
        let copy = crate::resource::host_copy(resource)?;
        let wanted = legal_state(texture, wanted);
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        match states.entry(std::sync::Arc::as_ptr(&copy) as usize) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert((wanted, wanted, copy));
                None
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                let before = std::mem::replace(&mut o.get_mut().1, wanted);
                (before != wanted).then_some((texture, before))
            }
        }
    }

    /// Records that this list starts and ends with `copy` in `wanted` state
    /// (test helper matching `transition`'s first-use tracking).
    #[cfg(test)]
    pub(crate) fn expect_texture_state(
        &self,
        copy: &std::sync::Arc<crate::resource::HostTexture>,
        wanted: u32,
    ) {
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        states
            .entry(std::sync::Arc::as_ptr(copy) as usize)
            .or_insert_with(|| (wanted, wanted, copy.clone()));
    }

    #[cfg(test)]
    pub(crate) fn has_deferred_work(&self) -> bool {
        !self.deferred().copies.is_empty()
    }

    /// At submission: the list that copies any newly dirtied textures from
    /// their staging buffers into their host textures and moves each texture
    /// this list uses from its real state to the one the list starts with;
    /// the textures' real states become those the list ends with.
    fn prelude(
        &self,
        uploads: &[(
            std::sync::Arc<crate::texture::Staging>,
            std::sync::Arc<crate::resource::HostTexture>,
        )],
    ) -> Option<ID3D12CommandList> {
        const COPY_DEST: u32 = D3D12_RESOURCE_STATE_COPY_DEST.0 as u32;
        let mut pre_barriers = Vec::new();
        for (_, copy) in uploads {
            let real = copy.state.swap(COPY_DEST, Ordering::Relaxed);
            if real != COPY_DEST {
                pre_barriers.push(transition(
                    &copy.texture,
                    D3D12_RESOURCE_STATES(real as i32),
                    D3D12_RESOURCE_STATE_COPY_DEST,
                ));
            }
        }
        let states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        let post_barriers: Vec<D3D12_RESOURCE_BARRIER> = states
            .values()
            .filter_map(|(first, last, copy)| {
                let real = copy.state.swap(*last, Ordering::Relaxed);
                (real != *first).then(|| {
                    transition(
                        &copy.texture,
                        D3D12_RESOURCE_STATES(real as i32),
                        D3D12_RESOURCE_STATES(*first as i32),
                    )
                })
            })
            .collect();
        if uploads.is_empty() && post_barriers.is_empty() {
            return None;
        }
        let (allocator, list) = &self.prelude;
        // SAFETY: the prelude is not executing (every submission before this
        // one is finished); the textures and staging buffers are live.
        let recorded = unsafe {
            allocator.Reset().and_then(|()| {
                list.Reset(allocator, None)?;
                if !pre_barriers.is_empty() {
                    list.ResourceBarrier(&pre_barriers);
                }
                for (staging, copy) in uploads {
                    staging.record_copies(list, &copy.texture);
                }
                if !post_barriers.is_empty() {
                    list.ResourceBarrier(&post_barriers);
                }
                list.Close()
            })
        };
        if let Err(e) = recorded {
            fail(format_args!("a host state prelude did not record ({e})"));
        }
        list.cast().ok()
    }

    /// Notes what the command about to be recorded does, as a marker
    /// command whose text the list keeps (diagnostics, with device-removed
    /// data on): the report pairs the breadcrumbs' marker commands with the
    /// texts in order and names the commands a fault stopped among.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn note_op(&self, what: impl FnOnce() -> String) {
        self.note(what, None);
    }

    /// As [`HostList::note_op`], for a copy that reads the host GPU range
    /// `(address, bytes)`: the report places the faulting address in it.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn note_read(&self, what: impl FnOnce() -> String, range: (u64, u64)) {
        self.note(what, Some(range));
    }

    #[cfg(feature = "diagnostics")]
    fn note(&self, what: impl FnOnce() -> String, range: Option<(u64, u64)>) {
        if dred_on() {
            if step() {
                let barrier = D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                    ..Default::default()
                };
                // SAFETY: an open list; the barrier names no resource.
                unsafe { self.list.ResourceBarrier(&[barrier]) };
            }
            let at = self.notes.lock().unwrap_or_else(|e| e.into_inner()).len() as u32;
            if let Some((heap, _)) = self.stamps() && at < STAMPS - 1 {
                // SAFETY: an open list and our query heap.
                unsafe { self.list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, at) };
            }
            let what = what();
            let text: Vec<u16> = what.encode_utf16().chain([0]).collect();
            const UNICODE: u32 = 0;
            // SAFETY: an open list; the marker is copied when recorded.
            unsafe {
                self.list.SetMarker(UNICODE, Some(text.as_ptr().cast()), 2 * text.len() as u32)
            };
            self.notes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((what, range));
        }
    }

    /// Begins query `index` of `heap` (occlusion or statistics, never a
    /// timestamp) unless it is running in the list already. Whether it
    /// began.
    pub(crate) fn begin_query(&self, heap: &ID3D12QueryHeap, kind: u32, index: u32) -> bool {
        let mut queries = self.queries.lock().unwrap_or_else(|e| e.into_inner());
        if kind == crate::query::TIMESTAMP || queries.iter().any(|(h, _, i, _)| h == heap && *i == index) {
            return false;
        }
        let kind = D3D12_QUERY_TYPE(kind as i32);
        // SAFETY: an open list and a live heap, kept until the list is reset.
        unsafe { self.list.BeginQuery(heap, kind, index) };
        queries.push((heap.clone(), kind, index, false));
        drop(queries);
        self.keep(heap);
        true
    }

    /// Ends query `index` of `heap`: a timestamp is taken; another query
    /// ends only if it began in this list, as the type it began as. The
    /// type it ended as, if it did and its result is the host's to give: an
    /// occlusion query with a draw inside that the host did not run counts
    /// too few samples, and a title would cull by it what the draw shows.
    pub(crate) fn end_query(&self, heap: &ID3D12QueryHeap, kind: u32, index: u32) -> Option<u32> {
        let (kind, missed) = if kind == crate::query::TIMESTAMP {
            (D3D12_QUERY_TYPE(kind as i32), false)
        } else {
            let mut queries = self.queries.lock().unwrap_or_else(|e| e.into_inner());
            let at = queries.iter().position(|(h, _, i, _)| h == heap && *i == index)?;
            let (_, kind, _, missed) = queries.swap_remove(at);
            (kind, missed)
        };
        // SAFETY: an open list and a live heap, kept until the list is reset.
        unsafe { self.list.EndQuery(heap, kind, index) };
        self.keep(heap);
        let occlusion = matches!(kind.0 as u32, crate::query::OCCLUSION | crate::query::BINARY_OCCLUSION);
        (!(missed && occlusion)).then_some(kind.0 as u32)
    }

    /// A draw the title recorded was not run on the host (no pipeline yet,
    /// a shader that does not translate, ...): the queries running around
    /// it did not see it.
    pub(crate) fn missed_draw(&self) {
        for q in self.queries.lock().unwrap_or_else(|e| e.into_inner()).iter_mut() {
            q.3 = true;
        }
    }

    /// Notes a pipeline the list binds and keeps it and its root signature
    /// until the list is reset, the first time only: the list keeps the
    /// first, so its address names the same pipeline from then on.
    pub(crate) fn bind_pipeline(&self, p: &crate::pipeline::host::HostPipeline) {
        let mut deferred = self.deferred();
        if !deferred.bound.insert(p.pso.as_raw() as usize) {
            return;
        }
        deferred.kept.push(p.pso.clone().into());
        deferred.kept.push(p.root.clone().into());
        drop(deferred);
        self.pipelines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(p.name.clone());
    }

    fn deferred(&self) -> std::sync::MutexGuard<'_, Deferred> {
        self.deferred.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Copies `len` bytes from `src` to `dst` when the list is submitted.
    pub(crate) fn defer_copy(&self, dst: *mut u8, src: *const u8, len: usize) {
        let mut deferred = self.deferred();
        deferred
            .copies
            .push(Step::Copy(dst as usize, src as usize, len));
        deferred.refreshed.clear();
        deferred.gpu_written.clear();
        deferred.filled.clear();
    }

    /// Copies `len` bytes of title memory at `src` into the list's upload
    /// memory at `dst` when the list is submitted.
    pub(crate) fn defer_fill(&self, dst: *mut u8, src: *const u8, len: usize) {
        self.deferred()
            .copies
            .push(Step::Fill(dst as usize, src as usize, len));
    }

    /// The host GPU address of `len` bytes of upload memory holding the title
    /// memory at `src` as of the list's submission: one copy for every ask
    /// until a title copy intervenes.
    pub(crate) fn fill_scratch(&self, src: *const u8, len: u64) -> Option<u64> {
        let key = (src as usize, len as usize);
        let known = self.deferred().filled.get(&key).copied();
        if known.is_some() {
            return known;
        }
        let (gpu, to) = self.scratch(len)?;
        self.defer_fill(to, src, len as usize);
        self.deferred().filled.insert(key, gpu);
        Some(gpu)
    }

    /// Brings `[address, address + len)` of a buffer's host copy up to date
    /// when the list is submitted (once per range until a title copy
    /// intervenes), keeping the copy alive until the list is reset.
    pub(crate) fn defer_refresh(
        &self,
        shadow: &std::sync::Arc<crate::shadow::Shadow>,
        address: usize,
        len: usize,
    ) {
        let mut deferred = self.deferred();
        let key = (std::sync::Arc::as_ptr(shadow) as usize, address, len);
        if deferred.refreshed.insert(key) {
            deferred
                .copies
                .push(Step::Refresh(shadow.clone(), address, len));
        }
    }

    /// Shaders of the list write `[address, address + len)` of a buffer's
    /// host copy: copied back into title memory once the list has run.
    pub(crate) fn defer_gpu_write(
        &self,
        shadow: &std::sync::Arc<crate::shadow::Shadow>,
        address: usize,
        len: usize,
    ) {
        let mut deferred = self.deferred();
        let key = (std::sync::Arc::as_ptr(shadow) as usize, address, len);
        if deferred.gpu_written.insert(key) {
            deferred
                .copies
                .push(Step::GpuWrite(shadow.clone(), address, len));
        }
    }

    /// A wait for a memory value.
    pub(crate) fn note_wait(&self, wait: crate::memwait::Wait) {
        let mut deferred = self.deferred();
        let before = deferred.writes.len();
        deferred.waits.push((before, wait));
        let work = *self.work.lock().unwrap_or_else(|e| e.into_inner());
        deferred.wait_marks.push(work);
    }

    /// Stores `what` (`bytes` of it) at `dst` once the list has run.
    ///
    /// Work is draws, dispatches, clears and copies: what has to run first.
    pub(crate) fn defer_write(&self, dst: *mut u8, what: crate::queue::Written, bytes: usize) {
        let work = *self.work.lock().unwrap_or_else(|e| e.into_inner());
        let mut deferred = self.deferred();
        deferred.writes.push((dst as usize, what, bytes));
        deferred.marks.push(work);
    }

    /// The list's memory waits and writes in order, with how many draws and
    /// dispatches precede each (for the log).
    pub(crate) fn script(&self) -> String {
        let deferred = self.deferred();
        let mut events: Vec<(usize, u8, String)> = Vec::new();
        for (i, (at, what, _)) in deferred.writes.iter().enumerate() {
            events.push((i, 1, format!("W[{at:x}]={what:x?}@{}", deferred.marks.get(i).copied().unwrap_or(0))));
        }
        for (before, w) in &deferred.waits {
            let mark = deferred.wait_marks.get(events.len().saturating_sub(deferred.writes.len())).copied().unwrap_or(0);
            events.push((*before, 0, format!("R[{:x}]>={:x}@{mark}", w.address, w.reference)));
        }
        events.sort_by_key(|e| (e.0, e.1));
        let work = *self.work.lock().unwrap_or_else(|e| e.into_inner());
        let texts: Vec<_> = events.into_iter().map(|e| e.2).collect();
        format!("{} ({work} in all)", texts.join(" "))
    }

    /// Whether the list has work: draws, dispatches, clears or copies.
    pub(crate) fn worked(&self) -> bool {
        *self.work.lock().unwrap_or_else(|e| e.into_inner()) > 0
    }

    /// How many queries begun in the list still run.
    pub(crate) fn open_queries(&self) -> usize {
        self.queries.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Counts a dispatch or a copy: what may write indirect arguments.
    pub(crate) fn count_write(&self) {
        self.writes.fetch_add(1, Ordering::Relaxed);
    }

    /// Dispatches and copies since the reset.
    pub(crate) fn writes(&self) -> u64 {
        self.writes.load(Ordering::Relaxed)
    }

    /// Room for `bytes` of clamped indirect arguments in this list's
    /// buffers, writable by shaders from here on in the list: the buffer and
    /// the offset. None for more than a buffer holds.
    pub(crate) fn clamp_room(&self, bytes: u64) -> Option<(ID3D12Resource, u64)> {
        const CHUNK: u64 = 256 << 10;
        let bytes = bytes.next_multiple_of(16);
        if bytes > CHUNK {
            return None;
        }
        let mut chunks = self.clamp_chunks.lock().unwrap_or_else(|e| e.into_inner());
        if chunks.used > 0 && chunks.at + bytes <= CHUNK {
            let at = chunks.at;
            chunks.at += bytes;
            return Some((chunks.all[chunks.used - 1].0.clone(), at));
        }
        // The next buffer, writable.
        if chunks.used == chunks.all.len() {
            let buffer = host()?.uav_buffer(CHUNK)?;
            name(&buffer, || "hoisted indirect arguments".to_string());
            chunks.all.push((buffer, D3D12_RESOURCE_STATE_COMMON));
        }
        let i = chunks.used;
        chunks.used += 1;
        chunks.at = bytes;
        let mut barriers = Vec::new();
        if !chunks.barrier {
            // What earlier lists' shaders wrote first.
            chunks.barrier = true;
            barriers.push(D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                ..Default::default()
            });
        }
        let (buffer, state) = &mut chunks.all[i];
        if *state != D3D12_RESOURCE_STATE_UNORDERED_ACCESS {
            barriers.push(transition(buffer, *state, D3D12_RESOURCE_STATE_UNORDERED_ACCESS));
            *state = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
        }
        let buffer = buffer.clone();
        if !barriers.is_empty() {
            // SAFETY: an open list and live buffers.
            unsafe { self.list.ResourceBarrier(&barriers) };
        }
        Some((buffer, 0))
    }

    /// Whether the list's clamps still have to bind their root signature,
    /// pipeline and budget: the first after the reset does, and the rest
    /// find them bound (nothing else records compute state in the list).
    pub(crate) fn clamp_unbound(&self) -> bool {
        let mut chunks = self.clamp_chunks.lock().unwrap_or_else(|e| e.into_inner());
        !std::mem::replace(&mut chunks.bound, true)
    }

    /// The clamped arguments written so far become readable as indirect
    /// arguments by the lists after this one.
    pub(crate) fn clamp_end(&self) {
        let mut chunks = self.clamp_chunks.lock().unwrap_or_else(|e| e.into_inner());
        let used = chunks.used;
        let barriers: Vec<_> = chunks.all[..used]
            .iter_mut()
            .filter(|(_, state)| *state == D3D12_RESOURCE_STATE_UNORDERED_ACCESS)
            .map(|(buffer, state)| {
                *state = D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT;
                transition(
                    buffer,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
                )
            })
            .collect();
        if !barriers.is_empty() {
            // SAFETY: an open list and live buffers.
            unsafe { self.list.ResourceBarrier(&barriers) };
        }
    }

    /// Counts a draw, dispatch, clear or copy.
    pub(crate) fn count_work(&self) {
        *self.work.lock().unwrap_or_else(|e| e.into_inner()) += 1;
    }

    /// Does the list's writes that come before its first wait not met, if no
    /// work of the list precedes them; whether there were any. With `line`
    /// they go behind what is in flight, which may be the queue's own earlier
    /// work, and what is in flight may meet waits.
    pub(crate) fn write_early(&self, line: Option<&Line>) -> bool {
        let mut deferred = self.deferred();
        let first = deferred
            .waits
            .iter()
            .find(|(_, wait)| !wait.met(wait.value()) && !met_in_flight(wait, line));
        let Some(&(before, _)) = first else {
            return false;
        };
        let done = deferred.early;
        // Those before any work (marks only grow).
        let before = before.min(deferred.marks.iter().take_while(|&&m| m == 0).count());
        if before <= done {
            return false;
        }
        for &(at, what, bytes) in &deferred.writes[done..before] {
            let write = crate::flight::After::Write(at, what, bytes);
            match line {
                Some(line) => line.after(write).unwrap_or_else(crate::flight::After::run),
                None => write.run(),
            }
        }
        deferred.early = before;
        true
    }

    /// Whether the list waits for nothing any more: each wait is met by
    /// memory, by a write of the list's own before it, or (with `line`) by
    /// what is in flight, ahead of it in the host queue.
    pub(crate) fn waits_met(&self, line: Option<&Line>) -> bool {
        let unmet = self.unmet();
        let met = unmet.iter().all(|wait| met_in_flight(wait, line));
        if met {
            crate::telemetry::count(Count::LateWaits, unmet.len() as u64);
        }
        met
    }

    /// The waits of the list that are not met yet, apart from those its own
    /// earlier writes meet.
    pub(crate) fn unmet(&self) -> Vec<crate::memwait::Wait> {
        let deferred = self.deferred();
        deferred
            .waits
            .iter()
            .filter(|(before, wait)| {
                let own = deferred.writes[..*before]
                    .iter()
                    .rev()
                    .find(|w| w.0 == wait.address);
                match own {
                    Some((_, crate::queue::Written::Value(v), _)) => !wait.met(*v),
                    _ => !wait.met(wait.value()),
                }
            })
            .map(|w| w.1)
            .collect()
    }

    /// Copies a GPU texture readback from `src` into title memory at `dst`
    /// once the list has finished running on the host GPU.
    pub(crate) fn defer_readback(
        &self,
        dst: *mut u8,
        src: *const u8,
        dst_pitch: usize,
        src_pitch: usize,
        lines: usize,
        row_bytes: usize,
    ) {
        self.deferred().readbacks.push(Readback {
            dst: dst as usize,
            src: src as usize,
            dst_pitch,
            src_pitch,
            lines,
            row_bytes,
        });
    }

    /// `size` bytes of mapped upload memory, 512-byte aligned for a placed
    /// footprint, alive until the list is reset: the buffer, the offset in
    /// it and the CPU pointer.
    pub(crate) fn upload(&self, size: u64) -> Option<(ID3D12Resource, u64, *mut u8)> {
        self.scratch_in(size, 512, |buffer, at, _, data| (buffer.clone(), at, data))
    }

    /// A mapped readback buffer of `size` bytes, alive until the list is
    /// reset.
    pub(crate) fn readback(&self, host: &Host, size: u64) -> Option<(ID3D12Resource, *const u8)> {
        let (buffer, data) = host.readback_buffer(size)?;
        name(&buffer, || format!("list readback {size:#x}"));
        self.deferred().uploads.push(buffer.clone());
        Some((buffer, data))
    }

    /// `len` bytes of upload memory (256-byte aligned) until the list is
    /// reset: the host GPU address and the CPU pointer.
    pub(crate) fn scratch(&self, len: u64) -> Option<(u64, *mut u8)> {
        self.scratch_in(len, 256, |_, _, gpu, data| (gpu, data))
    }

    /// `len` bytes of upload memory at a multiple of `align` until the list
    /// is reset, handed to `f` with the buffer, the offset in it, the host GPU
    /// address and the CPU pointer.
    fn scratch_in<R>(
        &self,
        len: u64,
        align: u64,
        f: impl FnOnce(&ID3D12Resource, u64, u64, *mut u8) -> R,
    ) -> Option<R> {
        let host = host()?;
        let mut s = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
        let len = len.max(1).next_multiple_of(256);
        if len > SCRATCH_CHUNK {
            let (buffer, data) = host.upload_buffer(len)?;
            name(&buffer, || format!("list scratch {len:#x}"));
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            let made = f(&buffer, 0, gpu, data);
            self.deferred().uploads.push(buffer);
            return Some(made);
        }
        if s.used.next_multiple_of(align) + len > SCRATCH_CHUNK && s.current < s.chunks.len() {
            (s.current, s.used) = (s.current + 1, 0);
        }
        if s.current == s.chunks.len() {
            let free = SCRATCH_FREE.lock().unwrap_or_else(|e| e.into_inner()).pop();
            let chunk = match free {
                Some(chunk) => chunk,
                None => {
                    let (buffer, data) = host.upload_buffer(SCRATCH_CHUNK)?;
                    name(&buffer, || "list scratch".into());
                    // SAFETY: a live buffer.
                    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
                    (buffer, data as usize, gpu)
                }
            };
            s.chunks.push(chunk);
            s.used = 0;
        }
        let (buffer, data, gpu) = &s.chunks[s.current];
        let at = s.used.next_multiple_of(align);
        let made = f(buffer, at, gpu + at, (data + at as usize) as *mut u8);
        s.used = at + len;
        Some(made)
    }

    /// The list's indirect-work budget, zeroed each time the list executes.
    pub(crate) fn budget(&self) -> Option<&ID3D12Resource> {
        let host = host()?;
        let zero = host
            .indirect_zero
            .get_or_init(|| {
                let (buffer, data) = host.upload_buffer(4)?;
                // SAFETY: this newly mapped upload has room for one dword.
                unsafe { data.cast::<u32>().write(0) };
                Some(buffer)
            })
            .as_ref();
        let budget = self
            .budget
            .get_or_init(|| {
                if zero.is_some() && let Some(buffer) = host.uav_buffer(256) {
                    return Some(IndirectBudget { buffer, mapped: None });
                }
                let (buffer, mapped) = host.shared_buffer(256)?;
                Some(IndirectBudget { buffer, mapped: Some(mapped as usize) })
            })
            .as_ref()?;
        let mut deferred = self.deferred();
        if !deferred.budget_zeroed {
            if let Some(at) = budget.mapped {
                // SAFETY: the list's own mapped word, written before it runs.
                deferred.copies.push(Step::Host(Box::new(move || unsafe {
                    (at as *mut u32).write_volatile(0);
                })));
            } else {
                crate::indirect::reset_budget(&self.list, &budget.buffer, zero?);
            }
            deferred.budget_zeroed = true;
        }
        Some(&budget.buffer)
    }

    /// A device-local unordered-access buffer of at least `size` bytes for
    /// clamped indirect arguments, reused across executions and resets of the
    /// list. Smaller buffers replaced mid-list stay alive until
    /// [`HostList::reset`].
    pub(crate) fn indirect_scratch(&self, size: u64) -> Option<ID3D12Resource> {
        let needed = size.max(64 << 10);
        let mut slot = self.indirect_args.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((buffer, cap)) = &*slot
            && *cap >= needed
        {
            return Some(buffer.clone());
        }
        let prev = slot.as_ref().map_or(0, |(_, cap)| *cap);
        let alloc = prev
            .saturating_mul(2)
            .max(needed)
            .next_multiple_of(64 << 10);
        let buffer = host()?.uav_buffer(alloc)?;
        name(&buffer, || format!("indirect arguments {alloc:#x}"));
        let old = slot.replace((buffer.clone(), alloc));
        drop(slot);
        if let Some((old_buf, _)) = old {
            self.keep(&old_buf);
        }
        Some(buffer)
    }

    /// Keeps `object` alive until the list is reset.
    pub(crate) fn keep(&self, object: &impl Interface) {
        if let Ok(object) = object.cast::<windows::core::IUnknown>() {
            self.deferred().kept.push(object);
        }
    }

    /// Whether this is the first time since the reset that the list asks
    /// about `key` (a texture to bring up to date).
    pub(crate) fn first_use(&self, key: usize) -> bool {
        self.deferred().textures.insert(key)
    }

    /// Runs `work` when the list is submitted, in order with its copies.
    /// `work` may write title memory (refreshes after it copy again).
    pub(crate) fn defer(&self, work: impl Fn() + Send + 'static) {
        let mut deferred = self.deferred();
        deferred.copies.push(Step::Run(Box::new(work)));
        deferred.refreshed.clear();
        deferred.gpu_written.clear();
        deferred.filled.clear();
    }

    /// Queues submission-time materialization and refresh of the bindless
    /// slots in `heap`, once per list; `writes`: the draw's shaders may write
    /// memory, so the slots' unordered access ranges go back to title memory.
    pub(crate) fn defer_bindless(
        &self,
        heap: &std::sync::Arc<crate::descriptor::host::HostHeap>,
        writes: bool,
    ) {
        let mut deferred = self.deferred();
        match deferred
            .bindless
            .iter_mut()
            .find(|(h, _)| std::sync::Arc::ptr_eq(h, heap))
        {
            Some((_, w)) => *w |= writes,
            None => deferred.bindless.push((heap.clone(), writes)),
        }
    }

    /// Whether each heap queued for bindless preparation may be written.
    #[cfg(test)]
    pub(crate) fn bindless_queued(&self) -> Vec<bool> {
        self.deferred().bindless.iter().map(|&(_, w)| w).collect()
    }

    /// As [`HostList::defer`], for work that writes only host memory.
    pub(crate) fn defer_host(&self, work: impl Fn() + Send + 'static) {
        self.deferred().copies.push(Step::Host(Box::new(work)));
    }

    /// Brings a texture's staging buffer up to date when the list is
    /// submitted (and copies it to `texture` on the prelude only if dirty),
    /// keeping both alive until the list is reset.
    pub(crate) fn defer_texture(
        &self,
        staging: std::sync::Arc<crate::texture::Staging>,
        texture: std::sync::Arc<crate::resource::HostTexture>,
    ) {
        self.deferred()
            .copies
            .push(Step::Texture(staging, texture));
    }

    /// Runs the list's submission-time steps; refreshes go into `batch`
    /// (the title's big buffers, a 174 MB one among them, are read by most
    /// lists of a submission, through many ranges). Returns the textures
    /// whose staging buffers changed and must be copied onto the prelude.
    fn run_deferred(
        &self,
        batch: &mut Batch,
    ) -> Vec<(
        std::sync::Arc<crate::texture::Staging>,
        std::sync::Arc<crate::resource::HostTexture>,
    )> {
        #[cfg(feature = "diagnostics")]
        use crate::copystat::{C, add};
        let mut uploads = Vec::new();
        let deferred = self.deferred();
        for (heap, writes) in &deferred.bindless {
            batch.bindless(heap, *writes);
        }
        // Title copies, then fills: how many, bytes, microseconds.
        let (mut copies, mut fills) = ([0u64; 3], [0u64; 3]);
        let mut declared = 0u64;
        let add_to = |sums: &mut [u64; 3], len: usize, since: std::time::Instant| {
            sums[0] += 1;
            sums[1] += len as u64;
            sums[2] += since.elapsed().as_micros() as u64;
        };
        #[cfg(feature = "diagnostics")]
        let mut copy_count = 0u64;
        #[cfg(feature = "diagnostics")]
        let mut refresh_count = 0u64;
        #[cfg(feature = "diagnostics")]
        let mut refresh_bytes = 0u64;
        for step in &deferred.copies {
            match step {
                &Step::Copy(dst, src, len) => {
                    #[cfg(feature = "diagnostics")]
                    {
                        copy_count += 1;
                    }
                    let started = std::time::Instant::now();
                    batch.title_written();
                    copy_mapped(dst as *mut u8, src as *const u8, len, true);
                    add_to(&mut copies, len, started);
                }
                &Step::Fill(dst, src, len) => {
                    #[cfg(feature = "diagnostics")]
                    {
                        copy_count += 1;
                    }
                    let started = std::time::Instant::now();
                    copy_mapped(dst as *mut u8, src as *const u8, len, false);
                    add_to(&mut fills, len, started);
                }
                Step::Refresh(shadow, address, len) => {
                    #[cfg(feature = "diagnostics")]
                    {
                        refresh_count += 1;
                        refresh_bytes += *len as u64;
                    }
                    batch.add(shadow, *address, *len);
                }
                Step::Run(work) | Step::Host(work) => {
                    #[cfg(feature = "diagnostics")]
                    let started = std::time::Instant::now();
                    if matches!(step, Step::Run(_)) {
                        batch.title_written();
                    }
                    let working = crate::telemetry::start();
                    work();
                    crate::telemetry::stop(Count::Steps, working);
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Runs, 1);
                        add(C::RunUs, started.elapsed().as_micros() as u64);
                    }
                }
                Step::GpuWrite(shadow, address, len) => {
                    declared += 1;
                    let new = batch.declare(shadow, *address, *len);
                    // The shaders start from the title's data.
                    if crate::shadow::pass::on() && shadow.is_sparse() {
                        // Its bytes are kept with the others', once every
                        // list's steps are done.
                        if new {
                            batch.flush_over(shadow, *address, *len);
                            shadow.close_to_refreshes(*address, *len);
                        }
                    } else {
                        batch.flush();
                        let started = crate::telemetry::start();
                        shadow.writing(*address, *len);
                        crate::telemetry::stop(Count::Snapshots, started);
                    }
                }
                Step::Texture(staging, copy) => {
                    if batch.texture_due(staging) && staging.refresh() {
                        uploads.push((staging.clone(), copy.clone()));
                    }
                }
            }
        }
        if crate::telemetry::on() {
            crate::telemetry::add(Count::Declared, declared);
            let counts = [Count::Copies, Count::CopyBytes, Count::CopyTime];
            counts.iter().zip(copies).for_each(|(c, n)| crate::telemetry::add(*c, n));
            let counts = [Count::Fills, Count::FillBytes, Count::FillTime];
            counts.iter().zip(fills).for_each(|(c, n)| crate::telemetry::add(*c, n));
        }
        let (copy_bytes, copy_us) = (copies[1] + fills[1], copies[2] + fills[2]);
        if copy_bytes > 0 || copy_us > 0 {
            PREPARING[0].fetch_add(copy_us, Ordering::Relaxed);
            PREPARING[1].fetch_add(copy_bytes, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            {
                add(C::Copies, copy_count);
                add(C::CopyBytes, copy_bytes);
            }
        }
        #[cfg(feature = "diagnostics")]
        if refresh_count > 0 {
            add(C::Refreshes, refresh_count);
            add(C::RefreshBytes, refresh_bytes);
        }
        uploads
    }
}

/// A submission's refreshes not done yet, merged per buffer copy into the
/// range that covers them: done before a step that writes title memory or
/// gives a range to shaders, and before the lists run.
///
/// Also the textures brought up to date in the submission so far: most
/// lists of a world submission sample the same textures, and a texture in
/// physical-page memory is hashed whole each time it is checked. Checked
/// once, it is checked again only after a step that writes title memory.
#[derive(Default)]
struct Batch {
    refreshes: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)>,
    /// The same of sparse copies, by the memory they share and where a piece
    /// starts: its end. Those near each other are one piece.
    pieces: std::collections::BTreeMap<(usize, usize), (usize, std::sync::Arc<crate::shadow::Shadow>)>,
    /// The ranges the lists' shaders write, each once, and their keys.
    writes: Vec<Range>,
    declared: std::collections::HashSet<(usize, usize, usize)>,
    /// Staging buffers (by address) refreshed since the last title write.
    textures: std::collections::HashSet<usize>,
    /// View heaps the submission's draws index directly, and whether their
    /// shaders may write memory.
    bindless: Vec<(std::sync::Arc<crate::descriptor::host::HostHeap>, bool)>,
    /// Their plans, which hold the host resources the slots view until the
    /// submission has run.
    plans: Vec<std::sync::Arc<crate::descriptor::host::BindlessPlan>>,
}

/// Pieces of one copy this near each other are refreshed as one.
const PIECE_GAP: usize = 4096;

impl Batch {
    fn add(&mut self, shadow: &std::sync::Arc<crate::shadow::Shadow>, address: usize, len: usize) {
        let end = address + len;
        if shadow.is_sparse() {
            // It stands for far more memory than a list reads.
            return self.add_piece(shadow, address, end);
        }
        match self
            .refreshes
            .iter_mut()
            .find(|(s, ..)| std::sync::Arc::ptr_eq(s, shadow))
        {
            Some((_, from, to)) => (*from, *to) = ((*from).min(address), (*to).max(end)),
            None => self.refreshes.push((shadow.clone(), address, end)),
        }
    }

    /// Adds `[from, to)` of a sparse copy, one piece with those near it.
    fn add_piece(&mut self, shadow: &std::sync::Arc<crate::shadow::Shadow>, mut from: usize, mut to: usize) {
        let root = shadow.root_key();
        // The piece that starts at or before it ...
        if let Some((&(r, a), &(b, _))) = self.pieces.range(..=(root, from)).next_back()
            && r == root
            && from <= b.saturating_add(PIECE_GAP)
        {
            if to <= b {
                return;
            }
            from = a;
        }
        // ... and those that start inside it or near its end.
        while let Some((&key, &(b, _))) =
            self.pieces.range((root, from)..=(root, to.saturating_add(PIECE_GAP))).next()
        {
            to = to.max(b);
            self.pieces.remove(&key);
        }
        self.pieces.insert((root, from), (to, shadow.clone()));
    }

    /// Shaders of the submission write a range: whether none did before.
    fn declare(&mut self, shadow: &Arc<crate::shadow::Shadow>, address: usize, len: usize) -> bool {
        let new = self.declared.insert((shadow.root_key(), address, len));
        if new {
            self.writes.push((shadow.clone(), address, len));
        }
        new
    }

    /// Does the refreshes of a sparse copy that meet a range of it, before
    /// its shaders write that. The others wait: what they bring up to date
    /// is not the range's.
    fn flush_over(&mut self, shadow: &std::sync::Arc<crate::shadow::Shadow>, address: usize, len: usize) {
        let root = shadow.root_key();
        // Pieces lie apart, so those that end in the range or past it are
        // the last of those that start before its end.
        let over: Vec<(usize, usize)> = self
            .pieces
            .range((root, 0)..(root, address + len))
            .rev()
            .take_while(|(_, (to, _))| *to > address)
            .map(|(&key, _)| key)
            .collect();
        for key in over {
            if let Some((to, shadow)) = self.pieces.remove(&key) {
                update(&shadow, key.1, to - key.1);
            }
        }
    }

    /// Whether `staging` is still to be refreshed in this submission (and
    /// notes it refreshed).
    fn texture_due<T>(&mut self, staging: &std::sync::Arc<T>) -> bool {
        self.textures.insert(std::sync::Arc::as_ptr(staging) as *const () as usize)
    }

    /// A list of the submission indexes `heap` directly.
    fn bindless(&mut self, heap: &std::sync::Arc<crate::descriptor::host::HostHeap>, writes: bool) {
        match self
            .bindless
            .iter_mut()
            .find(|(h, _)| std::sync::Arc::ptr_eq(h, heap))
        {
            Some((_, w)) => *w |= writes,
            None => self.bindless.push((heap.clone(), writes)),
        }
    }

    /// Before a step that writes title memory: pending refreshes are done
    /// first, and textures are checked again after it.
    fn title_written(&mut self) {
        self.flush();
        self.textures.clear();
    }

    fn flush(&mut self) {
        for (shadow, from, to) in self.refreshes.drain(..) {
            update(&shadow, from, to - from);
        }
        for ((_, from), (to, shadow)) in std::mem::take(&mut self.pieces) {
            update(&shadow, from, to - from);
        }
    }

    /// After every list's steps (no title copy follows): each directly
    /// indexed heap's slots, once per submission however many lists and
    /// draws use it. What they read is refreshed (unchanged pages cost only
    /// the write-watch's answer); their unordered access ranges are kept
    /// for copy-back only when a shader that may write memory uses the heap.
    fn prepare_bindless(&mut self) {
        self.flush();
        for (heap, writes) in std::mem::take(&mut self.bindless) {
            let started = std::time::Instant::now();
            let plan = heap.bindless_plan();
            BINDLESS_US[0].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
            let Some(plan) = plan else {
                continue;
            };
            let (reads, all) = plan.copy_constants();
            for reads in reads.into_iter().filter(|r| !r.is_empty()) {
                let started = std::time::Instant::now();
                let copied = AtomicU64::new(0);
                in_parallel(reads, 8, |reads| {
                    for (shadow, address, len) in reads {
                        #[cfg(feature = "diagnostics")]
                        {
                            use crate::copystat::{C, add};
                            add(C::Refreshes, 1);
                            add(C::RefreshBytes, *len as u64);
                        }
                        let n = shadow.update_lazily(*address, *len, all);
                        copied.fetch_add(n as u64, Ordering::Relaxed);
                    }
                });
                let us = started.elapsed().as_micros() as u64;
                PREPARING[0].fetch_add(us, Ordering::Relaxed);
                PREPARING[1].fetch_add(copied.into_inner(), Ordering::Relaxed);
                crate::telemetry::count(Count::PlanReads, reads.len() as u64);
                crate::telemetry::count(Count::PlanReadTime, us);
            }
            // shortcut: what shaders write through directly indexed slots into
            // copied memory stays in the copy (title memory used in place
            // needs no copy back); copying all of it back cost ~0.3 s a frame.
            if writes && cfg!(test) {
                for (shadow, address, len) in plan.writes.iter() {
                    if self.declare(shadow, *address, *len) {
                        shadow.writing(*address, *len);
                    }
                }
            }
            self.plans.push(plan);
        }
    }
}

/// Uploads before a submission's lists ([`crate::texture::upload_first`]).
#[derive(Default)]
struct Early {
    copies: Vec<(
        std::sync::Arc<crate::texture::Staging>,
        std::sync::Arc<crate::resource::HostTexture>,
    )>,
    before: Vec<D3D12_RESOURCE_BARRIER>,
    after: Vec<D3D12_RESOURCE_BARRIER>,
}

impl Early {
    /// Takes the textures waiting; `late`: after the lists' preludes took
    /// the textures' states, so not those the lists use (they upload them).
    fn take(&mut self, batch: &mut Batch, runs: &[&HostList], late: bool) {
        let first = crate::texture::take_first();
        if first.is_empty() {
            return;
        }
        let mut used = std::collections::HashSet::new();
        if late {
            for list in runs {
                let states = list.states.lock().unwrap_or_else(|e| e.into_inner());
                used.extend(states.values().map(|s| std::sync::Arc::as_ptr(&s.2) as usize));
            }
        }
        const COPY_DEST: i32 = D3D12_RESOURCE_STATE_COPY_DEST.0;
        const SHADER_RESOURCE: u32 = 0x40 | 0x80;
        for (staging, copy) in first {
            if used.contains(&(std::sync::Arc::as_ptr(&copy) as usize))
                || !batch.texture_due(&staging)
                || !staging.refresh()
            {
                continue;
            }
            let real = copy.state.swap(SHADER_RESOURCE, Ordering::Relaxed) as i32;
            let (to, from) = (D3D12_RESOURCE_STATES(COPY_DEST), D3D12_RESOURCE_STATES(real));
            if real != COPY_DEST {
                self.before.push(transition(&copy.texture, from, to));
            }
            self.after
                .push(transition(&copy.texture, to, D3D12_RESOURCE_STATES(SHADER_RESOURCE as i32)));
            self.copies.push((staging, copy));
        }
    }

    /// The list with the uploads, if there are any. A submission is prepared
    /// once those before it are finished, so there is one list.
    fn record(&self, host: &Host) -> Option<ID3D12CommandList> {
        if self.copies.is_empty() {
            return None;
        }
        struct List(ID3D12CommandAllocator, ID3D12GraphicsCommandList);
        // SAFETY: free-threaded objects, used under the submission's lock.
        unsafe impl Send for List {}
        // SAFETY: as above.
        unsafe impl Sync for List {}
        static LIST: OnceLock<Option<List>> = OnceLock::new();
        // SAFETY: the list is not executing (as above); the textures and
        // staging buffers are live.
        unsafe {
            let List(allocator, list) = LIST
                .get_or_init(|| {
                    let allocator: ID3D12CommandAllocator = host
                        .device
                        .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                        .ok()?;
                    let list: ID3D12GraphicsCommandList = host
                        .device
                        .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                        .ok()?;
                    list.Close().ok()?;
                    Some(List(allocator, list))
                })
                .as_ref()?;
            let recorded = allocator.Reset().and_then(|()| {
                list.Reset(allocator, None)?;
                if !self.before.is_empty() {
                    list.ResourceBarrier(&self.before);
                }
                for (staging, copy) in &self.copies {
                    staging.record_copies(list, &copy.texture);
                }
                list.ResourceBarrier(&self.after);
                list.Close()
            });
            if let Err(e) = recorded {
                fail(format_args!("texture uploads did not record ({e})"));
            }
            list.cast().ok()
        }
    }
}

/// Runs `f` on pieces of `list`, of `least` items or more, on some threads.
pub(crate) fn in_parallel<T: Sync>(list: &[T], least: usize, f: impl Fn(&[T]) + Sync) {
    const THREADS: usize = 6;
    thread_local! {
        static WORKER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if list.len() <= least || WORKER.with(std::cell::Cell::get) {
        return f(list);
    }
    // Small pieces, taken in turn: they are not equally long to do.
    let piece = (list.len() / (THREADS * 8)).max(least);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let work = || WORKER.with(|worker| {
        struct ResetWorker<'a>(&'a std::cell::Cell<bool>);
        impl Drop for ResetWorker<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        // A copy-back already parallel over shadows must not start another
        // set of workers for every sparse shadow it reaches.
        worker.set(true);
        let _reset = ResetWorker(worker);
        loop {
            let from = next.fetch_add(piece, Ordering::Relaxed);
            if from >= list.len() {
                break;
            }
            f(&list[from..(from + piece).min(list.len())]);
        }
    });
    let threads = THREADS.min(list.len().div_ceil(piece));
    crate::telemetry::count(Count::Parallel, 1);
    if crate::pool::on() {
        return crate::pool::join(threads - 1, &work);
    }
    std::thread::scope(|s| {
        for _ in 1..threads {
            s.spawn(work);
        }
        work();
    });
}

/// Brings `[from, from + len)` of a host copy up to date now.
fn update(shadow: &crate::shadow::Shadow, from: usize, len: usize) {
    let started = std::time::Instant::now();
    let copied = shadow.update(from, len);
    let us = started.elapsed().as_micros() as u64;
    #[cfg(feature = "diagnostics")]
    crate::copystat::range(from, len, us, 0);
    if crate::telemetry::on() {
        crate::telemetry::add(Count::Updates, 1);
        crate::telemetry::add(Count::UpdateTime, us);
    }
    PREPARING[0].fetch_add(us, Ordering::Relaxed);
    PREPARING[1].fetch_add(copied as u64, Ordering::Relaxed);
}

/// Copies title memory at submission. The ranges were checked against their
/// resources when recorded, but the title may since have unmapped pages of
/// them (tiled memory, page mappings), which read as zeros here; a title
/// destination (`title_dst`) that is not all mapped is left alone.
///
/// The source is read through the kernel (`ReadProcessMemory`), which
/// reports an unmapped page instead of faulting. Asking `VirtualQuery` first
/// cost about 2 ms per copy in the title's big pools (it walks the page
/// tables to the region's end), which made copies run at 100-200 MB/s.
pub(crate) fn copy_mapped(dst: *mut u8, src: *const u8, len: usize, title_dst: bool) {
    #[cfg(feature = "diagnostics")]
    match crate::copystat::mode() {
        1 => {
            // SAFETY: timing only: the caller vouches for the source.
            unsafe { std::ptr::copy(src, dst, len) };
            return;
        }
        2 => return,
        _ => {}
    }
    // Whether a title destination is all mapped: asked before every copy
    // (`VYCE_OPENING=0`, as before), or of those that failed. The copy then
    // wrote its mapped beginning, as the console's GPU would.
    let ask_first = title_dst && !crate::watch::opens_only();
    if len == 0 || ask_first && !crate::texture::committed(dst, len) {
        return;
    }
    if title_dst {
        // Guarded blocks of it are opened at once, not one fault at a time.
        crate::watch::opening(dst as usize, len);
    }
    #[cfg(feature = "diagnostics")]
    let moved = std::time::Instant::now();
    let whole = read_to(src, dst, len, title_dst);
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::MoveUs, moved.elapsed().as_micros() as u64);
    if whole {
        return;
    }
    if title_dst && !ask_first && !crate::texture::committed(dst, len) {
        return;
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::Fallbacks, 1);
    if !title_dst && len >= 64 << 10 {
        read_sparse(src, dst, len);
    } else {
        read_pieces(src, dst, len, title_dst);
    }
}

/// After a failed bulk read into host memory, skip contiguous holes. Never
/// cache the query across copies: the title may remap the same address.
fn read_sparse(src: *const u8, dst: *mut u8, len: usize) {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_NOACCESS, VirtualQuery,
    };
    let mut offset = 0;
    while offset < len {
        let at = src.wrapping_add(offset);
        let to = dst.wrapping_add(offset);
        let remaining = len - offset;
        // SAFETY: the query inspects an address and fills a local structure.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        let size = unsafe { VirtualQuery(at.cast(), &mut info, size_of_val(&info)) };
        let n = (info.BaseAddress as usize).saturating_add(info.RegionSize)
            .saturating_sub(at as usize).min(remaining);
        if size != size_of_val(&info) || n == 0 {
            read_pieces(at, to, remaining, false);
            break;
        }
        if info.State != MEM_COMMIT || info.Protect == PAGE_NOACCESS {
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::Unmapped, n as u64);
            // SAFETY: this path is only used with a writable host destination.
            unsafe { to.write_bytes(0, n) };
        } else if !read(at, to, n) {
            // A mapping may change between query and read.
            read_pieces(at, to, n, false);
        }
        offset += n;
    }
}

fn read_pieces(src: *const u8, dst: *mut u8, len: usize, title_dst: bool) {
    // Something is unmapped: page by page (64 KiB, then 4 KiB in a piece
    // that fails), zeros for what cannot be read.
    for_pieces(src as usize, len, 64 << 10, |at, n| {
        let to = dst.wrapping_add(at - src as usize);
        if !read_to(at as *const u8, to, n, title_dst) {
            for_pieces(at, n, 4 << 10, |at, n| {
                let to = dst.wrapping_add(at - src as usize);
                if !read_to(at as *const u8, to, n, title_dst) {
                    #[cfg(feature = "diagnostics")]
                    crate::copystat::add(crate::copystat::C::Unmapped, n as u64);
                    // SAFETY: inside the destination, which is mapped.
                    unsafe { to.write_bytes(0, n) };
                }
            });
        }
    });
}

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".globl victory_safe_copy",
    ".globl victory_safe_copy_start",
    ".globl victory_safe_copy_end",
    ".globl victory_safe_copy_fault",
    "victory_safe_copy:",
    "victory_safe_copy_start:",
    "cmp r8, 64",
    "jb 2f",
    "1:",
    "movdqu xmm0, xmmword ptr [rdx]",
    "movdqu xmm1, xmmword ptr [rdx + 16]",
    "movdqu xmm2, xmmword ptr [rdx + 32]",
    "movdqu xmm3, xmmword ptr [rdx + 48]",
    "movdqu xmmword ptr [rcx], xmm0",
    "movdqu xmmword ptr [rcx + 16], xmm1",
    "movdqu xmmword ptr [rcx + 32], xmm2",
    "movdqu xmmword ptr [rcx + 48], xmm3",
    "add rdx, 64",
    "add rcx, 64",
    "sub r8, 64",
    "cmp r8, 64",
    "jae 1b",
    "2:",
    "cmp r8, 16",
    "jb 3f",
    "movdqu xmm0, xmmword ptr [rdx]",
    "movdqu xmmword ptr [rcx], xmm0",
    "add rdx, 16",
    "add rcx, 16",
    "sub r8, 16",
    "jmp 2b",
    "3:",
    "cmp r8, 8",
    "jb 4f",
    "mov rax, qword ptr [rdx]",
    "mov qword ptr [rcx], rax",
    "add rdx, 8",
    "add rcx, 8",
    "sub r8, 8",
    "4:",
    "test r8, r8",
    "jz 5f",
    "mov al, byte ptr [rdx]",
    "mov byte ptr [rcx], al",
    "inc rdx",
    "inc rcx",
    "dec r8",
    "jnz 4b",
    "5:",
    "victory_safe_copy_end:",
    "mov eax, 1",
    "ret",
    "victory_safe_copy_fault:",
    "xor eax, eax",
    "ret",
);

#[cfg(target_arch = "x86_64")]
unsafe extern "C" {
    fn victory_safe_copy(dst: *mut u8, src: *const u8, len: usize) -> u32;
    static victory_safe_copy_start: u8;
    static victory_safe_copy_end: u8;
    static victory_safe_copy_fault: u8;
}

/// Installs our Vectored Exception Handler once, ahead of the crash reporter,
/// so `safe_copy` can copy mapped memory in pure user mode without kernel
/// syscalls and recover cleanly if a page is unmapped.
#[cfg(target_arch = "x86_64")]
fn ensure_safe_copy_veh() {
    use windows_sys::Win32::Foundation::{
        EXCEPTION_ACCESS_VIOLATION, EXCEPTION_IN_PAGE_ERROR, NTSTATUS,
    };
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
        EXCEPTION_POINTERS,
    };
    unsafe extern "system" fn handler(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: Windows passes valid exception and context pointers.
        let r = unsafe { &*(*info).ExceptionRecord };
        if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION as NTSTATUS
            && r.ExceptionCode != EXCEPTION_IN_PAGE_ERROR as NTSTATUS
        {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        // SAFETY: `ContextRecord` is valid for x86_64 exceptions.
        let ctx = unsafe { &mut *(*info).ContextRecord };
        let rip = ctx.Rip as usize;
        let start = (&raw const victory_safe_copy_start) as usize;
        let end = (&raw const victory_safe_copy_end) as usize;
        if rip >= start && rip < end {
            const WRITE: usize = 1;
            if r.ExceptionCode == EXCEPTION_ACCESS_VIOLATION as NTSTATUS
                && r.NumberParameters >= 2
                && r.ExceptionInformation[0] == WRITE
                && crate::watch::try_open_guarded(r.ExceptionInformation[1])
            {
                return EXCEPTION_CONTINUE_EXECUTION;
            }
            ctx.Rip = (&raw const victory_safe_copy_fault) as u64;
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        EXCEPTION_CONTINUE_SEARCH
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: registers a read-only IP-range check handler ahead of crash reporting.
    ONCE.call_once(|| unsafe {
        AddVectoredExceptionHandler(1, Some(handler));
    });
}

/// Whether to force kernel `ReadProcessMemory` / `WriteProcessMemory` copies
/// (`VICTORY_KERNEL_COPY=1`, diagnostic escape hatch).
fn kernel_copy() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_KERNEL_COPY").is_some())
}

/// Zero-syscall user-mode copy for non-overlapping ranges; returns `false` if
/// either side hits an unmapped or inaccessible page.
#[cfg(target_arch = "x86_64")]
fn try_user_copy(dst: *mut u8, src: *const u8, len: usize) -> Option<bool> {
    if kernel_copy() {
        return None;
    }
    let (d, s) = (dst as usize, src as usize);
    if len == 0 {
        return Some(true);
    }
    // Reject obviously invalid low addresses without even taking a fault.
    if d < 0x10000 || s < 0x10000 {
        return Some(false);
    }
    let (Some(d_end), Some(s_end)) = (d.checked_add(len), s.checked_add(len)) else {
        return Some(false);
    };
    // Overlapping ranges fall back to kernel copy (memmove semantics).
    if d < s_end && s < d_end {
        return None;
    }
    ensure_safe_copy_veh();
    // SAFETY: leaf assembly uses only volatile registers and recovers via VEH
    // if `src` or `dst` hits an unmapped page.
    Some(unsafe { victory_safe_copy(dst, src, len) != 0 })
}

/// Reads title memory at `src` into `dst`; false if a page of it is not
/// readable.
pub(crate) fn read_into(src: *const u8, dst: &mut [u8]) -> bool {
    read(src, dst.as_mut_ptr(), dst.len())
}

/// Copies `len` bytes from `src` to `dst` unless a source page is not
/// readable.
fn read(src: *const u8, dst: *mut u8, len: usize) -> bool {
    read_to(src, dst, len, false)
}

/// [`read`]; `title_dst`: into title memory, which may be guarded.
fn read_to(src: *const u8, dst: *mut u8, len: usize, title_dst: bool) -> bool {
    #[cfg(target_arch = "x86_64")]
    if let Some(ok) = try_user_copy(dst, src, len) {
        return ok;
    }
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    if title_dst {
        // The kernel writes it, which does not fault into our handler.
        crate::watch::writing(dst as usize, len);
    }
    let mut done = 0;
    // SAFETY: the destination is mapped writable memory of `len` bytes; the
    // kernel checks the source.
    unsafe { ReadProcessMemory(GetCurrentProcess(), src.cast(), dst.cast(), len, &mut done) != 0 }
}

/// Copies host memory into title memory after a submission (what shaders
/// wrote), leaving pages that are not mapped alone.
pub(crate) fn write_mapped(dst: *mut u8, src: *const u8, len: usize) {
    use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let write = |at: usize, n: usize| {
        let from = src.wrapping_add(at - dst as usize);
        #[cfg(target_arch = "x86_64")]
        if let Some(ok) = try_user_copy(at as *mut u8, from, n) {
            return ok;
        }
        // As in `read_to`.
        crate::watch::writing(at, n);
        let mut done = 0;
        // SAFETY: the source is our mapped host copy; the kernel checks the
        // destination.
        unsafe { WriteProcessMemory(GetCurrentProcess(), at as _, from.cast(), n, &mut done) != 0 }
    };
    if len == 0 || write(dst as usize, len) {
        return;
    }
    for_pieces(dst as usize, len, 64 << 10, |at, n| {
        if !write(at, n) {
            for_pieces(at, n, 4 << 10, |at, n| {
                write(at, n);
            });
        }
    });
}

/// Calls `f` with the pieces of `[start, start + len)` cut at multiples of
/// `step`.
fn for_pieces(start: usize, len: usize, step: usize, mut f: impl FnMut(usize, usize)) {
    let end = start + len;
    let mut at = start;
    while at < end {
        let next = ((at / step + 1) * step).min(end);
        f(at, next - at);
        at = next;
    }
}

impl Host {
    /// A buffer in GPU-upload (Resizable BAR VRAM, when supported and within
    /// 85% of the live VRAM budget) or standard upload heap, mapped for CPU
    /// writing.
    pub(crate) fn upload_buffer(&self, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let use_gpu_upload = self.gpu_upload
            && video_memory().is_none_or(|[(used, budget), _]| {
                budget == 0 || used.saturating_add(size) < budget * 85 / 100
            });
        let try_make = |heap_type: D3D12_HEAP_TYPE, state: D3D12_RESOURCE_STATES| {
            let heap = D3D12_HEAP_PROPERTIES {
                Type: heap_type,
                ..Default::default()
            };
            let mut buffer: Option<ID3D12Resource> = None;
            let mut data = std::ptr::null_mut();
            // SAFETY: plain creation and mapping of an upload buffer.
            let made = unsafe {
                self.device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        state,
                        None,
                        &mut buffer,
                    )
                    .and_then(|()| {
                        let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                        b.Map(0, None, Some(&mut data))
                    })
            };
            made.map(|()| (buffer, data))
        };
        let made = if use_gpu_upload {
            try_make(D3D12_HEAP_TYPE_GPU_UPLOAD, D3D12_RESOURCE_STATE_COMMON)
                .or_else(|_| try_make(D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ))
        } else {
            try_make(D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ)
        };
        match made {
            Ok((buffer, data)) => {
                let buffer = buffer?;
                crate::memstat::track(&buffer, 0, size);
                Some((buffer, data.cast()))
            }
            Err(e) => {
                wait_if_removed(&self.device, &e);
                tracing::warn!(error = %e, size, "no upload buffer");
                None
            }
        }
    }

    /// A buffer in the readback heap, mapped for reading.
    pub(crate) fn readback_buffer(&self, size: u64) -> Option<(ID3D12Resource, *const u8)> {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        // SAFETY: plain creation and mapping of a readback buffer.
        let made = unsafe {
            self.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut buffer,
                )
                .and_then(|()| {
                    let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                    b.Map(0, None, Some(&mut data))
                })
        };
        match made {
            Ok(()) => {
                let buffer = buffer?;
                crate::memstat::track(&buffer, 1, size);
                Some((buffer, data.cast()))
            }
            Err(e) => {
                tracing::warn!(error = %e, size, "no readback buffer");
                None
            }
        }
    }
}

/// The swap chain formats a presented texture can be copied into: its own
/// family's.
fn swap_format(format: DXGI_FORMAT) -> Option<DXGI_FORMAT> {
    Some(match format.0 {
        27..=32 => DXGI_FORMAT_R8G8B8A8_UNORM,
        87 | 90 | 91 => DXGI_FORMAT_B8G8R8A8_UNORM,
        23..=25 => DXGI_FORMAT_R10G10B10A2_UNORM,
        9..=14 => DXGI_FORMAT_R16G16B16A16_FLOAT,
        _ => return None,
    })
}

const BUFFERS: u32 = 2;

/// Lists that copy presented frames, for as many as are ever in flight.
const PRESENTS: usize = 3;

struct Presenter {
    swap_chain: IDXGISwapChain3,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    copy: HostList,
    /// Those that are not waited for ([`free_present`]).
    ahead: Vec<Arc<HostList>>,
    hwnd: HWND,
    meter: Meter,
    observed_path: Option<PresentPath>,
    /// One render target view, for the back buffer the readout is drawn on.
    rtv: ID3D12DescriptorHeap,
}

#[derive(PartialEq, Eq)]
struct PresentPath {
    kind: &'static str,
    scene: Option<DXGI_FORMAT>,
    overlay: Option<DXGI_FORMAT>,
    output: DXGI_FORMAT,
}

impl Presenter {
    /// A list for a present that is not waited for, reset: one whose last
    /// present has landed, or a new one.
    fn spare(&mut self, host: &Host) -> Option<Arc<HostList>> {
        let free = self.ahead.iter().position(|l| Arc::strong_count(l) == 1);
        let at = match free {
            Some(at) => at,
            None if self.ahead.len() < PRESENTS => {
                let list = host.new_list().ok()?;
                list.close();
                #[allow(clippy::arc_with_non_send_sync)] // see `list::Submitted`
                self.ahead.push(Arc::new(list));
                self.ahead.len() - 1
            }
            // All in flight: the oldest, once it landed (`HostList::reset`).
            None => 0,
        };
        // In the order they were used.
        let list = self.ahead.remove(at);
        self.ahead.push(list.clone());
        list.reset();
        Some(list)
    }

    fn note_path(
        &mut self,
        kind: &'static str,
        scene: Option<DXGI_FORMAT>,
        overlay: Option<DXGI_FORMAT>,
    ) {
        let path = PresentPath { kind, scene, overlay, output: self.format };
        if self.observed_path.as_ref() == Some(&path) {
            return;
        }
        self.observed_path = Some(path);
        let scene = scene.map_or(0, |f| f.0);
        let overlay = overlay.map_or(0, |f| f.0);
        let output = self.format.0;
        if self.meter.telemetry {
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr().lock(),
                "VYCE_PRESENT path={kind} scene_format={scene} overlay_format={overlay} output_format={output}"
            );
        } else {
            tracing::info!(path = kind, scene, overlay, output, "presentation formats");
        }
    }

    /// Writes the frame rate and frame time in the top left corner of
    /// `back`, which is a render target on `list`.
    ///
    /// # Safety
    /// `back` is this swap chain's buffer, in the render target state.
    unsafe fn readout(&self, device: &ID3D12Device, list: &ID3D12GraphicsCommandList, back: &ID3D12Resource) {
        let Some((fps, ms)) = self.meter.shown else { return };
        if !crate::readout::on() {
            return;
        }
        let text = format!("{fps:.1} FPS  {ms:.0} MS");
        // Dots that keep their share of the picture at any size.
        let dot = (self.height as i32 / 216).max(2);
        let at = (dot * 3, dot * 3);
        let (w, h) = crate::readout::size(&text);
        let behind = RECT {
            left: at.0,
            top: at.1,
            right: at.0 + w * dot,
            bottom: at.1 + h * dot,
        };
        let marks = crate::readout::rects(&text, at, dot);
        // SAFETY: our heap of one view, and rectangles inside the picture
        // (a text of a few characters at this size).
        unsafe {
            let view = self.rtv.GetCPUDescriptorHandleForHeapStart();
            device.CreateRenderTargetView(back, None, view);
            if behind.right <= self.width as i32 && behind.bottom <= self.height as i32 {
                list.ClearRenderTargetView(view, &[0.0, 0.0, 0.0, 1.0], Some(&[behind]));
                crate::readout::clear(list, view, &[1.0, 0.9, 0.1, 1.0], &marks);
            }
        }
    }
}

/// The frame rate, shown in the window's caption every second and logged
/// every few seconds.
struct Meter {
    since: std::time::Instant,
    frames: u32,
    frames_total: u64,
    drawn: u64,
    logged: std::time::Instant,
    telemetry: bool,
    /// The caption the window had, once read.
    caption: Option<Vec<u16>>,
    /// The frames a second and milliseconds a frame of the last second.
    shown: Option<(f64, f64)>,
}

impl Meter {
    fn new() -> Self {
        let now = std::time::Instant::now();
        Meter {
            since: now,
            frames: 0,
            frames_total: 0,
            drawn: crate::draw::drawn(),
            logged: now,
            telemetry: crate::telemetry::on(),
            caption: None,
            shown: None,
        }
    }

    /// Counts a presented frame.
    fn frame(&mut self, hwnd: HWND) {
        #[cfg(feature = "diagnostics")]
        tracing::debug!(target: "victory_gpu::frame", token = crate::frame::current(), "present");
        self.frames += 1;
        self.frames_total += 1;
        #[cfg(feature = "diagnostics")]
        TIMED_FRAMES.fetch_add(1, Ordering::Relaxed);
        let elapsed = self.since.elapsed();
        if elapsed < std::time::Duration::from_secs(1) {
            return;
        }
        let drawn = crate::draw::drawn();
        let frames = f64::from(self.frames);
        let fps = frames / elapsed.as_secs_f64();
        let ms = elapsed.as_secs_f64() * 1000.0 / frames;
        let draws = (drawn - self.drawn) as f64 / frames;
        (self.since, self.frames, self.drawn) = (std::time::Instant::now(), 0, drawn);
        self.shown = Some((fps, ms));
        // Of a frame, ms preparing submissions and ms waiting for the GPU.
        let [cpu, gpu] = [0, 1].map(|i| {
            SUBMISSION_US[i].swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames
        });
        let [copy_ms, copy_mb, texture_ms, texture_mb] = [0, 1, 2, 3].map(|i| {
            let scale = if i % 2 == 0 { 1e3 } else { 1e6 };
            PREPARING[i].swap(0, Ordering::Relaxed) as f64 / scale / frames
        });
        let [back_ms, back_mb] = [0, 1].map(|i| {
            COPY_BACK[i].swap(0, Ordering::Relaxed) as f64 / [1e3, 1e6][i] / frames
        });
        let [plans_ms, records_ms] =
            [0, 1].map(|i| BINDLESS_US[i].swap(0, Ordering::Relaxed) as f64 / 1e3 / frames);
        let [ordinary, screen] = crate::pipeline::take_wait_stats();
        let world_wait_ms = ordinary.micros as f64 / 1e3 / frames;
        let screen_wait_ms = screen.micros as f64 / 1e3 / frames;
        let pipeline_wait_ms = world_wait_ms + screen_wait_ms;
        let pipeline_waits = ordinary.calls + screen.calls;
        let pipeline_timeouts = ordinary.timeouts + screen.timeouts;
        let dropped = crate::draw::take_dropped() as f64 / frames;
        let indirect = crate::list::take_indirect() as f64 / frames;
        let count = |what| crate::telemetry::take(what) as f64 / frames;
        let time = |what| crate::telemetry::take(what) as f64 / 1e3 / frames;
        let prewarm_ms = PREWARM_US.swap(0, Ordering::Relaxed) as f64 / 1e3 / frames;
        if self.telemetry && [fps, ms, draws, gpu, cpu, back_ms, dropped].into_iter().all(f64::is_finite) {
            // Keep one complete sample together with respect to other stderr writers.
            // A closed or unavailable monitor output must not interrupt rendering.
            use std::io::Write as _;
            let [local, shared] = video_memory().unwrap_or_default();
            let _ = writeln!(
                std::io::stderr().lock(),
                "VYCE_TELEMETRY {{\"schema\":1,\"event\":\"frame\",\"fps\":{fps:.2},\
                 \"frame_ms\":{ms:.2},\"frames_total\":{},\"draws_per_frame\":{draws:.2},\
                 \"gpu_wait_ms\":{gpu:.2},\"cpu_prepare_ms\":{cpu:.2},\"copy_back_ms\":{back_ms:.2},\
                 \"cpu_copy_ms\":{copy_ms:.2},\"cpu_copy_mb\":{copy_mb:.2},\"guest_write_mb\":{back_mb:.2},\
                 \"texture_refresh_ms\":{texture_ms:.2},\"texture_refresh_mb\":{texture_mb:.2},\
                 \"bindless_plan_ms\":{plans_ms:.2},\"bindless_record_ms\":{records_ms:.2},\
                 \"indirect_per_frame\":{indirect:.2},\
                 \"pipeline_wait_ms\":{pipeline_wait_ms:.2},\"pipeline_waits\":{pipeline_waits},\
                 \"pipeline_world_wait_ms\":{world_wait_ms:.2},\"pipeline_screen_wait_ms\":{screen_wait_ms:.2},\
                 \"pipeline_wait_timeouts\":{pipeline_timeouts},\
                 \"submissions_per_frame\":{:.2},\"unwaited_per_frame\":{:.2},\
                 \"held_by_card_per_frame\":{:.2},\"gpu_busy_ms\":{:.2},\"gpu_timed_per_frame\":{:.2},\
                 \"snapshot_ms\":{:.2},\"steps_ms\":{:.2},\"card_record_ms\":{:.2},\"submit_ms\":{:.2},\
                 \"prewarm_ms\":{prewarm_ms:.2},\"vram_mb\":{},\"vram_budget_mb\":{},\
                 \"shared_mb\":{},\"card_mb\":{},\
                 \"dropped_per_frame\":{dropped:.2},\"pipelines_building\":{}{}}}",
                self.frames_total,
                count(Count::Submissions),
                count(Count::Unwaited),
                count(Count::HeldByCard),
                time(Count::Gpu),
                count(Count::GpuTimed),
                time(Count::Snapshots),
                time(Count::Steps),
                time(Count::Card),
                time(Count::Submit),
                local.0 >> 20,
                local.1 >> 20,
                shared.0 >> 20,
                crate::card::bytes() >> 20,
                crate::pipeline::making(),
                crate::telemetry::more(frames),
            );
        }
        // Over the same frames as the counts above.
        #[cfg(feature = "diagnostics")]
        let drops = crate::pipeline::take_drops();
        if self.logged.elapsed() >= std::time::Duration::from_secs(5) {
            self.logged = std::time::Instant::now();
            #[cfg(feature = "diagnostics")]
            tracing::info!("sparse writes since: {}", crate::shadow::sparse_writes_text());
            #[cfg(feature = "diagnostics")]
            log_gpu_waits();
            #[cfg(feature = "diagnostics")]
            log_gpu_times();
            tracing::info!(
                "[{} {}] {fps:.1} fps, {ms:.1} ms/frame ({cpu:.1} preparing: copies {copy_ms:.1} ms \
                 {copy_mb:.1} MB, textures {texture_ms:.1} ms {texture_mb:.1} MB, bindless plans \
                 {plans_ms:.1} ms; {gpu:.1} on the GPU; {back_ms:.1} copying back {back_mb:.1} MB; \
                 bindless records {records_ms:.1} ms; pipeline waits {world_wait_ms:.1} ms ordinary \
                 {screen_wait_ms:.1} ms screen, {pipeline_timeouts}/{pipeline_waits} timed out in sample), {draws:.0} draws/frame, \
                 dropped {dropped:.0} without a host pipeline ({making} being made) and \
                 {indirect:.0} indirect",
                crate::descriptor::host::BACKLOG[0].load(Ordering::Relaxed),
                crate::descriptor::host::BACKLOG[1].load(Ordering::Relaxed),
                making = crate::pipeline::making()
            );
            #[cfg(feature = "diagnostics")]
            {
                tracing::info!(
                    written = crate::resource::rewrites(),
                    evicted = crate::resource::EVICTED.load(Ordering::Relaxed),
                    late = crate::texture::CHANGED[0].load(Ordering::Relaxed),
                    at_once = crate::texture::CHANGED[1].load(Ordering::Relaxed),
                    "textures changed in place"
                );
                crate::texture::flips::log();
                crate::copystat::log(frames);
                for (why, n) in drops.iter().take(40) {
                    tracing::info!(
                        "dropped {:.1}/frame without a host pipeline: {why}",
                        *n as f64 / frames
                    );
                }
            }
        }
        if self.caption.is_none() {
            // The window's own text, read without a message.
            let mut buffer = vec![0u16; 512];
            // SAFETY: a window handle and a buffer of that length.
            let n = unsafe { InternalGetWindowText(hwnd, &mut buffer) };
            buffer.truncate(n.max(0) as usize);
            self.caption = Some(buffer);
        }
        let Some(caption) = &self.caption else { return };
        let text = format!(" | VIctory {fps:.0} fps {ms:.1} ms/frame {draws:.0} draws/frame");
        let mut text: Vec<u16> = caption.iter().copied().chain(text.encode_utf16()).collect();
        text.push(0);
        // Setting the text waits for the window's thread (a title thread,
        // which may not pump for a while), so a thread of ours does it and
        // presenting never waits; it takes the newest text when free.
        static PENDING: Mutex<Option<(usize, Vec<u16>)>> = Mutex::new(None);
        static SETTER: std::sync::Once = std::sync::Once::new();
        *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some((hwnd.0 as usize, text));
        SETTER.call_once(|| {
            let _ = std::thread::Builder::new()
                .name("victory-caption".into())
                .spawn(|| {
                    loop {
                        let next = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take();
                        match next {
                            // SAFETY: a window handle and a NUL-terminated text.
                            Some((hwnd, text)) => unsafe {
                                let _ = SetWindowTextW(HWND(hwnd as *mut _), PCWSTR(text.as_ptr()));
                            },
                            None => std::thread::sleep(std::time::Duration::from_millis(100)),
                        }
                    }
                });
        });
    }
}

impl Host {
    /// Shows `texture`, which is in `state` (a host state) and stays in it,
    /// as frame `frame` (saved if captures are on). `None` if nothing was
    /// shown; else, in diagnostics builds, whether the frame was black.
    pub(crate) fn present(&self, texture: &ID3D12Resource, state: u32, frame: u64) -> Option<bool> {
        // A frame that is saved is read back at once.
        #[cfg(feature = "diagnostics")]
        let saved = crate::capture::wanted(frame);
        #[cfg(not(feature = "diagnostics"))]
        let saved = false;
        let free = free_present() && async_submit() && self.overlap.load(Ordering::Relaxed) && !saved;
        if !free {
            self.line.drain(Count::WaitPresent);
        }
        // SAFETY: a live resource.
        let desc = unsafe { texture.GetDesc() };
        let Some(format) = swap_format(desc.Format) else {
            tracing::warn!(format = desc.Format.0, "cannot present this format");
            return None;
        };
        let (width, height) = (desc.Width as u32, desc.Height);
        let mut presenter = self.presenter.lock().unwrap_or_else(|e| e.into_inner());
        if presenter.is_none() {
            *presenter = match self.presenter(width, height, format) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(error = %e, "no swap chain");
                    return None;
                }
            };
        }
        let p = presenter.as_mut()?;
        if (p.width, p.height, p.format) != (width, height, format) {
            self.line.drain(Count::WaitPresent);
            // SAFETY: no back buffer is held; every submission was waited for.
            let resized = unsafe {
                p.swap_chain
                    .ResizeBuffers(BUFFERS, width, height, format, DXGI_SWAP_CHAIN_FLAG(0))
            };
            match resized {
                Ok(()) => (p.width, p.height, p.format) = (width, height, format),
                Err(e) => {
                    tracing::warn!(error = %e, width, height, "swap chain not resized");
                    return None;
                }
            }
        }
        let state = D3D12_RESOURCE_STATES(state as i32);
        // SAFETY: our swap chain, list and a live source texture.
        unsafe {
            let index = p.swap_chain.GetCurrentBackBufferIndex();
            let Ok(back) = p.swap_chain.GetBuffer::<ID3D12Resource>(index) else {
                return None;
            };
            let ahead = if free { p.spare(self) } else { None };
            if ahead.is_none() {
                p.copy.reset();
            }
            let copy = ahead.as_deref().unwrap_or(&p.copy);
            // The title may let go of it before the copy has run. (The back
            // buffer stays the swap chain's: one held is never resized.)
            copy.keep(texture);
            let list = &copy.list;
            list.ResourceBarrier(&[
                transition(texture, state, D3D12_RESOURCE_STATE_COPY_SOURCE),
                transition(
                    &back,
                    D3D12_RESOURCE_STATE_PRESENT,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                ),
            ]);
            list.CopyResource(&back, texture);
            #[cfg(feature = "diagnostics")]
            let capture = saved
                .then(|| crate::capture::read_back(&self.device, list, texture))
                .flatten();
            // The look whether it is black reads it back at once too.
            #[cfg(feature = "diagnostics")]
            let probe =
                if free { None } else { crate::capture::probe(&self.device, list, texture) };
            #[cfg(not(feature = "diagnostics"))]
            let _ = frame;
            let target = D3D12_RESOURCE_STATE_RENDER_TARGET;
            list.ResourceBarrier(&[
                transition(texture, D3D12_RESOURCE_STATE_COPY_SOURCE, state),
                transition(&back, D3D12_RESOURCE_STATE_COPY_DEST, target),
            ]);
            p.readout(&self.device, list, &back);
            list.ResourceBarrier(&[transition(&back, target, D3D12_RESOURCE_STATE_PRESENT)]);
            copy.close();
            match &ahead {
                Some(list) => self.send(list),
                None => self.execute(&[copy]),
            }
            #[cfg(feature = "diagnostics")]
            if let Some((buffer, footprint)) = capture {
                crate::capture::save_read_back(frame, &buffer, &footprint);
            }
            if p.swap_chain.Present(0, DXGI_PRESENT(0)).is_ok() {
                p.note_path("copy", Some(desc.Format), None);
                p.meter.frame(p.hwnd);
            }
            #[cfg(feature = "diagnostics")]
            return Some(probe.is_some_and(|b| crate::capture::black(&b)));
            #[cfg(not(feature = "diagnostics"))]
            Some(false)
        }
    }

    /// As [`Host::present`], with the texture `over` put over `scene` (each
    /// with the host state it is in and stays in).
    pub(crate) fn present_over(
        &self,
        scene: Option<(&ID3D12Resource, u32)>,
        over: (&ID3D12Resource, u32),
        frame: u64,
    ) -> Option<()> {
        // shortcut: waited for, until this path is used in the world.
        self.line.drain(Count::WaitPresent);
        // SAFETY: live resources.
        let (a, b) = unsafe { (scene.map(|s| s.0.GetDesc()), over.0.GetDesc()) };
        // The scene's size; with a smaller one (or none) the screen's.
        let (width, height) = match a {
            Some(a) if a.Height >= 1080 => (a.Width as u32, a.Height),
            _ => (1920, 1080),
        };
        let format = DXGI_FORMAT_R8G8B8A8_UNORM;
        let mut presenter = self.presenter.lock().unwrap_or_else(|e| e.into_inner());
        if presenter.is_none() {
            *presenter = self.presenter(width, height, format).ok();
        }
        let p = presenter.as_mut()?;
        if (p.width, p.height, p.format) != (width, height, format) {
            // SAFETY: no back buffer is held; every submission was waited for.
            unsafe {
                p.swap_chain
                    .ResizeBuffers(BUFFERS, width, height, format, DXGI_SWAP_CHAIN_FLAG(0))
            }
            .ok()?;
            (p.width, p.height, p.format) = (width, height, format);
        }
        // The swap-chain format normalizes copy-compatible formats. An SRV
        // must instead preserve a declared sRGB format and its transfer.
        let view = |d: &D3D12_RESOURCE_DESC| {
            if crate::compose::is_srgb(d.Format) {
                Some(d.Format)
            } else {
                swap_format(d.Format)
            }
        };
        let planes = [
            scene.and_then(|s| Some((s.0, view(&a?)?))),
            Some((over.0, view(&b)?)),
        ];
        // SAFETY: our swap chain, list and live textures.
        unsafe {
            let index = p.swap_chain.GetCurrentBackBufferIndex();
            let back = p.swap_chain.GetBuffer::<ID3D12Resource>(index).ok()?;
            p.copy.reset();
            let list = &p.copy.list;
            let read = D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE;
            let target = D3D12_RESOURCE_STATE_RENDER_TARGET;
            let mut into = vec![transition(&back, D3D12_RESOURCE_STATE_PRESENT, target)];
            let mut out = vec![transition(&back, target, D3D12_RESOURCE_STATE_PRESENT)];
            for (texture, state) in scene.iter().chain([&over]) {
                let state = D3D12_RESOURCE_STATES(*state as i32);
                if state != read {
                    into.push(transition(texture, state, read));
                    out.push(transition(texture, read, state));
                }
            }
            list.ResourceBarrier(&into);
            if !crate::compose::record(self, list, planes, &back, (width, height, format)) {
                // Discard this recording. The caller can still present the scene.
                p.copy.close();
                return None;
            }
            p.readout(&self.device, list, &back);
            #[cfg(feature = "diagnostics")]
            let capture = {
                list.ResourceBarrier(&[transition(&back, target, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                let capture = crate::capture::wanted(frame)
                    .then(|| crate::capture::read_back(&self.device, list, &back))
                    .flatten();
                list.ResourceBarrier(&[transition(&back, D3D12_RESOURCE_STATE_COPY_SOURCE, target)]);
                capture
            };
            #[cfg(not(feature = "diagnostics"))]
            let _ = frame;
            list.ResourceBarrier(&out);
            p.copy.close();
            self.execute(&[&p.copy]);
            #[cfg(feature = "diagnostics")]
            if let Some((buffer, footprint)) = capture {
                crate::capture::save_read_back(frame, &buffer, &footprint);
            }
            if p.swap_chain.Present(0, DXGI_PRESENT(0)).is_ok() {
                p.note_path("compose", a.map(|d| d.Format), Some(b.Format));
                p.meter.frame(p.hwnd);
            }
        }
        Some(())
    }

    fn presenter(
        &self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> windows::core::Result<Presenter> {
        let hwnd = match title_window() {
            Some(hwnd) => hwnd,
            None => window(width, height)?,
        };
        // SAFETY: plain object creation for our window.
        unsafe {
            let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: BUFFERS,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                ..Default::default()
            };
            let swap_chain = factory
                .CreateSwapChainForHwnd(&self.queue, hwnd, &desc, None, None)?
                .cast::<IDXGISwapChain3>()?;
            let copy = self.new_list()?;
            copy.close();
            let rtv: ID3D12DescriptorHeap =
                self.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                    NumDescriptors: 1,
                    ..Default::default()
                })?;
            tracing::info!(width, height, format = format.0, "presenting to a window");
            Ok(Presenter {
                swap_chain,
                width,
                height,
                format,
                copy,
                ahead: Vec::new(),
                hwnd,
                meter: Meter::new(),
                observed_path: None,
                rtv,
            })
        }
    }
}

/// A visible top-level window of this process: the one the title made, if
/// it made one.
fn title_window() -> Option<HWND> {
    extern "system" fn visit(hwnd: HWND, found: LPARAM) -> windows::core::BOOL {
        let mut process = 0;
        // SAFETY: a window handle from the enumeration; `found` is ours.
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut process));
            if process == windows::Win32::System::Threading::GetCurrentProcessId()
                && IsWindowVisible(hwnd).as_bool()
            {
                *(found.0 as *mut Option<HWND>) = Some(hwnd);
                return false.into();
            }
        }
        true.into()
    }
    let mut found: Option<HWND> = None;
    // SAFETY: the callback only writes `found`, which outlives the call.
    let _ = unsafe { EnumWindows(Some(visit), LPARAM(&raw mut found as isize)) };
    found
}

/// `state` without the bits the host texture's flags forbid: the console
/// lets the title name states its own resources allow (unordered access on
/// a depth target, whose host copy cannot have both).
pub(crate) fn legal_state(texture: &ID3D12Resource, state: u32) -> u32 {
    // SAFETY: a plain query.
    legal_for(unsafe { texture.GetDesc() }.Flags.0 as u32, state)
}

/// [`legal_state`] for a texture with resource flags `flags`.
pub(crate) fn legal_for(flags: u32, state: u32) -> u32 {
    // (flag, set or clear forbids, state bits): render target, unordered
    // access, depth, denied shader resource.
    const FORBIDDEN: [(u32, bool, u32); 4] = [
        (0x1, false, 0x4),
        (0x4, false, 0x8),
        (0x2, false, 0x10 | 0x20),
        (0x8, true, 0x40 | 0x80),
    ];
    FORBIDDEN
        .iter()
        .filter(|&&(flag, deny, _)| (flags & flag != 0) == deny)
        .fold(state, |s, &(_, _, bits)| s & !bits)
}

impl Host {
    pub(crate) fn queue(&self) -> &ID3D12CommandQueue {
        &self.queue
    }

    /// A buffer in system memory that the CPU maps (write-back, so reading
    /// it is fast too) and shaders read and write: for host copies of title
    /// memory that shaders write, which upload memory cannot be. Starts in
    /// the common state (buffers move out of it implicitly).
    pub(crate) fn shared_buffer(&self, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        // SAFETY: plain creation and mapping of a CPU-visible buffer.
        let made = unsafe {
            self.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut buffer,
                )
                .and_then(|()| {
                    let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                    b.Map(0, None, Some(&mut data))
                })
        };
        match made {
            Ok(()) => {
                let buffer = buffer?;
                crate::memstat::track(&buffer, 2, size);
                Some((buffer, data.cast()))
            }
            Err(e) => {
                wait_if_removed(&self.device, &e);
                tracing::warn!(error = %e, size, "no shared buffer");
                None
            }
        }
    }

    /// A device-local buffer shaders may write (unordered access) and indirect
    /// execution may read, starting in the common state.
    pub(crate) fn uav_buffer(&self, size: u64) -> Option<ID3D12Resource> {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a device-local buffer.
        let made = unsafe {
            self.device.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut buffer,
            )
        };
        match made {
            Ok(()) => {
                if let Some(b) = &buffer {
                    crate::memstat::track(b, 3, size);
                }
                buffer
            }
            Err(e) => {
                tracing::warn!(error = %e, size, "no unordered-access buffer");
                None
            }
        }
    }
}

/// For adapters without device-removed data (the software one): the last
/// commands each list of the failed submission noted (diagnostics, with
/// `VICTORY_D3D12_DRED`).
fn last_notes(runs: &[&HostList]) -> String {
    #[cfg(feature = "diagnostics")]
    {
        let mut text = String::new();
        for (i, r) in runs.iter().enumerate() {
            let notes = r.notes.lock().unwrap_or_else(|e| e.into_inner());
            for (n, _) in notes.iter().rev().take(6).rev() {
                text.push_str(&format!("; list {i} of {}, of its {}: {n}", runs.len(), notes.len()));
            }
        }
        text
    }
    #[cfg(not(feature = "diagnostics"))]
    {
        let _ = runs;
        String::new()
    }
}

/// Where the faulting address `va` lies against a range `(start, bytes)` a
/// command reads (diagnostics).
#[cfg(feature = "diagnostics")]
fn fault_in(va: u64, (start, bytes): (u64, u64)) -> String {
    let end = start + bytes;
    if va < start {
        format!(" (fault {:#x} below its source)", start - va)
    } else if va < end {
        format!(" (fault at source +{:#x})", va - start)
    } else {
        format!(" (fault {:#x} past its source's end)", va - end)
    }
}

/// The commands that may have been running where a list stopped at
/// command `done`: those between the barriers around it (work between
/// barriers overlaps on the GPU), by kind, and what the markers just
/// before them say (`HostList::note_op`: diagnostics builds with
/// device-removed data on).
fn in_flight(history: &[D3D12_AUTO_BREADCRUMB_OP], markers: &[(u32, String)], done: usize) -> String {
    let barrier = |op: &D3D12_AUTO_BREADCRUMB_OP| *op == D3D12_AUTO_BREADCRUMB_OP_RESOURCEBARRIER;
    let from = history[..done].iter().rposition(barrier).map_or(0, |b| b + 1);
    let to = history[done..]
        .iter()
        .position(barrier)
        .map_or(history.len(), |b| (done + b).max(done + 1));
    let mut text = format!(", commands {from}..{to} between its barriers:");
    let before = markers.iter().filter(|(at, _)| (*at as usize) < from);
    for (at, marker) in before.rev().take(6) {
        let short: String = marker.chars().take(90).collect();
        text.push_str(&format!(" (before, {at}: {short})"));
    }
    let mut shown = 0;
    for i in from..to {
        let op = history[i];
        if op == D3D12_AUTO_BREADCRUMB_OP_SETMARKER || shown == 100 {
            continue;
        }
        shown += 1;
        let marker = i
            .checked_sub(1)
            .filter(|&m| history[m] == D3D12_AUTO_BREADCRUMB_OP_SETMARKER)
            .and_then(|m| markers.iter().find(|(at, _)| *at as usize == m))
            .map_or("no marker", |(_, s)| s.as_str());
        text.push_str(&format!("; command {i} (op {}): {marker}", op.0));
        // Its constants as they are now: the title may have overwritten them.
        #[cfg(feature = "diagnostics")]
        for c in marker.split("Constant(0x").skip(1).take(4) {
            let mut parts = c.split([',', ')']);
            let at = parts.next().and_then(|a| u64::from_str_radix(a, 16).ok());
            let size = parts.next().and_then(|s| s.trim().parse::<usize>().ok());
            if let (Some(at), Some(size)) = (at, size) {
                let words = crate::shadow::words_text(at, size);
                let same = c.contains(&words);
                text.push_str(&format!(" NOW {at:#x} same={same} {words}"));
                if shown == 1 {
                    let mut slots: Vec<u32> = words
                        .split(['[', ',', ']'])
                        .filter_map(|w| u32::from_str_radix(w.trim(), 16).ok())
                        .filter(|w| (0x1000..0x13_0000).contains(w))
                        .collect();
                    for u in marker.split(" u").skip(1) {
                        let v = u.split_once("=0x").map(|(_, v)| v.split(' ').next().unwrap_or(""));
                        if let Some(v) = v.and_then(|v| u32::from_str_radix(v.trim(), 16).ok())
                            && v % 32 == 0
                            && v >= 0x10000
                        {
                            slots.extend([0, 1, 2, 3, 6, 24, 25].map(|k| v / 32 + k));
                        }
                    }
                    text.push_str(&crate::draw::report_slots(&slots));
                }
            }
        }
        // The records a skinning program takes its views from: those whose
        // slot holds no buffer.
        #[cfg(feature = "diagnostics")]
        if shown <= 3 && marker.contains("CSComputeSkinning") {
            let number = |key: &str, radix: u32, end: char| {
                marker
                    .split_once(key)
                    .and_then(|(_, v)| u64::from_str_radix(v.split(end).next()?, radix).ok())
            };
            let list = marker.split_once("Srv s1+16: placed").and_then(|(_, t)| {
                u64::from_str_radix(t.split_once(" at 0x")?.1.split('"').next()?, 16).ok()
            });
            if let (Some(list), Some(first), Some(count)) =
                (list, number(" u6=0x", 16, ' '), number(" u7=0x", 16, ' '))
            {
                text.push_str(&format!(" RECORDS {first}+{count} of {list:#x}"));
                for k in 0..count.min(64) {
                    let at = list + 192 + (first + k) * 176;
                    let words = crate::shadow::words_text(at, 128);
                    let w: Vec<u32> = words
                        .split(['[', ',', ']'])
                        .filter_map(|w| u32::from_str_radix(w.trim(), 16).ok())
                        .collect();
                    if w.len() < 32 {
                        text.push_str(&format!(" REC {k} unreadable"));
                        continue;
                    }
                    let slots = [w[18], w[15] & 0xff_ffff, w[19] & 0xff_ffff];
                    let what = crate::draw::report_slots(&slots);
                    if k < 1 || what.contains("null") || what.contains("Shader(") || what.contains("DIRTY") {
                        text.push_str(&format!(" REC {k} {w:x?}{what}"));
                    }
                }
            }
        }
        // What its root views start with now.
        #[cfg(feature = "diagnostics")]
        if shown <= 6 {
            for r in marker.split(" root").skip(1).take(10) {
                if let Some((slot, rest)) = r.split_once("=0x")
                    && let Ok(at) = u64::from_str_radix(rest.split(' ').next().unwrap_or(""), 16)
                {
                    text.push_str(&format!(" ROOT {slot} {at:#x} {}", crate::shadow::words_text(at, 80)));
                    if let Some(s) = crate::shadow::find(at, 80)
                        && s.is_sparse()
                    {
                        text.push_str(&format!(" {}", crate::shadow::args_text(at)));
                    }
                }
            }
        }
        // What an indirect command got, as the title's memory has it now.
        if let Some(at) = marker
            .strip_prefix("indirect kind ")
            .and_then(|m| m.split(" at 0x").nth(1))
            .and_then(|m| u64::from_str_radix(m.split(' ').next().unwrap_or(""), 16).ok())
        {
            text.push_str(&format!(" args now {}", crate::shadow::args_text(at)));
        }
    }
    text
}

pub(crate) fn transition(
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                // SAFETY: a borrowed pointer; the barrier never releases it.
                pResource: unsafe { std::mem::transmute_copy(resource) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

/// A window of the given client size, owned by its own message thread.
fn window(width: u32, height: u32) -> windows::core::Result<HWND> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("victory-window".into())
        .spawn(move || {
            let made = make_window(width, height);
            let ok = made.is_ok();
            let _ = sender.send(made.map(|h| h.0 as usize));
            if ok {
                pump();
            }
        })
        .map_err(|_| windows::core::Error::empty())?;
    let hwnd = receiver
        .recv()
        .map_err(|_| windows::core::Error::empty())??;
    Ok(HWND(hwnd as *mut _))
}

extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: the default handling for our window.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn make_window(width: u32, height: u32) -> windows::core::Result<HWND> {
    const CLASS: PCWSTR = w!("VIctory");
    // SAFETY: registers our class once and creates a window of it.
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassExW(&class);
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false)?;
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            w!("VIctory"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rect.right - rect.left,
            rect.bottom - rect.top,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
}

fn pump() {
    let mut message = MSG::default();
    // SAFETY: the standard message loop of this thread's window.
    unsafe {
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

#[cfg(test)]
mod batch {   // Batch::texture_due lives here; `mod tests` is already taken at the top of this file
    use super::{Batch, PIECE_GAP};
    use std::sync::Arc;

    #[test]
    fn a_texture_is_checked_once_per_submission() {
        let mut batch = Batch::default();
        let (a, b) = (Arc::new([0u8; 16]), Arc::new([0u8; 16]));
        // The first list sampling each texture checks it ...
        assert!(batch.texture_due(&a));
        assert!(batch.texture_due(&b));
        // ... later lists of the same submission do not.
        assert!(!batch.texture_due(&a));
        assert!(!batch.texture_due(&a.clone()));
        assert!(!batch.texture_due(&b));
    }

    /// Refreshes of one copy near each other are one; a range shaders will
    /// write has those that meet it done first, and only those.
    #[test]
    fn refreshes_join_and_those_a_write_meets_go_first() {
        let Some(host) = crate::host::Host::new_warp() else { return };
        let mut title = vec![1u8; 3 << 16];
        let base = (title.as_mut_ptr() as usize).next_multiple_of(1 << 16);
        let (shadow, copy) = crate::shadow::Shadow::for_test(&host, base, 1 << 16);
        let mut batch = Batch::default();
        let spans = |b: &Batch| {
            b.pieces.iter().map(|(&(_, from), (to, _))| (from - base, to - base)).collect::<Vec<_>>()
        };
        batch.add_piece(&shadow, base, base + 100);
        batch.add_piece(&shadow, base + 50, base + 300);
        batch.add_piece(&shadow, base + 10, base + 20);
        assert_eq!(spans(&batch), [(0, 300)]);
        batch.add_piece(&shadow, base + 300 + PIECE_GAP, base + 5000);
        batch.add_piece(&shadow, base + 20_000, base + 20_010);
        batch.add_piece(&shadow, base + 9200, base + 9201);
        assert_eq!(spans(&batch), [(0, 5000), (9200, 9201), (20_000, 20_010)]);
        // One that bridges two.
        batch.add_piece(&shadow, base + 5000 + PIECE_GAP, base + 9100);
        assert_eq!(spans(&batch), [(0, 9201), (20_000, 20_010)]);
        // The title writes; a range from 20,005 is about to be the shaders'.
        let skip = base - title.as_ptr() as usize;
        title[skip..][..1 << 16].fill(2);
        batch.flush_over(&shadow, base + 20_005, 64);
        // SAFETY: the copy's mapping; nothing runs.
        let held = |at: usize| unsafe { (copy as *const u8).add(at).read() };
        assert_eq!((held(20_000), held(20_009), held(0), held(9200)), (2, 2, 1, 1));
        assert_eq!(spans(&batch), [(0, 9201)]);
        batch.flush_over(&shadow, base + 9201, 100);
        assert_eq!(spans(&batch), [(0, 9201)], "it ends where the range begins");
        batch.flush();
        assert!(batch.pieces.is_empty());
        assert_eq!((held(0), held(9200), held(9201)), (2, 2, 1));
    }

    #[test]
    fn title_writes_make_textures_due_again() {
        let mut batch = Batch::default();
        let a = Arc::new(1u32);
        assert!(batch.texture_due(&a));
        assert!(!batch.texture_due(&a));
        // A copy into title memory or work that may write it may have
        // changed the texture's memory.
        batch.title_written();
        assert!(batch.texture_due(&a));
        assert!(!batch.texture_due(&a));
        // A fresh submission starts over.
        let mut next = Batch::default();
        assert!(next.texture_due(&a));
    }

    /// The hashing a submission does for textures in physical-page memory,
    /// with every list checking the textures it samples (before) and with
    /// each checked once (after). Run with
    /// `cargo test --release -p victory-gpu -- --ignored --nocapture texture_checks`.
    #[test]
    #[ignore = "benchmark"]
    fn texture_checks_per_submission() {
        const TEXTURES: usize = 48;
        const SIZE: usize = 4 << 20;
        const LISTS: usize = 12;
        let textures: Vec<Arc<Vec<u8>>> = (0..TEXTURES)
            .map(|t| Arc::new((0..SIZE).map(|i| (i * 31 + t) as u8).collect()))
            .collect();
        let submit = |dedupe: bool| {
            let mut batch = Batch::default();
            let mut hashed = 0usize;
            let started = std::time::Instant::now();
            for _ in 0..LISTS {
                for t in &textures {
                    if !dedupe || batch.texture_due(t) {
                        std::hint::black_box(blake3::hash(t));
                        hashed += 1;
                    }
                }
            }
            (started.elapsed(), hashed)
        };
        submit(false); // warm up
        let (before, n_before) = submit(false);
        let (after, n_after) = submit(true);
        assert_eq!((n_before, n_after), (TEXTURES * LISTS, TEXTURES));
        println!(
            "{LISTS} lists sampling {TEXTURES} textures of {} MiB: \
             before {n_before} hashes in {before:?}, after {n_after} in {after:?} ({:.1}x)",
            SIZE >> 20,
            before.as_secs_f64() / after.as_secs_f64()
        );
    }

    #[test]
    fn safe_copy_handles_valid_unaligned_and_unmapped_memory() {
        use super::{copy_mapped, read_into, write_mapped};
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE, VirtualAlloc,
            VirtualFree,
        };
        for len in [0usize, 1, 7, 8, 15, 16, 31, 63, 64, 65, 129, 4096, 70_000] {
            let src: Vec<u8> = (0..len).map(|i| ((i * 31 + 7) & 0xff) as u8).collect();
            let mut dst = vec![0xAAu8; len];
            assert!(read_into(src.as_ptr(), &mut dst), "valid read len={len}");
            assert_eq!(dst, src, "mismatch len={len}");
        }
        // Reserve 2 pages: page 0 committed RW, page 1 uncommitted (faults on access).
        // SAFETY: fresh reservation, freed below.
        let base = unsafe {
            VirtualAlloc(std::ptr::null(), 2 * 4096, MEM_RESERVE, PAGE_NOACCESS) as usize
        };
        assert_ne!(base, 0);
        // SAFETY: commit only page 0.
        let committed = unsafe {
            VirtualAlloc(base as *mut _, 4096, MEM_COMMIT, PAGE_READWRITE) as usize
        };
        assert_eq!(committed, base);
        // SAFETY: page 0 is committed RW.
        unsafe { std::ptr::write_bytes(base as *mut u8, 0x5A, 4096) };

        let mut buf = vec![0u8; 2 * 4096];
        // Reading across into uncommitted page 1 must fail cleanly without crashing.
        assert!(!read_into(base as *const u8, &mut buf));
        // copy_mapped fills page 0 with 0x5A and zeroes uncommitted page 1.
        copy_mapped(buf.as_mut_ptr(), base as *const u8, 2 * 4096, false);
        assert!(buf[..4096].iter().all(|&b| b == 0x5A));
        assert!(buf[4096..].iter().all(|&b| b == 0));

        // write_mapped writes page 0 and safely skips uncommitted page 1.
        let src = vec![0x3Cu8; 2 * 4096];
        write_mapped(base as *mut u8, src.as_ptr(), 2 * 4096);
        // SAFETY: page 0 is committed RW.
        let page0 = unsafe { std::slice::from_raw_parts(base as *const u8, 4096) };
        assert!(page0.iter().all(|&b| b == 0x3C));

        // SAFETY: our reservation.
        unsafe { VirtualFree(base as *mut _, 0, MEM_RELEASE) };
    }
}

/// The device is gone: why is logged, and the caller waits, so the thread
/// that runs the submission can report it before the title fails.
fn wait_if_removed(device: &ID3D12Device, error: &windows::core::Error) {
    if error.code().0 as u32 != 0x887A_0005 {
        return;
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: a plain query.
        let reason = unsafe { device.GetDeviceRemovedReason() };
        tracing::error!(?reason, layer = ?host().and_then(|h| h.log_messages()), "host GPU device gone");
    });
    std::thread::sleep(std::time::Duration::from_secs(20));
}
