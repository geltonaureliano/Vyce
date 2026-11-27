//! Console wave64 programs whose waves may be divided another way: run on a
//! host of 32-lane waves with each host wave as a wave of its own ("local"),
//! with nothing exchanged between the two halves of a console wave.
//!
//! What HLSL's wave operations promise holds for any division of the threads
//! into waves, and the idiom the title's compute programs use them for does
//! not care which wave a thread is in: count or sum over the wave, the first
//! lane reserves that many slots with an atomic add, every lane takes the base
//! (`WaveReadLaneFirst`) plus its prefix. Each lane still gets a slot of its
//! own; only the order differs.
//!
//! The check that a program only does that is fxceless's, from
//! github.com/fxceless/victory-fork (`emit::lower::wave_local`, as carried by
//! github.com/DexNom/VIctory), GPL-3.0-or-later.

use std::collections::{HashMap, HashSet};

use crate::gfx10::{Fields, NULL, Operand, Valu};
use crate::ir::{EXEC, Inst, Program, Reg, Value};
use crate::native::{NativeProgram, ResourceType, RootType, UsedResource};
use crate::resource::{Address, CODE, Resources, Role, Site, Source};
use crate::structure::{Atom, LaneExit, Structured};

type Why = &'static str;

/// Whether a whole-wave section leaves a sum over the wave or a prefix.
pub(crate) fn sums(p: &Program) -> bool {
    use super::wave::{Op, Shape};
    (0..p.blocks.len()).any(|b| {
        super::wave::sections(p, b).values().any(|s| {
            s.shapes.values().any(|shape| {
                matches!(
                    shape,
                    Shape::Active(Op::AddU | Op::AddF | Op::AddF16, ..)
                        | Shape::ActiveF64(..)
                        | Shape::Prefix(..)
                )
            })
        })
    })
}

/// What a register value holds, for [`independent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wave {
    /// Nothing that depends on how the threads are divided into waves.
    Plain,
    /// The number of the wave's first lane in a mask (`s_ff1`).
    First,
    /// A reduction over the wave's lanes: a count, a sum, a minimum...
    Total(super::wave::Op),
    /// A count or sum over the lanes below this one.
    Prefix,
    /// Derived from what a first-lane atomic returned: the slots the wave reserved.
    Base,
    /// A mix the idiom does not make.
    Mixed,
}

