//! `XboxBvhBuilder.dll`: the title's ray-tracing structure builder, a DLL of
//! its package that records the console GPU's own builder programs into the
//! title's command lists (`docs/fork-ray-tracing.md`).
//!
//! The first step of putting the host's own ray-tracing structures (DXR) in
//! its place (`docs/fork-dxr.md`): `VICTORY_BVH_LOG=1` wraps the four calls
//! the title makes into it, logs what the title asks for, and then calls the
//! DLL as before. Nothing else changes.
//!
//! What the title passes are PC D3D12's own descriptions. The DLL's
//! `BuildRaytracingAccelerationStructure` (which the title does not import)
//! copies one `D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC` (48 bytes)
//! into one batched description (80 bytes) and calls
//! `BuildRaytracingAccelerationStructureBatched2X` with it: the build
//! description, the post-build count at 0x30 and the post-build descriptions
//! at 0x38, and 16 bytes at 0x40 it leaves zero. The batched call steps
//! through them 80 bytes at a time and through geometry descriptions 56 bytes
//! at a time, as PC D3D12's `D3D12_RAYTRACING_GEOMETRY_DESC`.

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use victory_loader::{ImportResolver, Provenance, Resolution};
use victory_pe::ImportSymbol;

/// `D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Inputs {
    /// 0 top level, 1 bottom level.
    pub kind: u32,
    /// `D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAGS`: 1 allow update,
    /// 2 allow compaction, 4 fast trace, 8 fast build, 0x10 small, 0x20 an update.
    pub flags: u32,
    /// Instances (top level) or geometries (bottom level).
    pub count: u32,
    /// 0 an array, 1 an array of pointers (bottom level).
    pub layout: u32,
    /// Top level: the GPU address of the instance descriptions. Bottom level:
    /// a CPU pointer to the geometry descriptions (or to pointers to them).
    pub descs: u64,
}

/// `D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BuildDesc {
    pub dest: u64,
    pub inputs: Inputs,
    pub source: u64,
    pub scratch: u64,
}

/// `D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_DESC`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PostbuildDesc {
    pub dest: u64,
    /// 0 compacted size, 1 tools visualization, 2 serialization, 3 current
    /// size; the DLL skips 0x10.
    pub kind: u32,
    _pad: u32,
}

/// The console's batched build description, as the DLL reads it.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Batched {
    pub desc: BuildDesc,
    pub postbuild_count: u32,
    _pad: u32,
    pub postbuild: *const PostbuildDesc,
    /// Not known; the DLL's own single build leaves it zero.
    pub tail: [u64; 2],
}

/// `D3D12_RAYTRACING_GEOMETRY_DESC`: triangles (`kind` 0) or boxes (1).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub kind: u32,
    /// 1 opaque, 2 any-hit called once.
    pub flags: u32,
    /// Triangles: the transform's GPU address. Boxes: their count.
    pub transform: u64,
    pub index_format: u32,
    pub vertex_format: u32,
    pub index_count: u32,
    pub vertex_count: u32,
    pub index_buffer: u64,
    pub vertex_buffer: u64,
    pub vertex_stride: u64,
}

/// `D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PrebuildInfo {
    pub result: u64,
    pub scratch: u64,
    pub update_scratch: u64,
}

const _: () = assert!(size_of::<Inputs>() == 24);
const _: () = assert!(size_of::<BuildDesc>() == 48);
const _: () = assert!(size_of::<Batched>() == 80);
const _: () = assert!(size_of::<Geometry>() == 56);

type BuildBatchedFn = unsafe extern "system" fn(*mut c_void, *mut c_void, u32, *const Batched);
type RehydrateFn = unsafe extern "system" fn(*mut c_void, *mut c_void, u32, *const BuildDesc, *const u64) -> i32;
type PrebuildFn = unsafe extern "system" fn(*mut c_void, *const Inputs, *const c_void, *mut PrebuildInfo);
type CopyFn = unsafe extern "system" fn(*mut c_void, *mut c_void, u64, u64, u32);

const BUILD: usize = 0;
const REHYDRATE: usize = 1;
const PREBUILD: usize = 2;
const COPY: usize = 3;

