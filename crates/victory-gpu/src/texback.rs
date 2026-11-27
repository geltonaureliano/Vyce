//! Textures that shaders write and the title then reads on the CPU. On the
//! console the CPU and the GPU share memory: what a compute pass stores into
//! a texture is in title memory for the title's own code to read once the
//! submission's fence says so. Here a texture is a host copy, detiled from
//! title memory when the title writes it, and what shaders store stays in
//! that copy. The water's height and displacement maps are such textures:
//! the title's CPU code samples them (what boats float on), and read memory
//! nothing had written.
//!
//! For textures of the kinds in `VICTORY_TEXTURE_BACK` (comma-separated
//! `<width>x<height>f<format>`; unset or `1`: the four water kinds the
//! upstream trace found; `0`: none), a list whose shaders write one through
//! a descriptor table copies the host texture into a read-back buffer of
//! ours twice: just before the first draw or dispatch that may write it
//! ([`crate::host::HostList::texture_written`]), and when the list closes.
//! Once the submission has run, in its landing (after its shaders' buffer
//! writes and the title's read-backs, before its bottom-of-pipe writes and
//! the fences put behind it, `crate::flight`), the elements that differ
//! between the two copies are tiled into the texture's title memory in the
//! console's layout, as the console's GPU would have left them, and only
//! where title memory differs. So an element the title's CPU wrote and the
//! list's shaders did not keeps the title's value, and pages nothing changed
//! in are not written. The texture's own upload path does not take our write
//! for the title's ([`crate::texture::writing_back`]); other host copies of
//! the memory see it written, as they would see the console GPU's write.
//!
//! From github.com/avatarmanz/VIctory-7900XTX crates/victory-gpu/src/texback.rs
//! (23925a4, which takes fxceless/victory-fork f5c03f3b, pull request 999 by
//! Geckomacabre and cxy0072019-sketch) and the same file of
//! codeberg.org/p1nkman/p1nkman-VI (9aaa9ec): the kinds, the kind setting and
//! the tickets. GPL-3.0-or-later as they are. The copies before and after,
//! the compare and the landing are written for this host's model.
//!
//! shortcut: by kind, not by texture: textures of those kinds the title never
//! reads are copied back too (for the four water kinds about 1.7 MB read
//! back per list that writes all four). Writes through directly indexed
//! slots, render targets, copies and resolves into such a texture are not
//! copied back, nor those before the list's first draw or dispatch that may
//! write it, nor an element its shaders write with the value it held (title
//! memory keeps what it had there). A title read between the submission and
//! its fence sees what was there before, as it would before the console's
//! GPU has run.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::core::Interface;

use crate::object::This;

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

/// The kinds copied back when `VICTORY_TEXTURE_BACK` is unset or `1`, as
/// width x height and DXGI format: the water's height (R32_FLOAT) and
/// displacement (R32G32_FLOAT) maps and two smaller maps (R32G32_FLOAT,
/// R16G16_UINT), which the upstream read-watch trace found the title's CPU
/// code reading.
const DEFAULT: &str = "256x256f41,256x256f16,64x64f16,128x128f36";

/// A texture kind: width, height, DXGI format number.
pub(crate) type Kind = (u64, u32, u32);

/// The largest texture copied back, in title memory (the default kinds take
/// 512 KiB at most): a kind named by hand that matches big textures would
/// cost twice their size in read-back memory per list that writes one.
const MOST: u64 = 16 << 20;

/// The kinds `spec` names: unset or `1` the default ones, `0` (or nothing
/// that reads as a kind) none.
fn parse(spec: Option<&str>) -> Vec<Kind> {
    let spec = match spec.map(str::trim) {
        None | Some("1") => DEFAULT,
        Some("0") => "",
        Some(spec) => spec,
    };
    spec.split(',')
        .filter_map(|kind| {
            let (size, format) = kind.trim().split_once('f')?;
            let (width, height) = size.split_once('x')?;
            Some((width.trim().parse().ok()?, height.trim().parse().ok()?, format.trim().parse().ok()?))
        })
        .collect()
}

/// The kinds copied back (`VICTORY_TEXTURE_BACK`), read once.
fn kinds() -> &'static [Kind] {
    static KINDS: OnceLock<Vec<Kind>> = OnceLock::new();
    KINDS.get_or_init(|| {
        let spec = std::env::var("VICTORY_TEXTURE_BACK").ok();
        let kinds = parse(spec.as_deref());
        if kinds.is_empty() {
            tracing::info!(
                "textures shaders write are not copied back into title memory (VICTORY_TEXTURE_BACK)"
            );
        } else {
            tracing::info!(
                ?kinds,
                "textures of these kinds that shaders write are copied back into title memory \
                 (VICTORY_TEXTURE_BACK=0: none)"
            );
        }
        kinds
    })
}

/// Whether title texture `resource` is of a kind copied back.
pub(crate) fn wanted(resource: This) -> bool {
    let kinds = kinds();
    !kinds.is_empty() && crate::resource::texture_kind(resource).is_some_and(|k| kinds.contains(&k))
}

