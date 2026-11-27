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

/// A one-wave group with LDS whose lanes leave a loop after different
/// numbers of turns: lane t goes round t + 1 times, adding 1 to LDS[0] each
/// time, then every lane reads LDS[0]: 2080. On the console the wave goes
/// round while any lane does; as local waves no two threads run the loop
/// as often, so the host waves may meet only before and after it.
const LDS_LOOP: &[u32] = &[
    0x7E02_0280, // v_mov_b32 v1, 0
    0x7E08_0280, // v_mov_b32 v4, 0
    0xD834_0000,
    0x0000_0401, // ds_write_b32 v1, v4
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0x7E08_0281, // v_mov_b32 v4, 1
    0x7E04_0280, // v_mov_b32 v2, 0
    0xBE8A_047E, // s_mov_b64 s[10:11], exec
    // loop:
    0xD800_0000,
    0x0000_0401, // ds_add_u32 v1, v4
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0x4A04_0481, // v_add_nc_u32 v2, 1, v2
    0x7D88_0102, // v_cmp_gt_u32 vcc, v2, v0
    0x8AFE_6A7E, // s_andn2_b64 exec, exec, vcc
    0xBF89_FFF9, // s_cbranch_execnz loop
    0xBEFE_040A, // s_mov_b64 exec, s[10:11]
    0xD8D8_0000,
    0x0200_0001, // ds_read_b32 v2, v1
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
];

/// The allocation idiom of [`LDS_IDIOM`] over every lane (each lane's t in
/// the list from dword 4097 at the reserved base plus its rank), then a
/// uniform branch on that base: the lanes whose base has bit 5 clear add 1
/// to LDS[0], and every lane reads LDS[0] after the branch. On the console
/// the wave's base is a multiple of 64: 64 in every group. As local waves
/// each host wave reserves 32 slots of its own, so of the two host waves of
/// a group one may go into the branch and the other not.
const LDS_BASE: &[u32] = &[
    0x7E02_0280, // v_mov_b32 v1, 0
    0x7E08_0280, // v_mov_b32 v4, 0
    0xD834_0000,
    0x0000_0401, // ds_write_b32 v1, v4
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
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
    0x8713_A012, // s_and_b32 s19, s18, 32
    0xBF06_8013, // s_cmp_eq_u32 s19, 0
    0xBF84_0004, // s_cbranch_scc0 +4
    0x7E18_0281, // v_mov_b32 v12, 1
    0xD800_0000,
    0x0000_0C01, // ds_add_u32 v1, v12
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0xD8D8_0000,
    0x0200_0001, // ds_read_b32 v2, v1
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
];

/// A uniform branch on a lane read of an accumulator (`v_fmac_f32` into the
/// lane's t as a float, plus 0 * 1.0) around an LDS add, as HLSL's
/// `if (WaveReadLaneFirst(fma(a, b, t)) == 0)` makes it; every lane reads
/// LDS[0] after it. On the console the wave reads lane 0: 64 in every group.
/// As local waves the first host wave reads lane 0 and goes in, the second
/// reads lane 32 and does not: 32.
const LDS_FMAC: &[u32] = &[
    0x7E02_0280, // v_mov_b32 v1, 0
    0x7E08_0280, // v_mov_b32 v4, 0
    0xD834_0000,
    0x0000_0401, // ds_write_b32 v1, v4
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0x7E0C_0D00, // v_cvt_f32_u32 v6, v0
    0x7E0A_02F2, // v_mov_b32 v5, 1.0
    0x560C_0A80, // v_fmac_f32 v6, 0, v5
    0x7E28_0506, // v_readfirstlane_b32 s20, v6
    0xBF06_8014, // s_cmp_eq_u32 s20, 0
    0xBF84_0004, // s_cbranch_scc0 +4
    0x7E18_0281, // v_mov_b32 v12, 1
    0xD800_0000,
    0x0000_0C01, // ds_add_u32 v1, v12
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0xD8D8_0000,
    0x0200_0001, // ds_read_b32 v2, v1
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
];

