//! Console textures detiled on the card: the tiled bytes go up as they
//! are, and a compute shader writes them out in rows, where today the CPU
//! detiles every texture into its staging buffer
//! (`victory_formats::texture::Layout::detile`).
//!
//! Detiling on the GPU is AnyPS5's approach (its `TextureDetiler`,
//! github.com/boykopovar/AnyPS5); none of its code is used here: AnyPS5 is
//! GPL-2.0-only, which cannot be combined with this project's licence.
//! The addressing is our own CPU detiler's: an element's tiled offset is a
//! subresource base plus a part from its row and a part from its column
//! ([`Layout::tables`]), so the shader needs no tiling rules of its own,
//! only those two tables, and stays right for every mode the CPU detiler
//! handles.
//!
//! What a dispatch takes, all by GPU address (no descriptor heap):
//! - the tiled bytes (a raw buffer: the texture's memory, gathered or in
//!   place);
//! - its tables: the row parts, then the column parts, a `u32` each;
//! - the rows it writes (a raw buffer with unordered access), laid out as
//!   the upload's footprint: `row_pitch` bytes a row from `linear_base`.
//!
//! The rows are written in whole dwords: elements of 1 or 2 bytes are
//! packed four or two to a dword, which may write past the last element
//! of a row into its pitch (footprints keep rows 256-byte aligned).

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;

/// The shader. Constants: element count across and down, element bytes,
/// where the column parts start in the tables (bytes), the subresource's
/// base in the tiled bytes, the first row's offset in the output, the
/// output's row pitch, and whether the rows are written as zeros instead.
pub(crate) const DETILE: &str = r#"
#define RS "RootConstants(num32BitConstants=8, b0), SRV(t0), SRV(t1), UAV(u0)"
cbuffer Job : register(b0) {
    uint width;
    uint height;
    uint bpe;
    uint cols_at;
    uint tiled_base;
    uint linear_base;
    uint row_pitch;
    uint zero;
};
ByteAddressBuffer tiled : register(t0);
ByteAddressBuffer tables : register(t1);
RWByteAddressBuffer rows_out : register(u0);
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.y >= height) return;
    uint out_row = linear_base + id.y * row_pitch;
    if (zero != 0u) {
        // A level that is not the title's (levels on demand): zeros, as
        // the CPU's detile writes.
        if (bpe >= 4u) {
            if (id.x >= width) return;
            for (uint i = 0u; i < bpe; i += 4u) rows_out.Store(out_row + id.x * bpe + i, 0u);
        } else if (id.x * (4u / bpe) < width) {
            rows_out.Store(out_row + id.x * 4u, 0u);
        }
        return;
    }
    uint row = tiled_base + tables.Load(id.y * 4u);
    if (bpe >= 4u) {
        if (id.x >= width) return;
        uint from = row + tables.Load(cols_at + id.x * 4u);
        uint to = out_row + id.x * bpe;
        [unroll(4)]
        for (uint i = 0u; i < bpe; i += 4u) rows_out.Store(to + i, tiled.Load(from + i));
    } else {
        // Elements of 1 or 2 bytes: one dword of them per thread.
        uint per = 4u / bpe;
        uint x0 = id.x * per;
        if (x0 >= width) return;
        uint mask = bpe == 1u ? 0xFFu : 0xFFFFu;
        uint word = 0u;
        for (uint k = 0u; k < per; k++) {
            uint x = min(x0 + k, width - 1u);
            uint from = row + tables.Load(cols_at + x * 4u);
            uint v = (tiled.Load(from & ~3u) >> ((from & 3u) * 8u)) & mask;
            word |= v << (k * bpe * 8u);
        }
        rows_out.Store(out_row + x0 * bpe, word);
    }
}
"#;

/// The compiled shader and its pipeline.
pub(crate) struct Detiler {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

/// One subresource (one depth slice of a 3D one) to detile.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Job {
    /// Elements across and down.
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// 1, 2, 4, 8 or 16.
    pub(crate) bpe: u32,
    /// GPU address of the tiled bytes, and the subresource's base in them.
    pub(crate) tiled: u64,
    pub(crate) tiled_base: u32,
    /// GPU address of the tables: `height` row parts, then from `cols_at`
    /// (bytes) `width` column parts.
    pub(crate) tables: u64,
    pub(crate) cols_at: u32,
    /// GPU address of the output, the first row's offset in it, and its
    /// row pitch (a multiple of 4).
    pub(crate) linear: u64,
    pub(crate) linear_base: u32,
    pub(crate) row_pitch: u32,
    /// Zeros instead of the tiled bytes (a level that is not the title's).
    pub(crate) zero: bool,
}