/// The DLL's own entry points, by the methods' names.
const METHODS: [&str; 4] = [
    "?BuildRaytracingAccelerationStructureBatched2X@XboxBvhBuilder@@",
    "?RehydrateRaytracingAccelerationStructureBatched2X@XboxBvhBuilder@@",
    "?GetRaytracingAccelerationStructurePrebuildInfo@XboxBvhBuilder@@",
    "?CopyRaytracingAccelerationStructure@XboxBvhBuilder@@",
];

static REAL: [AtomicUsize; 4] = [const { AtomicUsize::new(0) }; 4];

/// `VICTORY_BVH_LOG=1`: the title's calls into its builder are logged.
fn logging() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BVH_LOG").is_some_and(|v| v == "1"))
}

/// The address the title's import `dll!symbol` is bound to: one of the
/// wrappers below in place of the DLL's `real` one, when asked for.
pub fn wrap(dll: &str, symbol: &str, real: usize) -> usize {
    if !(logging() || victory_gpu::dxr::on()) || !dll.eq_ignore_ascii_case("xboxbvhbuilder.dll") {
        return real;
    }
    let Some(which) = METHODS.iter().position(|m| symbol.starts_with(m)) else {
        return real;
    };
    REAL[which].store(real, Ordering::Release);
    tracing::info!(method = METHODS[which], "bvh log: the title's builder call is wrapped");
    match which {
        BUILD => build_batched as BuildBatchedFn as usize,
        REHYDRATE => rehydrate_batched as RehydrateFn as usize,
        PREBUILD => prebuild_info as PrebuildFn as usize,
        _ => copy as CopyFn as usize,
    }
}

/// Resolves through `.0`, binding the title's imports of its builder to the
/// wrappers where asked ([`wrap`]).
pub struct Wraps<'a>(pub &'a dyn ImportResolver);

impl ImportResolver for Wraps<'_> {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        match self.0.resolve(dll, symbol) {
            Resolution::Found {
                address,
                provenance: Provenance::Title,
            } => Resolution::Found {
                address: wrap(dll, &symbol.to_string(), address),
                provenance: Provenance::Title,
            },
            other => other,
        }
    }
}

/// What the title asked of its builder since the start.
#[derive(Default)]
struct Stats {
    calls: u64,
    top: u64,
    bottom: u64,
    updates: u64,
    instances: u64,
    geometries: u64,
    boxes: u64,
    triangles: u64,
    opaque: u64,
    transformed: u64,
    /// (vertex format, index format, stride): geometries.
    formats: std::collections::BTreeMap<(u32, u32, u64), u64>,
    /// By the build flags.
    flags: std::collections::BTreeMap<u32, u64>,
    postbuild: [u64; 5],
    tails: u64,
    rehydrations: u64,
    rehydrated: u64,
    prebuilds: u64,
    copies: [u64; 5],
    /// Calls logged one by one so far, by kind ([`BUILD`] and so on).
    said: [u32; 4],
    /// Geometries of index format 53 unpacked ([`peek`]) that agree with
    /// their descriptions, and that do not.
    unpacked_good: u64,
    unpacked_bad: u64,
    last: Option<Instant>,
}

static STATS: Mutex<Stats> = Mutex::new(Stats {
    calls: 0,
    top: 0,
    bottom: 0,
    updates: 0,
    instances: 0,
    geometries: 0,
    boxes: 0,
    triangles: 0,
    opaque: 0,
    transformed: 0,
    formats: std::collections::BTreeMap::new(),
    flags: std::collections::BTreeMap::new(),
    postbuild: [0; 5],
    tails: 0,
    rehydrations: 0,
    rehydrated: 0,
    prebuilds: 0,
    copies: [0; 5],
    said: [0; 4],
    unpacked_good: 0,
    unpacked_bad: 0,
    last: None,
});

/// Calls of each kind logged one by one, then only the summary every ten seconds.
const DETAILED: u32 = 30;

/// Geometries of index format 53 ([`packed`]) logged one by one when unpacked.
const PEEKS: u32 = 40;

