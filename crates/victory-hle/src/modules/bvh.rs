//! `XboxBvhBuilder.dll`: the console's ray-tracing acceleration structure
//! builder, a C++ class whose methods are the D3D12 DXR build API (`this`
//! in rcx). Its builds run as the title's own compute programs (the
//! structure builder VIctory keeps off), and the 14 programs that test rays
//! walk what it builds.
//!
//! For now this only watches: every method logs its arguments (the first
//! 300 calls of each, then every 1000th) and calls the title's own export,
//! whose address `apps/victory` hands over once the title's DLLs are mapped
//! ([`set_originals`]). One run then says which calls the title makes, how
//! often, with what geometry, and what Rehydrate is given: the ground for
//! building the structures on the RTX cores instead.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::{HleExport, HleModule};

const BUILD: &str = "?BuildRaytracingAccelerationStructure@XboxBvhBuilder@@QEAAXPEAUID3D12GraphicsCommandList4@@PEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC@@IPEBUD3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_DESC@@@Z";
const BATCHED2: &str = "?BuildRaytracingAccelerationStructureBatched2X@XboxBvhBuilder@@QEAAXPEAUID3D12GraphicsCommandList6@@IPEBUD3D12XBOX_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_BATCHED_DESC2@@@Z";
const COPY: &str = "?CopyRaytracingAccelerationStructure@XboxBvhBuilder@@QEAAXPEAUID3D12GraphicsCommandList4@@_K1W4D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE@@@Z";
const PREBUILD: &str = "?GetRaytracingAccelerationStructurePrebuildInfo@XboxBvhBuilder@@QEAAXPEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS@@PEBUD3D12XBOX_XDXR_STREAM_DESC@@PEAUD3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO@@@Z";
const REHYDRATE_BATCHED2: &str = "?RehydrateRaytracingAccelerationStructureBatched2X@XboxBvhBuilder@@QEAAJPEAUID3D12GraphicsCommandList8@@IPEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC@@PEB_K@Z";
const REHYDRATE_BATCHED: &str = "?RehydrateRaytracingAccelerationStructureBatchedX@XboxBvhBuilder@@QEAAJPEAUID3D12GraphicsCommandList8@@IPEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC@@@Z";
const REHYDRATE: &str = "?RehydrateRaytracingAccelerationStructureX@XboxBvhBuilder@@QEAAJPEAUID3D12Device11@@PEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC@@@Z";
const MORTON: &str = "?Set64BitMortonCodes@XboxBvhBuilder@@QEAAX_N@Z";

/// The methods watched, in the order of [`ORIGINALS`] and [`CALLS`].
pub const WATCHED: [&str; 8] = [BUILD, BATCHED2, COPY, PREBUILD, REHYDRATE_BATCHED2, REHYDRATE_BATCHED, REHYDRATE, MORTON];

/// The title's own export of each watched method (0 until set).
static ORIGINALS: [AtomicUsize; 8] = [const { AtomicUsize::new(0) }; 8];
static CALLS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];

/// Hands over the title's own exports, `find` naming each by its mangled
/// name. A method left without one logs and does nothing.
pub fn set_originals(find: impl Fn(&str) -> Option<usize>) {
    for (i, name) in WATCHED.iter().enumerate() {
        match find(name) {
            Some(address) => ORIGINALS[i].store(address, Ordering::Relaxed),
            None => tracing::warn!(name, "bvh: the title's builder has no such export"),
        }
    }
}

/// Counts a call of method `i`; whether to log it, and the title's export.
fn call(i: usize) -> (bool, u64, usize) {
    let n = CALLS[i].fetch_add(1, Ordering::Relaxed);
    let logs = std::env::var_os("VICTORY_BVH_WATCH").is_some_and(|v| v == "1");
    (logs && (n < 300 || n % 1000 == 0), n, ORIGINALS[i].load(Ordering::Relaxed))
}

/// `len` bytes at the title address `at`, if all of it is readable.
fn read(at: u64, len: usize) -> Option<Vec<u8>> {
    if at < 0x10000 {
        return None;
    }
    let mut bytes = vec![0u8; len];
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut done = 0;
        // SAFETY: the kernel checks the source; the destination is ours.
        let ok = unsafe { ReadProcessMemory(GetCurrentProcess(), at as *const _, bytes.as_mut_ptr().cast(), len, &mut done) };
        (ok != 0 && done == len).then_some(bytes)
    }
    #[cfg(not(windows))]
    None
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map_or(0, |w| u32::from_le_bytes(w.try_into().unwrap_or_default()))
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    b.get(at..at + 8).map_or(0, |w| u64::from_le_bytes(w.try_into().unwrap_or_default()))
}

