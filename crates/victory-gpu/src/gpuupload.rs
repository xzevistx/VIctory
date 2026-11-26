//! Texture uploads recorded on the card (`VICTORY_POOL_IN_PLACE=1`, with
//! `VICTORY_POOL_COMMIT=1`): for a texture whose memory lies in the
//! title's page pool, a refresh does not read or detile it on the CPU.
//! It queues work that the list's prelude records: the texture's 64 KiB
//! pages gathered on the card from the pool's own memory
//! (`crate::poolheap`), detiled by the card (`crate::gpudetile`), and
//! copied into the texture.
//!
//! The texture is in the copy destination state when the prelude ends:
//! in this mode `crate::texture::upload` asks for it to start the list
//! there, so the prelude's moves put it there and the work needs no move
//! of its own; the list's own moves take it on from there.
//!
//! A texture the card cannot read in place (a page of it is memory of
//! another kind) is detiled by the CPU into its staging buffer, as
//! before, and the work is then only the copy from there.
//!
//! The work is queued by the list it belongs to (`with_list`) and taken by
//! that list's prelude ([`record`]); its scratch buffers are kept until the
//! prelude is recorded again (it has run by then) and go back to a pool.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC};

const PAGE: u64 = 64 << 10;

/// One subresource's depth slice of a texture detiled on the card.
pub(crate) struct Part {
    /// The subresource's base in the gathered pages (the texture's offset
    /// in its first page included).
    pub(crate) base: u32,
    /// Byte offsets of its row parts and column parts in the work's tables.
    pub(crate) rows_at: u32,
    pub(crate) cols_at: u32,
    /// Where its first row goes in the detiled rows, and their pitch.
    pub(crate) linear: u32,
    pub(crate) pitch: u32,
    /// Elements across and down.
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// A level that is not the title's: zeros.
    pub(crate) zero: bool,
}

pub(crate) enum Work {
    /// Detiled on the card from the title's pages.
    Card {
        texture: ID3D12Resource,
        /// Each 64 KiB page from the texture's first: where the card reads
        /// it, or `None` for one not mapped (zeros, as the CPU reads it).
        pages: Vec<Option<(ID3D12Resource, u64)>>,
        bpe: u32,
        parts: Vec<Part>,
        tables: Vec<u32>,
        /// The detiled rows' layout: the staging buffer's footprints.
        footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
        linear_bytes: u64,
    },
    /// Detiled by the CPU into the staging buffer: copied from there.
    Cpu { texture: ID3D12Resource, buffer: ID3D12Resource, footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT> },
}

// SAFETY: D3D12 objects are free-threaded; the work is only read by the
// thread that records it.
unsafe impl Send for Work {}

thread_local! {
    /// The list whose submission refreshes textures on this thread now.
    static LIST: Cell<usize> = const { Cell::new(0) };
}

/// Runs `f` (a texture refresh) for the list `key`: work it queues is that
/// list's.
pub(crate) fn with_list<R>(key: usize, f: impl FnOnce() -> R) -> R {
    let before = LIST.with(|l| l.replace(key));
    let r = f();
    LIST.with(|l| l.set(before));
    r
}

/// Whether a refresh on this thread can queue work (it runs for a list).
pub(crate) fn can_queue() -> bool {
    LIST.with(Cell::get) != 0
}

/// Work queued by lists whose preludes have not taken it yet.
static PENDING: Mutex<Vec<(usize, Work)>> = Mutex::new(Vec::new());

pub(crate) fn queue(work: Work) {
    let key = LIST.with(Cell::get);
    if key == 0 {
        return;
    }
    PENDING.lock().unwrap_or_else(|e| e.into_inner()).push((key, work));
}

/// Since the start: textures detiled on the card, and copied from a
/// staging buffer the CPU detiled.
pub(crate) static QUEUED: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Whether list `key` has work for its prelude.
pub(crate) fn pending(key: usize) -> bool {
    PENDING.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|(k, _)| *k == key)
}

