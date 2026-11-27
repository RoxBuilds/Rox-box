//! The structures a title passes, laid out as on PC (the public D3D12
//! headers), the interface ids it asks with and the values it fills them
//! with. Declared here rather than taken from a Windows crate, so that an
//! executable of our own can carry them without the host's bindings; the
//! self-test checks each size and offset against those bindings
//! (`victory_gpu::selftest`, `structures_have_the_pc_layouts`).

use std::ffi::c_void;

/// An object a table hands out: a pointer to its function table first.
pub type Obj = *const c_void;
pub type Iid = [u8; 16];
pub type HRESULT = i32;
pub const S_OK: HRESULT = 0;

/// A GUID's bytes in memory order.
pub const fn guid(a: u32, b: u16, c: u16, d: [u8; 8]) -> Iid {
    let (a, b, c) = (a.to_le_bytes(), b.to_le_bytes(), c.to_le_bytes());
    [
        a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5], d[6],
        d[7],
    ]
}

// The interface ids a title asks with (the PC ones).
pub const DEVICE11: Iid = guid(
    0x5405c344,
    0xd457,
    0x444e,
    [0xb4, 0xdd, 0x23, 0x66, 0xe4, 0x5a, 0xee, 0x39],
);
pub const QUEUE: Iid = guid(
    0x0ec870a6,
    0x5d7e,
    0x4c22,
    [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed],
);
pub const ALLOCATOR: Iid = guid(
    0x6102dee4,
    0xaf59,
    0x4b09,
    [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24],
);
pub const LIST: Iid = guid(
    0x5b160d0f,
    0xac1b,
    0x4185,
    [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55],
);
pub const FENCE: Iid = guid(
    0x0a753dcf,
    0xc4d8,
    0x4b91,
    [0xad, 0xf6, 0xbe, 0x5a, 0x60, 0xd9, 0x5a, 0x76],
);
pub const RESOURCE: Iid = guid(
    0x696442be,
    0xa72e,
    0x4059,
    [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
);
pub const DESCRIPTOR_HEAP: Iid = guid(
    0x8efb471d,
    0x616c,
    0x4f49,
    [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51],
);
pub const ROOT_SIGNATURE: Iid = guid(
    0xc54a6b66,
    0x72df,
    0x4ee8,
    [0x8b, 0xe5, 0xa9, 0x46, 0xa1, 0x42, 0x92, 0x14],
);
pub const PIPELINE: Iid = guid(
    0x765a30f3,
    0xf624,
    0x4c6f,
    [0xa8, 0x28, 0xac, 0xe9, 0x48, 0x62, 0x24, 0x45],
);

pub const DIRECT: u32 = 0;
pub const BUFFER: u32 = 1;
pub const TEXTURE_2D: u32 = 3;
/// `DXGI_FORMAT_R8G8B8A8_UNORM`.
pub const RGBA8: u32 = 28;
/// `DXGI_FORMAT_R32G32_FLOAT`.
pub const RG32_FLOAT: u32 = 16;
pub const DEFAULT_HEAP: u32 = 1;
pub const UPLOAD_HEAP: u32 = 2;
pub const RENDER_TARGET_FLAG: u32 = 1;
pub const UNORDERED_FLAG: u32 = 4;
pub const COMMON: u32 = 0;
pub const RENDER_TARGET: u32 = 4;
pub const COPY_DEST: u32 = 0x400;
pub const COPY_SOURCE: u32 = 0x800;
pub const RTV_HEAP: u32 = 2;
pub const VIEW_HEAP: u32 = 0;
pub const SHADER_VISIBLE: u32 = 1;
pub const TRIANGLE_LIST: u32 = 4;

/// `D3D12_COMMAND_QUEUE_DESC`.
#[repr(C)]
pub struct QueueDesc {
    pub kind: u32,
    pub priority: i32,
    pub flags: u32,
    pub nodes: u32,
}

/// `D3D12_DESCRIPTOR_HEAP_DESC`, then the room newer titles' callers have
/// after it (a placement of their own), left empty.
#[repr(C)]
pub struct HeapDesc {
    pub kind: u32,
    pub count: u32,
    pub flags: u32,
    pub nodes: u32,
    pub after: [u64; 2],
}

/// `D3D12_HEAP_PROPERTIES`.
#[repr(C)]
pub struct HeapProperties {
    pub kind: u32,
    pub cpu_pages: u32,
    pub pool: u32,
    pub creation_nodes: u32,
    pub visible_nodes: u32,
}

/// `D3D12_RESOURCE_DESC`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ResourceDesc {
    pub dimension: u32,
    pub alignment: u64,
    pub width: u64,
    pub height: u32,
    pub depth_or_array_size: u16,
    pub mip_levels: u16,
    pub format: u32,
    pub sample_count: u32,
    pub sample_quality: u32,
    pub layout: u32,
    pub flags: u32,
}