fn dwords(b: &[u8]) -> String {
    b.chunks(4).map(|w| format!("{:08x}", u32_at(w, 0))).collect::<Vec<_>>().join(" ")
}

/// `D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS` (24 bytes) at
/// `at`, with its first geometries or instances, as text.
fn inputs(at: u64) -> String {
    let Some(b) = read(at, 24) else {
        return format!("inputs at {at:#x} unreadable");
    };
    let (kind, flags, count, layout, descs) = (u32_at(&b, 0), u32_at(&b, 4), u32_at(&b, 8), u32_at(&b, 12), u64_at(&b, 16));
    let mut text = format!(
        "{} flags {flags:#x} count {count} layout {} descs {descs:#x}",
        match kind {
            0 => "TLAS",
            1 => "BLAS",
            _ => "type?",
        },
        if layout == 0 { "array" } else { "pointers" }
    );
    if kind == 0 {
        // the instances (64 bytes each): the first two, raw
        for i in 0..count.min(2) as u64 {
            match read(descs + 64 * i, 64) {
                Some(d) => text += &format!("; instance {i}: {} blas {:#x}", dwords(&d[..56]), u64_at(&d, 56)),
                None => text += &format!("; instance {i} unreadable (GPU-written later?)"),
            }
        }
    } else {
        for i in 0..count.min(3) as u64 {
            let g = if layout == 0 { Some(descs + 56 * i) } else { read(descs + 8 * i, 8).map(|p| u64_at(&p, 0)) };
            let Some(d) = g.and_then(|g| read(g, 56)) else {
                text += &format!("; geometry {i} unreadable");
                continue;
            };
            let (gkind, gflags) = (u32_at(&d, 0), u32_at(&d, 4));
            if gkind == 0 {
                // the first vertices, raw and as two floats or four 16-bit units, to tell
                // what the console's vertex format numbers mean
                let stride = (u64_at(&d, 48) & 0xffff).clamp(4, 32);
                if let Some(v) = read(u64_at(&d, 40), 4 * stride as usize) {
                    for k in 0..4 {
                        let w = &v[k * stride as usize..][..stride as usize];
                        let f: Vec<String> = w.chunks(4).map(|c| format!("{:.3}", f32::from_le_bytes(c.try_into().unwrap_or_default()))).collect();
                        let h: Vec<String> = w.chunks(2).map(|c| format!("{:.3}", i16::from_le_bytes(c.try_into().unwrap_or_default()) as f32 / 32767.0)).collect();
                        let e: Vec<String> = w.chunks(2).map(|c| format!("{:.3}", half(u16::from_le_bytes(c.try_into().unwrap_or_default())))).collect();
                        text += &format!("; vertex {k}: {} f32 [{}] snorm16 [{}] f16 [{}]", dwords(w), f.join(" "), h.join(" "), e.join(" "));
                    }
                }
            }
            text += &if gkind == 0 {
                format!(
                    "; triangles {i} flags {gflags:#x} transform {:#x} index format {} vertex format {} indices {} vertices {} ib {:#x} vb {:#x} stride {}",
                    u64_at(&d, 8),
                    u32_at(&d, 16),
                    u32_at(&d, 20),
                    u32_at(&d, 24),
                    u32_at(&d, 28),
                    u64_at(&d, 32),
                    u64_at(&d, 40),
                    u64_at(&d, 48)
                )
            } else {
                format!(
                    "; geometry {i} kind {gkind} flags {gflags:#x} raw {}",
                    dwords(&d[8..])
                )
            };
        }
    }
    text
}

