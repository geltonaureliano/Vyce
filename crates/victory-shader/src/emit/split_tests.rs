//! Console wave64 compute programs on hosts with and without 64-lane waves:
//! hand-assembled GFX10 code, dispatched on every hardware adapter, checked
//! lane by lane against what the console's lane layout gives. On a host
//! without 64-lane waves, programs whose lanes read only their own 32-lane
//! half run as two host waves of 32 (see `lower::Lower::new`).

use super::host::{Dispatch, Host, Root, View, adapters};
use super::tests::{compute, emit_for};
use crate::native::{RootParam, RootType, StageIo};

const S_ENDPGM: u32 = 0xBF81_0000;

/// The wave size a DXIL container's `PSV0` requires (minimum = maximum).
fn wave_size(dxil: &[u8]) -> Option<u32> {
    let c = crate::Container::parse(dxil).ok()?;
    let psv = c.part(crate::FourCC::PSV0)?.data;
    let u = |at: usize| crate::dxbc::u32_at(psv, at).ok();
    let (min, max) = (u(4 + 16)?, u(4 + 20)?);
    (min == max).then_some(min)
}

/// A program over `threads` x 1 x 1 lanes per group with a UAV table in
/// user SGPR 0 and the group id in s1: lane t of group g stores the
/// result of `body` (v0 = t in, v2 out) at dword g * threads + t.
fn program(body: &[u32], threads: u32) -> crate::NativeProgram {
    let mut words = vec![
        0xBE82_0300, // s_mov_b32 s2, s0
        0xBE83_0380, // s_mov_b32 s3, 0
        0xF408_0101,
        0xFA00_0000, // s_load_dwordx4 s[4:7], s[2:3], null
        0x9308_FF01,
        threads * 4, // s_mul_i32 s8, s1, threads * 4
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    ];
    words.extend_from_slice(body);
    words.extend_from_slice(&[
        0x3406_0082, // v_lshlrev_b32 v3, 2, v0
        0x4A06_0608, // v_add_nc_u32 v3, s8, v3
        0xE070_1000,
        0x8001_0203, // buffer_store_dword v2, v3, s[4:7], 0 offen
        S_ENDPGM,
    ]);
    let mut p = compute(&words, true);
    p.layout = vec![RootParam {
        root: RootType::Table,
        table_offset_dwords: 0,
        sgprs: 1,
        base_register: 0,
        user_data: 0,
        slot: 0,
    }];
    // One user SGPR, then the group id x.
    p.io = StageIo::Cs(crate::native::ComputeIo {
        rsrc2: 1 << 1 | 1 << 7,
        threads: [threads, 1, 1],
    });
    p
}

/// Swaps the 16-lane rows of each 32 (`ds_swizzle` xor 16), then mirrors
/// each row (DPP): lane t ends with t ^ 31, every read inside its half.
const IN_HALF: &[u32] = &[
    0xD8D4_401F,
    0x0100_0000, // ds_swizzle_b32 v1, v0 offset:swizzle(BITMASK_PERM, and 0x1f, xor 0x10)
    0x7E04_02FA,
    0xFF01_4001, // v_mov_b32_dpp v2, v1 row_mirror row_mask:0xf bank_mask:0xf
];

