//! The two directions of a card's bytes on the CPU ([`super`]).
//!
//! [`differing`]: title memory against the CPU's side of the card (what the
//! host GPU has). Only dwords that differ are taken, and only those go up:
//! what shaders wrote beside them and has not come back stays.
//!
//! [`put_back`]: what shaders wrote into title memory. A dword that the GPU
//! has otherwise than the CPU's side is a shader's, and is stored; nothing
//! else of title memory is written or even read, so what the title's threads
//! write beside it meanwhile stays theirs. (The console's GPU writes whole
//! dwords too.)

/// The unit title memory is taken in.
const GRAIN: usize = 4;

/// Copies the dwords of title memory at `title` that differ from `side` (as
/// long) into it, and adds where they are to `runs` (offset from `at`,
/// bytes), joined where they touch.
///
/// The title may be writing that memory: each dword is taken as it is read
/// once, and what it writes after is looked at the next time.
///
/// # Safety
/// `title` is readable for as many bytes.
pub(super) unsafe fn differing(title: *const u8, side: &mut [u8], at: u32, runs: &mut Vec<(u32, u32)>) {
    let n = side.len();
    let mut take = |from: usize, bytes: &[u8], side: &mut [u8]| {
        side[from..from + bytes.len()].copy_from_slice(bytes);
        let from = at + from as u32;
        match runs.last_mut() {
            Some((a, l)) if *a + *l == from => *l += bytes.len() as u32,
            _ => runs.push((from, bytes.len() as u32)),
        }
    };
    let mut i = 0;
    while i < n {
        let len = CHUNK.min(n - i);
        // SAFETY: the caller's: `i + len` is inside.
        let from = unsafe { title.add(i) };
        // Most of a page is as it was: 64 bytes at a time while that lasts.
        // SAFETY: as above.
        if len == CHUNK && unsafe { same_chunk(from, side[i..].as_ptr()) } {
            i += CHUNK;
            continue;
        }
        let mut now = [0u8; CHUNK];
        // SAFETY: as above, into a buffer of our own.
        unsafe { std::ptr::copy_nonoverlapping(from, now.as_mut_ptr(), len) };
        for k in (0..len).step_by(GRAIN) {
            let grain = &now[k..(k + GRAIN).min(len)];
            if *grain != side[i + k..i + k + grain.len()] {
                take(i + k, grain, side);
            }
        }
        i += len;
    }
}

/// Bytes compared as one.
const CHUNK: usize = 64;

/// Whether [`CHUNK`] bytes are the same.
///
/// # Safety
/// Both are readable for that many bytes.
#[inline(always)]
unsafe fn same_chunk(a: *const u8, b: *const u8) -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::{_mm_and_si128, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8};
        // SAFETY: the caller's; SSE2 is part of x86-64.
        unsafe {
            let eq = |k: usize| _mm_cmpeq_epi8(_mm_loadu_si128(a.add(k).cast()), _mm_loadu_si128(b.add(k).cast()));
            let all = _mm_and_si128(_mm_and_si128(eq(0), eq(16)), _mm_and_si128(eq(32), eq(48)));
            _mm_movemask_epi8(all) == 0xffff
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        // SAFETY: the caller's.
        unsafe { std::slice::from_raw_parts(a, CHUNK) == std::slice::from_raw_parts(b, CHUNK) }
    }
}

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".globl victory_card_merge",
    ".globl victory_card_merge_end",
    ".globl victory_card_merge_fault",
    // rcx: title memory, rdx: the GPU's bytes, r8: the CPU's side, r9: how
    // many (a multiple of 32, not 0). eax: the dwords stored.
    "victory_card_merge:",
    "xor eax, eax",
    "vpcmpeqd ymm3, ymm3, ymm3",
    "3:",
    "vmovdqu ymm0, ymmword ptr [rdx]",
    "vpcmpeqd ymm1, ymm0, ymmword ptr [r8]",
    "vmovmskps r10d, ymm1",
    "xor r10d, 255",
    "jz 4f",
    "vpxor ymm1, ymm1, ymm3",
    "vpmaskmovd ymmword ptr [rcx], ymm1, ymm0",
    "vmovdqu ymmword ptr [r8], ymm0",
    "popcnt r10d, r10d",
    "add eax, r10d",
    "4:",
    "add rcx, 32",
    "add rdx, 32",
    "add r8, 32",
    "sub r9, 32",
    "jnz 3b",
    "victory_card_merge_end:",
    "vzeroupper",
    "ret",
    "victory_card_merge_fault:",
    "or eax, 2147483648",
    "vzeroupper",
    "ret",
);

