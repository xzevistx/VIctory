//! The host's own ray-tracing structures (DXR), built beside the title's
//! (`docs/fork-dxr.md`, step 2): `VICTORY_DXR=1`.
//!
//! The title's calls into its structure builder (`XboxBvhBuilder.dll`) are
//! wrapped (`victory_hle::modules::bvh`); each batched build of bottom
//! levels is written down here ([`record`]) at its place in the title's
//! command list, and when the list is carried out on the host ([`run`]) a
//! DXR bottom level of the same geometry is built, kept by the title's
//! destination address. The title's own builder is left as it was. Nothing
//! reads these structures yet (step 4); top levels and the static world's
//! rehydrated structures are not made yet.
//!
//! What each build reads: the title's vertex, index and transform buffers
//! through their host copies (`crate::shadow`, brought up to date as draws
//! have them), and its packed indices ([`packed`], index format 53) unpacked
//! from title memory into the list's upload memory.

use std::sync::OnceLock;

/// Index format 53: the builder's packed triangles (its `LoadTriangles` and
/// `ReloadTriangles` read them; the title's descriptions say 53, its size
/// queries 57). In bytes:
///
/// - 0: a dword whose low 17 bits are the number of blocks (bit 31 set);
///   4: the number of triangles; 8: the number of 16-bit entries in the table.
/// - 12: 16 bytes a block. Its table base is the low 16 bits of its second
///   dword, with bit 14 of its first as bit 16; the rest is not read here.
/// - 12 + blocks * 16: a dword a triangle: three 7-bit places in its block's
///   part of the table (bits 0-6, 7-13, 14-20) and its block (bits 21-31).
/// - after them: the table, the vertices' numbers as 16-bit numbers.
///
/// A geometry whose vertex stride has bit 15 set has its triangles' first and
/// third corners swapped (`flip`): the builder reads that bit of the word
/// that holds the stride.
pub mod packed {
    /// The bytes of a packed index buffer whose first 12 bytes are `head`.
    pub fn len(head: &[u8]) -> Option<usize> {
        let word = |at: usize| Some(u32::from_le_bytes(head.get(at..at + 4)?.try_into().ok()?) as usize);
        let blocks = word(0)? & 0x1_ffff;
        Some(12 + blocks * 16 + word(4)? * 4 + word(8)? * 2)
    }

    /// The triangles of a packed index buffer, as vertex numbers.
    pub fn triangles(bytes: &[u8], flip: bool) -> Result<Vec<[u32; 3]>, &'static str> {
        let mut out = Vec::new();
        each(bytes, flip, |t| out.push(t))?;
        Ok(out)
    }

    /// Each triangle of a packed index buffer, as vertex numbers, in order.
    pub fn each(bytes: &[u8], flip: bool, mut f: impl FnMut([u32; 3])) -> Result<(), &'static str> {
        let word = |at: usize| {
            bytes
                .get(at..at + 4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                .ok_or("a dword past the end")
        };
        let half = |at: usize| {
            bytes
                .get(at..at + 2)
                .map(|b| u32::from(u16::from_le_bytes(b.try_into().unwrap())))
                .ok_or("a table entry past the end")
        };
        let blocks = (word(0)? & 0x1_ffff) as usize;
        let count = word(4)? as usize;
        let records = 12 + blocks * 16;
        let table = records + count * 4;
        for t in 0..count {
            let record = word(records + t * 4)?;
            let block = (record >> 21) as usize;
            if block >= blocks {
                return Err("a triangle names a block that is not there");
            }
            let head = 12 + block * 16;
            let base = (word(head + 4)? & 0xffff) | ((word(head)? << 2) & 0x1_0000);
            let at = table + base as usize * 2;
            let corner = |shift: u32| half(at + ((record >> shift) & 0x7f) as usize * 2);
            let (a, b, c) = (corner(0)?, corner(7)?, corner(14)?);
            f(if flip { [c, b, a] } else { [a, b, c] });
        }
        Ok(())
    }
}

