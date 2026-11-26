//! What the host GPU does for cards around a submission ([`super`]).
//!
//! Before its lists ([`Engine::prepare`]): the bytes of title memory that
//! changed are copied from each card's side in system memory (`staging`) to
//! the card, and the hashes of the pages they lie in are made again.
//!
//! After them, in the same submission: a compute pass hashes the pages the
//! submission's shaders may have written, 4 KiB each, and compares them with
//! the hashes kept. A page that changed is copied, by the same pass, to the
//! next free place of a buffer in video memory with a note of which one it
//! is, and its hash is replaced. The buffer is then copied to read-back
//! memory in parts, each predicated on there being that many pages, so the
//! results are there when the submission's fence is: no second round trip,
//! and nothing copied when no page changed.
//!
//! After the fence ([`Engine::finish`]): the pages are merged into title
//! memory and the cards' sides ([`super::merge`]). More pages than the
//! buffer holds keep their old hashes, and are found by running the pass
//! again, waited for (the only wait of ours).
//!
//! One 64-thread group hashes the sixteen pages of a 64 KiB piece, four
//! threads a page. It uses no wave operations, so it runs the same on 32 and
//! 64 lanes. 185 MiB take 0.46 ms on an RTX 2070 SUPER (2026-11-25).
//!
//! The idea of hashing on the card and fetching what changed is fxceless'
//! (victory-fork, carried by victory-mix), the 4 KiB pages DexNom's
//! (VIctory, 570a941); the code here is written anew.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use super::{Card, LEAF, LEAVES, PIECE};
use crate::host::{Host, transition};
use crate::telemetry::{Count, add};

const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=8, b0), UAV(u0), SRV(t0), UAV(u1), UAV(u2), UAV(u3)"
cbuffer Constants : register(b0) {
    uint card;      // the card's number, for `index`
    uint pieces;    // the entries of `list`
    uint list_at;   // the first one's byte offset
    uint mode;      // 0: hashes are stored; 1: compared, changed pages taken; 2: every page taken
    uint room;      // the pages `emitted` holds
    uint first;     // (marks) the pages of the first part read back
    uint parts;     // (marks) the parts
};
RWByteAddressBuffer bytes : register(u0);
// Per entry: a 64 KiB piece, and a bit for each of its pages to look at.
ByteAddressBuffer list : register(t0);
// 16 bytes for each of a piece's 64 threads.
RWByteAddressBuffer hashes : register(u1);
RWByteAddressBuffer emitted : register(u2);
// The pages taken; from byte 16 on, for each, the card and its page.
RWByteAddressBuffer index : register(u3);

groupshared uint changed;
groupshared uint first_slot;

[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 group : SV_GroupID, uint lane : SV_GroupIndex) {
    if (group.x >= pieces) return;
    uint2 entry = list.Load2(list_at + group.x * 8u);
    uint piece = entry.x * 65536u;
    uint page = lane >> 2u;
    uint on = (entry.y >> page) & 1u;
    uint hash_at = (entry.x * 64u + lane) * 16u;
    uint4 h = uint4(0x9E3779B9u, 0x85EBCA6Bu, 0xC2B2AE35u, 0x27D4EB2Fu);
    if (on != 0u) {
        uint at = piece + page * 4096u + (lane & 3u) * 16u;
        [loop]
        for (uint i = 0u; i < 64u; i++) {
            h = (h ^ bytes.Load4(at + i * 64u)) * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
            h = h.yzwx ^ (h >> 15);
        }
    }
    if (mode == 0u) {
        if (on != 0u) hashes.Store4(hash_at, h);
        return;
    }
    if (lane == 0u) { changed = 0u; first_slot = 0u; }
    GroupMemoryBarrierWithGroupSync();
    if (on != 0u) {
        bool other = true;
        if (mode == 1u) other = any(h != hashes.Load4(hash_at));
        if (other) { uint was; InterlockedOr(changed, 1u << page, was); }
    }
    GroupMemoryBarrierWithGroupSync();
    uint mask = changed;
    if (lane == 0u && mask != 0u) {
        uint at;
        index.InterlockedAdd(0u, countbits(mask), at);
        first_slot = at;
    }
    GroupMemoryBarrierWithGroupSync();
    if (mask != 0u) {
        uint slot = first_slot;
        // A page without room keeps its old hash: found again.
        uint mine = slot + countbits(mask & ((1u << page) - 1u));
        if (mode == 1u && ((mask >> page) & 1u) != 0u && mine < room) hashes.Store4(hash_at, h);
        [loop]
        for (uint p = 0u; p < 16u; p++) {
            if (((mask >> p) & 1u) == 0u) continue;
            if (slot < room) {
                uint from = piece + p * 4096u + lane * 64u;
                uint to = slot * 4096u + lane * 64u;
                emitted.Store4(to, bytes.Load4(from));
                emitted.Store4(to + 16u, bytes.Load4(from + 16u));
                emitted.Store4(to + 32u, bytes.Load4(from + 32u));
                emitted.Store4(to + 48u, bytes.Load4(from + 48u));
                if (lane == 0u) index.Store2(16u + slot * 8u, uint2(card, entry.x * 16u + p));
            }
            slot++;
        }
    }
}

