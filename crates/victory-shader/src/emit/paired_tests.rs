//! Paired lanes (`emit::paired`) on the host's cards: console wave64 compute
//! programs whose waves branch apart, each console wave run by one host wave
//! of 32 lanes with two console lanes a thread. Hand-assembled GFX10 code
//! ([`Asm`], checked against the crate's decoder) in the patterns of the four
//! programs only this form translates: waves that walk loops of their own
//! with a lane handing an LDS value to the next round without a barrier, a
//! per-wave "any" test around a whole-wave minimum and maximum with the
//! first lane's LDS slot, a counting sort with prefix sums and lane 63's
//! wave totals, and a per-wave "any and any" count over two rounds appended
//! to a list. Each is checked against a CPU model of what the console's
//! waves compute; the console flow's one-wave cases run in paired lanes too.

use std::collections::HashMap;

use super::host::{Dispatch, Host, Root, View, adapters};
use super::tests::{compute, emit_for};
use super::{HostShader, Switch, Target, with_switch};
use crate::gfx10::Encoding;
use crate::native::{ComputeIo, NativeProgram, RootParam, RootType, StageIo};

/// An operand of [`Asm`]: an SGPR, VCC, EXEC, null, an inline integer, a
/// literal or a VGPR.
#[derive(Clone, Copy)]
enum Src {
    S(u8),
    Vcc,
    Exec,
    I(i32),
    L(u32),
    V(u8),
}

use Src::{I, L, S, V};

impl Src {
    /// The 9-bit source code and the literal dword it needs.
    fn code(self) -> (u32, Option<u32>) {
        match self {
            S(n) => (u32::from(n), None),
            Src::Vcc => (106, None),
            Src::Exec => (126, None),
            I(k @ 0..=64) => (128 + k as u32, None),
            I(k @ -16..=-1) => (192 + (-k) as u32, None),
            I(k) => (255, Some(k as u32)),
            L(x) => (255, Some(x)),
            V(n) => (256 + u32::from(n), None),
        }
    }
}

const VCC: u8 = 106;
const EXEC: u8 = 126;
const INF: u32 = 0x7F80_0000;
const NEG_INF: u32 = 0xFF80_0000;