/// Whether a program's wave-wide values are used only as the allocation idiom
/// uses them, which does not care how the threads are divided into waves. A
/// first-lane number only picks the first lane; a count or sum only goes into
/// that lane's atomic on a shared counter; a prefix only goes onto the base
/// that atomic returned; the first lane does nothing else. Anything else done
/// with them takes one wave to be the whole thread group (a tile's result,
/// say) and is refused, with the offset and what was found.
/// Lane masks read as numbers by other instructions and lane reads outside
/// whole-wave sections are stopped by the lowering itself.
// shortcut: `v_readfirstlane` of a value that differs between lanes is taken as it is (this
// host wave's first lane), and a guarded write of a plain value over a wave-dependent one is
// the only mix of old and new lanes that is looked at.
pub(crate) fn independent(p: &Program, st: &Structured) -> Result<(), (usize, &'static str)> {
    use super::wave::{Op, Shape};
    const FIRST: Why = "a wave's first-lane number used as data";
    const TOTAL: Why = "a count or sum over the wave used as data";
    const PREFIX: Why = "a count or sum over the lanes below used without a reserved base";
    const MIXED: Why = "a value that depends on the wave on some paths only";
    const EFFECT: Why = "a write by the wave's first lane other than its reservation";
    fn of(kind: &HashMap<Value, Wave>, v: Option<Value>) -> Wave {
        v.and_then(|v| kind.get(&v).copied()).unwrap_or(Wave::Plain)
    }
    fn put(kind: &mut HashMap<Value, Wave>, v: Value, k: Wave) {
        if k == Wave::Plain {
            kind.remove(&v);
        } else {
            kind.insert(v, k);
        }
    }
    let wild = |k: Wave| !matches!(k, Wave::Plain | Wave::Base);
    let why = |k: Wave| match k {
        Wave::First => FIRST,
        Wave::Total(_) => TOTAL,
        Wave::Prefix => PREFIX,
        _ => MIXED,
    };
    // a mask (by its low dword) under which only a wave's first lane runs
    let first_mask = |firsts: &HashSet<Value>, m: Option<Value>| {
        m.and_then(|m| st.masks.get(&m)).is_some_and(|&b| {
            st.bools
                .support(b)
                .iter()
                .any(|a| matches!(a, Atom::Lane(v) if firsts.contains(v)))
        })
    };
    // One instruction outside a section: the kind of what it writes, and whether it is a
    // first lane's reservation.
    let step = |inst: &Inst, kind: &HashMap<Value, Wave>, firsts: &mut HashSet<Value>| -> Result<(Wave, bool), Why> {
        let name = inst.name();
        let val = |op: &Operand| match *op {
            Operand::Vector { reg, .. } => inst.read(Reg::V(reg)),
            Operand::Scalar { code, .. } if code != NULL => inst.read(Reg::S(code)),
            _ => None,
        };
        let ops: Vec<Operand> = match &inst.fields {
            Fields::Valu(v) => v.src.clone(),
            Fields::Sop { dst, src, .. } => {
                let mut o = src.clone();
                if matches!(name, "s_addk_i32" | "s_mulk_i32") {
                    o.extend(dst.iter().copied());
                }
                o
            }
            Fields::Buffer { data, .. } if name.contains("_atomic_") || name.contains("_store_") => {
                data.iter().copied().collect()
            }
            _ => Vec::new(),
        };
        let kinds: Vec<Wave> = ops.iter().map(|o| of(kind, val(o))).collect();
        // Everything else it reads (addresses, descriptors, SCC, the high dwords of wide
        // operands) must not depend on the wave. Not EXEC, and not the old value of a VGPR it
        // writes, which only the lanes outside EXEC keep.
        let named: Vec<Value> = ops.iter().filter_map(&val).collect();
        for &(r, v) in &inst.reads {
            let kept = matches!(r, Reg::V(_)) && inst.writes.iter().any(|w| w.0 == r);
            if matches!(r, Reg::S(c) if c == EXEC || c == EXEC + 1) || kept || named.contains(&v) {
                continue;
            }
            let k = of(kind, Some(v));
            if wild(k) {
                return Err(why(k));
            }
        }
        let w = kinds.iter().copied().find(|&k| wild(k)).unwrap_or(Wave::Mixed);
        let sum = |k: Wave| k == Wave::Total(Op::AddU);
        let is_mask = |o: &Operand| {
            val(o).is_some_and(|v| st.masks.contains_key(&v) || st.highs.contains_key(&v))
        };
        let mut reserved = false;
        let out = match (name, &kinds[..]) {
            ("s_ff1_i32_b64" | "s_ff1_i32_b32", _) if ops.first().is_some_and(is_mask) => Wave::First,
            ("s_bcnt1_i32_b64" | "s_bcnt1_i32_b32", _) if ops.first().is_some_and(is_mask) => {
                Wave::Total(Op::AddU)
            }
            // bits of a mask below the lane; with -1 for the mask, the lane's number
            ("v_mbcnt_lo_u32_b32" | "v_mbcnt_hi_u32_b32", [_, b]) => {
                match (matches!(ops[0], Operand::Scalar { .. }), *b) {
                    (_, Wave::Prefix) | (true, Wave::Plain | Wave::Base) => Wave::Prefix,
                    (false, Wave::Plain | Wave::Base) => Wave::Plain,
                    (_, k) => return Err(why(k)),
                }
            }
            // nothing wave-dependent goes in; what derives from a reserved base stays marked
            _ if !kinds.iter().any(|&k| wild(k)) => {
                let derives = matches!(inst.fields, Fields::Valu(_) | Fields::Sop { .. });
                if derives && kinds.contains(&Wave::Base) { Wave::Base } else { Wave::Plain }
            }
            ("v_mov_b32" | "s_mov_b32", [k]) => *k,
            ("v_cmp_eq_i32" | "v_cmp_eq_u32" | "v_cmpx_eq_i32" | "v_cmpx_eq_u32", [a, b]) => {
                // the lane whose own number is the first lane's
                let lane = match (*a, *b) {
                    (Wave::First, k) if !wild(k) => &ops[1],
                    (k, Wave::First) if !wild(k) => &ops[0],
                    _ => return Err(why(w)),
                };
                if !matches!(lane, Operand::Vector { .. }) {
                    return Err(FIRST);
                }
                let mask = match &inst.fields {
                    _ if name.starts_with("v_cmpx") => inst.write(Reg::S(EXEC)),
                    Fields::Valu(v) => match v.dst {
                        Some(Operand::Scalar { code, .. }) => inst.write(Reg::S(code)),
                        _ => None,
                    },
                    _ => None,
                };
                firsts.extend(mask);
                Wave::Plain
            }
            ("v_add_nc_u32" | "s_add_i32", [a, b]) => match (*a, *b) {
                (Wave::Prefix, Wave::Base) | (Wave::Base, Wave::Prefix) => Wave::Base,
                (Wave::Prefix, Wave::Plain | Wave::Prefix) | (Wave::Plain, Wave::Prefix) => Wave::Prefix,
                (x, y) if sum(x) && sum(y) => x,
                _ => return Err(why(w)),
            },
            ("v_sub_nc_u32" | "s_sub_i32", [a, b]) => match (*a, *b) {
                (Wave::Base, Wave::Prefix) => Wave::Base,
                (Wave::Prefix, Wave::Plain) => Wave::Prefix,
                (Wave::Plain, y) if sum(y) && matches!(ops[0], Operand::Const(128)) => y,
                _ => return Err(why(w)),
            },
            ("s_mul_i32" | "v_mul_lo_u32" | "v_mul_u32_u24", [a, b]) => match (*a, *b) {
                (x, Wave::Plain) | (Wave::Plain, x) if sum(x) || x == Wave::Prefix => x,
                _ => return Err(why(w)),
            },
            ("s_lshl_b32", [x, Wave::Plain]) | ("v_lshlrev_b32", [Wave::Plain, x])
                if sum(*x) || *x == Wave::Prefix =>
            {
                *x
            }
            ("v_lshl_add_u32", [Wave::Prefix, Wave::Plain, c]) if !wild(*c) => {
                if *c == Wave::Base { Wave::Base } else { Wave::Prefix }
            }
            // the reservation: the first lane adds its wave's total to a shared counter
            (_, [Wave::Total(op)]) if name.contains("_atomic_") => {
                let want = match op {
                    Op::AddU => "buffer_atomic_add",
                    Op::MinU => "buffer_atomic_umin",
                    Op::MaxU => "buffer_atomic_umax",
                    Op::MinI => "buffer_atomic_smin",
                    Op::MaxI => "buffer_atomic_smax",
                    Op::Or => "buffer_atomic_or",
                    Op::And => "buffer_atomic_and",
                    Op::Xor => "buffer_atomic_xor",
                    _ => "",
                };
                if name != want || !first_mask(firsts, inst.read(Reg::S(EXEC))) {
                    return Err(TOTAL);
                }
                reserved = true;
                Wave::Base
            }
            _ => return Err(why(w)),
        };
        Ok((out, reserved))
    };
    let sections: Vec<_> = (0..p.blocks.len()).map(|b| super::wave::sections(p, b)).collect();
    let order = crate::ir::reverse_postorder(&p.blocks);
    let pass = |kind: &mut HashMap<Value, Wave>, firsts: &mut HashSet<Value>, check: bool| -> Result<(), (usize, Why)> {
        for &b in &order {
            let block = &p.blocks[b];
            for phi in &block.phis {
                let mut k = Wave::Plain;
                for (n, &a) in phi.args.iter().enumerate() {
                    let x = of(kind, Some(a));
                    k = match (k, x) {
                        _ if n == 0 => x,
                        (k, x) if k == x => k,
                        (Wave::Plain, m @ (Wave::Base | Wave::Prefix))
                        | (m @ (Wave::Base | Wave::Prefix), Wave::Plain) => m,
                        _ => Wave::Mixed,
                    };
                }
                put(kind, phi.value, k);
            }
            for (i, inst) in block.insts.iter().enumerate() {
                // inside a whole-wave section: what the simulation found each value to be
                if let Some((_, sec)) = sections[b].iter().find(|(s, sec)| i > **s && i < sec.end) {
                    for &(_, w) in &inst.writes {
                        let k = match sec.shapes.get(&w) {
                            None => Wave::Mixed,
                            Some(&Shape::K(_)) => Wave::Plain,
                            Some(&Shape::Value(v)) => of(kind, Some(v)),
                            Some(&Shape::Masked(src, _, _)) => of(kind, Some(src)),
                            Some(&Shape::Active(op, src, mask, _)) => match of(kind, Some(src)) {
                                Wave::Plain | Wave::Base => Wave::Total(op),
                                // the first lane's own sum of a total is that total
                                Wave::Total(o) if o == op && first_mask(firsts, Some(mask)) => Wave::Total(op),
                                _ => Wave::Mixed,
                            },
                            Some(&Shape::Prefix(Op::AddU, src, mask, _, _)) => match of(kind, Some(src)) {
                                Wave::Plain | Wave::Base => Wave::Prefix,
                                Wave::Total(Op::AddU) if first_mask(firsts, Some(mask)) => Wave::Prefix,
                                _ => Wave::Mixed,
                            },
                            Some(_) => Wave::Mixed,
                        };
                        put(kind, w, k);
                    }
                    continue;
                }
                let name = inst.name();
                if name.starts_with("s_cbranch") {
                    continue; // with the block's exit, below
                }
                let (k, reserved) = match step(inst, kind, firsts) {
                    Ok(r) => r,
                    Err(why) if check => return Err((inst.machine.offset, why)),
                    Err(_) => (Wave::Mixed, false),
                };
                // the same test as `Lower::inst`'s `effect`
                let effect = matches!(inst.fields, Fields::Exp { .. })
                    || matches!(inst.fields, Fields::Flat { .. }) && !name.starts_with("global_load_")
                    || name.contains("store")
                    || name.contains("atomic")
                    || matches!(inst.fields, Fields::Ds { .. })
                        && !(name.starts_with("ds_read")
                            || name.starts_with("ds_swizzle")
                            || name.contains("permute"));
                if check && effect && !reserved && first_mask(firsts, inst.read(Reg::S(EXEC))) {
                    return Err((inst.machine.offset, EFFECT));
                }
                let guarded = st.guards[b][i].is_some();
                for &(r, w) in &inst.writes {
                    let mut kw = match r {
                        // "some lane is counted" keeps the count's kind; any other SCC is one bit of data
                        Reg::Scc if !name.starts_with("s_bcnt1") => {
                            if wild(k) { Wave::Mixed } else { Wave::Plain }
                        }
                        _ => k,
                    };
                    // a guarded write leaves the old value in the lanes outside EXEC
                    if guarded
                        && matches!(r, Reg::V(_))
                        && kw == Wave::Plain
                        && matches!(of(kind, inst.read(r)), Wave::First | Wave::Total(_) | Wave::Prefix)
                    {
                        kw = Wave::Mixed;
                    }
                    put(kind, w, kw);
                }
            }
            // a branch on "some lane is counted": fine where the structure made it the lane's
            // own bit (the mask is EXEC under a scalar condition)
            if check
                && let Some(last) = block.insts.last()
                && last.name().starts_with("s_cbranch_scc")
                && let Some(scc) = last.read(Reg::Scc)
                && wild(of(kind, Some(scc)))
            {
                let own = matches!(st.exits[b], LaneExit::Branch { when, .. }
                    if !st.bools.support(when).iter().any(|a| matches!(a, Atom::Uniform(v) if *v == scc)));
                if !own {
                    return Err((last.machine.offset, TOTAL));
                }
            }
        }
        Ok(())
    };
    let mut kind: HashMap<Value, Wave> = HashMap::new();
    let mut firsts: HashSet<Value> = HashSet::new();
    // to a fixpoint (values carried around loops), then once more to say what is wrong
    let mut settled = false;
    for _ in 0..16 {
        let before = (kind.clone(), firsts.len());
        pass(&mut kind, &mut firsts, false)?;
        if (&before.0, before.1) == (&kind, firsts.len()) {
            settled = true;
            break;
        }
    }
    if !settled {
        return Err((0, MIXED));
    }
    pass(&mut kind, &mut firsts, true)
}

