//! Ray-tracing acceleration structures on the host's RTX cores, built from
//! the meshes the title hands its console builder (`XboxBvhBuilder.dll`,
//! watched in `victory_hle::modules::bvh`).
//!
//! The console builder writes its own structure format, which the host's
//! ray tracing cannot read; most of the title's bottom-level structures do
//! not even come from a build but are streamed in pre-built and
//! "rehydrated". Both calls still describe the meshes (vertex and index
//! buffers, formats, counts), so the host builds its own structure from
//! them at the same place in the title's command list, keyed by the
//! console structure's address. Nothing reads these yet: the 14 programs
//! that test rays still walk the console's structures. `VICTORY_RT_BUILD=1`
//! turns the builds on.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

use crate::object::This;

/// One triangle mesh as the console builder is given it (title addresses).
#[derive(Debug, Clone, Copy)]
pub struct Triangles {
    pub flags: u32,
    pub transform: u64,
    pub index_format: u32,
    pub vertex_format: u32,
    pub index_count: u32,
    pub vertex_count: u32,
    pub index_buffer: u64,
    pub vertex_buffer: u64,
    pub stride: u64,
}

/// Whether the host builds structures (`VICTORY_RT_BUILD=1`).
pub fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_RT_BUILD").is_some_and(|v| v == "1"))
}

/// The host structure of each console structure address, and its size.
static STRUCTURES: Mutex<Option<HashMap<u64, (ID3D12Resource, u64)>>> = Mutex::new(None);
/// Builds recorded, meshes left out, and builds given up.
static COUNTS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// The host's vertex format for the console's number: 16 is four 16-bit
/// signed-normalized values (x, y, z and one unused, run `bvh-watch3-*`),
/// 6 three floats.
fn vertex_format(console: u32) -> Option<DXGI_FORMAT> {
    match console {
        16 => Some(DXGI_FORMAT_R16G16B16A16_SNORM),
        6 => Some(DXGI_FORMAT_R32G32B32_FLOAT),
        _ => None,
    }
}

/// The console's compressed triangle stream ([`decode_compressed`]).
const COMPRESSED: u32 = 53;
/// Compressed streams decoded so far.
static DECODED: AtomicU64 = AtomicU64::new(0);

/// 57 is 16-bit indices (R16_UINT), 42 32-bit. 53 is no index buffer but
/// the console's compressed triangle stream (run `rt-indices-*`; read as
/// indices it reached far past the vertices and the host builder hung the
/// card): decoded on the CPU into 16-bit indices ([`decode_compressed`]).
fn index_format(console: u32, count: u32) -> Option<(DXGI_FORMAT, u64)> {
    match console {
        _ if count == 0 => Some((DXGI_FORMAT_UNKNOWN, 0)),
        57 | COMPRESSED => Some((DXGI_FORMAT_R16_UINT, 2)),
        42 => Some((DXGI_FORMAT_R32_UINT, 4)),
        _ => None,
    }
}

/// The console's compressed triangle stream (index format 53) at title
/// address `base` as plain 16-bit indices, `triangles` of them by three, or
/// None if it does not read. As the title builder's own `ReloadTriangles`
/// program (in `XboxBvhBuilder.dll`, read as DXIL) reads triangle `i`:
///
/// ```text
/// header  dwords h0, h1 at +0
/// table   A = ((h0 << 4) & 0x1ffff0) | 12; t = dword at A + 4i (h1 entries)
/// block   B = ((t >> 21) << 4) | 12; dwords b0, b1 at B;
///         first = (b1 & 0xffff) | ((b0 << 2) & 0x10000)
/// pool    P = A + 4 h1 + 2 first; the corners are the u16s at
///         P + 2 (t & 0x7f), P + 2 ((t >> 7) & 0x7f), P + 2 ((t >> 14) & 0x7f)
/// ```
///
/// and with bit 15 of the vertex stride set the first and last corners
/// swap (the winding flips).
fn decode_compressed(base: u64, triangles: u32, flip: bool) -> Option<Vec<u16>> {
    let read = |len: usize| {
        let mut bytes = vec![0u8; len];
        crate::host::read_into(base as *const u8, &mut bytes).then_some(bytes)
    };
    let dword = |b: &[u8], at: usize| b.get(at..at + 4).map(|w| u32::from_le_bytes(w.try_into().unwrap_or_default()));
    let head = read(12)?;
    let (h0, h1) = (dword(&head, 0)?, dword(&head, 4)?);
    if h1 < triangles || h1 > 1 << 22 {
        return None;
    }
    let a = (((h0 << 4) & 0x1f_fff0) | 12) as usize;
    let table = read(a + 4 * triangles as usize)?;
    let entries: Vec<u32> = (0..triangles as usize).map(|i| dword(&table, a + 4 * i)).collect::<Option<_>>()?;
    let block = |t: u32| (((t >> 21) << 4) | 12) as usize;
    let blocks_end = entries.iter().map(|&t| block(t) + 8).max()?;
    let blocks = read(blocks_end.max(table.len()))?;
    let pool = a + 4 * h1 as usize;
    let firsts: Vec<usize> = entries
        .iter()
        .map(|&t| {
            let (b0, b1) = (dword(&blocks, block(t))?, dword(&blocks, block(t) + 4)?);
            Some(pool + 2 * ((b1 & 0xffff) | ((b0 << 2) & 0x1_0000)) as usize)
        })
        .collect::<Option<_>>()?;
    let end = firsts.iter().max()? + 2 * 0x80;
    let all = read(end.max(blocks.len()))?;
    let u16_at = |at: usize| all.get(at..at + 2).map(|w| u16::from_le_bytes([w[0], w[1]]));
    let mut out = Vec::with_capacity(3 * triangles as usize);
    for (&t, &p) in entries.iter().zip(&firsts) {
        let corner = |shift: u32| u16_at(p + 2 * ((t >> shift) & 0x7f) as usize);
        let (c0, c1, c2) = (corner(0)?, corner(7)?, corner(14)?);
        if flip {
            out.extend([c2, c1, c0]);
        } else {
            out.extend([c0, c1, c2]);
        }
    }
    Some(out)
}

