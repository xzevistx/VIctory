//! Pipeline states. Both descriptions start as on PC: the root signature,
//! then the shader stages (five for graphics, one for compute); streams
//! carry the rest of the graphics state as subobjects of the standard
//! types.
//!
//! Host pipelines are made on a pipeline's first draw, not at creation:
//! The title creates some 60,000 pipelines in its first 20
//! seconds and draws with a few dozen (decision 0007, step 3). Shader
//! containers are shared by content hash and translated once, on first
//! use (decision 0003).
//!
//! shortcut: only vertex plus pixel pipelines from streams and compute
//! pipelines get host pipelines; mesh, tessellation and geometry stages,
//! and the plain graphics description, keep their containers only. The 32-byte
//! packet after the header stays zero: titles only copy it inline, which
//! our null command-list limits keep from running.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::HashMap;
use std::mem::offset_of;
use std::sync::{Arc, Mutex, OnceLock};

use crate::object::{self, E_INVALIDARG, HRESULT, Iid, Object, PIPELINE, Table, This, guid};
use crate::resource::PAGEABLE_IIDS;

const PIPELINE_IIDS: [Iid; 4] = [
    guid(
        0x765a30f3,
        0xf624,
        0x4c6f,
        [0xa8, 0x28, 0xac, 0xe9, 0x48, 0x62, 0x24, 0x45],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
pub(crate) struct Bytecode {
    code: *const u8,
    size: usize,
}

#[repr(C)]
pub(crate) struct GraphicsHead {
    root: This,
    /// Vertex, pixel, domain, hull, geometry.
    stages: [Bytecode; 5],
}

#[repr(C)]
pub(crate) struct ComputeHead {
    root: This,
    stage: Bytecode,
}

/// A shader container a title passed, shared by every pipeline with the
/// same bytes.
pub(crate) struct Shader {
    pub(crate) stage: &'static str,
    /// The first 8 bytes of its BLAKE3, as logged.
    pub(crate) hash: String,
    container: Vec<u8>,
    /// Its part in the stand-in for the indirect light.
    #[cfg(windows)]
    pub(crate) ambient: Option<crate::ambient::Role>,
    /// Its part in the light probe (`crate::lightprobe`).
    #[cfg(windows)]
    pub(crate) probe: Option<crate::lightprobe::Role>,
    #[cfg(windows)]
    translated: OnceLock<Option<Translated>>,
    /// The translation with float exports clamped ([`Shader::translated_clamped`]).
    #[cfg(windows)]
    clamped: OnceLock<Option<Translated>>,
}

impl std::fmt::Debug for Shader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.stage, self.hash)
    }
}

/// Every container seen, by full content hash.
static SHADERS: Mutex<Option<HashMap<[u8; 32], Arc<Shader>>>> = Mutex::new(None);

/// For the memory lines: containers kept and their bytes, translations
/// kept and the bytes of their host code. None of them is ever let go.
static KEPT: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// What the shader table and the pipeline library keep in memory:
/// containers and their bytes, translations and their bytes, the bytes of
/// the library's file (kept whole while the library lives) and what its
/// last save wrote.
#[cfg(windows)]
#[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
pub(crate) fn kept() -> [u64; 7] {
    use std::sync::atomic::Ordering::Relaxed;
    [
        KEPT[0].load(Relaxed),
        KEPT[1].load(Relaxed),
        KEPT[2].load(Relaxed),
        KEPT[3].load(Relaxed),
        library::BLOB.load(Relaxed),
        library::SAVED.load(Relaxed),
        library::TRIMMED_BYTES.load(Relaxed),
    ]
}