/// Whether a vector ALU instruction reads its destination's old value
/// beside its operands: an accumulator (`v_fmac_f32`, `v_mac_f32`,
/// `v_fmac_legacy_f32`, `v_mac_legacy_f32`, their 16-bit and packed forms,
/// the `v_dot*c` forms), a swap (its operand takes the destination's old
/// value), or a write of part of the destination, which keeps the rest
/// (SDWA, a 16-bit result, a result into the high half by opsel).
fn reads_destination(name: &str, v: &Valu) -> bool {
    v.sdwa.is_some()
        || v.opsel & 8 != 0
        || name.contains("16")
        || name.contains("mac")
        || name.starts_with("v_dot") && name.contains("c_")
        || name.starts_with("v_swap")
}

/// Values the two host waves of a console wave run as local waves may hold
/// apart, though the lanes of each hold them alike: what a lane read
/// (`v_readfirstlane_b32`, `v_readlane_b32`) takes from a value the lanes
/// may hold apart (what the first lane's atomic of the allocation idiom
/// returned among them: each host wave reserves slots of its own), lane
/// masks read as numbers, the hardware's state, the scalar loads in
/// `written` (memory the dispatch may write between the two host waves'
/// loads, [`loads_of_written_memory`]), what is made of those, and what a
/// lane branch's region leaves behind (what is set in it, joins where it
/// ends: one host wave may have run it, or gone round in it more often, and
/// the other not). A uniform branch on one of them may send the two host
/// waves different ways, so it has a region too
/// ([`super::lower::lane_regions`]); what that region leaves behind is
/// found in turn, to a fixpoint. By value (`Value.0`).
///
/// A value is alike in the lanes only if its instruction, unguarded and
/// not reaching across lanes, reads nothing but alike values: its
/// operands, and beside them the old value of a destination it reads or
/// keeps part of ([`reads_destination`]), any other VGPR and a lane mask
/// (`v_div_fmas_f32`'s VCC). Only the old value of a destination written
/// whole is not read: just lanes that never get there keep it. Vector loads
/// are never alike. A lane mask read lane by lane must hold the same bit in
/// every lane: as a select's or a carry's operand, no lane, every lane or a
/// uniform select (every lane's bit an SCC); as that VCC, also a compare or
/// carry of alike values, unless the host waves may hold it apart (set in a
/// region, joined where it ends, made of what is apart).
/// `VICTORY_HOST_WAVE_READS=0`: 1952aaf's rules, which counted beside the
/// operands only a partial write's old value, and among the operands a
/// lane mask only where EXEC or a compare made it.
/// shortcut: a scalar load not in `written` reads the same in both host
/// waves when its address is the same.
pub(crate) fn host_wave_varying(
    p: &Program,
    st: &Structured,
    g: &super::lower::LaneGraph,
    written: &HashSet<(usize, usize)>,
) -> Vec<bool> {
    let reads = super::Switch::HostWaveReads.on();
    let n = p.blocks.len().min(g.succ.len());
    // SSA values are numbered densely: sets of them are bit vectors.
    let values = p
        .inputs
        .iter()
        .map(|i| i.1)
        .chain(p.blocks.iter().flat_map(|b| {
            let phis = b.phis.iter().map(|phi| phi.value);
            phis.chain(b.insts.iter().flat_map(|i| i.writes.iter().map(|w| w.1)))
        }))
        .map(|v| v.0 as usize + 1)
        .max()
        .unwrap_or(0);
    let mut masks = vec![false; values];
    for v in st.masks.keys().chain(st.highs.keys()) {
        if let Some(m) = masks.get_mut(v.0 as usize) {
            *m = true;
        }
    }
    let is_mask = |v: Value| masks.get(v.0 as usize).copied().unwrap_or(false);
    let has = |set: &[bool], v: Value| set.get(v.0 as usize).copied().unwrap_or(false);
    let exec = |r: Reg| matches!(r, Reg::S(c) if c == EXEC || c == EXEC + 1);
    let order = crate::ir::reverse_postorder(&p.blocks);
    // The instruction (block, index) that writes each value; none for the
    // inputs and phis.
    let mut defs: Vec<Option<(usize, usize)>> = vec![None; values];
    for (b, block) in p.blocks.iter().enumerate() {
        for (i, inst) in block.insts.iter().enumerate() {
            for w in &inst.writes {
                if let Some(d) = defs.get_mut(w.1.0 as usize) {
                    *d = Some((b, i));
                }
            }
        }
    }
    let def = |v: Value| defs.get(v.0 as usize).copied().flatten();
    // Whether the lane mask `inst` reads from SGPR `code` on holds the same
    // bit in every lane by how it was made: no lane, every lane, or a
    // uniform select (every lane's bit an SCC), both dwords written by one
    // instruction.
    let uniform_mask = |inst: &Inst, code: u8| {
        let Some(lo) = inst.read(Reg::S(code)) else {
            return false;
        };
        let whole = !p.wave64
            || code
                .checked_add(1)
                .and_then(|c| inst.read(Reg::S(c)))
                .is_some_and(|hi| def(lo).is_some() && def(hi) == def(lo));
        whole
            && st.others.get(&lo).is_some_and(|&m| {
                st.bools.support(m).iter().all(|a| matches!(a, Atom::Uniform(_)))
            })
    };
    // Operand `k` of a vector instruction read lane by lane as a lane mask:
    // a select's or a carry's (`VICTORY_HOST_WAVE_READS=0`: not looked at).
    let lane_mask_at = |inst: &Inst, k: usize| {
        let name = inst.name();
        reads && k == 2 && (name == "v_cndmask_b32" || name.contains("_co_ci_"))
    };
    // A lane mask (or a wave-wide test of one) as operand `k` of a vector
    // instruction: read as a number, the host wave's lanes; read lane by
    // lane, a bit that may differ between the lanes.
    let mask_operand = |inst: &Inst, k: usize, o: &Operand| {
        let lanes = lane_mask_at(inst, k);
        match *o {
            Operand::Scalar { code, dwords } if code != NULL => {
                (0..dwords).any(|d| {
                    code.checked_add(d).is_none_or(|c| {
                        exec(Reg::S(c)) || inst.read(Reg::S(c)).is_some_and(is_mask)
                    })
                }) || lanes && !uniform_mask(inst, code)
            }
            // vccz, execz, lds_direct; read as a mask, a constant of some
            // lanes only
            Operand::Const(c) => matches!(c, 251 | 252 | 254) || lanes && !matches!(c, 128 | 193),
            Operand::Literal(x) => lanes && x != 0 && (p.wave64 || x != u32::MAX),
            _ => false,
        }
    };
    // Values every lane holds alike: written, unguarded, by a lane's own
    // operation that reads nothing but scalars, constants and such values
    // (see above).
    let mut alike = vec![false; values];
    for &b in &order {
        for (i, inst) in p.blocks[b].insts.iter().enumerate() {
            let Fields::Valu(v) = &inst.fields else {
                continue;
            };
            let name = inst.name();
            let lanes = v.dpp.is_some()
                || ["v_mbcnt", "v_permlane", "v_writelane", "v_interp", "v_movrel", "v_swaprel"]
                    .iter()
                    .any(|l| name.starts_with(l));
            let guarded = st.guards.get(b).and_then(|g| g.get(i)).is_some_and(|g| g.is_some());
            if lanes || guarded {
                continue;
            }
            let sources = v.src.iter().enumerate().all(|(k, o)| match *o {
                Operand::Vector { reg, dwords } => (0..dwords).all(|d| {
                    reg.checked_add(d)
                        .and_then(|r| inst.read(Reg::V(r)))
                        .is_some_and(|x| has(&alike, x))
                }),
                _ => !mask_operand(inst, k, o),
            });
            let beside = if reads {
                let within = |x: u8, start: u8, count: u8| {
                    (u16::from(start)..u16::from(start) + u16::from(count)).contains(&u16::from(x))
                };
                // A register an operand names (checked above).
                let named = |r: Reg| {
                    v.src.iter().any(|o| match (*o, r) {
                        (Operand::Vector { reg, dwords }, Reg::V(x)) => within(x, reg, dwords),
                        (Operand::Scalar { code, dwords }, Reg::S(x)) => within(x, code, dwords),
                        _ => false,
                    })
                };
                // A destination VGPR written whole: its old value is not read.
                let whole = |x: u8| {
                    !reads_destination(name, v)
                        && matches!(
                            v.dst,
                            Some(Operand::Vector { reg, dwords }) if within(x, reg, dwords)
                        )
                };
                inst.reads.iter().all(|&(r, x)| match r {
                    _ if named(r) => true,
                    Reg::V(c) => whole(c) || has(&alike, x),
                    // a lane mask (VCC), by the pair it starts
                    Reg::S(c) => {
                        let pair = if p.wave64 { c & !1 } else { c };
                        exec(r) || has(&alike, x) || uniform_mask(inst, pair)
                    }
                    Reg::Scc => true,
                })
            } else {
                // 1952aaf: a partial write keeps the rest of the old value,
                // which must be alike too.
                let partial = v.sdwa.is_some() || v.opsel & 8 != 0 || name.contains("16");
                !partial
                    || inst
                        .reads
                        .iter()
                        .filter(|r| {
                            matches!(r.0, Reg::V(_)) && inst.writes.iter().any(|w| w.0 == r.0)
                        })
                        .all(|r| has(&alike, r.1))
            };
            if sources && beside {
                // Lane masks too (a compare's, a carry's): each lane's bit.
                for w in inst.writes.iter().filter(|w| reads || matches!(w.0, Reg::V(_))) {
                    if let Some(a) = alike.get_mut(w.1.0 as usize) {
                        *a = true;
                    }
                }
            }
        }
    }
    // Whether what `inst` writes may differ between the host waves, with
    // `masks_apart` the lane masks they may hold apart.
    let apart = |inst: &Inst, varying: &[bool], masks_apart: &[bool]| -> bool {
        let name = inst.name();
        // the hardware's state, each wave its own
        if matches!(
            name,
            "s_getreg_b32" | "s_memtime" | "s_memrealtime" | "s_get_waveid_in_workgroup"
        ) || name.starts_with("s_sendmsg") && name.ends_with("_rtn")
        {
            return true;
        }
        match &inst.fields {
            // a lane read of a value the lanes may hold apart: each host
            // wave reads a lane of its own
            Fields::Valu(v) if matches!(name, "v_readfirstlane_b32" | "v_readlane_b32") => {
                let from = match v.src.first() {
                    Some(&Operand::Vector { reg, .. }) => inst.read(Reg::V(reg)),
                    _ => None,
                };
                if !from.is_some_and(|x| has(&alike, x)) {
                    return true;
                }
            }
            // a lane mask read as a number: the lanes of one host wave
            Fields::Sop { .. } if inst.reads.iter().any(|&(r, x)| exec(r) || is_mask(x)) => {
                return true;
            }
            Fields::Valu(v) if v.src.iter().enumerate().any(|(k, o)| mask_operand(inst, k, o)) => {
                return true;
            }
            // a lane mask read lane by lane beside the operands
            // (`v_div_fmas_f32`'s VCC) that the host waves may hold apart
            Fields::Valu(_) if inst.reads.iter().any(|&(r, x)| !exec(r) && has(masks_apart, x)) => {
                return true;
            }
            _ => {}
        }
        inst.reads.iter().any(|&(r, x)| !exec(r) && has(varying, x))
    };
    let mut varying = vec![false; values];
    // Lane masks the host waves may hold apart, though alike in the lanes
    // of each: set in a region, joined where it ends, or made of what is
    // apart (`VICTORY_HOST_WAVE_READS=0`: none).
    let mut masks_apart = vec![false; values];
    let mut count = 0;
    loop {
        let before = count;
        // The regions of lane branches and of branches on what is apart so
        // far, and the blocks their lanes leave them to.
        let mut inside = vec![false; n];
        let mut joined = vec![false; n];
        for (d, r) in super::lower::lane_regions(st, g, &|v| has(&varying, v), true) {
            for b in (0..n).filter(|&b| r[b] || b == d) {
                inside[b] |= r[b];
                for &t in g.succ[b].iter().filter(|&&t| t < n) {
                    joined[t] |= !r[t];
                }
            }
        }
        let mut put = |v: Value, set: &mut [bool]| match set.get_mut(v.0 as usize) {
            Some(x) if !*x => {
                *x = true;
                count += 1;
                true
            }
            _ => false,
        };
        let mut changed = true;
        while changed {
            changed = false;
            for &b in order.iter().filter(|&&b| b < n) {
                let block = &p.blocks[b];
                for phi in &block.phis {
                    if exec(phi.reg) {
                        continue;
                    }
                    let edge = inside[b] || joined[b];
                    if !is_mask(phi.value) {
                        if edge || phi.args.iter().any(|&a| has(&varying, a)) {
                            changed |= put(phi.value, &mut varying);
                        }
                    } else if reads && (edge || phi.args.iter().any(|&a| has(&masks_apart, a))) {
                        changed |= put(phi.value, &mut masks_apart);
                    }
                }
                for (i, inst) in block.insts.iter().enumerate() {
                    let loaded = written.contains(&(b, i));
                    if inside[b] || loaded || apart(inst, &varying, &masks_apart) {
                        for &(r, w) in &inst.writes {
                            if exec(r) {
                                continue;
                            }
                            if !is_mask(w) {
                                changed |= put(w, &mut varying);
                            } else if reads {
                                changed |= put(w, &mut masks_apart);
                            }
                        }
                    }
                }
            }
        }
        if count == before {
            return varying;
        }
    }
}

