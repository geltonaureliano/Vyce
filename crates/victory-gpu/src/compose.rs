//! Two presented planes as one image: the console's display puts the second
//! (the interface) over the first (the scene), each scaled to the screen.
//! Grand Theft Auto VI presents so on Series S.

use std::sync::atomic::{AtomicU32, Ordering};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{PCSTR, s};

use crate::host::Host;

const SOURCE: &str = r#"
#define RS "DescriptorTable(SRV(t0, numDescriptors=2), visibility=SHADER_VISIBILITY_PIXEL), RootConstants(num32BitConstants=1, b0, visibility=SHADER_VISIBILITY_PIXEL), StaticSampler(s0, filter=FILTER_MIN_MAG_MIP_LINEAR, addressU=TEXTURE_ADDRESS_CLAMP, addressV=TEXTURE_ADDRESS_CLAMP)"
Texture2D<float4> scene : register(t0);
Texture2D<float4> over : register(t1);
SamplerState smooth_ : register(s0);
cbuffer Transfer : register(b0) { uint color_mode; };
struct V { float4 at : SV_Position; float2 uv : TEXCOORD0; };
[RootSignature(RS)]
V vs(uint id : SV_VertexID) {
    V v;
    v.uv = float2((id << 1) & 2, id & 2);
    v.at = float4(v.uv * float2(2, -2) + float2(-1, 1), 0, 1);
    return v;
}
float3 encode_srgb(float3 value) {
    float3 rgb = saturate(value);
    return lerp(12.92 * rgb, 1.055 * pow(rgb, 1.0 / 2.4) - 0.055, step(0.0031308, rgb));
}
[RootSignature(RS)]
float4 ps(V v) : SV_Target {
    float4 s = scene.SampleLevel(smooth_, v.uv, 0);
    float4 o = over.SampleLevel(smooth_, v.uv, 0);
    bool blend_linear = (color_mode & 4u) != 0u;
    if (!blend_linear) {
        if ((color_mode & 1u) != 0u) s.rgb = encode_srgb(s.rgb);
        if ((color_mode & 2u) != 0u) o.rgb = encode_srgb(o.rgb);
    }
    float3 blended = o.rgb + s.rgb * (1 - saturate(o.a));
    return float4(blend_linear ? encode_srgb(blended) : saturate(blended), 1);
}
"#;

/// The typed sRGB planes already accepted by the presentation path.
pub(crate) fn is_srgb(format: DXGI_FORMAT) -> bool {
    matches!(format, DXGI_FORMAT_R8G8B8A8_UNORM_SRGB | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB)
}

/// Sampling a typed sRGB SRV decodes RGB to linear. Blend in linear when
/// all present planes declare sRGB. For a mixed pair, restore the sRGB
/// plane's transfer so the plane with unknown encoding keeps its old blend.
fn color_mode(formats: [Option<DXGI_FORMAT>; 2]) -> u32 {
    let srgb = formats.map(|format| format.is_some_and(is_srgb));
    let mask = u32::from(srgb[0]) | (u32::from(srgb[1]) << 1);
    let linear = mask != 0 && formats.iter().zip(srgb).all(|(f, srgb)| f.is_none() || srgb);
    mask | (u32::from(linear) << 2)
}

/// Descriptors of as many frames as are ever in flight.
const FRAMES: u32 = 16;

struct Composer {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    views: ID3D12DescriptorHeap,
    step: u32,
    next: AtomicU32,
}
// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Composer {}
// SAFETY: as above.
unsafe impl Sync for Composer {}

fn compile(entry: PCSTR, target: PCSTR) -> windows::core::Result<ID3DBlob> {
    let mut code: Option<ID3DBlob> = None;
    let mut messages: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; the out pointer is
    // ours.
    let compiled = unsafe {
        D3DCompile(
            SOURCE.as_ptr().cast(),
            SOURCE.len(),
            s!("compose"),
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut code,
            Some(&mut messages),
        )
    };
    if compiled.is_err()
        && let Some(messages) = messages
    {
        // SAFETY: the blob owns this byte range until it is dropped.
        let bytes = unsafe {
            std::slice::from_raw_parts(messages.GetBufferPointer().cast::<u8>(), messages.GetBufferSize())
        };
        tracing::warn!(diagnostics = %String::from_utf8_lossy(bytes).trim_end_matches('\0'), "presentation shader compilation failed");
    }
    compiled?;
    code.ok_or_else(windows::core::Error::empty)
}