/// The title memory a list's copies of a texture go back to, while it is
/// that texture's: the title releasing the texture, or the runtime mapping
/// other memory there, ends it. A landing writes under its lock, so an end
/// waits for a write under way and nothing is written after it.
struct Ticket {
    /// The title's texture, by address.
    owner: usize,
    at: usize,
    end: usize,
    valid: Mutex<bool>,
}

static TICKETS: Mutex<Vec<Weak<Ticket>>> = Mutex::new(Vec::new());
/// Whether a ticket was ever made: releases and mappings look at none before.
static TRACKING: AtomicBool = AtomicBool::new(false);

fn track(owner: usize, at: usize, end: usize) -> Arc<Ticket> {
    let ticket = Arc::new(Ticket { owner, at, end, valid: Mutex::new(true) });
    let mut tickets = TICKETS.lock().unwrap_or_else(|e| e.into_inner());
    tickets.retain(|weak| weak.strong_count() != 0);
    tickets.push(Arc::downgrade(&ticket));
    TRACKING.store(true, Ordering::Release);
    ticket
}

/// Ends the tickets `matches` picks. Lock order: the list of tickets, then
/// a ticket (a landing takes only its ticket's).
fn invalidate(matches: impl Fn(&Ticket) -> bool) {
    if !TRACKING.load(Ordering::Acquire) {
        return;
    }
    TICKETS.lock().unwrap_or_else(|e| e.into_inner()).retain(|weak| {
        let Some(ticket) = weak.upgrade() else { return false };
        if matches(&ticket) {
            *ticket.valid.lock().unwrap_or_else(|e| e.into_inner()) = false;
        }
        true
    });
}

/// The title releases texture `owner`: what its lists copied goes nowhere.
pub(crate) fn released(owner: usize) {
    invalidate(|ticket| ticket.owner == owner);
}

/// The runtime maps or unmaps title memory `[at, at + len)`: copies meant
/// for what was there go nowhere.
pub(crate) fn remapped(at: usize, len: usize) {
    let end = at.saturating_add(len);
    invalidate(|ticket| at < ticket.end && ticket.at < end);
}

/// A read-back buffer of ours, mapped for good, on the device whose address
/// is `device`; kept for another list once the one that used it is reset.
struct Kept {
    device: usize,
    size: u64,
    buffer: ID3D12Resource,
    data: *const u8,
}

// SAFETY: a D3D12 resource (free-threaded) and its mapping, read by one
// landing at a time while it is a list's.
unsafe impl Send for Kept {}

/// Read-back buffers no list holds.
static POOL: Mutex<Vec<Kept>> = Mutex::new(Vec::new());
/// At most this many are kept (with the default kinds 1 MiB each at most).
const POOL_MOST: usize = 16;

/// A read-back buffer of at least `size` bytes on `device`: one kept that is
/// not more than twice as large, or a new one.
fn take(device: &ID3D12Device, size: u64) -> Option<Kept> {
    let key = device.as_raw() as usize;
    {
        let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = pool
            .iter()
            .position(|k| k.device == key && k.size >= size && k.size <= size.saturating_mul(2))
        {
            return Some(pool.swap_remove(i));
        }
    }
    let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size.max(1),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    let mut data = std::ptr::null_mut();
    // SAFETY: plain creation and mapping of a read-back buffer; the out
    // pointers are ours.
    let made = unsafe {
        device
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
        Ok(()) if !data.is_null() => {
            let buffer = buffer?;
            crate::memstat::track(&buffer, 1, size);
            crate::host::name(&buffer, || format!("texture copy back {size:#x}"));
            Some(Kept { device: key, size, buffer, data: data.cast_const().cast() })
        }
        Ok(()) => None,
        Err(e) => {
            once!(error = %e, size, "no read-back buffer for a texture copied back; not copied back");
            None
        }
    }
}

/// A list's two copies of a texture its shaders may write, and where they
/// go in title memory.
pub(crate) struct Back {
    /// The title's texture (its address), which the list looks it up by.
    owner: usize,
    /// Its host copy, alive as long as this is.
    pub(crate) copy: Arc<crate::resource::HostTexture>,
    /// Its console layout (one plane) and the title memory it lies in.
    layout: victory_formats::texture::Layout,
    ticket: Arc<Ticket>,
    /// The copy before the first write in its first half, the copy when the
    /// list closes `half` bytes on; given back to [`POOL`] when this goes.
    buffer: Option<Kept>,
    half: u64,
    /// Each subresource's place in a half, and its rows.
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    mips: u32,
}

// SAFETY: D3D12 resources are free-threaded; the mapped read-back memory is
// read only by a landing, after the host GPU has written it, and the rest is
// plain data.
unsafe impl Send for Back {}
// SAFETY: as above.
unsafe impl Sync for Back {}

impl Drop for Back {
    fn drop(&mut self) {
        // The list holding this was reset or let go of: no submission of it
        // is to run, nor to land.
        if let Some(kept) = self.buffer.take() {
            let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
            if pool.len() < POOL_MOST {
                pool.push(kept);
            }
        }
    }
}