fn run(host: &Host, native: &crate::NativeProgram, groups: u32) -> Result<Vec<u32>, String> {
    let target = host.target();
    let cs = emit_for(native, target).map_err(|e| e.to_string())?;
    // The validator, where one is installed: drivers take some modules it
    // refuses.
    if let Ok(mut v) =
        super::validator::Validator::load(super::validator::Validator::default_path().as_deref())
    {
        v.validate(&mut cs.dxil.clone())
            .map_err(|e| format!("validation: {e}"))?;
    }
    let (root, pso) = host.compute_pipeline(&cs)?;
    let data = vec![0; (cs.user_data_dwords() as usize).max(1)];
    let StageIo::Cs(io) = &native.io else {
        unreachable!()
    };
    let n = io.threads.iter().product::<u32>() * groups;
    let views = [View::RawUav(n)];
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

#[test]
fn in_half_lane_reads_match_the_console_on_every_adapter() {
    let Ok(names) = adapters() else {
        eprintln!("no DXGI; skipped");
        return;
    };
    let mut ran = 0;
    for name in names {
        let Ok(host) = Host::on(&name) else {
            eprintln!("{name}: no SM 6.6 device; skipped");
            continue;
        };
        let target = host.target();
        // One console wave, two per group, and a group whose second wave
        // has its lower half only.
        for threads in [64, 128, 32] {
            let p = program(IN_HALF, threads);
            let cs = emit_for(&p, target).unwrap_or_else(|e| panic!("{name}: {e}"));
            let split = !target.runs(64);
            assert_eq!(
                wave_size(&cs.dxil),
                Some(if split { 32 } else { 64 }),
                "{name}: the declared wave size"
            );
            let groups = 3;
            let out = run(&host, &p, groups).unwrap_or_else(|e| panic!("{name}: {e}"));
            let want: Vec<u32> = (0..groups)
                .flat_map(|_| (0..threads).map(|t| t ^ 31))
                .collect();
            assert_eq!(out, want, "{name}: {threads} threads (split {split})");
            eprintln!("{name}: {threads} threads per group exact (split {split})");
        }
        ran += 1;
    }
    if ran == 0 {
        eprintln!("no hardware adapter; skipped");
    }
}

/// 3-D groups: the console forms waves from the flat thread id (x
/// fastest). Lane f (f = x + X * (y + Y * z)) stores f ^ 31 at dword f.
fn program_3d(threads: [u32; 3]) -> crate::NativeProgram {
    let [x, y, _] = threads;
    let (sx, sxy) = (x.trailing_zeros(), (x * y).trailing_zeros());
    assert!(x.is_power_of_two() && y.is_power_of_two());
    let words = [
        0xBE82_0300, // s_mov_b32 s2, s0
        0xBE83_0380, // s_mov_b32 s3, 0
        0xF408_0101,
        0xFA00_0000,               // s_load_dwordx4 s[4:7], s[2:3], null
        0xBF8C_C07F,               // s_waitcnt lgkmcnt(0)
        0x340A_0200 | (128 + sx),  // v_lshlrev_b32 v5, log2 X, v1
        0x340C_0400 | (128 + sxy), // v_lshlrev_b32 v6, log2 XY, v2
        0x4A08_0B00,               // v_add_nc_u32 v4, v0, v5
        0x4A08_0D04,               // v_add_nc_u32 v4, v4, v6
        0xD8D4_401F,
        0x0100_0004, // ds_swizzle_b32 v1, v4 offset:swizzle(BITMASK_PERM, and 0x1f, xor 0x10)
        0x7E04_02FA,
        0xFF01_4001, // v_mov_b32_dpp v2, v1 row_mirror
        0x3406_0882, // v_lshlrev_b32 v3, 2, v4
        0xE070_1000,
        0x8001_0203, // buffer_store_dword v2, v3, s[4:7], 0 offen
        S_ENDPGM,
    ];
    let mut p = compute(&words, true);
    p.layout = program(&[], 64).layout;
    // One user SGPR; thread ids x, y and z in v0..v2.
    p.io = StageIo::Cs(crate::native::ComputeIo {
        rsrc2: 1 << 1 | 2 << 11,
        threads,
    });
    p
}

#[test]
fn three_dimensional_groups_keep_the_consoles_lanes() {
    let Ok(names) = adapters() else {
        eprintln!("no DXGI; skipped");
        return;
    };
    for name in names {
        let Ok(host) = Host::on(&name) else {
            continue;
        };
        let split = !host.target().runs(64);
        for threads in [
            [4, 4, 4],
            [8, 8, 2],
            [2, 4, 8],
            [16, 2, 4],
            [8, 8, 4],
            [4, 8, 8],
            // Shapes the title uses most, and rows longer than NVIDIA's
            // waves are wide.
            [8, 8, 1],
            [16, 16, 1],
            [8, 16, 1],
            [16, 8, 1],
            [4, 16, 1],
            [32, 2, 1],
            [32, 32, 1],
            [64, 4, 2],
        ] {
            let p = program_3d(threads);
            let n: u32 = threads.iter().product();
            let out = run(&host, &p, 1).unwrap_or_else(|e| panic!("{name}: {e}"));
            let want: Vec<u32> = (0..n).map(|f| f ^ 31).collect();
            assert_eq!(out, want, "{name}: {threads:?} (split {split})");
            eprintln!("{name}: {threads:?} exact (split {split})");
        }
    }
}

/// Runs `body` (v0 = t in, v2 out) on every hardware adapter in groups of
/// `threads` and checks lane t against `want(t)`; on a host without
/// 64-lane waves it must have translated in the console flow.
fn console_flow_case(label: &str, body: &[u32], threads: u32, want: impl Fn(u32) -> u32) {
    flow_case(label, body, threads, want, true);
}

/// [`console_flow_case`], checking hosts with 64-lane waves (the per-lane
/// form) only when `native` says.
fn flow_case(label: &str, body: &[u32], threads: u32, want: impl Fn(u32) -> u32, native: bool) {
    let Ok(names) = adapters() else {
        eprintln!("no DXGI; skipped");
        return;
    };
    for name in names {
        let Ok(host) = Host::on(&name) else {
            continue;
        };
        let p = program(body, threads);
        let split = !host.target().runs(64);
        let groups = 2;
        let out = run(&host, &p, groups).unwrap_or_else(|e| panic!("{name}: {label}: {e}"));
        let expect: Vec<u32> = (0..groups).flat_map(|_| (0..threads).map(&want)).collect();
        if !split && !native {
            if out != expect {
                eprintln!("{name}: {label}: the per-lane form differs (known)");
            }
            continue;
        }
        assert_eq!(
            out, expect,
            "{name}: {label}, {threads} threads (split {split})"
        );
        eprintln!("{name}: {label}, {threads} threads exact (split {split})");
    }
}

/// Lane permutations inside halves of 32 (`v_permlane16`, `v_permlanex16`)
/// need nothing from the other host wave.
#[test]
fn lane_permutations_stay_in_their_half() {
    let select = |lo: u32, hi: u32| [0xBE94_03FF, lo, 0xBE95_03FF, hi];
    // v_permlanex16_b32 v2, v0, s20, s21 with each lane's own nibble: the
    // other row's lane.
    let mut body = select(0x7654_3210, 0xFEDC_BA98).to_vec();
    body.extend([0xD778_0002, 0x0054_2900]);
    flow_case("permlanex16", &body, 64, |t| t ^ 16, true);
    // v_permlane16_b32 v2, v0, s20, s21 reversing each row.
    let mut body = select(0x89AB_CDEF, 0x0123_4567).to_vec();
    body.extend([0xD777_0002, 0x0054_2900]);
    flow_case("permlane16", &body, 64, |t| (t & !15) | (15 - (t & 15)), true);
}

/// A mask counted whole: 40 lanes of the console wave, 32 in one half and
/// 8 in the other.
#[test]
fn masks_read_whole_count_both_halves() {
    let body = [
        0x7D88_00A8, // v_cmp_gt_u32 vcc, 40, v0
        0xBE89_106A, // s_bcnt1_i32_b64 s9, vcc
        0x7E04_0209, // v_mov_b32 v2, s9
    ];
    console_flow_case("s_bcnt1 of lanes below 40", &body, 64, |_| 40);
    // 48 threads: the upper half is partial.
    console_flow_case("s_bcnt1 of lanes below 40", &body, 48, |_| 40);
    // 32 threads: no upper half at all.
    console_flow_case("s_bcnt1 of lanes below 40", &body, 32, |_| 32);
}

/// A whole-wave section summing `v0` (as the console compiler expands
/// `WaveActiveSum` for wave64): each half of 32 lanes with DPP and
/// `v_permlanex16`, then lanes 0 and 32 read and added. The sum is in s12.
const WAVE_SUM: &[u32] = &[
    0xBE8A_25C1, // s_or_saveexec_b64 s[10:11], -1
    0xD501_0001,
    0x002A_0080, // v_cndmask_b32_e64 v1, 0, v0, s[10:11]
    0x4A02_02FA,
    0xFF01_6101, // v_add_nc_u32_dpp v1, v1, v1 row_xmask:1
    0x4A02_02FA,
    0xFF01_6201, // v_add_nc_u32_dpp v1, v1, v1 row_xmask:2
    0x4A02_02FA,
    0xFF01_6401, // v_add_nc_u32_dpp v1, v1, v1 row_xmask:4
    0x4A02_02FA,
    0xFF01_6801, // v_add_nc_u32_dpp v1, v1, v1 row_xmask:8
    0xD778_0006,
    0x0305_8301, // v_permlanex16_b32 v6, v1, -1, -1
    0x4A02_0D01, // v_add_nc_u32_e32 v1, v1, v6
    0xD760_000C,
    0x0001_0101, // v_readlane_b32 s12, v1, 0
    0xD760_000D,
    0x0001_4101, // v_readlane_b32 s13, v1, 32
    0x800C_0D0C, // s_add_u32 s12, s12, s13
    0xBEFE_040A, // s_mov_b64 exec, s[10:11]
];

/// A reduction over the console wave, in two host waves: each reduces its
/// half and they put the two together.
#[test]
fn wave_sums_cover_both_halves() {
    let mut body = WAVE_SUM.to_vec();
    body.push(0x7E04_020C); // v_mov_b32 v2, s12
    console_flow_case("WaveActiveSum(t)", &body, 64, |_| 2016);
    // The upper half is partial, or not there.
    console_flow_case("WaveActiveSum(t)", &body, 48, |_| 1128);
    console_flow_case("WaveActiveSum(t)", &body, 32, |_| 496);
}

/// The same inside a branch only lanes 10 to 49 take: the others bring
/// nothing, and lanes of both halves get the sum.
#[test]
fn wave_sums_in_a_branch_count_its_lanes() {
    let mut body = vec![
        0x7E04_0281, // v_mov_b32 v2, 1
        0x4A0E_00CA, // v_add_nc_u32 v7, -10, v0
        0x7D88_0EA8, // v_cmp_gt_u32 vcc, 40, v7
        0xBE8E_246A, // s_and_saveexec_b64 s[14:15], vcc
        0xBF88_0015, // s_cbranch_execz +21
    ];
    body.extend_from_slice(WAVE_SUM);
    body.push(0x7E04_020C); // v_mov_b32 v2, s12
    body.push(0x88FE_0E7E); // s_or_b64 exec, exec, s[14:15]
    console_flow_case("if (10 <= t < 50) WaveActiveSum(t)", &body, 64, |t| {
        if (10..50).contains(&t) { 1180 } else { 1 }
    });
}

/// A prefix sum: rows with `row_shr` steps, each 32 with `v_permlanex16`,
/// the upper half with lane 31; then moved up by one lane (exclusive).
#[test]
fn wave_prefix_sums_continue_into_the_upper_half() {
    let body = [
        0xBE8A_25C1, // s_or_saveexec_b64 s[10:11], -1
        0xD501_0001,
        0x002A_0080, // v_cndmask_b32_e64 v1, 0, v0, s[10:11]
        0x4A02_02FA,
        0xFF01_1101, // v_add_nc_u32_dpp v1, v1, v1 row_shr:1
        0x4A02_02FA,
        0xFF01_1201, // v_add_nc_u32_dpp v1, v1, v1 row_shr:2
        0x4A02_02FA,
        0xFF01_1401, // v_add_nc_u32_dpp v1, v1, v1 row_shr:4
        0x4A02_02FA,
        0xFF01_1801, // v_add_nc_u32_dpp v1, v1, v1 row_shr:8
        0xD778_0006,
        0x0305_8301, // v_permlanex16_b32 v6, v1, -1, -1
        0x7E0E_0280, // v_mov_b32_e32 v7, 0
        0x7E0E_02FA,
        0xAF00_E406, // v_mov_b32_dpp v7, v6 quad_perm:[0,1,2,3] row_mask:0xa
        0x4A02_0F01, // v_add_nc_u32_e32 v1, v1, v7
        0xD760_000C,
        0x0001_3F01, // v_readlane_b32 s12, v1, 31
        0x7E10_020C, // v_mov_b32_e32 v8, s12
        0x7E0E_0280, // v_mov_b32_e32 v7, 0
        0x7E0E_02FA,
        0xCF00_E408, // v_mov_b32_dpp v7, v8 quad_perm:[0,1,2,3] row_mask:0xc
        0x4A02_0F01, // v_add_nc_u32_e32 v1, v1, v7
        0x7E12_0280, // v_mov_b32_e32 v9, 0
        0x7E12_02FA,
        0xFF01_1101, // v_mov_b32_dpp v9, v1 row_shr:1
        0xD760_000D,
        0x0001_1F01, // v_readlane_b32 s13, v1, 15
        0xD761_0009,
        0x0001_200D, // v_writelane_b32 v9, s13, 16
        0xD760_000D,
        0x0001_3F01, // v_readlane_b32 s13, v1, 31
        0xD761_0009,
        0x0001_400D, // v_writelane_b32 v9, s13, 32
        0xD760_000D,
        0x0001_5F01, // v_readlane_b32 s13, v1, 47
        0xD761_0009,
        0x0001_600D, // v_writelane_b32 v9, s13, 48
        0xBEFE_040A, // s_mov_b64 exec, s[10:11]
        0x7E04_0309, // v_mov_b32 v2, v9
    ];
    console_flow_case("WavePrefixSum(t)", &body, 64, |t| t * t.saturating_sub(1) / 2);
    console_flow_case("WavePrefixSum(t)", &body, 48, |t| t * t.saturating_sub(1) / 2);
}

/// A lane of the other half: lane 40 is the upper half's lane 8.
#[test]
fn lane_reads_cross_halves() {
    let body = [
        0xD760_0009,
        0x0001_5100, // v_readlane_b32 s9, v0, 40
        0x7E04_0209, // v_mov_b32 v2, s9
    ];
    console_flow_case("v_readlane 40", &body, 64, |_| 40);
}

/// A branch only lanes below 20 take (all in the lower half), with a lane
/// read inside: the upper half has no lane there but still meets the lower
/// one at the exchange.
#[test]
fn exchanges_inside_a_branch_one_half_skips() {
    let body = [
        0x7E04_0281, // v_mov_b32 v2, 1
        0x7D88_0094, // v_cmp_gt_u32 vcc, 20, v0
        0xBE8A_246A, // s_and_saveexec_b64 s[10:11], vcc
        0xBF88_0003, // s_cbranch_execz +3
        0xD760_0009,
        0x0001_1500, // v_readlane_b32 s9, v0, 10
        0x7E04_0209, // v_mov_b32 v2, s9
        0x88FE_0A7E, // s_or_b64 exec, exec, s[10:11]
    ];
    console_flow_case("if (lane < 20) v2 = readlane(10)", &body, 64, |t| {
        if t < 20 { 10 } else { 1 }
    });
}

// v1 = thread index in bytes; gather from the next lane, wrapping at 32.
const GATHER_NEXT: &[u32] = &[
    0x3402_0082, // v_lshlrev_b32 v1, 2, v0
    0xDACC_0004,
    0x0200_0001, // ds_bpermute_b32 v2, v1, v0 offset:4
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
];

#[test]
fn backward_permute_wraps_in_each_half() {
    let nvidia = super::Target {
        lanes_min: 32,
        lanes_max: 32,
        exact_views: false,
    };
    for threads in [32, 48, 64, 128] {
        let cs = emit_for(&program(GATHER_NEXT, threads), nvidia).expect("half-wave gather");
        assert_eq!(wave_size(&cs.dxil), Some(32));
        console_flow_case("ds_bpermute within each half", GATHER_NEXT, threads, |t| {
            let source = (t & !31) | ((t + 1) & 31);
            if source < threads { source } else { 0 }
        });
    }
}

#[test]
fn backward_permute_zeros_inactive_sources() {
    let mut body = vec![
        0x7E04_0281, // v_mov_b32 v2, 1
        0x7D88_0094, // v_cmp_gt_u32 vcc, 20, v0
        0xBE8A_246A, // s_and_saveexec_b64 s[10:11], vcc
        0xBF88_0004, // s_cbranch_execz +4
    ];
    body.extend_from_slice(GATHER_NEXT);
    body.push(0x88FE_0A7E); // s_or_b64 exec, exec, s[10:11]
    console_flow_case("ds_bpermute with an EXEC mask", &body, 64, |t| match t {
        0..=18 => t + 1,
        19 => 0,
        _ => 1,
    });
}

#[test]
fn lds_reads_cross_halves_inside_a_masked_branch() {
    let body = [
        0x3402_0082, // v_lshlrev_b32 v1, 2, v0
        0xD834_0000,
        0x0000_0001, // ds_write_b32 v1, v0
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0), with no console group barrier
        0x7E04_0281, // v_mov_b32 v2, 1
        0x7D88_0094, // v_cmp_gt_u32 vcc, 20, v0
        0xBE8A_246A, // s_and_saveexec_b64 s[10:11], vcc
        0xBF88_0005, // s_cbranch_execz +5
        0x4A02_00A0, // v_add_nc_u32 v1, 32, v0
        0x3402_0282, // v_lshlrev_b32 v1, 2, v1
        0xD8D8_0000,
        0x0200_0001, // ds_read_b32 v2, v1
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0x88FE_0A7E, // s_or_b64 exec, exec, s[10:11]
    ];
    let mut p = program(&body, 64);
    let StageIo::Cs(io) = &mut p.io else {
        unreachable!()
    };
    io.rsrc2 |= 1 << 15; // 512 bytes of group-shared memory.
    let nvidia = super::Target {
        lanes_min: 32,
        lanes_max: 32,
        exact_views: false,
    };
    let cs = emit_for(&p, nvidia).expect("one group split into two 32-lane waves");
    assert_eq!(wave_size(&cs.dxil), Some(32));

    for name in adapters().unwrap_or_default() {
        let Ok(host) = Host::on(&name) else {
            continue;
        };
        // This test exercises the fallback's barriers. Use an ordinary
        // native Wave64 program on other adapters.
        if host.target().runs(64) {
            continue;
        }
        let groups = 8;
        let out = run(&host, &p, groups).unwrap_or_else(|e| panic!("{name}: {e}"));
        let expected: Vec<u32> = (0..groups)
            .flat_map(|_| (0..64).map(|t| if t < 20 { t + 32 } else { 1 }))
            .collect();
        assert_eq!(out, expected, "{name}: masked cross-half LDS reads");
        eprintln!("{name}: masked cross-half LDS reads exact");
    }
}