fn make(host: &Host, format: DXGI_FORMAT) -> windows::core::Result<Composer> {
    let (vs, ps) = (compile(s!("vs"), s!("vs_5_1"))?, compile(s!("ps"), s!("ps_5_1"))?);
    // SAFETY: the blobs' own bytes, alive through the calls.
    unsafe {
        let bytes = |b: &ID3DBlob| {
            std::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize())
        };
        let device = &host.device;
        let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes(&vs))?;
        let code = |b: &ID3DBlob| D3D12_SHADER_BYTECODE {
            pShaderBytecode: b.GetBufferPointer(),
            BytecodeLength: b.GetBufferSize(),
        };
        let mut desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            VS: code(&vs),
            PS: code(&ps),
            SampleMask: u32::MAX,
            RasterizerState: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_NONE,
                ..Default::default()
            },
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            NumRenderTargets: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        };
        desc.RTVFormats[0] = format;
        desc.BlendState.RenderTarget[0].RenderTargetWriteMask = D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8;
        let pso = device.CreateGraphicsPipelineState(&desc)?;
        let views = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            NumDescriptors: 2 * FRAMES,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
            NodeMask: 0,
        })?;
        Ok(Composer {
            root,
            pso,
            views,
            step: device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV),
            next: AtomicU32::new(0),
        })
    }
}

/// Records the planes (either may be missing), in the shader resource state,
/// into `back`, a render target of `format`.
pub(crate) fn record(
    host: &Host,
    list: &ID3D12GraphicsCommandList,
    planes: [Option<(&ID3D12Resource, DXGI_FORMAT)>; 2],
    back: &ID3D12Resource,
    (width, height, format): (u32, u32, DXGI_FORMAT),
) -> bool {
    static COMPOSER: std::sync::Mutex<Option<(DXGI_FORMAT, Option<Composer>)>> =
        std::sync::Mutex::new(None);
    let mut composer = COMPOSER.lock().unwrap_or_else(|e| e.into_inner());
    if composer.as_ref().is_none_or(|c| c.0 != format) {
        let made = make(host, format);
        if let Err(e) = &made {
            tracing::warn!(error = %e, "presented planes not put together");
        }
        *composer = Some((format, made.ok()));
    }
    let Some((_, Some(c))) = &*composer else {
        return false;
    };
    record_with(host, list, c, planes, back, (width, height));
    true
}