/// `D3D12_RAYTRACING_GEOMETRY_DESC`'s triangles as the title gave them.
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    /// 1 opaque, 2 any-hit called once.
    pub flags: u32,
    pub transform: u64,
    pub index_format: u32,
    pub vertex_format: u32,
    pub index_count: u32,
    pub vertex_count: u32,
    pub index_buffer: u64,
    pub vertex_buffer: u64,
    /// With the title's bit 15 (the corners swapped, [`packed`]).
    pub vertex_stride: u64,
}

/// A bottom level the title asked its builder for.
#[derive(Clone, Debug)]
pub struct Build {
    pub dest: u64,
    pub source: u64,
    /// `D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAGS`.
    pub flags: u32,
    pub geometries: Vec<Geometry>,
}

/// `VICTORY_DXR=1`: the host's own structures are built beside the title's.
pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_DXR").is_some_and(|v| v == "1"))
}

/// Writes `builds` down at the current place of the title's command list
/// `list`, to be built when it is carried out on the host.
pub fn record(list: usize, builds: Vec<Build>) {
    if builds.is_empty() || list == 0 {
        return;
    }
    crate::list::structures(list as crate::object::This, Box::new(builds));
}

#[cfg(windows)]
pub(crate) use host::run;

#[cfg(windows)]
mod host {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::core::Interface;

    use super::{Build, Geometry, packed};
    use crate::host::HostList;

    /// Pool memory for structures comes in blocks of this many bytes (more
    /// for a structure that is bigger).
    const BLOCK: u64 = 64 << 20;
    /// DXR's alignment of structures and scratch memory.
    const ALIGN: u64 = 256;
    /// Frames a replaced structure's memory waits before it is given out again
    /// (lists recorded before may still be carried out).
    const FREE_AFTER: u64 = 8;

    const PERFORM_UPDATE: u32 = 0x20;
    const ALLOW_UPDATE: u32 = 0x1;

    fn align(n: u64) -> u64 {
        n.div_ceil(ALIGN) * ALIGN
    }

    /// `VICTORY_DXR_MB`: the most memory on the card for structures (2048).
    fn most() -> u64 {
        static MOST: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        *MOST.get_or_init(|| {
            std::env::var("VICTORY_DXR_MB").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(2048) << 20
        })
    }

    /// A structure on the host, kept by the title's destination address.
    #[derive(Clone, Copy)]
    struct Held {
        block: usize,
        offset: u64,
        size: u64,
        gpu: u64,
        flags: u32,
        geometries: usize,
    }

    struct Block {
        /// Held for as long as the pool is.
        _buffer: ID3D12Resource,
        gpu: u64,
        /// Free ranges (offset, size), by offset.
        free: BTreeMap<u64, u64>,
    }

    #[derive(Default)]
    struct Pool {
        blocks: Vec<Block>,
        held: HashMap<u64, Held>,
        /// Memory of replaced structures and the frame it was let go.
        freeing: Vec<(u64, usize, u64, u64)>,
        bytes: u64,
    }

    // SAFETY: the host's resources are free-threaded.
    unsafe impl Send for Pool {}

    static POOL: Mutex<Option<Pool>> = Mutex::new(None);

    impl Pool {
        fn take(&mut self, size: u64) -> Option<(usize, u64)> {
            for (i, b) in self.blocks.iter_mut().enumerate() {
                let found = b.free.iter().find(|&(_, &len)| len >= size).map(|(&at, &len)| (at, len));
                if let Some((at, len)) = found {
                    b.free.remove(&at);
                    if len > size {
                        b.free.insert(at + size, len - size);
                    }
                    return Some((i, at));
                }
            }
            None
        }

