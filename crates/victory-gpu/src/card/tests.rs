//! Cards on the software adapter, and on the hardware one (ignored).

use super::engine::mark;
use super::*;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree};

fn rng(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// Memory standing for the title's: private, on 64 KiB lines.
struct Memory {
    at: usize,
    len: usize,
}

impl Memory {
    fn new(len: usize) -> Memory {
        // SAFETY: a plain allocation.
        let at = unsafe { VirtualAlloc(std::ptr::null(), len, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) } as usize;
        assert_ne!(at, 0);
        Memory { at, len }
    }

    fn bytes(&self) -> Vec<u8> {
        // SAFETY: our allocation; nothing else writes it while it is read.
        unsafe { std::slice::from_raw_parts(self.at as *const u8, self.len) }.to_vec()
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        // SAFETY: our allocation.
        unsafe { VirtualFree(self.at as *mut _, 0, MEM_RELEASE) };
    }
}

fn poke32(at: usize, value: u32) {
    // SAFETY: the tests' own memory; a guarded block is opened by the handler.
    unsafe { (at as *mut u32).write_volatile(value) };
}

/// A program that stores `n` dwords (offset, value) of a list.
const SCATTER: &str = r#"
#define RS "RootConstants(num32BitConstants=1, b0), UAV(u0), SRV(t0)"
cbuffer C : register(b0) { uint n; };
RWByteAddressBuffer bytes : register(u0);
ByteAddressBuffer list : register(t0);
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= n) return;
    uint2 e = list.Load2(id.x * 8);
    bytes.Store(e.x, e.y);
}
"#;

/// A queue's lists of ours, timed.
struct Gpu<'a> {
    host: &'a Host,
    root: ID3D12RootSignature,
    scatter: ID3D12PipelineState,
    fence: ID3D12Fence,
    value: std::cell::Cell<u64>,
    stamps: ID3D12QueryHeap,
    back: (ID3D12Resource, usize),
}