/// A uniform branch on a lane read of a select by a constant mask of lane
/// 0 around an LDS add: the lanes hold the select apart, so the lane each
/// host wave reads first decides its way. With every lane running, each
/// host wave tests its own lanes' bits of the constant and reads its lane
/// 0, set: both go round the add as the console's wave does, 0 in every
/// group; with other lanes running they could part.
const LDS_MASK: &[u32] = &[
    0x7E02_0280, // v_mov_b32 v1, 0
    0x7E08_0280, // v_mov_b32 v4, 0
    0xD834_0000,
    0x0000_0401, // ds_write_b32 v1, v4
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0xBE8A_0481, // s_mov_b64 s[10:11], 1
    0xD501_0006,
    0x0029_0280, // v_cndmask_b32 v6, 0, 1, s[10:11]
    0x7E28_0506, // v_readfirstlane_b32 s20, v6
    0xBF06_8014, // s_cmp_eq_u32 s20, 0
    0xBF84_0004, // s_cbranch_scc0 +4
    0x7E18_0281, // v_mov_b32 v12, 1
    0xD800_0000,
    0x0000_0C01, // ds_add_u32 v1, v12
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0xD8D8_0000,
    0x0200_0001, // ds_read_b32 v2, v1
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
];

/// A uniform branch on a flag loaded with `s_buffer_load_dword` (with
/// `glc` where `glc` says) through the view the program stores its results
/// to, at dword 4096, around an LDS add: another group, or the other host
/// wave, may write it between the two host waves' loads. Nothing writes it
/// here: every lane of every group reads 64, in both forms.
fn lds_flag(glc: bool) -> Vec<u32> {
    vec![
        0x7E02_0280, // v_mov_b32 v1, 0
        0x7E08_0280, // v_mov_b32 v4, 0
        0xD834_0000,
        0x0000_0401, // ds_write_b32 v1, v4
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xF420_0502 | u32::from(glc) << 16,
        0xFA00_4000, // s_buffer_load_dword s20, s[4:7], 0x4000 (glc)
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xBF06_8014, // s_cmp_eq_u32 s20, 0
        0xBF84_0004, // s_cbranch_scc0 +4
        0x7E18_0281, // v_mov_b32 v12, 1
        0xD800_0000,
        0x0000_0C01, // ds_add_u32 v1, v12
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xD8D8_0000,
        0x0200_0001, // ds_read_b32 v2, v1
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    ]
}

/// `native` lowered as local waves, as [`super::emit`] lowers a one-wave
/// group with LDS, with `VICTORY_LOCAL_MEET_CHECKS` as `checks` says.
fn local_lowered(native: &crate::NativeProgram, target: super::Target, checks: bool) -> super::lower::Lowered {
    local_lowered_with(native, target, checks, false).expect("local waves")
}

/// [`local_lowered`], of a program that reaches memory by address where
/// `addresses` says ([`crate::resource::Resources::addresses`]).
fn local_lowered_with(
    native: &crate::NativeProgram,
    target: super::Target,
    checks: bool,
    addresses: bool,
) -> Result<super::lower::Lowered, crate::ShaderError> {
    let p = crate::ir::build(&native.code, native.wave64, native.vgprs).expect("IR");
    let res = crate::resource::track_with(&p, &native.layout, native.stage, addresses).expect("resources");
    let io = crate::io::raise(&p, native).expect("io");
    let st = crate::structure::structure(&p).expect("structure");
    let parts = super::lower::Parts {
        native,
        psv: None,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
        target,
        console_cf: false,
        per_lane_only: false,
        local: true,
    };
    super::with_switch(super::Switch::LocalMeetChecks, checks, || super::lower::lower(&parts))
}

/// A program that reaches memory by address returns at its entry in the
/// threads of its last group past the dispatch, each alone: its local waves
/// cannot meet, as its shared-wave and lockstep forms cannot.
#[test]
fn local_waves_of_programs_by_address_do_not_meet() {
    let nvidia = super::Target { lanes_min: 32, lanes_max: 32, exact_views: false };
    let p = with_lds(LDS_SHARED);
    assert!(local_lowered_with(&p, nvidia, true, false).is_ok());
    let refused = local_lowered_with(&p, nvidia, true, true).err().expect("refused");
    assert!(refused.to_string().contains("by address"), "{refused}");
    assert!(local_lowered_with(&p, nvidia, false, true).is_ok(), "the fork's rule met there");
}