/// A transition barrier (`D3D12_RESOURCE_BARRIER`).
#[repr(C)]
pub struct Barrier {
    pub kind: u32,
    pub flags: u32,
    pub resource: Obj,
    pub subresource: u32,
    pub before: u32,
    pub after: u32,
    pub _pad: u32,
}

/// `D3D12_RECT`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// `D3D12_UNORDERED_ACCESS_VIEW_DESC` of a raw buffer.
#[repr(C)]
pub struct RawUav {
    pub format: u32,
    pub dimension: u32,
    pub first: u64,
    pub count: u32,
    pub stride: u32,
    pub counter_offset: u64,
    pub flags: u32,
    pub _pad: u32,
}

/// `D3D12_COMPUTE_PIPELINE_STATE_DESC`.
#[repr(C)]
pub struct ComputeDesc {
    pub root: Obj,
    pub code: *const u8,
    pub size: usize,
    pub nodes: u32,
    pub cached: *const u8,
    pub cached_size: usize,
    pub flags: u32,
}

/// One plane of a present (`D3D12XBOX_PRESENT_PLANE_PARAMETERS`): the frame
/// token, the resources, source rectangles and destination (none: whole),
/// colour space; two planes' room, the second left empty.
#[repr(C)]
pub struct Planes {
    pub token: u64,
    pub count: u32,
    pub _pad: u32,
    pub resources: *const Obj,
    pub sources: u64,
    pub destination: u64,
    pub rest: [u8; 0x90 - 0x28],
}

/// `D3D12_VERTEX_BUFFER_VIEW`.
#[repr(C)]
pub struct VertexView {
    pub address: u64,
    pub size: u32,
    pub stride: u32,
}

/// `D3D12_PIPELINE_STATE_STREAM_DESC`: the stream's size and where it is.
#[repr(C)]
pub struct StreamDesc {
    pub size: usize,
    pub stream: *const u8,
}

/// `D3D12_SHADER_BYTECODE`.
#[repr(C)]
pub struct Code {
    pub code: *const u8,
    pub size: usize,
}

/// `D3D12_RENDER_TARGET_BLEND_DESC`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BlendTarget {
    pub blend: i32,
    pub logic_op_enable: i32,
    pub src: i32,
    pub dest: i32,
    pub op: i32,
    pub src_alpha: i32,
    pub dest_alpha: i32,
    pub op_alpha: i32,
    pub logic_op: i32,
    pub write_mask: u8,
}

/// `D3D12_BLEND_DESC`.
#[repr(C)]
pub struct BlendDesc {
    pub alpha_to_coverage: i32,
    pub independent: i32,
    pub targets: [BlendTarget; 8],
}

/// `D3D12_RASTERIZER_DESC`.
#[repr(C)]
pub struct RasterizerDesc {
    pub fill: i32,
    pub cull: i32,
    pub front_counter_clockwise: i32,
    pub depth_bias: i32,
    pub depth_bias_clamp: f32,
    pub slope_scaled_depth_bias: f32,
    pub depth_clip: i32,
    pub multisample: i32,
    pub antialiased_lines: i32,
    pub forced_sample_count: u32,
    pub conservative: i32,
}

/// `D3D12_DEPTH_STENCILOP_DESC`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct StencilOps {
    pub fail: i32,
    pub depth_fail: i32,
    pub pass: i32,
    pub func: i32,
}

/// `D3D12_DEPTH_STENCIL_DESC1`.
#[repr(C)]
pub struct DepthStencilDesc1 {
    pub depth: i32,
    pub depth_write_mask: i32,
    pub depth_func: i32,
    pub stencil: i32,
    pub stencil_read_mask: u8,
    pub stencil_write_mask: u8,
    pub front: StencilOps,
    pub back: StencilOps,
    pub depth_bounds_test: i32,
}

/// `D3D12_INPUT_ELEMENT_DESC`.
#[repr(C)]
pub struct InputElement {
    pub semantic: *const u8,
    pub index: u32,
    pub format: u32,
    pub slot: u32,
    pub offset: u32,
    pub class: u32,
    pub step_rate: u32,
}

/// `D3D12_INPUT_LAYOUT_DESC`.
#[repr(C)]
pub struct InputLayout {
    pub elements: *const InputElement,
    pub count: u32,
}

