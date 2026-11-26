//! What a frame's submissions cost, for the session panel
//! (`VYCE_TELEMETRY=1`): how many ran and how many were waited for, the
//! parts of their preparation, and how long the host GPU worked on them
//! (its own timestamps, not our wait for it). Off, nothing is timed here.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use windows::Win32::Graphics::Direct3D12::*;

use crate::host::Host;

/// Whether the session panel asked for measurements.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VYCE_TELEMETRY").is_some_and(|v| v == "1"))
}

/// What is counted between two samples. Times are in microseconds.
#[derive(Clone, Copy)]
pub(crate) enum Count {
    /// Submissions, the title's and ours.
    Submissions,
    /// Those of the title that returned before the host GPU was done.
    Unwaited,
    /// Those of the title waited for only because shaders write buffers on
    /// the card.
    HeldByCard,
    /// Keeping the bytes of ranges shaders will write.
    Snapshots,
    /// Work of the runtime's own the lists left for their submission.
    Steps,
    /// Recording the uploads and compares of buffers on the card.
    Card,
    /// Giving the lists to the host queue.
    Submit,
    /// The host GPU from a submission's start to its end.
    Gpu,
    /// The submissions timed so.
    GpuTimed,
}

const COUNTS: usize = Count::GpuTimed as usize + 1;
static TOTALS: [AtomicU64; COUNTS] = [const { AtomicU64::new(0) }; COUNTS];

pub(crate) fn add(what: Count, n: u64) {
    TOTALS[what as usize].fetch_add(n, Ordering::Relaxed);
}

/// Takes what was counted since the last time.
pub(crate) fn take(what: Count) -> u64 {
    TOTALS[what as usize].swap(0, Ordering::Relaxed)
}

/// The time now, if measurements are on.
pub(crate) fn start() -> Option<Instant> {
    on().then(Instant::now)
}

/// Adds the time since `started`.
pub(crate) fn stop(what: Count, started: Option<Instant>) {
    if let Some(started) = started {
        add(what, started.elapsed().as_micros() as u64);
    }
}

/// What the title last showed, sizes as width and height in one word: the
/// picture, the planes it was put together from, and the scene its temporal
/// resolve made of it (0 until there was one).
static PICTURE: AtomicU64 = AtomicU64::new(0);
static PLANES: AtomicU64 = AtomicU64::new(0);
static SCENE: AtomicU64 = AtomicU64::new(0);

/// A picture of this size, of so many planes, is presented.
pub(crate) fn note_picture(width: u32, height: u32, planes: u64) {
    if on() {
        PICTURE.store(u64::from(width) << 32 | u64::from(height), Ordering::Relaxed);
        PLANES.store(planes, Ordering::Relaxed);
    }
}

/// The title's temporal resolve is dispatched for a scene of this size.
pub(crate) fn note_scene(width: u32, height: u32) {
    SCENE.store(u64::from(width) << 32 | u64::from(height), Ordering::Relaxed);
}

/// The picture's width, height and planes, and the scene's width and height.
pub(crate) fn picture() -> [u64; 5] {
    let [picture, scene] = [&PICTURE, &SCENE].map(|s| s.load(Ordering::Relaxed));
    let low = |v: u64| v & u64::from(u32::MAX);
    [picture >> 32, low(picture), PLANES.load(Ordering::Relaxed), scene >> 32, low(scene)]
}

/// Two lists that take the host GPU's time around a submission. One pair
/// does: a submission runs only after the one before it has.
pub(crate) struct Clock {
    begin: ID3D12CommandList,
    end: ID3D12CommandList,
    /// The two timestamps, in mapped read-back memory.
    times: *const u64,
    /// Ticks per second.
    frequency: u64,
    /// What the lists were recorded in and name.
    _allocators: [ID3D12CommandAllocator; 2],
    _heap: ID3D12QueryHeap,
    _buffer: ID3D12Resource,
}