/// The shared container for `bytes`.
fn shader(stage: &'static str, bytes: &[u8]) -> Arc<Shader> {
    let full = blake3::hash(bytes);
    let mut shaders = SHADERS.lock().unwrap_or_else(|e| e.into_inner());
    shaders
        .get_or_insert_with(HashMap::new)
        .entry(*full.as_bytes())
        .or_insert_with(|| {
            KEPT[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            KEPT[1].fetch_add(bytes.len() as u64, std::sync::atomic::Ordering::Relaxed);
            Arc::new(Shader {
                stage,
                hash: full.to_hex()[..16].to_owned(),
                container: bytes.to_vec(),
                #[cfg(windows)]
                ambient: (stage == "compute").then(|| crate::ambient::role(bytes)).flatten(),
                #[cfg(windows)]
                probe: crate::lightprobe::on().then(|| crate::lightprobe::role(bytes)).flatten(),
                #[cfg(windows)]
                translated: OnceLock::new(),
                #[cfg(windows)]
                clamped: OnceLock::new(),
            })
        })
        .clone()
}

/// A translated shader and the console resources its code reads.
#[cfg(windows)]
pub(crate) struct Translated {
    pub(crate) host: victory_shader::emit::HostShader,
    /// A compute program's threads in a group.
    pub(crate) threads: Option<[u32; 3]>,
    pub(crate) used: Vec<victory_shader::UsedResource>,
    pub(crate) tables: crate::draw::TablePlan,
    /// Whether the shader's code can store to buffer memory ([`stores`]);
    /// true when its code is not known.
    pub(crate) stores: bool,
    /// Whether it can store through raw addresses ([`stores_by_address`]).
    pub(crate) stores_by_address: bool,
    /// Whether it stores through a view it picks from the heap by an index,
    /// once asked (`crate::picked`).
    pub(crate) picks: std::sync::atomic::AtomicU8,
}

#[cfg(windows)]
impl Shader {
    /// The translation, made on first use; a shader that does not
    /// translate is logged once.
    pub(crate) fn translated(&self) -> Option<&Translated> {
        self.translated
            .get_or_init(|| {
                let done = {
                    let _translation = crate::timeline::span("pipeline: translate a shader");
                    translate(&self.container)
                };
                // `VICTORY_SHADER_SAVE=<dir>`: each shader as the title gave it (`.cdxil`) and as
                // translated (`.dxil`), by the hash crash reports name it with
                if let Some(dir) = std::env::var_os("VICTORY_SHADER_SAVE") {
                    let dir = std::path::PathBuf::from(dir);
                    let _ = std::fs::create_dir_all(&dir);
                    let stem = format!("{}.{}", self.hash, self.stage);
                    let _ = std::fs::write(dir.join(format!("{stem}.cdxil")), &self.container);
                    if let Ok(t) = &done {
                        let _ = std::fs::write(dir.join(format!("{stem}.dxil")), &t.host.dxil);
                    }
                }
                if let Ok(t) = &done {
                    KEPT[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    KEPT[3].fetch_add(t.host.dxil.len() as u64, std::sync::atomic::Ordering::Relaxed);
                }
                match &done {
                    Ok(t) => tracing::info!(
                        stage = self.stage,
                        hash = %self.hash,
                        user_data = t.host.user_data.len(),
                        samplers = t.host.samplers.len(),
                        "shader translated"
                    ),
                    Err(e) => tracing::warn!(
                        stage = self.stage,
                        hash = %self.hash,
                        error = %e,
                        "shader does not translate; its pipelines' draws are dropped"
                    ),
                }
                done.ok()
            })
            .as_ref()
    }
}

#[cfg(windows)]
impl Shader {
    /// The console programs of the container; `None` if they are not known.
    pub(crate) fn programs(&self) -> Option<Vec<victory_shader::NativeProgram>> {
        use victory_shader::{Container, FourCC};
        let c = Container::parse(&self.container).ok()?;
        let part = c.part(FourCC::DXIL).or(c.part(FourCC::ACCC))?;
        victory_shader::native_programs(part.data).ok()
    }
}

/// `VICTORY_EXPORT_CLAMP=1`: pixel shaders of pipelines with no 32-bit float
/// target get [`Shader::translated_clamped`] (float exports held at ±65504).
#[cfg(windows)]
pub(crate) fn export_clamp_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_EXPORT_CLAMP"))
}

#[cfg(windows)]
impl Shader {
    /// The translation with float exports to colour targets held at
    /// ±65504 ([`victory_shader::emit::with_export_clamp`]), made on first
    /// use; its resources and root are those of [`Shader::translated`].
    pub(crate) fn translated_clamped(&self) -> Option<&Translated> {
        self.clamped
            .get_or_init(|| match translate_as(&self.container, true) {
                Ok(t) => Some(t),
                Err(e) => {
                    tracing::warn!(stage = self.stage, hash = %self.hash, error = %e, "clamped translation failed; the plain one is used");
                    None
                }
            })
            .as_ref()
    }
}

#[cfg(windows)]
fn translate(container: &[u8]) -> Result<Translated, String> {
    translate_as(container, false)
}

#[cfg(windows)]
fn translate_as(container: &[u8], clamped: bool) -> Result<Translated, String> {
    use victory_shader::{Container, FourCC, HwStage};
    // The translation depends on it (a view index past a capped heap).
    crate::descriptor::host::probe_view_limit();
    let c = Container::parse(container).map_err(|e| e.to_string())?;
    let programs = match c.part(FourCC::DXIL).or(c.part(FourCC::ACCC)) {
        Some(part) => victory_shader::native_programs(part.data).map_err(|e| e.to_string())?,
        None => Vec::new(),
    };
    let host = if clamped {
        crate::shader_cache::host_shader_clamped(container)?
    } else {
        crate::shader_cache::host_shader(container)?
    };
    let stores = programs.is_empty() || programs.iter().any(stores);
    let threads = programs.iter().find_map(|p| match &p.io {
        victory_shader::native::StageIo::Cs(cs) if cs.threads.iter().all(|&n| n > 0) => Some(cs.threads),
        _ => None,
    });
    let stores_by_address = programs.is_empty() || programs.iter().any(stores_by_address);
    let used = programs
        .into_iter()
        .find(|p| {
            matches!(
                p.stage,
                HwStage::Vs | HwStage::Ps | HwStage::Ms | HwStage::Cs
            )
        })
        .map(|p| p.used)
        .unwrap_or_default();
    let tables = crate::draw::TablePlan::new(&used, &host.root_indexed_views);
    Ok(Translated {
        host,
        threads,
        used,
        tables,
        stores,
        stores_by_address,
        picks: Default::default(),
    })
}

/// The compute pipelines of the console's ray-tracing structure builder
/// (`XboxBvhBuilder.dll`; its programs are the ones with a spilled
/// user-data table) run **all or none**. Its stages feed each other
/// through memory: a stage that runs on what a dropped stage never wrote
/// computes its store offsets from garbage. The structures lie in plain
/// title memory, between the title's own objects, and a root view has no
/// size, so those stores land on the neighbours (run 98, with 14 of 35
/// stages translated: the title died within a second of the first builder
/// submission, on pointers that had become bounding-box bit patterns).
///
/// So a builder dispatch runs only while every builder pipeline the title
/// has created has a host pipeline; the first builder dispatch makes them
/// all on the spot instead of over the next frames (a stage "still being
/// made" would be a dropped stage). While one cannot be made the builder
/// is off; it is looked at again when the title creates another. A stage
/// that could not be bound stops the stages after it for the rest of that
/// frame and the next ([`builder_rests`]); it turned the builder off for the
/// rest of the run at first.
///
/// shortcut: and only with `VICTORY_BVH_BUILDER=1`, until a run has shown
/// the whole builder and the host side of title memory by address
/// (`memory.rs`) working on a card; nothing reads the structures before
/// the programs that walk them translate.
#[cfg(windows)]
struct Builder {
    pipelines: Vec<Arc<make::Pending>>,
    /// Every one of `pipelines` has a host pipeline.
    whole: bool,
    /// One could not be made, of so many (0: none).
    off: usize,
    /// No stage runs before this frame.
    rests: u64,
}

#[cfg(windows)]
static BUILDER: Mutex<Builder> = Mutex::new(Builder {
    pipelines: Vec::new(),
    whole: false,
    off: 0,
    rests: 0,
});

/// Whether `shaders` are a pipeline of the structure builder.
#[cfg(windows)]
fn of_builder(shaders: &[Arc<Shader>]) -> bool {
    use victory_shader::{Container, FourCC};
    let [shader] = shaders else { return false };
    if shader.stage != "compute" {
        return false;
    }
    let Ok(c) = Container::parse(&shader.container) else {
        return false;
    };
    c.part(FourCC::DXIL)
        .and_then(|part| victory_shader::native_programs(part.data).ok())
        .is_some_and(|programs| programs.iter().any(victory_shader::spill::is_spilled))
}

/// `VICTORY_BVH_QUARANTINE=1`: what the structure builder's stages store
/// stays in the host's copies and is not copied back into title memory. A
/// check for a first run of the whole builder: a stage that stores where
/// it should not can then not reach the title's own objects (run 98), and
/// the programs that walk the structures read the host's copies anyway.
/// shortcut: not a way to run: the title also reads results back (sizes,
/// counters) and gets none, and a copy's pages are overwritten from title
/// memory when the title writes them.
#[cfg(windows)]
pub(crate) fn builder_quarantined(this: This) -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    is_builder(this) && *ON.get_or_init(|| std::env::var_os("VICTORY_BVH_QUARANTINE").is_some_and(|v| v == "1"))
}