        fn give(&mut self, block: usize, offset: u64, size: u64) {
            let free = &mut self.blocks[block].free;
            free.insert(offset, size);
            // Joined with the ranges beside it.
            let after = offset + size;
            if let Some(len) = free.get(&after).copied() {
                free.remove(&after);
                *free.get_mut(&offset).unwrap() += len;
            }
            if let Some((&before, &len)) = free.range(..offset).next_back()
                && before + len == offset
            {
                let mine = free.remove(&offset).unwrap();
                *free.get_mut(&before).unwrap() += mine;
            }
        }

        /// Memory let go long enough ago given back.
        fn settle(&mut self, frame: u64) {
            let due: Vec<_> = self.freeing.iter().filter(|f| f.0 + FREE_AFTER <= frame).copied().collect();
            self.freeing.retain(|f| f.0 + FREE_AFTER > frame);
            for (_, block, offset, size) in due {
                self.give(block, offset, size);
            }
        }

        /// Room for a structure of `size` bytes: a range of a block, a new
        /// block made if none has one.
        fn room(&mut self, device: &ID3D12Device5, size: u64) -> Result<(usize, u64, u64), &'static str> {
            if let Some((block, at)) = self.take(size) {
                return Ok((block, at, self.blocks[block].gpu + at));
            }
            let len = size.max(BLOCK);
            if self.bytes + len > most() {
                return Err("no room (VICTORY_DXR_MB)");
            }
            let buffer = make_buffer(device, len, D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE)
                .ok_or("a block of structure memory not made")?;
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            self.bytes += len;
            let mut free = BTreeMap::new();
            if len > size {
                free.insert(size, len - size);
            }
            self.blocks.push(Block { _buffer: buffer, gpu, free });
            Ok((self.blocks.len() - 1, 0, gpu))
        }
    }

    /// A buffer on the card that shaders may write.
    fn make_buffer(device: &ID3D12Device5, len: u64, state: D3D12_RESOURCE_STATES) -> Option<ID3D12Resource> {
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: len,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut out: Option<ID3D12Resource> = None;
        // SAFETY: plain descriptions.
        let made = unsafe { device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut out) };
        if let Err(e) = &made {
            tracing::warn!(error = %e, "dxr: memory for host structures could not be made");
        }
        made.ok().and(out)
    }

    /// What has been done since the start.
    #[derive(Default)]
    struct Stats {
        ops: u64,
        builds: u64,
        updates: u64,
        /// Updates done as builds (no host structure to update from).
        rebuilt: u64,
        triangles: u64,
        unpacked: u64,
        failed: BTreeMap<&'static str, u64>,
        busy: Duration,
        last: Option<Instant>,
    }

    static STATS: Mutex<Option<Stats>> = Mutex::new(None);

    fn with_stats(f: impl FnOnce(&mut Stats)) {
        let mut s = STATS.lock().unwrap_or_else(|e| e.into_inner());
        f(s.get_or_insert_with(Stats::default));
    }

    /// Every ten seconds, what was built and what is held.
    fn summary() {
        let (held, bytes, blocks) = {
            let pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
            pool.as_ref().map_or((0, 0, 0), |p| (p.held.len(), p.bytes, p.blocks.len()))
        };
        with_stats(|s| {
            let now = Instant::now();
            if s.last.is_some_and(|t| now.duration_since(t) < Duration::from_secs(10)) {
                return;
            }
            s.last = Some(now);
            tracing::info!(
                ops = s.ops,
                builds = s.builds,
                updates = s.updates,
                rebuilt = s.rebuilt,
                triangles = s.triangles,
                unpacked = s.unpacked,
                failed = ?s.failed,
                held,
                mb = bytes >> 20,
                blocks,
                busy_ms = s.busy.as_millis() as u64,
                "dxr: host structures so far (failed by why; busy: time on our threads)"
            );
        });
    }

    /// The host address of `[address, address + len)` of title memory, through
    /// its host copy, brought up to date when the list is submitted.
    fn host_address(list: &HostList, address: u64, len: u64) -> Result<u64, &'static str> {
        let shadow = crate::shadow::find(address, len).ok_or("no host copy of a buffer")?;
        if shadow.left(address) < len {
            return Err("a buffer's host copy is shorter than the geometry");
        }
        shadow.refresh(list, address, len);
        Ok(shadow.gpu(address))
    }

    /// `len` bytes of title memory at `address`, if this process can read
    /// them all (region by region).
    fn title_bytes<'a>(address: u64, len: usize) -> Option<&'a [u8]> {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery,
        };
        let end = address.checked_add(len as u64)?;
        let mut at = address;
        while at < end {
            // SAFETY: only asks about the address.
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            let got = unsafe { VirtualQuery(at as *const _, &mut info, size_of::<MEMORY_BASIC_INFORMATION>()) };
            if got == 0 || info.State != MEM_COMMIT || info.Protect & (PAGE_NOACCESS | PAGE_GUARD) != 0 {
                return None;
            }
            at = info.BaseAddress as u64 + info.RegionSize as u64;
        }
        // SAFETY: committed and readable, region by region.
        Some(unsafe { std::slice::from_raw_parts(address as *const u8, len) })
    }

    /// The bytes of one vertex of a vertex format DXR takes.
    fn vertex_bytes(format: u32) -> Option<u64> {
        Some(match format {
            6 => 12,      // R32G32B32_FLOAT
            16 => 8,      // R32G32_FLOAT
            10 | 13 => 8, // R16G16B16A16_FLOAT, _SNORM
            34 | 37 => 4, // R16G16_FLOAT, _SNORM
            _ => return None,
        })
    }

    /// Indices written into the list's upload memory: the host address.
    fn upload_indices(list: &HostList, indices: &[u16]) -> Result<u64, &'static str> {
        let bytes = (indices.len() * 2) as u64;
        let (gpu, to) = list.scratch(bytes.max(4)).ok_or("no upload memory for indices")?;
        // SAFETY: `to` is mapped upload memory of at least `bytes`.
        unsafe { std::ptr::copy_nonoverlapping(indices.as_ptr().cast::<u8>(), to, bytes as usize) };
        Ok(gpu)
    }

    /// The host's description of one geometry.
    fn geometry(list: &HostList, g: &Geometry, triangles: &mut u64, unpacked: &mut u64) -> Result<D3D12_RAYTRACING_GEOMETRY_DESC, &'static str> {
        let stride = g.vertex_stride & 0x7fff;
        let flip = g.vertex_stride & 0x8000 != 0;
        let each = vertex_bytes(g.vertex_format).ok_or("a vertex format DXR does not take")?;
        if g.vertex_count == 0 || stride < each {
            return Err("no vertices, or a stride below a vertex");
        }
        let vertex_len = u64::from(g.vertex_count - 1) * stride + each;
        let vertices = host_address(list, g.vertex_buffer, vertex_len)?;
        let (index_format, index_count, index_buffer) = match g.index_format {
            0 => (DXGI_FORMAT_UNKNOWN, 0, 0),
            53 => {
                let head = title_bytes(g.index_buffer, 12).ok_or("packed indices not readable")?;
                let len = packed::len(head).filter(|&n| n <= 64 << 20).ok_or("packed indices too long")?;
                let bytes = title_bytes(g.index_buffer, len).ok_or("packed indices not readable")?;
                let mut out = Vec::with_capacity(g.index_count as usize);
                packed::each(bytes, flip, |t| out.extend(t.map(|v| v as u16)))?;
                if out.len() != g.index_count as usize {
                    return Err("packed indices not as many as the description says");
                }
                *unpacked += 1;
                (DXGI_FORMAT_R16_UINT, out.len() as u32, upload_indices(list, &out)?)
            }
            57 | 42 => {
                let wide = g.index_format == 42;
                let size = if wide { 4 } else { 2 };
                let len = u64::from(g.index_count) * size;
                if flip {
                    // The corners swapped: written out again.
                    let bytes = title_bytes(g.index_buffer, len as usize).ok_or("indices not readable")?;
                    let read = |i: usize| -> u32 {
                        if wide {
                            u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap())
                        } else {
                            u32::from(u16::from_le_bytes(bytes[i * 2..i * 2 + 2].try_into().unwrap()))
                        }
                    };
                    if wide && g.vertex_count > 0x1_0000 {
                        return Err("swapped corners of 32-bit indices past 65536 vertices");
                    }
                    let mut out = Vec::with_capacity(g.index_count as usize);
                    for t in 0..g.index_count as usize / 3 {
                        out.extend([read(t * 3 + 2), read(t * 3 + 1), read(t * 3)].map(|v| v as u16));
                    }
                    (DXGI_FORMAT_R16_UINT, out.len() as u32, upload_indices(list, &out)?)
                } else {
                    let format = if wide { DXGI_FORMAT_R32_UINT } else { DXGI_FORMAT_R16_UINT };
                    (format, g.index_count, host_address(list, g.index_buffer, len)?)
                }
            }
            _ => return Err("an index format not known"),
        };
        let transform = if g.transform == 0 { 0 } else { host_address(list, g.transform, 48)? };
        *triangles += u64::from(if index_count == 0 { g.vertex_count } else { index_count } / 3);
        Ok(D3D12_RAYTRACING_GEOMETRY_DESC {
            Type: D3D12_RAYTRACING_GEOMETRY_TYPE_TRIANGLES,
            Flags: D3D12_RAYTRACING_GEOMETRY_FLAGS(g.flags as i32 & 3),
            Anonymous: D3D12_RAYTRACING_GEOMETRY_DESC_0 {
                Triangles: D3D12_RAYTRACING_GEOMETRY_TRIANGLES_DESC {
                    Transform3x4: transform,
                    IndexFormat: index_format,
                    VertexFormat: DXGI_FORMAT(g.vertex_format as i32),
                    IndexCount: index_count,
                    VertexCount: g.vertex_count,
                    IndexBuffer: index_buffer,
                    VertexBuffer: D3D12_GPU_VIRTUAL_ADDRESS_AND_STRIDE { StartAddress: vertices, StrideInBytes: stride },
                },
            },
        })
    }

    /// A build made ready: its geometry, its sizes, what it updates.
    struct Ready {
        dest: u64,
        flags: u32,
        geometries: Vec<D3D12_RAYTRACING_GEOMETRY_DESC>,
        size: u64,
        scratch: u64,
        /// The host structure updated, if one is.
        from: Option<Held>,
    }

    impl Ready {
        fn inputs(&self) -> D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
            D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
                Type: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL,
                Flags: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAGS(self.flags as i32),
                NumDescs: self.geometries.len() as u32,
                DescsLayout: D3D12_ELEMENTS_LAYOUT_ARRAY,
                Anonymous: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0 {
                    pGeometryDescs: self.geometries.as_ptr(),
                },
            }
        }
    }

    fn ready(list: &HostList, device: &ID3D12Device5, b: &Build, s: &mut Stats) -> Result<Ready, &'static str> {
        let mut geometries = Vec::with_capacity(b.geometries.len());
        for g in &b.geometries {
            geometries.push(geometry(list, g, &mut s.triangles, &mut s.unpacked)?);
        }
        let mut flags = b.flags & 0x3f;
        let mut from = None;
        if flags & PERFORM_UPDATE != 0 {
            s.updates += 1;
            let key = if b.source != 0 { b.source } else { b.dest };
            let pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
            from = pool
                .as_ref()
                .and_then(|p| p.held.get(&key).copied())
                .filter(|h| h.flags & ALLOW_UPDATE != 0 && h.geometries == geometries.len());
            if from.is_none() {
                flags &= !PERFORM_UPDATE;
                s.rebuilt += 1;
            }
        }
        let mut r = Ready { dest: b.dest, flags, geometries, size: 0, scratch: 0, from };
        let inputs = r.inputs();
        let mut info = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO::default();
        // SAFETY: the inputs point at `r.geometries`, alive here.
        unsafe { device.GetRaytracingAccelerationStructurePrebuildInfo(&inputs, &mut info) };
        if info.ResultDataMaxSizeInBytes == 0 {
            return Err("the host gave no size for it");
        }
        r.size = align(info.ResultDataMaxSizeInBytes);
        r.scratch = align(if r.from.is_some() { info.UpdateScratchDataSizeInBytes } else { info.ScratchDataSizeInBytes }.max(1));
        if let Some(h) = r.from
            && h.size < r.size
        {
            return Err("an update bigger than the structure it updates");
        }
        Ok(r)
    }

    /// Where a build at the title's `dest` goes on the host: the structure
    /// held there if it is big enough (and updated in place), else new room
    /// (the one held there let go).
    fn place(device: &ID3D12Device5, r: &Ready, frame: u64) -> Result<Held, &'static str> {
        let mut guard = POOL.lock().unwrap_or_else(|e| e.into_inner());
        let pool = guard.get_or_insert_with(Pool::default);
        pool.settle(frame);
        if let Some(h) = pool.held.get(&r.dest).copied()
            && h.size >= r.size
        {
            let h = Held { flags: r.flags & !PERFORM_UPDATE, geometries: r.geometries.len(), ..h };
            pool.held.insert(r.dest, h);
            return Ok(h);
        }
        let (block, offset, gpu) = pool.room(device, r.size)?;
        if let Some(old) = pool.held.remove(&r.dest) {
            pool.freeing.push((frame, old.block, old.offset, old.size));
        }
        let h = Held { block, offset, size: r.size, gpu, flags: r.flags & !PERFORM_UPDATE, geometries: r.geometries.len() };
        pool.held.insert(r.dest, h);
        Ok(h)
    }

    /// The list's scratch memory on the card for structure builds, at least
    /// `len` bytes (a bigger one made, the old one kept until the list is reset).
    fn scratch(list: &HostList, device: &ID3D12Device5, len: u64) -> Option<u64> {
        let mut s = list.structure_scratch.lock().unwrap_or_else(|e| e.into_inner());
        if s.as_ref().is_none_or(|(_, size)| *size < len) {
            if let Some((old, _)) = s.take() {
                list.keep(&old);
            }
            let size = len.next_power_of_two().max(1 << 20);
            let buffer = make_buffer(device, size, D3D12_RESOURCE_STATE_UNORDERED_ACCESS)?;
            *s = Some((buffer, size));
        }
        // SAFETY: a live buffer.
        s.as_ref().map(|(b, _)| unsafe { b.GetGPUVirtualAddress() })
    }

    fn uav_barrier(list: &ID3D12GraphicsCommandList4) {
        let barrier = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER { pResource: std::mem::ManuallyDrop::new(None) }),
            },
        };
        // SAFETY: a list being recorded; a barrier on every resource.
        unsafe { list.ResourceBarrier(&[barrier]) };
    }

    /// Builds `builds` into the host list `list`, at its current place.
    pub(crate) fn run(list: &HostList, builds: &[Build]) {
        let started = Instant::now();
        let Some(host) = crate::host::host() else { return };
        let (Ok(device), Ok(list4)) = (host.device.cast::<ID3D12Device5>(), list.list.cast::<ID3D12GraphicsCommandList4>()) else {
            with_stats(|s| *s.failed.entry("no ray-tracing device or list").or_default() += 1);
            return;
        };
        let frame = crate::frame::current();
        let mut readies = Vec::with_capacity(builds.len());
        with_stats(|s| {
            s.ops += 1;
            for b in builds {
                match ready(list, &device, b, s) {
                    Ok(r) => readies.push(r),
                    Err(why) => *s.failed.entry(why).or_default() += 1,
                }
            }
        });
        if !readies.is_empty() {
            let total: u64 = readies.iter().map(|r| r.scratch).sum();
            match scratch(list, &device, total) {
                None => with_stats(|s| *s.failed.entry("no scratch memory").or_default() += readies.len() as u64),
                Some(scratch) => {
                    // What the list's work before wrote (moved vertices), first.
                    uav_barrier(&list4);
                    let mut at = 0;
                    let mut built = 0;
                    for r in &readies {
                        let to = match place(&device, r, frame) {
                            Ok(to) => to,
                            Err(why) => {
                                with_stats(|s| *s.failed.entry(why).or_default() += 1);
                                continue;
                            }
                        };
                        let desc = D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC {
                            DestAccelerationStructureData: to.gpu,
                            Inputs: r.inputs(),
                            SourceAccelerationStructureData: r.from.map_or(0, |h| h.gpu),
                            ScratchAccelerationStructureData: scratch + at,
                        };
                        // SAFETY: a list being recorded; the structure's room, its
                        // source and scratch memory are the host's, sized by its
                        // own prebuild answer; the inputs point at `r.geometries`.
                        unsafe { list4.BuildRaytracingAccelerationStructure(&desc, None) };
                        at += r.scratch;
                        built += 1;
                    }
                    // The structures built, before anything reads them, and the
                    // scratch memory before the next build uses it again.
                    uav_barrier(&list4);
                    with_stats(|s| s.builds += built);
                }
            }
        }
        with_stats(|s| s.busy += started.elapsed());
        summary();
    }
}

