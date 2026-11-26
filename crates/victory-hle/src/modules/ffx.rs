//! `amd_fidelityfx_loader_xbox.dll`: AMD's FidelityFX API (`ffxCreateContext`,
//! `ffxConfigure`, `ffxQuery`, `ffxDispatch`, `ffxDestroyContext`), through
//! which the title runs its temporal anti-aliasing and upscaling. Each call
//! takes a chain of descriptions (`u64 type`, next pointer, then the
//! type's fields): the upscale dispatch names the frame's colour, depth,
//! motion vectors, exposure, output, camera jitter and sizes, which is what
//! NVIDIA DLSS takes in its place.
//!
//! The title takes this path only on Series S; [`take_upscaler_path`] makes
//! it take it here. Its calls are then answered here, none by AMD's code:
//! the context and queries (FidelityFX's jitter sequence and render sizes,
//! so the title jitters and sizes as for FSR), and the upscale dispatch is
//! NVIDIA DLSS on the title's list (`victory_gpu::list::upscale`).
//! `VICTORY_FFX_DLSS=0` leaves the calls to the title's loader;
//! `VICTORY_FFX_WATCH=1` with it logs each call's description chain (types
//! and raw dwords; the first 200 calls, then every 1000th) and calls the
//! title's own export, handed over by `apps/victory` once the title's DLLs
//! are mapped ([`set_originals`]).
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::{HleExport, HleModule};

/// The exports watched, in the order of [`ORIGINALS`] and [`CALLS`].
pub const WATCHED: [&str; 5] = ["ffxCreateContext", "ffxDestroyContext", "ffxConfigure", "ffxQuery", "ffxDispatch"];

static ORIGINALS: [AtomicUsize; 5] = [const { AtomicUsize::new(0) }; 5];
static CALLS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// Hands over the title's own exports, `find` naming each.
pub fn set_originals(find: impl Fn(&str) -> Option<usize>) {
    for (i, name) in WATCHED.iter().enumerate() {
        match find(name) {
            Some(address) => ORIGINALS[i].store(address, Ordering::Relaxed),
            None => tracing::warn!(name, "ffx: the title's loader has no such export"),
        }
    }
}