impl Back {
    /// The copies of title texture `resource` for a list recorded on
    /// `device`; `None` if it cannot be copied back (logged once).
    pub(crate) fn new(device: &ID3D12Device, resource: This) -> Option<Back> {
        let copy = crate::resource::host_copy(resource)?;
        let (layout, at) = crate::resource::title_texture(resource)?;
        Back::make(device, resource as usize, copy, layout, at)
    }

    /// The copies of host texture `copy`, whose console layout is `layout`
    /// at title memory `at`, for the title's texture `owner`.
    pub(crate) fn make(
        device: &ID3D12Device,
        owner: usize,
        copy: Arc<crate::resource::HostTexture>,
        layout: victory_formats::texture::Layout,
        at: usize,
    ) -> Option<Back> {
        if layout.size > MOST {
            once!(desc = ?layout.desc, "a texture of a kind copied back is too big to be copied back");
            return None;
        }
        let size = usize::try_from(layout.size).ok()?;
        let end = at.checked_add(size).filter(|_| at != 0 && size != 0)?;
        if copy.skip != 0 {
            once!(desc = ?layout.desc, "a texture of a kind copied back lacks its largest mips on the host");
            return None;
        }
        if !layout.addressable() {
            once!(desc = ?layout.desc, "a texture of a kind copied back has a layout the copy cannot address");
            return None;
        }
        // SAFETY: a live texture.
        let desc = unsafe { copy.texture.GetDesc() };
        let mips = u32::from(desc.MipLevels.max(1));
        let slices = u32::from(desc.DepthOrArraySize.max(1));
        let same_shape = desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D
            && desc.SampleDesc.Count <= 1
            && mips == layout.desc.mips
            && slices == layout.desc.depth
            && crate::resource::element(desc.Format.0 as u32)
                == Some((u64::from(layout.desc.element_bytes), u64::from(layout.desc.block)));
        if !same_shape {
            once!(desc = ?layout.desc, host = ?desc, "a texture of a kind copied back has another shape on the host");
            return None;
        }
        let n = (mips * slices) as usize;
        let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); n];
        let mut rows = vec![0u32; n];
        let mut total = 0u64;
        // SAFETY: out arrays of `n` entries.
        unsafe {
            device.GetCopyableFootprints(
                &desc,
                0,
                n as u32,
                0,
                Some(footprints.as_mut_ptr()),
                Some(rows.as_mut_ptr()),
                None,
                Some(&mut total),
            )
        };
        // The last subresource's advertised end can leave out its last row's
        // padding; the tiler reads whole pitched rows.
        for (footprint, &rows) in footprints.iter().zip(&rows) {
            let f = footprint.Footprint;
            let pitched =
                u64::from(f.RowPitch).checked_mul(u64::from(rows))?.checked_mul(u64::from(f.Depth))?;
            total = total.max(footprint.Offset.checked_add(pitched)?);
        }
        let half = total.checked_next_multiple_of(u64::from(D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT))?;
        // (Rows the host pads take more than the title's layout, never twice.)
        if half > 2 * MOST {
            return None;
        }
        let buffer = take(device, 2 * half)?;
        let ticket = track(owner, at, end);
        Some(Back { owner, copy, layout, ticket, buffer: Some(buffer), half, footprints, rows, mips })
    }

    /// The title's texture this copies.
    pub(crate) fn owner(&self) -> usize {
        self.owner
    }

    /// The title memory the landing may write: start, end.
    pub(crate) fn range(&self) -> (usize, usize) {
        (self.ticket.at, self.ticket.end)
    }

    /// Records the copy of every subresource of the host texture, which is in
    /// the copy-source state, into the buffer's first half, or with `after`
    /// its second.
    pub(crate) fn record(&self, list: &ID3D12GraphicsCommandList, after: bool) {
        let Some(kept) = &self.buffer else { return };
        let base = if after { self.half } else { 0 };
        for (i, footprint) in self.footprints.iter().enumerate() {
            let mut placed = *footprint;
            placed.Offset += base;
            let to = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: a borrowed pointer the location never releases.
                pResource: unsafe { std::mem::transmute_copy(&kept.buffer) },
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: placed },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: as above.
                pResource: unsafe { std::mem::transmute_copy(&self.copy.texture) },
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: i as u32 },
            };
            // SAFETY: an open list; the texture is in the copy-source state and
            // the buffer, in the copy-destination state of its heap, has room
            // for both halves (`make`).
            unsafe { list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
        }
    }
}

/// Since the last [`take_counts`]: textures landed, bytes they wrote into
/// title memory, bytes of their copies read back, and copies that did not
/// land (their memory changed hands, could not be read, or did not tile).
static COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// [`COUNTS`] since the last call.
pub(crate) fn take_counts() -> [u64; 4] {
    [0, 1, 2, 3].map(|i| COUNTS[i].swap(0, Ordering::Relaxed))
}