/// Geometries of index format 53 unpacked in all, to check [`packed`].
const UNPACKS: u64 = 4000;

fn stats() -> std::sync::MutexGuard<'static, Stats> {
    STATS.lock().unwrap_or_else(|e| e.into_inner())
}

fn format_name(f: u32) -> String {
    match f {
        0 => "none".into(),
        2 => "rgba32f".into(),
        6 => "rgb32f".into(),
        10 => "rgba16f".into(),
        13 => "rgba16snorm".into(),
        16 => "rg32f".into(),
        34 => "rg16f".into(),
        37 => "rg16snorm".into(),
        42 => "r32u".into(),
        57 => "r16u".into(),
        other => format!("dxgi{other}"),
    }
}

impl Stats {
    /// The summary line, every ten seconds.
    fn summary(&mut self, now: Instant) {
        let due = self.last.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(10));
        if !due {
            return;
        }
        self.last = Some(now);
        let formats: Vec<String> = self
            .formats
            .iter()
            .map(|(&(v, i, s), n)| format!("{}/{}/{s}:{n}", format_name(v), format_name(i)))
            .collect();
        let flags: Vec<String> = self.flags.iter().map(|(f, n)| format!("{f:#x}:{n}")).collect();
        tracing::info!(
            calls = self.calls,
            top = self.top,
            bottom = self.bottom,
            updates = self.updates,
            instances = self.instances,
            geometries = self.geometries,
            boxes = self.boxes,
            triangles = self.triangles,
            opaque = self.opaque,
            transformed = self.transformed,
            formats = %formats.join(" "),
            flags = %flags.join(" "),
            postbuild = ?self.postbuild,
            tails = self.tails,
            rehydrations = self.rehydrations,
            rehydrated = self.rehydrated,
            prebuilds = self.prebuilds,
            copies = ?self.copies,
            unpacked_good = self.unpacked_good,
            unpacked_bad = self.unpacked_bad,
            "bvh log: what the title asked of its builder so far (formats vertex/index/stride:geometries, copies by mode)"
        );
    }
}

/// The geometry descriptions of a bottom level, or none.
///
/// # Safety
/// `inputs` as the title passes it to the builder.
unsafe fn geometries(inputs: &Inputs) -> Vec<Geometry> {
    if inputs.kind != 1 || inputs.descs == 0 {
        return Vec::new();
    }
    let n = inputs.count.min(1 << 16) as usize;
    (0..n)
        .filter_map(|i| unsafe {
            let p = if inputs.layout == 1 {
                *(inputs.descs as *const *const Geometry).add(i)
            } else {
                (inputs.descs as *const Geometry).add(i)
            };
            (!p.is_null()).then(|| p.read_unaligned())
        })
        .collect()
}

fn count(s: &mut Stats, desc: &BuildDesc, list: &[Geometry]) {
    let inputs = desc.inputs;
    *s.flags.entry(inputs.flags).or_default() += 1;
    s.updates += u64::from(inputs.flags & 0x20 != 0);
    if inputs.kind == 0 {
        s.top += 1;
        s.instances += u64::from(inputs.count);
        return;
    }
    s.bottom += 1;
    for g in list {
        s.geometries += 1;
        s.opaque += u64::from(g.flags & 1 != 0);
        if g.kind == 1 {
            s.boxes += g.transform;
            continue;
        }
        s.transformed += u64::from(g.transform != 0);
        let n = if g.index_format == 0 { g.vertex_count } else { g.index_count };
        s.triangles += u64::from(n / 3);
        *s.formats.entry((g.vertex_format, g.index_format, g.vertex_stride)).or_default() += 1;
    }
}