/// Whether the title's pipeline `this` is one of the structure builder's.
#[cfg(windows)]
pub(crate) fn is_builder(this: This) -> bool {
    !this.is_null() && pipeline(this).builder
}

/// A stage of the builder was dropped after all: none runs for the rest of
/// this frame and the next (what it fed is not there).
#[cfg(windows)]
pub(crate) fn builder_rests(why: &'static str) {
    let mut b = BUILDER.lock().unwrap_or_else(|e| e.into_inner());
    b.rests = crate::frame::current() + 2;
    static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 20 {
        tracing::warn!(why, "the ray-tracing structure builder rests for this frame and the next: its stages run all or none");
    }
}

/// The structure builder's state for the panel's RAYS line (g6-vt2): `OFF`
/// (not asked for), `WAITING` (asked, its programs not all seen yet),
/// `STAYS OFF` (one of them has no host pipeline), `RESTING`, `ON`.
#[cfg(windows)]
pub(crate) fn builder_state() -> &'static str {
    if !std::env::var_os("VICTORY_BVH_BUILDER").is_some_and(|v| v == "1") {
        return "OFF";
    }
    let b = BUILDER.lock().unwrap_or_else(|e| e.into_inner());
    if b.whole && crate::frame::current() < b.rests {
        "RESTING"
    } else if b.whole {
        "ON"
    } else if b.off != 0 && b.off == b.pipelines.len() {
        "STAYS OFF"
    } else {
        "WAITING"
    }
}

/// At a dispatch of a builder pipeline: whether the builder runs (see
/// [`Builder`]).
#[cfg(windows)]
pub(crate) fn builder_runs() -> bool {
    static ALLOWED: OnceLock<bool> = OnceLock::new();
    if !*ALLOWED.get_or_init(|| std::env::var_os("VICTORY_BVH_BUILDER").is_some_and(|v| v == "1")) {
        return false;
    }
    let mut b = BUILDER.lock().unwrap_or_else(|e| e.into_inner());
    if b.off == b.pipelines.len() || crate::frame::current() < b.rests {
        return false;
    }
    if b.whole {
        return true;
    }
    let missing: Vec<String> = b.pipelines.iter().filter(|p| !make::now(p)).map(|p| make::hashes(p)).collect();
    if !missing.is_empty() {
        b.off = b.pipelines.len();
        tracing::warn!(
            missing = missing.len(),
            of = b.pipelines.len(),
            which = ?missing,
            "the ray-tracing structure builder stays off: not all of its programs have a host pipeline, and its stages run all or none"
        );
        return false;
    }
    b.off = 0;
    tracing::info!(
        pipelines = b.pipelines.len(),
        "the ray-tracing structure builder runs: all of its programs have host pipelines"
    );
    b.whole = true;
    true
}

/// Whether a console program can store to buffer memory: it has a buffer,
/// typed-buffer, scalar-memory, flat or global store or atomic, or code
/// that is not all known (an instruction of those kinds outside our table,
/// an indirect jump, code that does not decode). Stores to textures
/// (image instructions) and to the program's own scratch memory do not
/// count: neither reaches a buffer's host copy. After upstream #413,
/// which leaves scalar-memory stores out and counts image stores.
#[cfg(windows)]
fn stores(program: &victory_shader::NativeProgram) -> bool {
    use victory_shader::gfx10::{Encoding, decode};
    let Ok(code) = decode(&program.code) else {
        return true;
    };
    code.iter().any(|i| {
        let name = i.name();
        match i.encoding {
            Encoding::Mubuf
            | Encoding::Mtbuf
            | Encoding::Smem
            | Encoding::Flat
            | Encoding::Global => name.is_none_or(|n| {
                (n.contains("store") || n.contains("atomic")) && !n.contains("scratch")
            }),
            Encoding::Sop1 => name.is_none_or(|n| matches!(n, "s_setpc_b64" | "s_swappc_b64")),
            _ => false,
        }
    })
}

/// Whether a console program can store to memory through raw addresses
/// (`crate::memory`): a flat or global store or atomic, or code that does
/// not decode. The programs that only walk ray-tracing structures do not,
/// whatever they store through views.
#[cfg(windows)]
fn stores_by_address(program: &victory_shader::NativeProgram) -> bool {
    use victory_shader::gfx10::{Encoding, decode};
    let Ok(code) = decode(&program.code) else {
        return true;
    };
    code.iter().any(|i| {
        matches!(i.encoding, Encoding::Flat | Encoding::Global)
            && i.name()
                .is_none_or(|n| (n.contains("store") || n.contains("atomic")) && !n.contains("scratch"))
    })
}

