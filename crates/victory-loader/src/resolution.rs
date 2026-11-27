//! The title's output size and render scale (`VICTORY_OUTPUT`,
//! `VICTORY_RENDER_SCALE`, `VICTORY_UPSCALER`; each off without it): places
//! in the title's code changed at load, before any of it runs. Every place
//! asked for is compared byte for byte with what it should hold first; if
//! one differs (another build of the title), none is changed, and the log
//! says which.
//!
//! Ported from Belobug's VIctory fork, branch a-publier
//! (github.com/Belobug/VIctory-Belobug-AMD, this file at 4d7185e, after
//! e151e71 and 45cad4c; it came there from the VIctory-7900XTX line by
//! f63b4b1, its picker patch after that repository's pull request #53).
//! Left out: `VICTORY_FFX_SCALE`, FidelityFX's own scale, which at 0.5 made
//! the title divide by zero 49 s in (+0x28f03e8, run res-6 there).
//! Changed here: all or nothing (upstream wrote each place that matched,
//! and an output size without its pool check stops at an `int3`).
//!
//! What the title does (game_scarlett_master.exe 1.0.2.1, image offsets):
//!
//! - **Output size.** Its settings (`settings.xml` in its storage,
//!   `video/resolutionFullscreen`; read into the live settings at
//!   +0x204/+0x208, the defaults' 1920x1080 at +0x5b37a20 overwritten) are
//!   applied by +0x22434a0: from +0x224352b it hands them to +0x2478130, the
//!   one writer of the screen size (+0xa9438cc/+0xa9438d0), and when the
//!   height changes calls +0xd8c3a0. The back buffers (+0x247ad10, by
//!   +0x501f4a0 and +0xcc2780) take that size through the copy +0x5eb24f0
//!   makes (+0x75e6fe0). The display mode list it matches against is its
//!   own (+0x5a38910: 1280x720, 1664x936, 1920x1080, 2560x1440, 3840x2160 at
//!   60 Hz, table +0x615c0c0), not the DXGI output's. On a Series X it ends
//!   at 3840x2160 whatever the file says (the owner's sessions: a
//!   2560x1440 scene into 3840x2160 with 1920x1080 in the file). Here that
//!   block becomes: write the size asked for into the settings
//!   (+0x204/+0x208), then the title's own else-path (screen size set,
//!   +0xd8c3a0 if the height changed). Everything that reads the settings
//!   after it (the device parameters +0x1e0fb30, the render size +0x7b4e10)
//!   sees the same size. The back buffers' pool (64 MB on Series X, 82 MB
//!   on Series S) is checked to be used up but for 2 MB (+0x247af25, an
//!   `int3`); with any output size asked for, 3840x2160 too, that check is
//!   jumped over.
//! - **Render scale.** The upscaler the title picks (+0x7e93f0: 6, its own,
//!   except on Series S with a flag, 7, AMD FidelityFX) gives the render
//!   size from the output size (+0x7e92f0 → the upscaler's +0x50 method):
//!   its own (+0x7e9440) multiplies by a scale at +0x90 of it, set to 2/3 at
//!   +0x28f1e78 (so 2560x1440 for 4K, 1280x720 for 1080p);
//!   `VICTORY_RENDER_SCALE` replaces that constant (upstream ran it at 1.0,
//!   run res-4). Below 2/3 the title's largest render size (+0x94,
//!   max(2/3, scale)) stays at 2/3.
//! - **Upscaler.** `VICTORY_UPSCALER=<kind>` makes the picker return
//!   `kind`: 6 the title's own, 7 FidelityFX. Series S picks FidelityFX,
//!   which NVIDIA forks saw draw a black world.

/// Sizes the title has a display mode for (its own list, +0x615c0c0).
pub const MODES: [(u32, u32); 5] = [(1280, 720), (1664, 936), (1920, 1080), (2560, 1440), (3840, 2160)];

/// What became of a place in the title's code (Belobug's a-publier
/// `frame_rate.rs`, 49cb83b, with `Withheld` added).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Patched {
    Done,
    /// Its bytes are not the ones expected: left alone.
    Differs,
    /// Its bytes were the ones expected, but another place asked for was
    /// not: left alone with it.
    Withheld,
    Failed(&'static str),
}