/// Forgets work list `key` queued and no prelude took (a submission that
/// did not record one).
pub(crate) fn forget(key: usize) {
    PENDING.lock().unwrap_or_else(|e| e.into_inner()).retain(|(k, _)| *k != key);
}

/// The detiler, made at the first ask; `None` if the shader could not be
/// had (no texture then goes this way).
pub(crate) fn detiler() -> Option<&'static crate::gpudetile::Detiler> {
    static DETILER: OnceLock<Option<crate::gpudetile::Detiler>> = OnceLock::new();
    DETILER
        .get_or_init(|| {
            let host = crate::host::host()?;
            match crate::gpudetile::Detiler::new(&host.device) {
                Ok(d) => {
                    tracing::info!("textures in the title's page pool are detiled on the card (VICTORY_POOL_IN_PLACE=1)");
                    Some(d)
                }
                Err(error) => {
                    tracing::warn!(%error, "no detiling on the card: textures are detiled by the CPU");
                    None
                }
            }
        })
        .as_ref()
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    /// Gathered pages: a default buffer.
    Tiled,
    /// Detiled rows: a default buffer shaders write.
    Rows,
    /// Tables: an upload buffer, mapped.
    Tables,
}

/// A scratch buffer, back to the pool when dropped.
struct Pooled {
    kind: Kind,
    size: u64,
    buffer: ID3D12Resource,
    mapped: usize,
}

static POOL: Mutex<Option<HashMap<(Kind, u64), Vec<(ID3D12Resource, usize)>>>> = Mutex::new(None);
/// Bytes in the pool; at most this many are kept.
static POOLED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const POOL_MOST: u64 = 768 << 20;

impl Drop for Pooled {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        if POOLED.load(Relaxed) + self.size > POOL_MOST {
            return;
        }
        POOLED.fetch_add(self.size, Relaxed);
        POOL.lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(HashMap::new)
            .entry((self.kind, self.size))
            .or_default()
            .push((self.buffer.clone(), self.mapped));
    }
}

fn take(device: &ID3D12Device, kind: Kind, bytes: u64) -> Option<Pooled> {
    use std::sync::atomic::Ordering::Relaxed;
    let size = bytes.max(PAGE).next_power_of_two();
    let spare = POOL.lock().unwrap_or_else(|e| e.into_inner()).as_mut().and_then(|p| p.get_mut(&(kind, size)).and_then(Vec::pop));
    if let Some((buffer, mapped)) = spare {
        POOLED.fetch_sub(size, Relaxed);
        return Some(Pooled { kind, size, buffer, mapped });
    }
    let (heap, state, flags) = match kind {
        Kind::Tiled => (D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_FLAG_NONE),
        Kind::Rows => (D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS),
        Kind::Tables => (D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ, D3D12_RESOURCE_FLAG_NONE),
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_UNKNOWN,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
        ..Default::default()
    };
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: plain creation.
    unsafe {
        device
            .CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: heap, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut made)
            .ok()?;
    }
    let buffer = made?;
    let mut mapped = std::ptr::null_mut();
    if kind == Kind::Tables {
        // SAFETY: an upload buffer, mapped for its life.
        unsafe { buffer.Map(0, None, Some(&mut mapped)).ok()? };
    }
    Some(Pooled { kind, size, buffer, mapped: mapped as usize })
}

/// 64 KiB of zeros on the card: the source of a page that is not mapped.
fn zeros(device: &ID3D12Device) -> Option<&'static ID3D12Resource> {
    static ZEROS: OnceLock<Option<ID3D12Resource>> = OnceLock::new();
    ZEROS
        .get_or_init(|| {
            // A committed resource's memory starts zeroed.
            take(device, Kind::Tiled, PAGE).map(|p| {
                let buffer = p.buffer.clone();
                std::mem::forget(p);
                buffer
            })
        })
        .as_ref()
}

/// What each list's last prelude used, kept until it is recorded again.
static KEPT: Mutex<Option<HashMap<usize, Vec<Pooled>>>> = Mutex::new(None);