/// Meshes left out because an index reached past their vertices.
static BAD_INDICES: AtomicU64 = AtomicU64::new(0);

/// Whether every index of `m` (as the title's memory holds it now) names one of its
/// vertices: the host builder does not check, and a build given indices past the end
/// read whatever lay there (run `rt-build3-*`: the card hung after some 2,000 builds,
/// while finding the same meshes without building them ran five minutes).
/// shortcut: checked when the call is made; the list builds from the copy brought up
/// to date at submission.
fn indices_in_range(m: &Triangles, size: u64) -> bool {
    if m.index_count == 0 {
        return m.index_count % 3 == 0;
    }
    if m.index_count % 3 != 0 || m.index_count > 1 << 22 || m.vertex_count == 0 {
        return false;
    }
    let mut bytes = vec![0u8; (size * u64::from(m.index_count)) as usize];
    if !crate::host::read_into(m.index_buffer as *const u8, &mut bytes) {
        return false;
    }
    let limit = m.vertex_count;
    #[cfg(feature = "diagnostics")]
    {
        static SHOWN: AtomicU64 = AtomicU64::new(0);
        if SHOWN.fetch_add(1, Ordering::Relaxed) < 24 {
            let u16s: Vec<u16> = bytes.chunks_exact(2).take(24).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            let max16 = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).max();
            let max32 = bytes.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).max();
            tracing::info!(format = m.index_format, count = m.index_count, vertices = limit, ?max16, ?max32, "rt: indices {u16s:?}");
        }
    }
    if size == 2 {
        bytes.chunks_exact(2).all(|c| u32::from(u16::from_le_bytes([c[0], c[1]])) < limit)
    } else {
        bytes.chunks_exact(4).all(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]) < limit)
    }
}

/// The host GPU address of `len` bytes of title memory at `address`, brought
/// up to date when `list` is submitted.
fn host_address(list: &crate::host::HostList, address: u64, len: u64) -> Option<u64> {
    let shadow = crate::shadow::find(address, len.max(1))?;
    if shadow.left(address) < len {
        return None;
    }
    shadow.refresh(list, address, len);
    Some(shadow.gpu(address))
}

/// Records, in the host list of the title's list `list`, the build of a
/// bottom-level structure for the console structure at `dest` from
/// `meshes`. Returns whether it was recorded.
pub fn build_blas(list: usize, dest: u64, flags: u32, meshes: &[Triangles]) -> bool {
    let made = build(list, dest, flags, meshes);
    let n = COUNTS[usize::from(!made) * 2].fetch_add(1, Ordering::Relaxed);
    if n < 400 || n % 1000 == 0 {
        tracing::info!(n, "rt: meshes {meshes:x?}");
    }
    if n < 8 || n % 1000 == 0 {
        tracing::info!(
            made,
            n,
            dest = format_args!("{dest:#x}"),
            meshes = meshes.len(),
            left_out = COUNTS[1].load(Ordering::Relaxed),
            bad_indices = BAD_INDICES.load(Ordering::Relaxed),
            decoded = DECODED.load(Ordering::Relaxed),
            "rt: host bottom-level structure"
        );
    }
    made
}