/// The title's choice of upscaler, made to take its FidelityFX path, which
/// it otherwise takes on Series S only: the function at RVA 0x7e93f0 picks
/// type 7 (its FidelityFX upscaler object, whose table holds the
/// `ffxCreateContext`/`ffxDispatch` calls) when the device type from
/// `HKLM\...\OEM\ConsoleMode` is 6 (`0x60`, Series S), else type 6 (TAA2
/// at the render size, then a plain upscale after post-processing). Its
/// `cmovne eax, ecx` at RVA 0x7e9426 becomes `mov eax, ecx; nop`, so on
/// Series X the title runs its post-processing at the output size after
/// the upscaler, which DLSS then replaces. The device type itself is
/// unchanged: 70 other places ask it. Only for build `b99f876b…`: the bytes
/// are checked first. `VICTORY_FSR_PATH=0` leaves the title's choice.
///
/// On that path the title post-processes at its output size, which on
/// Series X is 3840x2160, and its own render-target pool, sized for the
/// Series X path (post-processing at 2560x1440), runs out at the first
/// 4K target (an assert at RVA 0x2219650 from the pool at 0x2474690). So
/// the output size the title takes into its globals (the function at RVA
/// 0x1de4030, which also derives the most it renders and asks the upscaler
/// for its render size) can become `VICTORY_OUTPUT_SIZE` (e.g. 2560x1440;
/// off unless given: the layout stays 4K, see below). [`pool_room`] makes
/// the room the 4K path needs instead.
pub fn take_upscaler_path(image_base: usize) {
    if std::env::var_os("VICTORY_FSR_PATH").is_some_and(|v| v == "0") {
        return;
    }
    if !patch(image_base, 0x7e941f, &[0x84, 0xc0, 0xb8, 0x06, 0x00, 0x00, 0x00, 0x0f, 0x45, 0xc1], 7, &[0x89, 0xc8, 0x90]) {
        tracing::info!("ffx: the title's upscaler choice is not where this build has it; left as it is");
        return;
    }
    tracing::info!("ffx: the title takes its FidelityFX upscaler path (VICTORY_FSR_PATH=0 for its TAA2 path)");
    pool_room(image_base);
    render_scale(image_base);
    // Off unless asked for: the title's layout (its UI, its composition)
    // stays at 4K from other readers, and a smaller output shows the top
    // left of it zoomed in (Joshua's screenshots of run `dlss-balanced-*`).
    let Ok(size) = std::env::var("VICTORY_OUTPUT_SIZE") else { return };
    let Some((w, h)) = size.split_once('x').and_then(|(w, h)| Some((w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?))) else {
        tracing::info!(%size, "ffx: the title's own output size");
        return;
    };
    // In the function that sets the output size from a settings object (RVA
    // 0x1de4030, reached from the settings at start and from the display
    // object later): `mov ebp, [rsi+114h]` and `mov r12d, [rsi+118h]`.
    let mut width = [0xbd, 0, 0, 0, 0, 0x90]; // mov ebp, imm32; nop
    width[1..5].copy_from_slice(&w.to_le_bytes());
    let mut height = [0x41, 0xbc, 0, 0, 0, 0, 0x90]; // mov r12d, imm32; nop
    height[2..6].copy_from_slice(&h.to_le_bytes());
    // And the helper that gives render targets their sizes from the display
    // object (RVA 0x7b4e10, behind the ~90 callers of its width and height
    // wrappers; the two RGBA8 output targets that ran the pool out are
    // sized there): `mov eax, [rcx+204h]` and `mov eax, [rcx+208h]`.
    let mut target_width = [0xb8, 0, 0, 0, 0, 0x90]; // mov eax, imm32; nop
    target_width[1..5].copy_from_slice(&w.to_le_bytes());
    let mut target_height = [0xb8, 0, 0, 0, 0, 0x90];
    target_height[1..5].copy_from_slice(&h.to_le_bytes());
    if patch(image_base, 0x1de4070, &[0x8b, 0xae, 0x14, 0x01, 0x00, 0x00], 0, &width)
        && patch(image_base, 0x1de40a5, &[0x44, 0x8b, 0xa6, 0x18, 0x01, 0x00, 0x00], 0, &height)
        && patch(image_base, 0x7b4e20, &[0x8b, 0x81, 0x04, 0x02, 0x00, 0x00], 0, &target_width)
        && patch(image_base, 0x7b4e53, &[0x8b, 0x81, 0x08, 0x02, 0x00, 0x00], 0, &target_height)
    {
        tracing::info!(width = w, height = h, "ffx: the title's output size on its upscaler path (VICTORY_OUTPUT_SIZE)");
    } else {
        tracing::warn!("ffx: the title's output size is not where this build reads it; left as it is");
    }
}

/// Room in the title's first memory pool for its upscaler path at 4K. The
/// title sizes its nine pools from a table per console (RVA 0x6186250,
/// rows of 0x50 bytes, Series S then Series X; copied at RVA 0x1e1ced0 and
/// mapped at 0x1e1cf60): pool 0 is 1162 MB on Series S, which takes this
/// path at a 1440p output, and 1172 MB on Series X, whose TAA2 path
/// post-processes at 2560x1440. On Series X at 4K the first full-size
/// RGBA8 target found it full (33 MB at 0x2219650). The Series X row's
/// pool 0 (RVA 0x61862a8) grows by `VICTORY_POOL0_EXTRA_MB` (1536 unless
/// given), from the host's memory.
fn pool_room(image_base: usize) {
    const RVA: usize = 0x61862a8;
    const BEFORE: u64 = 0x4940_0000; // 1172 MB
    let extra: u64 = std::env::var("VICTORY_POOL0_EXTRA_MB").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(1536);
    if extra == 0 {
        return;
    }
    let after = BEFORE + (extra << 20);
    if patch(image_base, RVA, &BEFORE.to_le_bytes(), 0, &after.to_le_bytes()) {
        tracing::info!(pool_mb = after >> 20, "ffx: the title's first memory pool grown for its upscaler path at 4K (VICTORY_POOL0_EXTRA_MB)");
    } else {
        tracing::warn!("ffx: the title's pool sizes are not where this build has them; left as they are");
    }
}

