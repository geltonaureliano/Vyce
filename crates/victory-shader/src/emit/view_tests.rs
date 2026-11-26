//! Buffer accesses for hosts whose descriptors only read right through
//! accesses of their own kind (`Target::exact_views`), dispatched on every
//! hardware adapter with the views such a host makes: dwords, whatever the
//! title's stride is.

use super::host::{Dispatch, Host, Root, Stride, View, adapters};
use super::tests::{compute, emit_for};
use crate::native::{ComputeIo, NativeProgram, ResourceType, RootParam, RootType, StageIo};
use crate::{ShaderError, UsedResource};

const S_ENDPGM: u32 = 0xBF81_0000;

fn param(root: RootType, user_data: u8) -> RootParam {
    RootParam {
        root,
        table_offset_dwords: 0,
        sgprs: 1,
        base_register: 0,
        user_data: user_data.into(),
        slot: user_data,
    }
}

fn exact(host: &Host) -> super::Target {
    super::Target { exact_views: true, ..host.target() }
}

/// Runs `native` for an exact-views host, with `views`, the host's table of
/// strides by slot and the user data.
fn run(
    host: &Host,
    native: &NativeProgram,
    views: &[View],
    strides: Vec<u32>,
    mut data: Vec<u32>,
) -> Result<Vec<u32>, String> {
    let cs = emit_for(native, exact(host)).map_err(|e: ShaderError| e.to_string())?;
    if let Ok(mut v) =
        super::validator::Validator::load(super::validator::Validator::default_path().as_deref())
    {
        v.validate(&mut cs.dxil.clone()).map_err(|e| format!("validation: {e}"))?;
    }
    let (root, pso) = host.compute_pipeline(&cs)?;
    data.resize((cs.user_data_dwords() as usize).max(1), 0);
    let mut roots = vec![(0, Root::Constants(data))];
    if cs.stride_table {
        roots.push((1, Root::View { srv: true, data: strides }));
    }
    let out = host.dispatch(
        &root,
        &pso,
        &Dispatch { roots, views, samplers: &[], groups: [1, 1, 1] },
    )?;
    Ok(out.into_iter().next().unwrap_or_default())
}

/// Dword `3 * stride + offset` of a structured buffer read by element
/// (vector memory), and by scalar memory; then written by element 2 of a
/// structured UAV with an atomic on top.
fn structured(stride: u32) -> NativeProgram {
    let offset = if stride >= 8 { 4 } else { 0 };
    let words = [
        0xBE85_0380,          // s_mov_b32 s5, 0
        0xBE84_0300,          // s_mov_b32 s4, s0
        0xF408_0202,
        0xFA00_0000,          // s_load_dwordx4 s[8:11], s[4:5], 0x0 (SRV)
        0xF408_0302,
        0xFA00_0020,          // s_load_dwordx4 s[12:15], s[4:5], 0x20 (UAV)
        0x7E00_0283,          // v_mov_b32 v0, 3
        0x7E02_0280 | (offset / 4 * 4), // v_mov_b32 v1, offset (inline constant)
        0xE030_3000,
        0x8002_0200,          // buffer_load_dword v2, v[0:1], s[8:11], 0 idxen offen
        0xF420_0404,
        0xFA00_0000 | (3 * stride + offset), // s_buffer_load_dword s16, s[8:11], ...
        0x7E06_0210,          // v_mov_b32 v3, s16
        0x7E08_0282,          // v_mov_b32 v4, 2
        0x7E0A_0280,          // v_mov_b32 v5, 0
        0xE070_3000,
        0x8003_0204,          // buffer_store_dword v2, v[4:5], s[12:15], 0 idxen offen
        0x7E08_0283,          // v_mov_b32 v4, 3
        0xE070_3000,
        0x8003_0304,          // buffer_store_dword v3, v[4:5], s[12:15], 0 idxen offen
        0x7E0C_0285,          // v_mov_b32 v6, 5
        0xE0C8_3000,
        0x8003_0604,          // buffer_atomic_add v6, v[4:5], s[12:15], 0 idxen offen
        S_ENDPGM,
    ];
    let mut p = compute(&words, false);
    p.layout = vec![param(RootType::Table, 0)];
    let used = |kind, table_offset_dwords| UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords,
        lower_bound: 0,
        stride,
        dwords: 4,
        user_data: 0,
        slot: 0,
    };
    p.used = vec![used(ResourceType::Srv, 0), used(ResourceType::Uav, 8)];
    p.io = StageIo::Cs(ComputeIo { rsrc2: 1 << 1, threads: [1, 1, 1] });
    p
}