/// The bottom-level build desc at `at` (48 bytes) as the host builder takes it:
/// console structure address, build flags and triangle meshes. None for a top level.
fn blas(at: u64) -> Option<(u64, u32, Vec<victory_gpu::rt::Triangles>)> {
    let b = read(at, 48)?;
    let (dest, kind, flags, count, layout, descs) =
        (u64_at(&b, 0), u32_at(&b, 8), u32_at(&b, 12), u32_at(&b, 16), u32_at(&b, 20), u64_at(&b, 24));
    if kind != 1 || count == 0 || count > 64 {
        return None;
    }
    let mut meshes = Vec::new();
    for i in 0..u64::from(count) {
        let g = if layout == 0 { Some(descs + 56 * i) } else { read(descs + 8 * i, 8).map(|p| u64_at(&p, 0)) };
        let d = read(g?, 56)?;
        if u32_at(&d, 0) != 0 {
            continue;
        }
        meshes.push(victory_gpu::rt::Triangles {
            flags: u32_at(&d, 4),
            transform: u64_at(&d, 8),
            index_format: u32_at(&d, 16),
            vertex_format: u32_at(&d, 20),
            index_count: u32_at(&d, 24),
            vertex_count: u32_at(&d, 28),
            index_buffer: u64_at(&d, 32),
            vertex_buffer: u64_at(&d, 40),
            stride: u64_at(&d, 48),
        });
    }
    Some((dest, flags, meshes))
}

/// Host builds (`VICTORY_RT_BUILD=1`) of the bottom-level descs `count`
/// records of `stride` bytes apart from `descs`, into `list`'s host list.
fn host_builds(list: usize, descs: u64, count: u32, stride: u64) {
    if !victory_gpu::rt::on() {
        return;
    }
    for i in 0..u64::from(count.min(256)) {
        if let Some((dest, flags, meshes)) = blas(descs + stride * i) {
            victory_gpu::rt::build_blas(list, dest, flags, &meshes);
        }
    }
}

/// A 16-bit float's value.
fn half(h: u16) -> f32 {
    let (sign, exp, frac) = ((h >> 15) as u32, ((h >> 10) & 0x1f) as i32, (h & 0x3ff) as f32);
    let v = match exp {
        0 => frac / 1024.0 * 2f32.powi(-14),
        31 => f32::INFINITY,
        e => (1.0 + frac / 1024.0) * 2f32.powi(e - 15),
    };
    if sign == 1 { -v } else { v }
}

/// `D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC` (48 bytes) at `at`.
fn build_desc(at: u64) -> String {
    match read(at, 48) {
        Some(b) => format!(
            "dest {:#x} source {:#x} scratch {:#x} {}",
            u64_at(&b, 0),
            u64_at(&b, 32),
            u64_at(&b, 40),
            inputs(at + 8)
        ),
        None => format!("desc at {at:#x} unreadable"),
    }
}

extern "system" fn build(this: usize, list: usize, desc: u64, post_count: u32, post: usize) {
    let (log, n, original) = call(0);
    if log {
        tracing::info!(n, list = format_args!("{list:#x}"), post_count, post = format_args!("{post:#x}"), "bvh build: {}", build_desc(desc));
    }
    if original != 0 {
        // SAFETY: the title's own export, with the title's arguments.
        let f: extern "system" fn(usize, usize, u64, u32, usize) = unsafe { std::mem::transmute(original) };
        f(this, list, desc, post_count, post);
    }
    host_builds(list, desc, 1, 48);
}

extern "system" fn batched2(this: usize, list: usize, count: u32, descs: u64) {
    let (log, n, original) = call(1);
    if log {
        // layout unknown: the first 128 bytes, raw
        // records of 80 bytes: a build desc (48), then 32 bytes of the console's own
        let raw = read(descs, 80).map_or_else(|| "unreadable".to_owned(), |b| dwords(&b[48..]));
        tracing::info!(n, list = format_args!("{list:#x}"), count, descs = format_args!("{descs:#x}"), "bvh batched build: first {} extra {raw}", build_desc(descs));
    }
    if original != 0 {
        // SAFETY: as in `build`.
        let f: extern "system" fn(usize, usize, u32, u64) = unsafe { std::mem::transmute(original) };
        f(this, list, count, descs);
    }
    host_builds(list, descs, count, 80);
}

extern "system" fn copy(this: usize, list: usize, dest: u64, source: u64, mode: u32) {
    let (log, n, original) = call(2);
    if log {
        tracing::info!(n, list = format_args!("{list:#x}"), dest = format_args!("{dest:#x}"), source = format_args!("{source:#x}"), mode, "bvh copy");
    }
    if original != 0 {
        // SAFETY: as in `build`.
        let f: extern "system" fn(usize, usize, u64, u64, u32) = unsafe { std::mem::transmute(original) };
        f(this, list, dest, source, mode);
    }
}