// What each part read back is predicated on: whether there are pages in it.
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void marks() {
    uint n = index.Load(0u);
    for (uint k = 0u; k < parts; k++) {
        uint from = k == 0u ? 0u : first << (k - 1u);
        emitted.Store2(k * 8u, uint2(n > from ? 1u : 0u, 0u));
    }
}
"#;

/// The pages taken in one run at most.
const ROOM: u32 = 8192;
/// The pages of the first part read back; each next part ends at twice the
/// one before.
const FIRST: u32 = 64;
/// Bytes before the list of pages in `index`.
const INDEX_AT: u64 = 16;
/// Bytes of piece lists a submission has room for.
const LISTS: u64 = 1 << 20;

/// The bytes of the hashes of a card of `size` bytes.
pub(super) fn hash_bytes(size: usize) -> u64 {
    (size / PIECE) as u64 * 64 * 16
}

/// What one submission's lists of ours are recorded with, and what comes
/// back.
struct Slot {
    before: (ID3D12CommandAllocator, ID3D12GraphicsCommandList),
    after: (ID3D12CommandAllocator, ID3D12GraphicsCommandList),
    /// Piece lists, mapped.
    lists: (ID3D12Resource, usize),
    /// The index, then the pages, mapped.
    back: (ID3D12Resource, usize),
    /// The submission it was last used for.
    submission: u64,
}

/// The cards a submission's shaders may have written, until it is finished.
pub(crate) struct Plan {
    slot: Slot,
    cards: Vec<Planned>,
}

// SAFETY: D3D12 objects are free-threaded and the mappings persistent; a
// plan is used by one thread at a time.
unsafe impl Send for Plan {}

struct Planned {
    card: Arc<Card>,
    /// `None`: no memory for them; every page is taken.
    hashes: Option<ID3D12Resource>,
    /// Its entries of the piece list: the first one's byte offset, how many.
    list: (u32, u32),
    /// For a card all of which is looked at: which pages the submission's
    /// shaders were known to write, by piece.
    declared: Option<Vec<u16>>,
}

/// What a submission runs of ours: before its lists, and after them.
#[derive(Default)]
pub(crate) struct Prepared {
    pub(crate) before: Option<ID3D12CommandList>,
    pub(crate) after: Option<ID3D12CommandList>,
    pub(crate) plan: Option<Plan>,
}

pub(crate) struct Engine {
    root: ID3D12RootSignature,
    pass: ID3D12PipelineState,
    marks: ID3D12PipelineState,
    /// See [`ROOM`]; and the parts read back.
    room: u32,
    parts: u32,
    emitted: ID3D12Resource,
    index: ID3D12Resource,
    predicates: ID3D12Resource,
    /// 16 bytes of zeros to copy.
    zero: ID3D12Resource,
    fence: ID3D12Fence,
    value: u64,
    /// Those no plan has.
    slots: Vec<Slot>,
}

// SAFETY: D3D12 objects are free-threaded; used behind its host's lock.
unsafe impl Send for Engine {}

