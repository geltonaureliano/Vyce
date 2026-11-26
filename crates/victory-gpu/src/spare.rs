//! Buffers of indirect execution no list uses: clamped arguments and budgets
//! in video memory ([`crate::indirect`]). Lists give theirs back once they
//! have run and take them again when they need some. Each list keeping those
//! of its largest use added up to 1,964 buffers and 204 MiB (session 7824),
//! as the title has hundreds of lists of which a frame runs some.
//!
//! `VICTORY_INDIRECT_POOL=0` lets each list keep its own, as before.

use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

/// Spare bytes kept; more are released.
const KEPT: u64 = 32 << 20;
/// What a buffer takes at least, however small.
const LEAST: u64 = 64 << 10;

/// A buffer and its size.
pub(crate) type Buffer = (ID3D12Resource, u64);

/// What a list had of them.
#[derive(Default)]
pub(crate) struct Given {
    /// Chunks for clamped arguments, all of one size.
    pub(crate) chunks: Vec<Buffer>,
    /// Buffers for the clamped arguments of one execution each.
    pub(crate) arguments: Vec<Buffer>,
    pub(crate) budgets: Vec<Buffer>,
}

impl Given {
    const NONE: Given = Given { chunks: Vec::new(), arguments: Vec::new(), budgets: Vec::new() };

    fn bytes(&self) -> u64 {
        let all = [&self.chunks, &self.arguments, &self.budgets];
        all.iter().flat_map(|b| b.iter()).map(|b| b.1.max(LEAST)).sum()
    }

    /// Takes buffers out until those left fit `most` bytes.
    fn cut(&mut self, most: u64) -> Vec<Buffer> {
        let mut over = Vec::new();
        let mut bytes = self.bytes();
        while bytes > most {
            // The largest go first.
            let Some(b) = self.arguments.pop().or_else(|| self.chunks.pop()).or_else(|| self.budgets.pop())
            else {
                break;
            };
            bytes -= b.1.max(LEAST);
            over.push(b);
        }
        over
    }
}

/// The spare ones; `arguments` from the smallest to the largest.
static SPARE: Mutex<Given> = Mutex::new(Given::NONE);

pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_INDIRECT_POOL").is_none_or(|v| v != "0"))
}

fn spare() -> std::sync::MutexGuard<'static, Given> {
    SPARE.lock().unwrap_or_else(|e| e.into_inner())
}

/// A spare chunk, in the common state (buffers return to it when a list that
/// used them has run).
pub(crate) fn chunk() -> Option<Buffer> {
    spare().chunks.pop()
}

/// A spare budget, as [`chunk`].
pub(crate) fn budget() -> Option<Buffer> {
    spare().budgets.pop()
}

/// The smallest spare buffer of `bytes` or more for one execution's
/// arguments, as [`chunk`].
pub(crate) fn arguments(bytes: u64) -> Option<Buffer> {
    let mut spare = spare();
    let at = spare.arguments.partition_point(|b| b.1 < bytes);
    (at < spare.arguments.len()).then(|| spare.arguments.remove(at))
}

/// Gives back what a list had: its last submission has finished.
pub(crate) fn give(mut given: Given) {
    let over = {
        let mut spare = spare();
        spare.chunks.append(&mut given.chunks);
        spare.budgets.append(&mut given.budgets);
        for b in given.arguments {
            let at = spare.arguments.partition_point(|s| s.1 < b.1);
            spare.arguments.insert(at, b);
        }
        spare.cut(KEPT)
    };
    if !over.is_empty() {
        crate::host::retire(over);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A buffer of the size asked or the next larger one is given, and what
    /// is kept stays within bounds, largest first to go.
    #[test]
    fn spare_buffers_fit_and_are_bounded() {
        let Some(host) = crate::host::host() else {
            return; // no host GPU
        };
        let make = |size: u64| (host.uav_buffer(size).unwrap(), size);
        let mut given = Given::NONE;
        for size in [128 << 10, 64 << 10, 256 << 10] {
            let b = make(size);
            let at = given.arguments.partition_point(|s| s.1 < b.1);
            given.arguments.insert(at, b);
        }
        given.budgets.push(make(256));
        given.chunks.push(make(256 << 10));
        assert_eq!(given.bytes(), (128 + 64 + 256 + 64 + 256) << 10);
        let sizes = |g: &Given| g.arguments.iter().map(|b| b.1 >> 10).collect::<Vec<_>>();
        assert_eq!(sizes(&given), [64, 128, 256]);
        let over = given.cut(300 << 10);
        assert_eq!(over.iter().map(|b| b.1 >> 10).collect::<Vec<_>>(), [256, 128, 64, 256]);
        assert_eq!(given.bytes(), 64 << 10, "the budget is left");
    }
}