impl Detiler {
    pub(crate) fn new(device: &ID3D12Device) -> windows::core::Result<Detiler> {
        let mut code: Option<ID3DBlob> = None;
        let mut errors: Option<ID3DBlob> = None;
        // SAFETY: the source and names live through the call; the out
        // pointers are ours.
        let compiled = unsafe {
            D3DCompile(
                DETILE.as_ptr().cast(),
                DETILE.len(),
                windows::core::s!("detile on the card"),
                None,
                None,
                windows::core::s!("main"),
                windows::core::s!("cs_5_1"),
                0,
                0,
                &mut code,
                Some(&mut errors),
            )
        };
        if let Err(e) = compiled {
            // SAFETY: the error blob's own bytes.
            let why = errors.map(|b| unsafe {
                String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize())).into_owned()
            });
            tracing::warn!(error = %e, why = why.as_deref().unwrap_or(""), "the detile shader did not compile");
            return Err(e);
        }
        let code = code.ok_or_else(windows::core::Error::empty)?;
        // SAFETY: the blob's own bytes; the root signature is inside the
        // shader.
        unsafe {
            let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
            let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
            let pso: ID3D12PipelineState = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE { pShaderBytecode: bytes.as_ptr().cast(), BytecodeLength: bytes.len() },
                ..Default::default()
            })?;
            Ok(Detiler { root, pso })
        }
    }

    /// Records `job` on `list`, with this pipeline and root signature set
    /// (the caller sets its own again after, if it had others). The output
    /// must be in the unordered access state.
    pub(crate) fn record(&self, list: &ID3D12GraphicsCommandList, job: &Job) {
        if job.width == 0 || job.height == 0 {
            return;
        }
        let threads = if job.bpe >= 4 { job.width } else { job.width.div_ceil(4 / job.bpe) };
        let constants = [
            job.width,
            job.height,
            job.bpe,
            job.cols_at,
            job.tiled_base,
            job.linear_base,
            job.row_pitch,
            u32::from(job.zero),
        ];
        // SAFETY: live objects; the addresses are the caller's live buffers.
        unsafe {
            list.SetComputeRootSignature(&self.root);
            list.SetPipelineState(&self.pso);
            list.SetComputeRoot32BitConstants(0, constants.len() as u32, constants.as_ptr().cast(), 0);
            list.SetComputeRootShaderResourceView(1, job.tiled);
            list.SetComputeRootShaderResourceView(2, job.tables);
            list.SetComputeRootUnorderedAccessView(3, job.linear);
            list.Dispatch(threads.div_ceil(8), job.height.div_ceil(8), 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use victory_formats::texture::{Desc, Dimension, Layout, Swizzle};
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC};
    use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
    use windows::core::Interface;

    fn buffer(device: &ID3D12Device, size: u64, heap: D3D12_HEAP_TYPE, uav: bool) -> ID3D12Resource {
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(4),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_UNKNOWN,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: if uav { D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS } else { D3D12_RESOURCE_FLAG_NONE },
            ..Default::default()
        };
        let state = match heap {
            D3D12_HEAP_TYPE_UPLOAD => D3D12_RESOURCE_STATE_GENERIC_READ,
            D3D12_HEAP_TYPE_READBACK => D3D12_RESOURCE_STATE_COPY_DEST,
            _ => D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        };
        let mut made: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe {
            device
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES { Type: heap, ..Default::default() },
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    state,
                    None,
                    &mut made,
                )
                .unwrap();
        }
        made.unwrap()
    }

    fn fill(resource: &ID3D12Resource, bytes: &[u8]) {
        let mut at = std::ptr::null_mut();
        // SAFETY: an upload buffer at least `bytes` long.
        unsafe {
            resource.Map(0, None, Some(&mut at)).unwrap();
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), at.cast::<u8>(), bytes.len());
            resource.Unmap(0, None);
        }
    }

    /// Detiled on the card (WARP), every subresource equals what the CPU
    /// detiler writes, for the modes and element sizes titles use.
    #[test]
    fn the_card_detiles_as_the_cpu_does() {
        let adapter = crate::host::warp_adapter().expect("WARP");
        let mut device: Option<ID3D12Device> = None;
        // SAFETY: plain device creation on the software adapter.
        unsafe { D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &mut device).expect("device") };
        let device = device.unwrap();
        let detiler = Detiler::new(&device).expect("shader");
        // SAFETY: plain object creation and use; every buffer outlives the
        // list's execution, which is waited for.
        unsafe {
            let queue: ID3D12CommandQueue =
                device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() }).unwrap();
            let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let event = CreateEventW(None, false, false, None).unwrap();
            let mut fenced = 0u64;
            let mut seed = 0x2545_f491_u32;
            let mut checked = 0;
            for swizzle in [Swizzle::LINEAR, Swizzle::SW_4KB_S, Swizzle::SW_64KB_S_X, Swizzle::SW_64KB_D_X, Swizzle::SW_64KB_R_X, Swizzle::SW_64KB_Z_X] {
                for bpe in [1u32, 2, 4, 8, 16] {
                    for (w, h, depth, mips, dimension) in [(61u32, 45u32, 2u32, 5u32, Dimension::Tex2d), (33, 17, 9, 3, Dimension::Tex3d)] {
                        let d = Desc { dimension, width: w, height: h, depth, mips, element_bytes: bpe, block: 1, samples: 1, swizzle };
                        let Ok(layout) = Layout::new(&d) else { continue };
                        if !layout.addressable() {
                            continue;
                        }
                        let tiled: Vec<u8> = (0..layout.size)
                            .map(|_| {
                                seed ^= seed << 13;
                                seed ^= seed >> 17;
                                seed ^= seed << 5;
                                seed as u8
                            })
                            .collect();
                        // Every subresource and depth slice: its tables, its
                        // place in one output, and the CPU's rows for it.
                        let mut tables: Vec<u32> = Vec::new();
                        let mut jobs = Vec::new();
                        let mut want: Vec<u8> = Vec::new();
                        let slices = if dimension == Dimension::Tex3d { 1 } else { depth };
                        for mip in 0..mips {
                            let m = layout.mips[mip as usize];
                            let pitch = (m.width * bpe).next_multiple_of(256);
                            let depth_pitch = pitch as usize * m.height as usize;
                            for slice in 0..slices {
                                let mut rows = vec![0u8; depth_pitch * m.depth as usize];
                                layout.detile(&tiled, mip, slice, &mut rows, pitch as usize, depth_pitch).unwrap();
                                let zs = if dimension == Dimension::Tex3d { m.depth } else { 1 };
                                for z in 0..zs {
                                    let (base, row_parts, column_parts) = layout.tables(mip, slice, z).unwrap();
                                    let at = (tables.len() * 4) as u32;
                                    tables.extend(&row_parts);
                                    let cols_at = (tables.len() * 4) as u32 - at;
                                    tables.extend(&column_parts);
                                    jobs.push((at, cols_at, base as u32, want.len() as u32 + z * depth_pitch as u32, pitch, m.width, m.height));
                                }
                                want.extend(&rows);
                            }
                        }
                        let tiled_buffer = buffer(&device, tiled.len() as u64, D3D12_HEAP_TYPE_UPLOAD, false);
                        fill(&tiled_buffer, &tiled);
                        let table_bytes: Vec<u8> = tables.iter().flat_map(|t| t.to_le_bytes()).collect();
                        let table_buffer = buffer(&device, table_bytes.len() as u64, D3D12_HEAP_TYPE_UPLOAD, false);
                        fill(&table_buffer, &table_bytes);
                        let out = buffer(&device, want.len() as u64, D3D12_HEAP_TYPE_DEFAULT, true);
                        let back = buffer(&device, want.len() as u64, D3D12_HEAP_TYPE_READBACK, false);
                        allocator.Reset().unwrap();
                        let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).unwrap();
                        for &(at, cols_at, base, linear_base, pitch, width, height) in &jobs {
                            detiler.record(
                                &list,
                                &Job {
                                    width,
                                    height,
                                    bpe,
                                    tiled: tiled_buffer.GetGPUVirtualAddress(),
                                    tiled_base: base,
                                    tables: table_buffer.GetGPUVirtualAddress() + u64::from(at),
                                    cols_at,
                                    linear: out.GetGPUVirtualAddress(),
                                    linear_base,
                                    row_pitch: pitch,
                                    zero: false,
                                },
                            );
                        }
                        list.ResourceBarrier(&[crate::host::transition(&out, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                        list.CopyResource(&back, &out);
                        list.Close().unwrap();
                        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
                        fenced += 1;
                        queue.Signal(&fence, fenced).unwrap();
                        fence.SetEventOnCompletion(fenced, event).unwrap();
                        WaitForSingleObject(event, INFINITE);
                        let mut at = std::ptr::null_mut();
                        back.Map(0, None, Some(&mut at)).unwrap();
                        let got = std::slice::from_raw_parts(at.cast::<u8>(), want.len());
                        // Only the elements: a row's padding is the shader's
                        // to write (packed small elements spill into it).
                        let mut same = true;
                        for &(_, _, _, linear_base, pitch, width, height) in &jobs {
                            for y in 0..height {
                                let from = (linear_base + y * pitch) as usize;
                                let len = (width * bpe) as usize;
                                same &= got[from..from + len] == want[from..from + len];
                            }
                        }
                        back.Unmap(0, None);
                        assert!(same, "{swizzle:?} bpe {bpe} {w}x{h}x{depth} {dimension:?}");
                        checked += jobs.len();
                    }
                }
            }
            assert!(checked > 50, "only {checked} subresources checked");
        }
    }
}