fn compile(entry: windows::core::PCSTR) -> windows::core::Result<ID3DBlob> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let made = unsafe {
        D3DCompile(
            SOURCE.as_ptr().cast(),
            SOURCE.len(),
            windows::core::s!("card"),
            None,
            None,
            entry,
            windows::core::s!("cs_5_1"),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = made {
        if let Some(errors) = errors {
            // SAFETY: the blob's own bytes.
            let text = unsafe {
                std::slice::from_raw_parts(errors.GetBufferPointer().cast::<u8>(), errors.GetBufferSize())
            };
            tracing::warn!("cards: {}", String::from_utf8_lossy(text));
            #[cfg(test)]
            eprintln!("{}", String::from_utf8_lossy(text));
        }
        return Err(e);
    }
    code.ok_or_else(windows::core::Error::empty)
}

fn uav_barrier() -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        ..Default::default()
    }
}

const COMMON: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATE_COMMON;
const UAV: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
const TO: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATE_COPY_DEST;
const FROM: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATE_COPY_SOURCE;

impl Engine {
    pub(crate) fn new(host: &Host) -> windows::core::Result<Self> {
        Self::with_room(host, ROOM)
    }

    /// An engine that takes `room` pages in one run: [`FIRST`] times a power
    /// of two.
    pub(super) fn with_room(host: &Host, room: u32) -> windows::core::Result<Self> {
        let pass = compile(windows::core::s!("main"))?;
        let marks = compile(windows::core::s!("marks"))?;
        // SAFETY: the blobs' own bytes; the root signature is in the shader.
        let (root, pass, marks) = unsafe {
            let bytes = |b: &ID3DBlob| std::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize());
            let root: ID3D12RootSignature = host.device.CreateRootSignature(0, bytes(&pass))?;
            let pipeline = |code: &ID3DBlob| -> windows::core::Result<ID3D12PipelineState> {
                host.device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    // A borrowed pointer the description never releases.
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: code.GetBufferPointer(),
                        BytecodeLength: code.GetBufferSize(),
                    },
                    ..Default::default()
                })
            };
            (root.clone(), pipeline(&pass)?, pipeline(&marks)?)
        };
        let none = windows::core::Error::empty;
        let emitted = host.uav_buffer(u64::from(room) * LEAF as u64).ok_or_else(none)?;
        let index = host.uav_buffer(INDEX_AT + u64::from(room) * 8).ok_or_else(none)?;
        let predicates = host.uav_buffer(256).ok_or_else(none)?;
        let (zero, data) = host.upload_buffer(INDEX_AT).ok_or_else(none)?;
        // SAFETY: the mapping of 16 bytes.
        unsafe { data.write_bytes(0, INDEX_AT as usize) };
        // SAFETY: plain creation.
        let fence = unsafe { host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE)? };
        crate::host::name(&emitted, || "pages shaders wrote".into());
        crate::host::name(&index, || "which pages shaders wrote".into());
        let mut engine = Engine {
            root,
            pass,
            marks,
            room,
            parts: 1 + (room / FIRST).trailing_zeros(),
            emitted,
            index,
            predicates,
            zero,
            fence,
            value: 0,
            slots: Vec::new(),
        };
        // Its read-back memory is made now: a submission cannot do without.
        let slot = engine.slot(host, 0).ok_or_else(none)?;
        engine.slots.push(slot);
        Ok(engine)
    }

    /// A slot whose submission has run, or a new one.
    fn slot(&mut self, host: &Host, finished: u64) -> Option<Slot> {
        if let Some(i) = self.slots.iter().position(|s| s.submission <= finished) {
            return Some(self.slots.swap_remove(i));
        }
        let pair = || -> windows::core::Result<(ID3D12CommandAllocator, ID3D12GraphicsCommandList)> {
            // SAFETY: plain object creation.
            unsafe {
                let allocator: ID3D12CommandAllocator =
                    host.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
                let list: ID3D12GraphicsCommandList =
                    host.device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
                list.Close()?;
                Ok((allocator, list))
            }
        };
        let (lists, lists_at) = host.upload_buffer(LISTS)?;
        let (back, back_at) = host.readback_buffer(self.pages_at() + u64::from(self.room) * LEAF as u64)?;
        crate::host::name(&back, || "pages shaders wrote, read back".into());
        Some(Slot {
            before: pair().ok()?,
            after: pair().ok()?,
            lists: (lists, lists_at as usize),
            back: (back, back_at as usize),
            submission: 0,
        })
    }

    /// Where the pages start in the read-back memory, after the index.
    fn pages_at(&self) -> u64 {
        (INDEX_AT + u64::from(self.room) * 8).next_multiple_of(64 << 10)
    }

    /// Notes, for submission `submission` (`finished` ones have run), what
    /// it runs of ours: `live` are the cards there are, `reads` and `writes`
    /// what its shaders are known to read and write (a card and title
    /// addresses, from and to), `sweep` whether cards due for it are looked
    /// at whole. The caller runs
    /// `before` ahead of the submission's lists and `after` behind them, and
    /// calls [`Engine::finish`] with the plan once they have run.
    pub(crate) fn prepare(
        &mut self,
        host: &Host,
        live: &[Arc<Card>],
        reads: &[(Arc<Card>, usize, usize)],
        writes: &[(Arc<Card>, usize, usize)],
        sweep: bool,
        submission: u64,
        finished: u64,
    ) -> Prepared {
        // What the title changed.
        let mut uploads: Vec<(Arc<Card>, Vec<(u32, u32)>)> = Vec::new();
        for card in live {
            // Shaders that write a page read it, too.
            let reads: Vec<(usize, usize)> = if card.is_buffer() {
                let of = |r: &&(Arc<Card>, usize, usize)| Arc::ptr_eq(&r.0, card);
                reads.iter().chain(writes).filter(of).map(|r| (r.1, r.2)).collect()
            } else {
                Vec::new()
            };
            let runs = card.pull(&reads);
            if !runs.is_empty() {
                uploads.push((card.clone(), runs));
            }
        }
        // What shaders write: a mask of pages by piece.
        let mut written: Vec<(Arc<Card>, Vec<u16>, bool)> = Vec::new();
        let masks = |written: &mut Vec<(Arc<Card>, Vec<u16>, bool)>, card: &Arc<Card>| -> usize {
            match written.iter().position(|w| Arc::ptr_eq(&w.0, card)) {
                Some(i) => i,
                None => {
                    written.push((card.clone(), vec![0u16; card.pieces()], false));
                    written.len() - 1
                }
            }
        };
        for (card, from, to) in writes {
            let i = masks(&mut written, card);
            let Some((first, last)) = card.leaves_of(*from, *to) else { continue };
            mark(&mut written[i].1, first, last);
        }
        if sweep {
            for card in live {
                if card.sweep_due() {
                    let i = masks(&mut written, card);
                    written[i].2 = true;
                }
            }
        }
        written.retain(|w| w.2 || w.1.iter().any(|&m| m != 0));
        if uploads.is_empty() && written.is_empty() {
            return Prepared::default();
        }
        let Some(mut slot) = self.slot(host, finished) else {
            // Nothing can be sent: it is looked at again next time.
            for (card, runs) in &uploads {
                card.put_back(runs);
            }
            return Prepared::default();
        };
        let mut entries = Entries { at: 0, to: slot.lists.1 };
        let before = self.record_before(host, &slot, &uploads, &written, &mut entries);
        let before = match before {
            Ok(before) => before,
            Err(error) => {
                tracing::warn!(%error, "cards: what the title wrote was not sent");
                for (card, runs) in &uploads {
                    card.put_back(runs);
                }
                self.slots.push(slot);
                return Prepared::default();
            }
        };
        for (_, runs) in &uploads {
            add(Count::CardUploaded, runs.iter().map(|r| u64::from(r.1)).sum());
        }
        slot.submission = submission;
        let mut plan = Plan { slot, cards: Vec::new() };
        let mut compared = 0u64;
        for (card, masks, whole) in written {
            let hashes = card.hashes().filter(|_| card.hashed());
            let first = entries.at;
            let n = if whole {
                entries.push((0..masks.len()).map(|p| (p as u32, u16::MAX)))
            } else {
                entries.push(masks.iter().enumerate().filter(|(_, m)| **m != 0).map(|(p, &m)| (p as u32, m)))
            };
            compared += if whole {
                card.size as u64
            } else {
                masks.iter().map(|m| u64::from(m.count_ones()) * LEAF as u64).sum()
            };
            plan.cards.push(Planned {
                card,
                hashes,
                list: (first as u32, n),
                declared: whole.then_some(masks),
            });
        }
        if plan.cards.is_empty() {
            self.slots.push(plan.slot);
            return Prepared { before, after: None, plan: None };
        }
        match self.record_after(&plan) {
            Ok(after) => {
                add(Count::CardCompared, compared);
                add(Count::CardSubmissions, 1);
                Prepared { before, after: Some(after), plan: Some(plan) }
            }
            Err(error) => {
                // What shaders write is found by a later compare.
                tracing::warn!(%error, "cards: the compare was not recorded");
                self.slots.push(plan.slot);
                Prepared { before, after: None, plan: None }
            }
        }
    }

    /// The list that brings cards up to date, if there is anything to do.
    fn record_before(
        &self,
        host: &Host,
        slot: &Slot,
        uploads: &[(Arc<Card>, Vec<(u32, u32)>)],
        written: &[(Arc<Card>, Vec<u16>, bool)],
        entries: &mut Entries,
    ) -> windows::core::Result<Option<ID3D12CommandList>> {
        // Cards shaders write for the first time get all their hashes.
        let mut fresh: Vec<Arc<Card>> = Vec::new();
        for (card, ..) in written {
            if !card.hashed() && card.make_hashes(host).is_some() {
                fresh.push(card.clone());
            }
        }
        if uploads.is_empty() && fresh.is_empty() {
            return Ok(None);
        }
        let is_fresh = |card: &Arc<Card>| fresh.iter().any(|c| Arc::ptr_eq(c, card));
        let (allocator, list) = &slot.before;
        // SAFETY: the list's last submission has run; live buffers, and the
        // ranges lie in them.
        unsafe {
            allocator.Reset()?;
            list.Reset(allocator, None)?;
            let mut sent = Vec::new();
            let mut done = Vec::new();
            for (card, runs) in uploads {
                for &(at, len) in runs {
                    list.CopyBufferRegion(&card.buffer, u64::from(at), &card.staging, u64::from(at), u64::from(len));
                }
                let hashed = card.hashed() || is_fresh(card);
                sent.push(transition(&card.buffer, TO, if hashed { UAV } else { COMMON }));
                if hashed {
                    done.push(transition(&card.buffer, UAV, COMMON));
                }
            }
            if !sent.is_empty() {
                list.ResourceBarrier(&sent);
            }
            list.SetComputeRootSignature(&self.root);
            list.SetPipelineState(&self.pass);
            list.SetComputeRootUnorderedAccessView(4, self.emitted.GetGPUVirtualAddress());
            list.SetComputeRootUnorderedAccessView(5, self.index.GetGPUVirtualAddress());
            for card in &fresh {
                let first = entries.at;
                let n = entries.push((0..card.pieces()).map(|p| (p as u32, u16::MAX)));
                self.hash(list, slot, card, card.hashes().as_ref(), (first as u32, n), 0, 0);
            }
            for (card, runs) in uploads {
                if is_fresh(card) || !card.hashed() {
                    continue;
                }
                // The pages sent up are hashed again: what changed them is
                // not a shader.
                let mut masks = std::collections::BTreeMap::<u32, u16>::new();
                for &(at, len) in runs {
                    let (first, last) = (at as usize / LEAF, (at + len - 1) as usize / LEAF);
                    for leaf in first..=last {
                        *masks.entry((leaf / LEAVES) as u32).or_default() |= 1 << (leaf % LEAVES);
                    }
                }
                let first = entries.at;
                let n = entries.push(masks.into_iter());
                self.hash(list, slot, card, card.hashes().as_ref(), (first as u32, n), 0, 0);
            }
            done.push(uav_barrier());
            list.ResourceBarrier(&done);
            list.Close()?;
        }
        for card in &fresh {
            card.set_hashed();
        }
        Ok(Some(list.cast()?))
    }

    /// Records the pass over `pieces` entries of the piece list.
    ///
    /// # Safety
    /// An open list with the root signature and the pass bound, and the
    /// emitted pages and the index; live buffers.
    #[allow(clippy::too_many_arguments)] // one call's bindings
    unsafe fn hash(
        &self,
        list: &ID3D12GraphicsCommandList,
        slot: &Slot,
        card: &Card,
        hashes: Option<&ID3D12Resource>,
        (list_at, pieces): (u32, u32),
        number: u32,
        mode: u32,
    ) {
        if pieces == 0 {
            return;
        }
        let constants = [number, pieces, list_at, mode, self.room, FIRST, self.parts, 0];
        // SAFETY: the caller's.
        unsafe {
            list.SetComputeRoot32BitConstants(0, 8, constants.as_ptr().cast(), 0);
            list.SetComputeRootUnorderedAccessView(1, card.gpu);
            list.SetComputeRootShaderResourceView(2, slot.lists.0.GetGPUVirtualAddress());
            list.SetComputeRootUnorderedAccessView(
                3,
                hashes.unwrap_or(&self.predicates).GetGPUVirtualAddress(),
            );
            // Cards are below 1 GiB: 16,384 pieces, a dispatch holds 65,535.
            list.Dispatch(pieces, 1, 1);
        }
    }

    /// The list that finds what shaders wrote to the plan's cards.
    fn record_after(&self, plan: &Plan) -> windows::core::Result<ID3D12CommandList> {
        let (allocator, list) = &plan.slot.after;
        let back = &plan.slot.back.0;
        // SAFETY: the list's last run has finished; live buffers, in the
        // common state (as buffers are at the start of a submission of
        // lists), each range inside its buffer.
        unsafe {
            allocator.Reset()?;
            list.Reset(allocator, None)?;
            list.CopyBufferRegion(&self.index, 0, &self.zero, 0, INDEX_AT);
            // What the submission's shaders wrote first. The cards are bound
            // for unordered access, as shaders that wrote them had them.
            list.ResourceBarrier(&[uav_barrier(), transition(&self.index, TO, UAV)]);
            list.SetComputeRootSignature(&self.root);
            list.SetPipelineState(&self.pass);
            list.SetComputeRootUnorderedAccessView(4, self.emitted.GetGPUVirtualAddress());
            list.SetComputeRootUnorderedAccessView(5, self.index.GetGPUVirtualAddress());
            for (number, c) in plan.cards.iter().enumerate() {
                let mode = if c.hashes.is_some() { 1 } else { 2 };
                self.hash(list, &plan.slot, &c.card, c.hashes.as_ref(), c.list, number as u32, mode);
            }
            list.ResourceBarrier(&[uav_barrier()]);
            list.SetPipelineState(&self.marks);
            list.SetComputeRootUnorderedAccessView(4, self.predicates.GetGPUVirtualAddress());
            list.Dispatch(1, 1, 1);
            list.ResourceBarrier(&[
                transition(&self.emitted, UAV, FROM),
                transition(&self.index, UAV, FROM),
                transition(&self.predicates, UAV, D3D12_RESOURCE_STATE_PREDICATION),
            ]);
            list.CopyBufferRegion(back, 0, &self.index, 0, INDEX_AT + u64::from(self.room) * 8);
            for part in 0..self.parts {
                let (from, to) = self.part(part);
                list.SetPredication(&self.predicates, u64::from(part) * 8, D3D12_PREDICATION_OP_EQUAL_ZERO);
                list.CopyBufferRegion(
                    back,
                    self.pages_at() + u64::from(from) * LEAF as u64,
                    &self.emitted,
                    u64::from(from) * LEAF as u64,
                    u64::from(to - from) * LEAF as u64,
                );
            }
            list.SetPredication(None::<&ID3D12Resource>, 0, D3D12_PREDICATION_OP_EQUAL_ZERO);
            list.ResourceBarrier(&[
                transition(&self.emitted, FROM, COMMON),
                transition(&self.index, FROM, COMMON),
                transition(&self.predicates, D3D12_RESOURCE_STATE_PREDICATION, COMMON),
            ]);
            list.Close()?;
            list.cast()
        }
    }

    /// The pages of part `part` of what is read back: from, to.
    fn part(&self, part: u32) -> (u32, u32) {
        if part == 0 { (0, FIRST) } else { (FIRST << (part - 1), FIRST << part) }
    }

    /// The plan's lists ran: what shaders wrote goes into title memory. The
    /// bytes stored there.
    pub(crate) fn finish(&mut self, host: &Host, plan: Plan) -> usize {
        let mut stored = 0;
        loop {
            // SAFETY: a plain query.
            if unsafe { host.device.GetDeviceRemovedReason() }.is_err() {
                // Nothing came back; the submission reports it.
                break;
            }
            let count = self.merge(&plan, &mut stored);
            if count <= self.room {
                break;
            }
            // More pages than there was room for.
            add(Count::CardWaits, 1);
            if let Err(error) = self.again(host, &plan) {
                tracing::warn!(%error, "cards: what shaders wrote is left for the next compare");
                break;
            }
        }
        self.slots.push(plan.slot);
        stored
    }

    /// The plan's lists did not run.
    pub(crate) fn abandon(&mut self, plan: Plan) {
        self.slots.push(plan.slot);
    }

    /// Runs the compare again and waits for it.
    fn again(&mut self, host: &Host, plan: &Plan) -> windows::core::Result<()> {
        let list = self.record_after(plan)?;
        self.value += 1;
        // SAFETY: a closed list on the host's queue, our fence; a null event
        // blocks until the fence is there.
        unsafe {
            host.queue().ExecuteCommandLists(&[Some(list)]);
            host.queue().Signal(&self.fence, self.value)?;
            self.fence.SetEventOnCompletion(self.value, Default::default())
        }
    }

    /// Merges the pages that came back; how many there were to take.
    fn merge(&self, plan: &Plan, stored: &mut usize) -> u32 {
        let back = plan.slot.back.1;
        // SAFETY: the read-back memory holds the index and the pages; the GPU
        // is done with it.
        let count = unsafe { (back as *const u32).read_volatile() };
        let n = count.min(self.room) as usize;
        // SAFETY: as above.
        let index = unsafe { std::slice::from_raw_parts((back + INDEX_AT as usize) as *const [u32; 2], n) };
        let pages = back + self.pages_at() as usize;
        let done = AtomicU64::new(0);
        let undeclared = AtomicU64::new(0);
        let work: Vec<usize> = (0..n).collect();
        crate::host::in_parallel(&work, 512, |part| {
            let (mut bytes, mut unknown) = (0u64, 0u64);
            for &k in part {
                let [number, leaf] = index[k];
                let Some(c) = plan.cards.get(number as usize) else { continue };
                // SAFETY: the page's bytes, inside the read-back memory.
                let page = unsafe { std::slice::from_raw_parts((pages + k * LEAF) as *const u8, LEAF) };
                bytes += c.card.put_back_page(leaf as usize, page) as u64;
                if let Some(masks) = &c.declared
                    && masks.get(leaf as usize / LEAVES).is_some_and(|m| m >> (leaf as usize % LEAVES) & 1 == 0)
                {
                    unknown += 1;
                    c.card.note_undeclared(leaf as usize);
                }
            }
            done.fetch_add(bytes, Ordering::Relaxed);
            undeclared.fetch_add(unknown, Ordering::Relaxed);
        });
        add(Count::CardFetched, (n * LEAF) as u64);
        let undeclared = undeclared.into_inner();
        add(Count::CardUndeclared, undeclared);
        for c in plan.cards.iter().filter(|c| c.declared.is_some()) {
            c.card.swept(undeclared > 0);
        }
        *stored += done.into_inner() as usize;
        count
    }
}

