# G6 fork of VIctory (branch `g6-vt3`)

This branch runs **G6** (the Xbox Series X build of the open-world title) through VIctory on Windows PCs with NVIDIA cards. It is upstream [TaynanGT/Vice-Tay](https://github.com/TaynanGT/Vice-Tay) `main` at `aa917f3` merged into the G6 work described here (`g6-vt2` is the older branch, upstream `e939566`). The goal is a **stable 30 FPS with 30 FPS 1% lows**. On an RTX 4070 SUPER / Ryzen 9 5950X today:

| Setup | FPS | 1% low |
|---|---|---|
| **Series S profile + DLSS quality (recommended)** | **17–18** (20–26 in later cutscenes) | ~3–3.7 |
| Series X, no DLSS | ~13 | ~2 |
| Series X + DLSS 4K performance / ultra-performance | 12.4 / 13.7 | 2.3 / 1.3 |

Frames are held up by the CPU (copies of the title's memory to and from the card each frame), not the GPU. The Series S profile makes the title itself do less each frame, which is why it wins. Everything is GPL-3.0, like VIctory. Every ported piece names its origin in a code comment.

You need your own legal dump of the title. No game files are in this repository.

## Quick start for new users (Series S + DLSS)

1. Get `victory.exe`: the [release zip](https://github.com/xzevistx/VIctory/releases) (unpack it keeping its folders), or build it (below).
2. Put NVIDIA's `nvngx_dlss.dll` in your game folder (from the NVIDIA DLSS SDK, [github.com/NVIDIA/DLSS](https://github.com/NVIDIA/DLSS), `lib\Windows_x86_64\rel`). Without it the driver's copy is used, which some drivers do not start. With the NVIDIA App's DLSS override on, the App's copy runs either way.
3. Double-click `scripts\g6\play-g6-series-s-dlss.bat` and give it your game folder, or run:
   ```
   scripts\g6\play-g6-series-s-dlss.bat "D:\Games\G6" quality
   ```
   Presets: `quality` (1280x720 upscaled to 1920x1080, default), `balanced`, `performance`, `ultra-performance`.
4. F10 toggles the panel. Its line `DLSS QUALITY ON 24/S` says DLSS upscaled 24 frames in the last second, and `DLSS 1280X720 TO 1920X1080` gives the sizes.

## Use the exact build

1. Build the diagnostics build. This is the build every number here comes from; it has the in-game panel, F8 pictures and logs:
   ```
   cargo build --profile diag -p victory --features diagnostics
   ```
   The exe is `target\diag\victory.exe`. The panel's BUILD line shows the target folder and a hash of the exe, so two people can tell they run the same file.
2. Run it with the tested switch set:
   ```
   .\scripts\g6\run-g6.ps1 -GameDir 'D:\Games\G6'
   ```
   Add experiments with `-Extra 'NAME=value;NAME=value'`.
3. The first run builds thousands of shader pipelines (the panel's `SHADERS BUILDING`), and that stutters. Later runs reuse the cache in `%LOCALAPPDATA%\victory-g6\cache`. To pre-build everything:
   - run once with `-Extra 'VICTORY_PIPELINE_BOOK=1'` (it records every pipeline G6 makes);
   - then run `victory.exe warm` with the same switches and `VICTORY_CACHE_DIR`.
   - **Never** run `victory.exe shaders` into a cache that already has a driver pipeline library: every pipeline goes stale.

## The in-game panel (F10 toggles it)

| Line | Meaning |
|---|---|
| FPS / 1% LOW / FRAME | frame rate, the slowest 1% as FPS (last 1,000 frames, then last 10 s), mean frame time |
| WORST | slowest frame of the last second: the spike you feel |
| CPU / GPU | ms a frame preparing on the CPU / waiting for the card |
| GAME+WAIT | the rest of the frame: the title's own threads and waits |
| DROPS RAYS / SKIP / OTHER | draws a frame left out: ray tracing off, skipped on purpose, pipeline still being made |
| DRAWS, SHADERS BUILDING, NO SHADER | draws a frame, pipelines being made now, draws skipped for want of one |
| TEX UP | texture data uploaded a frame (MB, ms) |
| COPY / BACK | title buffers copied to the card / copied back from it, a frame (MB, ms) |
| VRAM a/b PC c | card memory in use / budget, and MB spilled into PC memory |
| STREAM | DirectStorage MB/s and requests/s |
| PAGEMAP | the title's GPU page-mapping copies a second and their cost |
| DLSS … ON n/S, DLSS a TO b | the host upscaler chosen, frames it upscaled a second (`ON IDLE` in menus and loading; `FELL BACK FSR` if DLSS failed), render and output size; `UPSCALER TITLE OWN` without one |
| RAYS | `OFF`, `OFF STAND-IN 0.8` (`VICTORY_AMBIENT`), or `BUILDER WAITING / ON / RESTING / STAYS OFF` (`VICTORY_BVH_BUILDER=1`) |
| NO PAGE n/S LEFT m/S | with the builder: pages its rays reached with no copy; LEFT ones read 0 (a black picture). Shown only when it happens |
| UPLOAD SLOTS free/all OUT n NO ROOM n | the memory reserved at start for list uploads and memory by address; NO ROOM above 0 is trouble |
| RAM FREE | the PC's free physical memory |
| BUILD | build folder and exe hash |
| N L B P | switches: wave native, lazy alias, buffers first, precompiled (1 on; the defaults show N1 L0 B0 P0) |

## What this branch adds over upstream (where to look)

- **Cutscene-prestream crash fix.** `crates/victory-gpu/src/upload.rs` `claim()` / `reserve()`, called from `host.rs` `host()`.
  - Cause: G6's `XboxBvhBuilder.dll` got null from the list's `allocate_upload` (vtable 0x4A0, `list.rs`) once [4 GiB, 2^37) was full of the title's memory, and its checked copy ended the game.
  - Fix: separate allocations of the sizes asked for, reserved at start. One big reservation cut in pieces broke textures; don't do that.
  - `VICTORY_UPLOAD_POOL_MB=0` turns it off.
- **High views** (`VICTORY_HIGH_VIEWS`, default on in code, **off in the tested set**): our whole-section views above 2^41 (`xmem.rs` `Section::window`, `inplace.rs` `high_view`).
- **Shader translator (version 8):**
  - tiled-lighting light lists read as integers (`emit/mod.rs` `tiled_lighting_uint_srvs`, from DexNom/VIctory);
  - the wave64 allocation idiom as plain 32-lane waves (`lower::wave_local`).
- **Pipelines:**
  - wet-weathering pipelines skipped (`pipeline.rs` `skip_hashes`, `VICTORY_WET_WEATHERING=1` keeps them);
  - blend fixes (`pipeline/host.rs`);
  - console rectangles (`pipeline/make.rs` `ready_rects`);
  - precompiled pipelines (`VICTORY_PRECOMPILED=1`);
  - a pipeline library per D3D12 runtime (`pipeline/library.rs`).
- **Lazy page mappings** (`VICTORY_LAZY_ALIAS=1`, from cool_fox): removes the stall of the title's page-mapping copies. It raised 1% lows from ~1 to ~4, but **it currently loses some walls and character geometry**, so it is off in the tested set. Fixing that is the biggest open win.
- **Capture and panel:** F8 pictures (`capture.rs`), the panel above (`fpsbox.rs`, fed by `host.rs` meter lines, `dstorage.rs`, `xmem.rs`), and save logging (`xgamesave.rs`).
- **DXR groundwork** (`dxr.rs`, `modules/bvh.rs`, `docs/fork-dxr.md`): inert unless `VICTORY_DXR=1`.

- **DLSS ultra-performance** for every host-upscaler profile (`VICTORY_DLSS`, `VICTORY_DLSS_4K`, `VICTORY_FSR_4K` accept `ultra-performance`: a third of the output per axis; `apps/victory/src/fsr_4k.rs`).
- **Room for the title's ray tracing** (experimental): with `VICTORY_BVH_BUILDER=1` 48 arenas of 64 MiB for memory by address are reserved at start instead of 4 (`VICTORY_MEMORY_ARENAS=<n>`, `upload.rs` `claim`). With 4 the builder ran out of room within seconds and the picture went black.

## Picture fixes on by default (since g6-vt2 after 0c0eec4)

Measured on the same scenes from one saved game, Series S + DLSS, rays off:

| Fault | Cause | Fix (default) | Turn it off |
|---|---|---|---|
| Black (and white) squares lying on the water: sea, pools, the shore | the translator's fallback for the water programs' 64-lane wave code (`VICTORY_WAVE_NATIVE=0`, set by the tested switch set since an old FPS suspicion) | `run-g6.ps1` sets `VICTORY_WAVE_NATIVE=1`: those programs run as plain 32-lane waves (`emit/mod.rs` `emit()`, `lower::wave_local`). Runs fix-t8 (blocks) against fix-t9 (clean), same shots, 18.1 vs 18.4 FPS. (`VICTORY_INDIRECT_WINDOW/BATCH` were first blamed; they are left out of `run-g6.ps1` too, no slower.) | `-Extra 'VICTORY_WAVE_NATIVE=0'` |
| Purple / magenta glow | with rays off, the screen probes' memory holds garbage that comes out magenta | the stand-in lighting is on unless `VICTORY_AMBIENT` is set: 0.8 of the estimate, air bounded at 2x, reflections held on the temporal reflection resolves (`ambient.rs` `G6_*`); magenta pixels 0.53% → 0.04–0.2% | `VICTORY_AMBIENT=0` |
| Old blocks of skin and clothes staying on characters | textures picked by index were checked 5 times (3 small samples) and then never again; G6 fills large textures later | checked every 400 frames for good, 8 samples spread over large textures (`draw.rs` `recheck_long`, `texture.rs` `fingerprint`) | `VICTORY_HEAP_RECHECK_LONG=0` |

Fixed by the upstream merge (`g6-vt3`):

| Fault | Cause | Fix (default) | Turn it off |
|---|---|---|---|
| Purple-red foliage with rays off | most likely upstream `fa8fc1d`: the ambient resolve now gets a depth view that matches its plane | merged | none |
| Every loading screen rebuilt ~418 pipelines (first minutes at 4.6 FPS) | upstream `fa8fc1d` keeps NVIDIA pipelines with static samplers out of the pipeline library (stale sampler indices after a restart were its worry) | `run-g6.ps1` sets upstream's own `VICTORY_LIBRARY_STATIC_SAMPLERS=1`: 0 rebuilt, 17.2–18.2 FPS, no filtering errors seen in the test shots | `-Extra 'VICTORY_LIBRARY_STATIC_SAMPLERS=0'` |

The stand-in's reflection hold now uses upstream's documented selectors (`CS_TemporalResolve2_Opaque,CS_TemporalResolve1_Alpha`, from TaynanGT/Vice-Tay `docs/fork-ray-tracing.md`); the guessed `CombinedReflections` never matched. `VICTORY_SKIP_NAMED=<text>[,<text>]` leaves out compute programs by name, for tests.

Still open: whiteout when the camera goes underwater and in the sunset rooftop chase (probe and air light with rays off); low-detail, untextured background characters in some runs (streaming arriving late; seen in one run of three on the same save); speckled light on shaded rock in a few shots.

## Ray tracing (why outdoor scenes glow white)

G6 traces its own rays: its `XboxBvhBuilder.dll` builds the structures and compute shaders walk them (see `docs/fork-ray-tracing.md`). VIctory leaves the builder out by default, so every ray misses. Outdoors that leaves the sky's light where there should be shadow (whiteout, glow), indoors nothing, and magenta probe memory. (The black blocks on water were not this: see the table above.) Options today:

- default: rays off with the stand-in (above);
- `VICTORY_AMBIENT=0`: rays off, the title's own output (magenta, glow);
- `VICTORY_BVH_BUILDER=1`: the title's own ray tracing. It works: a proper ray-traced picture at about 13 FPS once its ~1,000 shaders are compiled (two or three warm-up runs; black until then), against about 20 rays off.

## Known open problems (good places to help)

1. Lazy page mappings break geometry: `xmem.rs` `map_alias_lazy` / `lazy_source`, `host.rs` `read` / `write_mapped`.
2. A magenta stripe in the kitchen shot of the intro; its source pass is unknown. Capture intermediate render targets on that frame.
3. A few pipelines still build mid-run on a warm cache (panel `SHADERS BUILDING`; about 90 on a third run).
4. VRAM spill: ~8.6 GB in PC memory in the intro. Try `VICTORY_TEXTURE_BUDGET_MB`.
5. Per-frame copies: COPY + BACK about 50 ms a frame. `VICTORY_BACK_BY_COPY=1` lists copy-back per buffer; `VICTORY_BACK_SKIP=<addr>` skips one.
6. Two water compute programs have no 32-lane form: `6871d6e4b3ac9a10`, `a10179ab7eda0dc3`.

## Adding to this fork (people and their AI agents)

Give your agent this file first. Rules this branch follows:

- **Branch from `g6-vt3`** and open a pull request against it. One change per pull request, with:
  - the switch that turns it on or off;
  - before/after numbers from the same build otherwise, the same scene and the same switches;
  - a picture of the same shot before and after.
- **Every behaviour change gets a `VICTORY_*` switch**, documented in the doc comment of the function that reads it, with what it was measured to do. Default it on only when runs proved it.
- **Fixes live next to the code they fix**, not in a new "patches" module. Ports carry a credit comment: `From <owner>/<repo> <branch or commit>`.
- **Bump `TRANSLATOR_VERSION`** (`crates/victory-shader/src/lib.rs`) when a translation changes, and test with a fresh cache.
- **Don't run `cargo fmt`** over the tree (upstream's files are not formatted that way). Diagnostics-only code goes behind `#[cfg(feature = "diagnostics")]`.
- **Measure honestly:**
  - frame-time scores over a fixed time window compare different scenes if one build is faster, so match shots by picture;
  - saving pictures costs ~170 ms a frame, so leave F8 and frequent saves off while timing;
  - G6's own crash reports and minidumps land in `<data>\7952987A\temp`.
- **No game files, dumps, caches or paths with your name** in commits.

Upstream's own README follows unchanged in `README.md`.
