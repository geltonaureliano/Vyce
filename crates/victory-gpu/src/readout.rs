//! The frame rate and frame time written in the picture's top left corner,
//! as rectangles cleared on the back buffer: no shader, texture or font.
//! `VICTORY_FPS=0` turns it off. Under it, how far a load has come
//! ([`crate::loading`]).

use std::sync::OnceLock;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D12::{
    D3D12_CPU_DESCRIPTOR_HANDLE, ID3D12GraphicsCommandList,
};

/// Rectangles cleared by one call. NVIDIA's driver (610.88, RTX 2070 SUPER)
/// keeps them in an array of 32 on its stack and ends the process past it
/// (0xc0000409 in nvwgf2umx.dll): the readout has about a hundred.
pub(crate) const RECTS_AT_ONCE: usize = 32;

/// Clears `rects` of the render target `view` to `color`.
///
/// # Safety
/// `list` is open; `view` is a live view of a texture in the render target
/// state that holds the rectangles.
pub(crate) unsafe fn clear(
    list: &ID3D12GraphicsCommandList,
    view: D3D12_CPU_DESCRIPTOR_HANDLE,
    color: &[f32; 4],
    rects: &[RECT],
) {
    for part in rects.chunks(RECTS_AT_ONCE) {
        // SAFETY: the caller's.
        unsafe { list.ClearRenderTargetView(view, color, Some(part)) };
    }
}

/// Whether the frame rate is drawn.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_FPS").map_or(true, |v| v != "0"))
}

/// A glyph's seven rows, top first, of five dots each (bit 4 is the left).
fn glyph(c: char) -> [u8; 7] {
    match c {
        '0' => [0x0e, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0e],
        '1' => [0x04, 0x0c, 0x04, 0x04, 0x04, 0x04, 0x0e],
        '2' => [0x0e, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1f],
        '3' => [0x1f, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0e],
        '4' => [0x02, 0x06, 0x0a, 0x12, 0x1f, 0x02, 0x02],
        '5' => [0x1f, 0x10, 0x1e, 0x01, 0x01, 0x11, 0x0e],
        '6' => [0x06, 0x08, 0x10, 0x1e, 0x11, 0x11, 0x0e],
        '7' => [0x1f, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0e, 0x11, 0x11, 0x0e, 0x11, 0x11, 0x0e],
        '9' => [0x0e, 0x11, 0x11, 0x0f, 0x01, 0x02, 0x0c],
        'A' => [0x0e, 0x11, 0x11, 0x1f, 0x11, 0x11, 0x11],
        'C' => [0x0e, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0e],
        'D' => [0x1e, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1e],
        'E' => [0x1f, 0x10, 0x10, 0x1e, 0x10, 0x10, 0x1f],
        'F' => [0x1f, 0x10, 0x10, 0x1e, 0x10, 0x10, 0x10],
        'G' => [0x0e, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0f],
        'H' => [0x11, 0x11, 0x11, 0x1f, 0x11, 0x11, 0x11],
        'I' => [0x0e, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0e],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1f],
        'M' => [0x11, 0x1b, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x11],
        'O' => [0x0e, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0e],
        'P' => [0x1e, 0x11, 0x11, 0x1e, 0x10, 0x10, 0x10],
        'R' => [0x1e, 0x11, 0x11, 0x1e, 0x14, 0x12, 0x11],
        'S' => [0x0f, 0x10, 0x10, 0x0e, 0x01, 0x01, 0x1e],
        'T' => [0x1f, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x0c, 0x0c],
        ':' => [0x00, 0x0c, 0x0c, 0x00, 0x0c, 0x0c, 0x00],
        '%' => [0x18, 0x19, 0x02, 0x04, 0x08, 0x13, 0x03],
        '~' => [0x00, 0x00, 0x08, 0x15, 0x02, 0x00, 0x00],
        '?' => [0x0e, 0x11, 0x01, 0x02, 0x04, 0x00, 0x04],
        _ => [0; 7],
    }
}