impl<'a> Gpu<'a> {
    fn new(host: &'a Host) -> Gpu<'a> {
        // SAFETY: plain creation; the blob's own bytes.
        unsafe {
            let mut code: Option<ID3DBlob> = None;
            D3DCompile(
                SCATTER.as_ptr().cast(),
                SCATTER.len(),
                windows::core::s!("scatter"),
                None,
                None,
                windows::core::s!("main"),
                windows::core::s!("cs_5_1"),
                0,
                0,
                &mut code,
                None,
            )
            .expect("the program");
            let code = code.expect("its code");
            let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
            let root: ID3D12RootSignature = host.device.CreateRootSignature(0, bytes).expect("root signature");
            let scatter = host
                .device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: bytes.as_ptr().cast(),
                        BytecodeLength: bytes.len(),
                    },
                    ..Default::default()
                })
                .expect("pipeline");
            let mut stamps: Option<ID3D12QueryHeap> = None;
            host.device
                .CreateQueryHeap(
                    &D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: 2, NodeMask: 0 },
                    &mut stamps,
                )
                .expect("timestamps");
            let (back, data) = host.readback_buffer(16).expect("read-back");
            Gpu {
                host,
                root,
                scatter,
                fence: host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).expect("fence"),
                value: std::cell::Cell::new(0),
                stamps: stamps.expect("timestamps"),
                back: (back, data as usize),
            }
        }
    }

    fn list(&self) -> (ID3D12CommandAllocator, ID3D12GraphicsCommandList) {
        // SAFETY: plain creation.
        unsafe {
            let allocator: ID3D12CommandAllocator =
                self.host.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).expect("allocator");
            let list = self
                .host
                .device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                .expect("list");
            (allocator, list)
        }
    }

    /// Runs closed lists, each a submission of its own, and waits.
    fn run(&self, lists: &[&ID3D12CommandList]) {
        self.value.set(self.value.get() + 1);
        // SAFETY: closed lists on the host's queue, a fence of our own.
        unsafe {
            for list in lists {
                self.host.queue().ExecuteCommandLists(&[Some((*list).clone())]);
            }
            self.host.queue().Signal(&self.fence, self.value.get()).expect("signal");
            self.fence.SetEventOnCompletion(self.value.get(), Default::default()).expect("wait");
            self.host.device.GetDeviceRemovedReason().expect("the lists ran");
        }
    }

    /// Runs `list` between two timestamps: milliseconds.
    fn time(&self, list: &ID3D12CommandList) -> f64 {
        let (_a, begin) = self.list();
        let (_b, end) = self.list();
        // SAFETY: open lists, our heap and read-back memory.
        unsafe {
            begin.EndQuery(&self.stamps, D3D12_QUERY_TYPE_TIMESTAMP, 0);
            begin.Close().expect("close");
            end.EndQuery(&self.stamps, D3D12_QUERY_TYPE_TIMESTAMP, 1);
            end.ResolveQueryData(&self.stamps, D3D12_QUERY_TYPE_TIMESTAMP, 0, 2, &self.back.0, 0);
            end.Close().expect("close");
        }
        self.run(&[&begin.cast().expect("list"), list, &end.cast().expect("list")]);
        // SAFETY: both stamps were resolved.
        unsafe {
            let t = std::slice::from_raw_parts(self.back.1 as *const u64, 2);
            let frequency = self.host.queue().GetTimestampFrequency().expect("frequency");
            (t[1] - t[0]) as f64 * 1e3 / frequency as f64
        }
    }

    /// A closed list in which a shader stores `writes` (offset, value).
    fn store(&self, card: &Card, writes: &[(u32, u32)]) -> (ID3D12CommandAllocator, ID3D12CommandList, ID3D12Resource) {
        let (allocator, list) = self.list();
        let (entries, data) = self.host.upload_buffer((writes.len().max(1) * 8) as u64).expect("entries");
        // SAFETY: room for the entries; an open list, live buffers.
        unsafe {
            for (k, &(at, v)) in writes.iter().enumerate() {
                data.cast::<[u32; 2]>().add(k).write([at, v]);
            }
            list.SetComputeRootSignature(&self.root);
            list.SetPipelineState(&self.scatter);
            list.SetComputeRoot32BitConstant(0, writes.len() as u32, 0);
            list.SetComputeRootUnorderedAccessView(1, card.gpu);
            list.SetComputeRootShaderResourceView(2, entries.GetGPUVirtualAddress());
            list.Dispatch(writes.len().div_ceil(64) as u32, 1, 1);
            list.Close().expect("close");
        }
        (allocator, list.cast().expect("list"), entries)
    }

    /// What the card has.
    fn read(&self, card: &Card) -> Vec<u8> {
        let (back, data) = self.host.readback_buffer(card.size as u64).expect("read-back");
        let (_allocator, list) = self.list();
        // SAFETY: an open list, live buffers; read after it ran.
        unsafe {
            list.CopyBufferRegion(&back, 0, &card.buffer, 0, card.size as u64);
            list.Close().expect("close");
            self.run(&[&list.cast().expect("list")]);
            std::slice::from_raw_parts(data, card.size).to_vec()
        }
    }
}

/// A card of `memory`'s bytes `[from, to)`, every piece looked at each time.
fn plain(host: &Host, memory: &Memory, from: usize, to: usize) -> Arc<Card> {
    let base = (memory.at + from) & !(PIECE - 1);
    let place = Place { back: base, own_view: false, told: Told::Nothing };
    Card::new(host, memory.at + from, memory.at + to, place).expect("a card")
}

struct Submission {
    number: u64,
}

impl Submission {
    /// Runs a submission whose shaders make `writes`, of which the ranges in
    /// `declared` (title addresses) are known; `meanwhile` is done while the
    /// GPU would run. The bytes stored into title memory.
    #[allow(clippy::too_many_arguments)] // a submission's parts
    fn run(
        &mut self,
        gpu: &Gpu,
        engine: &mut Engine,
        live: &[Arc<Card>],
        writes: &[(Arc<Card>, Vec<(u32, u32)>)],
        declared: &[(Arc<Card>, usize, usize)],
        sweep: bool,
        meanwhile: impl FnOnce(),
    ) -> usize {
        self.number += 1;
        let p = engine.prepare(gpu.host, live, &[], declared, sweep, self.number, self.number - 1);
        meanwhile();
        let stores: Vec<_> = writes.iter().map(|(card, w)| gpu.store(card, w)).collect();
        let lists: Vec<&ID3D12CommandList> =
            p.before.iter().chain(stores.iter().map(|s| &s.1)).chain(&p.after).collect();
        gpu.run(&lists);
        p.plan.map_or(0, |plan| engine.finish(gpu.host, plan))
    }
}

/// Title memory and what is expected of it, with a card over part of it.
struct Model {
    memory: Memory,
    /// What title memory is to hold.
    title: Vec<u8>,
    /// What the card is to hold of it.
    card: Vec<u8>,
    from: usize,
    to: usize,
    /// Where the card starts in the memory.
    base: usize,
}

