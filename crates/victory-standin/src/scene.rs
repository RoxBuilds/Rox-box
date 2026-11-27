//! The pieces of the scene, each made once and recorded into lists as
//! often as the caller likes. A piece checks what a title can check by its
//! own means: what landed in its own memory. Pictures are the caller's
//! business (a title cannot read its swap chain back; the self-test reads
//! the host's copy).

use victory_formats::texture::{Dimension, Swizzle, Texture, TextureDesc};
use victory_shader::native::synth;

use crate::pc::*;
use crate::{Failed, Memory, SIDE, Title};

fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    a.iter().zip(b).position(|(x, y)| x != y)
}

/// A buffer the title fills through `Map` (an upload heap) and one it reads
/// back through its own `Map` (a default heap), copied by a list. Neither
/// has a host copy at first, so the layer copies at submission
/// (`HostList::defer_copy`).
pub struct BufferCopy {
    pub src: Obj,
    pub dst: Obj,
    from: Memory,
    to: Memory,
    len: usize,
}

impl BufferCopy {
    pub fn new(title: &Title, len: usize) -> Result<BufferCopy, Failed> {
        let src = title.buffer(UPLOAD_HEAP, len as u64, 0)?;
        let dst = title.buffer(DEFAULT_HEAP, len as u64, 0)?;
        let mut from = Title::mapped(src, len)?;
        let to = Title::mapped(dst, len)?;
        for (i, b) in from.bytes_mut().iter_mut().enumerate() {
            *b = (i as u32).wrapping_mul(2_654_435_761).rotate_right(13) as u8;
        }
        Ok(BufferCopy {
            src,
            dst,
            from,
            to,
            len,
        })
    }

    /// Every byte of the source set to `value` (the benchmark's rewrite).
    pub fn fill(&mut self, value: u8) {
        self.from.bytes_mut().fill(value);
    }

    /// The copy of the whole buffer, into the list being recorded.
    pub fn record(&self, title: &Title) {
        title.copy_buffer(self.dst, 0, self.src, 0, self.len as u64);
    }

    /// The first byte at which the copy differs from the source (the copy
    /// must have been waited for).
    pub fn differs(&self) -> Option<usize> {
        first_difference(self.from.bytes(), self.to.bytes())
    }

    /// The title changes three stretches of the source: one page, a few
    /// bytes in the middle of another, the last bytes; `round` 0 writes
    /// the self-test's values, each later round others.
    pub fn change(&mut self, round: u32) {
        let len = self.len;
        let shift = (round.wrapping_mul(29) % 251) as u8;
        let from = self.from.bytes_mut();
        from[4096..8192].fill(0xa5 ^ shift);
        from[500_000.min(len - 100)..500_100.min(len)].fill(0x5a ^ shift);
        from[len - 17..].fill(0x3c ^ shift);
    }
}

/// A [`SIDE`]-square RGBA8 texture the title writes itself in the
/// console's tiled layout (`victory_formats::texture`, the 4 KiB standard
/// swizzle the layer assumes for a texture of layout 0 without target or
/// depth flags, `resource::texture_layout`), copied into the render target
/// by a list (the layer detiles it on the way).
pub struct TiledTexture {
    pub texture: Obj,
    layout: Texture,
    memory: Memory,
    /// The picture as the title means it, rows of `SIDE` pixels.
    linear: Vec<u8>,
}