#[cfg(target_arch = "x86_64")]
unsafe extern "C" {
    fn victory_card_merge(title: *mut u8, gpu: *const u8, side: *mut u8, len: usize) -> u32;
    static victory_card_merge_end: u8;
    static victory_card_merge_fault: u8;
}

/// Set in what `victory_card_merge` gives when title memory was not there.
#[cfg(target_arch = "x86_64")]
const FAULTED: u32 = 1 << 31;

/// Where the fault handler goes on when the merge at `rip` met title memory
/// that is not there (and is not guarded).
#[cfg(target_arch = "x86_64")]
pub(crate) fn fault_at(rip: usize) -> Option<usize> {
    let (start, end) = (victory_card_merge as *const () as usize, (&raw const victory_card_merge_end) as usize);
    (rip >= start && rip < end).then_some((&raw const victory_card_merge_fault) as usize)
}

#[cfg(target_arch = "x86_64")]
fn fast() -> bool {
    std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("popcnt")
}

/// Stores the dwords of `gpu` that differ from `side` (as long, a multiple
/// of 4 bytes) into title memory at `title`, and into `side`. How many bytes
/// were stored. Title memory that is not there is left alone; `side` holds
/// the GPU's bytes after in any case.
///
/// `title` is a multiple of 4.
pub(super) fn put_back(title: usize, gpu: &[u8], side: &mut [u8]) -> usize {
    let n = gpu.len().min(side.len()) & !3;
    let mut done = 0;
    let mut stored = 0usize;
    #[cfg(target_arch = "x86_64")]
    if n >= 32 && fast() {
        let len = n & !31;
        // SAFETY: both slices hold `len` bytes; the routine uses volatile
        // registers only and comes back from a fault at title memory through
        // the handler `write_handler` set up.
        let dwords = unsafe {
            crate::host::ensure_safe_copy_veh();
            victory_card_merge(title as *mut u8, gpu.as_ptr(), side.as_mut_ptr(), len)
        };
        stored += (dwords & !FAULTED) as usize * 4;
        if dwords & FAULTED != 0 {
            side[..len].copy_from_slice(&gpu[..len]);
        }
        done = len;
    }
    // The rest, and all of it without AVX2: run by run.
    let dword = |bytes: &[u8], at: usize| u32::from_ne_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    while done < n {
        if dword(gpu, done) == dword(side, done) {
            done += 4;
            continue;
        }
        let from = done;
        while done < n && dword(gpu, done) != dword(side, done) {
            done += 4;
        }
        crate::host::write_mapped((title + from) as *mut u8, gpu[from..].as_ptr(), done - from);
        side[from..done].copy_from_slice(&gpu[from..done]);
        stored += done - from;
    }
    stored
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    /// The grains taken make the side equal to title memory, and no grain
    /// that was the same is taken.
    #[test]
    fn differing_grains_are_taken_and_no_others() {
        let mut state = 0x1234_5678_9abc_def1;
        for len in [1usize, 15, 16, 17, 64, 100, 4096, 65536, 65531] {
            for changes in [0usize, 1, 5, 200] {
                let title0: Vec<u8> = (0..len).map(|_| rng(&mut state) as u8).collect();
                let mut side = title0.clone();
                let mut title = title0.clone();
                let mut grains = std::collections::BTreeSet::new();
                for _ in 0..changes {
                    let at = rng(&mut state) as usize % len;
                    title[at] ^= 0x41 | 1;
                    grains.insert(at / GRAIN);
                }
                let grains: std::collections::BTreeSet<usize> = grains
                    .into_iter()
                    .filter(|g| {
                        let (a, b) = (g * GRAIN, ((g + 1) * GRAIN).min(len));
                        title[a..b] != title0[a..b]
                    })
                    .collect();
                let mut runs = vec![(7, 3)];
                // SAFETY: as long as the side.
                unsafe { differing(title.as_ptr(), &mut side, 1000, &mut runs) };
                assert_eq!(runs[0], (7, 3), "runs of before stay");
                assert!(side == title, "{len} {changes}");
                let mut taken = std::collections::BTreeSet::new();
                let mut last = 0;
                for &(a, l) in &runs[1..] {
                    assert!(a >= last, "in order");
                    last = a + l + 1;
                    let (a, b) = (a as usize - 1000, (a + l) as usize - 1000);
                    taken.extend(a / GRAIN..b.div_ceil(GRAIN));
                }
                assert_eq!(taken, grains, "{len} {changes}");
            }
        }
    }

    /// What the GPU changed is stored, dword by dword; what the title wrote
    /// meanwhile in other dwords stays; the side holds the GPU's bytes.
    #[test]
    fn only_the_gpus_dwords_are_stored() {
        let mut state = 0xfeed_f00d_1234_5679;
        for len in [4usize, 28, 32, 60, 64, 100, 4096] {
            let side0: Vec<u8> = (0..len).map(|_| rng(&mut state) as u8).collect();
            let mut gpu = side0.clone();
            let mut title = side0.clone();
            let mut wrote = vec![false; len / 4];
            for d in 0..len / 4 {
                match rng(&mut state) % 5 {
                    0 => {
                        gpu[d * 4 + rng(&mut state) as usize % 4] ^= 0x55;
                        wrote[d] = true;
                    }
                    // The title wrote it meanwhile.
                    1 => title[d * 4 + rng(&mut state) as usize % 4] ^= 0x2a,
                    _ => {}
                }
            }
            let title0 = title.clone();
            let mut side = side0.clone();
            let stored = put_back(title.as_mut_ptr() as usize, &gpu, &mut side);
            assert_eq!(stored, 4 * wrote.iter().filter(|w| **w).count(), "{len}");
            for d in 0..len / 4 {
                let at = d * 4..d * 4 + 4;
                let wanted = if wrote[d] { &gpu } else { &title0 };
                assert_eq!(title[at.clone()], wanted[at], "{len} dword {d}");
            }
            let whole = len & !3;
            assert_eq!(side[..whole], gpu[..whole]);
            assert_eq!(put_back(title.as_mut_ptr() as usize, &gpu, &mut side), 0, "nothing left");
        }
    }

    /// Title memory that went away: nothing happens but the side's update.
    #[test]
    fn title_memory_that_is_gone_is_left_alone() {
        use windows_sys::Win32::System::Memory::*;
        // SAFETY: a reservation of our own, one page of two committed.
        let base = unsafe {
            let base = VirtualAlloc(std::ptr::null(), 8192, MEM_RESERVE, PAGE_NOACCESS) as usize;
            assert_ne!(base, 0);
            assert!(!VirtualAlloc(base as *const _, 4096, MEM_COMMIT, PAGE_READWRITE).is_null());
            base
        };
        let gpu = vec![7u8; 4096];
        let mut side = vec![0u8; 4096];
        assert_eq!(put_back(base, &gpu, &mut side), 4096);
        // SAFETY: the committed page.
        assert!(unsafe { std::slice::from_raw_parts(base as *const u8, 4096) }.iter().all(|&b| b == 7));
        let mut side = vec![0u8; 4096];
        put_back(base + 4096, &gpu, &mut side);
        assert!(side == gpu);
        // SAFETY: our reservation.
        unsafe { VirtualFree(base as *mut _, 0, MEM_RELEASE) };
    }
}