/// `D3D12_RT_FORMAT_ARRAY`.
#[repr(C)]
pub struct TargetFormats {
    pub formats: [u32; 8],
    pub count: u32,
}

/// `DXGI_SAMPLE_DESC`.
#[repr(C)]
pub struct SampleDesc {
    pub count: u32,
    pub quality: u32,
}

/// `D3D12_VIEWPORT`.
#[repr(C)]
pub struct Viewport {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub min_depth: f32,
    pub max_depth: f32,
}

/// One subobject of a pipeline-state stream: its type, then its
/// description at its own alignment, the whole padded to a pointer, as the
/// D3D12 headers' stream helpers lay it out (and a title's code passes it).
#[repr(C, align(8))]
pub struct Sub<T>(pub u32, pub T);

/// The pipeline-state stream of a vertex and pixel pipeline, with the
/// state a title of the current generation gives one (D3D12's subobject
/// types: root signature 0, VS 1, PS 2, blend 8, sample mask 9, rasterizer
/// 10, input layout 12, topology 14, target formats 15, depth format 16,
/// samples 17, depth-stencil with bounds 21).
#[repr(C)]
pub struct GraphicsStream {
    pub root: Sub<Obj>,
    pub vs: Sub<Code>,
    pub ps: Sub<Code>,
    pub blend: Sub<BlendDesc>,
    pub sample_mask: Sub<u32>,
    pub rasterizer: Sub<RasterizerDesc>,
    pub depth: Sub<DepthStencilDesc1>,
    pub layout: Sub<InputLayout>,
    pub topology: Sub<u32>,
    pub targets: Sub<TargetFormats>,
    pub depth_format: Sub<u32>,
    pub samples: Sub<SampleDesc>,
}

impl GraphicsStream {
    /// Opaque triangles into one RGBA8 target: no blending, no culling (the
    /// triangle's winding is left to the programs), no depth.
    pub fn new(root: Obj, vs: &[u8], ps: &[u8], inputs: &[InputElement]) -> GraphicsStream {
        // D3D12_BLEND_ONE, _ZERO, D3D12_BLEND_OP_ADD, D3D12_LOGIC_OP_NOOP,
        // every channel written.
        let (one, zero, add, noop, all) = (2, 1, 1, 4, 0xf);
        let target = BlendTarget {
            blend: 0,
            logic_op_enable: 0,
            src: one,
            dest: zero,
            op: add,
            src_alpha: one,
            dest_alpha: zero,
            op_alpha: add,
            logic_op: noop,
            write_mask: all,
        };
        // D3D12_STENCIL_OP_KEEP, D3D12_COMPARISON_FUNC_ALWAYS.
        let (keep, always) = (1, 8);
        let keep = StencilOps {
            fail: keep,
            depth_fail: keep,
            pass: keep,
            func: always,
        };
        let code = |c: &[u8]| Code {
            code: c.as_ptr(),
            size: c.len(),
        };
        let mut formats = TargetFormats {
            formats: [0; 8],
            count: 1,
        };
        formats.formats[0] = RGBA8;
        GraphicsStream {
            root: Sub(0, root),
            vs: Sub(1, code(vs)),
            ps: Sub(2, code(ps)),
            blend: Sub(
                8,
                BlendDesc {
                    alpha_to_coverage: 0,
                    independent: 0,
                    targets: [target; 8],
                },
            ),
            sample_mask: Sub(9, u32::MAX),
            rasterizer: Sub(
                10,
                // D3D12_FILL_MODE_SOLID, D3D12_CULL_MODE_NONE, depth clip on.
                RasterizerDesc {
                    fill: 3,
                    cull: 1,
                    front_counter_clockwise: 0,
                    depth_bias: 0,
                    depth_bias_clamp: 0.0,
                    slope_scaled_depth_bias: 0.0,
                    depth_clip: 1,
                    multisample: 0,
                    antialiased_lines: 0,
                    forced_sample_count: 0,
                    conservative: 0,
                },
            ),
            depth: Sub(
                21,
                // No depth (D3D12_DEPTH_WRITE_MASK_ZERO), no stencil.
                DepthStencilDesc1 {
                    depth: 0,
                    depth_write_mask: 0,
                    depth_func: always,
                    stencil: 0,
                    stencil_read_mask: 0xff,
                    stencil_write_mask: 0xff,
                    front: keep,
                    back: keep,
                    depth_bounds_test: 0,
                },
            ),
            layout: Sub(
                12,
                InputLayout {
                    elements: inputs.as_ptr(),
                    count: inputs.len() as u32,
                },
            ),
            // D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE
            topology: Sub(14, 3),
            targets: Sub(15, formats),
            depth_format: Sub(16, 0),
            samples: Sub(
                17,
                SampleDesc {
                    count: 1,
                    quality: 0,
                },
            ),
        }
    }
}

