//! Title memory in the host GPU's own memory ("cards").
//!
//! The console's GPU and CPU share memory; here a shader's access to title
//! memory crosses the bus, where a random read is 50 times and a write 100
//! times as slow as in video memory (RTX 2070 SUPER, 2026-11-25). So some of
//! the title's memory is on the card: every view, address and argument the
//! host GPU gets in it is one of a buffer in video memory, and what either
//! side writes is brought to the other around each submission:
//!
//! - What the title's threads wrote before a submission's call is in video
//!   memory when its lists run. Which 64 KiB blocks they wrote is known by
//!   guards ([`track`]) or the kernel's write watch; of those, the dwords
//!   that differ from the card's side in system memory (`staging`, what the
//!   GPU has) go up, whatever views the lists use. What they write while the
//!   lists run arrives with the next submission.
//! - What shaders wrote is in title memory before the submission's
//!   bottom-of-pipe writes, read-backs and fences ([`engine`]): pages that
//!   changed are found and fetched by the GPU, and of them the dwords that
//!   differ from the card's side are stored ([`merge`]), nothing else.
//! - CPU work of ours (title copies, fills, texture uploads, waits for
//!   values) reads and writes title memory, before a submission's lists run
//!   or after they did, when it is current.
//!
//! So a card's bytes, its side and title memory are the same between
//! submissions, but for what the title wrote since. What makes it hold:
//!
//! 1. Every way the host GPU reaches a range goes through [`held`] first
//!    (`shadow::find`, `find_reaching`, `for_view`): inside one card, it gets
//!    the card. A range that meets a card and is not inside it is refused
//!    the memory in place ([`find`]).
//! 2. Which pages are looked at after a submission is what its lists declare
//!    (`Shadow::gpu_writes`): unordered access views in tables (used or not:
//!    shaders index past the used ones), root views up to the buffer's end,
//!    query results, and the unordered access slots of heaps indexed
//!    directly by shaders that may write. A write none of these told stays
//!    in video memory, where later shaders see it (only dwords the title
//!    changed are written over), and is put back by the next look at all of
//!    the card ([`Card::sweep_due`]), which counts it.
//! 3. One submission at a time is prepared, after the one before was
//!    finished (`Host::execute_inner`), so the card's side does not change
//!    while the GPU runs.
//!
//! `VICTORY_CARDS=0`: title memory stays where it was: in place, or copied.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use crate::host::Host;
use crate::shadow::Shadow;
use crate::telemetry::{Count, add};
use crate::watch::PAGE;

mod big;
mod engine;
mod merge;
pub(crate) mod track;
mod written;

pub(crate) use big::{concerns, note, registered, settle, unregistered};
pub(crate) use engine::{Engine, Plan, Prepared};
#[cfg(target_arch = "x86_64")]
pub(crate) use merge::fault_at;

/// The unit title memory is watched in.
const PIECE: usize = PAGE;
/// The unit shader writes are found in: the kernel's page.
const LEAF: usize = 4 << 10;
/// Those of a piece.
const LEAVES: usize = PIECE / LEAF;

/// Bytes a card's buffer goes on past its memory: a root view has no size,
/// and a program reading past the end of what it was given stops the host
/// GPU where the buffer ends.
const TAIL: u64 = 1 << 20;

/// Whether cards are used (`VICTORY_CARDS=0` turns them off). Tests make
/// theirs.
pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    !cfg!(test) && *ON.get_or_init(|| std::env::var_os("VICTORY_CARDS").is_none_or(|v| v != "0"))
}

/// Whether the title's 32-bit GPU heap is a card (`VICTORY_CARD_HEAP=0`: in
/// place as before).
fn heap_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    enabled() && *ON.get_or_init(|| std::env::var_os("VICTORY_CARD_HEAP").is_none_or(|v| v != "0"))
}

/// Whether big buffers the title made in its private memory are cards
/// (`VICTORY_CARD_BUFFERS=0`: in place as before).
fn buffers_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    enabled() && *ON.get_or_init(|| std::env::var_os("VICTORY_CARD_BUFFERS").is_none_or(|v| v != "0"))
}

/// The least bytes of such a buffer, and the most of all of them
/// (`VICTORY_CARD_BUFFERS_MIN_MB`, 4; `VICTORY_CARD_BUFFERS_MB`, 1024).
fn limits() -> (u64, u64) {
    static LIMITS: OnceLock<(u64, u64)> = OnceLock::new();
    *LIMITS.get_or_init(|| {
        let mb = |name: &str, default: u64| {
            std::env::var(name).ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(default) << 20
        };
        (
            mb("VICTORY_CARD_BUFFERS_MIN_MB", 4).max(1 << 20),
            mb("VICTORY_CARD_BUFFERS_MB", 1024),
        )
    })
}