/// The size the title renders at on its upscaler path: its FidelityFX
/// object's render-size method (RVA 0x28ef680) takes the output size times
/// a fixed scale, 0.6667 on Series S (FSR Quality) and 1.0 otherwise (the
/// whole output: DLSS could only be DLAA). `VICTORY_RENDER_SCALE` (0.58
/// unless given: DLSS Balanced, output / 1.72; `native` keeps the title's)
/// replaces the choice between the two (`test al, al` through both loads,
/// 22 bytes at RVA 0x28ef6a4) with `mov eax, <scale>; vmovd xmm0, eax`.
fn render_scale(image_base: usize) {
    let scale = std::env::var("VICTORY_RENDER_SCALE").unwrap_or_else(|_| "0.58".to_owned());
    let Ok(s) = scale.trim().parse::<f32>() else {
        tracing::info!(%scale, "ffx: the title's own render scale");
        return;
    };
    let s = s.clamp(0.25, 1.0);
    const BEFORE: [u8; 22] = [
        0x84, 0xc0, 0x75, 0x0a, 0xc5, 0xfa, 0x10, 0x05, 0x28, 0x78, 0x6d, 0x03, 0xeb, 0x08, 0xc5, 0xfa, 0x10, 0x05, 0xa2,
        0xe2, 0x86, 0x03,
    ];
    let mut after = [0x90u8; 22];
    after[0] = 0xb8; // mov eax, imm32
    after[1..5].copy_from_slice(&s.to_bits().to_le_bytes());
    after[5..9].copy_from_slice(&[0xc5, 0xf9, 0x6e, 0xc0]); // vmovd xmm0, eax
    if patch(image_base, 0x28ef6a4, &BEFORE, 0, &after) {
        tracing::info!(scale = s, "ffx: the title's render scale on its upscaler path (VICTORY_RENDER_SCALE)");
    } else {
        tracing::warn!("ffx: the title's render scale is not where this build has it; left as it is");
    }
}

/// Writes `after` at `offset` into the title's code at `rva`, if the bytes
/// there are `before`. Whether it did.
fn patch(image_base: usize, rva: usize, before: &[u8], offset: usize, after: &[u8]) -> bool {
    let at = image_base + rva;
    if read(at as u64, before.len()).as_deref() != Some(before) {
        return false;
    }
    use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
    let target = (at + offset) as *mut u8;
    let mut old = 0;
    // SAFETY: bytes of the title's mapped code, checked above; the protection
    // is restored after.
    unsafe {
        if VirtualProtect(target.cast(), after.len(), PAGE_EXECUTE_READWRITE, &mut old) == 0 {
            return false;
        }
        std::ptr::copy_nonoverlapping(after.as_ptr(), target, after.len());
        let mut ignored = 0;
        VirtualProtect(target.cast(), after.len(), old, &mut ignored);
    }
    true
}

/// Whether the loader is watched (`VICTORY_FFX_WATCH=1`).
pub fn watched() -> bool {
    std::env::var_os("VICTORY_FFX_WATCH").is_some_and(|v| v == "1")
}

fn call(i: usize) -> (bool, u64, usize) {
    let n = CALLS[i].fetch_add(1, Ordering::Relaxed);
    (n < 200 || n % 1000 == 0, n, ORIGINALS[i].load(Ordering::Relaxed))
}

/// `len` bytes at `at`, if all of it is readable.
fn read(at: u64, len: usize) -> Option<Vec<u8>> {
    if at < 0x10000 {
        return None;
    }
    let mut bytes = vec![0u8; len];
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut done = 0;
    // SAFETY: the kernel checks the source; the destination is ours.
    let ok = unsafe { ReadProcessMemory(GetCurrentProcess(), at as *const _, bytes.as_mut_ptr().cast(), len, &mut done) };
    (ok != 0 && done == len).then_some(bytes)
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    b.get(at..at + 8).map_or(0, |w| u64::from_le_bytes(w.try_into().unwrap_or_default()))
}

/// A description chain from `at` as text: each link's type and its first
/// 256 bytes after the header, raw.
fn chain(mut at: u64) -> String {
    let mut text = String::new();
    for _ in 0..8 {
        let Some(b) = read(at, 16 + 256) else {
            if at != 0 {
                text += &format!(" [{at:#x} unreadable]");
            }
            break;
        };
        let (kind, next) = (u64_at(&b, 0), u64_at(&b, 8));
        let words: Vec<String> = b[16..].chunks(4).map(|w| format!("{:08x}", u32::from_le_bytes(w.try_into().unwrap_or_default()))).collect();
        text += &format!(" [type {kind:#x} at {at:#x}: {}]", words.join(" "));
        at = next;
    }
    text
}