/// GFX10 code built instruction by instruction, with labels for branches.
/// [`Asm::finish`] decodes it again and checks every instruction's name.
#[derive(Default)]
struct Asm {
    w: Vec<u32>,
    names: Vec<(usize, &'static str)>,
    labels: HashMap<&'static str, usize>,
    jumps: Vec<(usize, &'static str)>,
}

/// The opcode of `name` in `enc` (VOP3 also takes the VOPC, VOP2 and VOP1
/// opcodes at their bases).
fn opcode(enc: Encoding, name: &str) -> u32 {
    let find = |e: Encoding| {
        crate::gfx10::test_asm::opcodes()
            .find(|&(x, _, n)| x == e && n == name)
            .map(|(_, op, _)| u32::from(op))
    };
    let op = match enc {
        Encoding::Vop3 => find(Encoding::Vopc)
            .or_else(|| find(Encoding::Vop2).map(|op| op + 0x100))
            .or_else(|| find(Encoding::Vop1).map(|op| op + 0x180))
            .or_else(|| find(Encoding::Vop3)),
        e => find(e),
    };
    op.unwrap_or_else(|| panic!("no {enc:?} opcode {name}"))
}

impl Asm {
    fn put(&mut self, name: &'static str, words: &[u32], lit: Option<u32>) {
        self.names.push((self.w.len(), name));
        self.w.extend_from_slice(words);
        self.w.extend(lit);
    }

    /// Words written by hand (not checked by name).
    fn raw(&mut self, words: &[u32]) {
        self.w.extend_from_slice(words);
    }

    fn lit(a: Option<u32>, b: Option<u32>) -> Option<u32> {
        assert!(a.is_none() || b.is_none() || a == b, "two literals");
        a.or(b)
    }

    fn sop2(&mut self, name: &'static str, d: u8, a: Src, b: Src) {
        let ((x, la), (y, lb)) = (a.code(), b.code());
        let op = opcode(Encoding::Sop2, name);
        let w = 0x8000_0000 | op << 23 | u32::from(d) << 16 | y << 8 | x;
        self.put(name, &[w], Self::lit(la, lb));
    }

    fn sop1(&mut self, name: &'static str, d: u8, a: Src) {
        let (x, la) = a.code();
        let op = opcode(Encoding::Sop1, name);
        self.put(name, &[0xBE80_0000 | u32::from(d) << 16 | op << 8 | x], la);
    }

    fn sopc(&mut self, name: &'static str, a: Src, b: Src) {
        let ((x, la), (y, lb)) = (a.code(), b.code());
        let op = opcode(Encoding::Sopc, name);
        self.put(name, &[0xBF00_0000 | op << 16 | y << 8 | x], Self::lit(la, lb));
    }

    fn sopp(&mut self, name: &'static str, imm: u16) {
        let op = opcode(Encoding::Sopp, name);
        self.put(name, &[0xBF80_0000 | op << 16 | u32::from(imm)], None);
    }

    /// A branch to `label` (resolved by [`Asm::finish`]).
    fn br(&mut self, name: &'static str, label: &'static str) {
        self.jumps.push((self.w.len(), label));
        self.sopp(name, 0);
    }

    fn at(&mut self, label: &'static str) {
        assert!(self.labels.insert(label, self.w.len()).is_none(), "label {label} twice");
    }

    fn waitcnt(&mut self) {
        self.sopp("s_waitcnt", 0xC07F);
    }

    fn vmcnt(&mut self) {
        self.sopp("s_waitcnt", 0x3F70);
    }

    /// `s_load_dword*` of `d` from `s[base:base+1]` plus `offset` bytes.
    fn smem(&mut self, name: &'static str, d: u8, base: u8, offset: u32) {
        let op = opcode(Encoding::Smem, name);
        let w0 = 0xF400_0000 | op << 18 | u32::from(d) << 6 | u32::from(base) >> 1;
        self.put(name, &[w0, 0xFA00_0000 | offset & 0x1F_FFFF], None);
    }

    fn vop1(&mut self, name: &'static str, d: u8, a: Src) {
        let (x, la) = a.code();
        let op = opcode(Encoding::Vop1, name);
        self.put(name, &[0x7E00_0000 | u32::from(d) << 17 | op << 9 | x], la);
    }

    fn vop2(&mut self, name: &'static str, d: u8, a: Src, b: u8) {
        let (x, la) = a.code();
        let op = opcode(Encoding::Vop2, name);
        self.put(name, &[op << 25 | u32::from(d) << 17 | u32::from(b) << 9 | x], la);
    }

    /// A compare into VCC (EXEC for `v_cmpx_*`).
    fn vopc(&mut self, name: &'static str, a: Src, b: u8) {
        let (x, la) = a.code();
        let op = opcode(Encoding::Vopc, name);
        self.put(name, &[0x7C00_0000 | op << 17 | u32::from(b) << 9 | x], la);
    }

    /// The VOP3 form; `d` is a VGPR, or the SGPR of a compare or lane read.
    fn vop3(&mut self, name: &'static str, d: u8, srcs: &[Src]) {
        let op = opcode(Encoding::Vop3, name);
        let mut w1 = 0;
        let mut lit = None;
        for (k, s) in srcs.iter().enumerate() {
            let (x, l) = s.code();
            w1 |= x << (9 * k);
            lit = Self::lit(lit, l);
        }
        self.put(name, &[0xD400_0000 | op << 16 | u32::from(d), w1], lit);
    }

    /// The DPP16 form of a VOP2 (`b` the second source) or, with `b` none, a
    /// VOP1; `ctrl` the DPP control, every row and bank writing.
    fn dpp(&mut self, name: &'static str, d: u8, a: u8, b: Option<u8>, ctrl: u32, rows: u32) {
        let ext = rows << 28 | 0xF << 24 | ctrl << 8 | u32::from(a);
        let w0 = match b {
            Some(b) => opcode(Encoding::Vop2, name) << 25 | u32::from(d) << 17 | u32::from(b) << 9,
            None => 0x7E00_0000 | u32::from(d) << 17 | opcode(Encoding::Vop1, name) << 9,
        };
        self.put(name, &[w0 | 0xFA, ext], None);
    }

    fn ds(&mut self, name: &'static str, d: u8, addr: u8, data: u8, offset: u16) {
        let op = opcode(Encoding::Ds, name);
        let w0 = 0xD800_0000 | op << 18 | u32::from(offset);
        self.put(name, &[w0, u32::from(d) << 24 | u32::from(data) << 8 | u32::from(addr)], None);
    }

    /// A buffer access through `s[rsrc:rsrc+3]` at byte `addr` (offen).
    fn mubuf(&mut self, name: &'static str, data: u8, addr: u8, rsrc: u8, offset: u16, glc: bool) {
        let op = opcode(Encoding::Mubuf, name);
        let w0 = 0xE000_0000
            | (op & 0x7F) << 18
            | (op >> 7) << 25
            | u32::from(glc) << 14
            | 1 << 12
            | u32::from(offset);
        let w1 = 128 << 24 | (u32::from(rsrc) >> 2) << 16 | u32::from(data) << 8 | u32::from(addr);
        self.put(name, &[w0, w1], None);
    }

    /// The code, branches resolved, each instruction checked by the decoder.
    fn finish(mut self) -> Vec<u32> {
        for &(at, label) in &self.jumps {
            let to = self.labels[label];
            let delta = to as i64 - (at as i64 + 1);
            self.w[at] |= u32::from(delta as i16 as u16);
        }
        let bytes = crate::gfx10::test_asm::bytes(&self.w);
        let insts = crate::gfx10::decode(&bytes).expect("assembled code decodes");
        for &(at, name) in &self.names {
            let found = insts.iter().find(|i| i.offset == 4 * at);
            assert_eq!(
                found.and_then(|i| i.name()),
                Some(name),
                "dword {at}: {:?}",
                found.map(|i| i.text(true))
            );
        }
        self.w
    }
}

/// A console wave64 compute program of `threads` (x, y) per group from
/// `body`, with the UAV table in user SGPR 0 and the group id in s1, thread
/// ids in v0 (and v1), `lds` bytes of LDS (512-byte granules).
fn program(words: Vec<u32>, threads: [u32; 2], lds: u32) -> NativeProgram {
    let mut p = compute(&words, true);
    p.vgprs = 32;
    p.layout = vec![RootParam {
        root: RootType::Table,
        table_offset_dwords: 0,
        sgprs: 1,
        base_register: 0,
        user_data: 0,
        slot: 0,
    }];
    let tidig = u32::from(threads[1] > 1);
    p.io = StageIo::Cs(ComputeIo {
        rsrc2: 1 << 1 | 1 << 7 | tidig << 11 | lds.div_ceil(512) << 15,
        threads: [threads[0], threads[1], 1],
    });
    p
}

/// The usual start: s[4:7] the UAV, s8 this group's first result byte.
fn prologue(a: &mut Asm, threads: u32) {
    a.sop1("s_mov_b32", 2, S(0));
    a.sop1("s_mov_b32", 3, I(0));
    a.smem("s_load_dwordx4", 4, 2, 0);
    a.sop2("s_mul_i32", 8, S(1), L(threads * 4));
    a.waitcnt();
}

/// Stores `value` as the result of thread `flat` (dword g * threads + flat).
fn store_result(a: &mut Asm, flat: u8, value: u8) {
    a.vop2("v_lshlrev_b32", 3, I(2), flat);
    a.vop2("v_add_nc_u32", 3, S(8), 3);
    a.mubuf("buffer_store_dword", value, 3, 4, 0, false);
}

/// `v_mbcnt` of all lanes: the console lane in `d`.
fn lane_id(a: &mut Asm, d: u8) {
    a.vop3("v_mbcnt_lo_u32_b32", d, &[I(-1), I(0)]);
    a.vop3("v_mbcnt_hi_u32_b32", d, &[I(-1), V(d)]);
}

/// Translates `native` in paired lanes for `target`, whatever other forms
/// would take it.
fn paired_for(native: &NativeProgram, target: Target) -> Result<HostShader, crate::ShaderError> {
    let c = crate::Container {
        digest: [0; 16],
        version: (1, 0),
        parts: vec![],
    };
    let p = crate::ir::build(&native.code, native.wave64, native.vgprs)?;
    let res = crate::resource::track(&p, &native.layout, native.stage)?;
    let io = crate::io::raise(&p, native)?;
    super::paired(&c, native, &p, &io, &res, target)
}

/// The thread group a DXIL container's `PSV0` declares.
fn threads_of(dxil: &[u8]) -> Option<[u32; 3]> {
    let c = crate::Container::parse(dxil).ok()?;
    let psv = c.part(crate::FourCC::PSV0)?.data;
    let u = |at: usize| crate::dxbc::u32_at(psv, at).ok();
    Some([u(4 + 36)?, u(4 + 40)?, u(4 + 44)?])
}

/// The wave size a DXIL container's `PSV0` requires.
fn wave_size(dxil: &[u8]) -> Option<u32> {
    let c = crate::Container::parse(dxil).ok()?;
    let psv = c.part(crate::FourCC::PSV0)?.data;
    let u = |at: usize| crate::dxbc::u32_at(psv, at).ok();
    let (min, max) = (u(4 + 16)?, u(4 + 20)?);
    (min == max).then_some(min)
}

/// Runs `cs` in `groups` groups over a zeroed raw UAV of `dwords` dwords,
/// after the validator where one is installed.
fn run(host: &Host, cs: &HostShader, groups: u32, dwords: u32) -> Result<Vec<u32>, String> {
    if let Ok(mut v) =
        super::validator::Validator::load(super::validator::Validator::default_path().as_deref())
    {
        v.validate(&mut cs.dxil.clone())
            .map_err(|e| format!("validation: {e}"))?;
    }
    let (root, pso) = host.compute_pipeline(cs)?;
    let data = vec![0; (cs.user_data_dwords() as usize).max(1)];
    let views = [View::RawUav(dwords)];
    let out = host.dispatch(
        &root,
        &pso,
        &Dispatch {
            roots: vec![(0, Root::Constants(data))],
            views: &views,
            samplers: &[],
            groups: [groups, 1, 1],
        },
    )?;
    Ok(out.into_iter().next().unwrap_or_default())
}

/// The hosts that run 32-lane waves, by name.
fn hosts() -> Vec<(String, Host)> {
    let Ok(names) = adapters() else {
        eprintln!("no DXGI; skipped");
        return Vec::new();
    };
    names
        .into_iter()
        .filter_map(|name| {
            let host = Host::on(&name).ok()?;
            host.target().runs(32).then_some((name, host))
        })
        .collect()
}

/// Translates `native` in paired lanes on every host of 32-lane waves,
/// checks the form (a host wave of 32 for each console wave) and runs it.
fn each_paired(
    label: &str,
    native: &NativeProgram,
    groups: u32,
    dwords: u32,
    check: impl Fn(&str, &[u32]),
) {
    let StageIo::Cs(io) = &native.io else { unreachable!() };
    let threads: u32 = io.threads.iter().product();
    let mut ran = 0;
    for (name, host) in hosts() {
        let cs = paired_for(native, host.target())
            .unwrap_or_else(|e| panic!("{name}: {label}: {e}"));
        assert_eq!(wave_size(&cs.dxil), Some(32), "{name}: {label}: wave size");
        assert_eq!(threads_of(&cs.dxil), Some([threads / 2, 1, 1]), "{name}: {label}: group");
        let out = run(&host, &cs, groups, dwords)
            .unwrap_or_else(|e| panic!("{name}: {label}: {e}"));
        check(&name, &out);
        eprintln!("{name}: {label}: exact in paired lanes ({} bytes of DXIL)", cs.dxil.len());
        ran += 1;
    }
    if ran == 0 {
        eprintln!("{label}: no host of 32-lane waves; skipped");
    }
}

#[test]
fn the_assembler_matches_the_decoder() {
    let mut a = Asm::default();
    a.sop2("s_add_u32", 9, S(9), L(700));
    a.vop3("v_readlane_b32", 9, &[V(0), I(40)]);
    a.dpp("v_add_nc_u32", 1, 1, Some(1), 0x161, 0xF);
    a.ds("ds_add_rtn_u32", 20, 10, 11, 1040);
    a.mubuf("buffer_atomic_add", 13, 14, 4, 0, true);
    a.at("end");
    a.br("s_cbranch_scc0", "end");
    a.sopp("s_endpgm", 0);
    let words = a.finish();
    // The split form's tests wrote these by hand.
    assert_eq!(words[2..4], [0xD760_0009, 0x0001_5100]);
    assert_eq!(words[4..6], [0x4A02_02FA, 0xFF01_6101]);
    assert_eq!(words[10] & 0xFFFF, 0xFFFF, "a branch to itself");
}

/// The switch: on by default, read once, part of the cache key's bits.
#[test]
fn the_wave_pairs_switch_is_a_translation_switch() {
    assert_eq!(Switch::WavePairs.var(), "VICTORY_WAVE_PAIRS");
    assert!(Switch::WavePairs.default_on());
    let bit = Switch::ALL.iter().position(|&s| s == Switch::WavePairs).expect("listed");
    assert_ne!(super::default_switches() & 1 << bit, 0);
}

// ---- A: two console waves walk rounds of their own (DefragHeap's walks) ----

/// 128 threads: each console wave goes round `5 + 4 w` times (`rounds(w)`),
/// every lane adding the wave's LDS dword to its sum each round, while lane
/// 63 adds `(lane ^ round) + 1` of lane 63 (a lane read) to that dword for
/// the next round, with no barrier between; then a barrier, and each thread
/// adds the other wave's final dword, shifted up by 16.
fn walks(base: u32, step: u32) -> Vec<u32> {
    let mut a = Asm::default();
    prologue(&mut a, 128);
    lane_id(&mut a, 4);
    a.vop2("v_lshrrev_b32", 5, I(6), 0); // the wave
    a.vop2("v_lshlrev_b32", 13, I(2), 5); // its dword's byte
    a.vop3("v_mul_lo_u32", 6, &[V(5), L(1000)]);
    a.vop2("v_add_nc_u32", 6, I(7), 6);
    a.vopc("v_cmp_eq_u32", I(0), 4);
    a.sop1("s_and_saveexec_b64", 10, Src::Vcc);
    a.ds("ds_write_b32", 0, 13, 6, 0);
    a.sop1("s_mov_b64", EXEC, S(10));
    a.waitcnt();
    a.sopp("s_barrier", 0);
    a.vop1("v_readfirstlane_b32", 12, V(5));
    a.sop2("s_mul_i32", 12, S(12), L(step));
    a.sop2("s_add_u32", 12, S(12), L(base));
    a.sop1("s_mov_b32", 13, I(0));
    a.vop1("v_mov_b32", 7, I(0));
    a.at("round");
    a.ds("ds_read_b32", 6, 13, 0, 0);
    a.waitcnt();
    a.vop2("v_add_nc_u32", 7, V(7), 6);
    a.vop2("v_xor_b32", 8, S(13), 4);
    a.vop3("v_readlane_b32", 14, &[V(8), I(63)]);
    a.vopc("v_cmp_eq_u32", I(63), 4);
    a.sop1("s_and_saveexec_b64", 10, Src::Vcc);
    a.br("s_cbranch_execz", "skip");
    a.ds("ds_read_b32", 9, 13, 0, 0);
    a.waitcnt();
    a.vop2("v_add_nc_u32", 9, S(14), 9);
    a.vop2("v_add_nc_u32", 9, I(1), 9);
    a.ds("ds_write_b32", 0, 13, 9, 0);
    a.at("skip");
    a.sop1("s_mov_b64", EXEC, S(10));
    a.sop2("s_add_u32", 13, S(13), I(1));
    a.sopc("s_cmp_lt_u32", S(13), S(12));
    a.br("s_cbranch_scc1", "round");
    a.waitcnt();
    a.sopp("s_barrier", 0);
    a.vop2("v_xor_b32", 10, I(4), 13); // the other wave's dword
    a.ds("ds_read_b32", 11, 10, 0, 0);
    a.waitcnt();
    a.vop2("v_lshlrev_b32", 11, I(16), 11);
    a.vop2("v_add_nc_u32", 2, V(7), 11);
    store_result(&mut a, 0, 2);
    a.sopp("s_endpgm", 0);
    a.finish()
}

/// What the console's waves leave in `walks`' results.
fn walks_want(base: u32, step: u32) -> Vec<u32> {
    let mut sum = [0u32; 2];
    let mut last = [0u32; 2];
    for w in 0..2u32 {
        let mut value = 1000 * w + 7;
        for r in 0..base + step * w {
            sum[w as usize] = sum[w as usize].wrapping_add(value);
            value = value.wrapping_add((63 ^ r) + 1);
        }
        last[w as usize] = value;
    }
    (0..128u32)
        .map(|t| {
            let w = (t / 64) as usize;
            sum[w].wrapping_add(last[w ^ 1] << 16)
        })
        .collect()
}

#[test]
fn waves_walk_rounds_of_their_own_and_hand_lds_on_without_barriers() {
    for (base, step, groups) in [(5, 4, 6), (1000, 1, 2)] {
        let native = program(walks(base, step), [128, 1], 512);
        let one = walks_want(base, step);
        let label = format!("walks of {base} and {} rounds", base + step);
        each_paired(&label, &native, groups, 128 * groups, |name, out| {
            for g in 0..groups as usize {
                assert_eq!(&out[g * 128..][..128], &one[..], "{name}: {label}, group {g}");
            }
        });
    }
}

// ---- B: a per-wave any around a whole-wave min and max (ShadowUnifiedClearRaster) ----

/// A whole-wave section reducing `x` with `op` (`v_min_f32` or
/// `v_max_f32`, identity `ident`) as a compiler expands `WaveActiveMin`:
/// rows by `row_xmask`, halves by `v_permlanex16`, then lane 32's and lane
/// 0's reads; the result in `out`. Uses v24, v25, s[28:29] and s30.
fn wave_reduce(a: &mut Asm, op: &'static str, ident: u32, x: u8, out: u8) {
    a.sop1("s_or_saveexec_b64", 28, I(-1));
    a.vop1("v_mov_b32", 24, L(ident));
    a.vop3("v_cndmask_b32", 24, &[V(24), V(x), S(28)]);
    for n in [1, 2, 4, 8] {
        a.dpp(op, 24, 24, Some(24), 0x160 + n, 0xF);
    }
    a.vop3("v_permlanex16_b32", 25, &[V(24), I(-1), I(-1)]);
    a.vop2(op, 24, V(24), 25);
    a.vop3("v_readlane_b32", 30, &[V(24), I(32)]);
    a.vop2(op, 24, S(30), 24);
    a.vop3("v_readlane_b32", out, &[V(24), I(0)]);
    a.sop1("s_mov_b64", EXEC, S(28));
}

/// 16 x 16 threads: lane t's depth `(37 t & 127)` as a float, flagged where
/// `13 t & 7` is 3 in the lower half of even waves and the upper half of odd
/// ones, never in the third wave. A wave whose lanes are all unflagged
/// (`s_bcnt1` of the flags) skips the sections; the others take the flagged
/// lanes' least and greatest depth. The first lane above 36 writes the
/// wave's two and its count to LDS (`s_ff1` of that EXEC), and after a
/// barrier thread 0 puts the four waves together (at dwords 256 groups + 4
/// g ...); each thread gives its wave's least bits xor t.
fn tiles(groups: u32) -> Vec<u32> {
    let mut a = Asm::default();
    prologue(&mut a, 256);
    a.vop3("v_lshl_add_u32", 20, &[V(1), I(4), V(0)]); // t
    a.vop2("v_lshrrev_b32", 21, I(6), 20); // the wave
    a.vop3("v_mul_lo_u32", 8, &[V(20), I(37)]);
    a.vop2("v_and_b32", 8, L(127), 8);
    a.vop1("v_cvt_f32_u32", 8, V(8));
    a.vop3("v_mul_lo_u32", 9, &[V(20), I(13)]);
    a.vop2("v_and_b32", 9, I(7), 9);
    a.vop3("v_cmp_eq_u32", 16, &[I(3), V(9)]);
    a.vop2("v_and_b32", 22, I(1), 21);
    a.vop2("v_lshlrev_b32", 22, I(5), 22);
    a.vop2("v_and_b32", 23, I(32), 20);
    a.vop3("v_cmp_eq_u32", 32, &[V(22), V(23)]);
    a.vopc("v_cmp_ne_u32", I(2), 21);
    a.sop2("s_and_b64", VCC, Src::Vcc, S(16));
    a.sop2("s_and_b64", VCC, Src::Vcc, S(32));
    a.vop1("v_mov_b32", 10, L(INF));
    a.vop1("v_mov_b32", 11, L(NEG_INF));
    a.sop2("s_and_b64", 18, Src::Vcc, Src::Exec);
    a.sop1("s_bcnt1_i32_b64", 18, S(18));
    a.sopc("s_cmp_eq_u32", S(18), I(0));
    a.br("s_cbranch_scc1", "none");
    a.sop1("s_and_saveexec_b64", 22, Src::Vcc);
    wave_reduce(&mut a, "v_min_f32", INF, 8, 12);
    wave_reduce(&mut a, "v_max_f32", NEG_INF, 8, 14);
    a.sop1("s_mov_b64", EXEC, S(22));
    a.vop1("v_mov_b32", 10, S(12));
    a.vop1("v_mov_b32", 11, S(14));
    a.at("none");
    lane_id(&mut a, 12);
    a.sop1("s_mov_b64", 26, Src::Exec);
    a.vopc("v_cmpx_lt_u32", I(36), 12);
    a.sop1("s_ff1_i32_b64", 24, Src::Exec);
    a.vopc("v_cmp_eq_u32", S(24), 12);
    a.sop2("s_and_b64", EXEC, Src::Exec, Src::Vcc);
    a.vop2("v_lshlrev_b32", 13, I(4), 21);
    a.ds("ds_write_b32", 0, 13, 10, 0);
    a.ds("ds_write_b32", 0, 13, 11, 4);
    a.vop1("v_mov_b32", 14, S(18));
    a.ds("ds_write_b32", 0, 13, 14, 8);
    a.sop1("s_mov_b64", EXEC, S(26));
    a.waitcnt();
    a.sopp("s_barrier", 0);
    a.vopc("v_cmp_eq_u32", I(0), 20);
    a.sop1("s_and_saveexec_b64", 26, Src::Vcc);
    a.br("s_cbranch_execz", "done");
    a.vop1("v_mov_b32", 15, I(0));
    a.ds("ds_read_b32", 16, 15, 0, 0);
    a.ds("ds_read_b32", 17, 15, 0, 4);
    a.ds("ds_read_b32", 18, 15, 0, 8);
    for w in 1..4u16 {
        a.ds("ds_read_b32", 19, 15, 0, 16 * w);
        a.ds("ds_read_b32", 22, 15, 0, 16 * w + 4);
        a.ds("ds_read_b32", 23, 15, 0, 16 * w + 8);
        a.waitcnt();
        a.vop2("v_min_f32", 16, V(16), 19);
        a.vop2("v_max_f32", 17, V(17), 22);
        a.vop2("v_add_nc_u32", 18, V(18), 23);
    }
    a.sop2("s_mul_i32", 31, S(1), I(16));
    a.sop2("s_add_u32", 31, S(31), L(256 * groups * 4));
    a.vop1("v_mov_b32", 3, S(31));
    a.mubuf("buffer_store_dword", 16, 3, 4, 0, false);
    a.mubuf("buffer_store_dword", 17, 3, 4, 4, false);
    a.mubuf("buffer_store_dword", 18, 3, 4, 8, false);
    a.at("done");
    a.sop1("s_mov_b64", EXEC, S(26));
    a.vop2("v_xor_b32", 2, V(10), 20);
    store_result(&mut a, 20, 2);
    a.sopp("s_endpgm", 0);
    a.finish()
}

/// `tiles`' results as the console's waves compute them.
fn tiles_want(groups: u32) -> Vec<u32> {
    let depth = |t: u32| ((37 * t) & 127) as f32;
    let flagged = |t: u32| (13 * t) & 7 == 3 && t & 32 == (t / 64 & 1) << 5 && t / 64 != 2;
    let mut per_wave = [(f32::INFINITY, f32::NEG_INFINITY, 0u32); 4];
    for t in 0..256 {
        let w = &mut per_wave[(t / 64) as usize];
        if flagged(t) {
            w.0 = w.0.min(depth(t));
            w.1 = w.1.max(depth(t));
            w.2 += 1;
        }
    }
    let mut out: Vec<u32> = (0..groups)
        .flat_map(|_| (0..256).map(|t| per_wave[(t / 64) as usize].0.to_bits() ^ t))
        .collect();
    let all = per_wave
        .iter()
        .fold((f32::INFINITY, f32::NEG_INFINITY, 0), |a, w| {
            (a.0.min(w.0), a.1.max(w.1), a.2 + w.2)
        });
    for _ in 0..groups {
        out.extend([all.0.to_bits(), all.1.to_bits(), all.2, 0]);
    }
    out
}

#[test]
fn waves_skip_sections_by_their_own_any_and_meet_at_the_barrier() {
    let groups = 8;
    let native = program(tiles(groups), [16, 16], 64);
    let want = tiles_want(groups);
    each_paired("per-wave any, minimum and maximum", &native, groups, 260 * groups, |name, out| {
        assert_eq!(out, &want[..], "{name}");
    });
}

// ---- C: a counting sort (SortRayTiles) ----

/// A whole-wave exclusive prefix sum of `x` into `out`, as a compiler
/// expands `WavePrefixSum` (the console flow's prefix test, on other
/// registers): rows by `row_shr`, the other row of each 32 by
/// `v_permlanex16`, the upper half after lane 31, then one lane up. Uses
/// v24 to v27, s[28:29], s30 and s31.
fn wave_prefix(a: &mut Asm, x: u8, out: u8) {
    a.sop1("s_or_saveexec_b64", 28, I(-1));
    a.vop3("v_cndmask_b32", 24, &[I(0), V(x), S(28)]);
    for n in [1, 2, 4, 8] {
        a.dpp("v_add_nc_u32", 24, 24, Some(24), 0x110 + n, 0xF);
    }
    a.vop3("v_permlanex16_b32", 25, &[V(24), I(-1), I(-1)]);
    a.vop1("v_mov_b32", 26, I(0));
    a.dpp("v_mov_b32", 26, 25, None, 0xE4, 0xA);
    a.vop2("v_add_nc_u32", 24, V(24), 26);
    a.vop3("v_readlane_b32", 30, &[V(24), I(31)]);
    a.vop1("v_mov_b32", 27, S(30));
    a.vop1("v_mov_b32", 26, I(0));
    a.dpp("v_mov_b32", 26, 27, None, 0xE4, 0xC);
    a.vop2("v_add_nc_u32", 24, V(24), 26);
    a.vop1("v_mov_b32", out, I(0));
    a.dpp("v_mov_b32", out, 24, None, 0x111, 0xF);
    for lane in [16, 32, 48] {
        a.vop3("v_readlane_b32", 31, &[V(24), I(lane - 1)]);
        a.vop3("v_writelane_b32", out, &[S(31), I(lane)]);
    }
    a.sop1("s_mov_b64", EXEC, S(28));
}

/// Records per group.
const RECORDS: u32 = 700;

/// The 8-bit key of record `i` of group `g`.
fn key(g: u32, i: u32) -> u32 {
    (g * RECORDS + i).wrapping_mul(0x9E37_79B1) >> 24
}

/// Loops thread t over its records (in v9) while some lane of its console
/// wave has one left (an exit mask, as the console compiler closes such
/// loops), running `body` for each; `tag` names the labels. A wave takes 192
/// records, 2 in each lower lane and then 4 in each upper one (those below
/// 700): the upper lanes go round longest.
fn each_record(a: &mut Asm, tag: (&'static str, &'static str), body: impl Fn(&mut Asm)) {
    a.vop2("v_and_b32", 7, I(63), 0);
    a.vop2("v_lshrrev_b32", 8, I(6), 0);
    a.vop3("v_mul_lo_u32", 8, &[V(8), L(192)]);
    a.vop2("v_lshlrev_b32", 9, I(1), 7);
    a.vop2("v_lshlrev_b32", 10, I(2), 7);
    a.vop2("v_subrev_nc_u32", 10, I(64), 10);
    a.vopc("v_cmp_gt_u32", I(32), 7);
    a.vop2("v_cndmask_b32", 9, V(10), 9);
    a.vop2("v_add_nc_u32", 7, V(8), 9);
    a.vop1("v_mov_b32", 11, I(4));
    a.vop1("v_mov_b32", 12, I(2));
    a.vop2("v_cndmask_b32", 11, V(11), 12);
    a.vop2("v_add_nc_u32", 8, V(7), 11);
    a.vop2("v_min_u32", 8, L(RECORDS), 8);
    a.vopc("v_cmp_lt_u32", V(7), 8);
    a.sop1("s_and_saveexec_b64", 10, Src::Vcc);
    a.br("s_cbranch_execz", tag.1);
    a.vop1("v_mov_b32", 9, V(7));
    a.sop1("s_mov_b64", 12, Src::Exec);
    a.at(tag.0);
    body(a);
    a.vop2("v_add_nc_u32", 9, I(1), 9);
    a.sop1("s_mov_b64", 14, Src::Exec);
    a.vopc("v_cmpx_eq_u32", V(9), 8);
    a.sop2("s_andn2_b64", 12, S(12), Src::Exec);
    a.br("s_cbranch_scc0", tag.1);
    a.sop2("s_and_b64", EXEC, S(14), S(12));
    a.br("s_branch", tag.0);
    a.at(tag.1);
    a.sop1("s_mov_b64", EXEC, S(10));
}

/// The key of v9's record into v10, as a byte offset.
fn record_key(a: &mut Asm) {
    a.vop2("v_add_nc_u32", 10, S(20), 9);
    a.vop3("v_mul_lo_u32", 10, &[V(10), L(0x9E37_79B1)]);
    a.vop2("v_lshrrev_b32", 10, I(24), 10);
    a.vop2("v_lshlrev_b32", 10, I(2), 10);
}

/// 256 threads sort 700 records a group by an 8-bit key: a histogram
/// (`ds_add_u32` per record), each wave's exclusive prefix of the 256
/// counts with lane 63's total to LDS, thread 0 runs over the four totals,
/// then each record's slot by `ds_add_rtn_u32`; the record goes there
/// (from dword 256 groups + 700 g). Thread t gives bucket t's first slot.
fn sort(groups: u32) -> Vec<u32> {
    let mut a = Asm::default();
    prologue(&mut a, 256);
    a.sop2("s_mul_i32", 20, S(1), L(RECORDS));
    a.sop2("s_mul_i32", 21, S(1), L(RECORDS * 4));
    a.sop2("s_add_u32", 21, S(21), L(256 * groups * 4));
    a.vop2("v_lshlrev_b32", 5, I(2), 0);
    a.vop1("v_mov_b32", 6, I(0));
    a.ds("ds_write_b32", 0, 5, 6, 0);
    a.waitcnt();
    a.sopp("s_barrier", 0);
    each_record(&mut a, ("count", "counted"), |a| {
        record_key(a);
        a.vop1("v_mov_b32", 11, I(1));
        a.ds("ds_add_u32", 0, 10, 11, 0);
    });
    a.waitcnt();
    a.sopp("s_barrier", 0);
    a.ds("ds_read_b32", 12, 5, 0, 0);
    a.waitcnt();
    wave_prefix(&mut a, 12, 13);
    lane_id(&mut a, 14);
    a.vopc("v_cmp_eq_u32", I(63), 14);
    a.sop1("s_and_saveexec_b64", 10, Src::Vcc);
    a.vop2("v_add_nc_u32", 15, V(13), 12);
    a.vop2("v_lshrrev_b32", 16, I(6), 0);
    a.vop2("v_lshlrev_b32", 16, I(2), 16);
    a.ds("ds_write_b32", 0, 16, 15, 1024);
    a.sop1("s_mov_b64", EXEC, S(10));
    a.waitcnt();
    a.sopp("s_barrier", 0);
    a.vopc("v_cmp_eq_u32", I(0), 0);
    a.sop1("s_and_saveexec_b64", 10, Src::Vcc);
    a.br("s_cbranch_execz", "summed");
    a.sop1("s_mov_b32", 16, I(0));
    a.sop1("s_mov_b32", 17, I(0));
    a.at("totals");
    a.sop2("s_lshl_b32", 18, S(17), I(2));
    a.vop1("v_mov_b32", 17, S(18));
    a.ds("ds_read_b32", 18, 17, 0, 1024);
    a.waitcnt();
    a.vop1("v_mov_b32", 19, S(16));
    a.ds("ds_write_b32", 0, 17, 19, 1024);
    a.vop1("v_readfirstlane_b32", 19, V(18));
    a.sop2("s_add_u32", 16, S(16), S(19));
    a.sop2("s_add_u32", 17, S(17), I(1));
    a.sopc("s_cmp_lt_u32", S(17), I(4));
    a.br("s_cbranch_scc1", "totals");
    a.at("summed");
    a.sop1("s_mov_b64", EXEC, S(10));
    a.waitcnt();
    a.sopp("s_barrier", 0);
    a.vop2("v_lshrrev_b32", 16, I(6), 0);
    a.vop2("v_lshlrev_b32", 16, I(2), 16);
    a.ds("ds_read_b32", 17, 16, 0, 1024);
    a.waitcnt();
    a.vop2("v_add_nc_u32", 17, V(17), 13);
    a.ds("ds_write_b32", 0, 5, 17, 1040);
    a.waitcnt();
    a.sopp("s_barrier", 0);
    each_record(&mut a, ("place", "placed"), |a| {
        record_key(a);
        a.vop1("v_mov_b32", 11, I(1));
        a.ds("ds_add_rtn_u32", 22, 10, 11, 1040);
        a.waitcnt();
        a.vop2("v_lshlrev_b32", 22, I(2), 22);
        a.vop2("v_add_nc_u32", 22, S(21), 22);
        a.mubuf("buffer_store_dword", 9, 22, 4, 0, false);
    });
    a.vop1("v_mov_b32", 2, V(17));
    store_result(&mut a, 0, 2);
    a.sopp("s_endpgm", 0);
    a.finish()
}

#[test]
fn waves_sort_with_loops_while_lanes_have_records() {
    let groups = 6;
    let native = program(sort(groups), [256, 1], 516 * 4);
    each_paired("counting sort", &native, groups, (256 + RECORDS) * groups, |name, out| {
        for g in 0..groups {
            let mut hist = [0u32; 256];
            for i in 0..RECORDS {
                hist[key(g, i) as usize] += 1;
            }
            let mut first = 0;
            for (t, &n) in hist.iter().enumerate() {
                let slot = out[(g * 256) as usize + t];
                assert_eq!(slot, first, "{name}: group {g}, bucket {t}'s slot");
                first += n;
            }
            let sorted = &out[(256 * groups + RECORDS * g) as usize..][..RECORDS as usize];
            let mut seen = vec![false; RECORDS as usize];
            for (p, &i) in sorted.iter().enumerate() {
                assert!(i < RECORDS && !seen[i as usize], "{name}: group {g}: record {i} at {p}");
                seen[i as usize] = true;
                if p > 0 {
                    assert!(key(g, sorted[p - 1]) <= key(g, i), "{name}: group {g}: order at {p}");
                }
            }
        }
    });
}

// ---- D: a per-wave "any and any" count appended to a list (AssembleComputeList) ----

/// The two flags of item `i` of group `g`.
fn flags(g: u32, i: u32) -> (bool, bool) {
    let h = i.wrapping_mul(0x9E37_79B1).wrapping_add(g).wrapping_mul(0x85EB_CA6B);
    (h >> 31 != 0, (h >> 8) & 15 > 13)
}

/// Where `list`'s counter and entries go (dwords).
fn list_at(groups: u32) -> (u32, u32) {
    (256 * groups, 256 * groups + 4)
}

/// 16 x 16 threads take items `256 r + (t ^ 32)` below 324 in two rounds (in
/// the second, the second wave's upper lanes 32 to 35 alone); a wave
/// with a lane of each flag (two `s_bcnt1` tests) adds 1 to an LDS count
/// from its first lane, and all meet at a barrier each round. Thread 0
/// appends (group, count) to a list (a global add for the place) when the
/// count is not 0; every thread gives the count.
fn list(groups: u32) -> Vec<u32> {
    let (counter, entries) = list_at(groups);
    let mut a = Asm::default();
    prologue(&mut a, 256);
    a.vop3("v_lshl_add_u32", 20, &[V(1), I(4), V(0)]);
    a.vopc("v_cmp_eq_u32", I(0), 20);
    a.sop1("s_and_saveexec_b64", 10, Src::Vcc);
    a.vop1("v_mov_b32", 3, I(0));
    a.ds("ds_write_b32", 0, 3, 3, 0);
    a.sop1("s_mov_b64", EXEC, S(10));
    a.waitcnt();
    a.sopp("s_barrier", 0);
    a.sop1("s_mov_b32", 12, I(0));
    a.sop1("s_mov_b32", 13, L(324));
    a.at("round");
    a.sop1("s_mov_b64", 14, Src::Exec);
    a.vop2("v_xor_b32", 4, I(32), 20);
    a.vop3("v_lshl_add_u32", 4, &[S(12), I(8), V(4)]);
    a.vopc("v_cmpx_gt_u32", S(13), 4);
    a.br("s_cbranch_execz", "next");
    a.vop3("v_mul_lo_u32", 5, &[V(4), L(0x9E37_79B1)]);
    a.vop2("v_add_nc_u32", 5, S(1), 5);
    a.vop3("v_mul_lo_u32", 5, &[V(5), L(0x85EB_CA6B)]);
    a.vop2("v_lshrrev_b32", 6, I(31), 5);
    a.vop3("v_bfe_u32", 7, &[V(5), I(8), I(4)]);
    a.vop3("v_cmp_ne_u32", 16, &[I(0), V(6)]);
    a.vop3("v_cmp_lt_u32", 18, &[I(13), V(7)]);
    a.sop2("s_and_b64", 16, S(16), Src::Exec);
    a.sop1("s_bcnt1_i32_b64", 20, S(16));
    a.sop2("s_cselect_b32", 20, I(1), I(0));
    a.sop2("s_and_b64", 18, S(18), Src::Exec);
    a.sop1("s_bcnt1_i32_b64", 21, S(18));
    a.sopc("s_cmp_eq_u32", S(21), I(0));
    a.br("s_cbranch_scc1", "next");
    a.sopc("s_cmp_eq_u32", S(20), I(0));
    a.br("s_cbranch_scc1", "next");
    lane_id(&mut a, 8);
    a.sop1("s_ff1_i32_b64", 22, Src::Exec);
    a.vopc("v_cmpx_eq_u32", S(22), 8);
    a.vop1("v_mov_b32", 9, I(0));
    a.vop1("v_mov_b32", 10, I(1));
    a.ds("ds_add_u32", 0, 9, 10, 0);
    a.at("next");
    a.sop1("s_mov_b64", EXEC, S(14));
    a.waitcnt();
    a.sopp("s_barrier", 0);
    a.sop2("s_add_u32", 12, S(12), I(1));
    a.sopc("s_cmp_lg_u32", S(12), I(2));
    a.br("s_cbranch_scc1", "round");
    a.vop1("v_mov_b32", 11, I(0));
    a.ds("ds_read_b32", 12, 11, 0, 0);
    a.waitcnt();
    a.vopc("v_cmp_eq_u32", I(0), 20);
    a.vop3("v_cmp_ne_u32", 24, &[I(0), V(12)]);
    a.sop2("s_and_b64", VCC, Src::Vcc, S(24));
    a.sop1("s_and_saveexec_b64", 26, Src::Vcc);
    a.br("s_cbranch_execz", "done");
    a.vop1("v_mov_b32", 13, I(1));
    a.vop1("v_mov_b32", 14, L(counter * 4));
    a.mubuf("buffer_atomic_add", 13, 14, 4, 0, true);
    a.vmcnt();
    a.vop2("v_lshlrev_b32", 15, I(3), 13);
    a.vop2("v_add_nc_u32", 15, L(entries * 4), 15);
    a.vop1("v_mov_b32", 16, S(1));
    a.mubuf("buffer_store_dword", 16, 15, 4, 0, false);
    a.mubuf("buffer_store_dword", 12, 15, 4, 4, false);
    a.at("done");
    a.sop1("s_mov_b64", EXEC, S(26));
    a.vop1("v_mov_b32", 2, V(12));
    store_result(&mut a, 20, 2);
    a.sopp("s_endpgm", 0);
    a.finish()
}

/// Each group's count in `list`, as the console's waves take the rounds.
fn list_counts(groups: u32) -> Vec<u32> {
    (0..groups)
        .map(|g| {
            let mut n = 0;
            for r in 0..2 {
                for w in 0..4 {
                    let items: Vec<u32> = (64 * w..64 * w + 64)
                        .map(|t| 256 * r + (t ^ 32))
                        .filter(|&i| i < 324)
                        .collect();
                    let any = |k: usize| items.iter().any(|&i| [flags(g, i).0, flags(g, i).1][k]);
                    n += u32::from(any(0) && any(1));
                }
            }
            n
        })
        .collect()
}

#[test]
fn waves_count_tiles_by_any_and_any_and_append_a_list() {
    let groups = 16;
    let native = program(list(groups), [16, 16], 4);
    let counts = list_counts(groups);
    assert!(counts.iter().any(|&n| n != 0) && counts.iter().any(|&n| n != counts[0]), "{counts:?}");
    // (some group counts the second round's upper lanes alone)
    let upper = |g: u32| {
        let any = |k: usize| (320..324).any(|i| [flags(g, i).0, flags(g, i).1][k]);
        any(0) && any(1)
    };
    assert!((0..groups).any(upper) && !(0..groups).all(upper));
    let (counter, entries) = list_at(groups);
    each_paired("per-wave any and any, listed", &native, groups, entries + 2 * groups, |name, out| {
        for g in 0..groups as usize {
            assert!(out[g * 256..][..256].iter().all(|&n| n == counts[g]), "{name}: group {g}");
        }
        let listed: Vec<u32> = counts.iter().copied().filter(|&n| n != 0).collect();
        assert_eq!(out[counter as usize], listed.len() as u32, "{name}: list length");
        let mut got: Vec<(u32, u32)> = (0..listed.len())
            .map(|k| (out[entries as usize + 2 * k], out[entries as usize + 2 * k + 1]))
            .collect();
        got.sort_unstable();
        let mut want: Vec<(u32, u32)> = counts
            .iter()
            .enumerate()
            .filter(|(_, n)| **n != 0)
            .map(|(g, &n)| (g as u32, n))
            .collect();
        want.sort_unstable();
        assert_eq!(got, want, "{name}: the list");
    });
}

// ---- The forms: tried last, and only where nothing else translates ----

/// A program of `threads` lanes whose lane t gives `body`'s v2 (v0 = t in)
/// at dword g * threads + t, as `split_tests` builds them.
fn flat_program(body: &[u32], threads: u32) -> NativeProgram {
    let mut a = Asm::default();
    prologue(&mut a, threads);
    a.raw(body);
    store_result(&mut a, 0, 2);
    a.sopp("s_endpgm", 0);
    program(a.finish(), [threads, 1], 0)
}

/// Lane 0's v0 decides whether a wave reads lane 40 (`split_tests`' waves
/// that branch apart): no form of several waves per group in step takes it.
const APART: &[u32] = &[
    0xD760_0009,
    0x0001_0100, // v_readlane_b32 s9, v0, 0
    0xBF06_8009, // s_cmp_eq_u32 s9, 0
    0xBF85_0003, // s_cbranch_scc1 +3
    0xD760_0009,
    0x0001_5100, // v_readlane_b32 s9, v0, 40
    0xBF80_0000, // s_nop 0
    0x7E04_0209, // v_mov_b32 v2, s9
];

const NVIDIA: Target = Target {
    lanes_min: 32,
    lanes_max: 32,
    exact_views: false,
};

#[test]
fn paired_lanes_are_the_last_form_and_switch_off() {
    let apart = flat_program(APART, 128);
    // Refused by every other form, with the reason they gave.
    let off = with_switch(Switch::WavePairs, false, || emit_for(&apart, NVIDIA));
    let Err(crate::ShaderError::Unsupported { what, .. }) = off else {
        panic!("translated without paired lanes: {off:?}");
    };
    assert_eq!(what, "console wave64 lanes on a host without 64-lane waves");
    let on = emit_for(&apart, NVIDIA).expect("in paired lanes");
    assert_eq!(threads_of(&on.dxil), Some([64, 1, 1]), "a host wave of 32 for each console wave");
    assert_eq!(wave_size(&on.dxil), Some(32));
    // What other forms take keeps them: the console flow of one wave, the
    // halves of one wave, the per-lane form.
    for (body, threads) in [(APART, 64), (&[0x7E04_0300][..], 128)] {
        let p = flat_program(body, threads);
        let with = emit_for(&p, NVIDIA).expect("translates");
        let without = with_switch(Switch::WavePairs, false, || emit_for(&p, NVIDIA))
            .expect("translates");
        assert_eq!(with.dxil, without.dxil, "{threads} threads");
        assert_eq!(threads_of(&with.dxil), Some([threads, 1, 1]));
    }
    // A host of 64-lane waves runs the console's waves as they are.
    let rdna = Target {
        lanes_min: 32,
        lanes_max: 64,
        exact_views: false,
    };
    let native = emit_for(&apart, rdna).expect("64-lane waves");
    assert_eq!(threads_of(&native.dxil), Some([128, 1, 1]));
    assert_eq!(wave_size(&native.dxil), Some(64));
    // Version 0 leaves out groups that do not fill their console waves.
    let partial = flat_program(APART, 96);
    assert!(paired_for(&partial, NVIDIA).is_err());
}

/// The console flow's one-wave cases (`split_tests`), in paired lanes in
/// groups of one, two and four console waves: each wave computes its own.
#[test]
fn the_console_flow_cases_hold_in_paired_lanes() {
    // s_bcnt1 of the lanes below 40.
    let below = [0x7D88_00A8, 0xBE89_106A, 0x7E04_0209];
    // A whole-wave sum (`split_tests::WAVE_SUM`).
    let sum = [
        0xBE8A_25C1, 0xD501_0001, 0x002A_0080, 0x4A02_02FA, 0xFF01_6101, 0x4A02_02FA, 0xFF01_6201,
        0x4A02_02FA, 0xFF01_6401, 0x4A02_02FA, 0xFF01_6801, 0xD778_0006, 0x0305_8301, 0x4A02_0D01,
        0xD760_000C, 0x0001_0101, 0xD760_000D, 0x0001_4101, 0x800C_0D0C, 0xBEFE_040A, 0x7E04_020C,
    ];
    // v_readlane 40.
    let read = [0xD760_0009, 0x0001_5100, 0x7E04_0209];
    // A lane read inside a branch only lanes below 20 take.
    let branch = [
        0x7E04_0281, 0x7D88_0094, 0xBE8A_246A, 0xBF88_0003, 0xD760_0009, 0x0001_1500, 0x7E04_0209,
        0x88FE_0A7E,
    ];
    // A loop lane t runs t times, reading lane 40 each time.
    let looped = [
        0xBE89_0380, 0x7E04_0280, 0x7D8A_0080, 0xBE8A_246A, 0xBF88_0006, 0xD760_0009, 0x0001_5100,
        0x4A04_0481, 0x7D82_0102, 0x87FE_6A7E, 0xBF89_FFFA, 0x88FE_0A7E, 0x4A04_0409,
    ];
    // An exit mask grown from 0, counted after the loop.
    let exits = [
        0x4A02_0081, 0xBE8A_0480, 0xBE89_0380, 0xD760_0009, 0x0001_5100, 0x4E02_0281, 0x7D84_0280,
        0x888A_0A6A, 0x8AFE_0A7E, 0xBF89_FFF9, 0x88FE_0A7E, 0xBE8C_100A, 0x7E04_0209, 0x4A04_040C,
    ];
    let base = |t: u32| t & !63;
    let cases: [(&str, &[u32], &dyn Fn(u32) -> u32); 6] = [
        ("s_bcnt1 of lanes below 40", &below, &|t| if t < 64 { 40 } else { 0 }),
        ("WaveActiveSum(t)", &sum, &|t| (base(t)..base(t) + 64).sum()),
        ("v_readlane 40", &read, &|t| base(t) + 40),
        ("if (t < 20) readlane(10)", &branch, &|t| if t < 20 { 10 } else { 1 }),
        ("loop t times reading lane 40", &looped, &|t| t + base(t) + 40),
        ("exit-mask loop", &exits, &|t| base(t) + 40 + 64),
    ];
    for (label, body, want) in cases {
        for threads in [64, 128, 256] {
            let native = flat_program(body, threads);
            let groups = 3;
            let expect: Vec<u32> = (0..groups).flat_map(|_| (0..threads).map(want)).collect();
            let label = format!("{label}, {threads} threads");
            each_paired(&label, &native, groups, threads * groups, |name, out| {
                assert_eq!(out, &expect[..], "{name}: {label}");
            });
        }
    }
}

/// `v_readfirstlane_b32` reads the first lane in the console wave's EXEC: one
/// of the upper half where only lanes above 44 are on, and lane 0 where none
/// is.
#[test]
fn first_lanes_come_from_either_half() {
    for (none, label) in [(false, "lanes above 44"), (true, "no lane")] {
        let mut a = Asm::default();
        prologue(&mut a, 128);
        a.vop2("v_and_b32", 1, I(63), 0);
        a.vop1("v_mov_b32", 2, I(1));
        a.vopc("v_cmp_lt_u32", I(if none { 64 } else { 44 }), 1);
        a.sop1("s_and_saveexec_b64", 10, Src::Vcc);
        a.vop1("v_readfirstlane_b32", 9, V(0));
        a.vop1("v_mov_b32", 2, S(9));
        a.sop2("s_or_b64", EXEC, Src::Exec, S(10));
        if none {
            a.vop1("v_mov_b32", 2, S(9));
        }
        store_result(&mut a, 0, 2);
        a.sopp("s_endpgm", 0);
        let native = program(a.finish(), [128, 1], 0);
        let groups = 2;
        let want = |t: u32| match (none, t & 63 > 44) {
            (true, _) => t & !63,
            (false, true) => (t & !63) + 45,
            (false, false) => 1,
        };
        let expect: Vec<u32> = (0..groups).flat_map(|_| (0..128).map(want)).collect();
        each_paired(label, &native, groups, 128 * groups, |name, out| {
            assert_eq!(out, &expect[..], "{name}: {label}");
        });
    }
}

/// `split_tests`' waves that branch apart, run: the first wave keeps 0, the
/// second reads its lane 40.
#[test]
fn waves_that_branch_apart_run_in_paired_lanes() {
    let native = flat_program(APART, 128);
    let groups = 4;
    let expect: Vec<u32> = (0..groups)
        .flat_map(|_| (0..128).map(|t| if t < 64 { 0 } else { 104 }))
        .collect();
    each_paired("waves that branch apart", &native, groups, 128 * groups, |name, out| {
        assert_eq!(out, &expect[..], "{name}");
    });
}