extern "system" fn prebuild(this: usize, inputs_at: u64, stream: u64, info: u64) {
    let (log, n, original) = call(3);
    if original != 0 {
        // SAFETY: as in `build`.
        let f: extern "system" fn(usize, u64, u64, u64) = unsafe { std::mem::transmute(original) };
        f(this, inputs_at, stream, info);
    }
    if log {
        let sizes = read(info, 24).map_or_else(|| "unreadable".to_owned(), |b| format!("result {} scratch {} update {}", u64_at(&b, 0), u64_at(&b, 8), u64_at(&b, 16)));
        let stream = read(stream, 32).map_or_else(|| "none".to_owned(), |b| dwords(&b));
        tracing::info!(n, stream, "bvh prebuild info: {} -> {sizes}", inputs(inputs_at));
    }
}

extern "system" fn rehydrate_batched2(this: usize, list: usize, count: u32, descs: u64, extra: u64) -> i32 {
    let (log, n, original) = call(4);
    if log {
        tracing::info!(n, list = format_args!("{list:#x}"), count, extra = format_args!("{extra:#x}"), "bvh rehydrate batched2: first {}", build_desc(descs));
    }
    if original == 0 {
        return 0x8000_4001_u32 as i32;
    }
    // SAFETY: as in `build`.
    let f: extern "system" fn(usize, usize, u32, u64, u64) -> i32 = unsafe { std::mem::transmute(original) };
    let hr = f(this, list, count, descs, extra);
    // (`VICTORY_RT_REHYDRATE=0`: only the built ones)
    if std::env::var_os("VICTORY_RT_REHYDRATE").is_none_or(|v| v != "0") {
        host_builds(list, descs, count, 48);
    }
    hr
}

extern "system" fn rehydrate_batched(this: usize, list: usize, count: u32, descs: u64) -> i32 {
    let (log, n, original) = call(5);
    if log {
        tracing::info!(n, list = format_args!("{list:#x}"), count, "bvh rehydrate batched: first {}", build_desc(descs));
    }
    if original == 0 {
        return 0x8000_4001_u32 as i32;
    }
    // SAFETY: as in `build`.
    let f: extern "system" fn(usize, usize, u32, u64) -> i32 = unsafe { std::mem::transmute(original) };
    f(this, list, count, descs)
}

extern "system" fn rehydrate(this: usize, device: usize, desc: u64) -> i32 {
    let (log, n, original) = call(6);
    if log {
        tracing::info!(n, device = format_args!("{device:#x}"), "bvh rehydrate: {}", build_desc(desc));
    }
    if original == 0 {
        return 0x8000_4001_u32 as i32;
    }
    // SAFETY: as in `build`.
    let f: extern "system" fn(usize, usize, u64) -> i32 = unsafe { std::mem::transmute(original) };
    f(this, device, desc)
}

extern "system" fn morton(this: usize, on: u8) {
    let (log, n, original) = call(7);
    if log {
        tracing::info!(n, on, "bvh 64-bit Morton codes");
    }
    if original != 0 {
        // SAFETY: as in `build`.
        let f: extern "system" fn(usize, u8) = unsafe { std::mem::transmute(original) };
        f(this, on);
    }
}

/// Whether the builder is watched (`VICTORY_BVH_WATCH=1`; off by default,
/// so the title binds its own builder directly).
fn watched() -> bool {
    std::env::var_os("VICTORY_BVH_WATCH").is_some_and(|v| v == "1") || victory_gpu::rt::on()
}

pub(crate) fn module() -> HleModule {
    let exports: [(&'static str, usize); 8] = [
        (BUILD, build as *const () as usize),
        (BATCHED2, batched2 as *const () as usize),
        (COPY, copy as *const () as usize),
        (PREBUILD, prebuild as *const () as usize),
        (REHYDRATE_BATCHED2, rehydrate_batched2 as *const () as usize),
        (REHYDRATE_BATCHED, rehydrate_batched as *const () as usize),
        (REHYDRATE, rehydrate as *const () as usize),
        (MORTON, morton as *const () as usize),
    ];
    HleModule {
        name: "bvh",
        dlls: &["xboxbvhbuilder.dll"],
        exports: if watched() {
            exports.into_iter().map(|(name, address)| HleExport { name, address }).collect()
        } else {
            Vec::new()
        },
    }
}