/// The text's width and height in dots, with a dot of margin all round.
pub(crate) fn size(text: &str) -> (i32, i32) {
    (text.chars().count() as i32 * 6 + 1, 9)
}

/// The rectangles of `text` with its top left margin at `at` and dots of
/// `dot` pixels: one for each run of dots in a row.
pub(crate) fn rects(text: &str, at: (i32, i32), dot: i32) -> Vec<RECT> {
    let mut all = Vec::new();
    for (i, c) in text.chars().enumerate() {
        let left = at.0 + (1 + i as i32 * 6) * dot;
        for (row, bits) in glyph(c).into_iter().enumerate() {
            let top = at.1 + (1 + row as i32) * dot;
            let mut x = 0;
            while x < 5 {
                if bits & (0x10 >> x) == 0 {
                    x += 1;
                    continue;
                }
                let start = x;
                while x < 5 && bits & (0x10 >> x) != 0 {
                    x += 1;
                }
                all.push(RECT {
                    left: left + start * dot,
                    top,
                    right: left + x * dot,
                    bottom: top + dot,
                });
            }
        }
    }
    all
}

/// The rows of dots a bar adds to the height of a line.
const BAR: i32 = 3;

/// The bar under a text of width `w` (in dots) at `at`, and the part of it
/// that is full, if any is.
fn bar(w: i32, at: (i32, i32), dot: i32, share: f64) -> (RECT, Option<RECT>) {
    let (left, top, width) = (at.0 + dot, at.1 + 9 * dot, (w - 2) * dot);
    let whole = RECT {
        left,
        top,
        right: left + width,
        bottom: top + 2 * dot,
    };
    let full = (f64::from(width) * share.clamp(0.0, 1.0)) as i32;
    let done = RECT {
        right: left + full,
        ..whole
    };
    (whole, (full > 0).then_some(done))
}