/// Whether a submission whose shaders write cards is finished when it is
/// waited for, as others are (`VICTORY_CARD_ASYNC=0`: waited for at once).
pub(crate) fn unwaited() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CARD_ASYNC").is_none_or(|v| v != "0"))
}

/// The most frames between two looks at all of a card, for writes no list
/// declared; `VICTORY_CARD_SWEEP=0`: never, `n`: every `n` frames.
fn sweeps() -> Option<u64> {
    static EVERY: OnceLock<Option<u64>> = OnceLock::new();
    *EVERY.get_or_init(|| {
        match std::env::var("VICTORY_CARD_SWEEP").ok().and_then(|v| v.trim().parse::<u64>().ok()) {
            Some(0) => None,
            Some(n) => Some(n),
            None => Some(SWEEP_MOST),
        }
    })
}

/// Looks at all of a card are this many frames apart once none found a
/// write that was not declared for some time.
const SWEEP_MOST: u64 = 32;
/// Looks without one after which they are twice as far apart.
const SWEEP_CLEAN: u32 = 16;

/// Whether the title's descriptor heaps are left out of cards: its threads
/// write them all over, and no shader reads them as data (translated ones
/// index the host's heaps). `VICTORY_CARD_DESCRIPTORS=1` takes them as any
/// memory.
fn skip_descriptors() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CARD_DESCRIPTORS").is_none_or(|v| v != "1"))
}

/// Bytes of video memory cards take, and their hashes.
static BYTES: AtomicU64 = AtomicU64::new(0);

/// Bytes of video memory the cards take.
pub(crate) fn bytes() -> u64 {
    BYTES.load(Ordering::Relaxed)
}

/// How a card learns what the title wrote.
enum Told {
    /// Its view is guarded.
    Guarded(&'static track::Track),
    /// The memory is write-watched.
    Written(Mutex<written::Written>),
    /// Every piece, every time.
    #[cfg(test)]
    Nothing,
}

pub(crate) struct Card {
    /// The title memory it stands for.
    from: usize,
    to: usize,
    /// The 64 KiB line its buffers start at, and the bytes of the pieces it
    /// has from there.
    base: usize,
    size: usize,
    /// The device's address: a submission looks after its own device's.
    device: usize,
    heap: ID3D12Heap,
    /// All of it, from `base` on.
    buffer: ID3D12Resource,
    gpu: u64,
    /// What the GPU has, on the CPU's side: in system memory, mapped.
    staging: ID3D12Resource,
    side: usize,
    /// Where `base` is for writing back: the title's address, or a view of
    /// our own, which neither faults nor tells of a write.
    back: usize,
    own_view: bool,
    told: Told,
    /// A hash for each quarter of a page ([`engine`]), and whether they are
    /// those of what the card has.
    hashes: Mutex<Option<ID3D12Resource>>,
    hashed: AtomicBool,
    /// What was taken into `side` and not sent.
    unsent: Mutex<Vec<(u32, u32)>>,
    /// It is not handed out any more: the title mapped other memory there,
    /// or something reaches the memory otherwise.
    dead: AtomicBool,
    /// Of those, the title's memory is another's now: nothing is put back.
    gone: AtomicBool,
    /// No one tells what the title writes: it is let go.
    untold: AtomicBool,
    sweep: Mutex<Sweep>,
}

/// When a card was looked at whole.
struct Sweep {
    /// The frame to look in next, and how many frames later again.
    next: u64,
    every: u64,
    /// Looks in a row without an undeclared write.
    clean: u32,
}

// SAFETY: D3D12 objects are free-threaded; the mappings are persistent and
// written by the thread that prepares or finishes a submission.
unsafe impl Send for Card {}
// SAFETY: as above.
unsafe impl Sync for Card {}

impl Drop for Card {
    fn drop(&mut self) {
        if let Told::Guarded(t) = &self.told {
            t.end();
        }
        let hashes = self.hashes.get_mut().unwrap_or_else(|e| e.into_inner()).take();
        BYTES.fetch_sub(self.local_bytes(hashes.is_some()), Ordering::Relaxed);
        if self.own_view {
            use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
            // SAFETY: our view, used by no one else.
            unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.back as *mut _ }) };
        }
        crate::host::retire((self.buffer.clone(), self.staging.clone(), hashes, self.heap.clone()));
    }
}

/// Where a card's memory is, besides at the title's address.
struct Place {
    /// `base` in a view of ours (the title's address otherwise).
    back: usize,
    own_view: bool,
    told: Told,
}