#[test]
fn structured_buffers_of_any_stride_read_right_through_dword_views() {
    let Ok(names) = adapters() else {
        eprintln!("no DXGI; skipped");
        return;
    };
    let data: Vec<u32> = (0..4096).map(|i| 0x5000_0000 | i).collect();
    for name in names {
        let Ok(host) = Host::on(&name) else {
            continue;
        };
        for stride in [4u32, 8, 12, 16, 20, 24, 28, 36, 48, 60, 376, 372, 2044] {
            let native = structured(stride);
            let offset = if stride >= 8 { 4 } else { 0 };
            let want = 0x5000_0000 | (3 * stride + offset) / 4;
            for view in [Stride::Raw, Stride::Structured(4)] {
                let views = [View::Buffer { data: &data, stride: view }, View::RawUav(2048)];
                let out = run(&host, &native, &views, vec![4, 4], vec![])
                    .unwrap_or_else(|e| panic!("{name}: stride {stride}: {e}"));
                let at = |i: u32| out[(i * stride / 4) as usize];
                assert_eq!(at(2), want, "{name}: element load, stride {stride}");
                assert_eq!(at(3), want + 5, "{name}: scalar load and atomic, stride {stride}");
            }
        }
    }
}

/// Four dwords from byte 16 of the buffer the heap holds at the index in
/// s2, by scalar memory; with `vector`, a dword by vector memory too.
fn picked(vector: bool) -> NativeProgram {
    let mut words = vec![
        0xBE85_0380, // s_mov_b32 s5, 0
        0xBE84_0300, // s_mov_b32 s4, s0
        0xF408_0202,
        0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0 (UAV)
        0x8F06_8502, // s_lshl_b32 s6, s2, 5
        0xBE84_0301, // s_mov_b32 s4, s1
        0xF408_0302,
        0x0C00_0000, // s_load_dwordx4 s[12:15], s[4:5], s6
        0xF428_0406,
        0xFA00_0010, // s_buffer_load_dwordx4 s[16:19], s[12:15], 0x10
        0x7E00_0280, // v_mov_b32 v0, 0
        0x7E02_0210, // v_mov_b32 v1, s16
        0x7E04_0211, // v_mov_b32 v2, s17
        0x7E06_0212, // v_mov_b32 v3, s18
        0x7E08_0213, // v_mov_b32 v4, s19
        0xE078_1000,
        0x8002_0100, // buffer_store_dwordx4 v[1:4], v0, s[8:11], 0 offen
    ];
    if vector {
        words.extend([
            0xE030_1004,
            0x8003_0500, // buffer_load_dword v5, v0, s[12:15], 4 offen
            0xE070_1010,
            0x8002_0500, // buffer_store_dword v5, v0, s[8:11], 16 offen
        ]);
    }
    words.push(S_ENDPGM);
    let mut p = compute(&words, false);
    p.layout = vec![
        param(RootType::Table, 0),
        param(RootType::ResourceHeap, 1),
        param(RootType::Constants, 2),
    ];
    p.used = vec![UsedResource {
        kind: ResourceType::Uav,
        root: RootType::Table,
        table_offset_dwords: 0,
        lower_bound: 0,
        stride: 0,
        dwords: 4,
        user_data: 0,
        slot: 0,
    }];
    p.io = StageIo::Cs(ComputeIo { rsrc2: 3 << 1, threads: [1, 1, 1] });
    p
}

#[test]
fn scalar_loads_of_heap_picked_buffers_follow_the_kind_of_view() {
    let Ok(names) = adapters() else {
        eprintln!("no DXGI; skipped");
        return;
    };
    let data: Vec<u32> = (0..256).map(|i| 0x6000_0000 | i).collect();
    for name in names {
        let Ok(host) = Host::on(&name) else {
            continue;
        };
        let views = [
            View::RawUav(64),
            View::Buffer { data: &data, stride: Stride::Raw },
            View::Buffer { data: &data, stride: Stride::Structured(4) },
            View::Cbv(&data),
        ];
        let strides = vec![4, 4, 12, 0];
        let want: Vec<u32> = (4..8).map(|i| 0x6000_0000 | i).collect();
        for index in 1..4 {
            let out = run(&host, &picked(false), &views, strides.clone(), vec![0, 0, index])
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(&out[..4], &want[..], "{name}: view {index}");
        }
        for index in 1..3 {
            let out = run(&host, &picked(true), &views, strides.clone(), vec![0, 0, index])
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(&out[..4], &want[..], "{name}: view {index}, beside a vector load");
            assert_eq!(out[4], 0x6000_0001, "{name}: view {index}, the vector load");
        }
    }
}

/// A descriptor vector memory reads too is taken for a buffer view without
/// asking the host's table; others ask it.
#[test]
fn only_unclear_heap_descriptors_ask_the_stride_table() {
    let target = super::Target { exact_views: true, ..super::Target::ANY };
    assert!(emit_for(&picked(false), target).unwrap().stride_table);
    assert!(!emit_for(&picked(true), target).unwrap().stride_table);
    // Hosts with one kind of buffer descriptor: no difference.
    assert!(!emit_for(&picked(false), super::Target::ANY).unwrap().stride_table);
}