/// A switch's value: none (unset, empty or `off`), one, or one not
/// understood (said why where it is read).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Asked<T> {
    No,
    Yes(T),
    Bad,
}

/// A switch's value `spec` (the environment's), read with `parse`.
pub fn asked<T>(spec: Option<&str>, parse: impl Fn(&str) -> Option<T>) -> Asked<T> {
    match spec.map(str::trim) {
        None | Some("") => Asked::No,
        Some(s) if s.eq_ignore_ascii_case("off") => Asked::No,
        Some(s) => parse(s).map_or(Asked::Bad, Asked::Yes),
    }
}

/// `VICTORY_OUTPUT=<w>x<h>`: one of [`MODES`].
pub fn parse_output(spec: &str) -> Option<(u32, u32)> {
    let (w, h) = spec.trim().split_once(['x', 'X'])?;
    let size = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    MODES.contains(&size).then_some(size)
}

/// `VICTORY_RENDER_SCALE=<s>`, 0.5 to 1.0 (0.5: half the output's width and
/// height): the title's own upscaler's scale (2/3 without it).
pub fn parse_render_scale(spec: &str) -> Option<f32> {
    spec.trim().parse::<f32>().ok().filter(|s| (0.5..=1.0).contains(s))
}

/// `VICTORY_UPSCALER=<kind>`: 6 (the title's own) or 7 (FidelityFX).
pub fn parse_upscaler(spec: &str) -> Option<u32> {
    spec.trim().parse::<u32>().ok().filter(|k| matches!(k, 6 | 7))
}

/// A place to change: its offset in the image and the bytes there
/// (compared first; the replacement is at most as long).
#[derive(Debug)]
pub struct Site {
    pub at: usize,
    pub what: &'static str,
    pub bytes: &'static [u8],
}

/// The screen size setter, its screen height, and what +0x22434a0 calls when
/// the height changed.
pub const SET_SCREEN_SIZE: usize = 0x2478130;
pub const SCREEN_HEIGHT: usize = 0xa9438d0;
pub const HEIGHT_CHANGED: usize = 0xd8c3a0;

/// +0x22434a0 from +0x224352b to +0x2243568: the settings' size handed to
/// the screen size.
pub const OUTPUT: Site = Site {
    at: 0x224352b,
    what: "settings' output size applied",
    bytes: &[
        0x8b, 0x0d, 0x9b, 0x03, 0x70, 0x08, // mov ecx, [+0xa9438cc]
        0x8b, 0x86, 0x04, 0x02, 0x00, 0x00, // mov eax, [rsi+204h]
        0x39, 0xc1, 0x75, 0x0d, // cmp ecx, eax ; jne
        0x8b, 0x96, 0x08, 0x02, 0x00, 0x00, // mov edx, [rsi+208h]
        0xe8, 0xea, 0x4b, 0x23, 0x00, 0xeb, 0x20, // call +0x2478130 ; jmp +0x2243568
        0x8b, 0xbe, 0x08, 0x02, 0x00, 0x00, // mov edi, [rsi+208h]
        0x8b, 0x2d, 0x7c, 0x03, 0x70, 0x08, // mov ebp, [+0xa9438d0]
        0x89, 0xc1, 0x89, 0xfa, // mov ecx, eax ; mov edx, edi
        0xe8, 0xd3, 0x4b, 0x23, 0x00, // call +0x2478130
        0x39, 0xfd, 0x74, 0x07, 0xb1, 0x01, // cmp ebp, edi ; je ; mov cl, 1
        0xe8, 0x38, 0x8e, 0xb4, 0xfe, // call +0xd8c3a0
    ],
};