fn describe(desc: &BuildDesc, list: &[Geometry]) -> String {
    let i = desc.inputs;
    let mut out = format!(
        "{} flags={:#x} count={} layout={} dest={:#x} source={:#x} scratch={:#x} descs={:#x}",
        if i.kind == 0 { "top" } else { "bottom" },
        i.flags,
        i.count,
        i.layout,
        desc.dest,
        desc.source,
        desc.scratch,
        i.descs
    );
    for g in list.iter().take(4) {
        out += &if g.kind == 1 {
            format!(" | boxes {} at {:#x} stride {} flags={}", g.transform, g.index_buffer, g.index_count, g.flags)
        } else {
            format!(
                " | tri flags={} vtx {} {} at {:#x} stride {} idx {} {} at {:#x} xform {:#x}",
                g.flags,
                format_name(g.vertex_format),
                g.vertex_count,
                g.vertex_buffer,
                g.vertex_stride,
                format_name(g.index_format),
                g.index_count,
                g.index_buffer,
                g.transform
            )
        };
    }
    if list.len() > 4 {
        out += &format!(" | and {} more", list.len() - 4);
    }
    out
}

/// Whether `len` bytes at `address` are committed memory this process can read.
#[cfg(windows)]
fn readable(address: u64, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery,
    };
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: only asks about the address.
    let got = unsafe { VirtualQuery(address as *const c_void, &mut info, size_of::<MEMORY_BASIC_INFORMATION>()) };
    got != 0
        && info.State == MEM_COMMIT
        && info.Protect & (PAGE_NOACCESS | PAGE_GUARD) == 0
        && address + len as u64 <= info.BaseAddress as u64 + info.RegionSize as u64
}

#[cfg(not(windows))]
fn readable(_: u64, _: usize) -> bool {
    false
}

/// Index format 53, the builder's packed triangles: unpacked as the host's
/// structures read them (`victory_gpu::dxr::packed`).
pub use victory_gpu::dxr::packed;

/// Unpacks a geometry of index format 53 as [`packed`] reads it, and logs
/// whether its triangles agree with its description: their number with the
/// index count, every vertex number below the vertex count.
fn peek(s: &mut Stats, g: &Geometry) {
    let fail = |s: &mut Stats, why: &str| {
        s.unpacked_bad += 1;
        if s.unpacked_bad <= 20 {
            tracing::info!(why, at = %format!("{:#x}", g.index_buffer), "bvh log: format 53 not unpacked");
        }
    };
    if g.index_buffer == 0 || !readable(g.index_buffer, 12) {
        return fail(s, "not readable from here");
    }
    // SAFETY: committed and readable, checked above.
    let head = unsafe { std::slice::from_raw_parts(g.index_buffer as *const u8, 12) };
    let Some(len) = packed::len(head).filter(|&n| n <= 64 << 20 && readable(g.index_buffer, n)) else {
        return fail(s, "its length is not readable from here");
    };
    // SAFETY: committed and readable, checked above.
    let bytes = unsafe { std::slice::from_raw_parts(g.index_buffer as *const u8, len) };
    let list = match packed::triangles(bytes, g.vertex_stride & 0x8000 != 0) {
        Ok(list) => list,
        Err(why) => return fail(s, why),
    };
    let largest = list.iter().flatten().copied().max().unwrap_or(0);
    let mut used = vec![false; g.vertex_count as usize];
    list.iter().flatten().for_each(|&v| {
        if let Some(u) = used.get_mut(v as usize) {
            *u = true;
        }
    });
    let used = used.iter().filter(|&&u| u).count();
    let agrees = list.len() as u64 * 3 == u64::from(g.index_count) && largest < g.vertex_count;
    if agrees {
        s.unpacked_good += 1;
    } else {
        s.unpacked_bad += 1;
    }
    if s.unpacked_good + s.unpacked_bad <= PEEKS as u64 {
        tracing::info!(
            agrees,
            triangles = list.len(),
            indices = g.index_count,
            largest,
            vertices = g.vertex_count,
            used,
            flip = g.vertex_stride & 0x8000 != 0,
            first = ?&list[..list.len().min(3)],
            "bvh log: format 53 unpacked"
        );
    }
}