impl Model {
    fn new(len: usize, from: usize, to: usize, state: &mut u64) -> Model {
        let memory = Memory::new(len);
        for at in (0..len).step_by(4) {
            poke32(memory.at + at, rng(state) as u32);
        }
        let title = memory.bytes();
        let base = from & !(PIECE - 1);
        let mut card = vec![0u8; to.next_multiple_of(PIECE) - base];
        card[from - base..to - base].copy_from_slice(&title[from..to]);
        Model { memory, title, card, from, to, base }
    }

    /// The title's thread stores a dword; `seen`: before a submission.
    fn cpu(&mut self, at: usize, value: u32, seen: bool) {
        poke32(self.memory.at + at, value);
        self.title[at..at + 4].copy_from_slice(&value.to_le_bytes());
        if seen {
            for (k, b) in value.to_le_bytes().into_iter().enumerate() {
                if at + k >= self.from && at + k < self.to {
                    self.card[at + k - self.base] = b;
                }
            }
        }
    }

    /// A shader stores a dword; `back`: it comes back to title memory.
    fn gpu(&mut self, at: usize, value: u32, back: bool) -> (u32, u32) {
        if back {
            self.title[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
        self.card[at - self.base..at - self.base + 4].copy_from_slice(&value.to_le_bytes());
        ((at - self.base) as u32, value)
    }

    /// A dword inside the card, on a multiple of 4.
    fn inside(&self, state: &mut u64) -> usize {
        let (from, to) = (self.from.next_multiple_of(4), self.to & !3);
        from + (rng(state) as usize % ((to - from) / 4)) * 4
    }

    fn check(&self, gpu: &Gpu, card: &Card, what: &str) {
        let now = self.memory.bytes();
        if let Some(at) = (0..now.len()).find(|&k| now[k] != self.title[k]) {
            panic!("{what}: title memory differs at {at:#x} of {:#x}", now.len());
        }
        let has = gpu.read(card);
        if let Some(at) = (0..has.len()).find(|&k| has[k] != self.card[k]) {
            panic!("{what}: the card differs at {at:#x}: {:#x} and {:#x}", has[at], self.card[at]);
        }
    }
}

/// The way there and back, over and over, for a card of part of a memory:
/// what the title wrote before is on the card when shaders run, what they
/// wrote is in title memory after, and what the title wrote meanwhile stays.
fn there_and_back(host: &Host, engine: &mut Engine, len: usize, from: usize, to: usize, rounds: usize, seed: u64) {
    let mut state = seed;
    let gpu = Gpu::new(host);
    let mut m = Model::new(len, from, to, &mut state);
    let card = plain(host, &m.memory, from, to);
    let live = [card.clone()];
    let mut s = Submission { number: 0 };
    let at = |m: &Model, a: usize| m.memory.at + a;
    for round in 0..rounds {
        // The title's threads, all over (outside the card too).
        for _ in 0..rng(&mut state) % 40 {
            let a = (rng(&mut state) as usize % (len / 4)) * 4;
            let v = rng(&mut state) as u32;
            m.cpu(a, v, true);
        }
        // Shaders, in some pages.
        let mut writes = std::collections::BTreeMap::new();
        for _ in 0..[0, 1, 3, 200, 1000][round % 5] {
            let a = m.inside(&mut state);
            writes.insert(a, rng(&mut state) as u32);
        }
        let declared: Vec<(Arc<Card>, usize, usize)> = if round % 3 == 0 {
            vec![(card.clone(), at(&m, from), at(&m, to))]
        } else {
            writes.keys().map(|&a| (card.clone(), at(&m, a), at(&m, a) + 4)).collect()
        };
        let entries: Vec<(u32, u32)> = writes.iter().map(|(&a, &v)| m.gpu(a, v, true)).collect();
        // The title's threads beside them, in the same pages.
        let mut meanwhile = Vec::new();
        for _ in 0..rng(&mut state) % 30 {
            let a = match writes.keys().nth(rng(&mut state) as usize % writes.len().max(1)) {
                Some(&a) => (a & !(LEAF - 1)) + (rng(&mut state) as usize % (LEAF / 4)) * 4,
                None => m.inside(&mut state),
            };
            if a + 4 <= len && !writes.contains_key(&a) {
                meanwhile.push((a, rng(&mut state) as u32));
            }
        }
        let memory_at = m.memory.at;
        let late = meanwhile.clone();
        s.run(&gpu, engine, &live, &[(card.clone(), entries)], &declared, false, move || {
            for (a, v) in late {
                poke32(memory_at + a, v);
            }
        });
        // In title memory they are the title's; the card gets them with the
        // next submission.
        for &(a, v) in &meanwhile {
            m.title[a..a + 4].copy_from_slice(&v.to_le_bytes());
        }
        m.check(&gpu, &card, &format!("round {round} of {from:#x}..{to:#x}"));
        for (a, v) in meanwhile {
            m.cpu(a, v, true);
        }
    }
    // Nothing happened: nothing is stored, and the card has it all.
    let all = [(card.clone(), at(&m, from), at(&m, to))];
    s.run(&gpu, engine, &live, &[], &all, false, || ());
    let stored = s.run(&gpu, engine, &live, &[], &all, true, || ());
    assert_eq!(stored, 0);
    m.check(&gpu, &card, "at the end");
}

/// Several sizes, and cards that start and end inside a page.
fn sizes(host: &Host, most: usize) {
    let mut engine = Engine::new(host).expect("the engine");
    for (len, from, to) in [
        (PIECE, 0, PIECE),
        (2 * PIECE, 0x1234 & !3, 2 * PIECE - 8),
        (4 * PIECE, PIECE + 0x2004, 3 * PIECE + 0x10c),
        (4 * PIECE, PIECE + 2, 2 * PIECE + 7),
        (9 * PIECE, 3 * PIECE, 9 * PIECE),
        (most, 0, most),
    ] {
        there_and_back(host, &mut engine, len, from, to, 12, 0x1357_9bdf_2468_ace1 ^ len as u64 ^ from as u64);
    }
}

#[test]
fn warp_title_memory_goes_there_and_back() {
    if let Some(host) = Host::new_warp() {
        sizes(&host, 20 * PIECE);
    }
}

#[test]
#[ignore = "requires a local D3D12 hardware adapter"]
fn hardware_title_memory_goes_there_and_back() {
    let host = crate::host::host().expect("D3D12 host device");
    println!("{:?}", host.target());
    sizes(host, 64 << 20);
}

/// Pages that changed than one run takes come back all the same.
fn more_than_room(host: &Host, rooms: &[u32], most: usize) {
    for &room in rooms {
        let mut engine = Engine::with_room(host, room).expect("the engine");
        let mut state = 0x7777_1234_0000_0001 + u64::from(room);
        let gpu = Gpu::new(host);
        let len = most * PIECE;
        let mut m = Model::new(len, 0, len, &mut state);
        let card = plain(host, &m.memory, 0, len);
        let all = [(card.clone(), m.memory.at, m.memory.at + len)];
        let mut s = Submission { number: 0 };
        for pages in [0usize, 1, 63, 64, 65, 127, 128, 129, 512, 513, most * 16].into_iter().filter(|&p| p <= most * 16) {
            let entries: Vec<(u32, u32)> = (0..pages)
                .map(|p| m.gpu(p * LEAF + (rng(&mut state) as usize % 1024) * 4, rng(&mut state) as u32, true))
                .collect();
            let waits = crate::telemetry::total(Count::CardWaits);
            let stored = s.run(&gpu, &mut engine, &[card.clone()], &[(card.clone(), entries)], &all, false, || ());
            assert_eq!(stored, pages * 4, "{pages} pages with room for {room}");
            let runs = pages.div_ceil(room as usize).max(1);
            assert!(crate::telemetry::total(Count::CardWaits) - waits >= (runs - 1) as u64);
            m.check(&gpu, &card, &format!("{pages} pages with room for {room}"));
        }
    }
}

#[test]
fn warp_more_pages_than_one_run_takes_come_back() {
    if let Some(host) = Host::new_warp() {
        more_than_room(&host, &[64, 128], 18);
    }
}

#[test]
#[ignore = "requires a local D3D12 hardware adapter"]
fn hardware_more_pages_than_one_run_takes_come_back() {
    more_than_room(crate::host::host().expect("D3D12 host device"), &[64, 128, 512], 90);
}

/// A write no list declared stays on the card, whatever the title writes
/// beside it, until a look at all of the card finds it.
#[test]
fn warp_undeclared_writes_stay_and_are_found_by_the_sweep() {
    let Some(host) = Host::new_warp() else { return };
    let mut engine = Engine::new(&host).expect("the engine");
    let gpu = Gpu::new(&host);
    let mut state = 99;
    let len = 8 * PIECE;
    let mut m = Model::new(len, 0, len, &mut state);
    let card = plain(&host, &m.memory, 0, len);
    let live = [card.clone()];
    let mut s = Submission { number: 0 };
    // Its hashes are made.
    let declared = [(card.clone(), m.memory.at + PIECE, m.memory.at + PIECE + 16)];
    s.run(&gpu, &mut engine, &live, &[], &declared, false, || ());
    // Declared one page, wrote that and two others.
    let entries = vec![
        m.gpu(PIECE + 4, 0x1111_1111, true),
        m.gpu(5 * PIECE + 0x2008, 0x2222_2222, false),
        m.gpu(6 * PIECE, 0x3333_3333, false),
    ];
    s.run(&gpu, &mut engine, &live, &[(card.clone(), entries)], &declared, false, || ());
    m.check(&gpu, &card, "undeclared");
    // The title writes all around one of them.
    for at in (5 * PIECE + 0x2000..5 * PIECE + 0x2020).step_by(4) {
        if at != 5 * PIECE + 0x2008 {
            m.cpu(at, at as u32, true);
        }
    }
    s.run(&gpu, &mut engine, &live, &[], &[], false, || ());
    m.check(&gpu, &card, "undeclared, with the title's writes around it");
    // Declared writes go on working beside them.
    let entries = vec![m.gpu(PIECE + 8, 0x4444_4444, true)];
    s.run(&gpu, &mut engine, &live, &[(card.clone(), entries)], &declared, false, || ());
    m.check(&gpu, &card, "declared beside undeclared");
    // The look at all of it finds the one whose page was not sent anew, and
    // the other too: its own dwords came back with a hash made after it.
    let before = crate::telemetry::total(Count::CardUndeclared);
    s.run(&gpu, &mut engine, &live, &[], &[], true, || ());
    assert!(crate::telemetry::total(Count::CardUndeclared) > before);
    let now = m.memory.bytes();
    assert_eq!(now[6 * PIECE..6 * PIECE + 4], 0x3333_3333u32.to_le_bytes());
}

/// What could not be sent goes with the next submission.
#[test]
fn warp_what_was_not_sent_is_sent_later() {
    let Some(host) = Host::new_warp() else { return };
    let mut engine = Engine::new(&host).expect("the engine");
    let gpu = Gpu::new(&host);
    let mut state = 5;
    let len = 3 * PIECE;
    let mut m = Model::new(len, 0, len, &mut state);
    let card = plain(&host, &m.memory, 0, len);
    let runs = card.pull(&[]);
    assert_eq!(runs, [(0, len as u32)], "new: all of it");
    card.put_back(&runs);
    m.cpu(PIECE, 0xabcd, true);
    let mut s = Submission { number: 0 };
    s.run(&gpu, &mut engine, &[card.clone()], &[], &[], false, || ());
    m.check(&gpu, &card, "sent later");
    assert!(card.pull(&[]).is_empty());
}

/// A guarded view of title memory, as the title's heap is.
fn heap_card(host: &Host) {
    let _one = track::tests::ONE.lock().unwrap_or_else(|e| e.into_inner());
    const BLOCKS: usize = 24;
    let mut views = track::tests::Views::new(BLOCKS);
    let at = views.map();
    let len = BLOCKS * PIECE;
    let mut engine = Engine::new(host).expect("the engine");
    let gpu = Gpu::new(host);
    let mut state = 0xdead_beef_0000_0007;
    for a in (0..len).step_by(4) {
        poke32(at + a, rng(&mut state) as u32);
    }
    let shadow = guarded(host, at, len, views.section, 0).expect("a card");
    let card = shadow.card().expect("a card").clone();
    let live = [card.clone()];
    // SAFETY: the tests' own view.
    let title = || unsafe { std::slice::from_raw_parts(at as *const u8, len) }.to_vec();
    let mut s = Submission { number: 0 };
    s.run(&gpu, &mut engine, &live, &[], &[], false, || ());
    assert!(gpu.read(&card) == title());
    for round in 0..30u32 {
        // The title's threads write, some through the kernel; another thread
        // goes on writing while the submission is prepared and runs.
        for _ in 0..20 {
            poke32(at + (rng(&mut state) as usize % (len / 4)) * 4, rng(&mut state) as u32);
        }
        crate::watch::kernel_will_write(at + 3 * PIECE, 64);
        let value = [round; 16];
        // SAFETY: 64 bytes inside the view, opened above.
        unsafe { std::ptr::copy_nonoverlapping(value.as_ptr().cast::<u8>(), (at + 3 * PIECE) as *mut u8, 64) };
        // Shaders write block 5, the title's thread block 6.
        let entries: Vec<(u32, u32)> = (0..50u32).map(|k| ((5 * PIECE) as u32 + k * 516, round * 1000 + k)).collect();
        let declared = [(card.clone(), at + 5 * PIECE, at + 6 * PIECE)];
        let stop = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut k = 0u32;
                while !stop.load(Ordering::Relaxed) {
                    poke32(at + 6 * PIECE + (k as usize % 1000) * 64, k);
                    k += 1;
                }
            });
            s.run(&gpu, &mut engine, &live, &[(card.clone(), entries.clone())], &declared, false, || ());
            stop.store(true, Ordering::Relaxed);
        });
        let now = title();
        for &(a, v) in &entries {
            assert_eq!(now[a as usize..a as usize + 4], v.to_le_bytes(), "round {round}");
        }
        // And the title's last writes arrive.
        s.run(&gpu, &mut engine, &live, &[], &[], false, || ());
        assert!(gpu.read(&card) == title(), "round {round}");
    }
    // The pinned block, and those left open, are all that is looked at.
    let Told::Guarded(t) = &card.told else { panic!("a guarded card") };
    assert!(t.take().len() < BLOCKS / 2);
    drop((shadow, card, live));
    poke32(at, 1);
}

