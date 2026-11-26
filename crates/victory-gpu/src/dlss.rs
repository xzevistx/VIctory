//! NVIDIA DLSS as the title's upscaler. The title has an upscaler path made
//! for AMD's FidelityFX (FSR): render at a lower size, upscale with jitter,
//! depth and motion vectors, then post-process at the output size. It takes
//! that path on Series S only; `victory_hle::modules::ffx` makes it take it
//! here and answers its FidelityFX calls, and its upscale dispatch comes
//! here ([`crate::list::upscale`]): DLSS writes the output FidelityFX would
//! have, and none of AMD's code (nor the title's TAA2) runs.
//!
//! Preset K (the transformer model) for every mode, as Joshua asked
//! (`VICTORY_DLSS_PRESET` picks another); the mode follows the title's
//! render and output sizes (Balanced at a render scale of 0.58).

use std::ffi::{c_char, c_void};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;
use windows::core::Interface;

use crate::host::HostList;

#[repr(C)]
struct Param(c_void);
#[repr(C)]
struct Handle(c_void);

const SUCCESS: u32 = 1;
const FEATURE_SUPER_SAMPLING: i32 = 1;
const ENGINE_CUSTOM: i32 = 0;
const SDK_VERSION: u32 = 0x15;
const QUALITY_MAX_PERF: i32 = 0;
const QUALITY_BALANCED: i32 = 1;
const QUALITY_MAX_QUALITY: i32 = 2;
const QUALITY_ULTRA_PERFORMANCE: i32 = 3;
const QUALITY_DLAA: i32 = 5;
/// DLSS feature flags.
pub(crate) const FLAG_HDR: i32 = 1 << 0;
pub(crate) const FLAG_MV_LOW_RES: i32 = 1 << 1;
pub(crate) const FLAG_DEPTH_INVERTED: i32 = 1 << 3;
pub(crate) const FLAG_AUTO_EXPOSURE: i32 = 1 << 6;
/// The DLSS preset (`VICTORY_DLSS_PRESET`): K (the transformer model, as
/// Joshua asked) unless given; J, L or M by letter; `default` (0) leaves it
/// to DLSS and to a driver override (NVIDIA App, Profile Inspector).
fn preset() -> (u32, &'static str) {
    match std::env::var("VICTORY_DLSS_PRESET").unwrap_or_default().trim().to_ascii_uppercase().as_str() {
        "DEFAULT" | "0" => (0, "default"),
        "J" => (10, "J"),
        "L" => (12, "L"),
        "M" => (13, "M"),
        _ => (11, "K"),
    }
}

unsafe extern "C" {
    fn NVSDK_NGX_D3D12_Init_with_ProjectID(
        project: *const c_char,
        engine: i32,
        engine_version: *const c_char,
        data_path: *const u16,
        device: *mut c_void,
        info: *const c_void,
        sdk: u32,
    ) -> u32;
    fn NVSDK_NGX_D3D12_GetCapabilityParameters(out: *mut *mut Param) -> u32;
    fn NVSDK_NGX_D3D12_CreateFeature(list: *mut c_void, feature: i32, params: *const Param, out: *mut *mut Handle) -> u32;
    fn NVSDK_NGX_D3D12_EvaluateFeature_C(list: *mut c_void, handle: *const Handle, params: *const Param, progress: *const c_void) -> u32;
    fn NVSDK_NGX_Parameter_SetUI(p: *mut Param, name: *const c_char, v: u32);
    fn NVSDK_NGX_Parameter_SetI(p: *mut Param, name: *const c_char, v: i32);
    fn NVSDK_NGX_Parameter_SetF(p: *mut Param, name: *const c_char, v: f32);
    fn NVSDK_NGX_Parameter_SetD3d12Resource(p: *mut Param, name: *const c_char, v: *mut c_void);
    fn NVSDK_NGX_Parameter_GetI(p: *mut Param, name: *const c_char, out: *mut i32) -> u32;
}

/// NGX, once initialised on the host device: its parameter block.
struct Ngx(*mut Param);
// SAFETY: the block is used only under `FEATURE`'s lock.
unsafe impl Send for Ngx {}
unsafe impl Sync for Ngx {}

/// The DLSS feature, for one render size, output size and set of flags.
struct Feature {
    handle: *mut Handle,
    key: [u32; 5],
}
// SAFETY: as `Ngx`.
unsafe impl Send for Feature {}

static FEATURE: Mutex<Option<Feature>> = Mutex::new(None);