/// Local waves meet only where both host waves run as often: not inside a
/// loop their lanes leave after different numbers of turns, not inside a
/// branch on the slots each host wave reserved, on a lane read of an
/// accumulator of lane values or of a select by a constant mask, or on a
/// flag loaded from memory the dispatch writes (with `glc`, or through the
/// view it stores to). The translation's group barriers are then met as
/// often on every way through the program, and none sits between a loop's
/// header and its back edge; the fork's meetings
/// (`VICTORY_LOCAL_MEET_CHECKS=0`) failed them all, and 1952aaf's analysis
/// (`VICTORY_HOST_WAVE_READS=0`) the accumulator, the mask and the flags
/// (checked here without the card: run there they could hang it).
#[test]
fn local_waves_meet_only_where_both_host_waves_run_as_often() {
    let nvidia = super::Target { lanes_min: 32, lanes_max: 32, exact_views: false };
    let flags = (lds_flag(true), lds_flag(false));
    let cases = [
        ("a loop lanes leave apart", LDS_LOOP, false),
        ("a branch on the reserved base", LDS_BASE, false),
        ("a branch on a lane read of an accumulator", LDS_FMAC, true),
        ("a branch on a lane read of a constant mask's select", LDS_MASK, true),
        ("a branch on a flag loaded with glc", &flags.0[..], true),
        ("a branch on a flag of the view it stores to, of no declared kind", &flags.1[..], true),
    ];
    for (label, body, read) in cases {
        let p = with_lds(body);
        let l = local_lowered(&p, nvidia, true);
        assert_eq!(l.barriers.0, 0, "{label}: no barrier inside a loop");
        assert_eq!(l.barriers.1.len(), 1, "{label}: as many barriers on every way: {:?}", l.barriers.1);
        assert!(l.barriers.1[0] > 0, "{label}: the waves meet around their LDS accesses");
        // The loop's block is in its own region, the branch's body in its:
        // no block there is a meeting place.
        assert_eq!(l.all_lane, Some(vec![true, false, true]), "{label}");
        // The production path takes this very translation.
        let emitted = emit_for(&p, nvidia).expect("translates");
        assert_eq!(emitted.dxil, l.dxil, "{label}: emit gives the local waves checked here");
        assert_eq!(wave_size(&emitted.dxil), Some(32));
        let fork = local_lowered(&p, nvidia, false);
        assert!(
            fork.barriers.0 > 0 || fork.barriers.1.len() > 1,
            "{label}: the fork's meetings part the threads: {:?}",
            fork.barriers
        );
        // 1952aaf met inside the branches on what an instruction reads
        // beside its operands, on a select's mask and on loaded flags: the
        // two ways round the LDS add met a different number of times.
        let old = super::with_switch(super::Switch::HostWaveReads, false, || local_lowered(&p, nvidia, true));
        if read {
            assert_eq!(old.all_lane, Some(vec![true, true, true]), "{label}: 1952aaf");
            assert!(old.barriers.1.len() > 1, "{label}: 1952aaf's meetings part the threads: {:?}", old.barriers);
        } else {
            assert_eq!(old.dxil, l.dxil, "{label}: as 1952aaf");
        }
    }
    // The shared-LDS and idiom programs keep their meetings.
    for body in [LDS_SHARED, LDS_IDIOM] {
        let p = with_lds(body);
        assert_eq!(local_lowered(&p, nvidia, true).dxil, local_lowered(&p, nvidia, false).dxil);
    }
}