#[test]
fn warp_a_guarded_view_goes_there_and_back() {
    if let Some(host) = Host::new_warp() {
        heap_card(&host);
    }
}

#[test]
#[ignore = "requires a local D3D12 hardware adapter"]
fn hardware_a_guarded_view_goes_there_and_back() {
    heap_card(crate::host::host().expect("D3D12 host device"));
}

/// Which card a range is given, and what one that meets a card gets.
#[test]
fn ranges_are_inside_a_card_or_across_it() {
    let Some(host) = Host::new_warp() else { return };
    let memory = Memory::new(8 * PIECE);
    let a = plain(&host, &memory, PIECE + 100, 2 * PIECE);
    let b = plain(&host, &memory, 4 * PIECE, 6 * PIECE);
    let mut owned = BTreeMap::new();
    for c in [&a, &b] {
        owned.insert(c.span().0, Shadow::on_card(c.clone()));
    }
    let routes: Vec<Route> = owned
        .values()
        .map(|s| {
            let (from, to) = s.card().unwrap().span();
            Route { from, to, shadow: Arc::downgrade(s) }
        })
        .collect();
    let at = |k: usize| (memory.at + k) as u64;
    let who = |from: usize, to: usize| match held_in(&routes, at(from), at(to)) {
        Held::Not => "none",
        Held::By(s) if Arc::ptr_eq(s.card().unwrap(), &a) => "a",
        Held::By(_) => "b",
        Held::Across(_) => "across",
    };
    assert_eq!(who(0, PIECE), "none");
    assert_eq!(who(PIECE, PIECE + 100), "none");
    assert_eq!(who(PIECE, PIECE + 101), "across");
    assert_eq!(who(PIECE + 100, PIECE + 104), "a");
    assert_eq!(who(PIECE + 100, 2 * PIECE), "a");
    assert_eq!(who(2 * PIECE - 4, 2 * PIECE + 1), "across");
    assert_eq!(who(2 * PIECE, 4 * PIECE), "none");
    assert_eq!(who(0, 8 * PIECE), "across");
    assert_eq!(who(3 * PIECE, 5 * PIECE), "across");
    assert_eq!(who(5 * PIECE, 6 * PIECE), "b");
    assert_eq!(who(6 * PIECE, 7 * PIECE), "none");
    // The offsets views are made with, from the card's 64 KiB line on.
    let s = &owned[&at(PIECE + 100)];
    assert_eq!(s.offset(at(PIECE + 100)), 100);
    assert_eq!(s.left(at(2 * PIECE - 8)), 8);
    assert_eq!(s.gpu(at(PIECE + 100)), a.gpu + 100);
}

