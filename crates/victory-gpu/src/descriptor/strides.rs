//! The table shaders find a view heap slot's element stride and element
//! count in (`victory_shader`, `Lower::slot_dword`): `OWN_SLOTS` strides,
//! then the counts, a dword a slot.
//!
//! Nearly every program of the world starts its buffer loads with one from
//! here, so the table shaders read is in the host GPU's own memory. The CPU
//! keeps the values in ordinary memory, notes which blocks of slots changed,
//! and each submission copies those blocks over before its other lists run
//! ([`sync`]): what a descriptor write changed before the submission's call
//! is there when its lists run, and what changes while they run is not (a
//! slot reused for another view does not reach a list still to read it, as
//! it could in a table the CPU writes directly).
//!
//! `VICTORY_STRIDE_LOCAL=0`: shaders read the upload buffer the CPU writes,
//! as before (write-combined system memory: on the RTX 2070 SUPER a random
//! read there takes 1.7 us and 0.35 us here, and 48 times as long for many
//! at once; bench of 2026-11-25).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

use crate::host::Host;

/// The dword of the table the element counts start at.
pub(super) const COUNTS_AT: u32 = victory_shader::emit::OWN_SLOTS;

/// Slots copied as one.
const BLOCK: usize = 64;
/// Bytes of a block's strides (and of its counts).
const BLOCK_BYTES: usize = BLOCK * 4;

/// Whether the table shaders read is in the host GPU's own memory.
fn local() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_STRIDE_LOCAL").is_none_or(|v| v != "0"))
}

pub(super) struct StrideTable {
    /// What shaders read.
    resource: ID3D12Resource,
    gpu: u64,
    len: u32,
    side: Side,
    /// The device's address: a submission copies its own device's tables.
    device: usize,
}

/// Where the CPU writes a table.
enum Side {
    /// Into the buffer shaders read (its mapping).
    Upload(usize),
    Values(Values),
}

struct Values {
    strides: Box<[AtomicU32]>,
    counts: Box<[AtomicU32]>,
    /// A bit per [`BLOCK`] slots: written since a submission last took them.
    dirty: Box<[AtomicU64]>,
    /// Any of those.
    any: AtomicBool,
}

impl Values {
    fn new(len: usize) -> Values {
        let blocks = len.div_ceil(BLOCK);
        Values {
            strides: (0..len).map(|_| AtomicU32::new(0)).collect(),
            counts: (0..len).map(|_| AtomicU32::new(0)).collect(),
            dirty: (0..blocks.div_ceil(64)).map(|_| AtomicU64::new(0)).collect(),
            any: AtomicBool::new(false),
        }
    }

    fn set(&self, index: usize, stride: u32, count: u32) {
        let (s, c) = (&self.strides[index], &self.counts[index]);
        if s.load(Ordering::Relaxed) == stride && c.load(Ordering::Relaxed) == count {
            return;
        }
        s.store(stride, Ordering::Relaxed);
        c.store(count, Ordering::Relaxed);
        // After the values: who takes the bit reads them, and one who took
        // it just before leaves it to the next submission.
        let block = index / BLOCK;
        self.dirty[block / 64].fetch_or(1 << (block % 64), Ordering::Release);
        self.any.store(true, Ordering::Release);
    }

    /// Takes the blocks written since the last time, as runs (first block,
    /// blocks) in order. The caller reads the values after.
    fn take(&self) -> Vec<(usize, usize)> {
        let mut runs: Vec<(usize, usize)> = Vec::new();
        if !self.any.swap(false, Ordering::Acquire) {
            return runs;
        }
        for (w, word) in self.dirty.iter().enumerate() {
            // Most words are clear: not written to look.
            if word.load(Ordering::Relaxed) == 0 {
                continue;
            }
            let mut bits = word.swap(0, Ordering::Acquire);
            while bits != 0 {
                let block = w * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                match runs.last_mut() {
                    Some((first, n)) if *first + *n == block => *n += 1,
                    _ => runs.push((block, 1)),
                }
            }
        }
        runs
    }

