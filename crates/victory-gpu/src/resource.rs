//! Heaps, committed and placed resources, and resource sizes.
//!
//! Heaps and resources are host memory and a resource's GPU address is its
//! CPU address, as on the console; textures also get a host copy, and
//! buffers a host copy when a draw reads them (`shadow`).
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::ffi::c_void;
use std::ptr::NonNull;

use victory_formats::texture;

use crate::object::{
    self, E_INVALIDARG, E_OUTOFMEMORY, HEAP, HRESULT, Iid, Object, RESOURCE, S_OK, Table, This,
    guid,
};

/// The interfaces every device child also answers for.
pub(crate) const PAGEABLE_IIDS: [Iid; 3] = [
    guid(
        0x63ee58fb,
        0x1268,
        0x4835,
        [0x86, 0xda, 0xf0, 0x08, 0xce, 0x62, 0xf0, 0xd6],
    ),
    guid(
        0x905db94b,
        0xa00c,
        0x4140,
        [0x9d, 0xf5, 0x2b, 0x64, 0xca, 0x9e, 0xa3, 0x57],
    ),
    guid(
        0xc4fec28f,
        0x7966,
        0x4e95,
        [0x9f, 0x94, 0xf4, 0x31, 0xcb, 0x56, 0xc3, 0xb8],
    ),
];
const HEAP_IIDS: [Iid; 4] = [
    guid(
        0x6b3b2502,
        0x6e51,
        0x45b3,
        [0x90, 0xee, 0x98, 0x84, 0x26, 0x5e, 0x8d, 0xf3],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];
const RESOURCE_IIDS: [Iid; 4] = [
    guid(
        0x696442be,
        0xa72e,
        0x4059,
        [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

const KIB_64: u64 = 64 << 10;
const MIB_4: u64 = 4 << 20;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct HeapProperties {
    kind: u32,
    cpu_pages: u32,
    pool: u32,
    creation_nodes: u32,
    visible_nodes: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct HeapDesc {
    size: u64,
    properties: HeapProperties,
    alignment: u64,
    flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct ResourceDesc {
    dimension: u32,
    alignment: u64,
    width: u64,
    height: u32,
    depth_or_array_size: u16,
    mip_levels: u16,
    format: u32,
    sample_count: u32,
    sample_quality: u32,
    layout: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AllocationInfo {
    size: u64,
    alignment: u64,
}

const _: () = assert!(size_of::<HeapProperties>() == 20);
const _: () = assert!(size_of::<HeapDesc>() == 48);
const _: () = assert!(size_of::<ResourceDesc>() == 56);

const BUFFER: u32 = 1;
const TEXTURE_3D: u32 = 4;

/// Bytes per element and the element's width in texels (4 for block
/// compression), by DXGI format number. `None` for 1-bit and video
/// formats. The console adds 116/117 (10-bit float color), 118-120 (16-bit
/// depth with 8-bit stencil, two planes: 3 bytes per texel), 189 (signed
/// 10-bit color), 190 (two 4-bit channels) and 191 (24/8 color).
pub(crate) fn element(format: u32) -> Option<(u64, u64)> {
    Some(match format {
        1..=4 => (16, 1),
        5..=8 => (12, 1),
        9..=22 => (8, 1),
        23..=47 | 67 | 87..=93 | 116 | 117 | 189 | 191 => (4, 1),
        118..=120 => (3, 1),
        48..=59 | 68 | 69 | 85 | 86 | 115 => (2, 1),
        60..=65 | 190 => (1, 1),
        70..=72 | 79..=81 => (8, 4),
        73..=78 | 82..=84 | 94..=99 => (16, 4),
        _ => return None,
    })
}

/// A texture's layout in console memory, as the console's layout library
/// gives it (`victory-hle`'s `xg` serves the same): the description's
/// layout field is `0x100 | swizzle mode`, 1 linear, 0 the library's pick.
pub(crate) fn texture_layout(desc: &ResourceDesc) -> Option<texture::Texture> {
    use texture::{Dimension, Swizzle, TextureDesc};
    const TEXTURE_1D: u32 = 2;
    const TEXTURE_2D: u32 = 3;
    const RENDER_TARGET: u32 = 1;
    const DEPTH: u32 = 2;
    let dimension = match desc.dimension {
        TEXTURE_1D => Dimension::Tex1d,
        TEXTURE_2D => Dimension::Tex2d,
        TEXTURE_3D => Dimension::Tex3d,
        _ => return None,
    };
    let swizzle = match desc.layout {
        0 if dimension == Dimension::Tex1d => Swizzle::LINEAR,
        0 if desc.flags & DEPTH != 0 => Swizzle::SW_64KB_Z_X,
        0 if desc.flags & RENDER_TARGET != 0 => Swizzle::SW_64KB_R_X,
        0 => Swizzle::SW_4KB_S,
        1 => Swizzle::LINEAR,
        l if l & !0xff == 0x100 => Swizzle(l & 0xff),
        _ => return None,
    };
    texture::Texture::new(&TextureDesc {
        dimension,
        width: u32::try_from(desc.width).ok()?,
        height: desc.height,
        depth_or_array: u32::from(desc.depth_or_array_size),
        mips: u32::from(desc.mip_levels),
        format: desc.format,
        samples: desc.sample_count.max(1),
        swizzle,
    })
    .ok()
}

/// Bytes and alignment a resource takes: buffers their width (64 KiB
/// aligned), textures their console layout (formats it does not know:
/// every mip of every array slice and sample, packed, 64 KiB aligned),
/// rounded up to the alignment.
fn footprint(desc: &ResourceDesc) -> Option<AllocationInfo> {
    if desc.dimension != BUFFER
        && let Some(t) = texture_layout(desc)
    {
        // As the console: the layout's own alignment unless more is asked
        // (4 KiB for 4 KiB modes, where PC says 64 KiB). Grand Theft Auto
        // VI keeps these sizes in its texture tables, and its streaming
        // asserts when they disagree with the streamed layout.
        let alignment = desc.alignment.max(t.alignment);
        if !alignment.is_power_of_two() {
            return None;
        }
        return Some(AllocationInfo {
            size: t.size.checked_next_multiple_of(alignment)?,
            alignment,
        });
    }
    let bytes = if desc.dimension == BUFFER {
        desc.width
    } else {
        let (element, block) = element(desc.format)?;
        let (depth, slices) = if desc.dimension == TEXTURE_3D {
            (u64::from(desc.depth_or_array_size), 1)
        } else {
            (1, u64::from(desc.depth_or_array_size))
        };
        let height = u64::from(desc.height);
        let mips = mip_count(desc);
        let mut chain = 0u64;
        for mip in 0..mips.min(64) {
            let w = (desc.width >> mip).max(1).div_ceil(block);
            let h = (height >> mip).max(1).div_ceil(block);
            let d = (depth >> mip).max(1);
            chain = chain.checked_add(w.checked_mul(h)?.checked_mul(d)?.checked_mul(element)?)?;
        }
        chain
            .checked_mul(slices)?
            .checked_mul(u64::from(desc.sample_count.max(1)))?
    };
    if bytes == 0 {
        return None;
    }
    let minimum = if desc.sample_count > 1 { MIB_4 } else { KIB_64 };
    let alignment = desc.alignment.max(minimum);
    if !alignment.is_power_of_two() {
        return None;
    }
    Some(AllocationInfo {
        size: bytes.checked_next_multiple_of(KIB_64)?,
        alignment,
    })
}

/// A format as the host has it. The console's 16-bit depth with stencil
/// (and its typeless views) has no PC format: the 24-bit ones stand in.
pub(crate) fn pc_format(format: u32) -> u32 {
    match format {
        118 => 45,
        119 => 46,
        120 => 47,
        other => other,
    }
}

/// R8G8B8A8_UNORM, which stands in for a packed 16-bit render target the
/// host cannot make (see [`target_format`]).
const RGBA8_UNORM: u32 = 28;

/// The console's packed 16-bit colour formats a title renders to:
/// B5G5R5A1_UNORM (86) and B4G4R4A4_UNORM (115).
const PACKED_16: [u32; 2] = [86, 115];

/// Whether the host GPU lacks render-target or typed unordered-access
/// support for `format` (asked once per format). NVIDIA has neither for
/// B4G4R4A4 and no typed UAV for B5G5R5A1, which Grand Theft Auto VI's
/// 2560x1440 UI targets need; AMD has both.
#[cfg(windows)]
fn host_lacks_target(format: u32) -> bool {
    use std::sync::OnceLock;
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
    static LACKS: [OnceLock<bool>; 2] = [OnceLock::new(), OnceLock::new()];
    let Some(i) = PACKED_16.iter().position(|&f| f == format) else {
        return false;
    };
    *LACKS[i].get_or_init(|| {
        let Some(host) = crate::host::host() else { return false };
        let mut support = D3D12_FEATURE_DATA_FORMAT_SUPPORT {
            Format: DXGI_FORMAT(format as i32),
            ..Default::default()
        };
        // SAFETY: the structure the feature names, ours.
        let asked = unsafe {
            host.device.CheckFeatureSupport(
                D3D12_FEATURE_FORMAT_SUPPORT,
                (&raw mut support).cast(),
                size_of::<D3D12_FEATURE_DATA_FORMAT_SUPPORT>() as u32,
            )
        };
        let needed = D3D12_FORMAT_SUPPORT1_RENDER_TARGET.0 | D3D12_FORMAT_SUPPORT1_TYPED_UNORDERED_ACCESS_VIEW.0;
        let lacks = asked.is_err() || support.Support1.0 & needed != needed;
        if lacks {
            tracing::info!(format, "packed 16-bit render targets are R8G8B8A8_UNORM on this GPU");
        }
        lacks
    })
}

#[cfg(not(windows))]
fn host_lacks_target(_: u32) -> bool {
    false
}

/// The format a render target or unordered-access texture of `format` is
/// made in on the host, and that render target views and pipelines name:
/// R8G8B8A8_UNORM for a packed 16-bit format the GPU cannot render to or
/// write typed (each channel keeps its value, with more precision); else
/// the format as the host has it. Textures only sampled keep theirs.
pub(crate) fn target_format(format: u32) -> u32 {
    let format = pc_format(format);
    if PACKED_16.contains(&format) && host_lacks_target(format) {
        RGBA8_UNORM
    } else {
        format
    }
}

/// The typeless format of a format's family (itself if it has none).
pub(crate) fn family(format: u32) -> u32 {
    const FAMILIES: [(u32, u32); 22] = [
        (1, 4),
        (5, 8),
        (9, 14),
        (15, 18),
        (19, 22),
        (23, 25),
        (27, 32),
        (33, 38),
        (39, 43),
        (44, 47),
        (48, 52),
        (53, 59),
        (60, 65),
        (70, 72),
        (73, 75),
        (76, 78),
        (79, 81),
        (82, 84),
        (90, 91),
        (92, 93),
        (94, 96),
        (97, 99),
    ];
    match format {
        87 => 90,
        88 => 92,
        f => FAMILIES
            .iter()
            .find(|&&(first, last)| (first..=last).contains(&f))
            .map_or(f, |&(first, _)| first),
    }
}

/// sRGB formats and their linear twins.
pub(crate) const SRGB: [(u32, u32); 3] = [(29, 28), (91, 87), (93, 88)];

/// The format a view of a title texture must name when its host copy is
/// typeless (see [`DEPTH_FORMATS`], [`SRGB`]): the title's own; `None` if
/// the host copy is typed.
pub(crate) fn view_format(this: This) -> Option<u32> {
    let own = pc_format(resource(this).desc.format);
    let host = host_resource(this)?;
    // SAFETY: a live resource.
    let format = unsafe { host.GetDesc() }.Format.0 as u32;
    (format != own).then_some(own)
}

/// Depth formats with their typeless family: depth textures that shaders
/// read are made typeless on the host, which only allows views of
/// typeless depth resources.
pub(crate) const DEPTH_FORMATS: [(u32, u32); 4] = [(20, 19), (40, 39), (45, 44), (55, 53)];

/// Mip levels, the full chain for 0.
fn mip_count(desc: &ResourceDesc) -> u32 {
    let depth = if desc.dimension == TEXTURE_3D {
        u64::from(desc.depth_or_array_size)
    } else {
        1
    };
    match desc.mip_levels {
        0 => {
            64 - desc
                .width
                .max(u64::from(desc.height))
                .max(depth)
                .max(1)
                .leading_zeros()
        }
        n => u32::from(n),
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlacedFootprint {
    pub(crate) offset: u64,
    pub(crate) format: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) depth: u32,
    pub(crate) row_pitch: u32,
}

impl PlacedFootprint {
    /// `(lines, row_bytes, total_bytes)` spanned in a buffer: every row
    /// pitched but the last.
    pub(crate) fn layout(&self) -> Option<(u64, u64, u64)> {
        let (element, block) = element(self.format)?;
        let lines = u64::from(self.height)
            .div_ceil(block)
            .checked_mul(u64::from(self.depth))?;
        let row_bytes = u64::from(self.width).div_ceil(block).checked_mul(element)?;
        if lines == 0 {
            return Some((0, row_bytes, 0));
        }
        let total = u64::from(self.row_pitch)
            .checked_mul(lines - 1)?
            .checked_add(row_bytes)?;
        Some((lines, row_bytes, total))
    }

    /// The bytes it spans in a buffer: every row pitched but the last.
    pub(crate) fn bytes(&self) -> Option<u64> {
        self.layout().map(|(_, _, total)| total)
    }
}
const _: () = assert!(size_of::<PlacedFootprint>() == 32);

const NO_FOOTPRINT: PlacedFootprint = PlacedFootprint {
    offset: u64::MAX,
    format: 0,
    width: 0,
    height: 0,
    depth: 0,
    row_pitch: 0,
};

/// One subresource's size in an upload buffer: footprint (offset 0), rows
/// and bytes per row. Planes of depth-stencil formats are not told apart.
fn subresource(desc: &ResourceDesc, index: u32) -> Option<(PlacedFootprint, u32, u64)> {
    const PITCH: u64 = 256;
    if desc.dimension == BUFFER {
        let footprint = PlacedFootprint {
            offset: 0,
            format: 0,
            width: u32::try_from(desc.width).ok()?,
            height: 1,
            depth: 1,
            row_pitch: u32::try_from(desc.width.next_multiple_of(PITCH)).ok()?,
        };
        return (index == 0).then_some((footprint, 1, desc.width));
    }
    let mips = mip_count(desc);
    let slices = if desc.dimension == TEXTURE_3D {
        1
    } else {
        u32::from(desc.depth_or_array_size).max(1)
    };
    let (mip, slice) = (index % mips, index / mips);
    if slice >= slices {
        return None;
    }
    let (element, block) = element(desc.format)?;
    let width = (desc.width >> mip).max(1).next_multiple_of(block);
    let height = (u64::from(desc.height) >> mip)
        .max(1)
        .next_multiple_of(block);
    let depth = if desc.dimension == TEXTURE_3D {
        (u32::from(desc.depth_or_array_size) >> mip).max(1)
    } else {
        1
    };
    let row_size = width / block * element;
    let footprint = PlacedFootprint {
        offset: 0,
        format: desc.format,
        width: u32::try_from(width).ok()?,
        height: u32::try_from(height).ok()?,
        depth,
        row_pitch: u32::try_from(row_size.next_multiple_of(PITCH)).ok()?,
    };
    Some((footprint, u32::try_from(height / block).ok()?, row_size))
}

/// Device method: where each subresource goes in an upload buffer, rows
/// 256-byte and subresources 512-byte aligned, as on PC. Unknown sizes give
/// the documented all-ones values.
pub(crate) extern "system" fn copyable_footprints(
    _: This,
    desc: *const ResourceDesc,
    first: u32,
    count: u32,
    base: u64,
    layouts: *mut PlacedFootprint,
    rows: *mut u32,
    row_sizes: *mut u64,
    total: *mut u64,
) {
    const PLACEMENT: u64 = 512;
    // SAFETY: the title passes a description (null is tolerated).
    let desc = unsafe { desc.as_ref() };
    let mut end = base;
    let mut known = desc.is_some();
    for i in 0..count {
        let one = desc.and_then(|d| subresource(d, first + i));
        let (layout, row_count, row_size) = match one {
            Some((mut footprint, row_count, row_size)) => {
                footprint.offset = end.next_multiple_of(PLACEMENT);
                let pitch = u64::from(footprint.row_pitch);
                let lines = u64::from(row_count) * u64::from(footprint.depth);
                end = footprint.offset + pitch * lines.saturating_sub(1) + row_size;
                (footprint, row_count, row_size)
            }
            None => {
                known = false;
                (NO_FOOTPRINT, u32::MAX, u64::MAX)
            }
        };
        let i = i as usize;
        // SAFETY: each output is null or holds `count` entries.
        unsafe {
            if !layouts.is_null() {
                layouts.add(i).write(layout);
            }
            if !rows.is_null() {
                rows.add(i).write(row_count);
            }
            if !row_sizes.is_null() {
                row_sizes.add(i).write(row_size);
            }
        }
    }
    if !known {
        tracing::warn!(?desc, "no footprint for this resource");
    }
    if !total.is_null() {
        // SAFETY: checked non-null.
        unsafe { total.write(if known { end - base } else { u64::MAX }) };
    }
}

/// Device method: the footprint of resources placed one after another.
/// Returns a 16-byte value, through a hidden first argument. Invalid input
/// gives the documented size `u64::MAX`.
pub(crate) extern "system" fn allocation_info(
    out: *mut AllocationInfo,
    _: This,
    _visible: u32,
    count: u32,
    descs: *const ResourceDesc,
) -> *mut AllocationInfo {
    let descs = if descs.is_null() {
        &[][..]
    } else {
        // SAFETY: the title passes `count` descriptions.
        unsafe { std::slice::from_raw_parts(descs, count as usize) }
    };
    // SAFETY: the caller passes room for the value.
    unsafe { out.write(place(descs.iter(), |_, _| ())) };
    out
}

/// The newer description: the plain one, then a sampler feedback mip
/// region (width, height, depth).
#[repr(C)]
pub(crate) struct ResourceDesc1 {
    desc: ResourceDesc,
    mip_region: [u32; 3],
}
const _: () = assert!(size_of::<ResourceDesc1>() == 72);

/// Where one resource goes in [`allocation_info2`]'s placement.
#[repr(C)]
pub(crate) struct AllocationInfo1 {
    offset: u64,
    alignment: u64,
    size: u64,
}

/// Device method 0x360 (`ID3D12Device8`): [`allocation_info`] for newer
/// descriptions, also telling where each resource goes.
pub(crate) extern "system" fn allocation_info2(
    out: *mut AllocationInfo,
    _: This,
    _visible: u32,
    count: u32,
    descs: *const ResourceDesc1,
    each: *mut AllocationInfo1,
) -> *mut AllocationInfo {
    let descs = if descs.is_null() {
        &[][..]
    } else {
        // SAFETY: the title passes `count` descriptions.
        unsafe { std::slice::from_raw_parts(descs, count as usize) }
    };
    let total = place(descs.iter().map(|d| &d.desc), |i, info| {
        if !each.is_null() {
            // SAFETY: the title passes room for `count` entries.
            unsafe { each.add(i).write(info) };
        }
    });
    // SAFETY: the caller passes room for the value.
    unsafe { out.write(total) };
    out
}

/// Resources placed one after another: the whole footprint, telling `each`
/// where every resource goes. Unknown sizes give the documented `u64::MAX`.
fn place<'a>(
    descs: impl Iterator<Item = &'a ResourceDesc>,
    mut each: impl FnMut(usize, AllocationInfo1),
) -> AllocationInfo {
    let mut total = AllocationInfo {
        size: 0,
        alignment: KIB_64,
    };
    for (i, desc) in descs.enumerate() {
        let Some(one) = footprint(desc) else {
            tracing::warn!(?desc, "no size for this resource");
            each(
                i,
                AllocationInfo1 {
                    offset: u64::MAX,
                    alignment: 0,
                    size: u64::MAX,
                },
            );
            return AllocationInfo {
                size: u64::MAX,
                alignment: 0,
            };
        };
        tracing::debug!(?desc, ?one, "allocation info");
        let offset = total.size.next_multiple_of(one.alignment);
        each(
            i,
            AllocationInfo1 {
                offset,
                alignment: one.alignment,
                size: one.size,
            },
        );
        total.size = offset + one.size;
        total.alignment = total.alignment.max(one.alignment);
    }
    total
}

/// Zeroed host memory standing in for GPU memory.
pub(crate) struct Memory {
    ptr: NonNull<u8>,
    layout: Layout,
    /// Set when the memory came from the title's allocator, with these
    /// allocation attributes (it goes back there).
    attributes: Option<u64>,
}

pub type AllocRoutine = extern "system" fn(usize, u64) -> *mut c_void;
pub type FreeRoutine = extern "system" fn(*mut c_void, u64);
static ALLOCATOR: std::sync::OnceLock<(AllocRoutine, FreeRoutine)> = std::sync::OnceLock::new();

/// Committed resources get their memory from the title's general
/// allocator, as on the console, where titles hook it to place graphics
/// memory themselves (Grand Theft Auto VI places its back buffers in a
/// block of its own and checks they fill it). The runtime names it before
/// the device exists.
pub fn use_allocator(alloc: AllocRoutine, free: FreeRoutine) {
    let _ = ALLOCATOR.set((alloc, free));
}

/// Maps the physical pages behind one range of title memory at another,
/// `(to, from, len)` (`from` 0 unmaps); false on failure.
pub type PageMapper = fn(u64, u64, u64) -> bool;
static PAGE_MAPPER: std::sync::OnceLock<PageMapper> = std::sync::OnceLock::new();

/// The runtime's page mapper, for the queues' page-mapping copies (memory
/// is the runtime's, as on the console).
pub fn use_page_mapper(mapper: PageMapper) {
    let _ = PAGE_MAPPER.set(mapper);
}

pub(crate) fn page_mapper() -> Option<PageMapper> {
    PAGE_MAPPER.get().copied()
}

/// Allocation attributes for resource memory: allocator id, page size
/// (bits 14-15: 2 MiB pages from 2 MiB up, else 64 KiB), alignment as log2
/// (bits 24-28) and memory type (bits 29-32: write-combined for GPU-only
/// heaps, cacheable for the CPU-visible ones).
/// shortcut: the console runtime's own choice of page size and memory type
/// is not known; this is the plausible one.
fn resource_attributes(info: AllocationInfo, heap_kind: u32) -> u64 {
    const GRAPHICS_ALLOCATOR: u64 = 128 << 16;
    const DEFAULT_HEAP: u32 = 1;
    let pages: u64 = if info.size >= 2 << 20 { 2 } else { 1 };
    let memory: u64 = if heap_kind == DEFAULT_HEAP { 1 } else { 3 };
    let alignment = u64::from(info.alignment.max(1).trailing_zeros());
    GRAPHICS_ALLOCATOR | pages << 14 | alignment << 24 | memory << 29
}

impl Memory {
    /// A committed resource's memory: from the title's allocator if the
    /// runtime named one, else our own; zeroed either way.
    fn for_resource(info: AllocationInfo, heap_kind: u32) -> Result<Memory, HRESULT> {
        let Some(&(alloc, _)) = ALLOCATOR.get() else {
            return Memory::new(info);
        };
        let layout = Layout::from_size_align(info.size as usize, info.alignment as usize)
            .map_err(|_| E_INVALIDARG)?;
        let attributes = resource_attributes(info, heap_kind);
        let ptr =
            NonNull::new(alloc(layout.size(), attributes).cast::<u8>()).ok_or(E_OUTOFMEMORY)?;
        // SAFETY: the allocator handed out `size` bytes.
        unsafe { ptr.as_ptr().write_bytes(0, layout.size()) };
        crate::memstat::add(&crate::memstat::TITLE_GPU, layout.size() as u64);
        Ok(Memory {
            ptr,
            layout,
            attributes: Some(attributes),
        })
    }

    /// `size` zeroed bytes aligned to `alignment` (a power of two).
    pub(crate) fn zeroed(size: u64, alignment: u64) -> Result<Memory, HRESULT> {
        Memory::new(AllocationInfo { size, alignment })
    }

    pub(crate) fn address(&self) -> u64 {
        self.ptr.as_ptr() as u64
    }

    fn new(info: AllocationInfo) -> Result<Memory, HRESULT> {
        let layout = usize::try_from(info.size)
            .ok()
            .zip(usize::try_from(info.alignment).ok())
            .filter(|&(size, _)| size > 0)
            .and_then(|(size, align)| Layout::from_size_align(size, align).ok())
            .ok_or(E_INVALIDARG)?;
        // SAFETY: the size is non-zero.
        let ptr = NonNull::new(unsafe { alloc_zeroed(layout) }).ok_or(E_OUTOFMEMORY)?;
        crate::memstat::add(&crate::memstat::TITLE_GPU, layout.size() as u64);
        Ok(Memory {
            ptr,
            layout,
            attributes: None,
        })
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        crate::memstat::sub(&crate::memstat::TITLE_GPU, self.layout.size() as u64);
        match (self.attributes, ALLOCATOR.get()) {
            (Some(attributes), Some(&(_, free))) => free(self.ptr.as_ptr().cast(), attributes),
            // SAFETY: allocated in `new` with this layout.
            _ => unsafe { dealloc(self.ptr.as_ptr(), self.layout) },
        }
    }
}

#[repr(C)]
struct Heap {
    object: Object,
    desc: HeapDesc,
    memory: Memory,
}

pub(crate) extern "system" fn create_heap(
    _: This,
    desc: *const HeapDesc,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    tracing::debug!(
        size = desc.size,
        kind = desc.properties.kind,
        flags = format_args!("{:#x}", desc.flags),
        "heap"
    );
    if desc.alignment != 0 && !desc.alignment.is_power_of_two() {
        return E_INVALIDARG;
    }
    let info = AllocationInfo {
        size: desc.size,
        alignment: desc.alignment.max(KIB_64),
    };
    let memory = match Memory::new(info) {
        Ok(memory) => memory,
        Err(hr) => return hr,
    };
    let heap = Box::new(Heap {
        object: Object::new(&HEAP_FUNCTIONS),
        desc,
        memory,
    });
    object::hand_out(HEAP, heap, iid, &HEAP_IIDS, out)
}

extern "system" fn heap_query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(HEAP, iid, object::is(iid, &HEAP_IIDS).then_some(this), out)
}
extern "system" fn heap_release(this: This) -> u32 {
    object::release_boxed::<Heap>(this)
}
extern "system" fn heap_desc(this: This, out: *mut HeapDesc) {
    if !out.is_null() {
        // SAFETY: `this` is a heap; the title passes room for a description.
        unsafe { out.write((*this.cast::<Heap>()).desc) };
    }
}

/// A resource's memory: its own, or a range of a heap it holds a reference
/// to.
enum Backing {
    Committed(Memory),
    Placed {
        heap: This,
        offset: u64,
    },
    /// Memory the title mapped itself, at this address.
    Address(*mut u8),
}

impl Drop for Backing {
    fn drop(&mut self) {
        if let Backing::Placed { heap, .. } = *self {
            heap_release(heap);
        }
    }
}

#[repr(C)]
struct Resource {
    object: Object,
    desc: ResourceDesc,
    heap: HeapProperties,
    heap_flags: u32,
    backing: Backing,
    /// Textures also live on the host GPU (decision 0007); buffers do not
    /// yet.
    #[cfg(windows)]
    host: Hosted,
}

/// A title texture's host side.
///
/// The host copy of a texture the host renders to or writes (target, depth
/// or unordered access flag) is made when something first uses it, not when
/// the title creates it: the title creates the targets of every pass and
/// size up front and places them over one another in its own heap, 4.2 GB of
/// host memory before its world loads (session 7824) of which a frame's lists
/// name some. That memory is what streamed textures were cut to 256 texels
/// for. A copy made stays until the title releases the texture: what the host
/// wrote exists nowhere else. Textures the title fills are made at once.
///
/// What uses a texture: a render target or depth view bound or cleared, a
/// copy or resolve, a table entry a shader of a draw or dispatch reads or
/// writes, a barrier to or from a state it is written in, a present. A
/// descriptor the title writes is no use: the slot reads nothing until the
/// texture is made (`crate::descriptor::make_waiting`).
///
/// From github.com/fxceless/victory-fork (commit 578d459).
#[cfg(windows)]
struct Hosted {
    made: std::sync::OnceLock<std::sync::Arc<HostTexture>>,
    /// Held while the host copy is made: threads that ask meanwhile wait for
    /// the one copy.
    making: std::sync::Mutex<()>,
    /// When the host may be asked again after it refused ([`now_ms`];
    /// `u64::MAX`: never).
    refused: std::sync::atomic::AtomicU64,
    /// The console state to make it in: the one the title created it in, or
    /// where a barrier took it since.
    state: std::sync::atomic::AtomicU32,
    clear: Option<[u32; 5]>,
    /// Made on first use.
    lazy: bool,
    /// The title memory it is, counted in [`crate::memstat::UNMADE`] until
    /// it is made.
    bytes: u64,
}

/// Textures of fewer bytes of title memory than this are made at once: they
/// take next to nothing together, and the small ones (tables, histograms,
/// counters) are those shaders reach in the most ways.
#[cfg(windows)]
const LAZY_LEAST: u64 = 256 << 10;

/// The bytes of title memory from which a texture the host renders to or
/// writes is made on first use: `VICTORY_LAZY_TEXTURES=<KiB>`, [`LAZY_LEAST`]
/// by default. `VICTORY_LAZY_TEXTURES=0` makes every texture when the title
/// creates it, as before.
#[cfg(windows)]
fn lazy_from() -> Option<u64> {
    static FROM: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *FROM.get_or_init(|| {
        let set = std::env::var("VICTORY_LAZY_TEXTURES").ok();
        match set.and_then(|v| v.trim().parse::<u64>().ok()) {
            Some(0) => None,
            None | Some(1) => Some(LAZY_LEAST),
            Some(kib) => Some(kib << 10),
        }
    })
}

/// Whether textures of 1 MiB the host writes are made on first use (tests of
/// that are left out if the switch says otherwise).
#[cfg(all(test, windows))]
pub(crate) fn lazy_targets() -> bool {
    lazy_from().is_some_and(|least| least <= 1 << 20)
}

/// Milliseconds since the first call.
#[cfg(windows)]
pub(crate) fn now_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

#[cfg(windows)]
impl Hosted {
    /// The host side of a texture described by `desc`, not made yet.
    fn new(desc: &ResourceDesc, state: u32, clear: *const c_void) -> Hosted {
        const TARGET_DEPTH_UNORDERED: u32 = 0x7;
        let written = desc.dimension != BUFFER && desc.flags & TARGET_DEPTH_UNORDERED != 0;
        let least = lazy_from().filter(|_| written);
        let bytes = least.and_then(|_| footprint(desc)).map_or(0, |f| f.size);
        let lazy = least.is_some_and(|least| bytes >= least);
        Hosted {
            made: std::sync::OnceLock::new(),
            making: std::sync::Mutex::new(()),
            refused: std::sync::atomic::AtomicU64::new(0),
            state: state.into(),
            // SAFETY: the title passes a whole clear value (the format, then
            // color or depth and stencil: 5 words) or null.
            clear: (!clear.is_null()).then(|| unsafe { clear.cast::<[u32; 5]>().read_unaligned() }),
            lazy,
            bytes,
        }
    }

    /// Whether the host refused the texture for good.
    fn never(&self) -> bool {
        self.refused.load(std::sync::atomic::Ordering::Relaxed) == u64::MAX
    }

    /// Makes the host copy of a texture described by `desc` without its
    /// `skip` largest mips; `None` if the host refuses it.
    fn make(&self, desc: &ResourceDesc, skip: u32) -> Option<&std::sync::Arc<HostTexture>> {
        use std::sync::atomic::Ordering::Relaxed;
        /// How long a host that was out of memory is not asked again.
        const AGAIN_MS: u64 = 1000;
        let _one = self.making.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(made) = self.made.get() {
            return Some(made);
        }
        if now_ms() < self.refused.load(Relaxed) {
            return None;
        }
        let state = self.state.load(Relaxed);
        let made = host_texture(desc, state, self.clear.as_ref(), skip);
        if matches!(made, Err(Refused::Memory)) {
            crate::texcap::refused();
        }
        match made {
            Ok(made) => {
                let made = std::sync::Arc::new(HostTexture::new(desc, state, made));
                list_target(&made);
                let _ = self.made.set(made);
            }
            // Memory may come back; what the host cannot make it never can.
            Err(Refused::Memory) if self.lazy => self.refused.store(now_ms() + AGAIN_MS, Relaxed),
            Err(_) => self.refused.store(u64::MAX, Relaxed),
        }
        let made = self.made.get()?;
        if self.lazy {
            crate::memstat::sub(&crate::memstat::UNMADE[0], self.bytes);
            crate::memstat::sub(&crate::memstat::UNMADE[1], 1);
        }
        Some(made)
    }
}

/// The host copies of textures the host renders to or writes (for the log),
/// and how many were alive when dead ones were last taken out.
#[cfg(windows)]
#[allow(clippy::type_complexity)]
static MADE_TARGETS: std::sync::Mutex<(Vec<std::sync::Weak<HostTexture>>, usize)> =
    std::sync::Mutex::new((Vec::new(), 0));

#[cfg(windows)]
fn list_target(made: &std::sync::Arc<HostTexture>) {
    if !made.target {
        return;
    }
    let mut list = MADE_TARGETS.lock().unwrap_or_else(|e| e.into_inner());
    if list.0.len() > 2 * list.1 + 256 {
        list.0.retain(|t| t.strong_count() > 0);
        list.1 = list.0.len();
    }
    list.0.push(std::sync::Arc::downgrade(made));
}

/// The bytes and the number of the textures the host renders to or writes
/// that a submission used in the last `ms` milliseconds.
#[cfg(windows)]
pub(crate) fn targets_used(ms: u64) -> [u64; 2] {
    let since = now_ms().saturating_sub(ms);
    let list = MADE_TARGETS.lock().unwrap_or_else(|e| e.into_inner());
    let used = list.0.iter().filter_map(std::sync::Weak::upgrade).filter(|t| {
        t.used.load(std::sync::atomic::Ordering::Relaxed) >= since
    });
    used.fold([0, 0], |sum, t| [sum[0] + t.bytes, sum[1] + 1])
}

/// Why the host does not make a texture.
#[cfg(windows)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Refused {
    /// Out of memory.
    Memory,
    /// Anything else.
    Other,
}

/// The host copy of title texture `this`, made now if this is its first use
/// ([`Hosted`]); `None` for buffers and textures the host refuses.
#[cfg(windows)]
fn hosted<'a>(this: This) -> Option<&'a std::sync::Arc<HostTexture>> {
    let r = resource(this);
    if let Some(made) = r.host.made.get() {
        return Some(made);
    }
    if !r.host.lazy {
        return None;
    }
    let made = r.host.make(&r.desc, 0)?;
    // Slots written with views of it are made before the next submission.
    crate::descriptor::texture_made(this as usize);
    Some(made)
}

/// The host copy of title texture `this` if it is made: for what is no use
/// of it.
#[cfg(windows)]
fn made<'a>(this: This) -> Option<&'a std::sync::Arc<HostTexture>> {
    if this.is_null() {
        return None;
    }
    resource(this).host.made.get()
}

/// Whether `this` is a texture whose host copy waits for its first use.
#[cfg(windows)]
pub(crate) fn waits(this: This) -> bool {
    if this.is_null() {
        return false;
    }
    let host = &resource(this).host;
    host.lazy && host.made.get().is_none() && !host.never()
}

/// The host copy of title texture `this` if it is made, without making it
/// (for diagnostics, which must not count as a use).
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn host_made<'a>(
    this: This,
) -> Option<&'a windows::Win32::Graphics::Direct3D12::ID3D12Resource> {
    made(this).map(|h| &h.texture)
}

/// Whether a title barrier on texture `this` from `before` to `after`
/// (console states) has a host copy to move. One nothing has used yet only
/// notes the state to be made in, unless it is written in one of the two:
/// shaders that pick it from the heap write it with no other sign of it.
#[cfg(windows)]
pub(crate) fn barrier_meets(this: This, before: u32, after: u32) -> bool {
    // Render target, unordered access, depth write, copy and resolve
    // destination.
    const WRITES: u32 = 0x4 | 0x8 | 0x10 | 0x400 | 0x1000;
    if !waits(this) || (before | after) & WRITES != 0 {
        return true;
    }
    let host = &resource(this).host;
    host.state.store(after, std::sync::atomic::Ordering::Relaxed);
    false
}

/// A title texture's host copy and the state it is in. The title's barriers
/// name console states (looser than the host's), so we transition from this
/// one. Lists that use the texture hold it until they are reset: the title
/// may release a resource whose commands it recorded but not yet submitted
/// (console memory outlives the object; a host texture must outlive the
/// lists that name it).
#[cfg(windows)]
pub(crate) struct HostTexture {
    pub(crate) texture: windows::Win32::Graphics::Direct3D12::ID3D12Resource,
    pub(crate) state: std::sync::atomic::AtomicU32,
    /// An upload, copy or resolve has been recorded for this copy. Kept
    /// with the texture because staging buffers can be evicted and remade.
    pub(crate) initialized: std::sync::atomic::AtomicBool,
    /// What the driver allocates for it (counted in [`crate::memstat`]).
    pub(crate) bytes: u64,
    /// The host renders to it or writes it (counted apart).
    pub(crate) target: bool,
    /// When a submission last used it ([`now_ms`]).
    pub(crate) used: std::sync::atomic::AtomicU64,
    /// Its memory, if in a shared heap.
    pub(crate) slot: Option<crate::texpool::Slot>,
    /// The title's largest mips it goes without ([`over`]).
    pub(crate) skip: u32,
    /// Views of it asked for larger mips whose memory was not there
    /// ([`texture_at`]).
    pub(crate) short: std::sync::atomic::AtomicBool,
    /// Created with RGB9E5 in the format list for sampling packed HDR data.
    pub(crate) shared_exponent: bool,
    /// When a list last moved it to a state shaders or draws write in, in
    /// order: of textures in the same memory, the last holds its contents.
    pub(crate) written: std::sync::atomic::AtomicU64,
    /// Planes shaders write as images, for depth textures the title also
    /// writes so ([`crate::twin`]); made on the first such view.
    pub(crate) twin: std::sync::OnceLock<Option<crate::twin::Twin>>,
}

/// Textures the host GPU writes, by the title memory they are placed in.
/// Titles read such memory through other views, with no resource object:
/// Grand Theft Auto VI writes its HDR images as 32-bit integers and reads
/// them as shared-exponent colors.
#[cfg(windows)]
static TARGETS: std::sync::Mutex<std::collections::BTreeMap<u64, Vec<usize>>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Shared-exponent colors (R9G9B9E5), which 32-bit textures that shaders
/// write may be read as.
pub(crate) const SHARED_EXPONENT: u32 = 67;

/// The written texture at `address` that a view described by `d` reads, the
/// last written of several.
#[cfg(windows)]
fn target_at(address: u64, d: &ResourceDesc) -> Option<This> {
    let targets = TARGETS.lock().unwrap_or_else(|e| e.into_inner());
    targets
        .get(&address)?
        .iter()
        .map(|&r| r as This)
        .filter(|&r| {
            let resource = resource(r);
            let o = &resource.desc;
            (o.dimension, o.width, o.height, o.depth_or_array_size, o.mip_levels)
                == (d.dimension, d.width, d.height, d.depth_or_array_size, d.mip_levels)
                && !resource.host.never()
                && (family(o.format) == family(d.format)
                    || d.format == SHARED_EXPONENT
                        && resource.host.made.get().map_or_else(
                            || shares_exponent(o.format, o.flags),
                            |host| host.shared_exponent,
                        ))
        })
        .max_by_key(|&r| {
            made(r).map_or(0, |h| h.written.load(std::sync::atomic::Ordering::Relaxed))
        })
}

/// Every texture the host GPU writes, by title address (diagnostics).
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn targets_text() -> String {
    let targets = TARGETS.lock().unwrap_or_else(|e| e.into_inner());
    let mut text = String::new();
    for (at, list) in targets.iter() {
        for &r in list {
            let o = &resource(r as This).desc;
            let written =
                made(r as This).map_or(0, |h| h.written.load(std::sync::atomic::Ordering::Relaxed));
            text.push_str(&format!(
                "target {at:#x} {r:#x} {}x{}x{} mips {} f{} flags {:#x} layout {:#x} written {written}
",
                o.width, o.height, o.depth_or_array_size, o.mip_levels, o.format, o.flags, o.layout
            ));
        }
    }
    text
}

/// Where the title has the texture `r`, and the host resource object (diagnostics).
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn data_of(r: This) -> u64 {
    resource(r).data() as u64
}

/// A list moves `texture` to `state` (a use of it).
#[cfg(windows)]
pub(crate) fn note_written(resource_: This, state: u32) {
    static CLOCK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    const RENDER_TARGET: u32 = 0x4;
    const UNORDERED: u32 = 0x8;
    if state & (RENDER_TARGET | UNORDERED) != 0
        && let Some(h) = hosted(resource_)
    {
        let now = CLOCK.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        h.written.store(now, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The host device, if 32-bit textures it makes may be read as
/// shared-exponent colors.
#[cfg(windows)]
fn caster(
    device: &windows::Win32::Graphics::Direct3D12::ID3D12Device,
) -> Option<&'static windows::Win32::Graphics::Direct3D12::ID3D12Device10> {
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::core::Interface;
    static DEVICE: std::sync::OnceLock<Option<ID3D12Device10>> = std::sync::OnceLock::new();
    DEVICE
        .get_or_init(|| {
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS12::default();
            // SAFETY: a structure of the size the feature has.
            unsafe {
                device
                    .CheckFeatureSupport(
                        D3D12_FEATURE_D3D12_OPTIONS12,
                        std::ptr::from_mut(&mut options).cast(),
                        std::mem::size_of_val(&options) as u32,
                    )
                    .ok()?;
            }
            if !options.RelaxedFormatCastingSupported.as_bool() {
                tracing::warn!("the host cannot read 32-bit textures as shared-exponent colors");
                return None;
            }
            device.cast().ok()
        })
        .as_ref()
}

/// The host refused to make a texture as [`castable`] does.
#[cfg(windows)]
static NOT_CASTABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the host copy of a title texture of `format` and `flags` will be
/// one shared-exponent views may be made of (it is not made yet).
#[cfg(windows)]
fn shares_exponent(format: u32, flags: u32) -> bool {
    const DEPTH: u32 = 2;
    const UNORDERED: u32 = 4;
    (39..=43).contains(&format)
        && flags & (DEPTH | UNORDERED) == UNORDERED
        && !NOT_CASTABLE.load(std::sync::atomic::Ordering::Relaxed)
        && crate::host::host().is_some_and(|h| caster(&h.device).is_some())
}

/// `desc` as a texture that 32-bit and shared-exponent views may be made of;
/// in the common state.
#[cfg(windows)]
fn castable(
    device: &windows::Win32::Graphics::Direct3D12::ID3D12Device,
    desc: &windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_DESC,
    clear: Option<*const windows::Win32::Graphics::Direct3D12::D3D12_CLEAR_VALUE>,
) -> Option<windows::Win32::Graphics::Direct3D12::ID3D12Resource> {
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    let device = caster(device)?;
    let desc = D3D12_RESOURCE_DESC1 {
        Dimension: desc.Dimension,
        Alignment: desc.Alignment,
        Width: desc.Width,
        Height: desc.Height,
        DepthOrArraySize: desc.DepthOrArraySize,
        MipLevels: desc.MipLevels,
        Format: desc.Format,
        SampleDesc: desc.SampleDesc,
        Layout: desc.Layout,
        Flags: desc.Flags,
        ..Default::default()
    };
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let formats = [
        DXGI_FORMAT_R32_UINT,
        DXGI_FORMAT_R32_FLOAT,
        DXGI_FORMAT_R32_SINT,
        DXGI_FORMAT_R9G9B9E5_SHAREDEXP,
    ];
    let mut texture: Option<ID3D12Resource> = None;
    // SAFETY: whole descriptions.
    let result = unsafe {
        device.CreateCommittedResource3(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            D3D12_BARRIER_LAYOUT_COMMON,
            clear,
            None,
            Some(&formats),
            &mut texture,
        )
    };
    if let Err(error) = result {
        NOT_CASTABLE.store(true, std::sync::atomic::Ordering::Relaxed);
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!(%error, format = desc.Format.0, "no RGB9E5 cast for packed HDR target");
        });
        return None;
    }
    texture
}

/// What [`host_texture`] made.
#[cfg(windows)]
struct Made {
    texture: windows::Win32::Graphics::Direct3D12::ID3D12Resource,
    slot: Option<crate::texpool::Slot>,
    skip: u32,
    shared_exponent: bool,
}

#[cfg(windows)]
impl HostTexture {
    /// The host copy of a title texture described by `desc`, made in the
    /// console state `state`.
    fn new(desc: &ResourceDesc, state: u32, made: Made) -> HostTexture {
        let Made { texture, slot, skip, shared_exponent } = made;
        let state = if shared_exponent {
            0
        } else {
            crate::host::legal_state(&texture, crate::host::texture_state(state))
        };
        let bytes = allocation_bytes(&texture);
        let target = desc.flags & 0x7 != 0;
        crate::memstat::add(&crate::memstat::HOST_TEXTURES, bytes);
        crate::memstat::add(&crate::memstat::HOST_TEXTURE_COUNT, 1);
        if target {
            crate::memstat::add(&crate::memstat::TARGETS[0], bytes);
            crate::memstat::add(&crate::memstat::TARGETS[1], 1);
        }
        #[cfg(feature = "diagnostics")]
        census(&texture, bytes as i64);
        HostTexture {
            texture,
            bytes,
            target,
            used: now_ms().into(),
            slot,
            skip,
            short: Default::default(),
            shared_exponent,
            written: Default::default(),
            initialized: Default::default(),
            twin: Default::default(),
            state: state.into(),
        }
    }
}

#[cfg(windows)]
impl Drop for HostTexture {
    fn drop(&mut self) {
        crate::memstat::sub(&crate::memstat::HOST_TEXTURES, self.bytes);
        crate::memstat::sub(&crate::memstat::HOST_TEXTURE_COUNT, 1);
        if self.target {
            crate::memstat::sub(&crate::memstat::TARGETS[0], self.bytes);
            crate::memstat::sub(&crate::memstat::TARGETS[1], 1);
        }
        #[cfg(feature = "diagnostics")]
        census(&self.texture, -(self.bytes as i64));
        // A submission recorded but not yet run may still name it, or its
        // twin.
        let leaving = crate::memstat::Leaving::new(self.bytes);
        crate::host::retire((self.texture.clone(), self.slot.take(), self.twin.take(), leaving));
    }
}

/// Host textures by what they are: size, format, flags; count and bytes.
#[cfg(all(windows, feature = "diagnostics"))]
type Census = std::collections::HashMap<(u64, u32, u16, u16, i32, u32), (i64, i64)>;
#[cfg(all(windows, feature = "diagnostics"))]
static CENSUS: std::sync::LazyLock<std::sync::Mutex<Census>> =
    std::sync::LazyLock::new(Default::default);

#[cfg(all(windows, feature = "diagnostics"))]
fn census(texture: &windows::Win32::Graphics::Direct3D12::ID3D12Resource, bytes: i64) {
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let key = (d.Width, d.Height, d.DepthOrArraySize, d.MipLevels, d.Format.0, d.Flags.0 as u32);
    let mut all = CENSUS.lock().unwrap_or_else(|e| e.into_inner());
    let e = all.entry(key).or_default();
    e.0 += bytes.signum();
    e.1 += bytes;
}

/// The kinds of host textures that take most (diagnostics).
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn census_log() {
    let all = CENSUS.lock().unwrap_or_else(|e| e.into_inner());
    let mut v: Vec<_> = all.iter().collect();
    v.sort_by_key(|(_, e)| -e.1);
    for (k, e) in v.iter().take(40) {
        tracing::info!("textures {k:?}: {} of {} MiB", e.0, e.1 >> 20);
    }
    let targets: i64 = v.iter().filter(|(k, _)| k.5 & 7 != 0).map(|(_, e)| e.1).sum();
    let small: i64 = v.iter().filter(|(_, e)| e.1 / e.0.max(1) <= 65536).map(|(_, e)| e.1).sum();
    tracing::info!("textures with target or write flags {} MiB, of 64 KiB each {} MiB", targets >> 20, small >> 20);
}

/// The driver's allocation size for `texture`.
#[cfg(windows)]
fn allocation_bytes(texture: &windows::Win32::Graphics::Direct3D12::ID3D12Resource) -> u64 {
    let Some(host) = crate::host::host() else {
        return 0;
    };
    // SAFETY: a live resource and its own description.
    unsafe {
        let desc = texture.GetDesc();
        host.device
            .GetResourceAllocationInfo(0, &[desc])
            .SizeInBytes
    }
}

/// Moves the host texture to `wanted` (a host texture state); the host
/// resource and the state it leaves, if that is a change.
#[cfg(windows)]
pub(crate) fn host_transition(this: This, wanted: u32) -> Option<(*mut c_void, u32)> {
    use windows::core::Interface;
    let host = hosted(this)?;
    let before = host
        .state
        .swap(wanted, std::sync::atomic::Ordering::Relaxed);
    (before != wanted).then(|| (host.texture.as_raw(), before))
}

/// The host GPU's copy of a title resource, if it has one: made now if this
/// is the texture's first use ([`Hosted`]). Asking is a use; what must not
/// be one asks [`waits`] first.
#[cfg(windows)]
pub(crate) fn host_resource<'a>(
    this: This,
) -> Option<&'a windows::Win32::Graphics::Direct3D12::ID3D12Resource> {
    if this.is_null() {
        return None;
    }
    hosted(this).map(|h| &h.texture)
}

/// The twins of a depth texture the title also writes as an image (see
/// [`crate::twin`]), made on first use; `None` for other textures.
#[cfg(windows)]
pub(crate) fn depth_twin<'a>(this: This) -> Option<&'a crate::twin::Twin> {
    if this.is_null() {
        return None;
    }
    let r = resource(this);
    if !crate::twin::wanted(r.desc.format, r.desc.flags) {
        return None;
    }
    let host = hosted(this)?;
    host.twin.get_or_init(|| crate::twin::make(&host.texture)).as_ref()
}

/// A title texture's host copy, shared, for a list to hold (a use of it, as
/// [`host_resource`]).
#[cfg(windows)]
pub(crate) fn host_copy(this: This) -> Option<std::sync::Arc<HostTexture>> {
    if this.is_null() {
        return None;
    }
    hosted(this).cloned()
}

#[cfg(windows)]
pub(crate) fn has_shared_exponent_view(this: This) -> bool {
    made(this).is_some_and(|host| host.shared_exponent)
}

/// How many of a streamed texture's largest mips to do without, for none of
/// its sides to pass `max` texels (0: none; [`crate::texcap`] says how many).
#[cfg(windows)]
fn over(d: &ResourceDesc, max: u64) -> u32 {
    const TEXTURE2D: u32 = 3;
    let block = (70..=84).contains(&d.format) || (94..=99).contains(&d.format);
    if max == 0
        || textures() != Textures::Stream
        || d.dimension != TEXTURE2D
        || d.flags & 7 != 0
        || !block
        || d.mip_levels < 2
    {
        return 0;
    }
    let mut skip = 0;
    while (d.width.max(u64::from(d.height)) >> skip) > max
        && skip + 1 < u32::from(d.mip_levels)
        && (d.width >> (skip + 1)).is_multiple_of(4)
        && (d.height >> (skip + 1)).is_multiple_of(4)
        && d.width >> (skip + 1) > 0
        && d.height >> (skip + 1) > 0
    {
        skip += 1;
    }
    skip
}

/// The title's largest mips the host copy of a texture goes without.
#[cfg(windows)]
pub(crate) fn skipped_mips(this: This) -> u32 {
    made(this).map_or(0, |h| h.skip)
}

/// A host texture like the title's: same description, in GPU memory,
/// without the console's layout and flags and the `skip` largest mips.
#[cfg(windows)]
fn host_texture(
    desc: &ResourceDesc,
    state: u32,
    clear: Option<&[u32; 5]>,
    skip: u32,
) -> Result<Made, Refused> {
    use windows::Win32::Graphics::Direct3D12::*;
    const PC_FLAGS: u32 = 0xf; // render target, depth, UAV, no shader access
    if desc.dimension == BUFFER {
        return Err(Refused::Other);
    }
    let host = crate::host::host().ok_or(Refused::Other)?;
    let mut d = *desc;
    d.layout = 0;
    d.alignment = 0;
    let skip = skip.min(u32::from(d.mip_levels).saturating_sub(1));
    d.width >>= skip;
    d.height >>= skip;
    d.mip_levels -= skip as u16;
    d.flags &= PC_FLAGS;
    // PC refuses depth targets that are also unordered-access (the console
    // allows both; Grand Theft Auto VI's shadow maps ask for it).
    // shortcut: the host copy loses UAV use; give it a second view-only
    // copy if draws write it as a UAV.
    const DEPTH: u32 = 2;
    const UNORDERED: u32 = 4;
    if d.flags & DEPTH != 0 {
        d.flags &= !UNORDERED;
    }
    const RENDER_TARGET: u32 = 1;
    d.format = if d.flags & (RENDER_TARGET | UNORDERED) != 0 {
        target_format(d.format)
    } else {
        pc_format(d.format)
    };
    const DENY_SHADER_RESOURCE: u32 = 8;
    if d.flags & DENY_SHADER_RESOURCE == 0
        && let Some(&(_, typeless)) = DEPTH_FORMATS.iter().find(|&&(f, _)| f == d.format)
    {
        d.format = typeless;
    }
    // The host has no sRGB unordered access: such textures are typeless,
    // their unordered access views plain UNORM.
    if d.flags & UNORDERED != 0 && SRGB.iter().any(|&(srgb, _)| srgb == d.format) {
        d.format = family(d.format);
    }
    // The clear value names the format too: format, then color or depth
    // and stencil (5 words).
    let clear_value = clear.map(|value| {
        let mut value = *value;
        value[0] = if d.flags & (RENDER_TARGET | UNORDERED) != 0 {
            target_format(value[0])
        } else {
            pc_format(value[0])
        };
        value
    });
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let mut texture: Option<ID3D12Resource> = None;
    let create = |d: &ResourceDesc, texture: &mut Option<ID3D12Resource>| {
        // A clear value is only legal on render targets and depth.
        let clear = clear_value
            .as_ref()
            .filter(|_| d.flags & (1 | DEPTH) != 0)
            .map(|v| std::ptr::from_ref(v).cast());
        // SAFETY: the description has the host layout (checked size, same
        // fields); the out pointer is ours.
        unsafe {
            host.device.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                std::ptr::from_ref(d).cast(),
                D3D12_RESOURCE_STATES(crate::host::legal_for(
                    d.flags,
                    crate::host::texture_state(state),
                ) as i32),
                // The same layout: format, then color or depth and stencil.
                clear,
                texture,
            )
        }
    };
    // SAFETY: the same fields (checked size).
    let desc = unsafe { std::ptr::from_ref(&d).cast::<D3D12_RESOURCE_DESC>().read() };
    let first = D3D12_RESOURCE_STATES(crate::host::legal_for(
        d.flags,
        crate::host::texture_state(state),
    ) as i32);
    if let Some((texture, slot)) = crate::texpool::place(&host.device, &desc, first) {
        return Ok(Made { texture, slot: Some(slot), skip, shared_exponent: false });
    }
    if (39..=43).contains(&d.format)
        && d.flags & UNORDERED != 0
        && let Some(texture) = castable(
            &host.device,
            &desc,
            clear_value
                .as_ref()
                .filter(|_| d.flags & RENDER_TARGET != 0)
                .map(|v| std::ptr::from_ref(v).cast()),
        )
    {
        return Ok(Made { texture, slot: None, skip, shared_exponent: true });
    }
    let mut made = create(&d, &mut texture);
    // Not every host GPU offers every use of every format (NVIDIA: no
    // unordered access to BGRA8, no B4G4R4A4 render targets, where the
    // console's AMD GPU has both). Give up the uses the host refuses, UAV
    // first; the views the host cannot make are skipped as for depth.
    for drop in [UNORDERED, 1] {
        if made.is_ok() || d.flags & drop == 0 {
            continue;
        }
        let mut fewer = d;
        fewer.flags &= !drop;
        let mut retry: Option<ID3D12Resource> = None;
        if create(&fewer, &mut retry).is_ok() {
            tracing::warn!(desc = ?d, dropped = drop, "host texture without a use the host refuses");
            d = fewer;
            texture = retry;
            made = Ok(());
        } else if drop == UNORDERED {
            d = fewer; // also try with neither
        }
    }
    if let Err(e) = made {
        tracing::warn!(error = %e, desc = ?d, "no host texture");
        return Err(if e.code() == windows::Win32::Foundation::E_OUTOFMEMORY {
            Refused::Memory
        } else {
            Refused::Other
        });
    }
    let texture = texture.ok_or(Refused::Other)?;
    crate::host::name(&texture, || {
        format!(
            "texture {}x{} f{} flags {:#x}",
            d.width, d.height, d.format, d.flags
        )
    });
    Ok(Made { texture, slot: None, skip, shared_exponent: false })
}