fn record_with(
    host: &Host,
    list: &ID3D12GraphicsCommandList,
    c: &Composer,
    planes: [Option<(&ID3D12Resource, DXGI_FORMAT)>; 2],
    back: &ID3D12Resource,
    (width, height): (u32, u32),
) {
    let first = 2 * (c.next.fetch_add(1, Ordering::Relaxed) % FRAMES);
    // SAFETY: an open list; the views are made in slots no frame in flight
    // reads; live textures.
    unsafe {
        let (cpu, gpu) = (
            c.views.GetCPUDescriptorHandleForHeapStart(),
            c.views.GetGPUDescriptorHandleForHeapStart(),
        );
        for (i, plane) in planes.iter().enumerate() {
            let at = D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: cpu.ptr + ((first as usize + i) * c.step as usize),
            };
            let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: plane.map_or(DXGI_FORMAT_R8G8B8A8_UNORM, |p| p.1),
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2D: D3D12_TEX2D_SRV {
                        MipLevels: 1,
                        ..Default::default()
                    },
                },
            };
            host.device
                .CreateShaderResourceView(plane.map(|p| p.0), Some(&desc), at);
        }
        let rtv = host.rtv(back, None);
        list.OMSetRenderTargets(1, Some(&rtv), false, None);
        list.SetDescriptorHeaps(&[Some(c.views.clone())]);
        list.SetGraphicsRootSignature(&c.root);
        list.SetPipelineState(&c.pso);
        list.SetGraphicsRoot32BitConstant(1, color_mode(planes.map(|p| p.map(|(_, f)| f))), 0);
        list.SetGraphicsRootDescriptorTable(
            0,
            D3D12_GPU_DESCRIPTOR_HANDLE {
                ptr: gpu.ptr + u64::from(first) * u64::from(c.step),
            },
        );
        list.IASetPrimitiveTopology(
            windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
        );
        list.RSSetViewports(&[D3D12_VIEWPORT {
            Width: width as f32,
            Height: height as f32,
            MaxDepth: 1.0,
            ..Default::default()
        }]);
        list.RSSetScissorRects(&[windows::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        }]);
        list.DrawInstanced(3, 1, 0, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_is_selected_only_for_declared_srgb_planes() {
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_FORMAT_B8G8R8A8_UNORM_SRGB as BGRA_SRGB,
            DXGI_FORMAT_R8G8B8A8_TYPELESS as TYPELESS,
            DXGI_FORMAT_R8G8B8A8_UNORM as UNORM,
            DXGI_FORMAT_R8G8B8A8_UNORM_SRGB as RGBA_SRGB,
        };
        assert_eq!(color_mode([Some(UNORM), Some(UNORM)]), 0);
        assert_eq!(color_mode([Some(TYPELESS), Some(UNORM)]), 0);
        assert_eq!(color_mode([Some(RGBA_SRGB), Some(UNORM)]), 1);
        assert_eq!(color_mode([Some(UNORM), Some(BGRA_SRGB)]), 2);
        assert_eq!(color_mode([None, Some(BGRA_SRGB)]), 6);
        assert_eq!(color_mode([Some(RGBA_SRGB), Some(BGRA_SRGB)]), 7);
    }

    #[test]
    fn warp_blends_typed_srgb_planes_in_linear_light() {
        let Some(host) = Host::new_warp() else { return };
        let output_format = DXGI_FORMAT_R8G8B8A8_UNORM;
        let composer = make(&host, output_format).expect("compile the presentation compositor");
        let target = D3D12_RESOURCE_STATE_RENDER_TARGET;
        let sampled = D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE;
        let make_target = |format| {
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: 1,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                ..Default::default()
            };
            let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
            let mut texture = None::<ID3D12Resource>;
            // SAFETY: our descriptors and output slot live through this call.
            unsafe {
                host.device.CreateCommittedResource(
                    &heap, D3D12_HEAP_FLAG_NONE, &desc, target, None, &mut texture,
                )
            }
            .expect("create a typed test texture");
            texture.unwrap()
        };
        let draw = |scene_format, overlay_format| {
            let scene = make_target(scene_format);
            let overlay = make_target(overlay_format);
            let back = make_target(output_format);
            let list = host.new_list().expect("record test composition");
            let (readback, data) = host.readback_buffer(256).expect("test pixel readback");
            // SAFETY: the clear targets and command list are live and in render-target state.
            unsafe {
                list.list.ClearRenderTargetView(host.rtv(&scene, None), &[1.0, 0.0, 0.0, 1.0], None);
                list.list.ClearRenderTargetView(host.rtv(&overlay, None), &[0.0, 0.0, 0.0, 0.5], None);
                list.list.ResourceBarrier(&[
                    crate::host::transition(&scene, target, sampled),
                    crate::host::transition(&overlay, target, sampled),
                ]);
            }
            record_with(&host, &list.list, &composer, [
                Some((&scene, scene_format)),
                Some((&overlay, overlay_format)),
            ], &back, (1, 1));

            // SAFETY: the location fields borrow live resources. The pixel
            // uses one row in the readback buffer at the required pitch.
            unsafe {
                list.list.ResourceBarrier(&[crate::host::transition(
                    &back, target, D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
                let source = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&back),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
                };
                let destination = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&readback),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                            Offset: 0,
                            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                                Format: output_format,
                                Width: 1,
                                Height: 1,
                                Depth: 1,
                                RowPitch: 256,
                            },
                        },
                    },
                };
                list.list.CopyTextureRegion(&destination, 0, 0, 0, &source, None);
            }
            list.close();
            host.execute(&[&list]);
            // SAFETY: the submission finished and the referenced readback is live.
            unsafe { data.cast::<[u8; 4]>().read() }
        };
        let rgba_srgb = DXGI_FORMAT_R8G8B8A8_UNORM_SRGB;
        let bgra_srgb = DXGI_FORMAT_B8G8R8A8_UNORM_SRGB;
        let linear = draw(rgba_srgb, bgra_srgb);
        let legacy = draw(output_format, output_format);
        let mixed = draw(rgba_srgb, output_format);
        assert!((186..=189).contains(&linear[0]), "linear-light red: {linear:?}");
        assert!((126..=129).contains(&legacy[0]), "legacy encoded red: {legacy:?}");
        for pixel in [linear, legacy, mixed] {
            assert_eq!(pixel[1..], [0, 0, 255]);
        }
        assert_eq!(mixed, legacy);
    }
}