    /// Blocks taken that were not sent after all.
    fn put_back(&self, runs: &[(usize, usize)]) {
        for &(first, n) in runs {
            for block in first..first + n {
                self.dirty[block / 64].fetch_or(1 << (block % 64), Ordering::Release);
            }
        }
        if !runs.is_empty() {
            self.any.store(true, Ordering::Release);
        }
    }
}

/// Tables with values to send, of every heap alive.
static TABLES: Mutex<Vec<Weak<StrideTable>>> = Mutex::new(Vec::new());

fn buffer_desc(bytes: u64) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    }
}

impl StrideTable {
    /// A table for `len` slots, zeros.
    pub(super) fn new(device: &ID3D12Device, len: u32) -> Option<Arc<Self>> {
        let made = local().then(|| Self::in_local(device, len)).flatten();
        made.or_else(|| Self::in_upload(device, len))
    }

    /// The bytes of a table: the element counts follow the strides, from
    /// [`COUNTS_AT`] dwords on (where shaders, which know no heap's size,
    /// look for them).
    fn bytes(len: u32) -> u64 {
        ((u64::from(COUNTS_AT) + u64::from(len)) * 4).max(4)
    }

    fn in_local(device: &ID3D12Device, len: u32) -> Option<Arc<Self>> {
        let mut resource: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a buffer, which starts as zeros.
        let gpu = unsafe {
            device
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_DEFAULT,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &buffer_desc(Self::bytes(len)),
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut resource,
                )
                .ok()?;
            resource.as_ref()?.GetGPUVirtualAddress()
        };
        let resource = resource?;
        crate::host::name(&resource, || "strides and counts by slot".into());
        let table = Arc::new(StrideTable {
            resource,
            gpu,
            len,
            side: Side::Values(Values::new(len as usize)),
            device: device.as_raw() as usize,
        });
        let mut tables = TABLES.lock().unwrap_or_else(|e| e.into_inner());
        tables.retain(|t| t.strong_count() > 0);
        tables.push(Arc::downgrade(&table));
        Some(table)
    }

    fn in_upload(device: &ID3D12Device, len: u32) -> Option<Arc<Self>> {
        let bytes = Self::bytes(len);
        let mut resource: Option<ID3D12Resource> = None;
        let mut p = std::ptr::null_mut();
        // SAFETY: plain committed upload buffer creation and persistent CPU map.
        let gpu = unsafe {
            device
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_UPLOAD,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &buffer_desc(bytes),
                    D3D12_RESOURCE_STATE_GENERIC_READ,
                    None,
                    &mut resource,
                )
                .ok()?;
            let res = resource.as_ref()?;
            res.Map(0, None, Some(&mut p)).ok()?;
            std::ptr::write_bytes(p.cast::<u8>(), 0, bytes as usize);
            res.GetGPUVirtualAddress()
        };
        Some(Arc::new(StrideTable {
            resource: resource?,
            gpu,
            len,
            side: Side::Upload(p as usize),
            device: device.as_raw() as usize,
        }))
    }

    /// The host GPU address of the table shaders read.
    pub(super) fn gpu(&self) -> u64 {
        self.gpu
    }

    /// The stride and the element count of the buffer view in slot `index`.
    pub(super) fn set(&self, index: u32, stride: u32, count: u32) {
        if index >= self.len {
            return;
        }
        match &self.side {
            // SAFETY: the mapping of `COUNTS_AT + len` dwords.
            Side::Upload(at) => unsafe {
                *(*at as *mut u32).add(index as usize) = stride;
                *(*at as *mut u32).add((COUNTS_AT + index) as usize) = count;
            },
            Side::Values(v) => v.set(index as usize, stride, count),
        }
    }
}

impl Drop for StrideTable {
    fn drop(&mut self) {
        // Lists not yet run may name its address.
        crate::host::retire(self.resource.clone());
    }
}

/// Upload memory a list of ours copies from, for one submission at a time.
struct Slot {
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    buffer: ID3D12Resource,
    data: usize,
    /// The submission it was last used for.
    submission: u64,
}