/// Groups of several console waves: each wave's halves exchange through
/// dwords of their own.
#[test]
fn waves_of_a_group_exchange_apart() {
    let read = [
        0xD760_0009,
        0x0001_5100, // v_readlane_b32 s9, v0, 40
        0x7E04_0209, // v_mov_b32 v2, s9
    ];
    for threads in [128, 192, 256, 1024] {
        console_flow_case("v_readlane 40 of each wave", &read, threads, |t| (t & !63) + 40);
    }
    let mut sum = WAVE_SUM.to_vec();
    sum.push(0x7E04_020C); // v_mov_b32 v2, s12
    let total = |t: u32, threads: u32| {
        let from = t & !63;
        (from..(from + 64).min(threads)).sum::<u32>()
    };
    for threads in [128, 160, 256, 320, 1024] {
        console_flow_case("WaveActiveSum(t) of each wave", &sum, threads, |t| total(t, threads));
    }
}

/// A branch only the lanes below 80 take: the second of four waves in part,
/// the last two not at all, and all of them meet at the exchanges inside.
#[test]
fn waves_of_a_group_run_branches_in_step() {
    let mut body = vec![
        0x7E04_0281, // v_mov_b32 v2, 1
        0x7D88_00FF,
        0x0000_0050, // v_cmp_gt_u32 vcc, 80, v0
        0xBE8E_246A, // s_and_saveexec_b64 s[14:15], vcc
        0xBF88_0015, // s_cbranch_execz +21
    ];
    body.extend_from_slice(WAVE_SUM);
    body.push(0x7E04_020C); // v_mov_b32 v2, s12
    body.push(0x88FE_0E7E); // s_or_b64 exec, exec, s[14:15]
    console_flow_case("if (t < 80) WaveActiveSum(t)", &body, 256, |t| match t {
        0..64 => 2016,
        64..80 => (64..80).sum(),
        _ => 1,
    });
}