/// A view whose elements do not fall on multiples of their size from the
/// card's start gets a buffer from another start, over the same memory.
#[test]
fn views_off_their_elements_get_the_same_memory_from_another_start() {
    let Some(host) = crate::host::host() else { return };
    let memory = Memory::new(8 * PIECE);
    let card = plain(host, &memory, 0, 8 * PIECE);
    let shadow = Shadow::on_card(card.clone());
    let at = memory.at as u64 + 5 * PIECE as u64 + 40;
    let same = shadow.element_start(at, 8).expect("a buffer");
    assert!(Arc::ptr_eq(&same, &shadow));
    let other = shadow.element_start(at, 28).expect("a buffer from another start");
    assert!(!Arc::ptr_eq(&other, &shadow));
    assert_eq!(other.offset(at) % 28, 0);
    assert!(Arc::ptr_eq(other.card().unwrap(), &card));
    assert_eq!(other.left(at), shadow.left(at));
    // The same memory: a write through one is read through the other.
    let gpu = Gpu::new(host);
    let mut engine = Engine::new(host).expect("the engine");
    let mut s = Submission { number: 0 };
    let live = [card.clone()];
    s.run(&gpu, &mut engine, &live, &[], &[], false, || ());
    let (entries, data) = host.upload_buffer(8).expect("entries");
    let (_allocator, list) = gpu.list();
    // SAFETY: room for an entry; an open list, live buffers.
    unsafe {
        data.cast::<[u32; 2]>().write([0, 0x600d_f00d]);
        list.SetComputeRootSignature(&gpu.root);
        list.SetPipelineState(&gpu.scatter);
        list.SetComputeRoot32BitConstant(0, 1, 0);
        list.SetComputeRootUnorderedAccessView(1, other.gpu(at));
        list.SetComputeRootShaderResourceView(2, entries.GetGPUVirtualAddress());
        list.Dispatch(1, 1, 1);
        list.Close().expect("close");
    }
    gpu.run(&[&list.cast().expect("list")]);
    let has = gpu.read(&card);
    let k = 5 * PIECE + 40;
    assert_eq!(has[k..k + 4], 0x600d_f00du32.to_le_bytes());
}