/// Writes `lines` under each other in the top left corner of a picture of
/// `size`, each with how much of a bar under it is full, or without one. A
/// line that does not fit is left out, and those after it.
///
/// # Safety
/// `list` is open; `view` is a live view of a texture of that size in the
/// render target state.
pub(crate) unsafe fn draw<'l>(
    list: &ID3D12GraphicsCommandList,
    view: D3D12_CPU_DESCRIPTOR_HANDLE,
    size: (u32, u32),
    lines: impl Iterator<Item = (&'l str, Option<f64>)>,
) {
    const PAPER: [f32; 4] = [0.0, 0.0, 0.0, 1.0];
    const TRACK: [f32; 4] = [0.25, 0.25, 0.25, 1.0];
    const INK: [f32; 4] = [1.0, 0.9, 0.1, 1.0];
    let (width, height) = (size.0 as i32, size.1 as i32);
    // Dots that keep their share of the picture at any size.
    let dot = (height / 216).max(2);
    let mut at = (dot * 3, dot * 3);
    for (text, share) in lines {
        let (w, h) = self::size(text);
        let h = h + if share.is_some() { BAR } else { 0 };
        let behind = RECT {
            left: at.0,
            top: at.1,
            right: at.0 + w * dot,
            bottom: at.1 + h * dot,
        };
        if behind.right > width || behind.bottom > height {
            return;
        }
        // SAFETY: the caller's; every rectangle is inside the picture.
        unsafe {
            clear(list, view, &PAPER, &[behind]);
            clear(list, view, &INK, &rects(text, at, dot));
            if let Some(share) = share {
                let (whole, done) = bar(w, at, dot, share);
                clear(list, view, &TRACK, &[whole]);
                clear(list, view, &INK, done.as_slice());
            }
        }
        at.1 = behind.bottom + dot;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_of_dots_is_one_rectangle() {
        // The top row of "F" is five dots.
        let all = rects("F", (10, 20), 4);
        assert_eq!(
            all[0],
            RECT {
                left: 14,
                top: 24,
                right: 34,
                bottom: 28
            }
        );
        assert!(rects(" ", (0, 0), 4).is_empty());
        assert_eq!(size("12 FPS"), (37, 9));
    }

    /// The longest line of each kind, with the longest frame rate.
    const LINES: [(&str, Option<f64>); 4] = [
        ("888.8 FPS  8888 MS", None),
        ("CARREGANDO 99%  ~99 MIN", Some(0.99)),
        ("CARREGANDO ~99%?  HA 99:59", Some(0.5)),
        ("FALTAM 8888 PIPELINES  ~99 MIN", None),
    ];

    #[test]
    fn every_character_shown_has_a_glyph() {
        let more = "0123456789 INICIANDO ~40 S";
        for c in LINES.iter().flat_map(|l| l.0.chars()).chain(more.chars()) {
            assert!(c == ' ' || glyph(c) != [0; 7], "{c:?}");
        }
        // A row has five dots.
        for c in ('\0'..='~').filter(|&c| glyph(c) != [0; 7]) {
            assert!(glyph(c).iter().all(|row| *row < 0x20), "{c:?}");
        }
    }

    #[test]
    fn the_lines_fit_the_pictures_shown() {
        for (width, height) in [(1280, 720), (1920, 1080), (2560, 1440), (3840, 2160)] {
            let dot = (height / 216).max(2);
            let mut bottom = dot * 3;
            for (text, share) in &LINES[..2] {
                let h = size(text).1 + share.map_or(0, |_| BAR);
                bottom += (h + 1) * dot;
            }
            assert!(bottom < height / 8, "{bottom} of {height}");
            for (text, _) in LINES {
                assert!(dot * 3 + size(text).0 * dot < width / 2, "{text} at {width}");
            }
        }
    }

    #[test]
    fn a_bar_is_as_full_as_it_is_told() {
        let (whole, done) = bar(139, (30, 120), 10, 0.5);
        assert_eq!((whole.left, whole.top, whole.right, whole.bottom), (40, 210, 1410, 230));
        assert_eq!(done.unwrap().right, 40 + 1370 / 2);
        assert!(bar(139, (30, 120), 10, 0.0).1.is_none());
        assert_eq!(bar(139, (30, 120), 10, 7.0).1.unwrap(), whole);
        assert!(bar(139, (30, 120), 10, f64::NAN).1.is_none());
    }

    /// On the hardware device, as the driver is what failed: the whole text
    /// in one call ended the process while recording.
    #[test]
    fn a_readout_of_many_rectangles_records() {
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::Win32::Graphics::Dxgi::Common::*;
        let Some(host) = crate::host::host() else { return };
        for (text, _) in LINES {
            assert!(rects(text, (15, 15), 5).len() > RECTS_AT_ONCE);
        }
        // SAFETY: objects of our own; the list is never run.
        unsafe {
            let mut texture: Option<ID3D12Resource> = None;
            host.device
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_DEFAULT,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &D3D12_RESOURCE_DESC {
                        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                        Width: 1920,
                        Height: 1080,
                        DepthOrArraySize: 1,
                        MipLevels: 1,
                        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                        Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                        ..Default::default()
                    },
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                    None,
                    &mut texture,
                )
                .unwrap();
            let texture = texture.unwrap();
            let heap: ID3D12DescriptorHeap = host
                .device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                    NumDescriptors: 1,
                    ..Default::default()
                })
                .unwrap();
            let view = heap.GetCPUDescriptorHandleForHeapStart();
            host.device.CreateRenderTargetView(&texture, None, view);
            let allocator: ID3D12CommandAllocator = host
                .device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                .unwrap();
            let list: ID3D12GraphicsCommandList = host
                .device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                .unwrap();
            draw(&list, view, (1920, 1080), LINES.into_iter());
            list.Close().unwrap();
        }
    }
}