#[repr(C)]
struct Pipeline {
    object: Object,
    packet: [u32; 8],
    /// Each used stage's container.
    shaders: Vec<Arc<Shader>>,
    /// Whether a list has bound it yet (its shaders are logged then).
    bound: std::sync::atomic::AtomicBool,
    /// What its host pipeline is made from, and the host pipeline once made
    /// (by a worker: the title may release the pipeline meanwhile).
    #[cfg(windows)]
    host: Arc<make::Pending>,
    /// One of the ray-tracing structure builder's ([`Builder`]).
    #[cfg(windows)]
    builder: bool,
}
const _: () = assert!(offset_of!(Pipeline, packet) == 0x10);

/// Stage names, in description order.
const GRAPHICS_STAGES: [&str; 5] = ["vertex", "pixel", "domain", "hull", "geometry"];

fn pipeline<'a>(this: This) -> &'a Pipeline {
    // SAFETY: the title passes one of our pipelines.
    unsafe { &*this.cast::<Pipeline>() }
}

/// List method: logs each pipeline's shaders the first time a list binds
/// it, so a run lists the shaders its frames need first.
pub(crate) fn bound(this: This) {
    if this.is_null() {
        return;
    }
    let p = pipeline(this);
    if p.bound.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let frame = crate::frame::current();
    for shader in &p.shaders {
        tracing::info!(pipeline = ?this, frame, stage = shader.stage, size = shader.container.len(), hash = %shader.hash, "first bind");
    }
}

/// The host pipeline of a title pipeline; `None` if one of its shaders does
/// not translate, the host refuses it, or it is still being made (its first
/// use starts that, see [`make`]).
#[cfg(windows)]
pub(crate) fn host_pipeline<'a>(this: This) -> Option<&'a host::HostPipeline> {
    if this.is_null() {
        return None;
    }
    make::ready(&pipeline(this).host)
}

/// As [`host_pipeline`], for draws of the console's rectangles
/// ([`make::ready_rects`]).
#[cfg(windows)]
pub(crate) fn host_pipeline_for<'a>(this: This, rects: bool) -> Option<&'a host::HostPipeline> {
    if this.is_null() {
        return None;
    }
    if rects {
        make::ready_rects(&pipeline(this).host)
    } else {
        make::ready(&pipeline(this).host)
    }
}

/// Host pipelines queued and not made yet.
#[cfg(windows)]
pub(crate) fn making() -> u64 {
    make::waiting()
}

/// Draws and dispatches dropped since the last [`take_drops`], by reason
/// (diagnostics).
#[cfg(all(windows, feature = "diagnostics"))]
static DROPS: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

/// Shader hashes as printed in the pipeline diagnostics.
#[cfg(windows)]
pub(super) fn parse_skip_hashes(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|hash| !hash.is_empty())
        .filter_map(|hash| {
            if hash.len() == 16 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                Some(hash.to_ascii_lowercase())
            } else {
                tracing::warn!(
                    hash,
                    "VICTORY_SKIP_PIPELINES expects 16-digit shader hashes"
                );
                None
            }
        })
        .collect()
}

/// The pixel shaders of the title's wet weathering passes
/// (`PS_TiledWeathering*_WET*`, both builds of each): left out unless
/// `VICTORY_WET_WEATHERING=1`. The tiles they shade are marked wet indoors
/// and dry, skin included: faces and arms showed sharp glossy streaks and
/// sparkles under neon and police lights, and dry walls blue-white smears
/// (runs glow-hold2-* and shots-b-*); without them (run no-wet-*) skin and
/// walls are matte. shortcut: what marks the tiles wet is not found yet;
/// rain and puddles are dry meanwhile. (From xzevistx/VIctory g6-dlss.)
#[cfg(windows)]
const WET_WEATHERING: &str = concat!(
    "bd768869e6a9bbb9,", "6c21bcbcc3416dce,", "9507e549b138f78a,", "1f8ed9edbc018811,", "75165dea28997f64,", "979be1a39ef9a780,",
    "1d19dead161179e0,", "45d9fb41a3f091ad,", "f5631d02141ccf4a,", "32dafebdbfabac96,", "2c42f1061b991556,", "b9e4d3aab6a1b39c,",
    "268d3bd5a97bc55a,", "5d31abc11729b4b8,", "4c3f209401b1c561,", "b00e9c5ddc9b19f6,", "79ee054eed4ef0f2,", "dd44029cb5188179,",
    "a03a5b004229d736,", "7e790dc9978e728b,", "b7eb97debe7f232f,", "51b6604981878b6e,", "edef484af6e06ecd,", "276095b0008866b5,",
    "37849edc52ccf0e0,", "09872d838262a1b2,", "cd494288f1a8e1c0,", "635fb84587970774,", "0fc6cbeb2f7104f6,", "424d7d00f2d6f528,",
    "7c7b0727a2a9363b,", "2896307ec3d5a1fd,", "c63d5562b559c111,", "4ef7f52b61ea7cb6,", "b8ae4f188396efe2,", "afee430d360e8b8c,",
    "9e34f46bca161e5f,", "33076f25f1436137,", "78ec1a6cca5c6b60,", "6663f768dce77155,", "3ff205fd9ea10c68,", "c4c70b24fb9a9e6f,",
    "9ada5cf01caffc72,", "5ed78af77dc3f3e2,", "fa3534fe1e406e51,", "766d52cd7d614053,", "17b5286d402a7c0c,", "94f4fdd63217f833,",
    "6e3dd0a722db7fbd,", "91980dc12cb0969a,", "40deac5fc988f351,", "1870bb9633c30fc4,", "3fc3a49cf5294684,", "18fa17c244b1043b,",
    "b77c96c8aa1687eb,", "c7b9d33d04ce489f,", "b986f4edd4ed5e53,", "32b625e06f106d8c,", "94268e8eff4dc1a2,", "6cab7f93bd2f57b1,",
    "7b5e713a559fc728,", "220fcfc4973cd801,", "7af9b426673ce896,", "cb8af919d9e2512c,",
);