#[test]
fn masks_hold_the_pages_of_a_range() {
    let naive = |first: usize, last: usize| {
        let mut m = vec![0u16; 8];
        for leaf in first..last {
            m[leaf / 16] |= 1 << (leaf % 16);
        }
        m
    };
    for first in 0..40 {
        for last in first..8 * 16 {
            let mut m = vec![0u16; 8];
            mark(&mut m, first, last);
            assert_eq!(m, naive(first, last), "{first}..{last}");
        }
    }
}

#[test]
fn runs_near_one_another_are_joined() {
    let mut runs = vec![(100, 4), (0, 8), (8, 4), (110, 2), (400, 4)];
    join_near(&mut runs, 0);
    assert_eq!(runs, [(0, 12), (100, 4), (110, 2), (400, 4)]);
    join_near(&mut runs, 16);
    assert_eq!(runs, [(0, 12), (100, 12), (400, 4)]);
}

/// Looks at all of a card grow apart while they find nothing.
#[test]
fn sweeps_grow_apart_and_come_back() {
    let Some(host) = Host::new_warp() else { return };
    let memory = Memory::new(PIECE);
    let card = plain(&host, &memory, 0, PIECE);
    assert_eq!(card.sweep.lock().unwrap().every, 1);
    for _ in 0..SWEEP_CLEAN * 3 {
        card.swept(false);
    }
    assert_eq!(card.sweep.lock().unwrap().every, 8);
    for _ in 0..SWEEP_CLEAN * 10 {
        card.swept(false);
    }
    assert_eq!(card.sweep.lock().unwrap().every, SWEEP_MOST);
    card.swept(true);
    assert_eq!(card.sweep.lock().unwrap().every, 1);
}

