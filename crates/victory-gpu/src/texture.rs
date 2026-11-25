//! Texture contents the title writes itself (decision 0007, "Texture
//! memory"): streamed pages, layout-library copies, movie frames, glyph
//! atlases. Such a texture lives in console memory in its tiled layout;
//! before a submission samples its host copy, the memory is detiled into a
//! staging buffer when its content changed, and the staging buffer is
//! copied into the host texture.
//!
//! "Changed" means a page of the memory changed since the last detiling
//! ([`crate::watch`], which follows writes through every mapping of the
//! same physical pages) in the runtime's memory; elsewhere, a content hash
//! of the whole memory at every submission that samples it.
//!
//! A change detiles only the subresources whose bytes it touched, and
//! tracked memory reads back only the pages written: the title streams
//! textures in place, a mip or a few pages at a time, as the view moves
//! (finer mips for what comes near), and detiling the whole texture for
//! each of those writes cost frames. Subresources are grouped into parts
//! of overlapping tiled bytes (the mip tail's block, a thick volume's
//! slabs), each changed or not as a whole.
//! Textures read through descriptor tables are brought up to date, and for
//! shaders that index the heap themselves every texture with a resource
//! object the heap views (`crate::bindless`); textures the title placed in
//! memory itself are not made for the heap (see `descriptor::host`). Plane
//! 0 is uploaded (every mip and array slice), depth and stencil planes are
//! not.

use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::*;

use crate::host::{HostList, host};
use crate::object::This;
use windows::core::Interface;

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

/// A texture's upload state: the staging buffer laid out as the host
/// texture's subresources, and what was last detiled.
pub(crate) struct Staging {
    buffer: ID3D12Resource,
    /// Its bytes.
    size: u64,
    data: *mut u8,
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    layout: victory_formats::texture::Layout,
    /// Mips per array slice.
    mips: u32,
    memory: *const u8,
    /// The memory's pages changed since detiled (none: not the runtime's
    /// memory, hashed instead).
    tracked: Option<crate::watch::Tracked>,
    /// The subresources by the tiled bytes they read ([`parts`]).
    parts: Vec<Part>,
    last: Mutex<Last>,
    /// Whether it was reported streamed in part (diagnostics).
    #[cfg(feature = "diagnostics")]
    reported: std::sync::atomic::AtomicBool,
}

/// Subresources (indices into the footprints) whose tiled bytes overlap,
/// and the bytes `[from, to)` of the texture's memory they read: one
/// changes only if those bytes do.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Part {
    from: usize,
    to: usize,
    subresources: Vec<u32>,
}

/// What a texture's staging buffer holds: the hash of each part's memory
/// detiled (memory not tracked), or the memory as last read (tracked:
/// pages not written since stay as read).
#[derive(Default)]
struct Last {
    hashes: Vec<Option<blake3::Hash>>,
    read: Vec<u8>,
    /// `read` holds the whole memory once.
    filled: bool,
}