impl Card {
    /// A card for title memory `[from, to)`, with what it holds still to be
    /// sent. `None` without the memory for it.
    fn new(host: &Host, from: usize, to: usize, place: Place) -> Option<Arc<Card>> {
        let base = from & !(PIECE - 1);
        let size = to.next_multiple_of(PIECE) - base;
        let made = Self::memory(host, base, size);
        let (heap, buffer, gpu, staging, side) = match made {
            Some(m) => m,
            None => {
                if place.own_view {
                    use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
                    // SAFETY: the view made for this card.
                    unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: place.back as *mut _ }) };
                }
                if let Told::Guarded(t) = &place.told {
                    t.end();
                }
                return None;
            }
        };
        let card = Arc::new(Card {
            from,
            to,
            base,
            size,
            device: host.device.as_raw() as usize,
            heap,
            buffer,
            gpu,
            staging,
            side,
            back: place.back,
            own_view: place.own_view,
            told: place.told,
            hashes: Mutex::new(None),
            hashed: AtomicBool::new(false),
            // All of it: whatever the memory held when it was made.
            unsent: Mutex::new(vec![(0, size as u32)]),
            dead: AtomicBool::new(false),
            gone: AtomicBool::new(false),
            untold: AtomicBool::new(false),
            sweep: Mutex::new(Sweep { next: 0, every: 1, clean: 0 }),
        });
        BYTES.fetch_add(card.local_bytes(false), Ordering::Relaxed);
        let mut live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
        live.retain(|c| c.strong_count() > 0);
        live.push(Arc::downgrade(&card));
        Some(card)
    }

    /// Video memory and the side in system memory.
    #[allow(clippy::type_complexity)] // taken apart at once
    fn memory(
        host: &Host,
        base: usize,
        size: usize,
    ) -> Option<(ID3D12Heap, ID3D12Resource, u64, ID3D12Resource, usize)> {
        let (staging, side) = host.shared_buffer(size as u64)?;
        crate::host::name(&staging, || format!("the CPU's side of title memory {base:#x}+{size:#x}"));
        let bytes = size as u64 + TAIL;
        let desc = D3D12_HEAP_DESC {
            SizeInBytes: bytes,
            Properties: D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            },
            Alignment: 0,
            Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS,
        };
        let mut heap: Option<ID3D12Heap> = None;
        // SAFETY: plain creation of a heap, which starts as zeros.
        if let Err(error) = unsafe { host.device.CreateHeap(&desc, &mut heap) } {
            tracing::warn!(%error, bytes, "no video memory for a card");
            return None;
        }
        let heap = heap?;
        // Shaders are at it all the time: the last to leave video memory.
        // SAFETY: a live heap, one priority for it.
        unsafe {
            if let Ok(device) = host.device.cast::<ID3D12Device1>()
                && let Ok(pageable) = heap.cast::<ID3D12Pageable>()
            {
                let _ = device.SetResidencyPriority(1, &Some(pageable), &D3D12_RESIDENCY_PRIORITY_HIGH);
            }
        }
        let buffer = part_of(host, &heap, 0, bytes)?;
        crate::host::name(&buffer, || format!("title memory {base:#x}+{size:#x} on the card"));
        // SAFETY: a live buffer.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        Some((heap, buffer, gpu, staging, side as usize))
    }

    fn local_bytes(&self, hashes: bool) -> u64 {
        self.size as u64 + TAIL + if hashes { engine::hash_bytes(self.size) } else { 0 }
    }

    /// Where the title memory it stands for starts and ends.
    pub(crate) fn span(&self) -> (u64, u64) {
        (self.from as u64, self.to as u64)
    }

    /// The title address its buffer starts at.
    pub(crate) fn base(&self) -> u64 {
        self.base as u64
    }

    pub(crate) fn buffer(&self) -> &ID3D12Resource {
        &self.buffer
    }

    pub(crate) fn gpu(&self) -> u64 {
        self.gpu
    }

    pub(crate) fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }

    /// Whether it is a title buffer's ([`big`]).
    pub(crate) fn is_buffer(&self) -> bool {
        matches!(self.told, Told::Written(_))
    }

    /// The card's memory from 64 KiB line `shift` of it on, as a buffer (for
    /// views whose elements do not fall on multiples of their size from its
    /// start).
    pub(crate) fn from_line(&self, host: &Host, shift: u64) -> Option<(ID3D12Resource, u64)> {
        let buffer = part_of(host, &self.heap, shift, self.size as u64 + TAIL - shift)?;
        crate::host::name(&buffer, || format!("title memory {:#x} on the card", self.base as u64 + shift));
        // SAFETY: a live buffer.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        Some((buffer, gpu))
    }

    /// The pieces it has.
    fn pieces(&self) -> usize {
        self.size / PIECE
    }

    /// The pages of title memory `[from, to)`, cut to the card: first and
    /// past the last.
    fn leaves_of(&self, from: usize, to: usize) -> Option<(usize, usize)> {
        let (from, to) = (from.max(self.from), to.min(self.to));
        (from < to).then(|| ((from - self.base) / LEAF, (to - self.base).div_ceil(LEAF)))
    }

    fn hashes(&self) -> Option<ID3D12Resource> {
        self.hashes.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn hashed(&self) -> bool {
        self.hashed.load(Ordering::Acquire)
    }

    fn set_hashed(&self) {
        self.hashed.store(true, Ordering::Release);
    }

    /// Memory for the hashes, if it has none.
    fn make_hashes(&self, host: &Host) -> Option<()> {
        let mut hashes = self.hashes.lock().unwrap_or_else(|e| e.into_inner());
        if hashes.is_none() {
            let bytes = engine::hash_bytes(self.size);
            let made = host.uav_buffer(bytes)?;
            crate::host::name(&made, || format!("hashes of title memory {:#x}", self.base));
            BYTES.fetch_add(bytes, Ordering::Relaxed);
            *hashes = Some(made);
        }
        Some(())
    }

    /// Takes what the title changed since the last time into the side: the
    /// ranges of it (offset, bytes) to send, in order. `reads`: title memory
    /// (from, to) the submission's lists are known to read.
    fn pull(&self, reads: &[(usize, usize)]) -> Vec<(u32, u32)> {
        let mut runs = std::mem::take(&mut *self.unsent.lock().unwrap_or_else(|e| e.into_inner()));
        let again = !runs.is_empty();
        let frame = crate::queue::presented();
        let pieces: Vec<(usize, u16)> = match &self.told {
            Told::Guarded(t) => t.take().into_iter().map(|p| (p, u16::MAX)).collect(),
            Told::Written(w) => {
                let reads: Vec<(usize, usize)> = reads
                    .iter()
                    .filter_map(|&(a, b)| self.leaves_of(a, b))
                    .map(|(a, b)| (a / LEAVES, b.div_ceil(LEAVES)))
                    .collect();
                let taken = w.lock().unwrap_or_else(|e| e.into_inner()).take(self.base, &reads, frame);
                taken.unwrap_or_else(|| {
                    // No one tells what the title writes there any more.
                    self.untold.store(true, Ordering::Release);
                    (0..self.pieces()).map(|p| (p, u16::MAX)).collect()
                })
            }
            #[cfg(test)]
            Told::Nothing => (0..self.pieces()).map(|p| (p, u16::MAX)).collect(),
        };
        if pieces.is_empty() {
            return runs;
        }
        let found: Mutex<Vec<(usize, Vec<(u32, u32)>)>> = Mutex::default();
        let scanned = AtomicU64::new(0);
        crate::host::in_parallel(&pieces, 128, |part| {
            thread_local! {
                static READ: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(vec![0; PIECE]);
            }
            READ.with_borrow_mut(|read| {
                let mut mine = Vec::new();
                for &(piece, leaves) in part {
                    let mut runs = Vec::new();
                    if leaves == u16::MAX {
                        self.pull_part(piece * PIECE, PIECE, read, &mut runs);
                    } else {
                        for k in (0..LEAVES).filter(|k| leaves >> k & 1 != 0) {
                            self.pull_part(piece * PIECE + k * LEAF, LEAF, read, &mut runs);
                        }
                    }
                    scanned.fetch_add(u64::from(leaves.count_ones()) * LEAF as u64, Ordering::Relaxed);
                    if !runs.is_empty() {
                        mine.push((piece, runs));
                    }
                }
                found.lock().unwrap_or_else(|e| e.into_inner()).extend(mine);
            });
        });
        add(Count::CardScanned, scanned.into_inner());
        let mut found = found.into_inner().unwrap_or_else(|e| e.into_inner());
        found.sort_unstable_by_key(|f| f.0);
        let changed: Vec<usize> = found.iter().map(|f| f.0).collect();
        match &self.told {
            Told::Guarded(t) => t.looked(&changed),
            Told::Written(w) => w.lock().unwrap_or_else(|e| e.into_inner()).looked(&changed, frame),
            #[cfg(test)]
            Told::Nothing => {}
        }
        runs.extend(found.into_iter().flat_map(|f| f.1));
        // Copies are cheap (0.1 us each), but not for any number.
        const MOST: usize = 1 << 14;
        if runs.len() > MOST {
            join_near(&mut runs, 256);
        } else if again {
            join_near(&mut runs, 0);
        }
        runs
    }

    /// The `n` bytes from `offset` on of [`Card::pull`]; `read` has room for
    /// them.
    fn pull_part(&self, offset: usize, n: usize, read: &mut [u8], runs: &mut Vec<(u32, u32)>) {
        let start = self.base + offset;
        let (from, to) = (start.max(self.from), (start + n).min(self.to));
        if from >= to {
            return;
        }
        let (at, n) = (from - self.base, to - from);
        // SAFETY: this part of the side's mapping, which one thread at a time
        // looks at; the GPU reads it only in lists that have run.
        let side = unsafe { std::slice::from_raw_parts_mut((self.side + at) as *mut u8, n) };
        if self.own_view {
            // Through our own view, which nothing unmaps.
            // SAFETY: inside that view.
            unsafe { merge::differing((self.back + at) as *const u8, side, at as u32, runs) };
            return;
        }
        let read = &mut read[..n];
        if !crate::host::read_into(from as *const u8, read) {
            // What of it is not there is left as it was.
            for k in (0..n).step_by(LEAF) {
                let end = (k + LEAF).min(n);
                if !crate::host::read_into((from + k) as *const u8, &mut read[k..end]) {
                    read[k..end].copy_from_slice(&side[k..end]);
                }
            }
        }
        // SAFETY: as long as the side.
        unsafe { merge::differing(read.as_ptr(), side, at as u32, runs) };
    }

    /// What [`Card::pull`] gave was not sent.
    fn put_back(&self, runs: &[(u32, u32)]) {
        self.unsent.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(runs);
    }

    /// Page `leaf` as the GPU has it, after a shader changed it: what differs
    /// from the side goes into title memory and the side. The bytes stored.
    fn put_back_page(&self, leaf: usize, page: &[u8]) -> usize {
        let start = leaf * LEAF;
        if start + LEAF > self.size || page.len() < LEAF {
            return 0;
        }
        let from = (self.from - self.base).max(start).next_multiple_of(4);
        let to = (self.to - self.base).min(start + LEAF) & !3;
        if from >= to || self.gone.load(Ordering::Acquire) {
            return 0;
        }
        // SAFETY: this page's part of the side's mapping; a page is merged by
        // one thread, with no submission being prepared.
        let side = unsafe { std::slice::from_raw_parts_mut((self.side + from) as *mut u8, to - from) };
        merge::put_back(self.back + from, &page[from - start..to - start], side)
    }

    /// Whether it is the turn of a look at all of it ([`sweeps`]).
    fn sweep_due(&self) -> bool {
        let Some(most) = sweeps() else { return false };
        let frame = crate::queue::presented() + 1;
        let mut s = self.sweep.lock().unwrap_or_else(|e| e.into_inner());
        if s.next > frame {
            return false;
        }
        s.next = frame + s.every.min(most);
        true
    }

    /// It was looked at whole; `found`: a write no list had declared.
    fn swept(&self, found: bool) {
        let mut s = self.sweep.lock().unwrap_or_else(|e| e.into_inner());
        if found {
            (s.every, s.clean) = (1, 0);
            return;
        }
        s.clean += 1;
        if s.clean >= SWEEP_CLEAN {
            (s.every, s.clean) = ((s.every * 2).min(SWEEP_MOST), 0);
        }
    }

    /// A shader wrote page `leaf`, which no list had declared.
    fn note_undeclared(&self, leaf: usize) {
        static COUNT: AtomicU64 = AtomicU64::new(0);
        let count = COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if count <= 16 || count.is_power_of_two() {
            tracing::info!(
                count,
                at = format_args!("{:#x}", self.base + leaf * LEAF),
                "shaders wrote a page of title memory no list had declared"
            );
        }
    }
}