/// What the passes and copies around a submission take on the hardware.
#[test]
#[ignore = "hardware timing"]
fn hardware_card_costs() {
    let host = crate::host::host().expect("D3D12 host device");
    let gpu = Gpu::new(host);
    let mut engine = Engine::new(host).expect("the engine");
    let len = 185 << 20;
    let memory = Memory::new(len);
    // Not zeros, which no one would compare.
    for a in (0..len).step_by(64) {
        poke32(memory.at + a, (a as u32).wrapping_mul(0x9e37_79b1));
    }
    let card = plain(host, &memory, 0, len);
    let live = [card.clone()];
    let all = [(card.clone(), memory.at, memory.at + len)];
    let mut s = Submission { number: 0 };
    let started = std::time::Instant::now();
    s.run(&gpu, &mut engine, &live, &[], &all, false, || ());
    println!("185 MiB sent the first time, hashed and compared: {:.1} ms in all", started.elapsed().as_secs_f64() * 1e3);
    let mut best = [f64::MAX; 12];
    let keep = |i: usize, ms: f64, best: &mut [f64; 12]| best[i] = best[i].min(ms);
    for round in 0..5u32 {
        // The compare of all of it, with nothing changed.
        s.number += 1;
        let p = engine.prepare(host, &live, &[], &all, false, s.number, s.number - 1);
        assert!(p.before.is_none());
        let ms = gpu.time(&p.after.expect("a compare"));
        keep(0, ms, &mut best);
        let t = std::time::Instant::now();
        assert_eq!(engine.finish(host, p.plan.unwrap()), 0);
        keep(1, t.elapsed().as_secs_f64() * 1e3, &mut best);
        // Of 1 MiB.
        let part = [(card.clone(), memory.at + (64 << 20), memory.at + (65 << 20))];
        s.number += 1;
        let p = engine.prepare(host, &live, &[], &part, false, s.number, s.number - 1);
        keep(2, gpu.time(&p.after.expect("a compare")), &mut best);
        engine.finish(host, p.plan.unwrap());
        // 10 MB the title wrote, whole pages.
        let mb10 = 10 << 20;
        for a in (0..mb10).step_by(4) {
            poke32(memory.at + a, (a as u32).wrapping_add(round + 1));
        }
        let t = std::time::Instant::now();
        s.number += 1;
        let p = engine.prepare(host, &live, &[], &[], false, s.number, s.number - 1);
        keep(3, t.elapsed().as_secs_f64() * 1e3, &mut best);
        keep(4, gpu.time(&p.before.expect("an upload")), &mut best);
        // 10 MB in 64-byte pieces, every other one.
        for a in (0..2 * mb10).step_by(128) {
            for k in 0..16 {
                poke32(memory.at + a + k * 4, round * 7 + a as u32 + 1);
            }
        }
        let t = std::time::Instant::now();
        s.number += 1;
        let p = engine.prepare(host, &live, &[], &[], false, s.number, s.number - 1);
        keep(5, t.elapsed().as_secs_f64() * 1e3, &mut best);
        keep(6, gpu.time(&p.before.expect("an upload")), &mut best);
        // 5 MB shaders wrote, every dword of whole pages.
        let mb5 = 5 << 20;
        let entries: Vec<(u32, u32)> =
            (0..mb5 as u32 / 4).map(|k| ((64 << 20) + k * 4, k.wrapping_mul(round + 3) ^ 0x5555)).collect();
        let part = [(card.clone(), memory.at + (64 << 20), memory.at + (64 << 20) + mb5)];
        s.number += 1;
        let p = engine.prepare(host, &live, &[], &part, false, s.number, s.number - 1);
        let store = gpu.store(&card, &entries);
        gpu.run(&[&store.1]);
        keep(7, gpu.time(&p.after.expect("a compare")), &mut best);
        let t = std::time::Instant::now();
        let stored = engine.finish(host, p.plan.unwrap());
        keep(8, t.elapsed().as_secs_f64() * 1e3, &mut best);
        assert!(stored > mb5 * 9 / 10);
        // One dword in each of 1,000 pages.
        let entries: Vec<(u32, u32)> = (0..1000u32).map(|k| ((100 << 20) + k * 4096, k + round + 1)).collect();
        let part = [(card.clone(), memory.at + (100 << 20), memory.at + (104 << 20))];
        s.number += 1;
        let p = engine.prepare(host, &live, &[], &part, false, s.number, s.number - 1);
        let store = gpu.store(&card, &entries);
        gpu.run(&[&store.1]);
        keep(9, gpu.time(&p.after.expect("a compare")), &mut best);
        let t = std::time::Instant::now();
        assert_eq!(engine.finish(host, p.plan.unwrap()), 4000);
        keep(10, t.elapsed().as_secs_f64() * 1e3, &mut best);
    }
    println!("compare of 185 MiB, nothing changed: {:.3} ms on the GPU, finished in {:.3} ms", best[0], best[1]);
    println!("compare of 1 MiB, nothing changed: {:.3} ms", best[2]);
    println!("10 MB the title wrote: {:.2} ms to look at 185 MiB and take it, {:.2} ms to send", best[3], best[4]);
    println!("10 MB in 64-byte pieces: {:.2} ms to take, {:.2} ms to send", best[5], best[6]);
    println!("5 MB shaders wrote: {:.2} ms to compare and fetch, {:.2} ms to store", best[7], best[8]);
    println!("a dword in each of 1000 pages: {:.2} ms to compare and fetch, {:.2} ms to store", best[9], best[10]);
}