#[cfg(test)]
mod tests {
    use super::packed;

    /// A packed index buffer of `blocks` (first dword, second dword) and
    /// `records`, with `table`.
    fn pack(blocks: &[(u32, u32)], records: &[u32], table: &[u16]) -> Vec<u8> {
        let mut out = Vec::new();
        for w in [0x8000_0000 | blocks.len() as u32, records.len() as u32, table.len() as u32] {
            out.extend(w.to_le_bytes());
        }
        for &(a, b) in blocks {
            for w in [a, b, 0, 0] {
                out.extend(w.to_le_bytes());
            }
        }
        records.iter().for_each(|r| out.extend(r.to_le_bytes()));
        table.iter().for_each(|t| out.extend(t.to_le_bytes()));
        out
    }

    fn record(block: u32, a: u32, b: u32, c: u32) -> u32 {
        block << 21 | c << 14 | b << 7 | a
    }

    #[test]
    fn packed_triangles_read_their_blocks_part_of_the_table() {
        // Block 0 from entry 0, block 1 from entry 4.
        let bytes = pack(
            &[(0, 0), (0, 4)],
            &[record(0, 0, 1, 2), record(0, 1, 3, 2), record(1, 0, 1, 2)],
            &[10, 11, 12, 13, 40, 41, 42],
        );
        assert_eq!(packed::len(&bytes[..12]), Some(bytes.len()));
        assert_eq!(
            packed::triangles(&bytes, false),
            Ok(vec![[10, 11, 12], [11, 13, 12], [40, 41, 42]])
        );
        assert_eq!(packed::triangles(&bytes, true).unwrap()[0], [12, 11, 10]);
    }

    #[test]
    fn a_block_base_past_65535_takes_bit_14_of_its_first_dword() {
        let mut table = vec![0u16; 0x1_0002];
        table[0x1_0001] = 7;
        let bytes = pack(&[(1 << 14, 1)], &[record(0, 0, 0, 0)], &table);
        assert_eq!(packed::triangles(&bytes, false), Ok(vec![[7, 7, 7]]));
    }

    #[test]
    fn a_packed_buffer_cut_short_or_naming_no_block_is_refused() {
        let bytes = pack(&[(0, 0)], &[record(1, 0, 0, 0)], &[1, 2, 3]);
        assert!(packed::triangles(&bytes, false).is_err());
        let bytes = pack(&[(0, 0)], &[record(0, 0, 1, 2)], &[1, 2, 3]);
        assert!(packed::triangles(&bytes[..bytes.len() - 2], false).is_err());
    }
}