/// The draw's one vertex attribute: x and y floats at the start of each
/// vertex of vertex buffer 0.
pub fn position_input() -> InputElement {
    InputElement {
        semantic: c"POSITION".as_ptr().cast(),
        index: 0,
        format: RG32_FLOAT,
        slot: 0,
        offset: 0,
        // D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA
        class: 0,
        step_rate: 0,
    }
}

/// `D3D12_DESCRIPTOR_RANGE`.
#[repr(C)]
pub struct DescriptorRange {
    pub kind: u32,
    pub count: u32,
    pub base_register: u32,
    pub space: u32,
    pub offset: u32,
}

/// `D3D12_ROOT_DESCRIPTOR_TABLE`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RootTable {
    pub count: u32,
    pub ranges: *const DescriptorRange,
}

/// `D3D12_ROOT_CONSTANTS`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RootConstants {
    pub register: u32,
    pub space: u32,
    pub count: u32,
}

/// The union of a root parameter (the two kinds used here).
#[repr(C)]
pub union RootValue {
    pub table: RootTable,
    pub constants: RootConstants,
}

/// `D3D12_ROOT_PARAMETER`.
#[repr(C)]
pub struct RootParameter {
    pub kind: u32,
    pub value: RootValue,
    pub visibility: u32,
}

pub const ROOT_TABLE: u32 = 0;
pub const ROOT_CONSTANTS: u32 = 1;
pub const RANGE_UAV: u32 = 1;
pub const VISIBLE_ALL: u32 = 0;
pub const VISIBLE_VERTEX: u32 = 1;
pub const VISIBLE_PIXEL: u32 = 5;
/// `D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT`.
pub const ROOT_INPUT_LAYOUT: u32 = 1;

impl RootParameter {
    pub fn constants(register: u32, count: u32, visibility: u32) -> RootParameter {
        RootParameter {
            kind: ROOT_CONSTANTS,
            value: RootValue {
                constants: RootConstants {
                    register,
                    space: 0,
                    count,
                },
            },
            visibility,
        }
    }

    pub fn table(ranges: &[DescriptorRange], visibility: u32) -> RootParameter {
        RootParameter {
            kind: ROOT_TABLE,
            value: RootValue {
                table: RootTable {
                    count: ranges.len() as u32,
                    ranges: ranges.as_ptr(),
                },
            },
            visibility,
        }
    }
}

/// `D3D12_ROOT_SIGNATURE_DESC`.
#[repr(C)]
pub struct RootDesc {
    pub count: u32,
    pub parameters: *const RootParameter,
    pub samplers: u32,
    pub static_samplers: *const c_void,
    pub flags: u32,
}

/// What `D3D12ConfigureVirtualMemoryX` takes and fills in: flags, the GPU
/// page table's size in 4 MiB pages, memory kept across title processes;
/// out, the range graphics addresses fall in.
#[repr(C)]
pub struct VirtualMemory {
    pub flags: u32,
    pub page_table_pages: u32,
    pub preserved_bytes: u32,
    pub lowest: u64,
    pub highest: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sizes the layer's walk of a stream takes (D3D12's padding to a
    /// pointer); the self-test checks them against the host's bindings.
    #[test]
    fn the_layouts_are_the_pc_ones() {
        assert_eq!(size_of::<ResourceDesc>(), 56);
        assert_eq!(size_of::<RawUav>(), 40);
        assert_eq!(size_of::<Planes>(), 0x90);
        assert_eq!(std::mem::offset_of!(Planes, resources), 0x10);
        assert_eq!(size_of::<QueueDesc>(), 16);
        assert_eq!(size_of::<Sub<BlendDesc>>(), 336);
        assert_eq!(size_of::<Sub<RasterizerDesc>>(), 48);
        assert_eq!(size_of::<Sub<DepthStencilDesc1>>(), 64);
        assert_eq!(size_of::<GraphicsStream>(), 0x268);
        assert_eq!(size_of::<RootParameter>(), 32);
        assert_eq!(size_of::<RootDesc>(), 40);
        assert_eq!(size_of::<VirtualMemory>(), 0x20);
    }

    #[test]
    fn ids_are_in_memory_order() {
        assert_eq!(DEVICE11[..4], [0x44, 0xc3, 0x05, 0x54]);
        assert_eq!(DEVICE11[4..8], [0x57, 0xd4, 0x4e, 0x44]);
    }
}