/// A buffer over `[offset, offset + len)` of a card's heap.
fn part_of(host: &Host, heap: &ID3D12Heap, offset: u64, len: u64) -> Option<ID3D12Resource> {
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: len,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: a live heap that holds the range.
    let made = unsafe {
        host.device.CreatePlacedResource(heap, offset, &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut buffer)
    };
    if let Err(error) = made {
        tracing::warn!(%error, offset, len, "no buffer over a card's memory");
        return None;
    }
    let buffer = buffer?;
    // Descriptor slots and lists hold the buffer: it holds its heap.
    const KEY: windows::core::GUID = windows::core::GUID::from_u128(0x2f4b_8d11_6e0a_4c3f_9a57_71c0_3d1e_55a2);
    // SAFETY: a live object; the interface is referenced until the buffer
    // is destroyed.
    let held = unsafe { buffer.SetPrivateDataInterface(&KEY, heap) };
    if let Err(error) = held {
        tracing::warn!(%error, "a card's buffer does not hold its heap");
        return None;
    }
    Some(buffer)
}

/// Joins runs (offset, bytes), in order, less than `gap` bytes apart.
fn join_near(runs: &mut Vec<(u32, u32)>, gap: u32) {
    runs.sort_unstable();
    let mut out: Vec<(u32, u32)> = Vec::with_capacity(runs.len());
    for &(at, len) in runs.iter() {
        match out.last_mut() {
            Some((a, l)) if at <= *a + *l + gap => *l = (*l).max(at + len - *a),
            _ => out.push((at, len)),
        }
    }
    *runs = out;
}