impl TiledTexture {
    pub fn new(title: &Title) -> Result<TiledTexture, Failed> {
        let layout = Texture::new(&TextureDesc {
            dimension: Dimension::Tex2d,
            width: SIDE,
            height: SIDE,
            depth_or_array: 1,
            mips: 1,
            format: RGBA8,
            samples: 1,
            swizzle: Swizzle::SW_4KB_S,
        })
        .map_err(|e| format!("no console layout for the texture: {e:?}"))?;
        let texture = title.committed(DEFAULT_HEAP, &Title::texture_desc(0), COPY_SOURCE)?;
        let mut memory = Title::mapped(texture, layout.size as usize)?;
        let linear: Vec<u8> = (0..SIDE * SIDE)
            .flat_map(|i| {
                let (x, y) = (i % SIDE, i / SIDE);
                [(x * 4) as u8, (y * 4) as u8, ((x ^ y) * 4) as u8, 255]
            })
            .collect();
        layout.planes[0]
            .layout
            .tile(memory.bytes_mut(), 0, 0, &linear, SIDE as usize * 4, 0)
            .map_err(|e| format!("tiling failed: {e:?}"))?;
        Ok(TiledTexture {
            texture,
            layout,
            memory,
            linear,
        })
    }

    /// The picture the title wrote, as rows of RGBA8 pixels.
    pub fn picture(&self) -> &[u8] {
        &self.linear
    }

    /// The texture copied into the render target, into the list being
    /// recorded (the target must be a copy destination by then).
    pub fn record_copy(&self, title: &Title) {
        title.copy_resource(title.target, self.texture);
    }

    /// A 16x16 block at (16, 8) rewritten in `colour`: only the bytes that
    /// change are stored, as a title updating part of a texture writes only
    /// its pages. How many bytes changed.
    pub fn rewrite_block(&mut self, colour: [u8; 4]) -> Result<usize, Failed> {
        for y in 8..24 {
            for x in 16..32 {
                let at = (y * SIDE as usize + x) * 4;
                self.linear[at..at + 4].copy_from_slice(&colour);
            }
        }
        let mut tiled = self.memory.bytes().to_vec();
        self.layout.planes[0]
            .layout
            .tile(&mut tiled, 0, 0, &self.linear, SIDE as usize * 4, 0)
            .map_err(|e| format!("tiling failed: {e:?}"))?;
        let mut changed = 0;
        for (m, t) in self.memory.bytes_mut().iter_mut().zip(&tiled) {
            if m != t {
                *m = *t;
                changed += 1;
            }
        }
        Ok(changed)
    }
}

/// The synthetic compute program (`synth::pattern_store`, one group of 64
/// threads, wave32) in a container, made into a pipeline with the root
/// signature its layout asks for (four root constants `b0`, a table of one
/// raw UAV `u0`), storing into a title buffer; and an upload buffer the
/// title copies a piece of into that buffer after the dispatch (which by
/// then has a host copy, so the copy is made on the card, `host/place.rs`).
pub struct Compute {
    pub container: Vec<u8>,
    pub root: Obj,
    pub pipeline: Obj,
    heap: Obj,
    table: u64,
    pub out: Obj,
    memory: Memory,
    piece: Obj,
    piece_memory: Memory,
    /// Whether a piece has been copied in (those bytes hold it since).
    pieced: bool,
}

/// The shader's buffer: 64 KiB, every dword `UNTOUCHED` until stored to.
pub const COMPUTE_BYTES: usize = 64 << 10;
pub const UNTOUCHED: u32 = 0xdead_beef;
/// Where in the shader's buffer the piece is copied, and how long it is.
pub const PIECE_AT: usize = 8192;
pub const PIECE: usize = 4096;