#[cfg(windows)]
pub(super) fn skip_hashes() -> &'static [String] {
    static HASHES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    HASHES.get_or_init(|| {
        let mut value = std::env::var("VICTORY_SKIP_PIPELINES").unwrap_or_default();
        if std::env::var_os("VICTORY_WET_WEATHERING").is_none_or(|v| v != "1") {
            value.push(',');
            value.push_str(WET_WEATHERING);
        }
        parse_skip_hashes(&value)
    })
}

#[cfg(windows)]
pub(super) fn skipped_in_list<'a>(
    shaders: &'a [std::sync::Arc<Shader>],
    hashes: &[String],
) -> Option<&'a str> {
    if hashes.is_empty() {
        return None;
    }
    shaders
        .iter()
        .find(|shader| hashes.contains(&shader.hash))
        .map(|shader| shader.hash.as_str())
}

#[cfg(all(windows, feature = "diagnostics"))]
fn skipped_shader(p: &Pipeline) -> Option<&str> {
    skipped_in_list(&p.shaders, skip_hashes())
}

#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn is_skipped(this: This) -> bool {
    !this.is_null() && skipped_shader(pipeline(this)).is_some()
}

/// Counts a draw or dispatch of `this` intentionally skipped or dropped
/// for want of a host pipeline (diagnostics).
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn note_drop(this: This) {
    let why = if this.is_null() {
        Some("no pipeline set".to_owned())
    } else {
        let p = pipeline(this);
        skipped_shader(p)
            .map(|hash| format!("skipped shader {hash}"))
            .or_else(|| make::missing(&p.host))
    };
    if let Some(why) = why {
        note_why(why);
    }
}

#[cfg(all(windows, feature = "diagnostics"))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum PrepareFailure {
    #[default]
    Unclassified,
    SkippedPipeline,
    MissingHostPipeline,
    PipelineKind,
    IndexBuffer,
    DynamicDescriptorTable,
    ShaderMemoryViews,
    RootUavBuffer,
    RootUavHeap,
    RootUavSlot,
    RootSrvHeap,
    RootSrvSlot,
}

/// A failed direct draw or mesh preparation in a frame operation capture.
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn direct_prepare_text(this: This, command: &str, why: PrepareFailure) -> String {
    if this.is_null() {
        return format!("DROPPED {command} preparation reason={why:?} recipe_key=none");
    }
    let pipeline = pipeline(this);
    let key = pipeline.host.key();
    let shaders: Vec<_> = pipeline.shaders.iter().map(|s| (s.stage, s.hash.as_str())).collect();
    format!("DROPPED {command} preparation reason={why:?} recipe_key={key} guest_shaders={shaders:?}")
}

/// Keep the first example of each failure per recipe, with room for every
/// kind even when one compute pipeline repeatedly fails during world load.
#[cfg(all(windows, feature = "diagnostics"))]
fn new_indirect_example(
    seen: &mut Vec<(u32, String, PrepareFailure)>,
    kind: u32,
    key: &str,
    why: PrepareFailure,
) -> bool {
    const PER_KIND: usize = 8;
    if seen.iter().any(|(k, p, r)| *k == kind && p == key && *r == why)
        || seen.iter().filter(|(k, _, _)| *k == kind).count() >= PER_KIND
    {
        return false;
    }
    seen.push((kind, key.to_owned(), why));
    true
}

#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn note_indirect_prepare(
    this: This,
    why: PrepareFailure,
    (kind, stride, max): (u32, u32, u32),
    (args, address): (&crate::shadow::Shadow, u64),
    counter: Option<&(Arc<crate::shadow::Shadow>, u64)>,
) {
    static ON: OnceLock<bool> = OnceLock::new();
    if !*ON.get_or_init(|| crate::switch::on("VICTORY_INDIRECT_PREPARE_LOG")) {
        return;
    }
    static SEEN: Mutex<Vec<(u32, String, PrepareFailure)>> = Mutex::new(Vec::new());
    let pipeline = pipeline(this);
    let key = pipeline.host.key();
    if !new_indirect_example(&mut SEEN.lock().unwrap_or_else(|e| e.into_inner()), kind, key, why) {
        return;
    }
    let shaders: Vec<_> = pipeline.shaders.iter().map(|s| (s.stage, s.hash.as_str())).collect();
    let command = match kind {
        0 => "draw",
        1 => "draw indexed",
        2 => "dispatch",
        10 => "dispatch mesh",
        _ => "unknown",
    };
    tracing::info!(
        frame = crate::frame::current(),
        kind, command, stride, max, reason = ?why, pipeline = ?this, recipe_key = key, guest_shaders = ?shaders,
        arguments = format_args!("{address:#x}"), argument_buffer = args.id(),
        argument_offset = args.offset(address),
        count_address = counter.map(|(_, at)| *at),
        count_buffer = counter.map(|(s, _)| s.id()),
        count_offset = counter.map(|(s, at)| s.offset(*at)),
        "indirect command preparation failed"
    );
}

/// Counts a draw or dispatch dropped for `why` (diagnostics).
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn note_why(why: String) {
    crate::capture::op(0, || format!("DROPPED {why}"));
    // The panel's DROPS line (g6-vt2): ray tracing left out, skipped on
    // purpose, the rest (mostly pipelines still being made).
    let kind = if why.contains("ray-tracing") || why.contains("BVH") {
        crate::fpsbox::Note::DropRays
    } else if why.contains("skipped") {
        crate::fpsbox::Note::DropSkipped
    } else {
        crate::fpsbox::Note::DropOther
    };
    crate::fpsbox::note(kind, 1);
    let mut drops = DROPS.lock().unwrap_or_else(|e| e.into_inner());
    *drops.get_or_insert_with(HashMap::new).entry(why).or_default() += 1;
}