// SAFETY: D3D12 objects are free-threaded; the mapped words are read only
// under the submission's lock.
unsafe impl Send for Clock {}
// SAFETY: as above.
unsafe impl Sync for Clock {}

impl Clock {
    pub(crate) fn new(host: &Host) -> Option<Clock> {
        let (buffer, times) = host.readback_buffer(16)?;
        let made = Self::record(host, buffer, times.cast());
        made.inspect_err(|e| {
            tracing::warn!(error = %e, "no host GPU timestamps for the session panel");
        })
        .ok()
    }

    fn record(host: &Host, buffer: ID3D12Resource, times: *const u64) -> windows::core::Result<Clock> {
        use windows::core::Interface;
        let device = &host.device;
        // SAFETY: plain creation; the recorded lists name objects the clock
        // keeps.
        unsafe {
            let mut heap: Option<ID3D12QueryHeap> = None;
            device.CreateQueryHeap(
                &D3D12_QUERY_HEAP_DESC {
                    Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                    Count: 2,
                    NodeMask: 0,
                },
                &mut heap,
            )?;
            let heap = heap.ok_or_else(windows::core::Error::empty)?;
            let list = || -> windows::core::Result<(ID3D12CommandAllocator, ID3D12GraphicsCommandList)> {
                let allocator: ID3D12CommandAllocator =
                    device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
                let list =
                    device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
                Ok((allocator, list))
            };
            let (first, begin) = list()?;
            begin.EndQuery(&heap, D3D12_QUERY_TYPE_TIMESTAMP, 0);
            begin.Close()?;
            let (second, end) = list()?;
            end.EndQuery(&heap, D3D12_QUERY_TYPE_TIMESTAMP, 1);
            end.ResolveQueryData(&heap, D3D12_QUERY_TYPE_TIMESTAMP, 0, 2, &buffer, 0);
            end.Close()?;
            Ok(Clock {
                begin: begin.cast()?,
                end: end.cast()?,
                times,
                frequency: host.queue().GetTimestampFrequency()?.max(1),
                _allocators: [first, second],
                _heap: heap,
                _buffer: buffer,
            })
        }
    }

    pub(crate) fn begin(&self) -> ID3D12CommandList {
        self.begin.clone()
    }

    pub(crate) fn end(&self) -> ID3D12CommandList {
        self.end.clone()
    }

    /// The timestamps of the submission that ran last.
    fn read(&self) -> [u64; 2] {
        // SAFETY: both words are mapped; the GPU has written them.
        [0, 1].map(|i| unsafe { self.times.add(i).read_volatile() })
    }

    /// Counts the submission that just ran between the two lists.
    pub(crate) fn ran(&self) {
        let [from, to] = self.read();
        if to > from {
            let us = u128::from(to - from) * 1_000_000 / u128::from(self.frequency);
            add(Count::Gpu, us as u64);
            add(Count::GpuTimed, 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lists run again and again, and an ordered pair of later times
    /// comes back each time.
    #[test]
    fn the_clock_times_every_submission_it_goes_around() {
        let Some(host) = crate::host::host() else { return };
        let Some(clock) = Clock::new(host) else { return };
        // SAFETY: our own queue, fence and closed lists; each run is waited
        // for before the next.
        unsafe {
            let queue: ID3D12CommandQueue = host
                .device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })
                .unwrap();
            let fence: ID3D12Fence = host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let mut last = 0;
            for run in 1..=3 {
                queue.ExecuteCommandLists(&[Some(clock.begin())]);
                queue.ExecuteCommandLists(&[Some(clock.end())]);
                queue.Signal(&fence, run).unwrap();
                while fence.GetCompletedValue() < run {
                    std::thread::yield_now();
                }
                let [from, to] = clock.read();
                assert!(to >= from, "run {run}: {from} to {to}");
                assert!(from > last, "run {run}: {from} after {last}");
                last = from;
            }
        }
    }
}