thread_local! {
    /// Title memory, and it with the copies before and after tiled in: one
    /// per landing thread, as large as the largest texture it landed.
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Once the submission whose lists made `backs` (in their order) has run:
/// what its shaders changed goes into title memory. The bytes written.
pub(crate) fn land(backs: &[Arc<Back>]) -> usize {
    backs.iter().map(|back| land_one(back)).sum()
}

fn not_landed(why: &str) {
    static NOT: AtomicU64 = AtomicU64::new(0);
    COUNTS[3].fetch_add(1, Ordering::Relaxed);
    let n = NOT.fetch_add(1, Ordering::Relaxed) + 1;
    if n.is_power_of_two() {
        tracing::info!(n, why, "a texture copied back did not land");
    }
}

fn land_one(back: &Back) -> usize {
    let (at, end) = back.range();
    let size = end - at;
    SCRATCH.with_borrow_mut(|scratch| {
        if scratch.len() < 3 * size {
            scratch.resize(3 * size, 0);
        }
        let (title, rest) = scratch[..3 * size].split_at_mut(size);
        let (before, after) = rest.split_at_mut(size);
        let written = land_in(back, title, before, after);
        // A texture far bigger than the default kinds' is a one-off: its
        // room is commit charge.
        if scratch.capacity() > 8 << 20 {
            *scratch = Vec::new();
        }
        written
    })
}

/// [`land_one`] with room for the texture's title memory three times.
fn land_in(back: &Back, title: &mut [u8], before: &mut [u8], after: &mut [u8]) -> usize {
    let Some(kept) = &back.buffer else { return 0 };
    let (at, size) = (back.range().0, title.len());
    // Held to the end of the write: the texture's memory stays its own.
    let valid = back.ticket.valid.lock().unwrap_or_else(|e| e.into_inner());
    if !*valid {
        not_landed("its texture was released or its memory mapped anew");
        return 0;
    }
    if !crate::host::read_into(at as *const u8, title) {
        not_landed("its title memory is not all there");
        return 0;
    }
    before.copy_from_slice(title);
    after.copy_from_slice(title);
    let mut read = 0;
    for (i, (footprint, &rows)) in back.footprints.iter().zip(&back.rows).enumerate() {
        let (mip, slice) = (i as u32 % back.mips, i as u32 / back.mips);
        let f = footprint.Footprint;
        let pitch = f.RowPitch as usize;
        let len = pitch * rows as usize * f.Depth as usize;
        let first = footprint.Offset as usize;
        let second = first + back.half as usize;
        if second + len > kept.size as usize {
            not_landed("its copy does not fit its buffer");
            return 0;
        }
        // SAFETY: both halves of our mapped buffer, inside it (checked
        // above), which the submission's lists wrote before it landed.
        let (was, now) = unsafe {
            (
                std::slice::from_raw_parts(kept.data.add(first), len),
                std::slice::from_raw_parts(kept.data.add(second), len),
            )
        };
        let depth_pitch = pitch * rows as usize;
        let tiled = back
            .layout
            .tile(before, mip, slice, was, pitch, depth_pitch)
            .and_then(|()| back.layout.tile(after, mip, slice, now, pitch, depth_pitch));
        if let Err(e) = tiled {
            once!(?e, mip, slice, desc = ?back.layout.desc, "a texture copied back did not tile");
            not_landed("it did not tile");
            return 0;
        }
        read += 2 * len;
    }
    COUNTS[2].fetch_add(read as u64, Ordering::Relaxed);
    let runs = changed(title, before, after, back.layout.desc.element_bytes as usize);
    let written: usize = runs.iter().map(|&(from, to)| to - from).sum();
    if let (Some(&(first, _)), Some(&(_, last))) = (runs.first(), runs.last()) {
        crate::texture::writing_back(back.owner, || {
            // Guarded blocks are opened once, not a fault at a time.
            crate::watch::opening(at + first, last - first);
            for &(from, to) in &runs {
                crate::host::write_mapped((at + from) as *mut u8, after[from..to].as_ptr(), to - from);
            }
        });
    }
    drop(valid);
    COUNTS[0].fetch_add(1, Ordering::Relaxed);
    COUNTS[1].fetch_add(written as u64, Ordering::Relaxed);
    static FIRST: std::sync::Once = std::sync::Once::new();
    FIRST.call_once(|| tracing::info!(bytes = written, size, "the first texture copied back landed"));
    written
}

/// The runs `[from, to)` of whole elements of `element` bytes that the
/// list's shaders changed (`after` differs from `before`) and that title
/// memory (`title`) does not hold already. All three are the texture's
/// title memory, `before` and `after` with the copies tiled in.
fn changed(title: &[u8], before: &[u8], after: &[u8], element: usize) -> Vec<(usize, usize)> {
    let len = title.len().min(before.len()).min(after.len());
    let len = len - len % element.max(1);
    let element = element.max(1);
    // Blocks nothing is written in are skipped whole.
    let block = 256usize.next_multiple_of(element);
    let mut runs = Vec::new();
    let mut open: Option<usize> = None;
    let mut close = |open: &mut Option<usize>, at: usize| {
        if let Some(from) = open.take() {
            runs.push((from, at));
        }
    };
    let mut at = 0;
    while at < len {
        let stop = (at + block).min(len);
        let quiet = after[at..stop] == before[at..stop] || after[at..stop] == title[at..stop];
        if quiet {
            close(&mut open, at);
            at = stop;
            continue;
        }
        for e in (at..stop).step_by(element) {
            let r = e..e + element;
            let write = after[r.clone()] != before[r.clone()] && after[r.clone()] != title[r];
            match (write, open) {
                (true, None) => open = Some(e),
                (false, Some(_)) => close(&mut open, e),
                _ => {}
            }
        }
        at = stop;
    }
    close(&mut open, len);
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flight::After;
    use crate::host::{Host, HostList};
    use crate::resource::HostTexture;
    use crate::telemetry::Count;
    use std::sync::atomic::AtomicU32;
    use std::time::{Duration, Instant};
    use victory_formats::texture::{Dimension, Layout, Swizzle, Texture, TextureDesc};
    use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
    use windows::Win32::Graphics::Direct3D::ID3DBlob;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::core::s;

    /// The tests that look at [`COUNTS`] or the default kinds, one at a time.
    static ONE: Mutex<()> = Mutex::new(());

    fn one() -> std::sync::MutexGuard<'static, ()> {
        ONE.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn kinds_read_as_the_setting_says() {
        let default = parse(None);
        assert_eq!(default, vec![(256, 256, 41), (256, 256, 16), (64, 64, 16), (128, 128, 36)]);
        assert_eq!(parse(Some("1")), default);
        assert_eq!(parse(Some(" 1 ")), default);
        assert!(parse(Some("0")).is_empty());
        assert!(parse(Some("")).is_empty());
        assert!(parse(Some("off")).is_empty(), "nothing that reads as a kind");
        assert_eq!(parse(Some("32x16f28, 8x8f41,bad,4x4fx")), vec![(32, 16, 28), (8, 8, 41)]);
    }

    #[test]
    fn only_copies_whose_memory_changed_hands_are_ended() {
        let valid = |t: &Arc<Ticket>| *t.valid.lock().unwrap();
        let first = track(91, 0x1000, 0x2000);
        let next = track(92, 0x2000, 0x3000);
        let far = track(93, 0x4000, 0x5000);
        remapped(0x0f00, 0x200);
        assert!(!valid(&first));
        assert!(valid(&next) && valid(&far));
        remapped(0x3000, 0x1000);
        assert!(valid(&next) && valid(&far), "ranges that only touch stay");
        released(92);
        assert!(!valid(&next));
        assert!(valid(&far));
        released(93);
        assert!(!valid(&far));
        assert!(valid(&track(94, 0x2000, 0x3000)), "a new texture there is copied back");
    }

    /// Elements the shaders changed and title memory lacks are written, in
    /// as few runs as they make; an element they left alone keeps what the
    /// title wrote there.
    #[test]
    fn only_what_the_shaders_changed_is_written() {
        let element = 8;
        let len = 2048;
        let title: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let mut before = title.clone();
        let mut after = title.clone();
        // Shaders changed elements 1..3 and 40; element 41 they wrote with
        // what title memory already holds; element 70 differs from title
        // memory (the title wrote it) but not between the copies.
        for e in [1, 2, 40, 41] {
            for b in &mut after[e * element..(e + 1) * element] {
                *b = b.wrapping_add(1);
            }
        }
        before[41 * element..42 * element].copy_from_slice(&title[41 * element..42 * element]);
        let mut title_now = title.clone();
        title_now[41 * element..42 * element].copy_from_slice(&after[41 * element..42 * element]);
        title_now[70 * element] ^= 0xff;
        before[70 * element] = title[70 * element];
        after[70 * element] = title[70 * element];
        assert_eq!(changed(&title_now, &before, &after, element), vec![(8, 24), (320, 328)]);
        // Nothing changed: nothing at all.
        assert!(changed(&title, &title, &title, element).is_empty());
        // Everything changed: one run.
        let all: Vec<u8> = title.iter().map(|b| b ^ 1).collect();
        assert_eq!(changed(&title, &title, &all, element), vec![(0, len)]);
        // A byte inside an element writes the whole element.
        let mut one = title.clone();
        one[1000] ^= 0x10;
        assert_eq!(changed(&title, &title, &one, 4), vec![(1000, 1004)]);
    }

    /// The console layout of a `width` x `height` R32G32_FLOAT texture with
    /// `mips` levels, as a texture with unordered access but no target is
    /// laid out (4 KiB standard blocks).
    fn console(width: u32, height: u32, mips: u32) -> Layout {
        let t = Texture::new(&TextureDesc {
            dimension: Dimension::Tex2d,
            width,
            height,
            depth_or_array: 1,
            mips,
            format: DXGI_FORMAT_R32G32_FLOAT.0 as u32,
            samples: 1,
            swizzle: Swizzle::SW_4KB_S,
        })
        .unwrap();
        t.planes[0].layout.clone()
    }

    /// A host texture shaders may write, as the title's are made.
    fn uav_texture(host: &Host, width: u32, height: u32, mips: u16) -> Arc<HostTexture> {
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: u64::from(width),
            Height: height,
            DepthOrArraySize: 1,
            MipLevels: mips,
            Format: DXGI_FORMAT_R32G32_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut texture: Option<ID3D12Resource> = None;
        // SAFETY: plain creation on the test's device.
        unsafe {
            host.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut texture,
                )
                .unwrap();
        }
        Arc::new(HostTexture {
            texture: texture.unwrap(),
            state: AtomicU32::new(D3D12_RESOURCE_STATE_COMMON.0 as u32),
            initialized: Default::default(),
            bytes: 0,
            slot: None,
            skip: 0,
            shared_exponent: false,
            written: Default::default(),
            twin: Default::default(),
        })
    }

    /// The value the test's shader writes into element (`x`, `y`) of mip 0
    /// in its pass `salt`.
    fn texel(x: u32, y: u32, salt: u32) -> [u8; 8] {
        let mut v = [0u8; 8];
        v[..4].copy_from_slice(&((x + y * 1000 + salt * 100_000) as f32).to_le_bytes());
        v[4..].copy_from_slice(&(salt as f32).to_le_bytes());
        v
    }

    const SHADER: &str = r#"
#define RS "DescriptorTable(UAV(u0)), RootConstants(num32BitConstants=2, b0)"
RWTexture2D<float2> target : register(u0);
cbuffer Settings : register(b0) { uint salt; uint columns; };
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= columns) return;
    target[id.xy] = float2(id.x + id.y * 1000 + salt * 100000, salt);
}
"#;

    /// The test's pass: its root signature and pipeline.
    fn pass(device: &ID3D12Device) -> (ID3D12RootSignature, ID3D12PipelineState) {
        let mut code: Option<ID3DBlob> = None;
        let mut errors: Option<ID3DBlob> = None;
        // SAFETY: the source and names live through the call; out pointers ours.
        let compiled = unsafe {
            D3DCompile(
                SHADER.as_ptr().cast(),
                SHADER.len(),
                s!("texback"),
                None,
                None,
                s!("main"),
                s!("cs_5_1"),
                0,
                0,
                &mut code,
                Some(&mut errors),
            )
        };
        let bytes = |b: &ID3DBlob| {
            // SAFETY: the blob's own bytes.
            unsafe { std::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize()) }.to_vec()
        };
        if compiled.is_err() {
            let text = errors.map(|e| String::from_utf8_lossy(&bytes(&e)).into_owned());
            panic!("{}", text.unwrap_or_default());
        }
        let code = bytes(&code.unwrap());
        // SAFETY: the root signature is inside the shader; plain creations.
        unsafe {
            let root: ID3D12RootSignature = device.CreateRootSignature(0, &code).unwrap();
            let pso = device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: code.as_ptr().cast(),
                        BytecodeLength: code.len(),
                    },
                    ..Default::default()
                })
                .unwrap();
            (root, pso)
        }
    }

    /// Records into `list` the test's pass over mip 0 of `texture`, which
    /// is in the unordered-access state there: `columns` columns of
    /// `width` x `height` texels get [`texel`] of `salt`.
    fn write_pattern(list: &HostList, texture: &ID3D12Resource, size: (u32, u32), columns: u32, salt: u32) {
        let mut device: Option<ID3D12Device> = None;
        // SAFETY: a plain query of the list's device.
        unsafe { list.list.GetDevice(&mut device).unwrap() };
        let device = device.unwrap();
        let (root, pso) = pass(&device);
        // SAFETY: plain creations, and recording into an open list of objects
        // it keeps until it is reset.
        unsafe {
            let heap: ID3D12DescriptorHeap = device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: 1,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .unwrap();
            let view = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                Format: DXGI_FORMAT_R32G32_FLOAT,
                ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
                Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                    Texture2D: D3D12_TEX2D_UAV { MipSlice: 0, PlaneSlice: 0 },
                },
            };
            device.CreateUnorderedAccessView(texture, None, Some(&view), heap.GetCPUDescriptorHandleForHeapStart());
            let l = &list.list;
            l.SetDescriptorHeaps(&[Some(heap.clone())]);
            l.SetComputeRootSignature(&root);
            l.SetPipelineState(&pso);
            l.SetComputeRootDescriptorTable(0, heap.GetGPUDescriptorHandleForHeapStart());
            let constants = [salt, columns];
            l.SetComputeRoot32BitConstants(1, 2, constants.as_ptr().cast(), 0);
            l.Dispatch(size.0.div_ceil(8), size.1.div_ceil(8), 1);
            list.keep(&heap);
            list.keep(&root);
            list.keep(&pso);
        }
        list.count_work();
    }

    /// Title memory with `layout`'s bytes `fill`, whose mip 0 elements of the
    /// first `columns` columns are then [`texel`] of `salt`; every element
    /// placed by `Layout::offset`, not by the tiler the landing uses.
    fn expected(layout: &Layout, fill: &[u8], columns: u32, salt: u32) -> Vec<u8> {
        let mut memory = fill.to_vec();
        let m = &layout.mips[0];
        for y in 0..m.height {
            for x in 0..columns.min(m.width) {
                let at = layout.offset(0, x, y, 0).unwrap() as usize;
                memory[at..at + 8].copy_from_slice(&texel(x, y, salt));
            }
        }
        memory
    }

    /// A software host of the test's own, whose title submissions fly.
    fn flying() -> Option<&'static Host> {
        if !crate::flight::late_signal() {
            return None;
        }
        Some(Host::new_warp()?.overlapping())
    }

    /// A fence of the title's (its address), and its completed value.
    fn fence() -> usize {
        let mut f = std::ptr::null();
        let iid = &crate::fence::FENCE_IIDS[0];
        assert_eq!(crate::fence::create(std::ptr::null(), 0, 0, iid, &mut f), 0);
        f as usize
    }

    fn completed(fence: usize) -> u64 {
        crate::object::slot::<extern "system" fn(This) -> u64>(fence as This, 0x40)(fence as This)
    }

    fn until(what: impl Fn() -> bool) {
        let started = Instant::now();
        while !what() {
            assert!(started.elapsed() < Duration::from_secs(30), "never came");
            std::thread::yield_now();
        }
    }

    /// A pass writes a texture of a kind copied back; its submission flies
    /// behind a gate with the title's fence signalled behind it. Nothing
    /// reaches title memory before the host GPU has run it, and by the time
    /// anyone sees the fence the texels the pass wrote are there in the
    /// console's layout, exactly, and nothing else of the texture's memory
    /// changed (the mip the pass leaves alone, the padding). A second pass
    /// that writes half the columns lands only those: an element the title
    /// wrote meanwhile in the other half keeps the title's value, one it
    /// wrote in this half gets the pass's. On WARP and on the machine's own
    /// GPU.
    #[test]
    fn a_written_texture_is_in_title_memory_before_its_fence_is_seen() {
        let _one = one();
        if let Some(host) = flying() {
            lands_before_its_fence(host);
        }
        if crate::flight::late_signal()
            && let Some(host) = Host::new_hardware()
        {
            lands_before_its_fence(host.overlapping());
        }
    }

    /// Opens a gate of the host queue, at the end of its scope at the latest.
    struct Gate(ID3D12Fence);

    impl Drop for Gate {
        fn drop(&mut self) {
            // SAFETY: our fence.
            unsafe { self.0.Signal(1).unwrap() };
        }
    }

    fn lands_before_its_fence(host: &'static Host) {
        const UNORDERED: u32 = 0x8;
        let (width, height) = (64u32, 64u32);
        let layout = console(width, height, 2);
        let copy = uav_texture(host, width, height, 2);
        let fill = vec![0xa5u8; layout.size as usize];
        // Title memory, as long as the copies may write it.
        let at = Box::leak(fill.clone().into_boxed_slice()).as_mut_ptr() as usize;
        let read = || {
            let mut now = vec![0u8; fill.len()];
            assert!(crate::host::read_into(at as *const u8, &mut now));
            now
        };
        let title_writes = |offset: usize, value: u8| {
            // SAFETY: inside the leaked title memory; nothing of ours writes
            // it while no submission is in flight.
            unsafe { std::ptr::write_bytes((at + offset) as *mut u8, value, 8) };
        };
        let line = host.line();
        // A list whose pass writes the first `columns` columns with `salt`,
        // sent, and a title fence signalled behind it.
        let pass = |columns: u32, salt: u32| {
            let list = host.new_list().unwrap();
            let mut device: Option<ID3D12Device> = None;
            // SAFETY: a plain query of the list's device.
            unsafe { list.list.GetDevice(&mut device).unwrap() };
            let back = Back::make(&device.unwrap(), at, copy.clone(), layout.clone(), at).expect("copies back");
            list.texture_back(Arc::new(back));
            list.settle(&copy, UNORDERED);
            write_pattern(&list, &copy.texture, (width, height), columns, salt);
            list.close();
            let list = Arc::new(list);
            host.execute_title(&[list.clone()]);
            let f = fence();
            // As a queue's signal goes behind what is in flight.
            line.after(After::Signal(crate::fence::Held::new(f as This), 1)).unwrap_or_else(After::run);
            (list, f)
        };

        // The first pass writes every column of mip 0, behind a gate.
        let gate = Gate(host.gate());
        let (first, f) = pass(width, 1);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(completed(f), 0, "told before it ran");
        assert!(read() == fill, "title memory written before the host GPU ran");
        let want = expected(&layout, &fill, width, 1);
        let seen = std::thread::scope(|s| {
            let watcher = s.spawn(|| {
                until(|| completed(f) >= 1);
                read()
            });
            drop(gate);
            watcher.join().unwrap()
        });
        assert!(seen == want, "the fence came before the texels, or they are not the pass's");
        line.drain(Count::WaitList);
        drop(first);

        // The title writes an element in each half; the second pass writes
        // the left half anew.
        let left = layout.offset(0, 5, 5, 0).unwrap() as usize;
        let right = layout.offset(0, 40, 7, 0).unwrap() as usize;
        title_writes(left, 7);
        title_writes(right, 9);
        let (second, g) = pass(width / 2, 2);
        until(|| completed(g) >= 1);
        let mut want = expected(&layout, &fill, width, 1);
        let half = expected(&layout, &fill, width / 2, 2);
        for x in 0..width / 2 {
            for y in 0..height {
                let e = layout.offset(0, x, y, 0).unwrap() as usize;
                want[e..e + 8].copy_from_slice(&half[e..e + 8]);
            }
        }
        want[right..right + 8].copy_from_slice(&[9; 8]);
        assert!(read() == want, "the second pass's texels, the title's element in the other half");
        line.drain(Count::WaitList);
        drop(second);
    }

    /// Of title textures shaders write, only those of a kind copied back
    /// get copies; one that is not keeps its title memory as it was, and
    /// the one that is gets its texels, through the title's own objects.
    #[test]
    fn only_textures_of_the_kinds_are_copied_back() {
        let _one = one();
        if std::env::var_os("VICTORY_TEXTURE_BACK").is_some() {
            return; // the kinds are the setting's
        }
        let Some(host) = crate::host::host() else { return };
        const UNORDERED: u32 = 0x8;
        let texture = |width: u32| {
            let format = DXGI_FORMAT_R32G32_FLOAT.0 as u32;
            let resource = crate::resource::test_texture(u64::from(width), width, format, UNORDERED_FLAG);
            let (layout, at) = crate::resource::title_texture(resource).unwrap();
            (resource, layout, at)
        };
        let read = |at: usize, layout: &Layout| {
            let mut now = vec![0u8; layout.size as usize];
            assert!(crate::host::read_into(at as *const u8, &mut now));
            now
        };
        // 64x64 is a kind copied back, 128x128 of the same format is not.
        let (wanted_one, layout, at) = texture(64);
        let (other, other_layout, other_at) = texture(128);
        assert!(wanted(wanted_one) && !wanted(other));
        // SAFETY: the title memory of the first texture, as made.
        unsafe { std::ptr::write_bytes(at as *mut u8, 0x3c, layout.size as usize) };
        let untouched = read(other_at, &other_layout);
        let _ = take_counts();
        let list = host.new_list().unwrap();
        for (resource, width) in [(wanted_one, 64), (other, 128)] {
            list.texture_written(resource);
            crate::list::ensure_state(&list, resource, UNORDERED);
            write_pattern(&list, crate::resource::host_resource(resource).unwrap(), (width, width), width, 3);
        }
        assert_eq!(list.texture_backs().len(), 1, "copies for the kind copied back only");
        list.close();
        host.execute(&[&list]);
        assert!(read(at, &layout) == expected(&layout, &vec![0x3c; layout.size as usize], 64, 3));
        assert!(read(other_at, &other_layout) == untouched, "a texture of another kind was written");
        let [landed, written, back, missed] = take_counts();
        assert_eq!((landed, missed), (1, 0));
        assert_eq!(written, 64 * 64 * 8, "every element of mip 0, nothing else");
        assert!(back >= 2 * 64 * 64 * 8);
        drop(list);
        for resource in [wanted_one, other] {
            crate::object::slot::<extern "system" fn(This) -> u32>(resource, 0x10)(resource);
        }
    }

    /// The console's flag for unordered access in a resource description.
    const UNORDERED_FLAG: u32 = 4;

    /// A recording whose commands do not run (its tables were lost,
    /// `HostList::skipped`) copies nothing back: its copies did not run
    /// either.
    #[test]
    fn a_list_that_does_not_run_copies_nothing_back() {
        let _one = one();
        if std::env::var_os("VICTORY_TEXTURE_BACK").is_some() {
            return;
        }
        let Some(host) = crate::host::host() else { return };
        const UNORDERED: u32 = 0x8;
        let resource = crate::resource::test_texture(64, 64, DXGI_FORMAT_R32G32_FLOAT.0 as u32, UNORDERED_FLAG);
        let (layout, at) = crate::resource::title_texture(resource).unwrap();
        let read = || {
            let mut now = vec![0u8; layout.size as usize];
            assert!(crate::host::read_into(at as *const u8, &mut now));
            now
        };
        let before = read();
        let list = host.new_list().unwrap();
        list.texture_written(resource);
        crate::list::ensure_state(&list, resource, UNORDERED);
        write_pattern(&list, crate::resource::host_resource(resource).unwrap(), (64, 64), 64, 5);
        list.close();
        list.lose();
        assert!(list.skipped());
        let _ = take_counts();
        host.execute(&[&list]);
        assert_eq!(take_counts(), [0; 4]);
        assert!(read() == before, "a list that did not run wrote title memory");
        drop(list);
        crate::object::slot::<extern "system" fn(This) -> u32>(resource, 0x10)(resource);
    }

    /// A texture released after its list was recorded gets nothing written
    /// into what was its memory.
    #[test]
    fn a_released_texture_gets_nothing() {
        let _one = one();
        if std::env::var_os("VICTORY_TEXTURE_BACK").is_some() {
            return;
        }
        let Some(host) = crate::host::host() else { return };
        const UNORDERED: u32 = 0x8;
        let resource = crate::resource::test_texture(64, 64, DXGI_FORMAT_R32G32_FLOAT.0 as u32, UNORDERED_FLAG);
        let list = host.new_list().unwrap();
        list.texture_written(resource);
        crate::list::ensure_state(&list, resource, UNORDERED);
        write_pattern(&list, crate::resource::host_resource(resource).unwrap(), (64, 64), 64, 4);
        list.close();
        let _ = take_counts();
        crate::object::slot::<extern "system" fn(This) -> u32>(resource, 0x10)(resource);
        host.execute(&[&list]);
        let [landed, written, _, missed] = take_counts();
        assert_eq!((landed, written, missed), (0, 0, 1));
    }
}