/// The bottom levels of a batched build, for the host's own structures
/// (`victory_gpu::dxr`, `VICTORY_DXR=1`), written down in the title's list.
///
/// # Safety
/// `descs` as the title passes it to the builder.
unsafe fn record_for_host(list: *mut c_void, n: u32, descs: *const Batched) {
    let mut builds = Vec::new();
    for k in 0..n.min(1 << 16) as usize {
        // SAFETY: as the title passes it to the builder.
        let b = unsafe { descs.add(k).read_unaligned() };
        if b.desc.inputs.kind != 1 {
            continue;
        }
        let geometries = unsafe { geometries(&b.desc.inputs) };
        if geometries.iter().any(|g| g.kind != 0) {
            continue;
        }
        builds.push(victory_gpu::dxr::Build {
            dest: b.desc.dest,
            source: b.desc.source,
            flags: b.desc.inputs.flags,
            geometries: geometries
                .iter()
                .map(|g| victory_gpu::dxr::Geometry {
                    flags: g.flags,
                    transform: g.transform,
                    index_format: g.index_format,
                    vertex_format: g.vertex_format,
                    index_count: g.index_count,
                    vertex_count: g.vertex_count,
                    index_buffer: g.index_buffer,
                    vertex_buffer: g.vertex_buffer,
                    vertex_stride: g.vertex_stride,
                })
                .collect(),
        });
    }
    victory_gpu::dxr::record(list as usize, builds);
}

unsafe extern "system" fn build_batched(this: *mut c_void, list: *mut c_void, n: u32, descs: *const Batched) {
    if !descs.is_null() && victory_gpu::dxr::on() {
        // SAFETY: as the title passes them to the builder.
        unsafe { record_for_host(list, n, descs) };
    }
    if !descs.is_null() && logging() {
        let mut s = stats();
        s.calls += 1;
        for k in 0..n.min(1 << 16) as usize {
            // SAFETY: as the title passes it to the builder.
            let b = unsafe { descs.add(k).read_unaligned() };
            let list_of = unsafe { geometries(&b.desc.inputs) };
            count(&mut s, &b.desc, &list_of);
            s.tails += u64::from(b.tail != [0, 0]);
            let mut kinds = Vec::new();
            if !b.postbuild.is_null() {
                for j in 0..b.postbuild_count.min(64) as usize {
                    let kind = unsafe { b.postbuild.add(j).read_unaligned() }.kind;
                    s.postbuild[(kind as usize).min(4)] += 1;
                    kinds.push(kind);
                }
            }
            if s.said[BUILD] < DETAILED && k < 8 {
                s.said[BUILD] += 1;
                tracing::info!(
                    batch = n,
                    at = k,
                    postbuild = ?kinds,
                    tail = ?b.tail,
                    "bvh log: build {}",
                    describe(&b.desc, &list_of)
                );
                if let Some(g) = list_of.first() {
                    // SAFETY: a geometry description of 56 bytes.
                    let words: [u32; 14] = unsafe { std::mem::transmute(*g) };
                    tracing::info!(?words, "bvh log: its first geometry description as dwords");
                }
            }
            for g in list_of.iter().filter(|g| g.kind == 0 && g.index_format == 53) {
                if s.unpacked_good + s.unpacked_bad >= UNPACKS {
                    break;
                }
                peek(&mut s, g);
            }
        }
        s.summary(Instant::now());
    }
    let real = REAL[BUILD].load(Ordering::Acquire);
    // SAFETY: the DLL's own entry point, with the title's arguments.
    unsafe { std::mem::transmute::<usize, BuildBatchedFn>(real)(this, list, n, descs) }
}

unsafe extern "system" fn rehydrate_batched(
    this: *mut c_void,
    list: *mut c_void,
    n: u32,
    descs: *const BuildDesc,
    extra: *const u64,
) -> i32 {
    if !descs.is_null() && logging() {
        let mut s = stats();
        s.rehydrations += 1;
        s.rehydrated += u64::from(n);
        if s.said[REHYDRATE] < DETAILED {
            s.said[REHYDRATE] += 1;
            // SAFETY: as the title passes it to the builder.
            let first = unsafe { descs.read_unaligned() };
            let word = if extra.is_null() { None } else { Some(unsafe { extra.read_unaligned() }) };
            tracing::info!(
                batch = n,
                extra = ?word.map(|w| format!("{w:#x}")),
                "bvh log: rehydration, first {}",
                describe(&first, &[])
            );
        }
        s.summary(Instant::now());
    }
    let real = REAL[REHYDRATE].load(Ordering::Acquire);
    // SAFETY: the DLL's own entry point, with the title's arguments.
    unsafe { std::mem::transmute::<usize, RehydrateFn>(real)(this, list, n, descs, extra) }
}