/// Scalar loads (by block and instruction) of memory the dispatch itself
/// may write while it runs, which the two host waves of a console wave run
/// as local waves may read apart ([`host_wave_varying`]): on the console
/// the wave loads once, here each host wave loads on its own, and another
/// group or the other host wave can land a write between the two. Every
/// load with `glc` or `dlc` (asking past the scalar cache for what was
/// written since) and every one other than `s_load_*` and `s_buffer_load_*`;
/// in a program that stores to memory or adds to it (beside LDS and the
/// lanes' own scratch), every one of a view the program's resources do not
/// name a constant buffer or shader resource view
/// ([`super::lower::used_entry`]): an unordered-access view, a descriptor
/// of the heap or one the code makes, a root view of no given kind; not the
/// descriptors of a table or a heap, nor the constants after the code.
/// `VICTORY_HOST_WAVE_READS=0`: none.
/// shortcut: a constant buffer or shader resource view is not written while
/// a dispatch reads it, through another view either (D3D12 keeps a resource
/// in a read state apart from unordered access).
pub(crate) fn loads_of_written_memory(
    native: &NativeProgram,
    p: &Program,
    res: &Resources,
) -> HashSet<(usize, usize)> {
    let mut out = HashSet::new();
    if !super::Switch::HostWaveReads.on() {
        return out;
    }
    let sites = || {
        p.blocks.iter().enumerate().flat_map(|(b, block)| {
            block.insts.iter().enumerate().map(move |(i, inst)| ((b, i), inst))
        })
    };
    // Stores and atomics on memory: not on LDS (or GDS), not on the lanes'
    // own scratch.
    let writes = sites().any(|(_, inst)| {
        let name = inst.name();
        !matches!(inst.fields, Fields::Ds { .. } | Fields::Exp { .. })
            && !name.starts_with("scratch_")
            && (name.contains("_store") || name.contains("_atomic"))
    });
    let scalar: HashMap<Site, Address> = res.scalar.iter().copied().collect();
    // A view the program's resources name a constant buffer or a shader
    // resource view.
    let read_only = |u: Option<&UsedResource>| {
        u.is_some_and(|u| matches!(u.kind, ResourceType::Cbv | ResourceType::Srv))
    };
    for (site, inst) in sites() {
        let Fields::Smem { cache, .. } = &inst.fields else {
            continue;
        };
        let name = inst.name();
        if !name.contains("_load_") {
            continue; // stores, cache controls, the hardware's state
        }
        let plain = name.starts_with("s_load_") || name.starts_with("s_buffer_load_");
        let unwritten = if name.starts_with("s_buffer_load_") {
            // Through a descriptor: every source it may come from.
            let leaves: Vec<&Source> = res
                .descriptors
                .iter()
                .filter(|d| d.site == site && d.role == Role::Resource)
                .flat_map(|d| d.source.leaves())
                .collect();
            !leaves.is_empty()
                && leaves.iter().all(|s| match s {
                    Source::Built { address, .. } if address.param == CODE => true,
                    s => read_only(super::lower::used_entry(native, &scalar, s)),
                })
        } else {
            // By address: a table's or a heap's descriptors, the code's
            // constants, a root view.
            scalar.get(&site).is_some_and(|a| {
                let root = native.layout.get(a.param).map(|rp| rp.root);
                a.param == CODE
                    || matches!(
                        root,
                        Some(RootType::Table | RootType::ResourceHeap | RootType::SamplerHeap)
                    )
                    || root == Some(RootType::Descriptor)
                        && read_only(super::lower::root_view_entry(native, a.param))
            })
        };
        if cache.glc || cache.dlc || !plain || writes && !unwritten {
            out.insert(site);
        }
    }
    out
}