/// The drops counted since the last call, most first (diagnostics).
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn take_drops() -> Vec<(String, u64)> {
    let drops = DROPS.lock().unwrap_or_else(|e| e.into_inner()).take();
    let mut drops: Vec<(String, u64)> = drops.into_iter().flatten().collect();
    // Frame reports only show the first 40 reasons. Keep ties ordered so
    // a HashMap iteration order change cannot change which reasons appear.
    drops.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    drops
}

/// The stage's container, if it has one.
fn copy(stage: &'static str, code: &Bytecode) -> Option<Arc<Shader>> {
    if code.code.is_null() || code.size == 0 {
        return None;
    }
    // SAFETY: the title passes `size` bytes of shader code.
    Some(shader(stage, unsafe {
        std::slice::from_raw_parts(code.code, code.size)
    }))
}

/// Finds the title's shader containers by their stage hash (the 16 bytes of
/// a container's `HASH` part after its flags): for each hash asked, the
/// container, or `None`. Set by the runner, which reads the title's shader
/// archive (the archive stays packed; containers are unpacked as asked).
/// Ported from the "cuda-nvidia card implant" patches (precompiled
/// pipelines, `VICTORY_PRECOMPILED=1`).
pub type ContainerFinder = dyn Fn(&[[u8; 16]]) -> Vec<Option<Vec<u8>>> + Send + Sync;
static FINDER: OnceLock<Box<ContainerFinder>> = OnceLock::new();

/// See [`ContainerFinder`].
pub fn set_container_finder(finder: Box<ContainerFinder>) {
    let _ = FINDER.set(finder);
}

/// The stage a container's native program runs as, for [`shader`].
fn stage_of(container: &[u8]) -> Option<&'static str> {
    use victory_shader::{Container, FourCC, HwStage, native_programs};
    let c = Container::parse(container).ok()?;
    let programs = native_programs(c.part(FourCC::DXIL)?.data).ok()?;
    programs.iter().find_map(|p| match p.stage {
        HwStage::Vs => Some("vertex"),
        HwStage::Ps => Some("pixel"),
        HwStage::Ms => Some("mesh"),
        HwStage::Cs => Some("compute"),
        _ => None,
    })
}