/// Bytes of upload memory a slot keeps (more, the first fill of a table, is
/// made for the one submission).
const ROOM: usize = 4 << 20;

/// What a host keeps to send tables ([`sync`]).
#[derive(Default)]
pub(crate) struct TableSync {
    slots: Vec<Slot>,
}

// SAFETY: D3D12 objects are free-threaded and the mappings persistent; used
// under its host's lock.
unsafe impl Send for TableSync {}

impl TableSync {
    /// The slot of a submission that has run, or a new one.
    fn slot(&mut self, host: &Host, finished: u64) -> Option<&mut Slot> {
        if let Some(i) = self.slots.iter().position(|s| s.submission <= finished) {
            return self.slots.get_mut(i);
        }
        // SAFETY: plain object creation.
        let (allocator, list) = unsafe {
            let allocator: ID3D12CommandAllocator =
                host.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).ok()?;
            let list: ID3D12GraphicsCommandList = host
                .device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                .ok()?;
            list.Close().ok()?;
            (allocator, list)
        };
        let (buffer, data) = host.upload_buffer(ROOM as u64)?;
        crate::host::name(&buffer, || "strides and counts to send".into());
        self.slots.push(Slot {
            allocator,
            list,
            buffer,
            data: data as usize,
            submission: 0,
        });
        self.slots.last_mut()
    }
}

/// Before submission `submission` runs (`finished` ones have): the list,
/// closed, that brings the tables shaders read up to date, if one has new
/// values; run before every other list of the submission. One thread at a
/// time prepares submissions.
pub(crate) fn sync(
    host: &Host,
    ours: &Mutex<TableSync>,
    submission: u64,
    finished: u64,
) -> Option<ID3D12CommandList> {
    let device = host.device.as_raw() as usize;
    let mut todo: Vec<(Arc<StrideTable>, Vec<(usize, usize)>)> = Vec::new();
    {
        let tables = TABLES.lock().unwrap_or_else(|e| e.into_inner());
        for table in tables.iter().filter_map(Weak::upgrade).filter(|t| t.device == device) {
            if let Side::Values(v) = &table.side {
                let runs = v.take();
                if !runs.is_empty() {
                    todo.push((table, runs));
                }
            }
        }
    }
    if todo.is_empty() {
        return None;
    }
    let list = record(host, ours, &todo, submission, finished);
    if list.is_none() {
        // Sent with the next submission instead.
        for (table, runs) in &todo {
            if let Side::Values(v) = &table.side {
                v.put_back(runs);
            }
        }
    }
    list
}