/// Calls the title's export `original` with `args` (up to three pointer-sized
/// arguments; the API's calls take two or three).
fn through(original: usize, a: u64, b: u64, c: u64) -> u32 {
    if original == 0 {
        // FFX_API_RETURN_ERROR
        return 1;
    }
    // SAFETY: the title's own export, with the title's arguments (extra ones are ignored
    // by a callee that takes fewer, in the Microsoft x64 convention).
    let f: extern "system" fn(u64, u64, u64) -> u32 = unsafe { std::mem::transmute(original) };
    f(a, b, c)
}

/// Whether DLSS answers the title's FidelityFX calls in place of AMD's
/// code (`VICTORY_FFX_DLSS=0` leaves them to the title's loader).
pub fn replaced() -> bool {
    std::env::var_os("VICTORY_FFX_DLSS").is_none_or(|v| v != "0")
}

// FidelityFX API (`ffx_api.h`, `ffx_api_upscale.h`): description types and
// return codes.
const OK: u32 = 0;
const ERROR_PARAMETER: u32 = 6;
const CREATE_UPSCALE: u64 = 0x1_0000;
const DISPATCH_UPSCALE: u64 = 0x1_0001;
const QUERY_RATIO_FROM_QUALITY: u64 = 0x1_0002;
const QUERY_RENDER_SIZE_FROM_QUALITY: u64 = 0x1_0003;
const QUERY_JITTER_PHASE_COUNT: u64 = 0x1_0004;
const QUERY_JITTER_OFFSET: u64 = 0x1_0005;
const DISPATCH_REACTIVE_MASK: u64 = 0x1_0006;
// `ffxCreateContextDescUpscale.flags`.
const FLAG_HDR: u32 = 1 << 0;
const FLAG_MV_DISPLAY_SIZE: u32 = 1 << 1;
const FLAG_DEPTH_INVERTED: u32 = 1 << 3;
const FLAG_AUTO_EXPOSURE: u32 = 1 << 5;
/// The context handed to the title: opaque to it.
const CONTEXT: u64 = 0xd155_0000_0000_0001;
/// The flags the title created its context with.
static CREATED: AtomicU64 = AtomicU64::new(0);

/// FidelityFX's upscale ratio per quality mode (native, Quality, Balanced,
/// Performance, Ultra Performance).
fn ratio(mode: u32) -> f32 {
    [1.0, 1.5, 1.7, 2.0, 3.0].get(mode as usize).copied().unwrap_or(1.0)
}

/// FidelityFX's jitter sequence: Halton bases 2 and 3.
fn halton(mut index: i32, base: i32) -> f32 {
    let (mut f, mut r) = (1.0f32, 0.0f32);
    while index > 0 {
        f /= base as f32;
        r += f * (index % base) as f32;
        index /= base;
    }
    r
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map_or(0, |w| u32::from_le_bytes(w.try_into().unwrap_or_default()))
}

fn f32_at(b: &[u8], at: usize) -> f32 {
    f32::from_bits(u32_at(b, at))
}

/// Writes `v` at the title's pointer `at`.
fn put<T: Copy>(at: u64, v: T) {
    if at >= 0x10000 {
        // SAFETY: an output pointer the title passes for this value.
        unsafe { (at as *mut T).write_unaligned(v) };
    }
}

/// The first link of a chain whose type is `kind`, its first `len` bytes.
fn link(mut at: u64, kind: u64, len: usize) -> Option<Vec<u8>> {
    for _ in 0..8 {
        let b = read(at, len.max(16))?;
        if u64_at(&b, 0) == kind {
            return Some(b);
        }
        at = u64_at(&b, 8);
    }
    None
}