unsafe extern "system" fn prebuild_info(
    this: *mut c_void,
    inputs: *const Inputs,
    stream: *const c_void,
    info: *mut PrebuildInfo,
) {
    let real = REAL[PREBUILD].load(Ordering::Acquire);
    // SAFETY: the DLL's own entry point, with the title's arguments.
    unsafe { std::mem::transmute::<usize, PrebuildFn>(real)(this, inputs, stream, info) };
    if inputs.is_null() || info.is_null() || !logging() {
        return;
    }
    let mut s = stats();
    s.prebuilds += 1;
    if s.said[PREBUILD] < DETAILED {
        s.said[PREBUILD] += 1;
        // SAFETY: as the title passed them, filled by the DLL.
        let (i, out) = unsafe { (inputs.read_unaligned(), info.read_unaligned()) };
        let desc = BuildDesc { dest: 0, inputs: i, source: 0, scratch: 0 };
        let list = unsafe { geometries(&i) };
        tracing::info!(
            stream = !stream.is_null(),
            result = out.result,
            scratch = out.scratch,
            update_scratch = out.update_scratch,
            "bvh log: sizes asked for {}",
            describe(&desc, &list)
        );
    }
}

unsafe extern "system" fn copy(this: *mut c_void, list: *mut c_void, dest: u64, source: u64, mode: u32) {
    if logging() {
        let mut s = stats();
        s.copies[(mode as usize).min(4)] += 1;
        if s.said[COPY] < DETAILED {
            s.said[COPY] += 1;
            tracing::info!(mode, dest = %format!("{dest:#x}"), source = %format!("{source:#x}"), "bvh log: copy");
        }
    }
    let real = REAL[COPY].load(Ordering::Acquire);
    // SAFETY: the DLL's own entry point, with the title's arguments.
    unsafe { std::mem::transmute::<usize, CopyFn>(real)(this, list, dest, source, mode) }
}

#[cfg(test)]
mod tests {
    use super::*;

    static CALLED: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "system" fn fake_build(_: *mut c_void, _: *mut c_void, n: u32, _: *const Batched) {
        CALLED.fetch_add(n as usize, Ordering::Relaxed);
    }

    #[test]
    fn a_bottom_level_is_counted_and_the_dll_still_called() {
        let geometry = [Geometry {
            kind: 0,
            flags: 1,
            transform: 0,
            index_format: 57,
            vertex_format: 6,
            index_count: 300,
            vertex_count: 120,
            index_buffer: 0x1000,
            vertex_buffer: 0x2000,
            vertex_stride: 12,
        }; 2];
        let batched = Batched {
            desc: BuildDesc {
                dest: 0x9000,
                inputs: Inputs { kind: 1, flags: 2, count: 2, layout: 0, descs: geometry.as_ptr() as u64 },
                source: 0,
                scratch: 0x8000,
            },
            postbuild_count: 0,
            _pad: 0,
            postbuild: std::ptr::null(),
            tail: [0; 2],
        };
        let list = unsafe { geometries(&batched.desc.inputs) };
        assert_eq!(list.len(), 2);
        let mut s = Stats::default();
        count(&mut s, &batched.desc, &list);
        assert_eq!((s.bottom, s.geometries, s.triangles, s.opaque), (1, 2, 200, 2));
        assert_eq!(s.formats.get(&(6, 57, 12)), Some(&2));
        REAL[BUILD].store(fake_build as BuildBatchedFn as usize, Ordering::Release);
        unsafe { build_batched(std::ptr::null_mut(), std::ptr::null_mut(), 1, &batched) };
        assert_eq!(CALLED.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn only_the_builder_dll_is_wrapped() {
        assert_eq!(wrap("kernel32.dll", METHODS[0], 7), 7);
    }
}