/// A loop a lane runs (t / 16) times: waves leave it at different turns.
#[test]
fn waves_of_a_group_loop_in_step() {
    let body = [
        0x2C02_0084, // v_lshrrev_b32 v1, 4, v0
        0x7E04_0280, // v_mov_b32 v2, 0
        0x7D8A_0280, // v_cmp_ne_u32 vcc, 0, v1
        0xBE8A_246A, // s_and_saveexec_b64 s[10:11], vcc
        0xBF88_0006, // s_cbranch_execz end
        // loop:
        0xD760_0009,
        0x0001_5100, // v_readlane_b32 s9, v0, 40
        0x4A04_0481, // v_add_nc_u32 v2, 1, v2
        0x7D82_0302, // v_cmp_lt_u32 vcc, v2, v1
        0x87FE_6A7E, // s_and_b64 exec, exec, vcc
        0xBF89_FFFA, // s_cbranch_execnz loop
        // end:
        0x88FE_0A7E, // s_or_b64 exec, exec, s[10:11]
    ];
    console_flow_case("loop t / 16 times", &body, 256, |t| t / 16);
}

/// A scalar branch on what differs between waves keeps a program of several
/// waves per group out of the lockstep form.
#[test]
fn waves_that_branch_apart_do_not_run_in_step() {
    let body = [
        0xD760_0009,
        0x0001_0100, // v_readlane_b32 s9, v0, 0
        0xBF06_8009, // s_cmp_eq_u32 s9, 0
        0xBF85_0003, // s_cbranch_scc1 +3
        0xD760_0009,
        0x0001_5100, // v_readlane_b32 s9, v0, 40
        0xBF80_0000, // s_nop 0
        0x7E04_0209, // v_mov_b32 v2, s9
    ];
    let nvidia = super::Target {
        lanes_min: 32,
        lanes_max: 32,
        exact_views: false,
    };
    assert!(emit_for(&program(&body, 64), nvidia).is_ok());
    assert!(emit_for(&program(&body, 128), nvidia).is_err());
}