impl Compute {
    pub fn new(title: &Title) -> Result<Compute, Failed> {
        let container = synth::container(&[synth::pattern_store()], &[]);
        let range = DescriptorRange {
            kind: RANGE_UAV,
            count: 1,
            base_register: 0,
            space: 0,
            offset: 0,
        };
        let parameters = [
            RootParameter::constants(0, 4, VISIBLE_ALL),
            RootParameter::table(std::slice::from_ref(&range), VISIBLE_ALL),
        ];
        let root = title.root_signature(&parameters, 0)?;
        let pipeline = title.compute_pipeline(root, &container)?;
        let (heap, cpu, table) = title.view_heap(4)?;
        let out = title.buffer(DEFAULT_HEAP, COMPUTE_BYTES as u64, UNORDERED_FLAG)?;
        let mut memory = Title::mapped(out, COMPUTE_BYTES)?;
        for word in memory.bytes_mut().chunks_exact_mut(4) {
            word.copy_from_slice(&UNTOUCHED.to_le_bytes());
        }
        // R32_TYPELESS, buffer, raw.
        let view = RawUav {
            format: 39,
            dimension: 1,
            first: 0,
            count: (COMPUTE_BYTES / 4) as u32,
            stride: 0,
            counter_offset: 0,
            flags: 1,
            _pad: 0,
        };
        title.raw_uav(out, &view, cpu);
        let piece = title.buffer(UPLOAD_HEAP, PIECE as u64, 0)?;
        let piece_memory = Title::mapped(piece, PIECE)?;
        Ok(Compute {
            container,
            root,
            pipeline,
            heap,
            table,
            out,
            memory,
            piece,
            piece_memory,
            pieced: false,
        })
    }

    /// The shader's buffer's title memory (its address).
    pub fn memory(&self) -> &Memory {
        &self.memory
    }

    /// One dispatch with root `constants`, into the list being recorded.
    pub fn record_dispatch(&self, title: &Title, constants: [u32; 4]) {
        title.set_heaps(&[self.heap]);
        title.set_compute_root(self.root);
        title.set_pipeline(self.pipeline);
        title.set_compute_constants(0, &constants);
        title.set_compute_table(1, self.table);
        title.dispatch(1, 1, 1);
    }

    /// Whether every dword of the shader's buffer holds what the program
    /// stores with `constants` (the dispatch must have been waited for),
    /// the rest untouched (the piece aside, once copied in); `what` names
    /// the dispatch in the failure.
    pub fn check_stores(&self, constants: [u32; 4], what: &str) -> Result<(), Failed> {
        let threads = synth::PATTERN_THREADS as usize;
        let piece = PIECE_AT / 4..(PIECE_AT + PIECE) / 4;
        for (i, word) in self.memory.bytes().chunks_exact(4).enumerate() {
            if self.pieced && piece.contains(&i) {
                continue;
            }
            let got = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
            let want = if i < threads {
                synth::pattern_value(i as u32, [constants[0], constants[1]])
            } else {
                UNTOUCHED
            };
            if got != want {
                return Err(format!("{what}: title memory at dword {i} holds {got:#010x}, the program stores {want:#010x}").into());
            }
        }
        Ok(())
    }

    /// The title writes the piece's bytes (`round` 0: the self-test's).
    pub fn write_piece(&mut self, round: u32) {
        for (i, b) in self.piece_memory.bytes_mut().iter_mut().enumerate() {
            *b = (i * 7 + 3) as u8 ^ (round.wrapping_mul(13) as u8);
        }
    }

    /// The piece copied into the shader's buffer at [`PIECE_AT`], into the
    /// list being recorded.
    pub fn record_copy_in(&mut self, title: &Title) {
        title.copy_buffer(self.out, PIECE_AT as u64, self.piece, 0, PIECE as u64);
        self.pieced = true;
    }

    /// Whether the piece landed in title memory, and nothing past it.
    pub fn check_copy_in(&self) -> Result<(), Failed> {
        let memory = self.memory.bytes();
        if let Some(at) = first_difference(
            self.piece_memory.bytes(),
            &memory[PIECE_AT..PIECE_AT + PIECE],
        ) {
            return Err(format!(
                "a copy into the shader's buffer did not land: byte {} differs",
                PIECE_AT + at
            )
            .into());
        }
        let past = &memory[PIECE_AT + PIECE..PIECE_AT + PIECE + 4];
        let after = u32::from_le_bytes([past[0], past[1], past[2], past[3]]);
        if after != UNTOUCHED {
            return Err(format!(
                "a copy into the shader's buffer wrote past its end ({after:#010x})"
            )
            .into());
        }
        Ok(())
    }
}