/// A pipeline the console's driver had compiled ahead (device methods
/// 0x1c8 and 0x218): made from the title's own containers of the stages it
/// names by hash, as if the title had passed those. `None` when a stage is
/// not found (the caller then refuses the call, as before).
pub(crate) fn create_precompiled(
    hashes: &[[u8; 16]],
    blocks: Option<(&[u8], &[u8])>,
    iid: *const Iid,
    out: *mut This,
) -> Option<HRESULT> {
    // Diagnostics: why one is not made (the first 32), for the next layout
    // to decode.
    #[cfg(feature = "diagnostics")]
    let why = |what: &str| {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 32 {
            let hex: Vec<String> = hashes.iter().map(|h| h.iter().map(|b| format!("{b:02x}")).collect()).collect();
            tracing::info!(state = blocks.map(|b| b.0.len()), "precompiled pipeline not made: {what} {hex:?}");
        }
    };
    #[cfg(not(feature = "diagnostics"))]
    let why = |_: &str| {};
    let finder = FINDER.get()?;
    let found = finder(hashes);
    let mut stages: Vec<(&'static str, Vec<u8>)> = Vec::new();
    for container in found {
        let Some(container) = container else {
            why("a stage's container is not in the archive");
            return None;
        };
        let Some(stage) = stage_of(&container) else {
            why("a stage's kind is not known");
            return None;
        };
        stages.push((stage, container));
    }
    // A compute stage beside a mesh stage is its amplification stage.
    if stages.iter().any(|s| s.0 == "mesh") {
        for s in &mut stages {
            if s.0 == "compute" {
                s.0 = "amplification";
            }
        }
    }
    const ORDER: [&str; 5] = ["amplification", "mesh", "vertex", "pixel", "compute"];
    stages.sort_by_key(|s| ORDER.iter().position(|o| *o == s.0));
    let compute = stages.iter().all(|s| s.0 == "compute");
    // A draw pipeline needs its vertex or mesh stage and its state.
    #[cfg(windows)]
    let graphics = if compute {
        None
    } else {
        if !stages.iter().any(|s| s.0 == "vertex" || s.0 == "mesh") {
            why("no vertex or mesh stage");
            return None;
        }
        let Some((state, layout)) = blocks else {
            why("no state block of a known size");
            return None;
        };
        let Some(g) = host::GraphicsState::from_console(state, layout) else {
            why("from_console refused the state");
            return None;
        };
        #[cfg(feature = "diagnostics")]
        if std::env::var_os("VICTORY_PRECOMPILED_LOG").is_some_and(|v| v == "1") {
            let hex = |b: &[u8]| {
                b.chunks(4)
                    .map(|w| format!("{:08x}", u32::from_le_bytes(w.try_into().unwrap_or_default())))
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            tracing::info!(
                stages = ?stages.iter().map(|s| (s.0, blake3::hash(&s.1).to_hex()[..16].to_owned())).collect::<Vec<_>>(),
                state_block = hex(state),
                layout_block = hex(layout),
                "precompiled pipeline state: {}",
                g.describe()
            );
        }
        Some(Box::new(g))
    };
    #[cfg(not(windows))]
    let _ = (compute, blocks);
    let shaders = stages.iter().map(|(stage, bytes)| shader(stage, bytes)).collect();
    Some(hand_out(
        shaders,
        #[cfg(windows)]
        graphics,
        iid,
        out,
    ))
}

fn hand_out(
    shaders: Vec<Arc<Shader>>,
    #[cfg(windows)] graphics: Option<Box<host::GraphicsState>>,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    tracing::trace!(?shaders, "pipeline state");
    #[cfg(windows)]
    let host = Arc::new(make::Pending::new(shaders.clone(), graphics));
    #[cfg(windows)]
    known::note(host.key(), &shaders, host.graphics.as_deref());
    #[cfg(windows)]
    make::early(&host);
    #[cfg(windows)]
    let builder = of_builder(&shaders);
    #[cfg(windows)]
    if builder {
        // Another stage: the group is looked at again at its next dispatch.
        let mut b = BUILDER.lock().unwrap_or_else(|e| e.into_inner());
        b.pipelines.push(host.clone());
        b.whole = false;
    }
    let pipeline = Box::new(Pipeline {
        object: Object::new(&FUNCTIONS),
        packet: [0; 8],
        #[cfg(windows)]
        host,
        #[cfg(windows)]
        builder,
        shaders,
        bound: std::sync::atomic::AtomicBool::new(false),
    });
    object::hand_out(PIPELINE, pipeline, iid, &PIPELINE_IIDS, out)
}

/// Device method.
pub(crate) extern "system" fn create_graphics(
    _: This,
    desc: *const GraphicsHead,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    let stages = GRAPHICS_STAGES.iter().zip(&desc.stages);
    let shaders = stages.filter_map(|(s, c)| copy(s, c)).collect();
    hand_out(
        shaders,
        #[cfg(windows)]
        None,
        iid,
        out,
    )
}

/// Device method.
pub(crate) extern "system" fn create_compute(
    _: This,
    desc: *const ComputeHead,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    hand_out(
        copy("compute", &desc.stage).into_iter().collect(),
        #[cfg(windows)]
        None,
        iid,
        out,
    )
}

/// Device method 0x1f0: the plain compute description plus console
/// extensions `(desc, extension count, extensions, iid, out)`.
/// shortcut: the extensions (console shader settings) are ignored, as is
/// the rest of the description.
pub(crate) extern "system" fn create_compute_x(
    this: This,
    desc: *const ComputeHead,
    _extensions: u32,
    _extended: *const u8,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    create_compute(this, desc, iid, out)
}

/// A pipeline described as a stream: its size and the subobjects, each a
/// type then its data, padded to 8 bytes.
#[repr(C)]
pub(crate) struct StreamDesc {
    size: usize,
    stream: *const u8,
}

/// A subobject type's size (type included), and the stage it is if it is a
/// shader. The console numbers amplification and mesh shaders 23 and 24
/// (PC: 24 and 25); its own types from 25 on are not sized yet.
fn subobject(kind: u32) -> Option<(usize, Option<&'static str>)> {
    let stage = |name| Some((24, Some(name)));
    match kind {
        1 => stage("vertex"),
        2 => stage("pixel"),
        3 => stage("domain"),
        4 => stage("hull"),
        5 => stage("geometry"),
        6 => stage("compute"),
        23 => stage("amplification"),
        24 => stage("mesh"),
        0 => Some((16, None)),
        12 | 19 => Some((24, None)),
        7 | 15 => Some((40, None)),
        8 => Some((336, None)),
        10 => Some((48, None)),
        11 => Some((56, None)),
        17 => Some((16, None)),
        21 => Some((64, None)),
        22 => Some((32, None)),
        9 | 13 | 14 | 16 | 18 | 20 => Some((8, None)),
        _ => None,
    }
}

/// Device method: a pipeline from a stream; the shaders are kept, and the
/// graphics state for the host pipeline.
pub(crate) extern "system" fn create_stream(
    _: This,
    desc: *const StreamDesc,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    if desc.stream.is_null() {
        return E_INVALIDARG;
    }
    let mut shaders = Vec::new();
    #[cfg(windows)]
    let mut graphics = host::GraphicsState::default();
    let mut at = 0;
    while at + 8 <= desc.size {
        // SAFETY: inside the stream the title passes.
        let kind = unsafe { desc.stream.add(at).cast::<u32>().read_unaligned() };
        let Some((size, stage)) = subobject(kind) else {
            tracing::warn!(
                kind,
                at,
                "pipeline subobject of unknown size; the rest is ignored"
            );
            break;
        };
        if let Some(stage) = stage {
            // SAFETY: a shader subobject has its bytecode at +8.
            let code = unsafe { &*desc.stream.add(at + 8).cast::<Bytecode>() };
            shaders.extend(copy(stage, code));
        }
        #[cfg(windows)]
        // SAFETY: a whole subobject of this kind lies at `at`.
        unsafe {
            graphics.read(kind, desc.stream.add(at))
        };
        at += size;
    }
    hand_out(
        shaders,
        #[cfg(windows)]
        Some(Box::new(graphics)),
        iid,
        out,
    )
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    let found = object::is(iid, &PIPELINE_IIDS).then_some(this);
    object::answer(PIPELINE, iid, found, out)
}
extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Pipeline>(this)
}

/// IUnknown 3, object 4, device child 1, pipeline 3.
static FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<PIPELINE, 24>());
    f[0] = query as *const ();
    f[2] = release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    Table(f)
};

#[cfg(windows)]
pub(crate) mod host;
#[cfg(windows)]
pub mod known;
#[cfg(windows)]
mod library;
#[cfg(windows)]
mod make;

/// The shader translator's fingerprint (its sources, `build.rs`): the
/// start-up warm-up runs again when it changes.
pub fn translator_id() -> &'static str {
    env!("VICTORY_TRANSLATOR")
}

/// Host pipelines queued and not made yet (the start-up splash's count).
#[cfg(windows)]
pub fn being_made() -> u64 {
    make::waiting()
}

/// Lets the pipeline library's mapped file go from memory when no pipeline
/// has loaded for a while (`library::trim`); called by the host's sweep.
#[cfg(windows)]
pub(crate) fn trim_library() {
    library::trim();
}

/// The pipeline library's part of the report when the host device is
/// removed (`library::removal_note`).
#[cfg(windows)]
pub(crate) fn library_removal_note() -> String {
    library::removal_note()
}

