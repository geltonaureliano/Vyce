//! Paired lanes ([`super::Parts::paired`], `emit::paired`): a console wave64
//! compute program on a host of 32-lane waves, each console wave run by one
//! host wave, host lane n running console lanes n ("lower") and n + 32
//! ("upper"). The program is lowered as the console runs it (the console
//! flow: every lane through every block, the branches taken wave-wide, EXEC
//! guarding each vector instruction), so control flow is the same in every
//! thread of a host wave, which goes where some lane of its console wave
//! goes; the waves of a group may part, and meet at the program's own
//! barriers as the console's waves do.
//!
//! What a lane runs is lowered once for each lane of the thread, the lower
//! one first, each time with that lane's state swapped in ([`LaneState`]):
//! its values, its EXEC and mask bits and their joins, its console thread.
//! That keeps the console's order of the wave's instructions: all lanes of
//! one before any lane of the next. Scalar code (SALU, SOPP, scalar loads)
//! runs once, as the console's wave runs it, and its results go to both
//! lanes. What reads the whole wave reads both lanes of every thread of the
//! host wave: a ballot is the lower lanes' and the upper lanes' host ballots
//! (the console mask's two dwords), a lane read picks the lane's host thread
//! and half, a whole-wave section reduces both halves and joins them.
//!
//! Version 0 refuses lane operations outside whole-wave sections other than
//! lane reads, ballots, lane counts and lane bits (DPP, `v_permlane*`,
//! `v_writelane`, `ds_swizzle`, `ds_(b)permute`, derivatives from quads),
//! and float sums over the wave (the console adds in its own order).
//!
//! The lanes of one console wave share LDS without barriers on the console;
//! here they are the two lanes of the threads of one host wave. Accesses one
//! lane makes are in the thread's program order; between threads of the
//! wave a group-shared memory fence before each LDS access that follows
//! another (unless both read) keeps the host compiler from moving one past
//! the other. shortcut: D3D12 orders no group-shared accesses of two threads
//! without a group barrier; the host is taken to run a wave's threads in
//! step where they all go the same way, as for the console's 32-lane
//! programs that share LDS within a wave. A group of one console wave is one
//! host wave: there the fence is a group barrier.
//!
//! After fxceless/victory-fork#854 (PiloteCode, "Paired Wave64 compute
//! lowering on 32-lane GPUs"; victory-3060TI 26600c6 and p1nkman-VI
//! `LaneState`, `swap_lanes`, `both`, `put_lanes`, `paired_cross`,
//! `paired_shape`; GPL-3.0-or-later). Vyce's console flow already takes
//! every branch wave-wide, which the fork's `structure_wide` does there.

use std::collections::HashMap;

use super::super::wave::{Op, Shape};
use super::{Attr, Bin, Lower, MaskData, PendingPath, Rmw, Stop, V, Val, Why, flags, pred};
use crate::gfx10::{Fields, Operand};
use crate::ir::{EXEC, Inst, Reg, Value};
use crate::structure::Bool;

/// What the lowering keeps per console lane, for the thread's other lane
/// ([`Lower::swap_lanes`]): the values (scalar ones too, the same in both),
/// float views of them, the lane's mask bits and booleans and their joins,
/// the phis still to fill, the console thread, the per-lane descriptor
/// joins.
#[derive(Default)]
pub(super) struct LaneState {
    vals: HashMap<Value, Val>,
    floats: HashMap<Value, V>,
    bools: HashMap<(usize, Bool), V>,
    lane_bits: HashMap<Value, V>,
    lane_fail: HashMap<Value, Why>,
    mask_phis: HashMap<Value, V>,
    pending_masks: Vec<(V, usize, usize)>,
    data_phis: Vec<(V, usize, usize)>,
    console_flat: Option<V>,
    lane_paths: Vec<(usize, V)>,
    pending_paths: Vec<PendingPath>,
}

impl LaneState {
    /// Joins still to fill.
    fn pending(&self) -> bool {
        !self.pending_masks.is_empty()
            || !self.data_phis.is_empty()
            || !self.pending_paths.is_empty()
    }
}