/// One draw of the triangle over the target: the vertex program's four
/// root constants (its colour's channels as floats' bits), the pixel
/// program's one (alpha), and the scissor.
#[derive(Clone, Copy, Debug)]
pub struct DrawCall {
    pub vs: [u32; 4],
    pub ps: u32,
    pub scissor: Rect,
}

impl DrawCall {
    /// The colour the programs draw, as RGBA8.
    pub fn colour(&self) -> [u8; 4] {
        synth::draw_colour(self.vs, self.ps).map(|c| (c * 255.0).round() as u8)
    }
}

/// The bits of `n / 255` as a float: a colour channel that is a whole step
/// of an 8-bit target.
pub fn step(n: u32) -> u32 {
    (n as f32 / 255.0).to_bits()
}

/// The whole target, and its left half.
pub const WHOLE: Rect = Rect {
    left: 0,
    top: 0,
    right: SIDE as i32,
    bottom: SIDE as i32,
};
pub const LEFT_HALF: Rect = Rect {
    left: 0,
    top: 0,
    right: SIDE as i32 / 2,
    bottom: SIDE as i32,
};

/// The synthetic vertex and pixel programs (`synth::draw_vertex`,
/// `synth::draw_pixel`, wave32, with the signatures they link by and the
/// pixel program's export patch), their root signature (four constants for
/// the vertex stage `b0`, slot 0; one for the pixel stage `b1`, slot 1),
/// a pipeline made from a pipeline-state stream, and one triangle over the
/// target in a vertex buffer in title memory (an upload-heap buffer the
/// title fills through `Map`, copied to the card by the draw, `shadow.rs`).
pub struct Drawing {
    pub vs: Vec<u8>,
    pub ps: Vec<u8>,
    pub root: Obj,
    pub pipeline: Obj,
    view: VertexView,
}

impl Drawing {
    pub fn new(title: &Title) -> Result<Drawing, Failed> {
        let (vs, ps) = (synth::draw_vertex(), synth::draw_pixel());
        let parameters = [
            RootParameter::constants(0, 4, VISIBLE_VERTEX),
            RootParameter::constants(1, 1, VISIBLE_PIXEL),
        ];
        let root = title.root_signature(&parameters, ROOT_INPUT_LAYOUT)?;
        let inputs = [position_input()];
        let stream = GraphicsStream::new(root, &vs, &ps, &inputs);
        let pipeline = title.stream_pipeline(&stream)?;
        // (-1, -1), (-1, 3), (3, -1) as x, y floats: one triangle that
        // covers the whole target.
        const STRIDE: u32 = 8;
        let corners: [f32; 6] = [-1.0, -1.0, -1.0, 3.0, 3.0, -1.0];
        let vertices = title.buffer(UPLOAD_HEAP, 256, 0)?;
        let mut memory = Title::mapped(vertices, 256)?;
        for (at, c) in memory.bytes_mut().chunks_exact_mut(4).zip(corners) {
            at.copy_from_slice(&c.to_le_bytes());
        }
        let view = VertexView {
            address: Title::gpu_address(vertices),
            size: STRIDE * 3,
            stride: STRIDE,
        };
        Ok(Drawing {
            vs,
            ps,
            root,
            pipeline,
            view,
        })
    }

    /// `draws` into the list being recorded, after the target, a viewport,
    /// the topology, the root signature, the pipeline and the vertex buffer
    /// are set (the target must be a render target by then).
    pub fn record(&self, title: &Title, draws: &[DrawCall]) {
        let viewport = Viewport {
            x: 0.0,
            y: 0.0,
            width: SIDE as f32,
            height: SIDE as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        };
        title.set_target();
        title.set_viewport(&viewport);
        title.set_topology(TRIANGLE_LIST);
        title.set_graphics_root(self.root);
        title.set_pipeline(self.pipeline);
        title.set_vertex_buffer(&self.view);
        for d in draws {
            title.set_scissor(&d.scissor);
            title.set_graphics_constants(0, &d.vs);
            title.set_graphics_constants(1, &[d.ps]);
            title.draw(3);
        }
    }
}
