# The host's own ray tracing (DXR) in place of the title's

Status, 2026-11-26: **step 1 of 5 done**: what the title gives its builder is
known, its packed triangles included. **Step 2 begun**: `VICTORY_DXR=1`
builds the host's own bottom levels beside the title's (not yet run on a
card). Nothing of the title's
picture or speed changes yet. `VICTORY_BVH_LOG=1` logs what the title asks of
its structure builder.

What this is for: the title's lighting and shadows from real rays, at the
speed of the card's ray-tracing hardware. Without it, rays are turned off
(`VICTORY_RAYS_MISS`, on while the title's own builder is off) and
`VICTORY_AMBIENT` puts an estimate of their light in its place, for next to
nothing. So this will not make the title faster than it is with rays off. It
makes real rays far cheaper than the title's own builder and walks
(`VICTORY_BVH_BUILDER=1`, 1-2 frames a second on an RTX 5070).

## What the title does (corrects `fork-ray-tracing.md` on one point)

The title does use the console's ray-tracing API (DXR 1.1, inline ray
queries). Two halves:

- **Building** goes through `XboxBvhBuilder.dll`, a DLL of the title's package
  that records the console GPU's builder programs into the title's command
  lists. The title imports seven of its methods:
  - the constructor and destructor, and `Set64BitMortonCodes`;
  - `BuildRaytracingAccelerationStructureBatched2X`: builds and updates, many
    at once;
  - `RehydrateRaytracingAccelerationStructureBatched2X`: the static world's
    structures, shipped packed in the title's data;
  - `GetRaytracingAccelerationStructurePrebuildInfo`: the sizes;
  - `CopyRaytracingAccelerationStructure`: copies and compaction.
- **Walking** is compiled into the trace programs by the console's shader
  compiler: no DXIL of them ships, only the console GPU's code. Each ray query
  became a loop with the node test `image_bvh64_intersect_ray`, a stack in
  shared memory (128 bytes a lane), the children sorted by
  `v_min3/v_med3/v_max3`, and reads of the structures' bytes by address
  (instances' transforms, primitives' data).

### What the builder is given

The DLL's own `BuildRaytracingAccelerationStructure` (not imported by the
title) shows the batched description: it copies one PC D3D12
`D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC` into one and calls the
batched method with it.