/// Why paired lanes do not lower `inst`, a lane operation version 0 leaves
/// out (outside whole-wave sections, which are simulated).
fn refusal(inst: &Inst) -> Option<Why> {
    let name = inst.name();
    match &inst.fields {
        Fields::Valu(v) if v.dpp.is_some() => {
            Some("DPP outside a whole-wave section in paired lanes")
        }
        Fields::Valu(_) if name.starts_with("v_permlane") => {
            Some("a lane permutation outside a whole-wave section in paired lanes")
        }
        Fields::Valu(_) if name == "v_writelane_b32" => {
            Some("a lane write outside a whole-wave section in paired lanes")
        }
        Fields::Ds { .. } if name.starts_with("ds_swizzle") || name.contains("permute") => {
            Some("a lane swizzle or permutation in paired lanes")
        }
        Fields::Image { .. } if super::implicit_derivatives(name) => {
            Some("a sample with derivatives from lanes in paired lanes")
        }
        _ => None,
    }
}

impl Lower<'_> {
    /// Paired lanes: makes the thread's other console lane the one being
    /// lowered.
    pub(super) fn swap_lanes(&mut self) {
        let Some(t) = self.twin.as_deref_mut() else {
            return;
        };
        std::mem::swap(&mut self.vals, &mut t.vals);
        std::mem::swap(&mut self.floats, &mut t.floats);
        std::mem::swap(&mut self.bools, &mut t.bools);
        std::mem::swap(&mut self.lane_bits, &mut t.lane_bits);
        std::mem::swap(&mut self.lane_fail, &mut t.lane_fail);
        std::mem::swap(&mut self.mask_phis, &mut t.mask_phis);
        std::mem::swap(&mut self.pending_masks, &mut t.pending_masks);
        std::mem::swap(&mut self.data_phis, &mut t.data_phis);
        std::mem::swap(&mut self.console_flat, &mut t.console_flat);
        std::mem::swap(&mut self.lane_paths, &mut t.lane_paths);
        std::mem::swap(&mut self.pending_paths, &mut t.pending_paths);
        self.upper = !self.upper;
    }

    /// Paired lanes: `f` for the thread's lower lane, then its upper one;
    /// the lane being lowered stays the same.
    fn both<T>(&mut self, mut f: impl FnMut(&mut Self) -> Result<T, Why>) -> Result<[T; 2], Why> {
        let was = self.upper;
        if was {
            self.swap_lanes();
        }
        let lo = f(self);
        self.swap_lanes();
        let hi = f(self);
        if !was {
            self.swap_lanes();
        }
        Ok([lo?, hi?])
    }

    /// Paired lanes: `lo` as the lower lane's value of `v`, `hi` as the
    /// upper lane's.
    fn put_lanes(&mut self, v: Value, lo: Val, hi: Val) {
        let (mine, other) = if self.upper { (hi, lo) } else { (lo, hi) };
        self.vals.insert(v, mine);
        if let Some(t) = self.twin.as_deref_mut() {
            t.vals.insert(v, other);
        }
    }

    /// Paired lanes: what `inst` wrote in the lane being lowered, for the
    /// other lane too (a scalar result, the same in both).
    fn mirror(&mut self, inst: &Inst) {
        let Some(t) = self.twin.as_deref_mut() else {
            return;
        };
        for &(_, w) in &inst.writes {
            if let Some(&v) = self.vals.get(&w) {
                t.vals.insert(w, v);
            }
            if let Some(&f) = self.floats.get(&w) {
                t.floats.insert(w, f);
            }
        }
    }

    /// Paired lanes, in the entry block: the console thread of each lane.
    /// The host wave takes the next console wave of the group from a count
    /// in group-shared memory (one lane adds 32 once every thread has
    /// cleared it: one group barrier), whatever threads the host puts in
    /// it; its lane n is console lane n of that wave, and lane n + 32.
    pub(super) fn paired_entry(&mut self) {
        let e = self.s.entry;
        let i32t = self.s.t.i32;
        let arr = self.s.m.array(1, i32t);
        let count = self.s.m.global("waves", arr, 3);
        let (z, k32) = (self.s.m.i32(0), self.s.m.i32(32));
        let at = self.s.m.element(e, count, z);
        self.s.m.store(e, at, z);
        let (void, mode) = (self.s.t.void, self.s.m.i32(1 | 8));
        self.s.op(e, "barrier", Attr::NoDuplicate, void, 80, &[mode]);
        let lane = self.s.op(e, "waveGetLaneIndex", Attr::ReadOnly, i32t, 111, &[]);
        let leads = self.s.m.cmp(e, pred::EQ, lane, z);
        let add = self.s.m.select(e, leads, k32, z);
        let old = self.s.m.atomic(e, Rmw::Add, at, add);
        // 32 host threads a console wave, 64 console threads.
        let base = self.s.op(e, "waveReadLaneFirst.i32", Attr::NoUnwind, i32t, 118, &[old]);
        let one = self.s.m.i32(1);
        let first = self.s.m.bin(e, Bin::Shl, base, one);
        let lo = self.s.m.bin(e, Bin::Add, first, lane);
        let hi = self.s.m.bin(e, Bin::Add, lo, k32);
        self.s.flags |= flags::WAVE_OPS;
        let (mine, other) = if self.upper { (hi, lo) } else { (lo, hi) };
        self.console_flat = Some(mine);
        if let Some(t) = self.twin.as_deref_mut() {
            t.console_flat = Some(other);
        }
    }

    /// Instruction `i` of IR block `b` in paired lanes: for the lower lane,
    /// then for the upper one; scalar code once for both; a read of another
    /// lane once, with what both lanes hold ([`Self::paired_cross`]).
    pub(super) fn paired_inst(&mut self, b: usize, i: usize) -> Result<(), Stop> {
        let p = self.x.p;
        let inst = &p.blocks[b].insts[i];
        let name = inst.name();
        if self.dead.contains(&(b, i))
            || matches!(inst.fields, Fields::Sop { .. } | Fields::Sopp { .. } | Fields::Smem { .. })
        {
            self.inst(b, i)?;
            self.mirror(inst);
            return Ok(());
        }
        if let Some(why) = refusal(inst) {
            if std::env::var_os("VICTORY_WHY").is_some() {
                eprintln!("missing {:#x} {name} {why}", inst.machine.offset);
            }
            for &(_, w) in &inst.writes {
                self.put_lanes(w, Val::Missing(why), Val::Missing(why));
            }
            return Ok(());
        }
        let Fields::Valu(v) = &inst.fields else {
            return self.paired_lanes(inst, b, i);
        };
        let cross = match name {
            // (not a value a whole-wave section made from that lane's)
            "v_readlane_b32" => !matches!(v.src.first(), Some(&Operand::Vector { reg, .. })
                if inst.read(Reg::V(reg)).is_some_and(|r| self.one_lane.contains(&r))),
            // (not a waterfall loop's read, the lane's own dword)
            "v_readfirstlane_b32" => {
                !inst.writes.first().is_some_and(|w| self.x.res.waterfall.reads.contains(&w.1))
            }
            _ => false,
        };
        if !cross {
            return self.paired_lanes(inst, b, i);
        }
        if let Err(why) = self.paired_cross(inst) {
            if std::env::var_os("VICTORY_WHY").is_some() {
                eprintln!("missing {:#x} {name} {why}", inst.machine.offset);
            }
            for &(_, w) in &inst.writes {
                self.put_lanes(w, Val::Missing(why), Val::Missing(why));
            }
        }
        Ok(())
    }

    /// [`Self::inst`] for each lane of the thread, the lower one first; an
    /// LDS access after another gets a fence first unless both read.
    fn paired_lanes(&mut self, inst: &Inst, b: usize, i: usize) -> Result<(), Stop> {
        if super::lds_access(inst) {
            let write = !inst.name().starts_with("ds_read");
            if write || self.lds_last != Some(false) {
                // A group of one console wave is one host wave: it meets.
                let mode = if self.group_threads() <= 32 { 1 | 8 } else { 8 };
                let (mode, void) = (self.s.m.i32(mode), self.s.t.void);
                self.s.op(self.cur, "barrier", Attr::NoDuplicate, void, 80, &[mode]);
            }
            self.lds_last = Some(write);
        }
        self.inst(b, i)?;
        self.swap_lanes();
        let r = self.inst(b, i);
        self.swap_lanes();
        r
    }

    /// Paired lanes: `v_readlane_b32` and `v_readfirstlane_b32`, which read
    /// a lane of the console wave whatever EXEC holds: the lane's host
    /// thread and half (console lane n is host lane n & 31, its upper lane
    /// from 32). The result is scalar, the same in both lanes.
    fn paired_cross(&mut self, inst: &Inst) -> Result<(), Why> {
        self.mask_ok = self.mask_data == MaskData::Console;
        self.cross_lane(false)?;
        let c = self.cur;
        let lane = if inst.name() == "v_readlane_b32" {
            self.src(inst, 1)?
        } else {
            // The console's first lane in EXEC (lane 0 when none), from
            // EXEC's two dwords (both lanes' ballots).
            let word = |l: &mut Self, r: u8| -> Result<V, Why> {
                match l.get(inst.read(Reg::S(r)).ok_or("EXEC")?)? {
                    Val::I32(x) => Ok(x),
                    _ => Err(super::LANE_MASK),
                }
            };
            let lo = word(self, EXEC)?;
            let hi = word(self, EXEC + 1)?;
            let i32t = self.s.t.i32;
            let flo = self.s.op(c, "unaryBits.i32", Attr::ReadNone, i32t, 32, &[lo]);
            let fhi = self.s.op(c, "unaryBits.i32", Attr::ReadNone, i32t, 32, &[hi]);
            let (z, k32) = (self.s.m.i32(0), self.s.m.i32(32));
            let fhi = self.s.m.bin(c, Bin::Add, fhi, k32);
            let none_hi = self.s.m.cmp(c, pred::EQ, hi, z);
            let fhi = self.s.m.select(c, none_hi, z, fhi);
            let some_lo = self.s.m.cmp(c, pred::NE, lo, z);
            self.s.m.select(c, some_lo, flo, fhi)
        };
        let [lo, hi] = self.both(|l| l.src(inst, 0))?;
        let (k31, k32, z) = (self.s.m.i32(31), self.s.m.i32(32), self.s.m.i32(0));
        let host = self.s.m.bin(c, Bin::And, lane, k31);
        let half = self.s.m.bin(c, Bin::And, lane, k32);
        let upper = self.s.m.cmp(c, pred::NE, half, z);
        let (lo, hi) = (self.read_lane(lo, host), self.read_lane(hi, host));
        let r = self.s.m.select(c, upper, hi, lo);
        self.write_i(inst, r, None)?;
        self.mirror(inst);
        Ok(())
    }

    /// Paired lanes: the console wave's ballot of mask `m` at DXIL block
    /// `at`: the lower lanes' host ballot and the upper lanes'.
    pub(super) fn paired_ballot(&mut self, at: usize, m: Bool) -> Result<(V, V), Why> {
        let [lo, hi] = self.both(|l| l.boolean(m, at))?;
        Ok((self.ballot_word(at, lo), self.ballot_word(at, hi)))
    }

    /// Paired lanes: value `w` a whole-wave section leaves, by its `shape`
    /// ([`Lower::section`]); `plain`: lowered as outside a section already.
    pub(super) fn paired_section_value(
        &mut self,
        w: Value,
        shape: Option<Shape>,
        plain: bool,
        unknown: Why,
    ) {
        match (shape.map(|sh| self.paired_shape(sh)), plain) {
            (Some(Ok([lo, hi])), _) => self.put_lanes(w, Val::I32(lo), Val::I32(hi)),
            (_, true) => {}
            (Some(Err(why)), false) => self.put_lanes(w, Val::Missing(why), Val::Missing(why)),
            (None, false) => self.put_lanes(w, Val::Missing(unknown), Val::Missing(unknown)),
        }
    }

    /// [`Lower::shape`] in paired lanes, for the lower and the upper lane:
    /// a reduction over the console wave is the host wave's over the lower
    /// lanes and over the upper ones, put together; a prefix sum the host
    /// wave's over the lower lanes, and over the upper ones after the lower
    /// lanes' total.
    fn paired_shape(&mut self, shape: Shape) -> Result<[V; 2], Why> {
        let c = self.cur;
        let masked = |l: &mut Self, src: Value, mask: Value, ident: u32| -> Result<[V; 2], Why> {
            let &m = l.x.st.masks.get(&mask).ok_or("a wave reduction's untracked mask")?;
            l.both(|l| {
                let on = l.boolean(m, c)?;
                let x = l.i32_of(src)?;
                let id = l.s.m.i32(ident);
                Ok(l.s.m.select(c, on, x, id))
            })
        };
        let (op, src, mask, ident, prefix) = match shape {
            Shape::K(k) => {
                let x = self.s.m.i32(k);
                return Ok([x, x]);
            }
            Shape::Value(v) => return self.both(|l| l.i32_of(v)),
            Shape::Masked(src, mask, ident) => return masked(self, src, mask, ident),
            Shape::ActiveF64(..) => return Err("a 64-bit float sum over the wave in paired lanes"),
            Shape::Active(op, src, mask, ident) => (op, src, mask, ident, None),
            Shape::Prefix(op, src, mask, ident, inclusive) => {
                (op, src, mask, ident, Some(inclusive))
            }
        };
        if matches!(op, Op::AddF | Op::AddF16) {
            // (the console adds the lanes in its own order)
            return Err("a float sum over the wave in paired lanes");
        }
        let [lo, hi] = masked(self, src, mask, ident)?;
        self.s.flags |= flags::WAVE_OPS;
        if let Some(inclusive) = prefix {
            if op != Op::AddU {
                return Err("a wave prefix of an operation other than a sum");
            }
            let (i32t, k0, k1) = (self.s.t.i32, self.s.m.i8(0), self.s.m.i8(1));
            let mut sums = [lo, hi];
            for x in &mut sums {
                let r = self.s.op(c, "wavePrefixOp.i32", Attr::NoUnwind, i32t, 121, &[*x, k0, k1]);
                *x = if inclusive { self.s.m.bin(c, Bin::Add, r, *x) } else { r };
            }
            let total = self.wave_reduction(op, lo);
            let upper = self.s.m.bin(c, Bin::Add, total, sums[1]);
            return Ok([sums[0], upper]);
        }
        let float = matches!(op, Op::MinF | Op::MaxF);
        let (lo, hi) = if float {
            (self.as_float(lo), self.as_float(hi))
        } else {
            (lo, hi)
        };
        let (rl, rh) = (self.wave_reduction(op, lo), self.wave_reduction(op, hi));
        let r = self.reduce_pair(op, rl, rh);
        let r = if float { self.as_int(r) } else { r };
        Ok([r, r])
    }

    /// Paired lanes: [`Self::fill_phis`] for the upper lane, and again for
    /// either while filling one leaves joins in the other (a ballot made
    /// while filling reads both lanes' masks).
    pub(super) fn paired_fill_phis(&mut self) -> Result<(), Stop> {
        loop {
            self.swap_lanes();
            let r = self.fill_phis();
            self.swap_lanes();
            r?;
            let mine = !self.pending_masks.is_empty()
                || !self.data_phis.is_empty()
                || !self.pending_paths.is_empty();
            let other = self.twin.as_deref().is_some_and(LaneState::pending);
            if !mine && !other {
                return Ok(());
            }
            self.fill_phis()?;
        }
    }
}