/// A loop lane t runs t times, with a lane read of the other half in
/// every iteration: the lower half's lanes are done after 31 iterations,
/// the upper half's go on to 63, and the console wave loops while any lane
/// does. After it, s9 holds lane 40's id, as the last iteration read it.
///
/// The per-lane form got this wrong on every host (#225); hosts with
/// 64-lane waves now run it in the console flow too, without exchanges.
#[test]
fn loops_run_while_any_lane_of_the_console_wave_does() {
    let body = [
        0xBE89_0380, // s_mov_b32 s9, 0
        0x7E04_0280, // v_mov_b32 v2, 0
        0x7D8A_0080, // v_cmp_ne_u32 vcc, 0, v0
        0xBE8A_246A, // s_and_saveexec_b64 s[10:11], vcc
        0xBF88_0006, // s_cbranch_execz end
        // loop:
        0xD760_0009,
        0x0001_5100, // v_readlane_b32 s9, v0, 40
        0x4A04_0481, // v_add_nc_u32 v2, 1, v2
        0x7D82_0102, // v_cmp_lt_u32 vcc, v2, v0
        0x87FE_6A7E, // s_and_b64 exec, exec, vcc
        0xBF89_FFFA, // s_cbranch_execnz loop
        // end:
        0x88FE_0A7E, // s_or_b64 exec, exec, s[10:11]
        0x4A04_0409, // v_add_nc_u32 v2, s9, v2
    ];
    console_flow_case("loop t times reading lane 40", &body, 64, |t| t + 40);
}