/// +0x247ad10, after the back buffers are made in their pool (64 MB on
/// Series X, 82 MB on Series S, chosen by the console at +0x247ad3e to
/// +0x247ad59): `int3` unless less than 2 MB of the pool is left over. Sized
/// for 3840x2160 on Series X; at 1920x1080 49 MB are left (upstream's run
/// res-1 stopped here), and Series S's pool leaves more than 2 MB at
/// 3840x2160 too. With any output size asked for the check becomes a jump
/// over the `int3`, to where `jbe` goes: at the title's own size that
/// changes nothing. The pool stays as large.
pub const POOL_CHECK: Site = Site {
    at: 0x247af25,
    what: "back buffer pool's left-over check",
    bytes: &[0x48, 0x3d, 0xff, 0xff, 0x1f, 0x00, 0x76, 0x01, 0xcc], // cmp rax, 1FFFFFh ; jbe +1 ; int3
};

/// The title's own upscaler, its method +0x28f1e70: `mov dword [rcx+90h],
/// 2/3` (the scale +0x7e9440 renders at).
pub const OWN_SCALE: Site = Site {
    at: 0x28f1e78,
    what: "the title's own upscaler's render scale",
    bytes: &[0xc7, 0x81, 0x90, 0x00, 0x00, 0x00, 0xab, 0xaa, 0x2a, 0x3f],
};

/// The upscaler picker, +0x7e93f0, to its first `ret`.
pub const PICKER: Site = Site {
    at: 0x7e93f0,
    what: "upscaler picker",
    bytes: &[
        0x48, 0x83, 0xec, 0x28, 0x80, 0x3d, 0x6d, 0xf0, 0x3a, 0x07, 0x00, 0x75, 0x09, 0x83, 0x3d, 0x68, 0xf0, 0x3a, 0x07, 0x00,
        0x74, 0x28, 0xe8, 0x35, 0xbd, 0x88, 0xff, 0x8a, 0x0d, 0x5b, 0xf0, 0x3a, 0x07, 0x80, 0xe1, 0x40, 0x80, 0xf9, 0x01, 0xb9,
        0x07, 0x00, 0x00, 0x00, 0x83, 0xd9, 0x00, 0x84, 0xc0, 0xb8, 0x06, 0x00, 0x00, 0x00, 0x0f, 0x45, 0xc1, 0x48, 0x83, 0xc4,
        0x28, 0xc3,
    ],
};

/// Every place this module may change.
pub const SITES: [&Site; 4] = [&OUTPUT, &POOL_CHECK, &OWN_SCALE, &PICKER];

/// `rel32` from the end of an instruction at image offset `next` to `to`.
fn rel(to: usize, next: usize) -> [u8; 4] {
    ((to as i64 - next as i64) as i32).to_le_bytes()
}

/// What goes at [`OUTPUT`] for `width` x `height` (the same length).
pub fn output_code(width: u32, height: u32) -> Vec<u8> {
    let at = OUTPUT.at;
    let mut c = Vec::with_capacity(OUTPUT.bytes.len());
    c.push(0xb8); // mov eax, width
    c.extend(width.to_le_bytes());
    c.extend([0x89, 0x86, 0x04, 0x02, 0x00, 0x00]); // mov [rsi+204h], eax
    c.push(0xbf); // mov edi, height
    c.extend(height.to_le_bytes());
    c.extend([0x89, 0xbe, 0x08, 0x02, 0x00, 0x00]); // mov [rsi+208h], edi
    c.extend([0x8b, 0x2d]); // mov ebp, [screen height]
    c.extend(rel(SCREEN_HEIGHT, at + c.len() + 4));
    c.extend([0x89, 0xc1, 0x89, 0xfa]); // mov ecx, eax ; mov edx, edi
    c.push(0xe8); // call the setter
    c.extend(rel(SET_SCREEN_SIZE, at + c.len() + 4));
    let end = OUTPUT.bytes.len();
    c.extend([0x39, 0xfd, 0x74, (end - (c.len() + 4)) as u8]); // cmp ebp, edi ; je end
    c.extend([0xb1, 0x01, 0xe8]); // mov cl, 1 ; call
    c.extend(rel(HEIGHT_CHANGED, at + c.len() + 4));
    c.extend([0xeb, (end - (c.len() + 2)) as u8]); // jmp end
    c.resize(end, 0xcc);
    c
}