/// Where a submission's piece lists are written.
struct Entries {
    /// Bytes written.
    at: usize,
    to: usize,
}

impl Entries {
    /// Adds entries (as many as there is room for); how many.
    fn push(&mut self, entries: impl Iterator<Item = (u32, u16)>) -> u32 {
        let mut n = 0;
        for (piece, mask) in entries {
            if self.at + 8 > LISTS as usize {
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| tracing::warn!("cards: more pieces in a submission than its list holds"));
                break;
            }
            // SAFETY: inside the list's mapping (checked).
            unsafe { ((self.to + self.at) as *mut [u32; 2]).write([piece, u32::from(mask)]) };
            self.at += 8;
            n += 1;
        }
        n
    }
}

/// Sets the bits of leaves `[first, last)` in masks by piece.
pub(super) fn mark(masks: &mut [u16], first: usize, last: usize) {
    if first >= last {
        return;
    }
    let (a, b) = (first / LEAVES, (last - 1) / LEAVES);
    let from = u16::MAX << (first % LEAVES);
    let to = u16::MAX >> (LEAVES - 1 - (last - 1) % LEAVES);
    if a == b {
        masks[a] |= from & to;
        return;
    }
    masks[a] |= from;
    masks[a + 1..b].fill(u16::MAX);
    masks[b] |= to;
}