/// Texture refreshes since the last frame-rate line: those that detiled,
/// the subresources they detiled, and the subresources they left as they
/// were (their bytes did not change). The bytes read are in
/// `host::PREPARING`.
pub(crate) static REFRESHES: [std::sync::atomic::AtomicU64; 3] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Groups subresources (`extents[i]`: the tiled bytes subresource `i`
/// reads, as offsets into the texture's memory) into parts of overlapping
/// bytes, by first byte.
fn parts(extents: &[(usize, usize)]) -> Vec<Part> {
    let mut order: Vec<u32> = (0..extents.len() as u32).collect();
    order.sort_by_key(|&i| extents[i as usize]);
    let mut parts: Vec<Part> = Vec::new();
    for i in order {
        let (from, to) = extents[i as usize];
        match parts.last_mut() {
            Some(p) if from < p.to => {
                p.to = p.to.max(to);
                p.subresources.push(i);
            }
            _ => parts.push(Part {
                from,
                to,
                subresources: vec![i],
            }),
        }
    }
    parts
}

/// Which of `parts` the written byte ranges `runs` (offsets into the
/// texture's memory) touch.
fn touched(parts: &[Part], runs: &[(usize, usize)]) -> Vec<bool> {
    parts
        .iter()
        .map(|p| runs.iter().any(|&(a, b)| a < p.to && p.from < b))
        .collect()
}

// SAFETY: the staging mapping is written only at submission, under
// `last`'s lock; the title memory is only read.
unsafe impl Send for Staging {}
// SAFETY: as above.
unsafe impl Sync for Staging {}

/// A host copy or resolve writes `resource`: its host texture's contents
/// are the host's, not title memory's, which stays as it was.
pub(crate) fn host_written(resource: This) {
    if let Some(copy) = crate::resource::host_copy(resource) {
        copy.host_written.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

fn is_host_written(resource: This) -> bool {
    crate::resource::host_copy(resource)
        .is_some_and(|c| c.host_written.load(std::sync::atomic::Ordering::Relaxed))
}

/// Whether `resource`'s host copy takes its contents from title memory:
/// a texture the host GPU does not render to or write.
pub(crate) fn title_written(texture: &ID3D12Resource) -> bool {
    const TARGET_DEPTH_UNORDERED: i32 = 0x7;
    // SAFETY: a live resource.
    unsafe { texture.GetDesc() }.Flags.0 & TARGET_DEPTH_UNORDERED == 0
}

/// Copies [`alias_copy`] recorded since the last frame-rate line.
pub(crate) static ALIAS_COPIES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Records, once per list, the copy of `from`'s host texture into `to`'s:
/// two title textures at one memory that cannot share a host texture
/// (`resource::alias_source`: block-compressed against plain formats of
/// the same bytes, or a cast the host refused), `to` (a placed view's)
/// reading what the GPU wrote through `from`. Through a buffer, each
/// subresource with its own format, as many as have the same bytes per
/// row and rows in both; `to` no longer takes its contents from title
/// memory.
pub(crate) fn alias_copy(list: &HostList, from: This, to: This) {
    const COPY_DEST: u32 = 0x400;
    const COPY_SOURCE: u32 = 0x800;
    let (Some(a), Some(b), Some(host)) = (
        crate::resource::host_resource(from),
        crate::resource::host_resource(to),
        host(),
    ) else {
        return;
    };
    // An odd key: textures' own first uses are keyed by the host texture.
    if !list.first_use(to as usize | 1) {
        return;
    }
    // SAFETY: live resources.
    let (da, db) = unsafe { (a.GetDesc(), b.GetDesc()) };
    let count = |d: &D3D12_RESOURCE_DESC| {
        if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
            u32::from(d.MipLevels)
        } else {
            u32::from(d.MipLevels) * u32::from(d.DepthOrArraySize).max(1)
        }
    };
    let footprints = |d: &D3D12_RESOURCE_DESC, n: u32| {
        let mut f = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); n as usize];
        let mut rows = vec![0u32; n as usize];
        let mut total = 0;
        // SAFETY: outputs sized for `n` subresources.
        unsafe {
            host.device
                .GetCopyableFootprints(d, 0, n, 0, Some(f.as_mut_ptr()), Some(rows.as_mut_ptr()), None, Some(&mut total));
        }
        (f, rows, total)
    };
    let (fa, ra, total) = footprints(&da, count(&da));
    let (fb, rb, _) = footprints(&db, count(&db));
    // Subresources in order while both have the same bytes there (mips of
    // a block-compressed texture past its last whole block do not).
    let n = (0..fa.len().min(fb.len()))
        .take_while(|&i| {
            let (x, y) = (fa[i], fb[i]);
            x.Offset == y.Offset && x.Footprint.RowPitch == y.Footprint.RowPitch && ra[i] == rb[i] && x.Footprint.Depth == y.Footprint.Depth
        })
        .count() as u32;
    if n == 0 {
        once!(?da, ?db, "an aliased texture not copied: its layouts differ on the host");
        return;
    }
    let Some(buffer) = host.default_buffer(total) else { return };
    list.keep(&buffer);
    crate::texture::host_written(to);
    crate::list::ensure_state(list, from, COPY_SOURCE);
    crate::list::ensure_state(list, to, COPY_DEST);
    let place = |r: &ID3D12Resource, f: D3D12_PLACED_SUBRESOURCE_FOOTPRINT| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the copy does not release.
        pResource: unsafe { std::mem::transmute_copy(r) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: f },
    };
    let sub = |r: &ID3D12Resource, i: u32| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(r) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: i },
    };
    let l = &list.list;
    // SAFETY: an open list; live resources in the states named.
    unsafe {
        for i in 0..n {
            l.CopyTextureRegion(&place(&buffer, fa[i as usize]), 0, 0, 0, &sub(a, i), None);
        }
        l.ResourceBarrier(&[crate::host::transition(
            &buffer,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        for i in 0..n {
            l.CopyTextureRegion(&sub(b, i), 0, 0, 0, &place(&buffer, fb[i as usize]), None);
        }
    }
    ALIAS_COPIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    static LOGGED: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());
    let mut logged = LOGGED.lock().unwrap_or_else(|e| e.into_inner());
    if logged.len() < 64 && !logged.contains(&(from as usize, to as usize)) {
        logged.push((from as usize, to as usize));
        tracing::info!(from = ?from, to = ?to, a = da.Format.0, b = db.Format.0, w = da.Width, h = da.Height, subresources = n, "aliased texture copied");
    }
}

/// Records, once per list, the copy of `resource`'s staging buffer into
/// its host copy, and the detiling that refreshes the staging buffer at
/// submission if the memory changed. Leaves the texture in the copy
/// destination state.
pub(crate) fn upload(list: &HostList, resource: This, texture: &ID3D12Resource) {
    // shortcut: a texture the host copied into keeps the host's contents
    // even if the title later writes its memory too.
    // Once per list for each host texture, however many title textures of
    // its memory the list samples.
    if is_host_written(resource) || !list.first_use(texture.as_raw() as usize) {
        return;
    }
    let Some(staging) = staging(resource, texture) else {
        return;
    };
    // The staging buffer lives as long as `staging`, which the deferred
    // refresh below holds until the list is reset.
    record(list, resource, texture, &staging, None);
    // Once per submission however many of its lists sample the texture.
    list.defer_texture(staging);
}

/// For textures shaders reach through the heap itself, at submission
/// (`crate::bindless::uploads`): brings the staging buffer up to date and
/// records its copy into `list` only if the memory changed since (or was
/// never uploaded). True if it recorded; the texture is then in the copy
/// destination state.
pub(crate) fn upload_changed(list: &HostList, resource: This, texture: &ID3D12Resource) -> bool {
    if is_host_written(resource) {
        return false;
    }
    let Some(staging) = staging(resource, texture) else {
        return false;
    };
    // Only the subresources that changed: the host texture keeps the rest.
    let detiled = staging.refresh_detiled();
    if detiled.is_empty() {
        return false;
    }
    record(list, resource, texture, &staging, Some(&detiled));
    list.keep(&staging.buffer);
    true
}

/// Records the copy of every subresource of `staging` (or of `only`) into
/// `texture`, moving it to the copy destination state first.
fn record(list: &HostList, resource: This, texture: &ID3D12Resource, staging: &Staging, only: Option<&[u32]>) {
    const COPY_DEST: u32 = 0x400;
    crate::list::ensure_state(list, resource, COPY_DEST);
    for (i, footprint) in staging.footprints.iter().enumerate() {
        if only.is_some_and(|only| !only.contains(&(i as u32))) {
            continue;
        }
        #[cfg(feature = "diagnostics")]
        {
            let f = footprint.Footprint;
            // SAFETY: live resources.
            let (gpu, d) = unsafe { (staging.buffer.GetGPUVirtualAddress(), texture.GetDesc()) };
            let len = u64::from(f.RowPitch) * u64::from(staging.rows[i]) * u64::from(f.Depth);
            list.note_read(
                || {
                    format!(
                        "texture upload {resource:?} ({}x{}x{} f{} mips {}) sub {i} of {} from staging {gpu:#x}+{:#x}: offset {:#x} {}x{}x{} pitch {}",
                        d.Width,
                        d.Height,
                        d.DepthOrArraySize,
                        d.Format.0,
                        d.MipLevels,
                        staging.footprints.len(),
                        staging.size,
                        footprint.Offset,
                        f.Width,
                        f.Height,
                        f.Depth,
                        f.RowPitch
                    )
                },
                (gpu + footprint.Offset, len),
            );
        }
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: i as u32,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(&staging.buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: *footprint,
            },
        };
        // SAFETY: an open list; both locations name live resources.
        unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
}

impl Staging {
    /// Detiles the texture memory into the staging buffer if it changed
    /// since the last time (and is mapped at all): whether it did.
    pub(crate) fn refresh(&self) -> bool {
        !self.refresh_detiled().is_empty()
    }

    /// [`Staging::refresh`], returning the subresources detiled.
    fn refresh_detiled(&self) -> Vec<u32> {
        use std::sync::atomic::Ordering::Relaxed;
        let started = std::time::Instant::now();
        let (read, detiled) = self.refresh_now();
        let preparing = &crate::host::PREPARING;
        preparing[2].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
        preparing[3].fetch_add(read as u64, Relaxed);
        detiled
    }

    /// Returns the bytes of title memory read, and the subresources it
    /// detiled.
    fn refresh_now(&self) -> (usize, Vec<u32>) {
        let size = self.layout.size as usize;
        let start = self.memory as usize;
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let last = &mut *last;
        if let Some(t) = &self.tracked {
            let runs: Vec<(usize, usize)> = t
                .take(start, start + size)
                .into_iter()
                .map(|(a, b)| (a - start, b - start))
                .collect();
            if runs.is_empty() {
                return (0, Vec::new());
            }
            // Read through the copy, which stands unmapped pages: memory of
            // big pools is too slow to ask page by page. Once the whole
            // memory is read, only the pages written since: the others are
            // guarded, so as they were.
            let whole = !last.filled || last.read.len() != size;
            last.read.resize(size, 0);
            let read = if whole {
                crate::host::copy_mapped(last.read.as_mut_ptr(), self.memory, size, false);
                last.filled = true;
                size
            } else {
                for &(a, b) in &runs {
                    // SAFETY: `take` cuts the runs to the memory, so both
                    // ranges are inside `read` and the texture's memory.
                    let (to, from) = unsafe { (last.read.as_mut_ptr().add(a), self.memory.add(a)) };
                    crate::host::copy_mapped(to, from, b - a, false);
                }
                runs.iter().map(|&(a, b)| b - a).sum()
            };
            let dirty = if whole {
                vec![true; self.parts.len()]
            } else {
                touched(&self.parts, &runs)
            };
            let detiled = self.detile(&last.read, &dirty);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexWatched, read as u64);
            return (read, detiled);
        }
        if !committed(self.memory, size) {
            #[cfg(feature = "diagnostics")]
            tracing::debug!(
                memory = ?self.memory,
                size,
                desc = ?self.layout.desc,
                "a sampled texture's memory is not committed"
            );
            return (0, Vec::new());
        }
        // SAFETY: committed memory of the texture's layout size.
        let tiled = unsafe { std::slice::from_raw_parts(self.memory, size) };
        // Each part hashed on its own: a part whose bytes did not change
        // keeps what it detiled to.
        last.hashes.resize(self.parts.len(), None);
        let dirty: Vec<bool> = self
            .parts
            .iter()
            .zip(&mut last.hashes)
            .map(|(p, held)| {
                let hash = blake3::hash(&tiled[p.from..p.to]);
                held.replace(hash) != Some(hash)
            })
            .collect();
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
        let detiled = self.detile(tiled, &dirty);
        #[cfg(feature = "diagnostics")]
        if !detiled.is_empty() {
            crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
        }
        (size, detiled)
    }

    /// Detiles the subresources of the `dirty` parts from `tiled` (the
    /// texture's memory) into the staging buffer: those it detiled.
    fn detile(&self, tiled: &[u8], dirty: &[bool]) -> Vec<u32> {
        use std::sync::atomic::Ordering::Relaxed;
        let mut done = Vec::new();
        let mut kept = 0u64;
        'parts: for (part, _) in self.parts.iter().zip(dirty).filter(|(_, d)| **d) {
            for &index in &part.subresources {
                let i = index as usize;
                let (Some(footprint), Some(&rows)) = (self.footprints.get(i), self.rows.get(i)) else {
                    continue;
                };
                let (mip, slice) = (i as u32 % self.mips, i as u32 / self.mips);
                let f = footprint.Footprint;
                let pitch = f.RowPitch as usize;
                let len = subresource_len(footprint.Offset, pitch, rows, f.Depth, self.size);
                // SAFETY: `subresource_len` clamps `len` to `self.size - footprint.Offset`,
                // keeping the slice strictly inside the mapped staging buffer even when
                // GetCopyableFootprints does not pad the final row of the last subresource.
                let linear = unsafe {
                    std::slice::from_raw_parts_mut(self.data.add(footprint.Offset as usize), len)
                };
                if let Err(e) =
                    self.layout
                        .detile(tiled, mip, slice, linear, pitch, pitch * rows as usize)
                {
                    tracing::warn!(?e, mip, slice, "texture not detiled");
                    break 'parts;
                }
                done.push(index);
            }
        }
        for (part, _) in self.parts.iter().zip(dirty).filter(|(_, d)| !**d) {
            kept += part.subresources.len() as u64;
        }
        if !done.is_empty() {
            REFRESHES[0].fetch_add(1, Relaxed);
            REFRESHES[1].fetch_add(done.len() as u64, Relaxed);
            REFRESHES[2].fetch_add(kept, Relaxed);
            #[cfg(feature = "diagnostics")]
            self.report_streamed(tiled);
        }
        done
    }

    /// Reports (once per texture, the first 64 textures) a texture whose
    /// finer mips are all zero while a coarser one is not: streamed in part,
    /// its finest mips not loaded (yet, or a failed read). Sampled there
    /// without a clamp keeping the sample off them, it reads black or
    /// garbage up close while the distance looks right; with views of all
    /// mips, the texture wants the title's clamp (`_cl` samples, the view's
    /// minimum level). All zero: not streamed at all.
    #[cfg(feature = "diagnostics")]
    fn report_streamed(&self, tiled: &[u8]) {
        use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
        static REPORTED: AtomicU32 = AtomicU32::new(0);
        if self.reported.load(Relaxed) || REPORTED.load(Relaxed) >= 64 {
            return;
        }
        // Mips of the first slice, finest first (each tail mip its share of
        // the tail block; a mip without a range counts as loaded).
        let empty: Vec<bool> = (0..self.mips)
            .map(|mip| {
                self.layout.tiled_extent(mip, 0).is_ok_and(|(from, to)| {
                    tiled
                        .get(from as usize..to as usize)
                        .is_some_and(|bytes| bytes.iter().all(|&b| b == 0))
                })
            })
            .collect();
        let Some(coarse) = empty.iter().rposition(|e| !e) else {
            if !empty.is_empty() {
                tracing::debug!(
                    memory = ?self.memory,
                    desc = ?self.layout.desc,
                    "a sampled texture's memory is all zero (not streamed yet, or a failed BCPack read)"
                );
            }
            return;
        };
        let missing: Vec<usize> = (0..coarse).filter(|&m| empty[m]).collect();
        if missing.is_empty() || self.reported.swap(true, Relaxed) {
            return;
        }
        REPORTED.fetch_add(1, Relaxed);
        tracing::info!(
            memory = ?self.memory,
            desc = ?self.layout.desc,
            ?missing,
            loaded_from = coarse,
            "a sampled texture is streamed in part: these mips are all zero, coarser ones are not"
        );
    }
}