/// A texture's console memory and description; `None` for buffers.
pub(crate) fn texture_memory(this: This) -> Option<(*const u8, ResourceDesc)> {
    if this.is_null() {
        return None;
    }
    let r = resource(this);
    (r.desc.dimension != BUFFER).then(|| (r.data().cast_const(), r.desc))
}

/// Whether `this` is one of our texture resources (it may be any address).
#[cfg(windows)]
pub(crate) fn is_texture(this: This) -> bool {
    let mut vtable = [0u8; 8];
    crate::host::read_into(this.cast::<u8>(), &mut vtable)
        && u64::from_le_bytes(vtable) == RESOURCE_FUNCTIONS.0.as_ptr() as u64
        && (made(this).is_some() || waits(this))
}

/// A buffer's host memory and size; `None` for textures.
pub(crate) fn buffer_bytes(this: This) -> Option<(*mut u8, u64)> {
    if this.is_null() {
        return None;
    }
    let r = resource(this);
    (r.desc.dimension == BUFFER).then(|| (r.data(), r.desc.width))
}

impl Resource {
    fn data(&self) -> *mut u8 {
        match self.backing {
            Backing::Committed(ref memory) => memory.ptr.as_ptr(),
            // SAFETY: placement checked the range lies inside the heap.
            Backing::Placed { heap, offset } => unsafe {
                (*heap.cast::<Heap>())
                    .memory
                    .ptr
                    .as_ptr()
                    .add(offset as usize)
            },
            Backing::Address(at) => at,
        }
    }
}