/// DPP's row mask names the console's rows: with `row_mask:0x3` only
/// rows 0 and 1 (lanes 0 to 31) write, so the upper half keeps its value
/// although its host rows are 0 and 1 too.
#[test]
fn dpp_row_masks_name_console_rows() {
    let body = [
        0x7E04_0281, // v_mov_b32 v2, 1
        0x7E04_02FA,
        0x3F00_E400, // v_mov_b32_dpp v2, v0 quad_perm:[0,1,2,3] row_mask:0x3 bank_mask:0xf
    ];
    console_flow_case(
        "DPP row_mask 0x3",
        &body,
        64,
        |t| if t < 32 { t } else { 1 },
    );
}

/// `v_writelane` ignores EXEC: inside a branch only lanes below 20 take,
/// it still writes lane 40. The per-lane form never had lane 40 there.
#[test]
fn lane_writes_reach_lanes_outside_the_branch() {
    let body = [
        0x7E04_0281, // v_mov_b32 v2, 1
        0x7D88_0094, // v_cmp_gt_u32 vcc, 20, v0
        0xBE8A_246A, // s_and_saveexec_b64 s[10:11], vcc
        0xBF88_0004, // s_cbranch_execz +4
        0xBE89_03FF,
        0x0000_004D, // s_mov_b32 s9, 77
        0xD761_0002,
        0x0001_5009, // v_writelane_b32 v2, s9, 40
        0x88FE_0A7E, // s_or_b64 exec, exec, s[10:11]
    ];
    console_flow_case("if (lane < 20) writelane(40, 77)", &body, 64, |t| {
        if t == 40 { 77 } else { 1 }
    });
}

/// The loop form compilers emit: lanes that are done are added to an exit
/// mask that starts as 0, EXEC loses them, and the mask restores EXEC
/// after the loop. Lane t runs t + 1 iterations with a read of lane 40 in
/// each; afterwards every lane has s9 = 40 and counts all 64 lanes in the
/// exit mask.
#[test]
fn loops_with_an_exit_mask_grown_from_zero() {
    let body = [
        0x4A02_0081, // v_add_nc_u32 v1, 1, v0
        0xBE8A_0480, // s_mov_b64 s[10:11], 0
        0xBE89_0380, // s_mov_b32 s9, 0
        // loop:
        0xD760_0009,
        0x0001_5100, // v_readlane_b32 s9, v0, 40
        0x4E02_0281, // v_subrev_nc_u32 v1, 1, v1
        0x7D84_0280, // v_cmp_eq_u32 vcc, 0, v1
        0x888A_0A6A, // s_or_b64 s[10:11], vcc, s[10:11]
        0x8AFE_0A7E, // s_andn2_b64 exec, exec, s[10:11]
        0xBF89_FFF9, // s_cbranch_execnz loop
        0x88FE_0A7E, // s_or_b64 exec, exec, s[10:11]
        0xBE8C_100A, // s_bcnt1_i32_b64 s12, s[10:11]
        0x7E04_0209, // v_mov_b32 v2, s9
        0x4A04_040C, // v_add_nc_u32 v2, s12, v2
    ];
    console_flow_case("exit-mask loop reading lane 40", &body, 64, |_| 40 + 64);
}