fn ngx() -> Option<&'static Ngx> {
    static NGX: OnceLock<Option<Ngx>> = OnceLock::new();
    NGX.get_or_init(|| {
        let host = crate::host::host()?;
        // NGX writes its logs here.
        let dir = std::env::temp_dir().join("victory-ngx");
        let _ = std::fs::create_dir_all(&dir);
        let path: Vec<u16> = dir.to_string_lossy().encode_utf16().chain([0]).collect();
        // SAFETY: a live device; NUL-terminated strings that outlive the call.
        let r = unsafe {
            NVSDK_NGX_D3D12_Init_with_ProjectID(
                c"5f1c5a2e-6b4d-4f0a-9d63-7a1b2c3d4e5f".as_ptr(),
                ENGINE_CUSTOM,
                c"1.0".as_ptr(),
                path.as_ptr(),
                host.device.as_raw(),
                std::ptr::null(),
                SDK_VERSION,
            )
        };
        if r != SUCCESS {
            tracing::warn!(result = format_args!("{r:#x}"), "DLSS: NGX did not start");
            return None;
        }
        let mut params = std::ptr::null_mut();
        // SAFETY: NGX is initialised.
        let r = unsafe { NVSDK_NGX_D3D12_GetCapabilityParameters(&mut params) };
        if r != SUCCESS || params.is_null() {
            tracing::warn!(result = format_args!("{r:#x}"), "DLSS: no NGX parameters");
            return None;
        }
        let mut available = 0;
        // SAFETY: a live parameter block.
        unsafe { NVSDK_NGX_Parameter_GetI(params, c"SuperSampling.Available".as_ptr(), &mut available) };
        if available == 0 {
            tracing::warn!("DLSS: not available on this card or driver");
            return None;
        }
        tracing::info!(log = %dir.display(), "DLSS: NGX started");
        Some(Ngx(params))
    })
    .as_ref()
}

/// DLSS's quality mode for an upscale by `ratio` (output over render width):
/// the nearest of DLAA 1.0, Quality 1.5, Balanced 1.72, Performance 2.0 and
/// Ultra Performance 3.0 (FidelityFX's Balanced is 1.7).
fn quality(ratio: f32) -> (i32, &'static str) {
    match ratio {
        r if r < 1.05 => (QUALITY_DLAA, "DLAA"),
        r if r < 1.6 => (QUALITY_MAX_QUALITY, "Quality"),
        r if r < 1.85 => (QUALITY_BALANCED, "Balanced"),
        r if r < 2.5 => (QUALITY_MAX_PERF, "Performance"),
        _ => (QUALITY_ULTRA_PERFORMANCE, "Ultra Performance"),
    }
}

/// One upscale: the title's resources and what its FidelityFX dispatch said.
pub(crate) struct Frame<'a> {
    pub(crate) color: &'a ID3D12Resource,
    pub(crate) depth: &'a ID3D12Resource,
    pub(crate) motion: &'a ID3D12Resource,
    pub(crate) output: &'a ID3D12Resource,
    pub(crate) exposure: Option<&'a ID3D12Resource>,
    /// The size rendered (the top left of the inputs).
    pub(crate) render: [u32; 2],
    /// The output size.
    pub(crate) upscale: [u32; 2],
    /// The projection's jitter, in render pixels.
    pub(crate) jitter: [f32; 2],
    /// What turns the motion vectors into render pixels.
    pub(crate) mv_scale: [f32; 2],
    pub(crate) pre_exposure: f32,
    pub(crate) reset: bool,
    /// `FLAG_*`.
    pub(crate) flags: i32,
}