fn create_dlss(context: u64, desc: u64) -> u32 {
    let Some(d) = link(desc, CREATE_UPSCALE, 40) else {
        tracing::warn!("ffx: a context that is not an upscaler; refused:{}", chain(desc));
        return ERROR_PARAMETER;
    };
    let flags = u32_at(&d, 16);
    CREATED.store(u64::from(flags), Ordering::Relaxed);
    put(context, CONTEXT);
    tracing::info!(
        flags = format_args!("{flags:#x}"),
        max_render = ?[u32_at(&d, 20), u32_at(&d, 24)],
        max_output = ?[u32_at(&d, 28), u32_at(&d, 32)],
        "ffx: the title's upscaler context, answered by DLSS"
    );
    OK
}

fn query_dlss(desc: u64) -> u32 {
    let Some(head) = read(desc, 16) else { return ERROR_PARAMETER };
    let kind = u64_at(&head, 0);
    let Some(d) = read(desc, 48) else { return ERROR_PARAMETER };
    static ASKED: AtomicU64 = AtomicU64::new(0);
    if ASKED.fetch_add(1, Ordering::Relaxed) < 12 {
        tracing::info!(kind = format_args!("{kind:#x}"), a = u32_at(&d, 16), b = u32_at(&d, 20), c = u32_at(&d, 24), "ffx: the title asks");
    }
    match kind {
        QUERY_JITTER_PHASE_COUNT => {
            let (render, display) = (u32_at(&d, 16).max(1), u32_at(&d, 20));
            let ratio = display as f32 / render as f32;
            put(u64_at(&d, 24), ((8.0 * ratio * ratio) as i32).max(1));
        }
        QUERY_JITTER_OFFSET => {
            let (index, phases) = (u32_at(&d, 16) as i32, (u32_at(&d, 20) as i32).max(1));
            let i = index.rem_euclid(phases) + 1;
            put(u64_at(&d, 24), halton(i, 2) - 0.5);
            put(u64_at(&d, 32), halton(i, 3) - 0.5);
        }
        QUERY_RENDER_SIZE_FROM_QUALITY => {
            let r = ratio(u32_at(&d, 24));
            put(u64_at(&d, 32), (u32_at(&d, 16) as f32 / r) as u32);
            put(u64_at(&d, 40), (u32_at(&d, 20) as f32 / r) as u32);
            static SHOWN: std::sync::Once = std::sync::Once::new();
            SHOWN.call_once(|| tracing::info!(quality = u32_at(&d, 24), ratio = r, "ffx: the title's quality mode"));
        }
        QUERY_RATIO_FROM_QUALITY => put(u64_at(&d, 24), ratio(u32_at(&d, 16))),
        _ => {
            static SHOWN: AtomicU64 = AtomicU64::new(0);
            if SHOWN.fetch_add(1, Ordering::Relaxed) < 16 {
                tracing::info!(kind = format_args!("{kind:#x}"), "ffx: a query DLSS does not answer:{}", chain(desc));
            }
        }
    }
    OK
}