fn resource<'a>(this: This) -> &'a Resource {
    // SAFETY: called from resource methods only.
    unsafe { &*this.cast::<Resource>() }
}

// The creation methods' own arguments, passed on; `skip`: the largest mips
// the host copy of a texture goes without.
#[allow(clippy::too_many_arguments)]
fn hand_out_resource(
    desc: ResourceDesc,
    heap: HeapProperties,
    heap_flags: u32,
    backing: Backing,
    state: u32,
    clear: *const c_void,
    skip: u32,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    #[cfg(not(windows))]
    let _ = (state, clear, skip);
    let resource = Box::new(Resource {
        object: Object::new(&RESOURCE_FUNCTIONS),
        desc,
        heap,
        heap_flags,
        backing,
        #[cfg(windows)]
        host: Hosted::new(&desc, state, clear),
    });
    #[cfg(windows)]
    if desc.dimension == BUFFER {
        let at = std::ptr::from_ref::<Resource>(&resource) as usize;
        crate::shadow::register(at, resource.data() as u64, desc.width);
    } else {
        if resource.host.lazy {
            crate::memstat::add(&crate::memstat::UNMADE[0], resource.host.bytes);
            crate::memstat::add(&crate::memstat::UNMADE[1], 1);
        } else {
            let _ = resource.host.make(&desc, skip);
        }
        if resource.listed() {
            let at = std::ptr::from_ref::<Resource>(&resource) as usize;
            let mut targets = TARGETS.lock().unwrap_or_else(|e| e.into_inner());
            targets.entry(resource.data() as u64).or_default().push(at);
        }
    }
    object::hand_out(RESOURCE, resource, iid, &RESOURCE_IIDS, out)
}

