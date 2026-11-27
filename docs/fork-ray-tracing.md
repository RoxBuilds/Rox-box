# Ray tracing architecture: BVH traversal and ambient fallback

With `VICTORY_BVH_BUILDER=1` (off by default), translated guest BVH builder
and traversal compute shaders execute on the host GPU. When disabled,
`VICTORY_AMBIENT=1` optionally runs a screen-space indirect lighting estimator
configured via an external shader profile (`VICTORY_SHADER_PROFILE`).

## Console BVH model

Titles targeting console GPUs may trace rays directly inside ordinary compute
shaders rather than calling a host ray-tracing API:

- Traversal is compute code managing a per-thread stack in LDS and loading nodes
  through raw 40-bit guest virtual addresses.
- Static acceleration structures ship pre-packed in the console's native node
  layout (64-byte box nodes with 16-bit float bounds, 128-byte box nodes with
  32-bit float bounds, triangle nodes, and 128-byte instance nodes) and are
  unpacked in place by compute passes after streaming.
- Dynamic structures are built/refitted on the GPU each frame by companion BVH
  builder compute shaders.
- Hardware intrinsics (`image_bvh64_intersect_ray` and
  `image_bvh_intersect_ray`) evaluate box-node slab intersections (returning
  child nodes sorted near-to-far) and ray-triangle intersections (returning
  hit `t`, denominator, and barycentric coordinates).

Because geometry descriptions live exclusively inside guest shaders, this layer
translates both the builder and traversal programs to DXIL and emulates the
hardware ray-intersection intrinsics directly in DXIL.

## Implementation components

1. **Compute builder translation.** Supports spilled user-data parameter tables,
   root unordered-access views, byte and 16-bit raw buffer loads/stores, and
   generalized control-flow lowering across RDNA 3 (wave64/wave32) and 32-lane
   GPUs.
2. **Guest memory by raw shader address (`memory.rs`).** Maps 64 KiB guest pages
   into GPU arenas so shaders can dereference guest pointers safely, backed by
   card copies and read/write-back tracking.
3. **DXIL ray-intersection emulation (`crates/victory-shader/src/bvh.rs`).**
   Implements slab tests + sorting networks for box nodes and watertight
   ray/triangle tests verified bit-for-bit against the Rust reference.
4. **Pipeline completeness guard (`pipeline.rs`, `draw.rs`).** Builder stages
   only execute once all created builder pipelines compile cleanly on the host;
   `VICTORY_BVH_QUARANTINE=1` isolates builder writes from guest memory during
   debugging.

## BVH header & instance layout notes

- Header (64 bytes): dword 0 holds version (6 in low bits) and status flags
  (`0x8000` BLAS, `0x4` compacted, `0x2` unpacked, `0x100` packed), dword 1
  holds node size (64-byte units), dword 3 holds element count, dwords 4-9 hold
  bounds, dword 10/12/13/14 hold box-node offset, root node pointer, element
  offset, and parent-table offset.
- Instance node (128 bytes at TLAS offset 13): 3x4 transform matrix (dwords
  0-11), instance ID/mask/flags, root pointer, and BLAS base address in
  256-byte units.

| Variable | Effect |
|---|---|
| `VICTORY_BVH_BUILDER=1` | Enable translated BVH builder and ray-traversal passes |
| `VICTORY_BVH_FENCES=0` | Disable fences around builder buffer atomics |
| `VICTORY_BVH_SIZES_WAIT=1` | Synchronize size-query submissions |
| `VICTORY_BVH_BOUNDS=0` | Disable builder memory bounds clamping |
| `VICTORY_BVH_TRACE=1` | Trace requested BVH sizes and copies (diagnostics) |
| `VICTORY_BVH_QUARANTINE=1` | Prevent builder writes from propagating back to guest memory |
| `VICTORY_MEMORY_CARD_MB` | Cap on-card BVH memory allocation (default 4096) |
| `VICTORY_MEMORY_DUMP=<folder>` | Write BVH arenas to disk when triggered (diagnostics) |
| `VICTORY_UPLOAD_LOW=0`, `VICTORY_LENT_HEAP=0` | Revert upload-memory and descriptor-heap workarounds |

## Known limitations

- **Compaction size readback race:** Dynamic BVH compaction can overlap in
  card memory if guest code reads size write-backs asynchronously while host
  submissions trail by 1-2 frames.
- **Unmapped-page reads:** Reads from unmapped virtual pages return 0 and stores
  are dropped safely instead of faulting the GPU.
- **Unresolved indirect-descriptor trace variants:** A small subset of shaders
  indexing BVH addresses indirectly from descriptor tables are currently skipped.

## Without rays: screen-space ambient estimate (`crates/victory-gpu/src/ambient.rs`)

When BVH ray traversal is disabled, `VICTORY_AMBIENT=<share>` substitutes a
screen-space estimate of indirect diffuse light after the shaders identified by
`VICTORY_SHADER_PROFILE` (`[ambient]` section: resolve, radiance, and air passes):

| Variable | Effect |
|---|---|
| `VICTORY_AMBIENT=<share>` | Estimate multiplier, e.g. `1` (unset/0 disables) |
| `VICTORY_AMBIENT=constant:<level>` | Fixed ambient constant fallback |
| `VICTORY_AMBIENT_SHADE=<strength>,<metres>` | Screen-space depth crease shading (`1,0.6`) |
| `VICTORY_AMBIENT_FLOOR=<level>` | Minimum night cell luminance (`0.05`; `off` disables) |
| `VICTORY_AMBIENT_AIR=<multiple>` | Bound volumetric air scattering against surrounding light |

Ambient fallback automatically disables itself while `VICTORY_BVH_BUILDER=1` is active.

## Source files

- `crates/victory-shader/src/bvh.rs` — BVH intersection reference & tests
- `crates/victory-shader/src/emit/lower.rs` — DXIL lowering for BVH ops & guest memory
- `crates/victory-shader/src/resource.rs` — Descriptor & raw address usage tracking
- `crates/victory-gpu/src/pipeline.rs`, `draw.rs`, `memory.rs`, `ambient.rs` — Host runtime & fallback