/// Records DLSS on `list`. Whether it did. The list's pipeline, root
/// signature and heaps are NGX's afterwards: the caller binds again.
pub(crate) fn evaluate(list: &HostList, f: Frame<'_>) -> bool {
    let Some(ngx) = ngx() else { return false };
    let p = ngx.0;
    let raw = list.list.as_raw();
    let key = [f.render[0], f.render[1], f.upscale[0], f.upscale[1], f.flags as u32];
    let mut feature = FEATURE.lock().unwrap_or_else(|e| e.into_inner());
    if feature.as_ref().is_none_or(|ft| ft.key != key) {
        let (mode, name) = quality(f.upscale[0] as f32 / f.render[0].max(1) as f32);
        let (preset, preset_name) = preset();
        // SAFETY: a live parameter block; names are NUL-terminated.
        unsafe {
            for hint in [
                c"DLSS.Hint.Render.Preset.DLAA",
                c"DLSS.Hint.Render.Preset.Quality",
                c"DLSS.Hint.Render.Preset.Balanced",
                c"DLSS.Hint.Render.Preset.Performance",
                c"DLSS.Hint.Render.Preset.UltraPerformance",
                c"DLSS.Hint.Render.Preset.UltraQuality",
            ] {
                NVSDK_NGX_Parameter_SetUI(p, hint.as_ptr(), preset);
            }
            NVSDK_NGX_Parameter_SetUI(p, c"CreationNodeMask".as_ptr(), 1);
            NVSDK_NGX_Parameter_SetUI(p, c"VisibilityNodeMask".as_ptr(), 1);
            NVSDK_NGX_Parameter_SetUI(p, c"Width".as_ptr(), f.render[0]);
            NVSDK_NGX_Parameter_SetUI(p, c"Height".as_ptr(), f.render[1]);
            NVSDK_NGX_Parameter_SetUI(p, c"OutWidth".as_ptr(), f.upscale[0]);
            NVSDK_NGX_Parameter_SetUI(p, c"OutHeight".as_ptr(), f.upscale[1]);
            NVSDK_NGX_Parameter_SetI(p, c"PerfQualityValue".as_ptr(), mode);
            NVSDK_NGX_Parameter_SetI(p, c"DLSS.Feature.Create.Flags".as_ptr(), f.flags);
            NVSDK_NGX_Parameter_SetI(p, c"DLSS.Enable.Output.Subrects".as_ptr(), 0);
        }
        let mut handle = std::ptr::null_mut();
        // SAFETY: an open list on the device NGX was given.
        let r = unsafe { NVSDK_NGX_D3D12_CreateFeature(raw, FEATURE_SUPER_SAMPLING, p, &mut handle) };
        if r != SUCCESS || handle.is_null() {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!(result = format_args!("{r:#x}"), ?key, "DLSS: the feature was not made"));
            return false;
        }
        // shortcut: a feature made for another size is not released (lists in
        // flight may still use it); sizes change rarely.
        tracing::info!(render = ?f.render, output = ?f.upscale, flags = f.flags, mode = name, preset = preset_name, "DLSS: feature made");
        *feature = Some(Feature { handle, key });
    }
    let Some(ft) = feature.as_ref() else { return false };
    for r in [f.color, f.depth, f.motion, f.output].into_iter().chain(f.exposure) {
        list.keep(r);
    }
    // SAFETY: a live parameter block and feature; resources the list keeps,
    // in the states the caller put them in (shader resources, the output
    // unordered access).
    let r = unsafe {
        NVSDK_NGX_Parameter_SetD3d12Resource(p, c"Color".as_ptr(), f.color.as_raw());
        NVSDK_NGX_Parameter_SetD3d12Resource(p, c"Output".as_ptr(), f.output.as_raw());
        NVSDK_NGX_Parameter_SetD3d12Resource(p, c"Depth".as_ptr(), f.depth.as_raw());
        NVSDK_NGX_Parameter_SetD3d12Resource(p, c"MotionVectors".as_ptr(), f.motion.as_raw());
        NVSDK_NGX_Parameter_SetD3d12Resource(p, c"ExposureTexture".as_ptr(), f.exposure.map_or(std::ptr::null_mut(), |e| e.as_raw()));
        NVSDK_NGX_Parameter_SetF(p, c"Jitter.Offset.X".as_ptr(), f.jitter[0]);
        NVSDK_NGX_Parameter_SetF(p, c"Jitter.Offset.Y".as_ptr(), f.jitter[1]);
        NVSDK_NGX_Parameter_SetF(p, c"Sharpness".as_ptr(), 0.0);
        NVSDK_NGX_Parameter_SetI(p, c"Reset".as_ptr(), i32::from(f.reset));
        NVSDK_NGX_Parameter_SetF(p, c"MV.Scale.X".as_ptr(), f.mv_scale[0]);
        NVSDK_NGX_Parameter_SetF(p, c"MV.Scale.Y".as_ptr(), f.mv_scale[1]);
        NVSDK_NGX_Parameter_SetUI(p, c"DLSS.Render.Subrect.Dimensions.Width".as_ptr(), f.render[0]);
        NVSDK_NGX_Parameter_SetUI(p, c"DLSS.Render.Subrect.Dimensions.Height".as_ptr(), f.render[1]);
        NVSDK_NGX_Parameter_SetF(p, c"DLSS.Pre.Exposure".as_ptr(), if f.pre_exposure > 0.0 { f.pre_exposure } else { 1.0 });
        NVSDK_NGX_D3D12_EvaluateFeature_C(raw, ft.handle, p, std::ptr::null())
    };
    if r != SUCCESS {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!(result = format_args!("{r:#x}"), "DLSS: evaluation failed"));
        return false;
    }
    static FIRST: std::sync::Once = std::sync::Once::new();
    FIRST.call_once(|| {
        tracing::info!(render = ?f.render, output = ?f.upscale, jitter = ?f.jitter, mv_scale = ?f.mv_scale, "DLSS: first frame upscaled")
    });
    true
}