#[cfg(windows)]
impl Resource {
    /// Whether it is in [`TARGETS`]: a texture the host writes that has a
    /// host copy or waits for its first use.
    fn listed(&self) -> bool {
        self.desc.dimension != BUFFER
            && self.desc.flags & 5 != 0
            && (self.host.lazy || self.host.made.get().is_some())
    }
}

#[cfg(windows)]
impl Drop for Resource {
    fn drop(&mut self) {
        // First: the slots that wait for its host copy are made under the
        // lock this takes (`crate::descriptor::make_waiting`).
        crate::descriptor::forget(std::ptr::from_ref::<Resource>(self) as usize);
        if self.listed() {
            let at = std::ptr::from_ref::<Resource>(self) as usize;
            let mut targets = TARGETS.lock().unwrap_or_else(|e| e.into_inner());
            let start = self.data() as u64;
            if let Some(list) = targets.get_mut(&start) {
                list.retain(|&r| r != at);
                if list.is_empty() {
                    targets.remove(&start);
                }
            }
        }
        #[cfg(feature = "diagnostics")]
        if self
            .host
            .made
            .get()
            .is_some_and(|h| std::sync::Arc::strong_count(h) > 1)
        {
            // Before lists held their textures, this freed a texture that
            // lists not yet run still named (a GPU page fault).
            static OUTLIVED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = OUTLIVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n.is_power_of_two() {
                tracing::info!(
                    n,
                    desc = ?self.desc,
                    "a title texture was released while a list not yet reset names its host copy"
                );
            }
        }
        match self.host.made.take() {
            // A running submission may still read the host copy: it is
            // retired when the last that holds it lets go.
            Some(host) => drop(host),
            None if self.host.lazy => {
                crate::memstat::sub(&crate::memstat::UNMADE[0], self.host.bytes);
                crate::memstat::sub(&crate::memstat::UNMADE[1], 1);
            }
            None => {}
        }
        crate::texture::forget(std::ptr::from_ref::<Resource>(self) as usize);
        if self.desc.dimension == BUFFER {
            crate::shadow::unregister(
                std::ptr::from_ref::<Resource>(self) as usize,
                self.data() as u64,
            );
        }
    }
}

pub(crate) extern "system" fn create_committed(
    _: This,
    heap: *const HeapProperties,
    heap_flags: u32,
    desc: *const ResourceDesc,
    state: u32,
    clear: *const c_void,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes both descriptions (null is tolerated).
    let (Some(&heap), Some(&desc)) = (unsafe { (heap.as_ref(), desc.as_ref()) }) else {
        return E_INVALIDARG;
    };
    let Some(info) = footprint(&desc) else {
        tracing::warn!(?desc, "no size for this resource");
        return E_INVALIDARG;
    };
    tracing::debug!(
        dimension = desc.dimension,
        size = info.size,
        heap = heap.kind,
        "committed resource"
    );
    let memory = match Memory::for_resource(info, heap.kind) {
        Ok(memory) => memory,
        Err(hr) => return hr,
    };
    hand_out_resource(
        desc,
        heap,
        heap_flags,
        Backing::Committed(memory),
        state,
        clear,
        0,
        iid,
        out,
    )
}

/// Device method 0x3f8, inferred from Grand Theft Auto VI's calls
/// (title+0x193e1b4: its back buffers, heap flag "allow display"): the
/// committed resource creation of the enhanced-barrier interfaces,
/// `(heap, heap flags, desc1, initial layout, clear value, protected
/// session, castable format count, castable formats, iid, out)`. The host
/// copy starts in the common state, as layouts are not tracked.
#[allow(clippy::too_many_arguments)] // the method's own arguments
pub(crate) extern "system" fn create_committed3(
    this: This,
    heap: *const HeapProperties,
    heap_flags: u32,
    desc: *const ResourceDesc,
    _layout: u32,
    clear: *const c_void,
    _protected: This,
    _castable_count: u32,
    _castable: *const u32,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    const COMMON: u32 = 0;
    create_committed(this, heap, heap_flags, desc, COMMON, clear, iid, out)
}

/// Registered page pools (64 KiB pages the title maps into tiled
/// resources by page index): first address and page count; a handle is an
/// index plus one, a removed pool leaves `None`.
static PAGE_POOLS: std::sync::Mutex<Vec<Option<(u64, u32)>>> = std::sync::Mutex::new(Vec::new());

fn page_pools() -> std::sync::MutexGuard<'static, Vec<Option<(u64, u32)>>> {
    PAGE_POOLS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Device method 0x250: `(address, page count, handle out)`.
pub(crate) extern "system" fn register_page_pool(
    _: This,
    address: u64,
    pages: u32,
    out: *mut usize,
) -> HRESULT {
    if out.is_null() || pages == 0 || !address.is_multiple_of(KIB_64) {
        return E_INVALIDARG;
    }
    tracing::debug!(address = format_args!("{address:#x}"), pages, "page pool");
    let mut pools = page_pools();
    pools.push(Some((address, pages)));
    // SAFETY: checked non-null; the title passes room for a handle.
    unsafe { out.write(pools.len()) };
    S_OK
}

/// Device method 0x258.
pub(crate) extern "system" fn unregister_page_pool(_: This, handle: usize) -> HRESULT {
    match page_pools().get_mut(handle.wrapping_sub(1)) {
        Some(pool @ Some(_)) => {
            *pool = None;
            S_OK
        }
        _ => E_INVALIDARG,
    }
}

/// Device method 0x428, one of the `ID3D12Device9` to `ID3D12Device11`
/// additions. From Grand Theft Auto VI's call: a resource at a virtual
/// address the title mapped itself, `(address, desc1, initial barrier
/// layout, clear value, iid, out)`, as `CreatePlacedResourceX` with
/// enhanced barriers. GPU addresses are CPU addresses, so the resource is
/// a view of that memory. `D3D12_RESOURCE_DESC1` starts like the plain
/// description; its sampler feedback region is not needed.
pub(crate) extern "system" fn create_at_address(
    _: This,
    address: u64,
    desc: *const ResourceDesc,
    layout: u32,
    clear: *const c_void,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    if address == 0 {
        return E_INVALIDARG;
    }
    tracing::debug!(
        address = format_args!("{address:#x}"),
        dimension = desc.dimension,
        width = desc.width,
        layout,
        "resource at an address"
    );
    const COMMON: u32 = 0;
    // shortcut: the title's own objects keep the size they are made in; only
    // textures it views without one are made again ([`care`]).
    #[cfg(windows)]
    let skip = match desc.dimension {
        BUFFER => 0,
        _ => over(&desc, crate::texcap::caps_now().new),
    };
    #[cfg(not(windows))]
    let skip = 0;
    hand_out_resource(
        desc,
        AT_ADDRESS,
        0,
        Backing::Address(address as *mut u8),
        COMMON,
        clear,
        skip,
        iid,
        out,
    )
}

/// The heap properties of memory the title mapped itself.
const AT_ADDRESS: HeapProperties = HeapProperties {
    kind: 1,
    cpu_pages: 0,
    pool: 0,
    creation_nodes: 1,
    visible_nodes: 1,
};

/// A texture the title placed itself ([`PLACED_TEXTURES`]).
#[cfg(windows)]
#[derive(Clone, Copy)]
struct Placed {
    /// Its resource, of ours.
    resource: usize,
    /// Where the title memory its host copy holds ends.
    end: u64,
    /// The largest mip views of it read so far: the memory of larger ones may
    /// be another texture's.
    largest: u32,
    /// On its way out ([`care`]): its bytes, counted as leaving; else 0.
    leaving: u64,
}

#[cfg(windows)]
impl Placed {
    /// Nothing new is given it.
    fn leave(&mut self, bytes: u64) {
        self.stay();
        self.leaving = bytes.max(1);
        crate::memstat::add(&crate::memstat::LEAVING, self.leaving);
    }

    /// It stays after all, or is gone.
    fn stay(&mut self) {
        crate::memstat::sub(&crate::memstat::LEAVING, self.leaving);
        self.leaving = 0;
    }
}

/// Address and description of a placed texture, and in the place of the
/// description's last word (padding) how many of its largest mips the host
/// copy goes without: the copies of a texture are beside each other, the
/// one with the most mips first.
#[cfg(windows)]
type PlacedKey = (u64, [u32; 14]);

/// Textures the title placed in its own memory and views without a
/// resource object (device 0x230, streamed and interface textures): made the
/// first time a draw reads one and evicted when
/// [`crate::watch::title_memory_remapped`] unmaps or remaps the range.
#[cfg(windows)]
type PlacedTextureMap = std::collections::BTreeMap<PlacedKey, Placed>;

#[cfg(all(test, windows))]
pub(crate) fn shares() -> bool {
    share()
}

/// Whether the copies of a placed texture are told apart by the mips they
/// have: views that read from another mip on, but find the same ones under
/// the size cap, share one. `VICTORY_TEXTURE_SHARE=0` makes one for each
/// largest mip read, as before.
#[cfg(windows)]
fn share() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_SHARE").is_none_or(|v| v != "0"))
}

#[cfg(windows)]
static PLACED_TEXTURES: std::sync::Mutex<PlacedTextureMap> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Incremented whenever [`evict_placed_textures`] removes an entry so
/// bindless heaps re-check placed texture slots.
#[cfg(windows)]
static PLACED_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(windows)]
pub(crate) fn placed_epoch() -> u64 {
    PLACED_EPOCH.load(std::sync::atomic::Ordering::Acquire)
}

/// A reference to a placed texture: eviction does not free it while held.
#[cfg(windows)]
pub(crate) struct HeldTexture(This);

#[cfg(windows)]
impl Drop for HeldTexture {
    fn drop(&mut self) {
        resource_release(self.0);
    }
}

/// A reference to `texture` if it is a placed texture alive now.
#[cfg(windows)]
pub(crate) fn hold_placed(texture: This) -> Option<HeldTexture> {
    let _map = PLACED_TEXTURES.lock().unwrap_or_else(|e| e.into_inner());
    let live = PLACED_LIVE.lock().unwrap_or_else(|e| e.into_inner());
    live.contains(&(texture as usize)).then(|| {
        object::add_ref(texture);
        HeldTexture(texture)
    })
}

/// The placed textures alive now, and the longest there was. Locked after
/// [`PLACED_TEXTURES`], changed with it.
#[cfg(windows)]
static PLACED_LIVE: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<usize>>> =
    std::sync::LazyLock::new(Default::default);
#[cfg(windows)]
static PLACED_LONGEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The placed textures alive now (resource objects).
#[cfg(windows)]
pub(crate) fn placed_live() -> std::collections::HashSet<usize> {
    let mut live: std::collections::HashSet<usize> = PLACED_TEXTURES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .map(|p| p.resource)
        .collect();
    let targets = TARGETS.lock().unwrap_or_else(|e| e.into_inner());
    live.extend(targets.values().flatten());
    live
}

/// The resource for a texture placed at `address` with the resource
/// description `desc` (as words) that a view reads from mip `largest` on,
/// made on first use.
#[cfg(windows)]
pub(crate) fn texture_at(address: u64, desc: &[u32; 14], largest: u32) -> Option<This> {
    if address == 0 {
        return None;
    }
    // The description's padding holds anything; the same texture was made
    // up to four times for it. It holds the mips left out here.
    let mut base = *desc;
    base[1] = 0;
    base[13] = 0;
    let d = described(&base);
    if d.dimension == BUFFER {
        return None;
    }
    if let Some(target) = target_at(address, &d) {
        return Some(target);
    }
    let largest = largest.min(u32::from(d.mip_levels).saturating_sub(1));
    // A copy with fewer mips than it might have, and the mips it goes without:
    // shown if there is no better.
    let mut shown: Option<(This, u32)> = None;
    {
        let mut map = PLACED_TEXTURES.lock().unwrap_or_else(|e| e.into_inner());
        let copies = if share() { [0, u32::MAX] } else { [largest; 2] };
        let [from, to] = copies.map(|k| (address, without(&base, k)));
        if let Some((key, placed)) = map.range_mut(from..=to).find(|(_, p)| p.leaving == 0) {
            let left_out = key.1[13];
            // A view that reads a larger mip than those before: the title
            // streamed it in. A copy the cap keeps from having more mips is
            // made again when there is room ([`care`]), a few at a time.
            let more = largest < placed.largest;
            if !share()
                || left_out <= largest
                || !more
                || left_out <= over(&d, crate::texcap::caps().new).max(largest)
            {
                placed.largest = placed.largest.min(largest);
                return Some(placed.resource as This);
            }
            shown = Some((placed.resource as This, left_out));
        }
    }
    let skip = over(&d, crate::texcap::caps_now().new).max(largest);
    if let Some((shown, left_out)) = shown
        && left_out <= skip
    {
        return Some(shown);
    }
    // Outside the lock: making one takes a while.
    let copy = place_copy(address, &base, &d, skip, largest);
    let shown = shown.map(|s| s.0);
    if copy.is_none()
        && let Some(host) = shown.and_then(made)
    {
        host.short.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    copy.or(shown)
}

/// The bytes of the host copy of texture `this`, if it is made.
#[cfg(windows)]
pub(crate) fn host_bytes(this: This) -> u64 {
    made(this).map_or(0, |h| h.bytes)
}

/// The title memory the host copy of texture `this` holds (address and
/// bytes: from its first mip kept on), if it is made and the title fills
/// it; not a use of it.
#[cfg(windows)]
pub(crate) fn held_memory(this: This) -> Option<(u64, u64)> {
    let host = made(this)?;
    let r = resource(this);
    (r.desc.flags & 0x7 == 0).then(|| (r.data() as u64, reach(&r.desc, host.skip)))
}

/// The host copy of title texture `this` if it is made, without making it:
/// for what is no use of it.
#[cfg(windows)]
pub(crate) fn made_resource<'a>(
    this: This,
) -> Option<&'a windows::Win32::Graphics::Direct3D12::ID3D12Resource> {
    made(this).map(|h| &h.texture)
}

/// Whether a view of placed texture `this` read mips whose memory was not
/// there: worth another look when memory is mapped.
#[cfg(windows)]
pub(crate) fn short(this: This) -> bool {
    made(this).is_some_and(|h| h.short.load(std::sync::atomic::Ordering::Relaxed))
}

/// The description words `base` as the key of the copy that goes without the
/// `skip` largest mips.
#[cfg(windows)]
fn without(base: &[u32; 14], skip: u32) -> [u32; 14] {
    let mut key = *base;
    key[13] = skip;
    key
}

/// A resource description from its words.
#[cfg(windows)]
fn described(words: &[u32; 14]) -> ResourceDesc {
    // SAFETY: 56 bytes in the description's layout.
    unsafe { words.as_ptr().cast::<ResourceDesc>().read_unaligned() }
}

/// The title memory of a texture from its start to the end of the mips from
/// `skip` on.
#[cfg(windows)]
fn reach(d: &ResourceDesc, skip: u32) -> u64 {
    match texture_layout(d) {
        Some(t) if skip > 0 && t.planes.len() == 1 => t.planes[0].layout.reach(skip),
        _ => footprint(d).map_or(1, |info| info.size.max(1)),
    }
}

/// Whether the title's pages of a placed texture are asked for before its
/// copy is made: tests make them over any memory unless they ask
/// ([`PAGES_IN_TEST`]).
#[cfg(windows)]
fn pages_asked() -> bool {
    #[cfg(test)]
    return PAGES_IN_TEST.get();
    #[cfg(not(test))]
    true
}

#[cfg(all(test, windows))]
thread_local! {
    /// Placed textures are made in this test only where their pages are.
    pub(crate) static PAGES_IN_TEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Makes the copy of the texture placed at `address` (description words
/// `base`, `d`) that goes without the `skip` largest mips, for a view that
/// reads from mip `largest` on; another thread's, if it was quicker.
#[cfg(windows)]
fn place_copy(
    address: u64,
    base: &[u32; 14],
    d: &ResourceDesc,
    skip: u32,
    largest: u32,
) -> Option<This> {
    const COMMON: u32 = 0;
    let size = reach(d, if share() { skip } else { largest });
    let end = address.saturating_add(size);
    // The title's pages for it may not be there yet (streaming): nothing to
    // show, and the console's whole address space would not fit in memory.
    if pages_asked() && !crate::texture::committed(address as *const u8, size as usize) {
        return None;
    }
    let mut out = std::ptr::null();
    let made = hand_out_resource(
        *d,
        AT_ADDRESS,
        0,
        Backing::Address(address as *mut u8),
        COMMON,
        std::ptr::null(),
        skip,
        &RESOURCE_IIDS[0],
        &mut out,
    );
    if made != S_OK {
        return None;
    }
    if self::made(out).is_none() && !waits(out) {
        resource_release(out);
        return None;
    }
    let key = (address, without(base, if share() { skip } else { largest }));
    let mut map = PLACED_TEXTURES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(placed) = map.get_mut(&key) {
        placed.largest = placed.largest.min(largest);
        placed.stay();
        let other = placed.resource;
        drop(map);
        resource_release(out);
        return Some(other as This);
    }
    PLACED_LIVE.lock().unwrap_or_else(|e| e.into_inner()).insert(out as usize);
    PLACED_LONGEST.fetch_max(end - address, std::sync::atomic::Ordering::AcqRel);
    map.insert(key, Placed { resource: out as usize, end, largest, leaving: 0 });
    crate::memstat::PLACED_COUNT.store(map.len() as u64, std::sync::atomic::Ordering::Relaxed);
    Some(out)
}

/// What [`care`] did: textures and their bytes.
#[cfg(windows)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct Cared {
    /// Made again with more mips.
    pub(crate) better: [u64; 2],
    /// On their way out: a copy with more mips of the same texture is there.
    pub(crate) twice: [u64; 2],
    /// On their way out for a smaller one: video memory is about to run out.
    pub(crate) cut: [u64; 2],
    /// Let go: nothing shows them (any more) or used them for a while.
    pub(crate) gone: [u64; 2],
    /// Of those looked at, how many have fewer mips than their views read,
    /// for the size cap.
    pub(crate) capped: [u64; 2],
}

/// What [`care`] is to do.
#[cfg(windows)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct Care {
    /// The side streamed textures may have now, and the one they are made
    /// again for.
    pub(crate) caps: crate::texcap::Caps,
    /// Bytes of host textures, and how many, to make again with more mips.
    pub(crate) better: [u64; 2],
    /// Bytes of host textures to let go for smaller ones.
    pub(crate) cut: u64,
    /// How long nothing showed or used a texture that is let go, in
    /// milliseconds, and how many bytes of them go at most, those idle the
    /// longest first (`None`: kept).
    pub(crate) idle: Option<(u64, u64)>,
}

/// Placed textures (resource objects) another copy has taken the place of,
/// lately, and how many had.
#[cfg(windows)]
static REPLACED: std::sync::Mutex<std::collections::VecDeque<usize>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());
#[cfg(windows)]
static REPLACES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The placed textures replaced after the first `seen`, and how many were by
/// now: slots that show one are made again by the next submission's plan.
#[cfg(windows)]
pub(crate) fn replaced_since(seen: u64) -> (std::collections::HashSet<usize>, u64) {
    use std::sync::atomic::Ordering::Acquire;
    if REPLACES.load(Acquire) == seen {
        return (Default::default(), seen);
    }
    let list = REPLACED.lock().unwrap_or_else(|e| e.into_inner());
    let now = REPLACES.load(Acquire);
    let new = now.saturating_sub(seen).min(list.len() as u64) as usize;
    (list.iter().skip(list.len() - new).copied().collect(), now)
}

#[cfg(windows)]
fn replaced(textures: &[usize]) {
    const KEPT: usize = 1 << 14;
    let mut list = REPLACED.lock().unwrap_or_else(|e| e.into_inner());
    list.extend(textures);
    while list.len() > KEPT {
        list.pop_front();
    }
    REPLACES.fetch_add(textures.len() as u64, std::sync::atomic::Ordering::Release);
}