/// Records list `key`'s queued work at the end of its prelude `list`
/// (open). Returns how many textures it copied.
pub(crate) fn record(key: usize, list: &ID3D12GraphicsCommandList) -> usize {
    use std::sync::atomic::Ordering::Relaxed;
    let works: Vec<Work> = {
        let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        let (mine, others): (Vec<_>, Vec<_>) = pending.drain(..).partition(|(k, _)| *k == key);
        *pending = others;
        mine.into_iter().map(|(_, w)| w).collect()
    };
    // The prelude's last run is over: what it used goes back to the pool.
    let mut kept = Vec::new();
    let Some(host) = crate::host::host() else {
        return 0;
    };
    let device = &host.device;
    let copy = |texture: &ID3D12Resource, from: &ID3D12Resource, footprints: &[D3D12_PLACED_SUBRESOURCE_FOOTPRINT]| {
        for (i, footprint) in footprints.iter().enumerate() {
            let to = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: a borrowed pointer the location never releases.
                pResource: unsafe { std::mem::transmute_copy(texture) },
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: i as u32 },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: as above.
                pResource: unsafe { std::mem::transmute_copy(from) },
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: *footprint },
            };
            // SAFETY: an open list; both locations name live resources.
            unsafe { list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
        }
    };
    let mut done = 0;
    for work in works {
        match work {
            Work::Cpu { texture, buffer, footprints } => {
                copy(&texture, &buffer, &footprints);
                QUEUED[1].fetch_add(1, Relaxed);
                done += 1;
            }
            Work::Card { texture, pages, bpe, parts, tables, footprints, linear_bytes } => {
                let (Some(detiler), Some(zeros)) = (detiler(), zeros(device)) else {
                    continue;
                };
                let tiled = take(device, Kind::Tiled, pages.len() as u64 * PAGE);
                let rows = take(device, Kind::Rows, linear_bytes);
                let table = take(device, Kind::Tables, tables.len() as u64 * 4);
                let (Some(tiled), Some(rows), Some(table)) = (tiled, rows, table) else {
                    once_warn("no scratch buffers for a texture detiled on the card: it keeps its old contents");
                    continue;
                };
                // SAFETY: the mapped upload buffer holds at least the tables.
                unsafe { std::ptr::copy_nonoverlapping(tables.as_ptr(), table.mapped as *mut u32, tables.len()) };
                // SAFETY: an open list; every buffer is live and kept until
                // the prelude has run (`KEPT`).
                unsafe {
                    for (k, page) in pages.iter().enumerate() {
                        let (source, offset) = match page {
                            Some((buffer, offset)) => (buffer, *offset),
                            None => (zeros, 0),
                        };
                        list.CopyBufferRegion(&tiled.buffer, k as u64 * PAGE, source, offset, PAGE);
                    }
                    list.ResourceBarrier(&[crate::host::transition(
                        &tiled.buffer,
                        D3D12_RESOURCE_STATE_COPY_DEST,
                        D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                    )]);
                    let (tiled_gpu, table_gpu, rows_gpu) =
                        (tiled.buffer.GetGPUVirtualAddress(), table.buffer.GetGPUVirtualAddress(), rows.buffer.GetGPUVirtualAddress());
                    for part in &parts {
                        detiler.record(
                            list,
                            &crate::gpudetile::Job {
                                width: part.width,
                                height: part.height,
                                bpe,
                                tiled: tiled_gpu,
                                tiled_base: part.base,
                                tables: table_gpu + u64::from(part.rows_at),
                                cols_at: part.cols_at - part.rows_at,
                                linear: rows_gpu,
                                linear_base: part.linear,
                                row_pitch: part.pitch,
                                zero: part.zero,
                            },
                        );
                    }
                    list.ResourceBarrier(&[crate::host::transition(
                        &rows.buffer,
                        D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                        D3D12_RESOURCE_STATE_COPY_SOURCE,
                    )]);
                }
                copy(&texture, &rows.buffer, &footprints);
                kept.extend([tiled, rows, table]);
                QUEUED[0].fetch_add(1, Relaxed);
                done += 1;
            }
        }
    }
    KEPT.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(key, kept);
    done
}

fn once_warn(what: &'static str) {
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!("{what}");
    }
}