/// Bytes available in the staging buffer for a subresource at `offset`.
/// D3D12's `GetCopyableFootprints` omits trailing row-pitch padding on the
/// final row of the final subresource, so `pitch * rows * depth` can exceed
/// `total - offset`.
fn subresource_len(offset: u64, pitch: usize, rows: u32, depth: u32, total: u64) -> usize {
    let padded = pitch
        .saturating_mul(rows as usize)
        .saturating_mul(depth as usize);
    let remaining = usize::try_from(total.saturating_sub(offset)).unwrap_or(usize::MAX);
    padded.min(remaining)
}

/// Whether `[at, at + len)` is committed, readable memory (streamed textures
/// may be unmapped until their pages arrive).
pub(crate) fn committed(at: *const u8, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery,
    };
    let mut p = at as usize;
    let Some(end) = p.checked_add(len) else {
        return false;
    };
    while p < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
        let unreadable = info.Protect == 0
            || info.Protect & (PAGE_NOACCESS | PAGE_GUARD) != 0;
        if n == 0 || info.State != MEM_COMMIT || unreadable {
            return false;
        }
        let next = (info.BaseAddress as usize).saturating_add(info.RegionSize);
        if next <= p {
            return false;
        }
        p = next;
    }
    true
}

/// The staging state of a title texture's host texture, made on first use
/// (one for all title textures of its memory).
fn staging(resource: This, texture: &ID3D12Resource) -> Option<Arc<Staging>> {
    let copy = crate::resource::host_copy(resource)?;
    let mut held = copy.staging.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = held.as_ref() {
        return Some(s.clone());
    }
    let (memory, desc) = crate::resource::texture_memory(resource)?;
    let Some(layout) = crate::resource::texture_layout(&desc).map(|t| t.planes[0].layout.clone())
    else {
        once!(
            ?desc,
            "texture contents without a console layout not uploaded"
        );
        return None;
    };
    if layout.desc.swizzle.xor() {
        once!(?desc, "texture contents in an XOR mode not uploaded");
        return None;
    }
    let host = host()?;
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let count = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        u32::from(d.MipLevels)
    } else {
        u32::from(d.MipLevels) * u32::from(d.DepthOrArraySize)
    };
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
    let mut rows = vec![0u32; count as usize];
    let mut total = 0;
    // SAFETY: outputs for `count` subresources.
    unsafe {
        host.device.GetCopyableFootprints(
            &d,
            0,
            count,
            0,
            Some(footprints.as_mut_ptr()),
            Some(rows.as_mut_ptr()),
            None,
            Some(&mut total),
        );
    }
    let (buffer, data) = host.upload_buffer(total)?;
    crate::host::name(&buffer, || {
        format!(
            "staging of {resource:?} {}x{}x{} f{}",
            d.Width, d.Height, d.DepthOrArraySize, d.Format.0
        )
    });
    tracing::debug!(?desc, total, "texture contents uploaded from title memory");
    let layout_size = layout.size as usize;
    let mips = u32::from(d.MipLevels).max(1);
    // The tiled bytes each subresource reads (all of them where the layout
    // cannot say: changed with any write).
    let extents: Vec<(usize, usize)> = (0..count)
        .map(|i| {
            layout
                .tiled_extent(i % mips, i / mips)
                .ok()
                .map(|(from, to)| (from as usize, (to as usize).min(layout_size)))
                .filter(|&(from, to)| from < to)
                .unwrap_or((0, layout_size))
        })
        .collect();
    let s = Arc::new(Staging {
        buffer,
        size: total,
        data,
        footprints,
        rows,
        layout,
        mips,
        memory,
        tracked: crate::watch::track(memory as usize, layout_size, true),
        parts: parts(&extents),
        last: Mutex::default(),
        #[cfg(feature = "diagnostics")]
        reported: std::sync::atomic::AtomicBool::new(false),
    });
    *held = Some(s.clone());
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Subresources whose tiled bytes overlap (the mip tail's, a volume's
    /// slabs) form one part; a write touches the parts it overlaps only.
    #[test]
    fn parts_group_overlapping_subresources() {
        // Mip 0 at [64K, 128K), mips 1 and 2 in the tail block [0, 64K),
        // then a second slice's three mips 128K on.
        let extents = [
            (65536, 131072),
            (32768, 40000),
            (0, 30000),
            (196608, 262144),
            (163840, 171000),
            (131072, 160000),
        ];
        let p = parts(&extents);
        let subresources: Vec<Vec<u32>> = p.iter().map(|p| p.subresources.clone()).collect();
        assert_eq!(subresources, [vec![2], vec![1], vec![0], vec![5], vec![4], vec![3]]);
        // Overlapping tail mips are one part.
        let p = parts(&[(65536, 131072), (0, 40000), (32768, 65536)]);
        assert_eq!(p.len(), 2);
        assert_eq!((p[0].from, p[0].to, p[0].subresources.clone()), (0, 65536, vec![1, 2]));
        // A page written in mip 0 only, then one across the tail and mip 0.
        assert_eq!(touched(&p, &[(65536 + 4096, 65536 + 8192)]), [false, true]);
        assert_eq!(touched(&p, &[(60000, 70000)]), [true, true]);
        assert_eq!(touched(&p, &[(131072, 140000)]), [false, false]);
    }

    #[test]
    fn subresource_len_stays_inside_staging_total() {
        // A 4x4 RGBA8 texture has RowPitch=256, rows=4, last_row=16 -> total=784.
        // Padded size (1024) must be clamped to 784 so from_raw_parts_mut stays in bounds.
        assert_eq!(subresource_len(0, 256, 4, 1, 784), 784);
        assert_eq!(subresource_len(512, 256, 4, 1, 784), 272);
        assert_eq!(subresource_len(0, 256, 4, 1, 2048), 1024);
    }

    #[test]
    fn committed_rejects_overflow_and_noaccess_pages() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE, VirtualAlloc,
            VirtualFree,
        };
        assert!(!committed(usize::MAX as *const u8, 16));
        // SAFETY: reserving and committing one private page for the test, freed below.
        let noaccess =
            unsafe { VirtualAlloc(std::ptr::null(), 4096, MEM_RESERVE | MEM_COMMIT, PAGE_NOACCESS) };
        assert!(!noaccess.is_null());
        assert!(!committed(noaccess.cast::<u8>(), 4096));
        // SAFETY: freeing the allocation above.
        unsafe { VirtualFree(noaccess, 0, MEM_RELEASE) };

        // Negative control: a readable committed page still passes.
        // SAFETY: reserving and committing one private page for the test, freed below.
        let rw =
            unsafe { VirtualAlloc(std::ptr::null(), 4096, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
        assert!(!rw.is_null());
        assert!(committed(rw.cast::<u8>(), 4096));
        // SAFETY: freeing the allocation above.
        unsafe { VirtualFree(rw, 0, MEM_RELEASE) };
    }
}