/// Looks at some of the placed textures, others each time: makes those again
/// with more mips that were made when video memory was fuller, and lets go of
/// the copies they replace and of those nothing uses.
///
/// A copy that is replaced is on its way out ([`Placed::leaving`]): nothing
/// new is given it, the slots that show it are made anew from the other one
/// by the next submission's plan, which also uploads that one before its
/// lists run, and with nothing running (a slot rewritten under a running list
/// would show what was never uploaded). Once nothing holds it, it is let go.
/// Its object lives on for some seconds ([`crate::host::retire`]): a thread
/// may just have been given it.
#[cfg(windows)]
pub(crate) fn care(what: &Care) -> Cared {
    static CURSOR: std::sync::Mutex<Option<PlacedKey>> = std::sync::Mutex::new(None);
    let mut cursor = CURSOR.lock().unwrap_or_else(|e| e.into_inner());
    care_from(what, &mut cursor, 0..u64::MAX)
}

/// [`care`] for the textures placed in `within`, from `cursor` on.
#[cfg(windows)]
fn care_from(
    what: &Care,
    cursor: &mut Option<PlacedKey>,
    within: std::ops::Range<u64>,
) -> Cared {
    use std::sync::atomic::Ordering::Relaxed;
    /// Looked at in one go (the lock is taken by every draw that reads one),
    /// and in all.
    const AT_ONCE: usize = 1024;
    const LOOKED: usize = 8192;
    /// A texture no slot shows is in use if a list used it this lately.
    const LATELY_MS: u64 = 10_000;
    struct Better {
        /// Mips more than it has.
        mips: u32,
        key: PlacedKey,
        skip: u32,
        placed: Placed,
        /// The bytes it has and is to have.
        had: u64,
        bytes: u64,
    }
    let mut done = Cared::default();
    if textures() != Textures::Stream {
        return done;
    }
    let now = now_ms();
    let mut better: Vec<Better> = Vec::new();
    let mut large: Vec<(u64, PlacedKey, usize)> = Vec::new();
    // To let go (and whether they were on their way out when looked at), and
    // to have their slots made again.
    let mut gone: Vec<(PlacedKey, usize, bool)> = Vec::new();
    let mut shown: Vec<usize> = Vec::new();
    // Nothing showed or used them for long enough: how long, their bytes.
    let mut idle_ones: Vec<(u64, u64, PlacedKey, usize)> = Vec::new();
    for _ in 0..LOOKED / AT_ONCE {
        let mut map = PLACED_TEXTURES.lock().unwrap_or_else(|e| e.into_inner());
        let from = match *cursor {
            Some(key) => std::ops::Bound::Excluded(key),
            None => std::ops::Bound::Included((within.start, [0; 14])),
        };
        let to = std::ops::Bound::Excluded((within.end, [0; 14]));
        let mut looked = 0;
        let mut last = *cursor;
        // The copy with the most mips of the texture being looked at.
        let mut best = (*cursor).filter(|k| map.get(k).is_some_and(|p| p.leaving == 0));
        for (key, placed) in map.range_mut((from, to)).take(AT_ONCE) {
            looked += 1;
            last = Some(*key);
            let r = resource(placed.resource as This);
            let Some(host) = r.host.made.get().filter(|_| r.desc.flags & 0x7 == 0) else {
                continue;
            };
            let key = *key;
            let held = std::sync::Arc::strong_count(host) > 1;
            if placed.leaving > 0 {
                if held {
                    shown.push(placed.resource);
                } else {
                    gone.push((key, placed.resource, true));
                }
                continue;
            }
            let twin = |best: &PlacedKey| best.0 == key.0 && best.1[..13] == key.1[..13];
            if share() && best.as_ref().is_some_and(twin) {
                done.twice[0] += 1;
                done.twice[1] += host.bytes;
                placed.leave(host.bytes);
                shown.push(placed.resource);
                continue;
            }
            best = Some(key);
            // Held, a list or a slot has it ([`HostHeap::keep_with_view`]
            // holds the copy a slot shows): no shader can read one let go.
            let idle = now.saturating_sub(host.used.load(Relaxed));
            if !held && what.idle.is_some_and(|(ms, _)| idle >= ms) {
                idle_ones.push((idle, host.bytes, key, placed.resource));
                continue;
            }
            let d = described(&key.1);
            let has = host.skip;
            done.capped[0] += u64::from(has > placed.largest);
            done.capped[1] += 1;
            let may = over(&d, what.caps.again).max(placed.largest);
            if share() && may < has && what.better[0] > 0 && (held || idle < LATELY_MS) {
                // Each mip more takes four times the bytes.
                let had = host.bytes;
                let bytes = had << (2 * (has - may)).min(16);
                let placed = *placed;
                better.push(Better { mips: has - may, key, skip: may, placed, had, bytes });
            }
            if share() && what.cut > 0 && over(&d, what.caps.new) > has {
                large.push((host.bytes, key, placed.resource));
            }
        }
        *cursor = if looked < AT_ONCE { None } else { last };
        if looked < AT_ONCE {
            break;
        }
        // Who waits for the lock goes first.
        drop(map);
        std::thread::yield_now();
    }
    // The longest idle first, as many as the bytes allow (one at least).
    if let Some((_, most)) = what.idle {
        idle_ones.sort_unstable_by_key(|i| std::cmp::Reverse(i.0));
        let mut bytes = 0u64;
        for (_, size, key, resource) in idle_ones {
            if bytes >= most {
                break;
            }
            bytes = bytes.saturating_add(size);
            gone.push((key, resource, false));
        }
    }
    // Marks a copy to go, if it is still there.
    let mut leave = |key: &PlacedKey, resource: usize, bytes: u64| {
        let mut map = PLACED_TEXTURES.lock().unwrap_or_else(|e| e.into_inner());
        let Some(placed) = map.get_mut(key).filter(|p| p.resource == resource) else {
            return false;
        };
        placed.leave(bytes);
        shown.push(resource);
        true
    };
    large.sort_unstable_by_key(|l| std::cmp::Reverse(l.0));
    for (bytes, key, resource) in large {
        if done.cut[1] >= what.cut {
            break;
        }
        if leave(&key, resource, bytes) {
            done.cut[0] += 1;
            done.cut[1] += bytes;
        }
    }
    // Those with the most to gain first, the smallest of them first.
    better.sort_unstable_by_key(|b| (std::cmp::Reverse(b.mips), b.bytes));
    for b in better {
        if done.better[0] >= what.better[1] {
            break;
        }
        // One at least, however large.
        if done.better[0] > 0 && done.better[1] + b.bytes > what.better[0] {
            continue;
        }
        let d = described(&b.key.1);
        let base = without(&b.key.1, 0);
        let size = reach(&d, b.skip) as usize;
        let there = || cfg!(test) || crate::texture::committed(b.key.0 as *const u8, size);
        if !there() {
            continue;
        }
        let Some(copy) = place_copy(b.key.0, &base, &d, b.skip, b.placed.largest) else {
            continue;
        };
        // Unmapped meanwhile: what evicted the one it replaces may not have
        // seen it.
        if !leave(&b.key, b.placed.resource, b.had) || !there() {
            gone.push(((b.key.0, without(&base, b.skip)), copy as usize, false));
            continue;
        }
        done.better[0] += 1;
        done.better[1] += b.bytes;
    }
    if !shown.is_empty() {
        replaced(&shown);
    }
    let (went, bytes) = {
        let mut map = PLACED_TEXTURES.lock().unwrap_or_else(|e| e.into_inner());
        let mut live = PLACED_LIVE.lock().unwrap_or_else(|e| e.into_inner());
        let mut went = Vec::new();
        let mut bytes = 0;
        for (key, resource, leaving) in gone {
            // Not one that stays after all.
            let same = |p: &&mut Placed| p.resource == resource && (p.leaving > 0) == leaving;
            if let Some(placed) = map.get_mut(&key).filter(same) {
                placed.stay();
                map.remove(&key);
                live.remove(&resource);
                bytes += host_bytes(resource as This);
                // The table's own reference.
                went.push(HeldTexture(resource as This));
            }
        }
        crate::memstat::PLACED_COUNT.store(map.len() as u64, Relaxed);
        (went, bytes)
    };
    done.gone = [went.len() as u64, bytes];
    if !went.is_empty() {
        // What else names them (uploaded textures) forgets them.
        PLACED_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Release);
        crate::host::retire((went, crate::memstat::Leaving::new(bytes)));
    }
    done
}

/// What is done about textures whose title memory changes
/// (`VICTORY_TEXTURES`, to tell what texture streaming here costs and does):
/// `keep` uploads each once, whole, and keeps it whatever the title does
/// with its memory; `always` looks at each at every use, whole.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Textures {
    Stream,
    Keep,
    Always,
}

pub(crate) fn textures() -> Textures {
    static MODE: std::sync::OnceLock<Textures> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("VICTORY_TEXTURES").as_deref() {
        Ok("keep") => Textures::Keep,
        Ok("always") => Textures::Always,
        _ => Textures::Stream,
    })
}

/// Placed textures whose memory the runtime wrote since they were uploaded,
/// and how many were (bindless heaps take those they have not seen).
#[cfg(windows)]
static REWRITTEN: std::sync::Mutex<std::collections::VecDeque<usize>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());
#[cfg(windows)]
static REWRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many placed textures were evicted so far.
#[cfg(windows)]
pub(crate) static EVICTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many textures were rewritten so far.
#[cfg(windows)]
pub(crate) fn rewrites() -> u64 {
    REWRITES.load(std::sync::atomic::Ordering::Acquire)
}

/// The textures rewritten after the first `seen`, and how many were by now.
#[cfg(windows)]
pub(crate) fn rewritten_since(seen: u64) -> (Vec<usize>, u64) {
    let list = REWRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    let now = rewrites();
    let new = now.saturating_sub(seen).min(list.len() as u64) as usize;
    (list.iter().skip(list.len() - new).copied().collect(), now)
}

/// The runtime wrote `[start, end)`: the textures placed there are stale.
#[cfg(windows)]
pub(crate) fn textures_rewritten(start: u64, end: u64) {
    if textures() == Textures::Keep {
        return;
    }
    let found: Vec<usize> = {
        let map = PLACED_TEXTURES.lock().unwrap_or_else(|e| e.into_inner());
        let longest = PLACED_LONGEST.load(std::sync::atomic::Ordering::Acquire);
        let from = (start.saturating_sub(longest), [0u32; 14]);
        map.range(from..(end, [0u32; 14]))
            .filter(|(_, placed)| placed.end > start)
            .map(|(_, placed)| placed.resource)
            .collect()
    };
    if found.is_empty() {
        return;
    }
    for &r in &found {
        crate::texture::rewritten(r);
    }
    const KEPT: usize = 1 << 16;
    let mut list = REWRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    list.extend(&found);
    while list.len() > KEPT {
        list.pop_front();
    }
    REWRITES.fetch_add(found.len() as u64, std::sync::atomic::Ordering::Release);
}

/// Releases placed textures whose backing range overlaps `[start, end)`.
#[cfg(windows)]
pub(crate) fn evict_placed_textures(start: u64, end: u64) {
    if start >= end || textures() == Textures::Keep {
        return;
    }
    let mut evicted = Vec::new();
    {
        let mut map = PLACED_TEXTURES.lock().unwrap_or_else(|e| e.into_inner());
        let longest = PLACED_LONGEST.load(std::sync::atomic::Ordering::Acquire);
        let from = (start.saturating_sub(longest), [0u32; 14]);
        let over: Vec<_> = map
            .range(from..(end, [0u32; 14]))
            .filter(|(_, placed)| placed.end > start)
            .map(|(k, _)| *k)
            .collect();
        let mut live = PLACED_LIVE.lock().unwrap_or_else(|e| e.into_inner());
        for key in over {
            if let Some(mut placed) = map.remove(&key) {
                placed.stay();
                live.remove(&placed.resource);
                evicted.push(placed.resource);
            }
        }
    }
    EVICTED.fetch_add(evicted.len() as u64, std::sync::atomic::Ordering::Relaxed);
    if !evicted.is_empty() {
        PLACED_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Release);
        for res in evicted {
            resource_release(res as This);
        }
    }
}

#[cfg(all(test, windows))]
pub(crate) fn has_placed_texture(address: u64, desc: &[u32; 14]) -> bool {
    PLACED_TEXTURES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&(address, *desc))
}

pub(crate) extern "system" fn create_placed(
    _: This,
    heap: This,
    offset: u64,
    desc: *const ResourceDesc,
    state: u32,
    clear: *const c_void,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    if heap.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title passes one of our heaps.
    let h = unsafe { &*heap.cast::<Heap>() };
    let fits = footprint(&desc).is_some_and(|info| {
        offset.is_multiple_of(info.alignment)
            && offset
                .checked_add(info.size)
                .is_some_and(|end| end <= h.desc.size)
    });
    if !fits {
        tracing::warn!(
            ?desc,
            offset,
            heap = h.desc.size,
            "placed resource does not fit"
        );
        return E_INVALIDARG;
    }
    object::add_ref(heap);
    let backing = Backing::Placed { heap, offset };
    hand_out_resource(
        desc,
        h.desc.properties,
        h.desc.flags,
        backing,
        state,
        clear,
        0,
        iid,
        out,
    )
}

extern "system" fn resource_query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(
        RESOURCE,
        iid,
        object::is(iid, &RESOURCE_IIDS).then_some(this),
        out,
    )
}
extern "system" fn resource_release(this: This) -> u32 {
    object::release_boxed::<Resource>(this)
}
/// Memory is always mapped (subresources are not told apart yet); a null
/// `out` only asks for residency.
extern "system" fn map(this: This, _sub: u32, _read: *const c_void, out: *mut *mut u8) -> HRESULT {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a pointer.
        unsafe { out.write(resource(this).data()) };
    }
    S_OK
}
extern "system" fn unmap(_: This, _sub: u32, _written: *const c_void) {}
extern "system" fn resource_desc(this: This, out: *mut ResourceDesc) {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a description.
        unsafe { out.write(resource(this).desc) };
    }
}
/// Device method `GetCustomHeapProperties`, returning its struct through a
/// hidden first argument: what a standard heap type is as a custom heap.
/// Memory is unified and cache-coherent (as the architecture query says),
/// so every type is in pool L0, default heaps are not CPU-visible and the
/// others are write-back.
pub(crate) extern "system" fn custom_heap_properties(
    out: *mut HeapProperties,
    _: This,
    nodes: u32,
    kind: u32,
) -> *mut HeapProperties {
    const CUSTOM: u32 = 4;
    const NOT_AVAILABLE: u32 = 1;
    const WRITE_BACK: u32 = 3;
    const L0: u32 = 1;
    let nodes = nodes.max(1);
    let cpu_pages = match kind {
        1 => NOT_AVAILABLE,
        2 | 3 => WRITE_BACK,
        _ => 0,
    };
    let properties = HeapProperties {
        kind: CUSTOM,
        cpu_pages,
        pool: if cpu_pages == 0 { 0 } else { L0 },
        creation_nodes: nodes,
        visible_nodes: nodes,
    };
    // SAFETY: the title passes room for the result (null is tolerated).
    if let Some(out) = unsafe { out.as_mut() } {
        *out = properties;
    }
    out
}

extern "system" fn gpu_address(this: This) -> u64 {
    let r = resource(this);
    let at = r.data() as u64;
    // The console's GPU addresses fit in 40 bits; its ray-tracing builder
    // refuses others (diagnostics: which memory breaks that).
    #[cfg(feature = "diagnostics")]
    if at >> 40 != 0 {
        static ONCE: [std::sync::Once; 2] = [std::sync::Once::new(), std::sync::Once::new()];
        let backing = match r.backing {
            Backing::Committed(ref m) => format!("committed, attributes {:?}", m.attributes),
            Backing::Placed { offset, .. } => format!("placed in a heap at {offset:#x}"),
            Backing::Address(_) => "at a title address".into(),
        };
        ONCE[usize::from(r.desc.dimension == BUFFER)].call_once(|| {
            tracing::warn!(address = format_args!("{at:#x}"), backing, desc = ?r.desc, "a GPU address above 40 bits")
        });
    }
    at
}
extern "system" fn heap_properties(
    this: This,
    properties: *mut HeapProperties,
    flags: *mut u32,
) -> HRESULT {
    let r = resource(this);
    // SAFETY: either pointer may be null; otherwise the title passes room.
    unsafe {
        if let Some(p) = properties.as_mut() {
            *p = r.heap;
        }
        if let Some(f) = flags.as_mut() {
            *f = r.heap_flags;
        }
    }
    S_OK
}

/// IUnknown 3, object 4, device child 1, heap 1.
static HEAP_FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<HEAP, 24>());
    f[0] = heap_query as *const ();
    f[2] = heap_release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    f[8] = heap_desc as *const (); // 0x40
    Table(f)
};

/// IUnknown 3, object 4, device child 1, resource 10 (7 as on PC, then 3
/// console ones).
static RESOURCE_FUNCTIONS: Table<32> = {
    let mut f = object::d3d12_object(object::missing_table::<RESOURCE, 32>());
    f[0] = resource_query as *const ();
    f[2] = resource_release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    f[8] = map as *const (); // 0x40
    f[9] = unmap as *const (); // 0x48
    f[10] = resource_desc as *const (); // 0x50
    f[11] = gpu_address as *const (); // 0x58
    f[14] = heap_properties as *const (); // 0x70
    Table(f)
};

/// A committed texture of the title's with these flags (1 render target, 2
/// depth, 4 unordered access), in console state `state`.
#[cfg(test)]
pub(crate) fn test_texture(width: u64, height: u32, format: u32, flags: u32, state: u32) -> This {
    let heap = AT_ADDRESS;
    let desc = ResourceDesc {
        dimension: 3,
        alignment: 0,
        width,
        height,
        depth_or_array_size: 1,
        mip_levels: 1,
        format,
        sample_count: 1,
        sample_quality: 0,
        layout: 0,
        flags,
    };
    let (null, iid) = (std::ptr::null(), &RESOURCE_IIDS[0]);
    let mut out = null;
    assert_eq!(create_committed(null, &heap, 0, &desc, state, null.cast(), iid, &mut out), S_OK);
    out
}

/// Makes the textures placed at `address` again for sides up to `side`.
#[cfg(all(test, windows))]
pub(crate) fn test_care(address: u64, side: u64) {
    let caps = crate::texcap::Caps { new: side, again: side };
    crate::texcap::FORCED.set(caps);
    let what = Care { caps, better: [u64::MAX, u64::MAX], cut: 0, idle: None };
    care_from(&what, &mut None, address..address + 1);
}