fn record(
    host: &Host,
    ours: &Mutex<TableSync>,
    todo: &[(Arc<StrideTable>, Vec<(usize, usize)>)],
    submission: u64,
    finished: u64,
) -> Option<ID3D12CommandList> {
    let bytes: usize = todo
        .iter()
        .flat_map(|(_, runs)| runs)
        .map(|(_, n)| 2 * n * BLOCK_BYTES)
        .sum();
    let mut ours = ours.lock().unwrap_or_else(|e| e.into_inner());
    let slot = ours.slot(host, finished)?;
    let (buffer, data) = if bytes <= ROOM {
        (slot.buffer.clone(), slot.data)
    } else {
        let (buffer, data) = host.upload_buffer(bytes as u64)?;
        // Kept until this submission, and some after, have run.
        crate::host::retire(buffer.clone());
        (buffer, data as usize)
    };
    // SAFETY: the list last ran with a submission that has finished; both
    // buffers hold the ranges copied, and the upload memory is this
    // submission's alone.
    unsafe {
        slot.allocator.Reset().ok()?;
        slot.list.Reset(&slot.allocator, None).ok()?;
        let mut at = 0usize;
        for (table, runs) in todo {
            let Side::Values(values) = &table.side else { continue };
            for &(first, n) in runs {
                let from = first * BLOCK;
                let to = ((first + n) * BLOCK).min(table.len as usize);
                let part = (to - from) * 4;
                for (words, to_dword) in [(&values.strides, 0), (&values.counts, COUNTS_AT as usize)] {
                    let mut dst = (data + at) as *mut u32;
                    for word in &words[from..to] {
                        dst.write(word.load(Ordering::Relaxed));
                        dst = dst.add(1);
                    }
                    slot.list.CopyBufferRegion(
                        &table.resource,
                        ((to_dword + from) * 4) as u64,
                        &buffer,
                        at as u64,
                        part as u64,
                    );
                    at += part;
                }
            }
        }
        let barriers: Vec<D3D12_RESOURCE_BARRIER> = todo
            .iter()
            .map(|(table, _)| {
                crate::host::transition(
                    &table.resource,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE | D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                )
            })
            .collect();
        slot.list.ResourceBarrier(&barriers);
        slot.list.Close().ok()?;
        crate::telemetry::add(crate::telemetry::Count::StrideBytes, at as u64);
    }
    slot.submission = submission;
    slot.list.cast().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `lists` on a queue of their own and waits.
    fn run(device: &ID3D12Device, lists: &[ID3D12CommandList]) {
        // SAFETY: closed lists on a queue and fence of our own.
        unsafe {
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })
                .expect("queue");
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).expect("fence");
            for list in lists {
                queue.ExecuteCommandLists(&[Some(list.clone())]);
            }
            queue.Signal(&fence, 1).expect("signal");
            fence.SetEventOnCompletion(1, Default::default()).expect("wait");
            device.GetDeviceRemovedReason().expect("the lists ran");
        }
    }

    /// The table as the GPU has it.
    fn read(host: &Host, table: &StrideTable) -> Vec<u32> {
        let bytes = StrideTable::bytes(table.len);
        let (back, data) = host.readback_buffer(bytes).expect("read-back");
        // SAFETY: a list of our own over live buffers; read after it ran.
        unsafe {
            let allocator: ID3D12CommandAllocator =
                host.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).expect("allocator");
            let list: ID3D12GraphicsCommandList = host
                .device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                .expect("list");
            list.CopyBufferRegion(&back, 0, &table.resource, 0, bytes);
            list.Close().expect("close");
            run(&host.device, &[list.cast().expect("list")]);
            std::slice::from_raw_parts(data.cast::<u32>(), bytes as usize / 4).to_vec()
        }
    }

    fn rng(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    /// Random writes, sent in several submissions, arrive: every slot of the
    /// GPU's table is what the CPU wrote last before the submission.
    fn writes_arrive(host: &Host) {
        const LEN: u32 = 3 * 64 * 64 + 17;
        let table = StrideTable::in_local(&host.device, LEN).expect("table");
        let ours = Mutex::new(TableSync::default());
        let mut model = vec![0u32; (COUNTS_AT + LEN) as usize];
        assert!(sync(host, &ours, 1, 0).is_none(), "a new table is zeros already");
        let mut state = 0x9e37_79b9_7f4a_7c15;
        for submission in 1..6u64 {
            for _ in 0..[1, 40, 4000, 3, 700][submission as usize - 1] {
                let index = (rng(&mut state) % u64::from(LEN)) as u32;
                let (stride, count) = (rng(&mut state) as u32, rng(&mut state) as u32);
                table.set(index, stride, count);
                model[index as usize] = stride;
                model[(COUNTS_AT + index) as usize] = count;
            }
            // The last slot, whose block ends early.
            table.set(LEN - 1, submission as u32, 7);
            model[LEN as usize - 1] = submission as u32;
            model[(COUNTS_AT + LEN - 1) as usize] = 7;
            // Past the end: nothing.
            table.set(LEN, 99, 99);
            let list = sync(host, &ours, submission, submission - 1).expect("new values");
            run(&host.device, &[list]);
            assert!(read(host, &table) == model, "submission {submission}");
            // The same values again: nothing to send.
            table.set(LEN - 1, submission as u32, 7);
            assert!(sync(host, &ours, submission, submission).is_none());
        }
        assert_eq!(ours.lock().unwrap().slots.len(), 1, "one submission at a time takes one slot");
    }

    #[test]
    fn warp_written_slots_reach_the_table_shaders_read() {
        if let Some(host) = Host::new_warp() {
            writes_arrive(&host);
        }
    }

    #[test]
    #[ignore = "requires a local D3D12 hardware adapter"]
    fn hardware_written_slots_reach_the_table_shaders_read() {
        writes_arrive(crate::host::host().expect("D3D12 host device"));
    }

    /// A slot written between the taking of the values and the run is not in
    /// that submission, and is in the next one. Submissions whose lists are
    /// still to run each have upload memory of their own.
    #[test]
    fn a_slot_written_after_the_taking_goes_with_the_next_submission() {
        let Some(host) = Host::new_warp() else { return };
        let table = StrideTable::in_local(&host.device, 1000).expect("table");
        let ours = Mutex::new(TableSync::default());
        table.set(5, 16, 100);
        let first = sync(&host, &ours, 1, 0).expect("new values");
        table.set(5, 28, 200);
        table.set(700, 12, 3);
        // The first has not run: this one is recorded beside it.
        let second = sync(&host, &ours, 2, 0).expect("new values");
        assert_eq!(ours.lock().unwrap().slots.len(), 2);
        run(&host.device, &[first]);
        let now = read(&host, &table);
        assert_eq!((now[5], now[(COUNTS_AT + 5) as usize], now[700]), (16, 100, 0));
        run(&host.device, &[second]);
        let now = read(&host, &table);
        assert_eq!((now[5], now[(COUNTS_AT + 5) as usize], now[700]), (28, 200, 12));
        assert_eq!(now[(COUNTS_AT + 700) as usize], 3);
    }

    /// A first fill larger than the upload memory kept arrives whole.
    #[test]
    fn a_fill_larger_than_the_room_kept_arrives() {
        let Some(host) = Host::new_warp() else { return };
        let len = (ROOM / 8 + 5000) as u32;
        let table = StrideTable::in_local(&host.device, len).expect("table");
        let ours = Mutex::new(TableSync::default());
        for i in 0..len {
            table.set(i, i + 1, i ^ 0x5555);
        }
        let list = sync(&host, &ours, 1, 0).expect("new values");
        run(&host.device, &[list]);
        let now = read(&host, &table);
        for i in 0..len {
            assert_eq!((now[i as usize], now[(COUNTS_AT + i) as usize]), (i + 1, i ^ 0x5555));
        }
    }

    /// Blocks taken and not sent are sent later.
    #[test]
    fn blocks_put_back_are_taken_again() {
        let v = Values::new(64 * 200);
        for index in [0, 63, 64, 64 * 70, 64 * 71 + 3, 64 * 199] {
            v.set(index, 4, 1);
        }
        let runs = v.take();
        assert_eq!(runs, [(0, 2), (70, 2), (199, 1)]);
        assert!(v.take().is_empty());
        v.put_back(&runs);
        v.set(64 * 72, 8, 1);
        assert_eq!(v.take(), [(0, 2), (70, 3), (199, 1)]);
        // Unchanged values are not written.
        v.set(64 * 72, 8, 1);
        assert!(v.take().is_empty());
    }

    /// Writers on several threads and takers beside them: every value is
    /// taken at or after its last write.
    #[test]
    fn no_write_is_lost_between_threads() {
        const LEN: usize = 64 * 64;
        let v = Values::new(LEN);
        let stop = AtomicBool::new(false);
        let mut seen = vec![0u32; LEN];
        let mut look = |v: &Values| {
            for (first, n) in v.take() {
                for i in first * BLOCK..(first + n) * BLOCK {
                    seen[i] = v.strides[i].load(Ordering::Relaxed);
                }
            }
        };
        std::thread::scope(|s| {
            let writers: Vec<_> = (0..4)
                .map(|t| {
                    let v = &v;
                    s.spawn(move || {
                        for round in 1..200u32 {
                            for i in (t..LEN).step_by(4) {
                                v.set(i, round, t as u32);
                            }
                        }
                    })
                })
                .collect();
            while !writers.iter().all(|w| w.is_finished()) {
                look(&v);
            }
            stop.store(true, Ordering::Relaxed);
        });
        look(&v);
        assert!(seen.iter().all(|&s| s == 199));
    }
}