/// Keeps compiled host pipelines, and the list of title pipelines to start
/// early, in `dir` (a folder of the cache) across runs.
#[cfg(windows)]
pub fn use_pipeline_cache(dir: std::path::PathBuf) {
    library::use_pipeline_cache(dir);
}

/// Saves the pipeline cache now if anything was added (at the end of a run).
#[cfg(windows)]
pub fn save_pipeline_cache() {
    library::save(true);
    known::flush();
}

#[cfg(test)]
mod tests {
    #[cfg(all(windows, feature = "diagnostics"))]
    #[test]
    fn direct_prepare_capture_identifies_an_unset_pipeline() {
        let line = super::direct_prepare_text(
            std::ptr::null_mut(),
            "mesh",
            super::PrepareFailure::MissingHostPipeline,
        );
        assert_eq!(
            line,
            "DROPPED mesh preparation reason=MissingHostPipeline recipe_key=none"
        );
    }

    #[cfg(all(windows, feature = "diagnostics"))]
    #[test]
    fn indirect_prepare_examples_preserve_each_command_kind() {
        use PrepareFailure::{DynamicDescriptorTable, IndexBuffer};

        let mut seen = Vec::new();
        assert!(new_indirect_example(&mut seen, 2, "first", DynamicDescriptorTable));
        assert!(!new_indirect_example(&mut seen, 2, "first", DynamicDescriptorTable));
        for key in 1..8 {
            assert!(new_indirect_example(&mut seen, 2, &key.to_string(), DynamicDescriptorTable));
        }
        assert!(!new_indirect_example(&mut seen, 2, "later", DynamicDescriptorTable));
        assert!(new_indirect_example(&mut seen, 10, "first", DynamicDescriptorTable));
        assert!(new_indirect_example(&mut seen, 1, "other", DynamicDescriptorTable));
        assert!(new_indirect_example(&mut seen, 1, "other", IndexBuffer));
    }

    #[cfg(all(windows, feature = "diagnostics"))]
    #[test]
    fn skip_hashes_accepts_full_hashes_only() {
        assert!(super::parse_skip_hashes(" , ").is_empty());
        assert_eq!(
            super::parse_skip_hashes(" 0123456789ABCDEF, 0123456789abcde, xyz, 0011223344556677,"),
            ["0123456789abcdef", "0011223344556677"]
        );
    }

    use super::*;
    use crate::object::{S_OK, slot};

    fn shaders(p: This) -> Vec<(&'static str, Vec<u8>)> {
        pipeline(p)
            .shaders
            .iter()
            .map(|s| (s.stage, s.container.clone()))
            .collect()
    }

    #[test]
    fn pipelines_keep_their_shaders() {
        let (vs, ps) = ([1u8, 2, 3], [4u8; 5]);
        let none = || Bytecode {
            code: std::ptr::null(),
            size: 0,
        };
        let desc = GraphicsHead {
            root: std::ptr::null(),
            stages: [
                Bytecode {
                    code: vs.as_ptr(),
                    size: vs.len(),
                },
                Bytecode {
                    code: ps.as_ptr(),
                    size: ps.len(),
                },
                none(),
                none(),
                none(),
            ],
        };
        let mut p = std::ptr::null();
        assert_eq!(
            create_graphics(std::ptr::null(), &desc, &PIPELINE_IIDS[0], &mut p),
            S_OK
        );
        assert_eq!(
            shaders(p),
            [("vertex", vs.to_vec()), ("pixel", ps.to_vec())]
        );
        // A second pipeline with the same pixel shader shares its container.
        let mut q = std::ptr::null();
        create_graphics(std::ptr::null(), &desc, &PIPELINE_IIDS[0], &mut q);
        assert!(Arc::ptr_eq(
            &pipeline(p).shaders[1],
            &pipeline(q).shaders[1]
        ));
        assert_eq!(slot::<extern "system" fn(This) -> u32>(p, 0x10)(p), 0);
    }

    /// The stream layout the title builds (title+0x86f090): root
    /// signature, five shader slots (the first a mesh shader), blend, sample
    /// mask, rasterizer, depth-stencil 1, sample description, input layout,
    /// topology, render target and depth formats: 0x2b0 bytes.
    #[test]
    fn streams_yield_their_shaders() {
        let (ms, ps) = ([9u8; 7], [4u8; 5]);
        let mut stream = vec![0u8; 0x2b0];
        let mut put = |at: usize, v: u64| stream[at..at + 8].copy_from_slice(&v.to_le_bytes());
        for (at, kind) in [
            (0x10, 24),
            (0x28, 2),
            (0x40, 3),
            (0x58, 4),
            (0x70, 5),
            (0x88, 8),
            (0x1d8, 9),
            (0x1e0, 10),
            (0x210, 21),
            (0x250, 17),
            (0x260, 12),
            (0x278, 14),
            (0x280, 15),
            (0x2a8, 16),
        ] {
            put(at, kind);
        }
        put(0x18, ms.as_ptr() as u64);
        put(0x20, ms.len() as u64);
        put(0x30, ps.as_ptr() as u64);
        put(0x38, ps.len() as u64);
        // Topology type 3 (triangles), one render target of format 28.
        put(0x278, 14 | 3 << 32);
        put(0x280, 15 | 28 << 32);
        put(0x2a0, 1 << 32);
        let desc = StreamDesc {
            size: stream.len(),
            stream: stream.as_ptr(),
        };
        let mut p = std::ptr::null();
        let hr = create_stream(std::ptr::null(), &desc, &PIPELINE_IIDS[0], &mut p);
        assert_eq!(hr, S_OK);
        assert_eq!(shaders(p), [("mesh", ms.to_vec()), ("pixel", ps.to_vec())]);
        #[cfg(windows)]
        {
            let g = pipeline(p).host.graphics.as_deref().unwrap();
            assert_eq!(g.topology.0, 3);
            assert_eq!(
                (g.targets.NumRenderTargets, g.targets.RTFormats[0].0),
                (1, 28)
            );
        }
    }
}