#[cfg(test)]
pub(crate) fn test_buffer(width: u64) -> This {
    let heap = HeapProperties {
        kind: 1,
        cpu_pages: 0,
        pool: 0,
        creation_nodes: 0,
        visible_nodes: 0,
    };
    let desc = ResourceDesc {
        dimension: BUFFER,
        alignment: 0,
        width,
        height: 1,
        depth_or_array_size: 1,
        mip_levels: 1,
        format: 0,
        sample_count: 1,
        sample_quality: 0,
        layout: 1,
        flags: 0,
    };
    let mut out = std::ptr::null();
    let hr = create_committed(
        std::ptr::null(),
        &heap,
        0,
        &desc,
        0,
        std::ptr::null(),
        &RESOURCE_IIDS[0],
        &mut out,
    );
    assert_eq!(hr, S_OK);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;

    type Release = extern "system" fn(This) -> u32;
    type Map = extern "system" fn(This, u32, *const c_void, *mut *mut u8) -> HRESULT;
    type Address = extern "system" fn(This) -> u64;

    const PROPERTIES: HeapProperties = HeapProperties {
        kind: 1,
        cpu_pages: 0,
        pool: 0,
        creation_nodes: 0,
        visible_nodes: 0,
    };

    fn buffer(width: u64) -> ResourceDesc {
        ResourceDesc {
            dimension: BUFFER,
            alignment: 0,
            width,
            height: 1,
            depth_or_array_size: 1,
            mip_levels: 1,
            format: 0,
            sample_count: 1,
            sample_quality: 0,
            layout: 1,
            flags: 0,
        }
    }

    fn texture(width: u64, height: u32, format: u32, mips: u16) -> ResourceDesc {
        ResourceDesc {
            dimension: 3,
            width,
            height,
            format,
            mip_levels: mips,
            layout: 0,
            ..buffer(0)
        }
    }

    fn heap(size: u64) -> This {
        let desc = HeapDesc {
            size,
            properties: PROPERTIES,
            alignment: 0,
            flags: 0,
        };
        let mut heap = std::ptr::null();
        assert_eq!(
            create_heap(std::ptr::null(), &desc, &HEAP_IIDS[0], &mut heap),
            S_OK
        );
        heap
    }

    #[test]
    fn heaps_are_zeroed_aligned_memory() {
        let heap = heap(1 << 20);
        // SAFETY: a heap we just made.
        let memory = unsafe { &(*heap.cast::<Heap>()).memory };
        assert_eq!(memory.ptr.as_ptr() as u64 % KIB_64, 0);
        let mut back = std::mem::MaybeUninit::<HeapDesc>::uninit();
        slot::<extern "system" fn(This, *mut HeapDesc)>(heap, 0x40)(heap, back.as_mut_ptr());
        // SAFETY: written above.
        assert_eq!(unsafe { back.assume_init() }.size, 1 << 20);
        assert_eq!(slot::<Release>(heap, 0x10)(heap), 0);

        let bad = HeapDesc {
            size: 1 << 20,
            properties: PROPERTIES,
            alignment: 3,
            flags: 0,
        };
        let mut out = std::ptr::null();
        assert_eq!(
            create_heap(std::ptr::null(), &bad, &HEAP_IIDS[0], &mut out),
            E_INVALIDARG
        );
    }

    #[test]
    fn device_setup_heap_properties_and_resources_at_addresses() {
        let device = crate::device::device();
        // GetCustomHeapProperties (0xd0): the struct comes back through the
        // hidden first argument, which is also returned.
        type Custom =
            extern "system" fn(*mut HeapProperties, This, u32, u32) -> *mut HeapProperties;
        let mut p = PROPERTIES;
        let back = slot::<Custom>(device, 0xd0)(&mut p, device, 1, 2);
        assert_eq!(back, &raw mut p);
        assert_eq!(
            (
                p.kind,
                p.cpu_pages,
                p.pool,
                p.creation_nodes,
                p.visible_nodes
            ),
            (4, 3, 1, 1, 1),
            "upload: custom, write-back, L0"
        );
        slot::<Custom>(device, 0xd0)(&mut p, device, 1, 1);
        assert_eq!(p.cpu_pages, 1, "default: not CPU-visible");

        // 0x428: a buffer over memory the title owns, as GTA VI asks.
        type At = extern "system" fn(
            This,
            u64,
            *const ResourceDesc,
            u32,
            *const c_void,
            *const Iid,
            *mut This,
        ) -> HRESULT;
        let memory = vec![0u8; 0x1_0000];
        let address = memory.as_ptr() as u64;
        let mut r = std::ptr::null();
        let create = slot::<At>(device, 0x428);
        let desc = buffer(0x1_0000);
        let null = std::ptr::null();
        assert_eq!(
            create(
                device,
                address,
                &desc,
                u32::MAX,
                null,
                &RESOURCE_IIDS[0],
                &mut r
            ),
            0
        );
        assert_eq!(slot::<Address>(r, 0x58)(r), address);
        assert_eq!(slot::<Release>(r, 0x10)(r), 0);
        // The title's memory is still there after the release.
        assert_eq!(memory[0xffff], 0);
        assert_ne!(
            create(device, 0, &desc, u32::MAX, null, &RESOURCE_IIDS[0], &mut r),
            0
        );
    }

    #[test]
    fn sizes_cover_every_mip() {
        let size = |d: &ResourceDesc| footprint(d).map(|i| i.size);
        assert_eq!(size(&buffer(256)), Some(KIB_64));
        // Textures take their console layout and its alignment (4 KiB
        // standard swizzle when the description leaves it to the library):
        // 512x512 BC7 with 10 mips is 0x56000 bytes, 4 KiB aligned, as on
        // the console.
        let layout = |d: &ResourceDesc| texture_layout(d).unwrap().size;
        let rgba = texture(256, 256, 28, 0);
        assert_eq!(size(&rgba), Some(layout(&rgba).next_multiple_of(4096)));
        let bc7 = footprint(&texture(512, 512, 98, 10));
        let console = AllocationInfo {
            size: 0x56000,
            alignment: 0x1000,
        };
        assert_eq!(bc7, Some(console));
        assert!(layout(&rgba) >= 4 * (256 * 256 * 4) / 3, "every mip");
        // BC1 is 8 bytes per 4x4 block: 1024x1024, one mip = 512 KiB.
        assert_eq!(size(&texture(1024, 1024, 71, 1)), Some(512 << 10));
        assert_eq!(size(&texture(16, 16, 66, 1)), None, "1-bit format");
        // Linear layout (1) and an explicit mode (64 KiB display, 0x10a).
        let linear = ResourceDesc {
            layout: 1,
            ..texture(100, 60, 28, 1)
        };
        assert_eq!(
            texture_layout(&linear).unwrap().planes[0]
                .layout
                .desc
                .swizzle
                .0,
            0
        );
        let display = ResourceDesc {
            layout: 0x10a,
            ..texture(100, 60, 28, 1)
        };
        assert_eq!(
            texture_layout(&display).unwrap().planes[0]
                .layout
                .desc
                .swizzle
                .0,
            10
        );

        let mut info = AllocationInfo {
            size: 0,
            alignment: 0,
        };
        let descs = [buffer(100), texture(256, 256, 28, 0)];
        allocation_info(&mut info, std::ptr::null(), 0, 2, descs.as_ptr());
        assert_eq!(
            info,
            AllocationInfo {
                size: KIB_64 + size(&descs[1]).unwrap(),
                alignment: KIB_64
            }
        );
        allocation_info(&mut info, std::ptr::null(), 0, 1, &texture(1, 1, 999, 1));
        assert_eq!(info.size, u64::MAX);
    }

    #[test]
    fn upload_footprints_follow_the_pc_rules() {
        let null = std::ptr::null();
        let mut layouts = [NO_FOOTPRINT; 2];
        let (mut rows, mut sizes, mut total) = ([0; 2], [0; 2], 0);
        let mut footprints = |desc: &ResourceDesc, first, count| {
            let (l, r, s) = (layouts.as_mut_ptr(), rows.as_mut_ptr(), sizes.as_mut_ptr());
            copyable_footprints(null, desc, first, count, 0, l, r, s, &mut total);
            (layouts, rows, sizes, total)
        };
        // 100x60 RGBA8, 2 mips: 400-byte rows pitched to 512.
        let (l, r, s, t) = footprints(&texture(100, 60, 28, 2), 0, 2);
        assert_eq!((l[0].row_pitch, r[0], s[0]), (512, 60, 400));
        // Mip 1 (50x30, 200-byte rows) starts after 59 pitched rows and one
        // packed row, rounded up to 512.
        assert_eq!(l[1].offset, (512 * 59 + 400u64).next_multiple_of(512));
        assert_eq!((l[1].width, l[1].height, r[1]), (50, 30, 30));
        assert_eq!(t, l[1].offset + 256 * 29 + 200);
        // BC1: rows of 4x4 blocks, 8 bytes each.
        let (_, r, s, _) = footprints(&texture(64, 64, 71, 1), 0, 1);
        assert_eq!((r[0], s[0]), (16, 128));
        let (_, _, _, t) = footprints(&texture(64, 64, 71, 1), 1, 1);
        assert_eq!(t, u64::MAX, "no second subresource");
    }


    /// A 4x4 texture filled from a title buffer through our list and queue,
    /// then read back from the host GPU.
    #[cfg(windows)]
    #[test]
    fn buffer_data_reaches_the_host_texture() {
        use windows::Win32::Graphics::Direct3D12::*;
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let null = std::ptr::null();
        let iid = &RESOURCE_IIDS[0];
        const COPY_DEST: u32 = 0x400;
        let (mut image, mut source) = (null, null);
        let rgba = texture(4, 4, 28, 1);
        assert_eq!(
            create_committed(
                null,
                &PROPERTIES,
                0,
                &rgba,
                COPY_DEST,
                null.cast(),
                iid,
                &mut image
            ),
            S_OK
        );
        assert_eq!(
            create_committed(
                null,
                &PROPERTIES,
                0,
                &buffer(1024),
                0,
                null.cast(),
                iid,
                &mut source
            ),
            S_OK
        );
        let (data, _) = buffer_bytes(source).unwrap();
        // SAFETY: the buffer holds 1024 bytes.
        let bytes = unsafe { std::slice::from_raw_parts_mut(data, 1024) };
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }

        let queue_iid = guid(
            0x0ec870a6,
            0x5d7e,
            0x4c22,
            [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed],
        );
        let allocator_iid = guid(
            0x6102dee4,
            0xaf59,
            0x4b09,
            [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24],
        );
        let list_iid = guid(
            0x5b160d0f,
            0xac1b,
            0x4185,
            [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55],
        );
        let (mut queue, mut allocator, mut list) = (null, null, null);
        let queue_desc = [0u32; 4];
        assert_eq!(
            crate::queue::create(null, queue_desc.as_ptr().cast(), &queue_iid, &mut queue),
            S_OK
        );
        assert_eq!(
            crate::list::create_allocator(null, 0, &allocator_iid, &mut allocator),
            S_OK
        );
        assert_eq!(
            crate::list::create_list(null, 0, 0, allocator, null, &list_iid, &mut list),
            S_OK
        );

        #[repr(C)]
        struct Location {
            resource: This,
            kind: u32,
            footprint: PlacedFootprint,
        }
        let rows = PlacedFootprint {
            offset: 0,
            format: 28,
            width: 4,
            height: 4,
            depth: 1,
            row_pitch: 256,
        };
        let to = Location {
            resource: image,
            kind: 0,
            footprint: PlacedFootprint { offset: 0, ..rows },
        };
        let from = Location {
            resource: source,
            kind: 1,
            footprint: rows,
        };
        type Copy =
            extern "system" fn(This, *const Location, u32, u32, u32, *const Location, *const u8);
        slot::<Copy>(list, 0x80)(list, &to, 0, 0, 0, &from, null.cast());
        slot::<extern "system" fn(This) -> HRESULT>(list, 0x48)(list);
        slot::<extern "system" fn(This, u32, *const This)>(queue, 0x50)(queue, 1, &list);

        // Read the host texture back into a readback buffer.
        let readback_heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let readback_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: 1024,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut readback: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a readback buffer.
        unsafe {
            host.device
                .CreateCommittedResource(
                    &readback_heap,
                    D3D12_HEAP_FLAG_NONE,
                    &readback_desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut readback,
                )
                .unwrap();
        }
        let readback = readback.unwrap();
        let host_image = host_resource(image).unwrap();
        let back = host.new_list().unwrap();
        let (_, before) = host_transition(image, 0x800).unwrap();
        // The upload list holds the texture until it is reset.
        assert!(std::sync::Arc::strong_count(&host_copy(image).unwrap()) > 2);
        use windows::core::Interface;
        let out_to = Location {
            resource: readback.as_raw().cast_const().cast(),
            kind: 1,
            footprint: rows,
        };
        let out_from = Location {
            resource: host_image.as_raw().cast_const().cast(),
            kind: 0,
            footprint: to.footprint,
        };
        // SAFETY: an open list, live resources, locations in the host layout.
        unsafe {
            back.list.ResourceBarrier(&[crate::host::transition(
                host_image,
                D3D12_RESOURCE_STATES(before as i32),
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            back.list.CopyTextureRegion(
                (&raw const out_to).cast(),
                0,
                0,
                0,
                (&raw const out_from).cast(),
                None,
            );
        }
        back.close();
        host.execute(&[&back]);
        let mut mapped = std::ptr::null_mut();
        // SAFETY: the copy finished (execute waits); the buffer is 1024 bytes.
        let read = unsafe {
            readback
                .Map(
                    0,
                    Some(&D3D12_RANGE {
                        Begin: 0,
                        End: 1024,
                    }),
                    Some(&mut mapped),
                )
                .unwrap();
            std::slice::from_raw_parts(mapped.cast::<u8>(), 1024)
        };
        for row in 0..4 {
            let at = row * 256;
            assert_eq!(read[at..at + 16], bytes[at..at + 16], "row {row}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn copy_texture_region_reads_back_into_title_buffer() {
        if crate::host::host().is_none() {
            return;
        }
        let null = std::ptr::null();
        let iid = &RESOURCE_IIDS[0];
        let tex_desc = ResourceDesc {
            dimension: 3, // Texture2D
            alignment: 0,
            width: 4,
            height: 4,
            depth_or_array_size: 1,
            mip_levels: 1,
            format: 28, // R8G8B8A8_UNORM
            sample_count: 1,
            sample_quality: 0,
            layout: 0,
            flags: 0,
        };
        let (mut image, mut upload_buf, mut dst_aligned, mut dst_packed, mut dst_crop) =
            (null, null, null, null, null);
        assert_eq!(
            create_committed(null, &PROPERTIES, 0, &tex_desc, 0, null.cast(), iid, &mut image),
            S_OK
        );
        assert_eq!(
            create_committed(null, &PROPERTIES, 0, &buffer(1024), 0, null.cast(), iid, &mut upload_buf),
            S_OK
        );
        assert_eq!(
            create_committed(null, &PROPERTIES, 0, &buffer(2048), 0, null.cast(), iid, &mut dst_aligned),
            S_OK
        );
        assert_eq!(
            create_committed(null, &PROPERTIES, 0, &buffer(256), 0, null.cast(), iid, &mut dst_packed),
            S_OK
        );
        assert_eq!(
            create_committed(null, &PROPERTIES, 0, &buffer(1024), 0, null.cast(), iid, &mut dst_crop),
            S_OK
        );

        // Fill a 4x4 RGBA8 pattern (row_pitch 256): pixel (x, y) = [x*40+1, y*50+2, (x+y)*20+3, 0xff].
        let (up_ptr, _) = buffer_bytes(upload_buf).unwrap();
        // SAFETY: upload_buf holds 1024 bytes.
        let up = unsafe { std::slice::from_raw_parts_mut(up_ptr, 1024) };
        for y in 0..4usize {
            for x in 0..4usize {
                let at = y * 256 + x * 4;
                up[at..at + 4].copy_from_slice(&[
                    (x * 40 + 1) as u8,
                    (y * 50 + 2) as u8,
                    ((x + y) * 20 + 3) as u8,
                    0xff,
                ]);
            }
        }

        let queue_iid = guid(
            0x0ec870a6,
            0x5d7e,
            0x4c22,
            [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed],
        );
        let allocator_iid = guid(
            0x6102dee4,
            0xaf59,
            0x4b09,
            [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24],
        );
        let list_iid = guid(
            0x5b160d0f,
            0xac1b,
            0x4185,
            [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55],
        );
        let (mut queue, mut allocator, mut list) = (null, null, null);
        let queue_desc = [0u32; 4];
        assert_eq!(
            crate::queue::create(null, queue_desc.as_ptr().cast(), &queue_iid, &mut queue),
            S_OK
        );
        assert_eq!(
            crate::list::create_allocator(null, 0, &allocator_iid, &mut allocator),
            S_OK
        );
        assert_eq!(
            crate::list::create_list(null, 0, 0, allocator, null, &list_iid, &mut list),
            S_OK
        );

        #[repr(C)]
        struct Location {
            resource: This,
            kind: u32,
            footprint: PlacedFootprint,
        }
        type Copy =
            extern "system" fn(This, *const Location, u32, u32, u32, *const Location, *const [u32; 6]);
        let copy = slot::<Copy>(list, 0x80);

        let full_rows = PlacedFootprint {
            offset: 0,
            format: 28,
            width: 4,
            height: 4,
            depth: 1,
            row_pitch: 256,
        };
        let tex_loc = Location {
            resource: image,
            kind: 0,
            footprint: PlacedFootprint { offset: 0, ..full_rows },
        };
        let up_loc = Location {
            resource: upload_buf,
            kind: 1,
            footprint: full_rows,
        };
        // 1) Buffer -> Texture upload.
        copy(list, &tex_loc, 0, 0, 0, &up_loc, std::ptr::null());

        // 2) Texture -> Buffer readback at non-zero offset (256), 256-aligned row pitch.
        let aligned_loc = Location {
            resource: dst_aligned,
            kind: 1,
            footprint: PlacedFootprint {
                offset: 256,
                ..full_rows
            },
        };
        copy(list, &aligned_loc, 0, 0, 0, &tex_loc, std::ptr::null());

        // 3) Texture -> Buffer readback with unaligned row_pitch (16 bytes, packed).
        let packed_loc = Location {
            resource: dst_packed,
            kind: 1,
            footprint: PlacedFootprint {
                offset: 32,
                format: 28,
                width: 4,
                height: 4,
                depth: 1,
                row_pitch: 16,
            },
        };
        copy(list, &packed_loc, 0, 0, 0, &tex_loc, std::ptr::null());

        // 4) Texture -> Buffer sub-box readback: 2x2 crop from (1, 1) to (3, 3).
        let crop_box = [1u32, 1, 0, 3, 3, 1];
        let crop_loc = Location {
            resource: dst_crop,
            kind: 1,
            footprint: PlacedFootprint {
                offset: 0,
                format: 28,
                width: 2,
                height: 2,
                depth: 1,
                row_pitch: 256,
            },
        };
        copy(list, &crop_loc, 0, 0, 0, &tex_loc, &crop_box);

        // Both a source box and a whole subresource can fill only part of a footprint.
        let larger = PlacedFootprint {
            width: 6,
            height: 6,
            row_pitch: 24,
            ..full_rows
        };
        let partials = [(full_rows, 0, 0, Some(crop_box)), (larger, 1, 1, None)].map(
            |(footprint, x, y, region)| {
                let size = u64::from(footprint.row_pitch) * u64::from(footprint.height);
                let mut output = null;
                assert_eq!(
                    create_committed(
                        null, &PROPERTIES, 0, &buffer(size), 0, null.cast(), iid, &mut output,
                    ),
                    S_OK
                );
                let (data, len) = buffer_bytes(output).unwrap();
                // SAFETY: fill this mapped destination before the GPU copy.
                unsafe { data.write_bytes(0xa5, len as usize) };
                let dst = Location {
                    resource: output,
                    kind: 1,
                    footprint,
                };
                let src_box = region.as_ref().map_or(std::ptr::null(), std::ptr::from_ref);
                copy(list, &dst, x, y, 0, &tex_loc, src_box);
                (output, footprint, x, y, region)
            },
        );

        // 5) Console formats 118..=120 (3 B/texel) map to 4-byte PC formats
        // (45..=47): skipped cleanly without breaking Close() or touching dst.
        let mut dst_d16s8 = null;
        assert_eq!(
            create_committed(null, &PROPERTIES, 0, &buffer(1024), 0, null.cast(), iid, &mut dst_d16s8),
            S_OK
        );
        let d16s8_loc = Location {
            resource: dst_d16s8,
            kind: 1,
            footprint: PlacedFootprint {
                offset: 0,
                format: 118,
                width: 4,
                height: 4,
                depth: 1,
                row_pitch: 256,
            },
        };
        copy(list, &d16s8_loc, 0, 0, 0, &tex_loc, std::ptr::null());

        assert_eq!(slot::<extern "system" fn(This) -> HRESULT>(list, 0x48)(list), S_OK);
        slot::<extern "system" fn(This, u32, *const This)>(queue, 0x50)(queue, 1, &list);

        // Verify 256-aligned readback at offset 256 (leading 256 bytes untouched).
        let (al_ptr, _) = buffer_bytes(dst_aligned).unwrap();
        // SAFETY: dst_aligned holds 2048 bytes.
        let al = unsafe { std::slice::from_raw_parts(al_ptr, 2048) };
        assert!(al[..256].iter().all(|&b| b == 0), "bytes before offset touched");
        for row in 0..4 {
            let at = 256 + row * 256;
            assert_eq!(al[at..at + 16], up[row * 256..row * 256 + 16], "aligned row {row}");
        }

        // Verify unaligned packed readback (row_pitch 16, offset 32).
        let (pk_ptr, _) = buffer_bytes(dst_packed).unwrap();
        // SAFETY: dst_packed holds 256 bytes.
        let pk = unsafe { std::slice::from_raw_parts(pk_ptr, 256) };
        assert!(pk[..32].iter().all(|&b| b == 0), "bytes before packed offset touched");
        for row in 0..4 {
            let at = 32 + row * 16;
            assert_eq!(pk[at..at + 16], up[row * 256..row * 256 + 16], "packed row {row}");
        }

        // Verify 2x2 sub-box crop from (1, 1)..(3, 3).
        let (cr_ptr, _) = buffer_bytes(dst_crop).unwrap();
        // SAFETY: dst_crop holds 1024 bytes.
        let cr = unsafe { std::slice::from_raw_parts(cr_ptr, 1024) };
        for row in 0..2 {
            let src_at = (row + 1) * 256 + 4; // x=1, y=row+1
            let dst_at = row * 256;
            assert_eq!(cr[dst_at..dst_at + 8], up[src_at..src_at + 8], "crop row {row}");
        }

        // Verify skipped 118..=120 readback left its buffer untouched.
        let (d16_ptr, _) = buffer_bytes(dst_d16s8).unwrap();
        // SAFETY: dst_d16s8 holds 1024 bytes.
        let d16 = unsafe { std::slice::from_raw_parts(d16_ptr, 1024) };
        assert!(d16.iter().all(|&b| b == 0), "mismatched format 118 buffer touched");

        for (output, footprint, dst_x, dst_y, region) in partials {
            let [left, top, _, right, bottom, _] = region.unwrap_or([0, 0, 0, 4, 4, 1]);
            let (data, len) = buffer_bytes(output).unwrap();
            // SAFETY: the destination is still mapped after the submission.
            let pixels = unsafe { std::slice::from_raw_parts(data, len as usize) };
            for y in 0..footprint.height {
                for x in 0..footprint.width {
                    let at = (y * footprint.row_pitch + x * 4) as usize;
                    let expected: &[u8] = if (dst_x..dst_x + (right - left)).contains(&x)
                        && (dst_y..dst_y + (bottom - top)).contains(&y)
                    {
                        let src_at = ((top + y - dst_y) * 256 + (left + x - dst_x) * 4) as usize;
                        &up[src_at..src_at + 4]
                    } else {
                        &[0xa5; 4]
                    };
                    assert_eq!(
                        &pixels[at..at + 4], expected,
                        "pixel ({x}, {y}) in {footprint:?}"
                    );
                }
            }
        }
    }

    /// Textures the host writes are made on first use, by one thread of
    /// those that ask, in the state the title has them in; those the title
    /// fills and small ones are made at once.
    #[cfg(windows)]
    #[test]
    fn targets_are_made_on_first_use() {
        use windows::core::Interface;
        const TARGET: u32 = 1;
        const UNORDERED: u32 = 4;
        const SHADER_RESOURCE: u32 = 0x40 | 0x80;
        if crate::host::host().is_none() || !lazy_targets() {
            return; // no host GPU here, or made at once
        }
        let release = |t: This| assert_eq!(slot::<Release>(t, 0x10)(t), 0);
        for eager in [
            test_texture(512, 512, 28, 0, 0),
            test_texture(64, 64, 28, TARGET | UNORDERED, 0),
            test_buffer(1 << 20),
        ] {
            assert!(!waits(eager));
            release(eager);
        }
        let target = test_texture(1024, 512, 28, TARGET, SHADER_RESOURCE);
        assert!(waits(target) && made(target).is_none());
        assert!(is_texture(target));
        assert_eq!(skipped_mips(target), 0);
        assert!(waits(target), "asking what it is makes no use of it");
        let at = target as usize;
        let copies: Vec<usize> = std::thread::scope(|s| {
            let threads: Vec<_> = (0..4)
                .map(|_| s.spawn(move || host_resource(at as This).unwrap().as_raw() as usize))
                .collect();
            threads.into_iter().map(|t| t.join().unwrap()).collect()
        });
        assert!(copies.iter().all(|&c| c == copies[0]), "{copies:?}");
        assert!(!waits(target));
        let copy = host_copy(target).unwrap();
        assert_eq!(copy.state.load(std::sync::atomic::Ordering::Relaxed), SHADER_RESOURCE);
        assert!(copy.target);
        // SAFETY: a live resource.
        let d = unsafe { copy.texture.GetDesc() };
        assert_eq!((d.Width, d.Height, d.Flags.0), (1024, 512, TARGET as i32));
        assert!(targets_used(60_000)[1] > 0);
        release(target);
        // One never used is released without ever having had a host copy.
        release(test_texture(1024, 512, 28, TARGET, 0));
    }

    /// What a first use costs the thread that records it. Run with
    /// `cargo test --release -p victory-gpu -- --ignored --nocapture first_uses`.
    #[cfg(windows)]
    #[test]
    #[ignore = "benchmark"]
    fn benchmark_first_uses_of_targets() {
        if crate::host::host().is_none() {
            return;
        }
        for (name, width, height, format, flags) in [
            ("1440p RGBA16F target", 2560, 1440, 10, 1),
            ("1440p RGBA16F target and unordered", 2560, 1440, 10, 5),
            ("1440p R11G11B10F unordered", 2560, 1440, 26, 4),
            ("1440p depth and stencil", 2560, 1440, 20, 2),
            ("4K RGBA8 target", 3840, 2160, 28, 1),
            ("4096 x 4096 16-bit depth", 4096, 4096, 55, 2),
            ("720p RGBA8 target", 1280, 720, 28, 1),
        ] {
            const N: u32 = 16;
            let targets: Vec<This> =
                (0..N).map(|_| test_texture(width, height, format, flags, 0)).collect();
            let mut slowest = 0.0f64;
            let started = std::time::Instant::now();
            for &t in &targets {
                let one = std::time::Instant::now();
                assert!(host_resource(t).is_some());
                slowest = slowest.max(one.elapsed().as_secs_f64() * 1e3);
            }
            let ms = started.elapsed().as_secs_f64() * 1e3 / f64::from(N);
            let mib = host_bytes(targets[0]) as f64 / f64::from(1 << 20);
            println!("{name}: {mib:.1} MiB, {ms:.2} ms each, {slowest:.2} ms the slowest of {N}");
            for t in targets {
                assert_eq!(slot::<Release>(t, 0x10)(t), 0);
            }
        }
    }

    /// A title barrier on a texture nothing has used yet is a use of it only
    /// if it is written in one of the states; the state it is left in is the
    /// one it is made in.
    #[cfg(windows)]
    #[test]
    fn barriers_use_targets_that_are_written() {
        use std::sync::atomic::Ordering::Relaxed;
        const UNORDERED: u32 = 0x8;
        const COMMON: u32 = 0;
        const SHADER_RESOURCE: u32 = 0x40 | 0x80;
        let Some(host) = crate::host::host().filter(|_| lazy_targets()) else {
            return; // no host GPU here, or made at once
        };
        #[repr(C)]
        struct Barrier {
            kind: u32,
            flags: u32,
            first: This,
            second: u64,
            last: u64,
        }
        let transition = |t: This, before: u32, after: u32| Barrier {
            kind: 0,
            flags: 0,
            first: t,
            second: 0xffff_ffff | u64::from(before) << 32,
            last: u64::from(after),
        };
        let null = std::ptr::null();
        let [read, written, synced] = [0; 3].map(|_| test_texture(1024, 512, 28, 4, COMMON));
        let (mut allocator, mut list) = (null, null);
        let allocator_iid = guid(
            0x6102dee4,
            0xaf59,
            0x4b09,
            [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24],
        );
        let list_iid = guid(
            0x5b160d0f,
            0xac1b,
            0x4185,
            [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55],
        );
        assert_eq!(crate::list::create_allocator(null, 0, &allocator_iid, &mut allocator), S_OK);
        let made = crate::list::create_list(null, 0, 0, allocator, null, &list_iid, &mut list);
        assert_eq!(made, S_OK);
        let barrier = slot::<extern "system" fn(This, u32, *const Barrier) -> u32>(list, 0xd0);
        barrier(list, 1, &transition(read, COMMON, SHADER_RESOURCE));
        assert!(waits(read), "read, and nothing wrote it");
        // Sharing memory is no use either.
        let alias = Barrier { kind: 1, flags: 0, first: read, second: written as u64, last: 0 };
        barrier(list, 1, &alias);
        assert!(waits(read) && waits(written));
        barrier(list, 1, &transition(written, COMMON, UNORDERED));
        assert!(!waits(written));
        let sync = Barrier { kind: 2, flags: 0, first: synced, second: 0, last: 0 };
        barrier(list, 1, &sync);
        assert!(!waits(synced), "its shader writes are waited for");
        let commands = crate::list::host_list(list).unwrap();
        commands.close();
        host.execute(&[commands]);
        assert_eq!(host_copy(written).unwrap().state.load(Relaxed), UNORDERED);
        assert_eq!(host_copy(read).unwrap().state.load(Relaxed), SHADER_RESOURCE);
        for object in [list, allocator, read, written, synced] {
            assert_eq!(slot::<Release>(object, 0x10)(object), 0);
        }
    }

    /// A target whose first use is a clear holds what it was cleared to, and
    /// is read back by a copy recorded with it.
    #[cfg(windows)]
    #[test]
    fn a_target_first_used_by_a_clear_is_read_back() {
        const TARGET: u32 = 1;
        const RENDER_TARGET: u64 = 0x4;
        const COPY_SOURCE: u64 = 0x800;
        let Some(host) = crate::host::host().filter(|_| lazy_targets()) else {
            return; // no host GPU here, or made at once
        };
        let null = std::ptr::null();
        let target = test_texture(512, 256, 28, TARGET, 0);
        let pixels = test_buffer(512 * 4 * 256);
        let view = [0u64; 16];
        crate::descriptor::create_rtv(null, target, null.cast(), view.as_ptr() as u64);
        assert!(waits(target), "a view is no use");
        let (mut allocator, mut list) = (null, null);
        let allocator_iid = guid(
            0x6102dee4,
            0xaf59,
            0x4b09,
            [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24],
        );
        let list_iid = guid(
            0x5b160d0f,
            0xac1b,
            0x4185,
            [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55],
        );
        assert_eq!(crate::list::create_allocator(null, 0, &allocator_iid, &mut allocator), S_OK);
        let made = crate::list::create_list(null, 0, 0, allocator, null, &list_iid, &mut list);
        assert_eq!(made, S_OK);
        type Clear = extern "system" fn(This, u64, *const [f32; 4], u32, *const u8);
        let magenta = [1.0, 0.0, 1.0, 0.0];
        slot::<Clear>(list, 0x1a0)(list, view.as_ptr() as u64, &magenta, 0, null.cast());
        assert!(!waits(target));
        let barrier: [u64; 4] =
            [0, target as u64, 0xffff_ffff | RENDER_TARGET << 32, COPY_SOURCE];
        type Barriers = extern "system" fn(This, u32, *const u64) -> u32;
        slot::<Barriers>(list, 0xd0)(list, 1, barrier.as_ptr());
        #[repr(C)]
        struct Location {
            resource: This,
            kind: u32,
            footprint: PlacedFootprint,
        }
        let rows = PlacedFootprint {
            offset: 0,
            format: 28,
            width: 512,
            height: 256,
            depth: 1,
            row_pitch: 2048,
        };
        let to = Location { resource: pixels, kind: 1, footprint: rows };
        let from = Location { resource: target, kind: 0, footprint: rows };
        type Copy =
            extern "system" fn(This, *const Location, u32, u32, u32, *const Location, *const u8);
        slot::<Copy>(list, 0x80)(list, &to, 0, 0, 0, &from, null.cast());
        let commands = crate::list::host_list(list).unwrap();
        commands.close();
        host.execute(&[commands]);
        let (data, len) = buffer_bytes(pixels).unwrap();
        // SAFETY: the buffer's memory, written before `execute` returned.
        let bytes = unsafe { std::slice::from_raw_parts(data, len as usize) };
        assert!(bytes.chunks(4).all(|p| p == [255, 0, 255, 0]));
        for object in [list, allocator, target, pixels] {
            assert_eq!(slot::<Release>(object, 0x10)(object), 0);
        }
    }

    /// A title list and its allocator.
    #[cfg(windows)]
    fn title_list() -> (This, This) {
        let null = std::ptr::null();
        let (mut allocator, mut list) = (null, null);
        let allocator_iid =
            guid(0x6102dee4, 0xaf59, 0x4b09, [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24]);
        let list_iid = guid(0x5b160d0f, 0xac1b, 0x4185, [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55]);
        assert_eq!(crate::list::create_allocator(null, 0, &allocator_iid, &mut allocator), S_OK);
        let made = crate::list::create_list(null, 0, 0, allocator, null, &list_iid, &mut list);
        assert_eq!(made, S_OK);
        (allocator, list)
    }

    /// A copy location as titles pass it.
    #[cfg(windows)]
    #[repr(C)]
    struct Location {
        resource: This,
        kind: u32,
        footprint: PlacedFootprint,
    }

    #[cfg(windows)]
    type CopyRegion = extern "system" fn(This, *const Location, u32, u32, u32, *const Location, *const u8);
    #[cfg(windows)]
    type Barriers = extern "system" fn(This, u32, *const [u64; 4]) -> u32;

    /// A title transition barrier of a whole texture.
    #[cfg(windows)]
    fn transition_words(texture: This, before: u32, after: u32) -> [u64; 4] {
        [0, texture as u64, 0xffff_ffff | u64::from(before) << 32, u64::from(after)]
    }

    /// Copies a whole host texture into a readback buffer, in `list`.
    #[cfg(windows)]
    fn read_back(
        host: &crate::host::Host,
        list: &crate::host::HostList,
        texture: &windows::Win32::Graphics::Direct3D12::ID3D12Resource,
        width: u32,
        height: u32,
    ) -> *const u8 {
        use windows::Win32::Graphics::Direct3D12::*;
        let (back, seen) = host.readback_buffer(u64::from(width * 4 * height)).unwrap();
        // SAFETY: a live texture.
        let format = unsafe { texture.GetDesc() }.Format;
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(&back) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: format,
                        Width: width,
                        Height: height,
                        Depth: 1,
                        RowPitch: width * 4,
                    },
                },
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
        };
        // SAFETY: an open list; the texture is in the copy source state there.
        unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
        list.keep(&back);
        seen
    }

    /// A target whose first use is a copy into it is made then, in the state
    /// the title had it in, and holds what the copy wrote.
    #[cfg(windows)]
    #[test]
    fn a_target_first_used_by_a_copy_holds_what_it_was_given() {
        use std::sync::atomic::Ordering::Relaxed;
        const TARGET: u32 = 1;
        const COPY_DEST: u32 = 0x400;
        const COPY_SOURCE: u32 = 0x800;
        const SHADER_RESOURCE: u32 = 0x40 | 0x80;
        let Some(host) = crate::host::host().filter(|_| lazy_targets()) else {
            return; // no host GPU here, or made at once
        };
        let null: This = std::ptr::null();
        let target = test_texture(512, 256, 28, TARGET, SHADER_RESOURCE);
        let (source, pixels) = (test_buffer(512 * 4 * 256), test_buffer(512 * 4 * 256));
        assert!(waits(target));
        let (data, len) = buffer_bytes(source).unwrap();
        // SAFETY: the buffer's own memory.
        let given = unsafe { std::slice::from_raw_parts_mut(data, len as usize) };
        given.iter_mut().enumerate().for_each(|(i, b)| *b = (i * 7 % 253) as u8);
        let (allocator, list) = title_list();
        let barrier = |before, after| transition_words(target, before, after);
        slot::<Barriers>(list, 0xd0)(list, 1, &barrier(SHADER_RESOURCE, COPY_DEST));
        assert!(!waits(target), "a barrier to a state it is written in");
        let copy = host_copy(target).unwrap();
        assert_eq!(copy.state.load(Relaxed), SHADER_RESOURCE, "made in the title's state");
        let rows = PlacedFootprint { offset: 0, format: 28, width: 512, height: 256, depth: 1, row_pitch: 2048 };
        let into = Location { resource: target, kind: 0, footprint: rows };
        let from = Location { resource: source, kind: 1, footprint: rows };
        slot::<CopyRegion>(list, 0x80)(list, &into, 0, 0, 0, &from, null.cast());
        slot::<Barriers>(list, 0xd0)(list, 1, &barrier(COPY_DEST, COPY_SOURCE));
        let to = Location { resource: pixels, kind: 1, footprint: rows };
        let back = Location { resource: target, kind: 0, footprint: rows };
        slot::<CopyRegion>(list, 0x80)(list, &to, 0, 0, 0, &back, null.cast());
        let commands = crate::list::host_list(list).unwrap();
        commands.close();
        host.execute(&[commands]);
        assert_eq!(copy.state.load(Relaxed), COPY_SOURCE);
        let (out, n) = buffer_bytes(pixels).unwrap();
        // SAFETY: the buffer's memory, written before `execute` returned.
        let read = unsafe { std::slice::from_raw_parts(out, n as usize) };
        assert!(read == &given[..], "the bytes the copy wrote");
        drop(copy);
        for object in [list, allocator, target, source, pixels] {
            assert_eq!(slot::<Release>(object, 0x10)(object), 0);
        }
    }

    /// A target first bound for a draw is made then, in the state the title
    /// had it in, and holds what the draw wrote.
    #[cfg(windows)]
    #[test]
    fn a_target_first_bound_for_a_draw_holds_what_it_drew() {
        use std::sync::atomic::Ordering::Relaxed;
        use windows::Win32::Foundation::RECT;
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob};
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::Win32::Graphics::Dxgi::Common::*;
        use windows::core::{PCSTR, s};
        const TARGET: u32 = 1;
        const COMMON: u32 = 0;
        const RENDER_TARGET: u32 = 0x4;
        const COPY_SOURCE: u32 = 0x800;
        let Some(host) = crate::host::host().filter(|_| lazy_targets()) else {
            return; // no host GPU here, or made at once
        };
        const SOURCE: &str = r#"
#define RS "RootFlags(0)"
[RootSignature(RS)]
float4 vs(uint id : SV_VertexID) : SV_Position {
    float2 uv = float2((id << 1) & 2, id & 2);
    return float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
}
[RootSignature(RS)]
float4 ps() : SV_Target { return float4(0.0, 1.0, 1.0, 1.0); }
"#;
        fn compile(entry: PCSTR, target: PCSTR) -> ID3DBlob {
            let mut code: Option<ID3DBlob> = None;
            // SAFETY: static source and names; the out pointer is ours.
            unsafe {
                D3DCompile(SOURCE.as_ptr().cast(), SOURCE.len(), s!("draw"), None, None, entry, target, 0, 0, &mut code, None)
            }
            .unwrap();
            code.unwrap()
        }
        let code = |b: &ID3DBlob| D3D12_SHADER_BYTECODE {
            // SAFETY: the blob lives through the pipeline's making.
            pShaderBytecode: unsafe { b.GetBufferPointer() },
            // SAFETY: as above.
            BytecodeLength: unsafe { b.GetBufferSize() },
        };
        let (vs, ps) = (compile(s!("vs"), s!("vs_5_1")), compile(s!("ps"), s!("ps_5_1")));
        // SAFETY: the blob's own bytes; a root signature the compiler embedded.
        let root: ID3D12RootSignature = unsafe {
            host.device
                .CreateRootSignature(0, std::slice::from_raw_parts(vs.GetBufferPointer().cast::<u8>(), vs.GetBufferSize()))
        }
        .unwrap();
        let target = test_texture(512, 256, 28, TARGET, COMMON);
        let view = [0u64; 16];
        crate::descriptor::create_rtv(std::ptr::null(), target, std::ptr::null(), view.as_ptr() as u64);
        assert!(waits(target), "a view is no use");
        let list = host.new_list().unwrap();
        let mut state = crate::draw::State::default();
        state.set_targets(&list, &[view.as_ptr() as u64], 0);
        assert!(!waits(target), "bound for a draw");
        let copy = host_copy(target).unwrap();
        assert_eq!(copy.state.load(Relaxed), COMMON, "made in the title's state");
        // SAFETY: a live texture.
        let format = unsafe { copy.texture.GetDesc() }.Format;
        let mut formats = [DXGI_FORMAT_UNKNOWN; 8];
        formats[0] = format;
        let mut blend = D3D12_BLEND_DESC::default();
        blend.RenderTarget[0].RenderTargetWriteMask = D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8;
        let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            // SAFETY: borrowed for the call; the test holds the root.
            pRootSignature: unsafe { std::mem::transmute_copy(&root) },
            VS: code(&vs),
            PS: code(&ps),
            BlendState: blend,
            SampleMask: u32::MAX,
            RasterizerState: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_NONE,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            NumRenderTargets: 1,
            RTVFormats: formats,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        // SAFETY: a complete description with live shaders and root.
        let pso: ID3D12PipelineState = unsafe { host.device.CreateGraphicsPipelineState(&desc) }.unwrap();
        crate::list::ensure_state(&list, target, RENDER_TARGET);
        // SAFETY: an open list; our root signature and pipeline; the target
        // bound above.
        unsafe {
            let l = &list.list;
            l.SetGraphicsRootSignature(&root);
            l.SetPipelineState(&pso);
            l.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            l.RSSetViewports(&[D3D12_VIEWPORT { Width: 512.0, Height: 256.0, MaxDepth: 1.0, ..Default::default() }]);
            l.RSSetScissorRects(&[RECT { left: 0, top: 0, right: 512, bottom: 256 }]);
            l.DrawInstanced(3, 1, 0, 0);
        }
        crate::list::ensure_state(&list, target, COPY_SOURCE);
        let seen = read_back(host, &list, &copy.texture, 512, 256);
        list.close();
        host.execute(&[&list]);
        // SAFETY: the readback buffer the list kept, written before
        // `execute` returned.
        let read = unsafe { std::slice::from_raw_parts(seen, 512 * 4 * 256) };
        assert!(read.chunks(4).all(|p| p == [0, 255, 255, 255]), "{:?}", &read[..8]);
        drop(copy);
        assert_eq!(slot::<Release>(target, 0x10)(target), 0);
    }

    /// A depth texture nothing has used yet that a dispatch writes as an
    /// image is made then, with its twins, in its state; the twins are
    /// copied into it.
    #[cfg(windows)]
    #[test]
    fn a_depth_target_first_written_as_an_image_gets_its_twins() {
        use std::sync::atomic::Ordering::Relaxed;
        const DEPTH_UNORDERED: u32 = 2 | 4;
        // 16-bit depth with 8-bit stencil, the console's.
        const D16_S8: u32 = 118;
        let Some(host) = crate::host::host().filter(|_| lazy_targets()) else {
            return; // no host GPU here, or made at once
        };
        if !crate::twin::wanted(D16_S8, DEPTH_UNORDERED) {
            return; // `VICTORY_DEPTH_TWINS=0`
        }
        let depth = test_texture(512, 512, D16_S8, DEPTH_UNORDERED, 0);
        assert!(waits(depth));
        let list = host.new_list().unwrap();
        // As `draw::readable` for a table entry a dispatch writes.
        if depth_twin(depth).is_none() {
            assert_eq!(slot::<Release>(depth, 0x10)(depth), 0);
            return; // the host makes no such texture
        }
        assert!(!waits(depth), "made with its twins");
        let copy = host_copy(depth).unwrap();
        assert_eq!(copy.state.load(Relaxed), 0, "made in the title's state");
        crate::twin::note(depth, None);
        assert!(crate::twin::resolve(&list), "the twins copied in");
        list.close();
        host.execute(&[&list]);
        // SAFETY: a plain query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
        drop(copy);
        assert_eq!(slot::<Release>(depth, 0x10)(depth), 0);
    }

    /// Targets made by first uses on recording threads are made in the state
    /// the title had them in, and their lists' preludes move them from there
    /// at submission: what each list cleared its own to is read back.
    #[cfg(windows)]
    #[test]
    fn targets_made_on_recording_threads_start_in_their_state() {
        use std::sync::atomic::Ordering::Relaxed;
        const TARGET: u32 = 1;
        const SHADER_RESOURCE: u32 = 0x40 | 0x80;
        const RENDER_TARGET: u32 = 0x4;
        const COPY_SOURCE: u32 = 0x800;
        if crate::host::host().is_none() || !lazy_targets() {
            return; // no host GPU here, or made at once
        }
        struct Sent<T>(T);
        // SAFETY: D3D12 objects are free-threaded; each list is recorded by
        // one thread, then run by this one.
        unsafe impl<T> Send for Sent<T> {}
        let targets: Vec<usize> =
            (0..4).map(|_| test_texture(512, 256, 28, TARGET, SHADER_RESOURCE) as usize).collect();
        let recorded: Vec<Sent<(crate::host::HostList, usize)>> = std::thread::scope(|s| {
            let threads: Vec<_> = targets
                .iter()
                .enumerate()
                .map(|(i, &t)| {
                    s.spawn(move || {
                        let host = crate::host::host().unwrap();
                        let t = t as This;
                        let list = host.new_list().unwrap();
                        assert!(waits(t));
                        crate::list::ensure_state(&list, t, RENDER_TARGET);
                        let copy = host_copy(t).unwrap();
                        assert_eq!(copy.state.load(Relaxed), SHADER_RESOURCE, "moved at submission");
                        let rtv = host.rtv(&copy.texture, None);
                        let color = [i as f32 * 0.2, 1.0, 0.0, 1.0];
                        // SAFETY: an open list; the target is in the render
                        // target state there.
                        unsafe { list.list.ClearRenderTargetView(rtv, &color, None) };
                        crate::list::ensure_state(&list, t, COPY_SOURCE);
                        let seen = read_back(host, &list, &copy.texture, 512, 256);
                        list.close();
                        Sent((list, seen as usize))
                    })
                })
                .collect();
            threads.into_iter().map(|t| t.join().unwrap()).collect()
        });
        let host = crate::host::host().unwrap();
        let lists: Vec<&crate::host::HostList> = recorded.iter().map(|r| &r.0.0).collect();
        host.execute(&lists);
        for (i, r) in recorded.iter().enumerate() {
            // SAFETY: the readback buffer its list kept, written before
            // `execute` returned.
            let read = unsafe { std::slice::from_raw_parts(r.0.1 as *const u8, 512 * 4 * 256) };
            let red = (i as f32 * 0.2 * 255.0).round() as u8;
            assert!(read.chunks(4).all(|p| p == [red, 255, 0, 255]), "target {i}: {:?}", &read[..4]);
            let copy = host_copy(targets[i] as This).unwrap();
            assert_eq!(copy.state.load(Relaxed), COPY_SOURCE);
        }
        drop(recorded);
        for t in targets {
            assert_eq!(slot::<Release>(t as This, 0x10)(t as This), 0);
        }
    }

    /// The first copy into part of a texture the title fills, which nothing
    /// uploaded, goes over zeros: what it does not write reads as zeros
    /// whatever the copy's memory held before.
    #[cfg(windows)]
    #[test]
    fn a_first_copy_into_a_texture_the_title_fills_goes_over_zeros() {
        use std::sync::atomic::Ordering::Relaxed;
        const COPY_DEST: u32 = 0x400;
        const COPY_SOURCE: u32 = 0x800;
        let Some(host) = crate::host::host() else {
            return;
        };
        if crate::texture::copy_over() != crate::texture::Over::Zeros {
            return; // `VICTORY_COPY_OVER_TITLE`
        }
        let null: This = std::ptr::null();
        let texture = test_texture(256, 256, 28, 0, COPY_DEST);
        let (source, pixels) = (test_buffer(64 * 4 * 64), test_buffer(256 * 4 * 256));
        let (data, len) = buffer_bytes(source).unwrap();
        // SAFETY: the buffer's own memory.
        unsafe { std::slice::from_raw_parts_mut(data, len as usize) }.fill(0x5a);
        let zeros = crate::list::OVER_ZEROS.load(Relaxed);
        let (allocator, list) = title_list();
        let part = PlacedFootprint { offset: 0, format: 28, width: 64, height: 64, depth: 1, row_pitch: 256 };
        let into = Location { resource: texture, kind: 0, footprint: part };
        let from = Location { resource: source, kind: 1, footprint: part };
        slot::<CopyRegion>(list, 0x80)(list, &into, 16, 8, 0, &from, null.cast());
        slot::<Barriers>(list, 0xd0)(list, 1, &transition_words(texture, COPY_DEST, COPY_SOURCE));
        let whole = PlacedFootprint { offset: 0, format: 28, width: 256, height: 256, depth: 1, row_pitch: 1024 };
        let to = Location { resource: pixels, kind: 1, footprint: whole };
        let back = Location { resource: texture, kind: 0, footprint: whole };
        slot::<CopyRegion>(list, 0x80)(list, &to, 0, 0, 0, &back, null.cast());
        assert!(crate::list::OVER_ZEROS.load(Relaxed) > zeros, "over zeros");
        let commands = crate::list::host_list(list).unwrap();
        commands.close();
        host.execute(&[commands]);
        let (out, n) = buffer_bytes(pixels).unwrap();
        // SAFETY: the buffer's memory, written before `execute` returned.
        let read = unsafe { std::slice::from_raw_parts(out, n as usize) };
        for y in 0..256usize {
            for x in 0..256usize {
                let inside = (16..80).contains(&x) && (8..72).contains(&y);
                let want = if inside { 0x5a } else { 0 };
                assert_eq!(read[(y * 256 + x) * 4], want, "at {x}, {y}");
            }
        }
        for object in [list, allocator, texture, source, pixels] {
            assert_eq!(slot::<Release>(object, 0x10)(object), 0);
        }
    }

    /// A 2048x2048 BC7 texture with all its mips, as description words.
    #[cfg(windows)]
    const STREAMED: [u32; 14] = [3, 0, 0, 0, 2048, 0, 2048, 12 << 16 | 1, 98, 1, 0, 0, 0, 0];

    #[cfg(windows)]
    fn caps(new: u64, again: u64) -> crate::texcap::Caps {
        let caps = crate::texcap::Caps { new, again };
        crate::texcap::FORCED.set(caps);
        caps
    }

    /// The mips the copies of the texture placed at `address` go without, and
    /// those of the ones on their way out.
    #[cfg(windows)]
    fn copies(address: u64) -> [Vec<u32>; 2] {
        let map = PLACED_TEXTURES.lock().unwrap_or_else(|e| e.into_inner());
        let all = || map.iter().filter(|(k, _)| k.0 == address);
        [false, true].map(|leaving| {
            all().filter(|(_, p)| (p.leaving > 0) == leaving).map(|(k, _)| k.1[13]).collect()
        })
    }

    /// Under a size cap all views of a texture share one copy, whichever mip
    /// they start at; a better one is made for a view that reads a mip the
    /// title streamed in, and shared from then on.
    #[cfg(windows)]
    #[test]
    fn views_of_a_streamed_texture_share_a_copy() {
        if crate::host::host().is_none() || !share() {
            return;
        }
        let memory = Memory::zeroed(8 << 20, KIB_64).unwrap();
        let address = memory.address();
        let at = |largest| texture_at(address, &STREAMED, largest).unwrap();
        caps(256, 256);
        let first = at(3);
        assert_eq!(skipped_mips(first), 3);
        for largest in [0, 1, 2, 3] {
            assert_eq!(at(largest), first, "read from mip {largest} on");
        }
        // One that reads less than the cap allows has a copy of its own ...
        let small = at(5);
        assert_eq!(small, first, "which the better copy serves too");
        // ... unless there is no better.
        let other = address + (4 << 20);
        let at_other = |largest| texture_at(other, &STREAMED, largest).unwrap();
        let small = at_other(5);
        assert_eq!(skipped_mips(small), 5);
        caps(1024, 1024);
        let better = at_other(2);
        assert_eq!(skipped_mips(better), 2, "the title streamed two mips in");
        assert_eq!(at_other(5), better, "shown to all views");
        assert_eq!(at_other(0), at_other(1));
        assert_eq!(skipped_mips(at_other(0)), 1, "as far as the cap allows");
        assert_eq!(copies(other), [vec![1, 2, 5], vec![]]);
        // More room alone gives no view another copy.
        assert_eq!(at(0), first);
        crate::watch::title_memory_remapped(address as usize, 8 << 20);
        assert_eq!(copies(address), [vec![], vec![]]);
        caps(0, 0);
    }

    /// Copies made under a tighter cap are made again when there is room, some
    /// at a time; what they replace is shown to nothing new, and goes once
    /// nothing holds it.
    #[cfg(windows)]
    #[test]
    fn copies_are_made_again_when_there_is_room() {
        if crate::host::host().is_none() || !share() {
            return;
        }
        const APART: u64 = 8 << 20;
        let memory = Memory::zeroed(3 * APART, KIB_64).unwrap();
        let address = |i: u64| memory.address() + i * APART;
        let at = |i, largest| texture_at(address(i), &STREAMED, largest).unwrap();
        let within = || memory.address()..memory.address() + 3 * APART;
        let care = |what: &Care| care_from(what, &mut None, within());
        let nothing = |caps| Care { caps, better: [0, 0], cut: 0, idle: None };
        let tight = caps(256, 256);
        let first: Vec<This> = (0..3).map(|i| at(i, 0)).collect();
        assert_eq!(care(&nothing(tight)), Cared { capped: [3, 3], ..Cared::default() });
        // Room for 512: one fits the bytes given, a second the number.
        let room = caps(512, 512);
        let bytes = host_bytes(first[0]) * 4;
        let seen = replaced_since(0).1;
        let some = Care { better: [2 * bytes, 8], ..nothing(room) };
        let done = care(&some);
        assert_eq!(done.better[0], 2, "{done:?}");
        let made: Vec<This> = (0..3).map(|i| at(i, 0)).collect();
        assert_eq!(made.iter().map(|&t| skipped_mips(t)).collect::<Vec<_>>(), [2, 2, 3]);
        assert_eq!(copies(address(0)), [vec![2], vec![3]], "the old one leaves");
        let (replaced, _) = replaced_since(seen);
        let [made, left] = [0, 2].map(|i| replaced.contains(&(first[i] as usize)));
        assert!(made && !left);
        assert!(hold_placed(first[0]).is_some(), "still alive for the slots that show it");
        // Held by a slot it stays, and is announced again.
        let held = host_copy(first[0]);
        let seen = replaced_since(0).1;
        let done = care(&some);
        assert_eq!(done.better[0], 1, "the third");
        assert!(replaced_since(seen).0.contains(&(first[0] as usize)));
        assert_eq!(copies(address(0)), [vec![2], vec![3]]);
        assert_eq!(copies(address(1)), [vec![2], vec![]], "nothing held this one");
        drop(held);
        care(&some);
        assert_eq!(copies(address(0)), [vec![2], vec![]]);
        assert!(hold_placed(first[0]).is_none());
        // Full: cut back to what the cap allows now, the copy made at the next use.
        let tight = caps(256, 256);
        let done = care(&Care { cut: 1, ..nothing(tight) });
        assert_eq!(done.cut[0], 1, "as many as there are bytes to cut");
        let cut = (0..3).filter(|&i| copies(address(i))[1] == [2]).count();
        assert_eq!(cut, 1);
        assert!((0..3).any(|i| skipped_mips(at(i, 0)) == 3));
        // Made again before it was gone, a copy stays.
        let i = (0..3).find(|&i| copies(address(i))[1] == [2]).unwrap();
        let room = caps(512, 512);
        let done = care(&Care { better: [8 * bytes, 8], ..nothing(room) });
        assert_eq!((done.better[0], copies(address(i))), (1, [vec![2], vec![3]]));
        // Unused ones are let go after a while, and not before.
        let idle = |ms| Care { idle: Some((ms, u64::MAX)), ..nothing(room) };
        care(&idle(60_000));
        assert_eq!(copies(address(1))[0], [2]);
        std::thread::sleep(std::time::Duration::from_millis(5));
        care(&idle(1));
        for i in 0..3 {
            assert_eq!(copies(address(i)), [vec![], vec![]]);
        }
        caps(0, 0);
    }

    /// Copies nothing holds go once nothing used them for long enough: those
    /// idle the longest first, as many as the bytes allow; one a list or a
    /// slot holds stays until it is let go of; the next view makes another.
    #[cfg(windows)]
    #[test]
    fn idle_copies_go_longest_first_and_never_while_held() {
        use std::sync::atomic::Ordering::Relaxed;
        if crate::host::host().is_none() || !share() {
            return;
        }
        const APART: u64 = 8 << 20;
        let memory = Memory::zeroed(4 * APART, KIB_64).unwrap();
        let address = |i: u64| memory.address() + i * APART;
        let within = || memory.address()..memory.address() + 4 * APART;
        let tight = caps(256, 256);
        let made: Vec<This> = (0..4).map(|i| texture_at(address(i), &STREAMED, 0).unwrap()).collect();
        std::thread::sleep(std::time::Duration::from_millis(20));
        // Idle longest the first; the last in use for good.
        for (&t, used) in made.iter().zip([1, 2, 3, u64::MAX]) {
            host_copy(t).unwrap().used.store(used, Relaxed);
        }
        let one = host_bytes(made[0]);
        let held = host_copy(made[1]);
        let idle = |most| Care { caps: tight, better: [0, 0], cut: 0, idle: Some((1, most)) };
        let epoch = placed_epoch();
        let done = care_from(&idle(one), &mut None, within());
        assert_eq!(done.gone, [1, one], "the longest idle, as many bytes as allowed");
        assert!(placed_epoch() > epoch, "what names it forgets it");
        let left = |i| copies(address(i))[0].len();
        assert_eq!([0, 1, 2, 3].map(left), [0, 1, 1, 1]);
        care_from(&idle(u64::MAX), &mut None, within());
        assert_eq!([0, 1, 2, 3].map(left), [0, 1, 0, 1], "held, and in use");
        drop(held);
        care_from(&idle(u64::MAX), &mut None, within());
        assert_eq!([0, 1, 2, 3].map(left), [0, 0, 0, 1]);
        // Kept: nothing goes.
        let kept = Care { idle: None, ..idle(u64::MAX) };
        host_copy(made[3]).unwrap().used.store(1, Relaxed);
        care_from(&kept, &mut None, within());
        assert_eq!(left(3), 1);
        // Viewed again, it is made again.
        let again = texture_at(address(0), &STREAMED, 0).unwrap();
        assert!(!waits(again) && host_bytes(again) == one);
        assert_eq!(left(0), 1);
        crate::watch::title_memory_remapped(memory.address() as usize, (4 * APART) as usize);
        caps(0, 0);
    }

    #[test]
    fn committed_and_placed_resources_map_at_their_gpu_address() {
        let null = std::ptr::null();
        let mut committed = null;
        let hr = create_committed(
            null,
            &PROPERTIES,
            0,
            &buffer(256),
            0,
            null.cast(),
            &RESOURCE_IIDS[0],
            &mut committed,
        );
        assert_eq!(hr, S_OK);
        let mut data = std::ptr::null_mut();
        assert_eq!(
            slot::<Map>(committed, 0x40)(committed, 0, null.cast(), &mut data),
            S_OK
        );
        assert_eq!(slot::<Address>(committed, 0x58)(committed), data as u64);
        // SAFETY: the buffer is at least 256 zeroed bytes.
        assert_eq!(unsafe { *data.add(255) }, 0);
        assert_eq!(slot::<Release>(committed, 0x10)(committed), 0);

        let heap = heap(4 * KIB_64);
        let place = |offset, desc: &ResourceDesc, out: &mut This| {
            create_placed(
                null,
                heap,
                offset,
                desc,
                0,
                null.cast(),
                &RESOURCE_IIDS[0],
                out,
            )
        };
        let mut placed = null;
        assert_eq!(place(KIB_64, &buffer(KIB_64), &mut placed), S_OK);
        // SAFETY: a heap we made.
        let base = unsafe { (*heap.cast::<Heap>()).memory.ptr.as_ptr() } as u64;
        assert_eq!(slot::<Address>(placed, 0x58)(placed), base + KIB_64);
        let mut other = null;
        assert_eq!(
            place(4 * KIB_64, &buffer(1), &mut other),
            E_INVALIDARG,
            "past the end"
        );
        assert_eq!(
            place(100, &buffer(1), &mut other),
            E_INVALIDARG,
            "misaligned"
        );
        // The resource keeps the heap alive.
        assert_eq!(slot::<Release>(heap, 0x10)(heap), 1);
        assert_eq!(slot::<Release>(placed, 0x10)(placed), 0);
    }
}