/// [`run`] with a raw UAV of `dwords` dwords (at least the lanes' results).
fn run_dwords(host: &Host, cs: &super::HostShader, threads: u32, groups: u32, dwords: u32) -> Result<Vec<u32>, String> {
    let (root, pso) = host.compute_pipeline(cs)?;
    let data = vec![0; (cs.user_data_dwords() as usize).max(1)];
    let views = [View::RawUav(dwords.max(threads * groups))];
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

/// A one-wave group's LDS shared between its lanes without barriers, as the
/// console compiler leaves it: every lane writes LDS[0] and LDS[1], adds
/// t + 1 to LDS[0], the lanes below 40 (both halves of the wave) take the
/// greatest t into LDS[1] inside a branch, and every lane reads both after
/// it: 2080 + 39.
const LDS_SHARED: &[u32] = &[
    0x7E02_0280, // v_mov_b32 v1, 0
    0x7E08_0280, // v_mov_b32 v4, 0
    0xD834_0000,
    0x0000_0401, // ds_write_b32 v1, v4
    0xD834_0004,
    0x0000_0401, // ds_write_b32 v1, v4 offset:4
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0x4A06_0081, // v_add_nc_u32 v3, 1, v0
    0xD800_0000,
    0x0000_0301, // ds_add_u32 v1, v3
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0x7D88_00A8, // v_cmp_gt_u32 vcc, 40, v0
    0xBE8A_246A, // s_and_saveexec_b64 s[10:11], vcc
    0xBF88_0003, // s_cbranch_execz +3
    0xD820_0004,
    0x0000_0001, // ds_max_u32 v1, v0 offset:4
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0x88FE_0A7E, // s_or_b64 exec, exec, s[10:11]
    0xD8D8_0000,
    0x0200_0001, // ds_read_b32 v2, v1
    0xD8D8_0004,
    0x0500_0001, // ds_read_b32 v5, v1 offset:4
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0x4A04_0B02, // v_add_nc_u32 v2, v2, v5
];

/// The allocation idiom of the title's culls and tile classifications in a
/// one-wave group with LDS: the odd lanes count themselves (`s_bcnt1`), the
/// first of them (`s_ff1` against its own number) reserves that many slots
/// with an atomic add on a counter at dword 4096, every odd lane writes its
/// t at the reserved base plus its rank among them (`v_mbcnt` of EXEC) into
/// the list from dword 4097 and adds 1 to LDS[0], and every lane reads
/// LDS[0] after the branch: 32.
const LDS_IDIOM: &[u32] = &[
    0x7E02_0280, // v_mov_b32 v1, 0
    0x7E08_0280, // v_mov_b32 v4, 0
    0xD834_0000,
    0x0000_0401, // ds_write_b32 v1, v4
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0x360A_0081, // v_and_b32 v5, 1, v0
    0x7D8A_0A80, // v_cmp_ne_u32 vcc, 0, v5
    0xBE8E_246A, // s_and_saveexec_b64 s[14:15], vcc
    0xBF88_001E, // s_cbranch_execz +30
    0xBE8C_107E, // s_bcnt1_i32_b64 s12, exec
    0xD765_0006,
    0x0001_00C1, // v_mbcnt_lo_u32_b32 v6, -1, 0
    0xD766_0006,
    0x0002_0CC1, // v_mbcnt_hi_u32_b32 v6, -1, v6
    0xD765_0007,
    0x0001_007E, // v_mbcnt_lo_u32_b32 v7, exec_lo, 0
    0xD766_0007,
    0x0002_0E7F, // v_mbcnt_hi_u32_b32 v7, exec_hi, v7
    0xBE8D_147E, // s_ff1_i32_b64 s13, exec
    0x7E12_02FF,
    0x0000_4000, // v_mov_b32 v9, 0x4000
    0x7E10_020C, // v_mov_b32 v8, s12
    0xBE90_047E, // s_mov_b64 s[16:17], exec
    0x7DA4_0C0D, // v_cmpx_eq_u32 exec, s13, v6
    0xE0C8_5000,
    0x8001_0809, // buffer_atomic_add v8, v9, s[4:7], 0 offen glc
    0xBF8C_3F70, // s_waitcnt vmcnt(0)
    0xBEFE_0410, // s_mov_b64 exec, s[16:17]
    0x7E24_0508, // v_readfirstlane_b32 s18, v8
    0x4A14_0E12, // v_add_nc_u32 v10, s18, v7
    0x3416_1482, // v_lshlrev_b32 v11, 2, v10
    0x4A16_16FF,
    0x0000_4004, // v_add_nc_u32 v11, 0x4004, v11
    0xE070_1000,
    0x8001_000B, // buffer_store_dword v0, v11, s[4:7], 0 offen
    0x7E18_0281, // v_mov_b32 v12, 1
    0xD800_0000,
    0x0000_0C01, // ds_add_u32 v1, v12
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0x88FE_0E7E, // s_or_b64 exec, exec, s[14:15]
    0xD8D8_0000,
    0x0200_0001, // ds_read_b32 v2, v1
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
];

/// The idiom through whole-wave sections, as the title's tile
/// classifications have it: every lane adds t to LDS[0], an exclusive prefix
/// sum of t + 1 (v9) and the sum of t + 1 (s12) over the wave, the first
/// lane reserves the sum at the counter (dword 4096), every lane writes t + 1
/// at the base plus its prefix in the list from dword 4097, and reads
/// LDS[0]: 2016.
fn lds_sections() -> Vec<u32> {
    let mut words = vec![
        0x7E18_0280, // v_mov_b32 v12, 0
        0xD834_0000,
        0x0000_0C0C, // ds_write_b32 v12, v12
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xD800_0000,
        0x0000_000C, // ds_add_u32 v12, v0
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0x7E1A_0300, // v_mov_b32 v13, v0
        0x4A00_0081, // v_add_nc_u32 v0, 1, v0
        // WavePrefixSum(t + 1) into v9, as `wave_prefix_sums_continue_into_the_upper_half`.
        0xBE8A_25C1, // s_or_saveexec_b64 s[10:11], -1
        0xD501_0001,
        0x002A_0080, // v_cndmask_b32_e64 v1, 0, v0, s[10:11]
        0x4A02_02FA,
        0xFF01_1101, // v_add_nc_u32_dpp v1, v1, v1 row_shr:1
        0x4A02_02FA,
        0xFF01_1201, // v_add_nc_u32_dpp v1, v1, v1 row_shr:2
        0x4A02_02FA,
        0xFF01_1401, // v_add_nc_u32_dpp v1, v1, v1 row_shr:4
        0x4A02_02FA,
        0xFF01_1801, // v_add_nc_u32_dpp v1, v1, v1 row_shr:8
        0xD778_0006,
        0x0305_8301, // v_permlanex16_b32 v6, v1, -1, -1
        0x7E0E_0280, // v_mov_b32_e32 v7, 0
        0x7E0E_02FA,
        0xAF00_E406, // v_mov_b32_dpp v7, v6 quad_perm:[0,1,2,3] row_mask:0xa
        0x4A02_0F01, // v_add_nc_u32_e32 v1, v1, v7
        0xD760_000C,
        0x0001_3F01, // v_readlane_b32 s12, v1, 31
        0x7E10_020C, // v_mov_b32_e32 v8, s12
        0x7E0E_0280, // v_mov_b32_e32 v7, 0
        0x7E0E_02FA,
        0xCF00_E408, // v_mov_b32_dpp v7, v8 quad_perm:[0,1,2,3] row_mask:0xc
        0x4A02_0F01, // v_add_nc_u32_e32 v1, v1, v7
        0x7E12_0280, // v_mov_b32_e32 v9, 0
        0x7E12_02FA,
        0xFF01_1101, // v_mov_b32_dpp v9, v1 row_shr:1
        0xD760_000D,
        0x0001_1F01, // v_readlane_b32 s13, v1, 15
        0xD761_0009,
        0x0001_200D, // v_writelane_b32 v9, s13, 16
        0xD760_000D,
        0x0001_3F01, // v_readlane_b32 s13, v1, 31
        0xD761_0009,
        0x0001_400D, // v_writelane_b32 v9, s13, 32
        0xD760_000D,
        0x0001_5F01, // v_readlane_b32 s13, v1, 47
        0xD761_0009,
        0x0001_600D, // v_writelane_b32 v9, s13, 48
        0xBEFE_040A, // s_mov_b64 exec, s[10:11]
    ];
    words.extend_from_slice(WAVE_SUM); // WaveActiveSum(t + 1) into s12
    words.extend_from_slice(&[
        0xD765_000E,
        0x0001_00C1, // v_mbcnt_lo_u32_b32 v14, -1, 0
        0xD766_000E,
        0x0002_1CC1, // v_mbcnt_hi_u32_b32 v14, -1, v14
        0xBE8F_147E, // s_ff1_i32_b64 s15, exec
        0x7E10_020C, // v_mov_b32 v8, s12
        0x7E1E_02FF,
        0x0000_4000, // v_mov_b32 v15, 0x4000
        0xBE90_047E, // s_mov_b64 s[16:17], exec
        0x7DA4_1C0F, // v_cmpx_eq_u32 exec, s15, v14
        0xE0C8_5000,
        0x8001_080F, // buffer_atomic_add v8, v15, s[4:7], 0 offen glc
        0xBF8C_3F70, // s_waitcnt vmcnt(0)
        0xBEFE_0410, // s_mov_b64 exec, s[16:17]
        0x7E24_0508, // v_readfirstlane_b32 s18, v8
        0x4A14_1212, // v_add_nc_u32 v10, s18, v9
        0x3416_1482, // v_lshlrev_b32 v11, 2, v10
        0x4A16_16FF,
        0x0000_4004, // v_add_nc_u32 v11, 0x4004, v11
        0xE070_1000,
        0x8001_000B, // buffer_store_dword v0, v11, s[4:7], 0 offen
        0x7E00_030D, // v_mov_b32 v0, v13
        0xD8D8_0000,
        0x0200_000C, // ds_read_b32 v2, v12
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    ]);
    words
}

/// [`program`] of 64 threads with 512 bytes of LDS.
fn with_lds(body: &[u32]) -> crate::NativeProgram {
    let mut p = program(body, 64);
    p.vgprs = 16;
    let StageIo::Cs(io) = &mut p.io else {
        unreachable!()
    };
    io.rsrc2 |= 1 << 15; // 512 bytes of group-shared memory.
    p
}

/// One-wave groups with LDS as two local 32-lane waves that meet around
/// their LDS accesses (`VICTORY_LDS_LOCAL`, VIctory-7900XTX's local waves of
/// such groups): every lane's result as in the console flow, the idiom's
/// slots the same ones in another order; and the forms differ.
#[test]
fn local_waves_of_one_wave_groups_keep_their_lds_order() {
    let nvidia = super::Target { lanes_min: 32, lanes_max: 32, exact_views: false };
    let local = |p: &crate::NativeProgram| emit_for(p, nvidia).expect("local waves");
    let console = |p: &crate::NativeProgram| {
        super::with_switch(super::Switch::LdsLocal, false, || emit_for(p, nvidia)).expect("console flow")
    };
    let (shared, idiom, sections) = (with_lds(LDS_SHARED), with_lds(LDS_IDIOM), with_lds(&lds_sections()));
    for p in [&shared, &idiom, &sections] {
        let (l, c) = (local(p), console(p));
        assert_eq!(wave_size(&l.dxil), Some(32));
        assert_ne!(l.dxil, c.dxil, "local waves are not the console flow");
        if let Ok(mut v) =
            super::validator::Validator::load(super::validator::Validator::default_path().as_deref())
        {
            v.validate(&mut l.dxil.clone()).expect("local waves validate");
        }
    }
    for name in adapters().unwrap_or_default() {
        let Ok(host) = Host::on(&name) else {
            continue;
        };
        if host.target().runs(64) {
            continue; // the forms are for hosts of 32-lane waves
        }
        let groups = 8;
        for form in ["local", "console"] {
            let make = |p: &crate::NativeProgram| if form == "local" { local(p) } else { console(p) };
            // Shared LDS: the same total in every lane of every group.
            let out = run_dwords(&host, &make(&shared), 64, groups, 0).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(out.iter().all(|&x| x == 2080 + 39), "{name}: {form}: shared LDS: {:?}", &out[..64]);
            // The idiom: 32 a group counted in LDS, the counter at 32 per group,
            // the list the odd t of every group.
            let out = run_dwords(&host, &make(&idiom), 64, groups, 4097 + 32 * groups)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(out[..(64 * groups) as usize].iter().all(|&x| x == 32), "{name}: {form}: LDS count");
            assert_eq!(out[4096], 32 * groups, "{name}: {form}: reserved slots");
            let mut list = out[4097..(4097 + 32 * groups) as usize].to_vec();
            list.sort_unstable();
            let mut want: Vec<u32> = (0..groups).flat_map(|_| (1..64).step_by(2)).collect();
            want.sort_unstable();
            assert_eq!(list, want, "{name}: {form}: the slots");
            // The idiom through sections: every lane's t + 1 at a slot of its own.
            let total = 2080 * groups;
            let out = run_dwords(&host, &make(&sections), 64, groups, 4097 + total)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(out[..(64 * groups) as usize].iter().all(|&x| x == 2016), "{name}: {form}: LDS sum");
            assert_eq!(out[4096], total, "{name}: {form}: reserved slots");
            let mut list: Vec<u32> = out[4097..(4097 + total) as usize].iter().copied().filter(|&x| x != 0).collect();
            list.sort_unstable();
            let mut want: Vec<u32> = (0..groups).flat_map(|_| 1..65).collect();
            want.sort_unstable();
            assert_eq!(list, want, "{name}: {form}: the slots of the sections' idiom");
            eprintln!("{name}: one-wave LDS groups exact ({form})");
        }
    }
}