/// Every card there is, for submissions to look after: as long as a list or
/// a slot still names one.
static LIVE: Mutex<Vec<Weak<Card>>> = Mutex::new(Vec::new());

/// The cards of a device.
pub(crate) fn live(device: &ID3D12Device) -> Vec<Arc<Card>> {
    let device = device.as_raw() as usize;
    let live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    live.iter().filter_map(Weak::upgrade).filter(|c| c.device == device).collect()
}

/// Cards handed out, by where their memory starts.
static OWNED: Mutex<BTreeMap<u64, Arc<Shadow>>> = Mutex::new(BTreeMap::new());

/// [`OWNED`], to look up without a lock: in order of their starts, apart
/// from one another.
struct Route {
    from: u64,
    to: u64,
    shadow: Weak<Shadow>,
}

/// The routes now. Old ones are never freed (a few hundred bytes each time
/// a card comes or goes): a thread may be reading them.
static ROUTES: AtomicPtr<Vec<Route>> = AtomicPtr::new(std::ptr::null_mut());

/// Makes `map` the routes.
fn publish(map: &BTreeMap<u64, Arc<Shadow>>) {
    let routes: Vec<Route> = map
        .values()
        .filter_map(|s| {
            let (from, to) = s.card()?.span();
            Some(Route { from, to, shadow: Arc::downgrade(s) })
        })
        .collect();
    ROUTES.store(Box::into_raw(Box::new(routes)), Ordering::Release);
}