/// What goes at [`POOL_CHECK`]: `jmp +1` in place of `jbe +1`.
pub fn pool_check_code() -> Vec<u8> {
    let mut c = POOL_CHECK.bytes[..8].to_vec();
    c[6] = 0xeb;
    c
}

/// What goes at [`OWN_SCALE`] for `scale`.
pub fn own_scale_code(scale: f32) -> Vec<u8> {
    let mut c = OWN_SCALE.bytes[..6].to_vec();
    c.extend(scale.to_le_bytes());
    c
}

/// `mov eax, kind ; ret`.
pub fn picker_code(kind: u32) -> Vec<u8> {
    let mut c = vec![0xb8];
    c.extend(kind.to_le_bytes());
    c.push(0xc3);
    c
}

/// A change to make: the place, its new bytes, and what was asked for.
pub type Change = (&'static Site, Vec<u8>, String);

/// The changes the switches ask for; none without any.
pub fn plan(output: Option<(u32, u32)>, scale: Option<f32>, upscaler: Option<u32>) -> Vec<Change> {
    let mut changes: Vec<Change> = Vec::new();
    if let Some((w, h)) = output {
        changes.push((&OUTPUT, output_code(w, h), format!("{w}x{h}")));
        // At any size: the pool's size depends on the console, not on it.
        changes.push((&POOL_CHECK, pool_check_code(), format!("{w}x{h}")));
    }
    if let Some(s) = scale {
        changes.push((&OWN_SCALE, own_scale_code(s), format!("{s}")));
    }
    if let Some(kind) = upscaler {
        changes.push((&PICKER, picker_code(kind), format!("returns {kind}")));
    }
    changes
}

/// Compares `site`'s bytes in the image of `size` bytes at `base`.
///
/// # Safety
/// The image is mapped.
pub unsafe fn matches(base: usize, size: usize, site: &Site) -> Patched {
    if site.at.checked_add(site.bytes.len()).is_none_or(|end| end > size) {
        return Patched::Failed("outside the image");
    }
    // SAFETY: inside the mapped image (checked above).
    let there = unsafe { std::slice::from_raw_parts((base + site.at) as *const u8, site.bytes.len()) };
    if there == site.bytes { Patched::Done } else { Patched::Differs }
}

/// Writes every change over its place if every place holds the bytes
/// expected; else writes nothing. What became of each, in order.
///
/// # Safety
/// The image of `size` bytes at `base` stays mapped and no thread runs its
/// code yet.
#[cfg(windows)]
pub unsafe fn write(base: usize, size: usize, changes: &[Change]) -> Vec<Patched> {
    use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut done: Vec<Patched> = changes
        .iter()
        .map(|(site, code, _)| {
            if code.len() > site.bytes.len() {
                Patched::Failed("replacement longer than the bytes compared")
            } else {
                // SAFETY: the caller's promise.
                unsafe { matches(base, size, site) }
            }
        })
        .collect();
    let withhold = |done: &mut Vec<Patched>| {
        for d in done.iter_mut().filter(|d| **d == Patched::Done) {
            *d = Patched::Withheld;
        }
    };
    if done.iter().any(|d| *d != Patched::Done) {
        withhold(&mut done);
        return done;
    }
    // All the places writable first, so that a refusal leaves all as they
    // were; put back in the reverse order, as places on one page each saw
    // the protection the one before left.
    let close = |opened: &[(usize, usize, u32)]| {
        for &(at, len, old) in opened.iter().rev() {
            let mut was = 0;
            // SAFETY: as when it was opened: inside the image.
            unsafe { VirtualProtect(at as *const _, len, old, &mut was) };
        }
    };
    let mut opened: Vec<(usize, usize, u32)> = Vec::with_capacity(changes.len());
    for (i, (site, code, _)) in changes.iter().enumerate() {
        let at = base + site.at;
        let mut old = 0;
        // SAFETY: inside the image (checked above); put back below.
        if unsafe { VirtualProtect(at as *const _, code.len(), PAGE_EXECUTE_READWRITE, &mut old) } == 0 {
            close(&opened);
            withhold(&mut done);
            done[i] = Patched::Failed("Windows would not make it writable");
            return done;
        }
        opened.push((at, code.len(), old));
    }
    for ((_, code, _), &(at, len, _)) in changes.iter().zip(&opened) {
        // SAFETY: the place's own bytes, writable now; nothing runs them yet.
        unsafe { std::ptr::copy_nonoverlapping(code.as_ptr(), at as *mut u8, len) };
    }
    close(&opened);
    for &(at, len, _) in &opened {
        // SAFETY: a plain call over the bytes written.
        unsafe { FlushInstructionCache(GetCurrentProcess(), at as *const _, len) };
    }
    done
}

/// Applies what the switches ask for; a line for each place in the log and
/// on standard output. Whether the title's code was changed.
///
/// # Safety
/// The image of `size` bytes at `base` stays mapped and no thread runs its
/// code yet.
#[cfg(windows)]
pub unsafe fn apply_asked(base: usize, size: usize) -> bool {
    let output = switch("VICTORY_OUTPUT", parse_output, "one of the title's modes: 1920x1080, 2560x1440, 1280x720, 1664x936, 3840x2160 (or off)");
    let scale = switch("VICTORY_RENDER_SCALE", parse_render_scale, "0.5 to 1.0 (or off)");
    let upscaler = switch("VICTORY_UPSCALER", parse_upscaler, "6 (the title's own) or 7 (FidelityFX), or off");
    let changes = plan(output, scale, upscaler);
    if changes.is_empty() {
        return false;
    }
    // SAFETY: the caller's promise.
    let done = unsafe { write(base, size, &changes) };
    let all = done.iter().all(|d| *d == Patched::Done);
    for ((site, _, value), done) in changes.iter().zip(&done) {
        let offset = format!("{:#x}", site.at);
        match done {
            Patched::Done => tracing::info!(site = %offset, what = site.what, value, "resolution: patched"),
            Patched::Differs => tracing::warn!(
                site = %offset,
                what = site.what,
                "resolution: not patched, the title's bytes there are not the ones expected (another build?)"
            ),
            Patched::Withheld => tracing::warn!(
                site = %offset,
                what = site.what,
                "resolution: not patched, another place asked for differs"
            ),
            Patched::Failed(why) => tracing::warn!(site = %offset, what = site.what, why, "resolution: not patched"),
        }
        println!("resolution     {} ({value}) at title+{offset}: {done:?}", site.what);
    }
    if !all {
        tracing::warn!("resolution: the title is left as it is: its own output size and render scale");
    }
    all
}

/// A switch read from the environment; said why if it is not understood.
#[cfg(windows)]
fn switch<T>(name: &str, parse: impl Fn(&str) -> Option<T>, expected: &str) -> Option<T> {
    let spec = std::env::var(name).ok();
    match asked(spec.as_deref(), parse) {
        Asked::No => None,
        Asked::Yes(v) => Some(v),
        Asked::Bad => {
            let spec = spec.unwrap_or_default();
            tracing::warn!(name, %spec, expected, "resolution: a switch not understood is left out");
            eprintln!("VIctory: {name}={spec}: expected {expected}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_parse() {
        assert_eq!(parse_output("1920x1080"), Some((1920, 1080)));
        assert_eq!(parse_output(" 2560X1440 "), Some((2560, 1440)));
        assert_eq!(parse_output("1600x900"), None);
        assert_eq!(parse_output("1080p"), None);
        assert_eq!(parse_render_scale("0.67"), Some(0.67));
        assert_eq!(parse_render_scale("0.5"), Some(0.5));
        assert_eq!(parse_render_scale("1"), Some(1.0));
        assert_eq!(parse_render_scale("0.4"), None);
        assert_eq!(parse_render_scale("1.5"), None);
        assert_eq!(parse_upscaler("6"), Some(6));
        assert_eq!(parse_upscaler("7"), Some(7));
        assert_eq!(parse_upscaler("5"), None);
        // Unset, empty and `off` ask for nothing; anything else not
        // understood is said so.
        for none in [None, Some(""), Some("  "), Some("off"), Some("OFF")] {
            assert_eq!(asked(none, parse_output), Asked::No, "{none:?}");
        }
        assert_eq!(asked(Some("1280x720"), parse_output), Asked::Yes((1280, 720)));
        assert_eq!(asked(Some("big"), parse_output), Asked::Bad);
        assert_eq!(asked(Some("2"), parse_render_scale), Asked::Bad);
    }

    /// Without a switch nothing is to be changed, and nothing of the image
    /// is read (an image at address 0 of no size is never looked at).
    #[cfg(windows)]
    #[test]
    fn without_switches_the_title_is_left_alone() {
        assert!(plan(None, None, None).is_empty());
        let unset = ["VICTORY_OUTPUT", "VICTORY_RENDER_SCALE", "VICTORY_UPSCALER"]
            .iter()
            .all(|v| std::env::var_os(v).is_none());
        if unset {
            // SAFETY: with nothing asked the image is not touched.
            assert!(!unsafe { apply_asked(0, 0) });
        }
    }

    /// What each switch changes: the output's block and, at any size (4K
    /// too: Series S's pool is larger), the pool's check; the scale; the
    /// picker.
    #[test]
    fn the_switches_plan_their_places() {
        let at = |changes: &[Change]| changes.iter().map(|c| c.0.at).collect::<Vec<_>>();
        for size in MODES {
            let changes = plan(Some(size), None, None);
            assert_eq!(at(&changes), [OUTPUT.at, POOL_CHECK.at], "{size:?}");
            assert_eq!(changes[1].1, pool_check_code());
        }
        assert_eq!(at(&plan(None, Some(1.0), None)), [OWN_SCALE.at]);
        assert_eq!(at(&plan(None, None, Some(6))), [PICKER.at]);
        let all = plan(Some((1280, 720)), Some(0.75), Some(6));
        assert_eq!(at(&all), [OUTPUT.at, POOL_CHECK.at, OWN_SCALE.at, PICKER.at]);
        assert_eq!(all[0].1, output_code(1280, 720));
        assert_eq!(all[2].1, own_scale_code(0.75));
        for (site, code, _) in &all {
            assert!(code.len() <= site.bytes.len(), "{}", site.what);
        }
    }

    /// The calls and loads in the bytes compared are the ones the
    /// replacement keeps.
    #[test]
    fn the_output_block_is_the_titles() {
        let b = OUTPUT.bytes;
        assert_eq!(b.len(), 61);
        let target = |op_end: usize| {
            (OUTPUT.at + op_end) as i64 + i64::from(i32::from_le_bytes(b[op_end - 4..op_end].try_into().unwrap()))
        };
        assert_eq!(target(27), SET_SCREEN_SIZE as i64);
        assert_eq!(target(50), SET_SCREEN_SIZE as i64);
        assert_eq!(target(61), HEIGHT_CHANGED as i64);
        assert_eq!(target(41), SCREEN_HEIGHT as i64);
        assert_eq!(target(6), (SCREEN_HEIGHT - 4) as i64, "the width before it");
        // jne to the else path at 29; jmp +0x20 and je +7 both land at the end.
        assert_eq!((b[14], 16 + b[15] as usize), (0x75, 29));
        assert_eq!((b[27], 29 + b[28] as usize), (0xeb, 61));
        assert_eq!((b[52], 54 + b[53] as usize), (0x74, 61));
    }

    #[test]
    fn the_output_code_keeps_the_titles_else_path() {
        let c = output_code(1920, 1080);
        assert_eq!(c.len(), OUTPUT.bytes.len());
        assert_eq!(c[0], 0xb8);
        assert_eq!(c[1..5], 1920u32.to_le_bytes());
        assert_eq!(c[5..11], [0x89, 0x86, 0x04, 0x02, 0x00, 0x00]);
        assert_eq!(c[12..16], 1080u32.to_le_bytes());
        assert_eq!(c[16..22], [0x89, 0xbe, 0x08, 0x02, 0x00, 0x00]);
        let target = |op_end: usize| {
            (OUTPUT.at + op_end) as i64 + i64::from(i32::from_le_bytes(c[op_end - 4..op_end].try_into().unwrap()))
        };
        assert_eq!(c[22..24], [0x8b, 0x2d]);
        assert_eq!(target(28), SCREEN_HEIGHT as i64);
        assert_eq!(c[32], 0xe8);
        assert_eq!(target(37), SET_SCREEN_SIZE as i64);
        assert_eq!(c[37..40], [0x39, 0xfd, 0x74]);
        assert_eq!(41 + c[40] as usize, 61, "je lands at the end");
        assert_eq!(c[43], 0xe8);
        assert_eq!(target(48), HEIGHT_CHANGED as i64);
        assert_eq!(c[48], 0xeb);
        assert_eq!(50 + c[49] as usize, 61, "jmp lands at the end");
        assert!(c[50..].iter().all(|&x| x == 0xcc));
    }

    #[test]
    fn the_scale_and_picker_codes() {
        let own = own_scale_code(0.5);
        assert_eq!(own[..6], OWN_SCALE.bytes[..6]);
        assert_eq!(own[6..], 0.5f32.to_le_bytes());
        assert_eq!(OWN_SCALE.bytes[6..], (2.0f32 / 3.0).to_le_bytes());
        assert_eq!(picker_code(6), [0xb8, 6, 0, 0, 0, 0xc3]);
        assert_eq!(pool_check_code(), [0x48, 0x3d, 0xff, 0xff, 0x1f, 0x00, 0xeb, 0x01]);
    }

    /// Places are written only when every one holds what it should, and
    /// their pages are left as they were found (two places share a page).
    #[cfg(windows)]
    #[test]
    fn places_are_written_all_or_none() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ, PAGE_READWRITE,
            VirtualAlloc, VirtualFree, VirtualProtect, VirtualQuery,
        };
        static A: Site = Site { at: 0x100, what: "a", bytes: OWN_SCALE.bytes };
        static B: Site = Site { at: 0x200, what: "b", bytes: POOL_CHECK.bytes };
        static C: Site = Site { at: 0x1100, what: "c", bytes: PICKER.bytes };
        static FAR: Site = Site { at: 0x2ff8, what: "far", bytes: PICKER.bytes };
        const SIZE: usize = 0x3000;
        // SAFETY: fresh memory of our own, read-only to code as the image's
        // is; written here only through `write` or after being made
        // writable, and released at the end.
        unsafe {
            let base = VirtualAlloc(std::ptr::null(), SIZE, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE).cast::<u8>();
            assert!(!base.is_null());
            for s in [&A, &B, &C] {
                std::ptr::copy_nonoverlapping(s.bytes.as_ptr(), base.add(s.at), s.bytes.len());
            }
            let mut old = 0;
            assert_ne!(VirtualProtect(base.cast(), SIZE, PAGE_EXECUTE_READ, &mut old), 0);
            let protection = |at: usize| {
                let mut info: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
                VirtualQuery(base.add(at).cast(), &mut info, size_of::<MEMORY_BASIC_INFORMATION>());
                info.Protect
            };
            let poke = |at: usize, byte: u8| {
                let mut was = 0;
                VirtualProtect(base.add(at).cast(), 1, PAGE_READWRITE, &mut was);
                *base.add(at) = byte;
                VirtualProtect(base.add(at).cast(), 1, was, &mut was);
            };
            let bytes = |s: &Site| std::slice::from_raw_parts(base.add(s.at), s.bytes.len()).to_vec();
            let changes: Vec<Change> = vec![
                (&A, own_scale_code(1.0), String::new()),
                (&B, pool_check_code(), String::new()),
                (&C, picker_code(6), String::new()),
            ];
            // One place differs (another build): nothing is written.
            poke(C.at + 4, 0x81);
            let done = write(base as usize, SIZE, &changes);
            assert_eq!(done, [Patched::Withheld, Patched::Withheld, Patched::Differs]);
            assert_eq!(bytes(&A), A.bytes);
            assert_eq!(bytes(&B), B.bytes);
            // One outside the image: nothing either.
            poke(C.at + 4, PICKER.bytes[4]);
            let mut outside = changes.clone();
            outside.push((&FAR, picker_code(6), String::new()));
            let done = write(base as usize, SIZE, &outside);
            assert_eq!(done[..3], [Patched::Withheld; 3]);
            assert!(matches!(done[3], Patched::Failed(_)));
            assert_eq!(bytes(&C), C.bytes);
            // All as expected: all written, the pages as they were.
            assert_eq!(write(base as usize, SIZE, &changes), [Patched::Done; 3]);
            assert_eq!(bytes(&A)[6..], 1.0f32.to_le_bytes());
            assert_eq!(bytes(&B)[..8], pool_check_code()[..]);
            assert_eq!(bytes(&B)[8], 0xcc, "the int3 stays, jumped over");
            assert_eq!(bytes(&C)[..6], picker_code(6)[..]);
            assert_eq!(bytes(&C)[6..], C.bytes[6..]);
            assert_eq!((protection(A.at), protection(C.at)), (PAGE_EXECUTE_READ, PAGE_EXECUTE_READ));
            // Done already: the bytes differ now, nothing is written again.
            let again: Vec<Change> = vec![(&A, own_scale_code(0.5), String::new())];
            assert_eq!(write(base as usize, SIZE, &again), [Patched::Differs]);
            assert_eq!(bytes(&A)[6..], 1.0f32.to_le_bytes());
            VirtualFree(base.cast(), 0, MEM_RELEASE);
        }
    }

    /// The owner's executable on disk (`VICTORY_TITLE_EXE`, read only; the
    /// test says so and passes without it): every place holds the bytes
    /// compared, in code, with no base relocation and no TLS load the
    /// loader rewrites in it (the loaded image then holds them too,
    /// wherever it is mapped), for build 1.0.2.1.
    #[test]
    fn the_places_are_the_titles_on_disk() {
        const BUILD: &str = "b99f876b74d4a77f03af5f878223b27bd647b5c35a79d5decd9f716504239e93";
        const EXECUTE: u32 = 0x2000_0000;
        let Some(path) = std::env::var_os("VICTORY_TITLE_EXE") else {
            println!("VICTORY_TITLE_EXE is not set: the title's bytes are not checked");
            return;
        };
        let bytes = std::fs::read(&path).expect("the title's executable");
        let hash = blake3::hash(&bytes).to_hex().to_string();
        println!("{} is {hash}", std::path::Path::new(&path).display());
        assert_eq!(hash, BUILD, "not the 1.0.2.1 build these places are for");
        let info = victory_pe::ImageInfo::parse(&bytes).unwrap();
        let relocations: Vec<u32> = crate::prepare::dir64_relocations(&bytes, &info).into_iter().map(|r| r.1).collect();
        assert!(!relocations.is_empty());
        for site in SITES {
            let len = site.bytes.len() as u32;
            let at = site.at as u32;
            let file = info.rva_to_file_offset(at).expect("file-backed") as usize;
            let last = info.rva_to_file_offset(at + len - 1);
            assert_eq!(last, Some((file + site.bytes.len() - 1) as u32), "{}: in one section", site.what);
            let section = info.sections.iter().find(|s| (s.rva..s.rva + s.virtual_size).contains(&at)).unwrap();
            assert!(section.characteristics & EXECUTE != 0, "{} is code", site.what);
            assert_eq!(&bytes[file..file + site.bytes.len()], site.bytes, "{}", site.what);
            let inside = relocations.iter().filter(|&&r| r + 8 > at && r < at + len).count();
            assert_eq!(inside, 0, "{}: a relocation would change its bytes", site.what);
            // Nor does the loader's own change of the image, its TLS loads
            // (`tls::redirect`, 9 bytes each): none starts in reach of it.
            #[cfg(windows)]
            {
                let mut around = bytes[file - 8..file + site.bytes.len() + 8].to_vec();
                assert_eq!(crate::tls::patch(&mut around, 0x1000, 0x2000), 0, "{}: a TLS load in it", site.what);
            }
            println!("{} at +{:#x} (file {file:#x}, {}): matches", site.what, site.at, section.name);
        }
    }
}