| bytes | what |
|---|---|
| 0x00-0x2f | `D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC`: destination, inputs (kind, flags, count, layout, descriptions), source, scratch |
| 0x30 | the number of post-build descriptions |
| 0x38 | a pointer to them (16 bytes each, PC D3D12's) |
| 0x40-0x4f | not known: the single build leaves them zero |

The batched method steps 80 bytes a description and 56 bytes a geometry
(`D3D12_RAYTRACING_GEOMETRY_DESC`, PC D3D12's). So a bottom level arrives with
its triangles' vertex and index buffers by address, their formats and counts,
which is what the host's DXR needs to build one.

### What the title asks for (run dlss-diag 2026-11-26 06:53, 7.7-8.1 frames a second)

Over two minutes in the world, at about 8 frames a second:

- **Per frame:** some 5 batched calls with about 64 bottom levels, 61 of
  them updates of moving things (flags 0x2b: allow update, allow compaction,
  fast build, perform update), about 340 geometries and 400,000 triangles; and
  about 5 top levels of some 950 instances each (up to 32,768).
- **The static world:** 396 rehydration calls for 4,395 structures, most in
  the first half minute, then a few as the world streams in.
- **Geometry:** triangles only (no boxes). 96% opaque, so some 6,000
  geometries have alpha tests. Vertices `R32G32B32_FLOAT` (stride 12) or
  `R32G32_FLOAT` (stride 8, the third coordinate 0, which DXR takes too).
  Hardly any has a transform (285 of 142,583).
- **Indices:** 16-bit, `R16_UINT` (57) when sizes are asked for, and mostly
  53 (`R16_TYPELESS` on the PC) in the builds. The DLL takes 57, 42
  (`R32_UINT`) and 53 as three kinds of index (bits 16, 17 and 18 of a word
  of its own). **53 is a packed form, not indices** (run dlss-diag
  2026-11-26 07:02): the same geometry description and the same index buffer
  address as the size query that said 57, but the buffer begins with a
  header, as 16-bit numbers `[blocks, 0x8000, triangles, 0, about the
  vertices, 0, two half floats, 0, 0, ...]`: the triangle count is the index
  count over three every time (130 of 390, 426 of 1278, 76 of 228), the
  blocks 1-5. The builder's programs (from Microsoft's builder, built into the
  title's package, with their DXIL kept) include `LoadTriangles`,
  `ReloadTriangles`, `RearrangeTriangles` and `ResetToIndexBuffer`: the form
  is read there. **Worked out from `LoadTriangles`' and `ReloadTriangles`'
  DXIL** (`dxc -dumpbin` of the programs from the DLL), in bytes:
  - 0: a dword, the number of blocks in its low 17 bits (bit 31 set); 4: the
    number of triangles; 8: the number of entries in the table.
  - 12: 16 bytes a block. Its table base: the low 16 bits of its second
    dword, with bit 14 of its first dword as bit 16.
  - then a dword a triangle: three 7-bit places in its block's part of the
    table (bits 0-6, 7-13, 14-20) and the block (bits 21-31).
  - then the table: the vertices' numbers, 16 bits each.

  `crates/victory-hle/src/modules/bvh.rs` (`packed`) unpacks it to plain
  triangles, with tests; `VICTORY_BVH_LOG=1` unpacks the title's own and logs
  whether they agree with their descriptions (`format 53 unpacked`,
  `unpacked_good`/`unpacked_bad` in the summary). **Checked on a run**
  (dlss-diag 2026-11-26 07:18): of the first 4,000 packed geometries, 3,914
  unpacked to exactly their index count over three triangles, every vertex
  number below the vertex count and every vertex used, the one with bit 15
  set with its corners swapped. Of the 86 others, the 20 logged were all
  buffers the process could not read whole from its side in one region
  (large ones, the same few again each frame), not ones that disagreed; the
  host will read them through its own copies of title memory.
- **Vertices of builds** are mostly at addresses of a per-frame area (the
  moved and skinned copies), not where the size query had them.
- **Strides with bit 15 set** (0x8008, 0x800c), on a geometry that uses the
  same vertex buffer as the one before it: the builder reads that bit of the
  word that holds the stride as "swap each triangle's first and third
  corner", the same vertices facing the other way. The stride is the low 15
  bits.
- **Post-build:** only compacted sizes (kind 0). No call ever copies a
  structure: compaction goes through the builds (a build or update with
  another source), so `CopyRaytracingAccelerationStructure` is not needed.
- The 16 unknown bytes of each batched description are always zero.
- Sizes are asked in pairs, the second with every buffer address
  0x6_0000_0000 higher: the same memory seen at two addresses.

For the card's ray-tracing hardware this is little: refitting 61 small bottom
levels and building five top levels a frame takes it well under a millisecond
or two. The static world's 4,395 structures are built once each.

### What the trace programs look like

Three of the 64, from saved copies:

| program | node tests | sorts of children | stack writes | reads by address | texture samples |
|---|---|---|---|---|---|
| `CS_ScreenProbes_TraceRays_RayBinning_BVH_Mixed` | 5 | 15 | 20 | 115 | 0 |
| `CS_TraceShadowRays_Eighth` | 4 | 16 | 7 | 50 | 17 |
| `CS_TraceShadowRays_FullAdaptive` | 8 | 20 | 13 | 88 | 20 |

The same compiled loop in each. The shadow programs' texture samples are in
the loop: alpha tests of leaves, fences and the like, run for each candidate
triangle (a ray query's non-opaque candidates).

## The plan

1. **See what the builder is given** (built): `VICTORY_BVH_LOG=1` wraps the
   four calls, logs them and calls the DLL as before. One run shows the
   formats, counts, flags, the unknown 16 bytes, the post-build kinds and how
   much is rehydrated against built.
2. **Host structures beside the title's**: for each build, a host DXR build of
   the same geometry (the title's addresses taken to the host's resources),
   kept by the title's destination address; top levels from the title's
   instance descriptions, their bottom-level addresses replaced on the card;
   sizes, compaction and copies on the host's structures. Behind a switch, off
   by default, and checked against the title's own structures.
3. **The static world**: its packed structures carry no geometry. Either the
   title's own unpacking programs run and the triangles are read out of the
   unpacked triangle nodes (five vertices of three floats a node), or the
   packed format is decoded on the card.
4. **The trace programs**: each walk loop replaced with a DXIL ray query on
   the host's top level (`RayQuery`, shader model 6.5). Its inputs are the
   node test's operands (origin, direction, extent) and the query's flags; its
   outputs are what the program reads after the loop (committed or not, the
   distance, barycentrics, instance and primitive). The alpha tests become the
   query's candidate loop; reads of a hit's data out of the title's structures
   are taken from the query instead. Program by program, each checked against
   the title's walk on the same rays.
5. **Measure** in the same place against rays off with `VICTORY_AMBIENT`.

DXR 1.1 does all of this. Microsoft's October 2026 preview (Agility SDK
1.722: clustered geometry, partitioned top levels, builds whose sizes the card
decides) makes builds cheaper and could take the place of parts of step 2
later. It needs Windows' developer mode and a driver with the preview.

Since 2026-11-26 the executable loads that runtime (1.722.1 preview) when the
SDK package is in an `agility-*` folder of `victory-deps` beside the repository at build time:
`apps/victory/build.rs` puts `D3D12Core.dll` and `d3d12SDKLayers.dll` in
`D3D12\` beside the executable and exports `D3D12SDKVersion` (722) and
`D3D12SDKPath`; developer mode was on. The log's `D3D12 runtime` line gives the
SDK version the device came from and whether the card does clustered geometry
and partitioned top levels: on the RTX 5070 with driver 621.40, `sdk=722`,
tier 1.2, and those two not supported (they need NVIDIA's preview driver).
Deleting the `D3D12` folder beside the executable goes back to the system's
runtime.

## Step 2 so far: bottom levels on the host (`crates/victory-gpu/src/dxr.rs`)

With `VICTORY_DXR=1`, each batched build of bottom levels the title asks for is
written down at its place in the title's command list (as the DLSS upscale is:
a step of the list's stream with `VICTORY_THIN`) and, when the list is carried
out on the host:

- each geometry's vertices, indices and transform are read through their host
  copies (`crate::shadow`, brought up to date as draws have them), so vertices
  the title's own programs moved earlier in the frame are there;
- packed indices (53) are unpacked from title memory into the list's upload
  memory as 16-bit indices, the corners swapped where the stride's bit 15 says;
  plain indices with that bit are written out again swapped;
- the host's prebuild answer sizes the structure; it goes into a pool on the
  card (blocks of 64 MiB, at most `VICTORY_DXR_MB`, 2048), kept by the title's
  destination address. An update is done as one where the host holds the
  structure it updates (built to allow updates, with as many geometries), and
  as a build where not. A structure replaced by a bigger one gives its memory
  back eight frames later;
- scratch memory is the list's own (one buffer a list, grown as needed), with a
  barrier on everything before the builds and after.

Not yet: top levels (their instances name the title's bottom levels, which
must become the host's), the static world's rehydrated structures (step 3),
compaction, and letting structures go when the title frees their memory.
Every ten seconds the log has a line `dxr: host structures so far` with the
builds, updates, updates done as builds, triangles, packed buffers unpacked,
failures by why, structures and memory held, and time spent.

## Switches

| variable | does |
|---|---|
| `VICTORY_DXR=1` | the host's own bottom levels are built beside the title's (step 2; nothing reads them yet) |
| `VICTORY_DXR_MB` | the most memory on the card for them (2048) |
| `VICTORY_BVH_LOG=1` | logs the title's calls into its builder: the first 30 of each kind one by one, the first indices of 12 geometries of index format 53, then a summary every ten seconds (`bvh log:` in the log) |

## Where to look

- `crates/victory-hle/src/modules/bvh.rs`: the layouts and the wrappers.
- `crates/victory-gpu/src/dxr.rs`: the packed triangles and the host's
  structures.
- `apps/victory/src/run.rs`: where the title's imports of the DLL are bound
  through `bvh::Wraps`.
- `fork-ray-tracing.md`: the title's builder and walks run as translated
  programs, and the node test.