/// What a range of title memory is to the cards.
pub(crate) enum Held {
    /// No card has any of it.
    Not,
    /// Inside this one.
    By(Arc<Shadow>),
    /// It meets this one and is not inside it.
    Across(Arc<Shadow>),
}

/// Which card has `[address, end)`.
pub(crate) fn held(address: u64, end: u64) -> Held {
    // SAFETY: routes are never freed.
    match unsafe { ROUTES.load(Ordering::Acquire).as_ref() } {
        Some(routes) => held_in(routes, address, end),
        None => Held::Not,
    }
}

fn held_in(routes: &[Route], address: u64, end: u64) -> Held {
    let i = routes.partition_point(|r| r.from <= address);
    if let Some(r) = i.checked_sub(1).map(|k| &routes[k])
        && address < r.to
        && let Some(s) = r.shadow.upgrade()
    {
        return if end <= r.to { Held::By(s) } else { Held::Across(s) };
    }
    match routes.get(i) {
        Some(r) if r.from < end => r.shadow.upgrade().map_or(Held::Not, Held::Across),
        _ => Held::Not,
    }
}

/// The card `[address, end)` is inside, made now if it is to be one.
/// `Err`: no card has it, and its memory must not be used in place.
pub(crate) fn find(address: u64, end: u64) -> Result<Option<Arc<Shadow>>, ()> {
    match held(address, end) {
        Held::By(s) => Ok(Some(s)),
        Held::Not => Ok(heap(address, end)),
        Held::Across(s) => {
            across(&s, address, end);
            // Those that can be are gone now.
            match held(address, end) {
                Held::By(s) => Ok(Some(s)),
                Held::Not => Ok(None),
                Held::Across(_) => Err(()),
            }
        }
    }
}

/// The card `address` is in, for a view without a size, of a title buffer
/// that ends at `end` (if it is in one): whatever the view reaches, as far
/// as the card goes.
pub(crate) fn find_reaching(address: u64, end: Option<u64>) -> Option<Arc<Shadow>> {
    match find(address, end.unwrap_or(address + 1)) {
        Ok(card) => card,
        // The title's heap, which is never in place.
        Err(()) => match held(address, address + 1) {
            Held::By(s) => Some(s),
            _ => None,
        },
    }
}

/// A range that meets a card (`s`, the first of them) and is not inside it
/// was asked for. The title's buffers among such cards go back to their
/// place: what reaches them as part of something else reaches them there.
fn across(s: &Arc<Shadow>, address: u64, end: u64) {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let count = COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if count <= 64 || count.is_power_of_two() {
        let (from, to) = s.card().map_or((0, 0), |c| c.span());
        tracing::info!(
            count,
            address = format_args!("{address:#x}"),
            len = end - address,
            card = format_args!("{from:#x}..{to:#x}"),
            "title memory asked for goes past a card"
        );
    }
    let mut owned = OWNED.lock().unwrap_or_else(|e| e.into_inner());
    let over: Vec<u64> = owned
        .iter()
        .filter(|(_, s)| {
            s.card().is_some_and(|c| c.is_buffer() && c.span().0 < end && address < c.span().1)
        })
        .map(|(k, _)| *k)
        .collect();
    for key in over {
        if let Some(shadow) = owned.remove(&key) {
            big::never(&shadow);
            let_go(&owned, shadow);
        }
    }
}

/// The title's 32-bit GPU heap: not looked for yet, a card, or never one.
static HEAP: AtomicU8 = AtomicU8::new(0);
const HEAP_MADE: u8 = 1;
const HEAP_REFUSED: u8 = 2;