/// The upscale dispatch (`ffxDispatchDescUpscale`): the command list at
/// +16, seven resources of 48 bytes (`void*` resource, 32-byte description,
/// state) from +24 (colour, depth, motion vectors, exposure, reactive,
/// transparency, output), then jitter +360, motion vector scale +368,
/// render size +376, output size +384, pre-exposure +404, reset +408.
fn dispatch_dlss(desc: u64) -> u32 {
    let Some(head) = read(desc, 16) else { return ERROR_PARAMETER };
    match u64_at(&head, 0) {
        DISPATCH_REACTIVE_MASK => return OK,
        DISPATCH_UPSCALE => {}
        kind => {
            tracing::warn!(kind = format_args!("{kind:#x}"), "ffx: a dispatch DLSS does not take");
            return OK;
        }
    }
    let Some(d) = read(desc, 432) else { return ERROR_PARAMETER };
    let resource = |i: usize| u64_at(&d, 24 + 48 * i) as usize;
    let flags = CREATED.load(Ordering::Relaxed) as u32;
    let pair = |name: &str| -> [f32; 2] {
        std::env::var(name)
            .ok()
            .and_then(|v| {
                let (a, b) = v.split_once(',')?;
                Some([a.trim().parse().ok()?, b.trim().parse().ok()?])
            })
            .unwrap_or([1.0, 1.0])
    };
    let (js, ms) = (pair("VICTORY_DLSS_JITTER"), pair("VICTORY_DLSS_MV"));
    let u = victory_gpu::list::Upscale {
        color: resource(0),
        depth: resource(1),
        motion: resource(2),
        exposure: resource(3),
        output: resource(6),
        jitter: [f32_at(&d, 360) * js[0], f32_at(&d, 364) * js[1]],
        mv_scale: [f32_at(&d, 368) * ms[0], f32_at(&d, 372) * ms[1]],
        render: [u32_at(&d, 376), u32_at(&d, 380)],
        upscale: [u32_at(&d, 384), u32_at(&d, 388)],
        pre_exposure: f32_at(&d, 404),
        reset: d[408] != 0,
        hdr: flags & FLAG_HDR != 0,
        depth_inverted: flags & FLAG_DEPTH_INVERTED != 0,
        auto_exposure: flags & FLAG_AUTO_EXPOSURE != 0,
        mv_display_size: flags & FLAG_MV_DISPLAY_SIZE != 0,
    };
    let list = u64_at(&d, 16) as usize;
    let sane = list != 0
        && [u.color, u.depth, u.motion, u.output].iter().all(|&r| r != 0)
        && u.render.iter().all(|&v| (1..=16384).contains(&v))
        && u.render[0] <= u.upscale[0]
        && u.render[1] <= u.upscale[1];
    static SHOWN: AtomicU64 = AtomicU64::new(0);
    let n = SHOWN.fetch_add(1, Ordering::Relaxed);
    if n < 3 || !sane {
        tracing::info!(?u, sane, "ffx: the title's upscale dispatch:{}", chain(desc));
    } else if n < 40 || n % 300 == 0 {
        tracing::info!(n, jitter = ?u.jitter, render = ?u.render, reset = u.reset, "ffx: upscale");
    }
    if !sane {
        return ERROR_PARAMETER;
    }
    if !victory_gpu::list::upscale(list, &u) {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!("ffx: DLSS did not run this upscale"));
    }
    OK
}

extern "system" fn create(context: u64, desc: u64, memory: u64) -> u32 {
    if replaced() {
        return create_dlss(context, desc);
    }
    let (log, n, original) = call(0);
    let r = through(original, context, desc, memory);
    if log {
        let handle = read(context, 8).map_or(0, |b| u64_at(&b, 0));
        tracing::info!(n, result = r, context = format_args!("{handle:#x}"), "ffx create:{}", chain(desc));
    }
    r
}

extern "system" fn destroy(context: u64, memory: u64) -> u32 {
    if replaced() {
        return OK;
    }
    let (log, n, original) = call(1);
    if log {
        tracing::info!(n, "ffx destroy");
    }
    through(original, context, memory, 0)
}

extern "system" fn configure(context: u64, desc: u64) -> u32 {
    if replaced() {
        return OK;
    }
    let (log, n, original) = call(2);
    if log {
        tracing::info!(n, "ffx configure:{}", chain(desc));
    }
    through(original, context, desc, 0)
}

extern "system" fn query(context: u64, desc: u64) -> u32 {
    if replaced() {
        return query_dlss(desc);
    }
    let (log, n, original) = call(3);
    let r = through(original, context, desc, 0);
    if log {
        tracing::info!(n, result = r, "ffx query:{}", chain(desc));
    }
    r
}

extern "system" fn dispatch(context: u64, desc: u64) -> u32 {
    if replaced() {
        return dispatch_dlss(desc);
    }
    let (log, n, original) = call(4);
    if log {
        tracing::info!(n, "ffx dispatch:{}", chain(desc));
    }
    through(original, context, desc, 0)
}

pub(crate) fn module() -> HleModule {
    let exports: [(&'static str, usize); 5] = [
        ("ffxCreateContext", create as *const () as usize),
        ("ffxDestroyContext", destroy as *const () as usize),
        ("ffxConfigure", configure as *const () as usize),
        ("ffxQuery", query as *const () as usize),
        ("ffxDispatch", dispatch as *const () as usize),
    ];
    HleModule {
        name: "ffx",
        // The loader the title imports, and the upscaler it looks up at run time.
        dlls: &["amd_fidelityfx_loader_xbox.dll", "amd_fidelityfx_upscaler_xbox.dll"],
        exports: if watched() || replaced() {
            exports.into_iter().map(|(name, address)| HleExport { name, address }).collect()
        } else {
            Vec::new()
        },
    }
}