fn build(list: usize, dest: u64, flags: u32, meshes: &[Triangles]) -> bool {
    let (Some(host), Some(list)) = (crate::host::host(), crate::list::host_list(list as This)) else {
        return false;
    };
    let (Ok(device), Ok(recorder)) = (
        host.device.cast::<ID3D12Device5>(),
        list.list.cast::<ID3D12GraphicsCommandList4>(),
    ) else {
        return false;
    };
    let mut geometry = Vec::new();
    for m in meshes {
        let (Some(vformat), Some((iformat, isize))) = (vertex_format(m.vertex_format), index_format(m.index_format, m.index_count)) else {
            COUNTS[1].fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let stride = m.stride & 0x7fff;
        let vertices = host_address(list, m.vertex_buffer, stride * u64::from(m.vertex_count));
        let indices = if m.index_count == 0 {
            Some(0)
        } else if m.index_format == COMPRESSED {
            // decoded now into the list's upload memory (16-bit indices)
            let decoded = decode_compressed(m.index_buffer, m.index_count / 3, m.stride & 0x8000 != 0)
                .filter(|d| d.iter().all(|&i| u32::from(i) < m.vertex_count));
            match decoded {
                Some(d) => list.scratch(2 * d.len() as u64).map(|(gpu, cpu)| {
                    // SAFETY: the scratch holds at least that many bytes, ours until the list resets.
                    unsafe { std::ptr::copy_nonoverlapping(d.as_ptr().cast::<u8>(), cpu, 2 * d.len()) };
                    DECODED.fetch_add(1, Ordering::Relaxed);
                    gpu
                }),
                None => {
                    BAD_INDICES.fetch_add(1, Ordering::Relaxed);
                    None
                }
            }
        } else {
            host_address(list, m.index_buffer, isize * u64::from(m.index_count))
        };
        let transform = if m.transform == 0 { Some(0) } else { host_address(list, m.transform, 48).filter(|a| a % 16 == 0) };
        let (Some(vertices), Some(indices), Some(transform)) = (vertices, indices, transform) else {
            COUNTS[1].fetch_add(1, Ordering::Relaxed);
            continue;
        };
        if m.index_format != COMPRESSED && !indices_in_range(m, isize) {
            BAD_INDICES.fetch_add(1, Ordering::Relaxed);
            COUNTS[1].fetch_add(1, Ordering::Relaxed);
            continue;
        }
        geometry.push(D3D12_RAYTRACING_GEOMETRY_DESC {
            Type: D3D12_RAYTRACING_GEOMETRY_TYPE_TRIANGLES,
            Flags: D3D12_RAYTRACING_GEOMETRY_FLAGS(m.flags as i32 & 3),
            Anonymous: D3D12_RAYTRACING_GEOMETRY_DESC_0 {
                Triangles: D3D12_RAYTRACING_GEOMETRY_TRIANGLES_DESC {
                    Transform3x4: transform,
                    IndexFormat: iformat,
                    VertexFormat: vformat,
                    IndexCount: m.index_count,
                    VertexCount: m.vertex_count,
                    IndexBuffer: indices,
                    VertexBuffer: D3D12_GPU_VIRTUAL_ADDRESS_AND_STRIDE { StartAddress: vertices, StrideInBytes: stride },
                },
            },
        });
    }
    if geometry.is_empty() {
        return false;
    }
    // `VICTORY_RT_DRY=1` (diagnostics): the meshes are found and their copies kept current,
    // but nothing is built, to tell a fault of the builds from one of the copies.
    if std::env::var_os("VICTORY_RT_DRY").is_some_and(|v| v == "1") {
        return true;
    }
    // (no updates: each call builds anew; compaction is the console's affair)
    let flags = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAGS(flags as i32 & 0x1c);
    let inputs = D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
        Type: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL,
        Flags: flags,
        NumDescs: geometry.len() as u32,
        DescsLayout: D3D12_ELEMENTS_LAYOUT_ARRAY,
        Anonymous: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0 { pGeometryDescs: geometry.as_ptr() },
    };
    let mut sizes = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO::default();
    // SAFETY: inputs and the geometry they point at live through the call.
    unsafe { device.GetRaytracingAccelerationStructurePrebuildInfo(&inputs, &mut sizes) };
    if sizes.ResultDataMaxSizeInBytes == 0 {
        return false;
    }
    let Some(result) = structure(&device, dest, sizes.ResultDataMaxSizeInBytes) else {
        return false;
    };
    let Some(scratch) = host.uav_buffer(sizes.ScratchDataSizeInBytes.max(256)) else {
        return false;
    };
    list.keep(&scratch);
    list.keep(&result);
    // SAFETY: live buffers kept by the list until it has run; geometry as above.
    unsafe {
        let desc = D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC {
            DestAccelerationStructureData: result.GetGPUVirtualAddress(),
            Inputs: inputs,
            SourceAccelerationStructureData: 0,
            ScratchAccelerationStructureData: scratch.GetGPUVirtualAddress(),
        };
        recorder.BuildRaytracingAccelerationStructure(&desc, None);
        recorder.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                    pResource: std::mem::ManuallyDrop::new(Some(result.clone())),
                }),
            },
        }]);
    }
    true
}

/// The host structure buffer for console structure `dest`, at least `size`
/// bytes (made anew when it grew).
fn structure(device: &ID3D12Device5, dest: u64, size: u64) -> Option<ID3D12Resource> {
    let mut map = STRUCTURES.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(HashMap::new);
    if let Some((buffer, have)) = map.get(&dest)
        && *have >= size
    {
        return Some(buffer.clone());
    }
    let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: plain creation of a device-local buffer.
    let made = unsafe {
        device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE,
            None,
            &mut buffer,
        )
    };
    if let Err(e) = made {
        tracing::warn!(error = %e, size, "rt: no structure buffer");
        return None;
    }
    let buffer = buffer?;
    map.insert(dest, (buffer.clone(), size));
    Some(buffer)
}