/// The card of the title's 32-bit GPU heap, if `[address, end)` lies in it:
/// the one view of 64 to 512 MiB below 4 GiB (the runtime shows Grand Theft
/// Auto VI's 185 MiB by one view, `xmem`'s `map_whole`). Made the first time
/// something asks for memory in it, before any of it was handed out in
/// place: pages the host GPU has open cannot be guarded.
fn heap(address: u64, end: u64) -> Option<Arc<Shadow>> {
    const LEAST: usize = 64 << 20;
    const MOST: usize = 512 << 20;
    if end > 1 << 32 || !heap_enabled() {
        return None;
    }
    match HEAP.load(Ordering::Acquire) {
        0 => {}
        // Made by another thread since this one looked.
        HEAP_MADE => {
            return match held(address, end) {
                Held::By(s) => Some(s),
                _ => None,
            };
        }
        _ => return None,
    }
    let (start, len, section, offset) = crate::watch::view_of(address as usize, end as usize)?;
    if !(LEAST..=MOST).contains(&len) || !len.is_multiple_of(PIECE) {
        return None;
    }
    let mut owned = OWNED.lock().unwrap_or_else(|e| e.into_inner());
    if HEAP.load(Ordering::Acquire) != 0 {
        return owned.get(&(start as u64)).cloned();
    }
    // Settled once and for all: a thread that meets the memory while this one
    // works waits for the lock above, and none uses it in place before.
    let made = crate::host::host()
        .filter(|h| h.card_ready())
        .and_then(|host| guarded(host, start, len, section, offset));
    match &made {
        Some(shadow) => {
            owned.insert(start as u64, shadow.clone());
            publish(&owned);
            HEAP.store(HEAP_MADE, Ordering::Release);
            tracing::info!(start = format_args!("{start:#x}"), len, "title memory on the card");
        }
        None => {
            HEAP.store(HEAP_REFUSED, Ordering::Release);
            tracing::warn!(start = format_args!("{start:#x}"), len, "the title's heap is not on the card");
        }
    }
    made
}

/// A card of the view of title memory `[start, start + len)`, which shows
/// the bytes of `section` from `offset` on.
fn guarded(host: &Host, start: usize, len: usize, section: usize, offset: u64) -> Option<Arc<Shadow>> {
    use windows_sys::Win32::System::Memory::{FILE_MAP_WRITE, MapViewOfFile};
    // SAFETY: a section the runtime told us of, alive while it has views;
    // the stretch is inside it.
    let view = unsafe {
        MapViewOfFile(section as _, FILE_MAP_WRITE, (offset >> 32) as u32, offset as u32, len)
    };
    if view.Value.is_null() {
        let error = std::io::Error::last_os_error();
        tracing::warn!(%error, start = format_args!("{start:#x}"), len, "no second view of title memory for a card");
        return None;
    }
    let back = view.Value as usize;
    crate::watch::give_up(start, len);
    let Some(t) = track::Track::new(start, len / PIECE) else {
        use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
        // SAFETY: the view just made.
        unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: back as *mut _ }) };
        return None;
    };
    let place = Place { back, own_view: true, told: Told::Guarded(t) };
    let card = Card::new(host, start, start + len, place)?;
    // Pages shown at other addresses already, and descriptors.
    for at in crate::watch::shown_twice(start, len) {
        track::shown_elsewhere(at);
    }
    for &(from, to) in DESCRIPTORS.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        track::skip(from, to, true);
    }
    Some(Shadow::on_card(card))
}

/// The title's descriptor heaps in memory of its own (from, to).
static DESCRIPTORS: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

/// A descriptor heap is in title memory `[from, to)`.
pub(crate) fn descriptors(from: u64, to: u64) {
    if !skip_descriptors() {
        return;
    }
    let (from, to) = (from as usize, to as usize);
    DESCRIPTORS.lock().unwrap_or_else(|e| e.into_inner()).push((from, to));
    track::skip(from, to, true);
}

/// It is gone.
pub(crate) fn descriptors_gone(from: u64, to: u64) {
    if !skip_descriptors() {
        return;
    }
    let (from, to) = (from as usize, to as usize);
    let mut all = DESCRIPTORS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(i) = all.iter().position(|d| *d == (from, to)) {
        all.swap_remove(i);
    }
    let left = all.clone();
    drop(all);
    track::skip(from, to, false);
    // Another heap over some of the same memory.
    for (a, b) in left.into_iter().filter(|(a, b)| *a < to && from < *b) {
        track::skip(a, b, true);
    }
}

/// The title mapped or unmapped memory in `[start, end)`: cards there are no
/// longer its.
pub(crate) fn remapped(start: u64, end: u64) {
    // SAFETY: routes are never freed.
    let routes = unsafe { ROUTES.load(Ordering::Acquire).as_ref() };
    if !routes.is_some_and(|r| r.iter().any(|r| r.from < end && start < r.to)) {
        return;
    }
    let mut owned = OWNED.lock().unwrap_or_else(|e| e.into_inner());
    let over: Vec<u64> = owned
        .iter()
        .filter(|(_, s)| s.card().is_some_and(|c| c.span().0 < end && start < c.span().1))
        .map(|(k, _)| *k)
        .collect();
    for key in over {
        let Some(shadow) = owned.remove(&key) else { continue };
        if let Some(card) = shadow.card() {
            card.gone.store(true, Ordering::Release);
        }
        big::never(&shadow);
        let_go(&owned, shadow);
        tracing::info!(start = format_args!("{key:#x}"), "title memory on the card was mapped anew");
    }
}

