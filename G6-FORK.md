# G6 fork of VIctory (branch `g6-vt2`)

This branch runs **G6** (the Xbox Series X build of the open-world title) through VIctory on Windows PCs with NVIDIA cards. It is upstream [TaynanGT/Vice-Tay](https://github.com/TaynanGT/Vice-Tay) `main` at `e939566` plus the G6 work described here. The goal is a **stable 30 FPS with 30 FPS 1% lows**; today it runs at about 13 FPS (1% low ~2) on an RTX 4070 SUPER / Ryzen 9 5950X. Everything is GPL-3.0, like VIctory. Every ported piece names its origin in a code comment.

You need your own legal dump of the title. No game files are in this repository.

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
| FPS / 1% LOW / FRAME | frame rate, the slowest 1% as FPS, mean frame time |
| WORST | slowest frame of the last second: the spike you feel |
| CPU / GPU | ms a frame preparing on the CPU / waiting for the card |
| DRAWS, SHADERS BUILDING, NO SHADER | draws a frame, pipelines being made now, draws skipped for want of one |
| TEX UP | texture data uploaded a frame (MB, ms) |
| COPY / BACK | title buffers copied to the card / copied back from it, a frame (MB, ms) |
| VRAM a/b PC c | card memory in use / budget, and MB spilled into PC memory |
| STREAM | DirectStorage MB/s and requests/s |
| PAGEMAP | the title's GPU page-mapping copies a second and their cost |
| BUILD | build folder and exe hash |
| N L B P | switches: wave native, lazy alias, buffers first, precompiled (1 on) |

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

## Known open problems (good places to help)

1. Lazy page mappings break geometry: `xmem.rs` `map_alias_lazy` / `lazy_source`, `host.rs` `read` / `write_mapped`.
2. A magenta stripe in the kitchen shot of the intro; its source pass is unknown. Capture intermediate render targets on that frame.
3. Pipelines still build mid-run on a warm cache (panel `SHADERS BUILDING`).
4. VRAM spill: ~8.6 GB in PC memory in the intro. Try `VICTORY_TEXTURE_BUDGET_MB`.
5. Per-frame copies: COPY + BACK about 50 ms a frame. `VICTORY_BACK_BY_COPY=1` lists copy-back per buffer; `VICTORY_BACK_SKIP=<addr>` skips one.
6. Two water compute programs have no 32-lane form: `6871d6e4b3ac9a10`, `a10179ab7eda0dc3`.

## Adding to this fork (people and their AI agents)

Give your agent this file first. Rules this branch follows:

- **Branch from `g6-vt2`** and open a pull request against it. One change per pull request, with:
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