/// The SCC each uniform branch of `p` reads, by block, and whether the host
/// waves of local waves may hold it apart; `written` as
/// [`super::local_waves::loads_of_written_memory`] gives it.
fn branches_apart(p: &crate::ir::Program, written: &std::collections::HashSet<(usize, usize)>) -> Vec<(usize, bool)> {
    use crate::structure::{Atom, LaneExit};
    let st = crate::structure::structure(p).expect("structure");
    let g = super::lower::LaneGraph::of(&st).expect("graph");
    let varying = super::local_waves::host_wave_varying(p, &st, &g, written);
    (0..st.exits.len())
        .filter_map(|b| match st.exits[b] {
            LaneExit::Branch { when, .. } => match st.bools.support(when)[..] {
                [Atom::Uniform(scc)] => Some((b, varying[scc.0 as usize])),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// What the two host waves of local waves may hold apart: a lane read of a
/// lane's own value (a thread id) and the slots a first lane reserved are,
/// a lane read of a scalar every lane holds is not; nor is a user SGPR.
/// What an instruction reads beside its operands counts: an accumulator's
/// old value (`v_fmac_f32`, `v_mac_f32`, `v_fmac_legacy_f32`,
/// `v_dot4c_i32_i8`, the 16-bit and packed forms), both registers of a
/// swap, `v_div_fmas_f32`'s VCC (apart where a loop its lanes leave apart
/// set it), any VGPR but the old value of a destination written whole; and
/// a select or a carry by a constant mask of some lanes takes each lane's
/// own bit. 1952aaf (`VICTORY_HOST_WAVE_READS=0`) took the 32-bit ones as
/// alike.
#[test]
fn host_waves_hold_lane_reads_and_reserved_slots_apart() {
    use crate::ir::Reg;
    let words = [
        0x7E0A_0200, // v_mov_b32 v5, s0
        0x7E0C_0505, // v_readfirstlane_b32 s6, v5
        0x7E0E_0500, // v_readfirstlane_b32 s7, v0
        0xBF06_8006, // s_cmp_eq_u32 s6, 0
        0xBF85_0001, // s_cbranch_scc1 +1
        0x7E02_0280, // v_mov_b32 v1, 0
        0xBF06_8007, // s_cmp_eq_u32 s7, 0
        0xBF85_0001, // s_cbranch_scc1 +1
        0x7E02_0281, // v_mov_b32 v1, 1
        0xBF06_8000, // s_cmp_eq_u32 s0, 0
        0xBF85_0001, // s_cbranch_scc1 +1
        0x7E02_0282, // v_mov_b32 v1, 2
        S_ENDPGM,
    ];
    let ir = |words: &[u32]| {
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        crate::ir::build(&code, true, 16).expect("IR")
    };
    // Whether the host waves may hold apart the SCC each uniform branch
    // reads, in block order; and so by 1952aaf's rules.
    let apart_in = |p: &crate::ir::Program| -> Vec<bool> {
        branches_apart(p, &Default::default()).into_iter().map(|f| f.1).collect()
    };
    let apart = |words: &[u32]| apart_in(&ir(words));
    let old = |words: &[u32]| super::with_switch(super::Switch::HostWaveReads, false, || apart(words));
    assert_eq!(apart(&words), [false, true, false]);
    // The reserved base's bit: apart.
    assert_eq!(apart(&LDS_BASE.iter().copied().chain([S_ENDPGM]).collect::<Vec<_>>()), [true]);
    // An accumulator reads its destination: of a lane's own value (t as a
    // float), apart; of a value every lane holds, not. 1952aaf counted the
    // old value of 16-bit results only.
    for (name, acc, half) in [
        ("v_fmac_f32", 0x5602_0A00, false),        // v_fmac_f32 v1, s0, v5
        ("v_mac_f32", 0x3E02_0A00, false),         // v_mac_f32 v1, s0, v5
        ("v_fmac_legacy_f32", 0x0C02_0A00, false), // v_fmac_legacy_f32 v1, s0, v5
        ("v_dot4c_i32_i8", 0x1A02_0A00, false),    // v_dot4c_i32_i8 v1, s0, v5
        ("v_fmac_f16", 0x6C02_0A00, true),         // v_fmac_f16 v1, s0, v5
        ("v_pk_fmac_f16", 0x7802_0A00, true),      // v_pk_fmac_f16 v1, s0, v5
    ] {
        let of = |first: u32| {
            [
                first,
                0x7E0A_0200, // v_mov_b32 v5, s0
                acc,
                0x7E0C_0501, // v_readfirstlane_b32 s6, v1
                0xBF06_8006, // s_cmp_eq_u32 s6, 0
                0xBF85_0001, // s_cbranch_scc1 +1
                0x7E08_0280, // v_mov_b32 v4, 0
                S_ENDPGM,
            ]
        };
        let lane = of(0x7E02_0D00); // v_cvt_f32_u32 v1, v0
        assert_eq!(apart(&lane), [true], "{name} of a lane's own value");
        assert_eq!(old(&lane), [half], "{name}: 1952aaf");
        assert_eq!(apart(&of(0x7E02_0201)), [false], "{name} of s1"); // v_mov_b32 v1, s1
    }
    // A swap hands a lane's own value to its source register.
    let swap = [
        0x7E04_0200, // v_mov_b32 v2, s0
        0x7E02_0300, // v_mov_b32 v1, v0
        0x7E02_CB02, // v_swap_b32 v1, v2
        0x7E0C_0502, // v_readfirstlane_b32 s6, v2
        0xBF06_8006, // s_cmp_eq_u32 s6, 0
        0xBF85_0001, // s_cbranch_scc1 +1
        0x7E08_0280, // v_mov_b32 v4, 0
        S_ENDPGM,
    ];
    assert_eq!(apart(&swap), [true]);
    assert_eq!(old(&swap), [false]);
    // Any other VGPR read beside the operands counts; the old value of a
    // destination written whole does not (only lanes that never get there
    // keep it). No decoded instruction reads one today: one is put into a
    // move here.
    let mut p = ir(&[
        0x7E02_0300, // v_mov_b32 v1, v0
        0x7E02_0200, // v_mov_b32 v1, s0
        0x7E0C_0501, // v_readfirstlane_b32 s6, v1
        0xBF06_8006, // s_cmp_eq_u32 s6, 0
        0xBF85_0001, // s_cbranch_scc1 +1
        0x7E08_0280, // v_mov_b32 v4, 0
        S_ENDPGM,
    ]);
    assert_eq!(apart_in(&p), [false], "the old value of a whole write");
    let t = p.inputs.iter().find(|i| i.0 == Reg::V(0)).expect("v0 on entry").1;
    let mov = p
        .blocks
        .iter_mut()
        .flat_map(|b| &mut b.insts)
        .find(|i| i.name() == "v_mov_b32" && i.read(Reg::S(0)).is_some())
        .expect("v_mov_b32 v1, s0");
    mov.reads.push((Reg::V(9), t));
    assert_eq!(apart_in(&p), [true], "another VGPR read beside the operands");
    // `v_div_fmas_f32` reads VCC beside its operands: the carry of a scale
    // of values every lane holds, or a compare of the lane's own id.
    let fmas = [
        0x7E0A_0200, // v_mov_b32 v5, s0
        0xD56D_6A01,
        0x0416_0B05, // v_div_scale_f32 v1, vcc, v5, v5, v5
        0xD56F_0002,
        0x0406_0301, // v_div_fmas_f32 v2, v1, v1, v1
        0x7E0C_0502, // v_readfirstlane_b32 s6, v2
        0x7D88_0085, // v_cmp_gt_u32 vcc, 5, v0
        0xD56F_0003,
        0x0416_0B05, // v_div_fmas_f32 v3, v5, v5, v5
        0x7E0E_0503, // v_readfirstlane_b32 s7, v3
        0xBF06_8006, // s_cmp_eq_u32 s6, 0
        0xBF85_0001, // s_cbranch_scc1 +1
        0x7E08_0280, // v_mov_b32 v4, 0
        0xBF06_8007, // s_cmp_eq_u32 s7, 0
        0xBF85_0001, // s_cbranch_scc1 +1
        0x7E08_0281, // v_mov_b32 v4, 1
        S_ENDPGM,
    ];
    assert_eq!(apart(&fmas), [false, true]);
    assert_eq!(old(&fmas), [false, false]);
    // The carry of a scale of values every lane holds, set in a loop that
    // lane t leaves after t + 1 turns and read after it: the lanes that left
    // before the last turn hold a clear bit, so the host waves may read
    // apart what the division made of it.
    let looped = [
        0x7E0A_0200, // v_mov_b32 v5, s0
        0x7E04_0280, // v_mov_b32 v2, 0
        0xBE8A_047E, // s_mov_b64 s[10:11], exec
        // loop:
        0xD56D_6A01,
        0x0416_0B05, // v_div_scale_f32 v1, vcc, v5, v5, v5
        0x4A04_0481, // v_add_nc_u32 v2, 1, v2
        0xD4C4_000C,
        0x0002_0102, // v_cmp_gt_u32 s[12:13], v2, v0
        0x8AFE_0C7E, // s_andn2_b64 exec, exec, s[12:13]
        0xBF89_FFF9, // s_cbranch_execnz loop
        0xBEFE_040A, // s_mov_b64 exec, s[10:11]
        0xD56F_0003,
        0x0416_0B05, // v_div_fmas_f32 v3, v5, v5, v5
        0x7E0C_0503, // v_readfirstlane_b32 s6, v3
        0xBF06_8006, // s_cmp_eq_u32 s6, 0
        0xBF85_0001, // s_cbranch_scc1 +1
        0x7E08_0280, // v_mov_b32 v4, 0
        S_ENDPGM,
    ];
    assert_eq!(apart(&looped), [true]);
    assert_eq!(old(&looped), [false]);
    // A select by a mask of neither EXEC nor a compare: of a constant of
    // some lanes, apart; of a uniform select (every lane's bit an SCC), not.
    let selects = [
        0xBE8A_0481, // s_mov_b64 s[10:11], 1
        0xD501_0001,
        0x0029_0280, // v_cndmask_b32 v1, 0, 1, s[10:11]
        0x7E0C_0501, // v_readfirstlane_b32 s6, v1
        0xBF06_8000, // s_cmp_eq_u32 s0, 0
        0x858C_80C1, // s_cselect_b64 s[12:13], -1, 0
        0xD501_0002,
        0x0031_0280, // v_cndmask_b32 v2, 0, 1, s[12:13]
        0x7E0E_0502, // v_readfirstlane_b32 s7, v2
        0xBF06_8006, // s_cmp_eq_u32 s6, 0
        0xBF85_0001, // s_cbranch_scc1 +1
        0x7E08_0280, // v_mov_b32 v4, 0
        0xBF06_8007, // s_cmp_eq_u32 s7, 0
        0xBF85_0001, // s_cbranch_scc1 +1
        0x7E08_0281, // v_mov_b32 v4, 1
        S_ENDPGM,
    ];
    assert_eq!(apart(&selects), [true, false]);
    assert_eq!(old(&selects), [false, false]);
    // A carry-in of a constant of some lanes: lane 0 adds 1.
    let carry = [
        0xBE8A_0481, // s_mov_b64 s[10:11], 1
        0xD528_6A01,
        0x0029_0080, // v_add_co_ci_u32 v1, vcc, 0, 0, s[10:11]
        0x7E0C_0501, // v_readfirstlane_b32 s6, v1
        0xBF06_8006, // s_cmp_eq_u32 s6, 0
        0xBF85_0001, // s_cbranch_scc1 +1
        0x7E08_0280, // v_mov_b32 v4, 0
        S_ENDPGM,
    ];
    assert_eq!(apart(&carry), [true]);
    assert_eq!(old(&carry), [false]);
}

/// A uniform branch on a scalar load of memory the dispatch itself writes
/// may part the two host waves (a write can land between their two loads):
/// with `glc` or `dlc` always; in a program that stores to memory, through
/// an unordered-access view, a view of no declared kind or a root view of
/// either; never through a constant buffer or shader resource view, nor in
/// a program that writes no memory. 1952aaf (`VICTORY_HOST_WAVE_READS=0`)
/// took every one as alike.
#[test]
fn host_waves_may_load_written_memory_apart() {
    use crate::native::{ResourceType, UsedResource};
    // s20 from `load`, then a branch on it; the results stored through the
    // table's first view where `stores` says.
    let body = |load: &[u32], stores: bool| -> Vec<u32> {
        let mut words = vec![
            0xBE82_0300, // s_mov_b32 s2, s0
            0xBE83_0380, // s_mov_b32 s3, 0
            0xF408_0101,
            0xFA00_0000, // s_load_dwordx4 s[4:7], s[2:3], null
            0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        ];
        words.extend_from_slice(load);
        words.extend_from_slice(&[
            0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
            0xBF06_8014, // s_cmp_eq_u32 s20, 0
            0xBF85_0001, // s_cbranch_scc1 +1
            0x7E04_0281, // v_mov_b32 v2, 1
        ]);
        if stores {
            words.extend_from_slice(&[
                0x3406_0082, // v_lshlrev_b32 v3, 2, v0
                0xE070_1000,
                0x8001_0203, // buffer_store_dword v2, v3, s[4:7], 0 offen
            ]);
        }
        words.push(S_ENDPGM);
        words
    };
    // The table in s0 (its view at dword 0 as `first`, at dword 4 as
    // `second`), a root view in s[8:9] (as `root`); none: no kind declared.
    let kinds = |first: Option<ResourceType>, second: Option<ResourceType>, root: Option<ResourceType>| {
        let entry = |kind, root, table_offset_dwords, user_data, slot| UsedResource {
            kind,
            root,
            table_offset_dwords,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data,
            slot,
        };
        let mut used = Vec::new();
        used.extend(first.map(|k| entry(k, RootType::Table, 0, 0, 0)));
        used.extend(second.map(|k| entry(k, RootType::Table, 4, 0, 0)));
        used.extend(root.map(|k| entry(k, RootType::Descriptor, 0, 8, 1)));
        used
    };
    let apart = |load: &[u32], stores: bool, used: Vec<UsedResource>| -> Vec<bool> {
        let mut native = super::tests::compute(&body(load, stores), true);
        native.layout = vec![
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Descriptor,
                table_offset_dwords: 0,
                sgprs: 2,
                base_register: 0,
                user_data: 8,
                slot: 1,
            },
        ];
        native.used = used;
        // Ten user SGPRs: the table in s0, the root view in s[8:9].
        native.io = StageIo::Cs(crate::native::ComputeIo { rsrc2: 10 << 1, threads: [64, 1, 1] });
        let p = crate::ir::build(&native.code, native.wave64, native.vgprs).expect("IR");
        let res = crate::resource::track_with(&p, &native.layout, native.stage, false).expect("resources");
        let written = super::local_waves::loads_of_written_memory(&native, &p, &res);
        branches_apart(&p, &written).into_iter().map(|f| f.1).collect()
    };
    let (uav, srv, cbv) = (Some(ResourceType::Uav), Some(ResourceType::Srv), Some(ResourceType::Cbv));
    // s_buffer_load_dword s20, s[4:7], 0x4000 (glc, dlc as `bits`): the
    // table's first view, the one the program stores to.
    let first = |bits: u32| [0xF420_0502 | bits, 0xFA00_4000];
    assert_eq!(apart(&first(0), true, kinds(uav, None, None)), [true], "the view it stores to");
    assert_eq!(apart(&first(0), true, kinds(None, None, None)), [true], "a view of no declared kind");
    assert_eq!(apart(&first(0), false, kinds(uav, None, None)), [false], "a program that writes no memory");
    assert_eq!(apart(&first(1 << 16), false, kinds(cbv, None, None)), [true], "glc");
    assert_eq!(apart(&first(1 << 14), false, kinds(cbv, None, None)), [true], "dlc");
    // The table's second view, which the program only reads.
    let second = |glc: bool| {
        [
            0xF408_0301,
            0xFA00_0010, // s_load_dwordx4 s[12:15], s[2:3], 0x10
            0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
            0xF420_0506 | u32::from(glc) << 16,
            0xFA00_0000, // s_buffer_load_dword s20, s[12:15], 0 (glc)
        ]
    };
    assert_eq!(apart(&second(false), true, kinds(uav, cbv, None)), [false], "a constant buffer");
    assert_eq!(apart(&second(false), true, kinds(uav, srv, None)), [false], "a shader resource view");
    assert_eq!(apart(&second(false), true, kinds(uav, uav, None)), [true], "another unordered-access view");
    assert_eq!(apart(&second(true), true, kinds(uav, cbv, None)), [true], "a constant buffer with glc");
    // By address, from the root view in s[8:9].
    let root = [
        0xF400_0504,
        0xFA00_0010, // s_load_dword s20, s[8:9], 0x10
    ];
    assert_eq!(apart(&root, true, kinds(uav, None, cbv)), [false], "a root constant buffer");
    assert_eq!(apart(&root, true, kinds(uav, None, uav)), [true], "a root unordered-access view");
    assert_eq!(apart(&root, true, kinds(uav, None, None)), [true], "a root view of no declared kind");
    // 1952aaf took them all as alike.
    let old = super::with_switch(super::Switch::HostWaveReads, false, || {
        apart(&first(1 << 16), true, kinds(uav, None, None))
    });
    assert_eq!(old, [false]);
}

/// [`LDS_LOOP`], [`LDS_BASE`], [`LDS_FMAC`], [`LDS_MASK`] and
/// [`lds_flag`] on every host of 32-lane waves, as local waves and in the
/// console flow: every lane of a group reads the LDS total its group made,
/// and the idiom's slots are each lane's own.
#[test]
fn local_waves_of_loops_and_reserved_branches_match_the_console() {
    let nvidia = super::Target { lanes_min: 32, lanes_max: 32, exact_views: false };
    let local = |p: &crate::NativeProgram| emit_for(p, nvidia).expect("local waves");
    let console = |p: &crate::NativeProgram| {
        super::with_switch(super::Switch::LdsLocal, false, || emit_for(p, nvidia)).expect("console flow")
    };
    let (looped, base, fmac) = (with_lds(LDS_LOOP), with_lds(LDS_BASE), with_lds(LDS_FMAC));
    let (mask, glc, flag) = (with_lds(LDS_MASK), with_lds(&lds_flag(true)), with_lds(&lds_flag(false)));
    // The console flow has no lane read of a constant mask's select (an
    // SCC it does not compute): the mask runs as local waves only.
    assert!(super::with_switch(super::Switch::LdsLocal, false, || emit_for(&mask, nvidia)).is_err());
    for p in [&looped, &base, &fmac, &mask, &glc, &flag] {
        let l = local(p);
        let c = (!std::ptr::eq(p, &mask)).then(|| console(p));
        assert!(c.as_ref().is_none_or(|c| c.dxil != l.dxil), "local waves are not the console flow");
        // The barriers are checked before anything runs on the card.
        let checked = local_lowered(p, nvidia, true);
        assert_eq!(checked.dxil, l.dxil);
        assert_eq!((checked.barriers.0, checked.barriers.1.len()), (0, 1));
        if let Ok(mut v) =
            super::validator::Validator::load(super::validator::Validator::default_path().as_deref())
        {
            v.validate(&mut l.dxil.clone()).expect("local waves validate");
            if let Some(c) = &c {
                v.validate(&mut c.dxil.clone()).expect("the console flow validates");
            }
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
            let out = run_dwords(&host, &make(&looped), 64, groups, 0).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(out.iter().all(|&x| x == 2080), "{name}: {form}: the loop's LDS total: {:?}", &out[..64]);
            let out = run_dwords(&host, &make(&base), 64, groups, 4097 + 64 * groups)
                .unwrap_or_else(|e| panic!("{name}: {form}: {e}"));
            let mut total = 0;
            for g in 0..groups as usize {
                let lanes = &out[64 * g..64 * (g + 1)];
                assert!(lanes.iter().all(|&x| x == lanes[0]), "{name}: {form}: group {g} reads one total: {lanes:?}");
                if form == "console" {
                    assert_eq!(lanes[0], 64, "{name}: console: the wave's base is a multiple of 64");
                } else {
                    assert!(matches!(lanes[0], 0 | 32 | 64), "{name}: local: group {g}: {}", lanes[0]);
                }
                total += lanes[0];
            }
            // Local waves: of the 2 * groups host waves, those that reserved
            // at an even multiple of 32 went into the branch.
            let want = if form == "console" { 64 * groups } else { 32 * groups };
            assert_eq!(total, want, "{name}: {form}: the lanes that went into the branch");
            assert_eq!(out[4096], 64 * groups, "{name}: {form}: reserved slots");
            let mut list = out[4097..(4097 + 64 * groups) as usize].to_vec();
            list.sort_unstable();
            let mut want: Vec<u32> = (0..groups).flat_map(|_| 0..64).collect();
            want.sort_unstable();
            assert_eq!(list, want, "{name}: {form}: the slots");
            // The branch on the first lane's t: the whole wave on the
            // console, the first host wave of each group as local waves.
            let out = run_dwords(&host, &make(&fmac), 64, groups, 0).unwrap_or_else(|e| panic!("{name}: {e}"));
            let want = if form == "console" { 64 } else { 32 };
            assert!(out.iter().all(|&x| x == want), "{name}: {form}: the accumulator's branch: {:?}", &out[..64]);
            // The branch on the first lane's bit of lane 0's mask: each
            // host wave tests its own lanes' bits, so with every lane
            // running both go round it, as the console's wave would.
            if form == "local" {
                let out = run_dwords(&host, &local(&mask), 64, groups, 0).unwrap_or_else(|e| panic!("{name}: {e}"));
                assert!(out.iter().all(|&x| x == 0), "{name}: local: the mask's branch: {:?}", &out[..64]);
            }
            // The branch on a flag nothing writes: both host waves go in.
            for (what, p) in [("glc", &glc), ("plain", &flag)] {
                let out = run_dwords(&host, &make(p), 64, groups, 4097).unwrap_or_else(|e| panic!("{name}: {e}"));
                let lanes = &out[..(64 * groups) as usize];
                assert!(lanes.iter().all(|&x| x == 64), "{name}: {form}: the {what} flag's branch: {:?}", &out[..64]);
            }
            eprintln!("{name}: loops and reserved branches of one-wave LDS groups exact ({form})");
        }
    }
}