/// A card taken out of `owned` is handed out no more. Lists and slots that
/// name it keep it, and submissions look after it, until they go.
fn let_go(owned: &BTreeMap<u64, Arc<Shadow>>, shadow: Arc<Shadow>) {
    publish(owned);
    if let Some(card) = shadow.card() {
        card.dead.store(true, Ordering::Release);
        // What no list declared comes back with the next submission, all of
        // it: it is read in place from now on.
        card.sweep.lock().unwrap_or_else(|e| e.into_inner()).next = 0;
        moved(card.span());
    }
    crate::shadow::card_died(&shadow);
    crate::host::retire(shadow);
}

/// Title memory that came onto a card or off one, lately (from, to), and how
/// many times that happened: slots made for it are made again.
static MOVED: Mutex<(Vec<(u64, u64)>, u64)> = Mutex::new((Vec::new(), 0));
/// The second of [`MOVED`].
static MOVES: AtomicU64 = AtomicU64::new(0);
const MOVED_KEPT: usize = 256;

fn moved(span: (u64, u64)) {
    let mut m = MOVED.lock().unwrap_or_else(|e| e.into_inner());
    if m.0.len() == MOVED_KEPT {
        m.0.remove(0);
    }
    m.0.push(span);
    m.1 += 1;
    MOVES.store(m.1, Ordering::Release);
}

/// How often title memory came onto a card or off one: descriptor slots
/// made over it before are made again (`HostHeap::forget_moved`).
pub(crate) fn moves() -> u64 {
    MOVES.load(Ordering::Acquire)
}

/// What of it came or went since the first `seen` (`None`: not known any
/// more), and how often it happened.
pub(crate) fn moved_since(seen: u64) -> (Option<Vec<(u64, u64)>>, u64) {
    let m = MOVED.lock().unwrap_or_else(|e| e.into_inner());
    let n = (m.1 - seen) as usize;
    ((n <= m.0.len()).then(|| m.0[m.0.len() - n..].to_vec()), m.1)
}

/// The cards of buffers with bytes in `[from, to)`, alive.
fn buffers_over(from: usize, to: usize) -> Vec<Arc<Card>> {
    if moves() == 0 {
        return Vec::new();
    }
    let live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    live.iter()
        .filter_map(Weak::upgrade)
        .filter(|c| c.is_buffer() && c.from < to && from < c.to)
        .collect()
}

/// Title memory `[at, at + len)` is looked at with the next submission: the
/// host GPU wrote it in place, or the title committed it anew.
pub(crate) fn wrote_in_place(at: usize, len: usize) {
    for card in buffers_over(at, at.saturating_add(len)) {
        if let (Told::Written(w), Some((a, b))) = (&card.told, card.leaves_of(at, at.saturating_add(len))) {
            w.lock().unwrap_or_else(|e| e.into_inner()).force(a / LEAVES, b.div_ceil(LEAVES));
        }
    }
}

/// The kernel may write title memory `[at, at + len)` for a while.
pub(crate) fn kernel_will_write(at: usize, len: usize) {
    track::opening(at, len, true);
    for card in buffers_over(at, at.saturating_add(len)) {
        if let (Told::Written(w), Some((a, b))) = (&card.told, card.leaves_of(at, at.saturating_add(len))) {
            w.lock().unwrap_or_else(|e| e.into_inner()).pin(a / LEAVES, b.div_ceil(LEAVES));
        }
    }
}

/// Shaders reach title memory by its address ([`crate::memory`]), whose map
/// shows a window as it was when first reached, and whose writes no list
/// declares: the title's buffers stay in place from now on.
pub(crate) fn by_address() {
    static ONCE: AtomicBool = AtomicBool::new(false);
    if !ONCE.swap(true, Ordering::AcqRel) {
        big::stop();
    }
}

/// The memory of every card handed out.
pub(crate) fn all() -> Vec<Arc<Shadow>> {
    // SAFETY: routes are never freed.
    let routes = unsafe { ROUTES.load(Ordering::Acquire).as_ref() };
    routes.map_or(Vec::new(), |r| r.iter().filter_map(|r| r.shadow.upgrade()).collect())
}

/// For device-removed reports: the cards' host addresses, title addresses
/// and lengths.
pub(crate) fn places() -> Vec<(u64, u64, u64)> {
    let live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    live.iter()
        .filter_map(Weak::upgrade)
        .map(|c| (c.gpu, c.base as u64, c.size as u64))
        .collect()
}

#[cfg(test)]
mod tests;
