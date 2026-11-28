//! Heaps, committed and placed resources, and resource sizes.
//!
//! Heaps and resources are host memory and a resource's GPU address is its
//! CPU address, as on the console; textures also get a host copy, and
//! buffers a host copy when a draw reads them (`shadow`).
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::ffi::c_void;
use std::ptr::NonNull;

use victory_formats::texture;

use crate::object::{
    self, E_INVALIDARG, E_OUTOFMEMORY, HEAP, HRESULT, Iid, Object, RESOURCE, S_OK, Table, This,
    guid,
};

/// The interfaces every device child also answers for.
pub(crate) const PAGEABLE_IIDS: [Iid; 3] = [
    guid(
        0x63ee58fb,
        0x1268,
        0x4835,
        [0x86, 0xda, 0xf0, 0x08, 0xce, 0x62, 0xf0, 0xd6],
    ),
    guid(
        0x905db94b,
        0xa00c,
        0x4140,
        [0x9d, 0xf5, 0x2b, 0x64, 0xca, 0x9e, 0xa3, 0x57],
    ),
    guid(
        0xc4fec28f,
        0x7966,
        0x4e95,
        [0x9f, 0x94, 0xf4, 0x31, 0xcb, 0x56, 0xc3, 0xb8],
    ),
];
const HEAP_IIDS: [Iid; 4] = [
    guid(
        0x6b3b2502,
        0x6e51,
        0x45b3,
        [0x90, 0xee, 0x98, 0x84, 0x26, 0x5e, 0x8d, 0xf3],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];
const RESOURCE_IIDS: [Iid; 4] = [
    guid(
        0x696442be,
        0xa72e,
        0x4059,
        [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

const KIB_64: u64 = 64 << 10;
const MIB_4: u64 = 4 << 20;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct HeapProperties {
    kind: u32,
    cpu_pages: u32,
    pool: u32,
    creation_nodes: u32,
    visible_nodes: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct HeapDesc {
    size: u64,
    properties: HeapProperties,
    alignment: u64,
    flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct ResourceDesc {
    dimension: u32,
    alignment: u64,
    width: u64,
    height: u32,
    depth_or_array_size: u16,
    mip_levels: u16,
    format: u32,
    sample_count: u32,
    sample_quality: u32,
    layout: u32,
    flags: u32,
}

#[cfg(windows)]
impl ResourceDesc {
    /// The texture of this one's levels from `base` on.
    fn levels_from(&self, base: u32) -> ResourceDesc {
        let mut d = *self;
        if base > 0 {
            d.width = (d.width >> base).max(1);
            d.height = (d.height >> base).max(1);
            d.mip_levels -= base as u16;
        }
        d
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AllocationInfo {
    size: u64,
    alignment: u64,
}

const _: () = assert!(size_of::<HeapProperties>() == 20);
const _: () = assert!(size_of::<HeapDesc>() == 48);
// The padding a placed view leaves out (`descriptor::placed_resource_words`):
// word 1, before the alignment, and word 13, after the flags.
const _: () = assert!(std::mem::offset_of!(ResourceDesc, alignment) == 8);
const _: () = assert!(std::mem::offset_of!(ResourceDesc, flags) == 48);
const _: () = assert!(size_of::<ResourceDesc>() == 56);

const BUFFER: u32 = 1;
const TEXTURE_3D: u32 = 4;

/// Bytes per element and the element's width in texels (4 for block
/// compression), by DXGI format number. `None` for 1-bit and video
/// formats. The console adds 116/117 (10-bit float color), 118-120 (16-bit
/// depth with 8-bit stencil, two planes: 3 bytes per texel), 189 (signed
/// 10-bit color), 190 (two 4-bit channels) and 191 (24/8 color).
pub(crate) fn element(format: u32) -> Option<(u64, u64)> {
    Some(match format {
        1..=4 => (16, 1),
        5..=8 => (12, 1),
        9..=22 => (8, 1),
        23..=47 | 67 | 87..=93 | 116 | 117 | 189 | 191 => (4, 1),
        118..=120 => (3, 1),
        48..=59 | 68 | 69 | 85 | 86 | 115 => (2, 1),
        60..=65 | 190 => (1, 1),
        70..=72 | 79..=81 => (8, 4),
        73..=78 | 82..=84 | 94..=99 => (16, 4),
        _ => return None,
    })
}

/// A texture's layout in console memory, as the console's layout library
/// gives it (`victory-hle`'s `xg` serves the same): the description's
/// layout field is `0x100 | swizzle mode`, 1 linear, 0 the library's pick.
pub(crate) fn texture_layout(desc: &ResourceDesc) -> Option<texture::Texture> {
    use texture::{Dimension, Swizzle, TextureDesc};
    const TEXTURE_1D: u32 = 2;
    const TEXTURE_2D: u32 = 3;
    const RENDER_TARGET: u32 = 1;
    const DEPTH: u32 = 2;
    let dimension = match desc.dimension {
        TEXTURE_1D => Dimension::Tex1d,
        TEXTURE_2D => Dimension::Tex2d,
        TEXTURE_3D => Dimension::Tex3d,
        _ => return None,
    };
    let swizzle = match desc.layout {
        0 if dimension == Dimension::Tex1d => Swizzle::LINEAR,
        0 if desc.flags & DEPTH != 0 => Swizzle::SW_64KB_Z_X,
        0 if desc.flags & RENDER_TARGET != 0 => Swizzle::SW_64KB_R_X,
        0 => Swizzle::SW_4KB_S,
        1 => Swizzle::LINEAR,
        l if l & !0xff == 0x100 => Swizzle(l & 0xff),
        _ => return None,
    };
    texture::Texture::new(&TextureDesc {
        dimension,
        width: u32::try_from(desc.width).ok()?,
        height: desc.height,
        depth_or_array: u32::from(desc.depth_or_array_size),
        mips: u32::from(desc.mip_levels),
        format: desc.format,
        samples: desc.sample_count.max(1),
        swizzle,
    })
    .ok()
}

/// Bytes and alignment a resource takes: buffers their width (64 KiB
/// aligned), textures their console layout (formats it does not know:
/// every mip of every array slice and sample, packed, 64 KiB aligned),
/// rounded up to the alignment.
fn footprint(desc: &ResourceDesc) -> Option<AllocationInfo> {
    if desc.dimension != BUFFER
        && let Some(t) = texture_layout(desc)
    {
        // As the console: the layout's own alignment unless more is asked
        // (4 KiB for 4 KiB modes, where PC says 64 KiB).
        // The title keeps these sizes in its texture tables, and its streaming
        // asserts when they disagree with the streamed layout.
        let alignment = desc.alignment.max(t.alignment);
        if !alignment.is_power_of_two() {
            return None;
        }
        return Some(AllocationInfo {
            size: t.size.checked_next_multiple_of(alignment)?,
            alignment,
        });
    }
    let bytes = if desc.dimension == BUFFER {
        desc.width
    } else {
        let (element, block) = element(desc.format)?;
        let (depth, slices) = if desc.dimension == TEXTURE_3D {
            (u64::from(desc.depth_or_array_size), 1)
        } else {
            (1, u64::from(desc.depth_or_array_size))
        };
        let height = u64::from(desc.height);
        let mips = mip_count(desc);
        let mut chain = 0u64;
        for mip in 0..mips.min(64) {
            let w = (desc.width >> mip).max(1).div_ceil(block);
            let h = (height >> mip).max(1).div_ceil(block);
            let d = (depth >> mip).max(1);
            chain = chain.checked_add(w.checked_mul(h)?.checked_mul(d)?.checked_mul(element)?)?;
        }
        chain
            .checked_mul(slices)?
            .checked_mul(u64::from(desc.sample_count.max(1)))?
    };
    if bytes == 0 {
        return None;
    }
    let minimum = if desc.sample_count > 1 { MIB_4 } else { KIB_64 };
    let alignment = desc.alignment.max(minimum);
    if !alignment.is_power_of_two() {
        return None;
    }
    Some(AllocationInfo {
        size: bytes.checked_next_multiple_of(KIB_64)?,
        alignment,
    })
}

/// A format as the host has it. The console's 16-bit depth with stencil
/// (and its typeless views) has no PC format: the 24-bit ones stand in.
pub(crate) fn pc_format(format: u32) -> u32 {
    match format {
        118 => 45,
        119 => 46,
        120 => 47,
        other => other,
    }
}

/// The format of a texture the host GPU renders to or writes as unordered
/// access, for the title's `format`. The console's GPU does both with the
/// 16-bit colour formats B5G5R5A1 (86) and B4G4R4A4 (115), and
/// the title has full-screen targets in both; a host that refuses either use
/// (NVIDIA) refused the whole texture, so every pass that wrote it drew
/// nothing. There 8-bit RGBA stands in: the same channels in the same
/// order, every 4-bit level exact and every 5-bit level within 1/510.
/// The title also renders to R9G9B9E5 (67), which no PC GPU does: 16-bit
/// float RGBA stands in, with at least R9G9B9E5's precision and range.
/// The title's pipelines and views of such targets name the stand-in too
/// ([`view_format`], the host view checks).
/// shortcut: copies between such a target and a texture the title fills
/// itself in the 16-bit format are skipped ([`stands_in`]); convert them
/// if a pass turns out to need one.
pub(crate) fn target_format(format: u32) -> u32 {
    const STAND_INS: [(u32, u32); 3] = [(86, 28), (115, 28), (67, 10)];
    let Some(i) = STAND_INS.iter().position(|&(f, _)| f == format) else {
        return format;
    };
    #[cfg(windows)]
    {
        use windows::Win32::Graphics::Direct3D12::*;
        static REFUSED: std::sync::OnceLock<[bool; 3]> = std::sync::OnceLock::new();
        let refused = REFUSED.get_or_init(|| {
            STAND_INS.map(|(f, to)| {
                let Some(host) = crate::host::host() else {
                    return false;
                };
                let mut support = D3D12_FEATURE_DATA_FORMAT_SUPPORT {
                    Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT(f as i32),
                    ..Default::default()
                };
                // SAFETY: a plain query into our own structure.
                let asked = unsafe {
                    host.device.CheckFeatureSupport(
                        D3D12_FEATURE_FORMAT_SUPPORT,
                        (&raw mut support).cast(),
                        size_of_val(&support) as u32,
                    )
                };
                let both = D3D12_FORMAT_SUPPORT1_RENDER_TARGET.0
                    | D3D12_FORMAT_SUPPORT1_TYPED_UNORDERED_ACCESS_VIEW.0;
                let refused = asked.is_err() || support.Support1.0 & both != both;
                if refused {
                    tracing::info!(format = f, stand_in = to, "the host does not render to this format: a stand-in is used");
                }
                refused
            })
        });
        if refused[i] {
            return STAND_INS[i].1;
        }
    }
    format
}

/// Whether the host copy of the title texture `this` is in a stand-in
/// format ([`target_format`]): its texels are not laid out as the title's.
#[cfg(windows)]
pub(crate) fn stands_in(this: This) -> bool {
    if this.is_null() {
        return false;
    }
    let own = pc_format(resource(this).desc.format);
    target_format(own) != own
        && host_resource(this)
            // SAFETY: a live resource.
            .is_some_and(|host| unsafe { host.GetDesc() }.Format.0 as u32 == target_format(own))
}

/// Depth textures the title also asks unordered access for (the console
/// allows both; the host copy loses the unordered access): how many were
/// made, and how often such a texture was bound as the depth target,
/// cleared as depth, and asked for an unordered access view the host could
/// not give. Measured to decide what such textures need (a colour twin, or
/// to be colour textures altogether).
pub(crate) static DEPTH_UNORDERED: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// Whether the title asked for both depth and unordered access for `this`.
#[cfg(windows)]
pub(crate) fn depth_and_unordered(this: This) -> bool {
    !this.is_null() && resource(this).desc.flags & 6 == 6
}

/// The colour twin of a depth texture the title also writes as unordered
/// access. The console allows both on one texture; the host gives a depth
/// texture no unordered access, so shaders write a colour texture of the
/// same size and format family instead, and one is copied into the other
/// when a side is used after the other was written
/// (`HostList::twin_use`).
/// shortcut: whole textures are copied, not the mips and slices written.
/// A depth texture with a stencil plane gets a twin for each plane (
/// the title's tiled lighting writes each pixel's material class into
/// the depth plane and masks into the stencil plane, then draws each class
/// with the depth test "equal"): the stencil plane's is copied through a
/// buffer, the depth plane's is drawn into the plane by a pass of ours
/// (`HostList::pass_plane`). Unordered access views of a multisampled
/// texture stay null.
#[cfg(windows)]
pub(crate) struct Twin {
    /// The colour texture and the state it is in, as a host copy's.
    pub(crate) colour: std::sync::Arc<HostTexture>,
    /// Which side lacks what the other was written last, after the lists
    /// submitted so far: bit 0 the depth texture, bit 1 the twin.
    pub(crate) behind: std::sync::atomic::AtomicU32,
    /// For a depth texture with a stencil plane, `colour` is the twin of
    /// that plane (8 bits a texel) and this the buffer the two are copied
    /// through: no colour format shares a family with a depth-stencil
    /// format, but the host copies between a buffer and one plane.
    pub(crate) through: Option<windows::Win32::Graphics::Direct3D12::ID3D12Resource>,
    /// For a depth texture with a stencil plane: the twin of its depth
    /// plane, 16 bits a texel for the console's 16-bit depth-stencil
    /// formats and a 32-bit float otherwise. No host colour format holds
    /// the bits of a 24-bit depth plane, so the two are not copied: a pass
    /// draws the twin into the plane.
    pub(crate) plane: Option<std::sync::Arc<HostTexture>>,
}

#[cfg(windows)]
impl Drop for Twin {
    fn drop(&mut self) {
        TWINS[0].fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Colour twins alive, depth textures with unordered access left without
/// one, and the copies recorded into depth textures and into twins since
/// the last frame-rate line.
#[cfg(windows)]
pub(crate) static TWINS: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// Depth planes drawn from their twins by the pass since the last
/// frame-rate line; and since the start, how often a depth plane's twin
/// lacked what was written to the plane (not given to it), and how often a
/// depth plane was used in the list that had written its twin, before the
/// pass (which runs when the list closes).
#[cfg(windows)]
pub(crate) static PLANES: [std::sync::atomic::AtomicU64; 3] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// The colour twin of title texture `this`, if it has one.
#[cfg(windows)]
pub(crate) fn twin(this: This) -> Option<std::sync::Arc<Twin>> {
    if this.is_null() {
        return None;
    }
    hosted(this, true).and_then(|made| made.1)
}

/// The twin's texture, for the unordered access views of `this`.
#[cfg(windows)]
pub(crate) fn twin_resource<'a>(
    this: This,
) -> Option<&'a windows::Win32::Graphics::Direct3D12::ID3D12Resource> {
    if this.is_null() {
        return None;
    }
    let (_, twin) = hosted(this, true)?;
    // SAFETY: as in `host_resource`: the twin lives as long as the host
    // copy, which a depth texture keeps until the title releases it.
    twin.map(|t| unsafe { &*std::ptr::from_ref(&t.colour.texture) })
}

/// The texture of the twin of the depth plane of `this`, a depth texture
/// with a stencil plane, for the unordered access views of that plane.
#[cfg(windows)]
pub(crate) fn twin_plane_resource<'a>(
    this: This,
) -> Option<&'a windows::Win32::Graphics::Direct3D12::ID3D12Resource> {
    if this.is_null() {
        return None;
    }
    let (_, twin) = hosted(this, true)?;
    let twin = twin?;
    // SAFETY: as in `twin_resource`.
    twin.plane
        .as_ref()
        .map(|p| unsafe { &*std::ptr::from_ref(&p.texture) })
}

/// The colour twin for a texture described by `desc` whose host copy is
/// `depth`, if the title asks for depth and unordered access both and the
/// host can copy between the two: the same size, mips and slices in the
/// depth format's typeless family (R16 for 16-bit depth, R32 for 32-bit),
/// or R8 for the stencil plane of a format that has one, with unordered
/// access only. A format with a stencil plane gets a twin for its depth
/// plane too, R16 for the console's 16-bit depth-stencil formats and R32
/// otherwise. `VICTORY_DEPTH_TWINS=0` makes none (such views stay null),
/// `VICTORY_DEPTH_TWINS=stencil` only those of formats with a stencil
/// plane, `VICTORY_DEPTH_TWINS=nodepth` none for the depth planes of such
/// formats.
#[cfg(windows)]
fn twin_texture(
    desc: &ResourceDesc,
    depth: &windows::Win32::Graphics::Direct3D12::ID3D12Resource,
) -> Option<std::sync::Arc<Twin>> {
    use std::sync::atomic::Ordering::Relaxed;
    use windows::Win32::Graphics::Direct3D12::*;
    const DEPTH_AND_UNORDERED: u32 = 6;
    const UNORDERED: u32 = 4;
    const UNORDERED_ACCESS: u32 = 0x8;
    // R32 and R16 typeless: the families of the depth formats of one plane.
    const ONE_PLANE: [u32; 2] = [39, 53];
    if desc.dimension == BUFFER || desc.flags & DEPTH_AND_UNORDERED != DEPTH_AND_UNORDERED {
        return None;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("VICTORY_DEPTH_TWINS").is_none_or(|v| v != "0")) {
        return None;
    }
    // R32G8X24 and R24G8 typeless: depth with a stencil plane, whose twin
    // is R8 typeless.
    const WITH_STENCIL: [u32; 2] = [19, 44];
    const R8_TYPELESS: u32 = 60;
    let typeless = family(pc_format(desc.format));
    let stencil = WITH_STENCIL.contains(&typeless);
    static STENCIL_ONLY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let stencil_only = *STENCIL_ONLY
        .get_or_init(|| std::env::var_os("VICTORY_DEPTH_TWINS").is_some_and(|v| v == "stencil"));
    if !stencil && stencil_only {
        return None;
    }
    if !(stencil || ONE_PLANE.contains(&typeless)) || desc.sample_count > 1 {
        TWINS[1].fetch_add(1, Relaxed);
        return None;
    }
    let host = crate::host::host()?;
    let mut d = *desc;
    d.layout = 0;
    d.alignment = 0;
    d.flags = UNORDERED;
    d.format = if stencil { R8_TYPELESS } else { typeless };
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let mut texture: Option<ID3D12Resource> = None;
    // SAFETY: the description has the host layout, as in `host_texture`;
    // the out pointer is ours.
    let made = unsafe {
        host.device.CreateCommittedResource(
            &heap,
            crate::host::not_resident(),
            (&raw const d).cast(),
            D3D12_RESOURCE_STATES(UNORDERED_ACCESS as i32),
            None,
            &mut texture,
        )
    };
    if let Err(e) = made {
        tracing::warn!(error = %e, desc = ?d, "no colour twin for a depth texture");
    }
    let Some(texture) = texture else {
        TWINS[1].fetch_add(1, Relaxed);
        return None;
    };
    let _ = crate::host::resident(&host.device, &texture, crate::host::NewMemory::Twin);
    crate::host::name(&texture, || {
        format!("depth twin {}x{} f{}", d.width, d.height, d.format)
    });
    // Shaders write it as they write targets: it stays on the card first.
    host.hint_residency(&[(&texture, crate::host::Host::RESIDENCY_HIGH)]);
    // The buffer a stencil plane and its twin are copied through: as big
    // as the plane's mips and slices laid out for a copy (they follow the
    // depth plane's in the texture's subresources).
    let through = if stencil {
        // SAFETY: a live texture.
        let of = unsafe { depth.GetDesc() };
        let planes = u32::from(of.MipLevels) * u32::from(of.DepthOrArraySize);
        let mut bytes = 0;
        // SAFETY: a plain query; the out pointer is ours.
        unsafe {
            host.device.GetCopyableFootprints(
                &of,
                planes,
                planes,
                0,
                None,
                None,
                None,
                Some(&mut bytes),
            );
        }
        let Some(buffer) = host.uav_buffer(bytes) else {
            TWINS[1].fetch_add(1, Relaxed);
            return None;
        };
        Some(buffer)
    } else {
        None
    };
    // The twin of the depth plane of a format with a stencil plane: R16
    // typeless (53) for the console's 16-bit depth-stencil formats (118 to
    // 120), whose shaders write 16-bit steps, R32 typeless (39) otherwise.
    static NO_PLANE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let no_plane = *NO_PLANE
        .get_or_init(|| std::env::var_os("VICTORY_DEPTH_TWINS").is_some_and(|v| v == "nodepth"));
    let plane = if stencil && !no_plane {
        d.format = if (118..=120).contains(&desc.format) {
            53
        } else {
            39
        };
        let mut texture: Option<ID3D12Resource> = None;
        // SAFETY: as for the first twin.
        let made = unsafe {
            host.device.CreateCommittedResource(
                &heap,
                crate::host::not_resident(),
                (&raw const d).cast(),
                D3D12_RESOURCE_STATES(UNORDERED_ACCESS as i32),
                None,
                &mut texture,
            )
        };
        if let Err(e) = made {
            tracing::warn!(error = %e, desc = ?d, "no twin for a depth plane");
        }
        texture.map(|texture| {
            let _ = crate::host::resident(&host.device, &texture, crate::host::NewMemory::Twin);
            crate::host::name(&texture, || {
                format!("depth plane twin {}x{} f{}", d.width, d.height, d.format)
            });
            host.hint_residency(&[(&texture, crate::host::Host::RESIDENCY_HIGH)]);
            std::sync::Arc::new(HostTexture::new(texture, UNORDERED_ACCESS))
        })
    } else {
        None
    };
    TWINS[0].fetch_add(1, Relaxed);
    Some(std::sync::Arc::new(Twin {
        colour: std::sync::Arc::new(HostTexture::new(texture, UNORDERED_ACCESS)),
        behind: std::sync::atomic::AtomicU32::new(0),
        through,
        plane,
    }))
}

/// The typeless format of a format's family (itself if it has none).
pub(crate) fn family(format: u32) -> u32 {
    const FAMILIES: [(u32, u32); 22] = [
        (1, 4),
        (5, 8),
        (9, 14),
        (15, 18),
        (19, 22),
        (23, 25),
        (27, 32),
        (33, 38),
        (39, 43),
        (44, 47),
        (48, 52),
        (53, 59),
        (60, 64),
        (70, 72),
        (73, 75),
        (76, 78),
        (79, 81),
        (82, 84),
        (90, 91),
        (92, 93),
        (94, 96),
        (97, 99),
    ];
    match format {
        87 => 90,
        88 => 92,
        f => FAMILIES
            .iter()
            .find(|&&(first, last)| (first..=last).contains(&f))
            .map_or(f, |&(first, _)| first),
    }
}

/// sRGB formats and their linear twins.
pub(crate) const SRGB: [(u32, u32); 3] = [(29, 28), (91, 87), (93, 88)];

/// The format a view of a title texture must name when its host copy is
/// typeless (see [`DEPTH_FORMATS`], [`SRGB`]): the title's own; `None` if
/// the host copy is typed.
pub(crate) fn view_format(this: This) -> Option<u32> {
    // First: a view of no resource (a typed null view) has none.
    let host = host_resource(this)?;
    let own = pc_format(resource(this).desc.format);
    // SAFETY: a live resource.
    let format = unsafe { host.GetDesc() }.Format.0 as u32;
    // A stand-in target is typed.
    (format != own && format != target_format(own)).then_some(own)
}

/// Depth formats with their typeless family: depth textures that shaders
/// read are made typeless on the host, which only allows views of
/// typeless depth resources.
pub(crate) const DEPTH_FORMATS: [(u32, u32); 4] = [(20, 19), (40, 39), (45, 44), (55, 53)];

/// Mip levels, the full chain for 0.
fn mip_count(desc: &ResourceDesc) -> u32 {
    let depth = if desc.dimension == TEXTURE_3D {
        u64::from(desc.depth_or_array_size)
    } else {
        1
    };
    match desc.mip_levels {
        0 => {
            64 - desc
                .width
                .max(u64::from(desc.height))
                .max(depth)
                .max(1)
                .leading_zeros()
        }
        n => u32::from(n),
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlacedFootprint {
    pub(crate) offset: u64,
    format: u32,
    width: u32,
    height: u32,
    depth: u32,
    row_pitch: u32,
}

impl PlacedFootprint {
    /// The bytes it spans in a buffer: every row pitched but the last.
    pub(crate) fn bytes(&self) -> Option<u64> {
        let (element, block) = element(self.format)?;
        let lines = u64::from(self.height).div_ceil(block) * u64::from(self.depth);
        let last = u64::from(self.width).div_ceil(block) * element;
        Some(u64::from(self.row_pitch) * lines.saturating_sub(1) + last)
    }

    /// For a read back through a host buffer: this footprint at offset 0
    /// with its rows pitched as the host wants them (multiples of 256
    /// bytes), then the bytes that buffer needs, the bytes of a row, the
    /// rows, and the title's own pitch.
    pub(crate) fn host_rows(&self) -> Option<(PlacedFootprint, [u64; 4])> {
        let (element, block) = element(self.format)?;
        let lines = (u64::from(self.height).div_ceil(block) * u64::from(self.depth)).max(1);
        let row = u64::from(self.width).div_ceil(block) * element;
        let pitch = row.max(1).next_multiple_of(256);
        let host = PlacedFootprint {
            offset: 0,
            row_pitch: u32::try_from(pitch).ok()?,
            ..*self
        };
        Some((host, [pitch * lines, row, lines, u64::from(self.row_pitch)]))
    }
}
const _: () = assert!(size_of::<PlacedFootprint>() == 32);

const NO_FOOTPRINT: PlacedFootprint = PlacedFootprint {
    offset: u64::MAX,
    format: 0,
    width: 0,
    height: 0,
    depth: 0,
    row_pitch: 0,
};

/// One subresource's upload footprint (offset 0), row count and unpadded row size.
fn subresource(desc: &ResourceDesc, index: u32) -> Option<(PlacedFootprint, u32, u64)> {
    const PITCH: u64 = 256;
    if desc.dimension == BUFFER {
        let footprint = PlacedFootprint {
            offset: 0,
            format: 0,
            width: u32::try_from(desc.width).ok()?,
            height: 1,
            depth: 1,
            row_pitch: u32::try_from(desc.width.next_multiple_of(PITCH)).ok()?,
        };
        return (index == 0).then_some((footprint, 1, desc.width));
    }
    let mips = mip_count(desc);
    let slices = if desc.dimension == TEXTURE_3D {
        1
    } else {
        u32::from(desc.depth_or_array_size).max(1)
    };
    let mip = index % mips;
    let plane = index / mips / slices;
    let depth_stencil = matches!(desc.format, 19..=22 | 44..=47);
    // Each plane contains all the mip levels and array slices. Depth and
    // stencil copies use separate formats and 512-byte row alignment.
    let (format, pitch_alignment) = match (depth_stencil, plane) {
        (true, 0) => (39, 512), // DXGI_FORMAT_R32_TYPELESS
        (true, 1) => (60, 512), // DXGI_FORMAT_R8_TYPELESS
        (false, 0) => (desc.format, PITCH),
        _ => return None,
    };
    let (element, block) = element(format)?;
    let width = (desc.width >> mip).max(1).next_multiple_of(block);
    let height = (u64::from(desc.height) >> mip)
        .max(1)
        .next_multiple_of(block);
    let depth = if desc.dimension == TEXTURE_3D {
        (u32::from(desc.depth_or_array_size) >> mip).max(1)
    } else {
        1
    };
    let row_size = width / block * element;
    let footprint = PlacedFootprint {
        offset: 0,
        format,
        width: u32::try_from(width).ok()?,
        height: u32::try_from(height).ok()?,
        depth,
        row_pitch: u32::try_from(row_size.next_multiple_of(pitch_alignment)).ok()?,
    };
    Some((footprint, u32::try_from(height / block).ok()?, row_size))
}

/// Places subresources at 512-byte boundaries in an upload buffer.
/// Row pitch is aligned to 512 bytes for depth/stencil planes and 256 bytes
/// otherwise. Unknown sizes give the documented all-ones values.
pub(crate) extern "system" fn copyable_footprints(
    _: This,
    desc: *const ResourceDesc,
    first: u32,
    count: u32,
    base: u64,
    layouts: *mut PlacedFootprint,
    rows: *mut u32,
    row_sizes: *mut u64,
    total: *mut u64,
) {
    const PLACEMENT: u64 = 512;
    // SAFETY: the title passes a description (null is tolerated).
    let desc = unsafe { desc.as_ref() };
    let mut end = base;
    let mut known = desc.is_some();
    for i in 0..count {
        let one = desc.and_then(|d| subresource(d, first + i));
        let (layout, row_count, row_size) = match one {
            Some((mut footprint, row_count, row_size)) => {
                footprint.offset = end.next_multiple_of(PLACEMENT);
                let pitch = u64::from(footprint.row_pitch);
                let lines = u64::from(row_count) * u64::from(footprint.depth);
                end = footprint.offset + pitch * lines.saturating_sub(1) + row_size;
                (footprint, row_count, row_size)
            }
            None => {
                known = false;
                (NO_FOOTPRINT, u32::MAX, u64::MAX)
            }
        };
        let i = i as usize;
        // SAFETY: each output is null or holds `count` entries.
        unsafe {
            if !layouts.is_null() {
                layouts.add(i).write(layout);
            }
            if !rows.is_null() {
                rows.add(i).write(row_count);
            }
            if !row_sizes.is_null() {
                row_sizes.add(i).write(row_size);
            }
        }
    }
    if !known {
        tracing::warn!(?desc, "no footprint for this resource");
    }
    if !total.is_null() {
        // SAFETY: checked non-null.
        unsafe { total.write(if known { end - base } else { u64::MAX }) };
    }
}

/// Device method: the footprint of resources placed one after another.
/// Returns a 16-byte value, through a hidden first argument. Invalid input
/// gives the documented size `u64::MAX`.
pub(crate) extern "system" fn allocation_info(
    out: *mut AllocationInfo,
    _: This,
    _visible: u32,
    count: u32,
    descs: *const ResourceDesc,
) -> *mut AllocationInfo {
    let descs = if descs.is_null() {
        &[][..]
    } else {
        // SAFETY: the title passes `count` descriptions.
        unsafe { std::slice::from_raw_parts(descs, count as usize) }
    };
    // SAFETY: the caller passes room for the value.
    unsafe { out.write(place(descs.iter(), |_, _| ())) };
    out
}

/// The newer description: the plain one, then a sampler feedback mip
/// region (width, height, depth).
#[repr(C)]
pub(crate) struct ResourceDesc1 {
    desc: ResourceDesc,
    mip_region: [u32; 3],
}
const _: () = assert!(size_of::<ResourceDesc1>() == 72);

/// Where one resource goes in [`allocation_info2`]'s placement.
#[repr(C)]
pub(crate) struct AllocationInfo1 {
    offset: u64,
    alignment: u64,
    size: u64,
}

/// Device method 0x360 (`ID3D12Device8`): [`allocation_info`] for newer
/// descriptions, also telling where each resource goes.
pub(crate) extern "system" fn allocation_info2(
    out: *mut AllocationInfo,
    _: This,
    _visible: u32,
    count: u32,
    descs: *const ResourceDesc1,
    each: *mut AllocationInfo1,
) -> *mut AllocationInfo {
    let descs = if descs.is_null() {
        &[][..]
    } else {
        // SAFETY: the title passes `count` descriptions.
        unsafe { std::slice::from_raw_parts(descs, count as usize) }
    };
    let total = place(descs.iter().map(|d| &d.desc), |i, info| {
        if !each.is_null() {
            // SAFETY: the title passes room for `count` entries.
            unsafe { each.add(i).write(info) };
        }
    });
    // SAFETY: the caller passes room for the value.
    unsafe { out.write(total) };
    out
}

/// Resources placed one after another: the whole footprint, telling `each`
/// where every resource goes. Unknown sizes give the documented `u64::MAX`.
fn place<'a>(
    descs: impl Iterator<Item = &'a ResourceDesc>,
    mut each: impl FnMut(usize, AllocationInfo1),
) -> AllocationInfo {
    let mut total = AllocationInfo {
        size: 0,
        alignment: KIB_64,
    };
    for (i, desc) in descs.enumerate() {
        let Some(one) = footprint(desc) else {
            tracing::warn!(?desc, "no size for this resource");
            each(
                i,
                AllocationInfo1 {
                    offset: u64::MAX,
                    alignment: 0,
                    size: u64::MAX,
                },
            );
            return AllocationInfo {
                size: u64::MAX,
                alignment: 0,
            };
        };
        tracing::debug!(?desc, ?one, "allocation info");
        let offset = total.size.next_multiple_of(one.alignment);
        each(
            i,
            AllocationInfo1 {
                offset,
                alignment: one.alignment,
                size: one.size,
            },
        );
        total.size = offset + one.size;
        total.alignment = total.alignment.max(one.alignment);
    }
    total
}

/// Zeroed host memory standing in for GPU memory.
pub(crate) struct Memory {
    ptr: NonNull<u8>,
    layout: Layout,
    /// Set when the memory came from the title's allocator, with these
    /// allocation attributes (it goes back there).
    attributes: Option<u64>,
    /// Set when the memory is a `VirtualAlloc` allocation made with write
    /// watching ([`Memory::new`], `VICTORY_TITLE_HEAP_WATCH`): it goes back
    /// with `VirtualFree`, not to the global allocator.
    virtual_watched: bool,
}

pub type AllocRoutine = extern "system" fn(usize, u64) -> *mut c_void;
pub type FreeRoutine = extern "system" fn(*mut c_void, u64);
static ALLOCATOR: std::sync::OnceLock<(AllocRoutine, FreeRoutine)> = std::sync::OnceLock::new();

/// Committed resources get their memory from the title's general
/// allocator, as on the console, where titles hook it to place graphics
/// memory themselves (the title places its back buffers in a
/// block of its own and checks they fill it). The runtime names it before
/// the device exists.
pub fn use_allocator(alloc: AllocRoutine, free: FreeRoutine) {
    let _ = ALLOCATOR.set((alloc, free));
}

/// Maps the physical pages behind one range of title memory at another,
/// `(to, from, len)` (`from` 0 unmaps); false on failure.
pub type PageMapper = fn(u64, u64, u64) -> bool;
static PAGE_MAPPER: std::sync::OnceLock<PageMapper> = std::sync::OnceLock::new();

/// The runtime's page mapper, for the queues' page-mapping copies (memory
/// is the runtime's, as on the console).
pub fn use_page_mapper(mapper: PageMapper) {
    let _ = PAGE_MAPPER.set(mapper);
}

/// As a [`PageMapper`], many at once: `(to, from, len)` each, in order.
pub type ManyPageMapper = fn(&[(u64, u64, u64)]) -> bool;

static MANY_PAGE_MAPPER: std::sync::OnceLock<ManyPageMapper> = std::sync::OnceLock::new();

/// The runtime's mapper of many pages at once.
pub fn use_many_page_mapper(mapper: ManyPageMapper) {
    let _ = MANY_PAGE_MAPPER.set(mapper);
}

pub(crate) fn many_page_mapper() -> Option<ManyPageMapper> {
    MANY_PAGE_MAPPER.get().copied()
}

pub(crate) fn page_mapper() -> Option<PageMapper> {
    PAGE_MAPPER.get().copied()
}

/// Allocation attributes for resource memory: allocator id, page size
/// (bits 14-15: 2 MiB pages from 2 MiB up, else 64 KiB), alignment as log2
/// (bits 24-28) and memory type (bits 29-32: write-combined for GPU-only
/// heaps, cacheable for the CPU-visible ones).
/// shortcut: the console runtime's own choice of page size and memory type
/// is not known; this is the plausible one.
fn resource_attributes(info: AllocationInfo, heap_kind: u32) -> u64 {
    const GRAPHICS_ALLOCATOR: u64 = 128 << 16;
    const DEFAULT_HEAP: u32 = 1;
    let pages: u64 = if info.size >= 2 << 20 { 2 } else { 1 };
    let memory: u64 = if heap_kind == DEFAULT_HEAP { 1 } else { 3 };
    let alignment = u64::from(info.alignment.max(1).trailing_zeros());
    GRAPHICS_ALLOCATOR | pages << 14 | alignment << 24 | memory << 29
}

impl Memory {
    /// A committed resource's memory: from the title's allocator if the
    /// runtime named one, else our own; zeroed either way.
    fn for_resource(info: AllocationInfo, heap_kind: u32) -> Result<Memory, HRESULT> {
        let Some(&(alloc, _)) = ALLOCATOR.get() else {
            return Memory::new(info);
        };
        let layout = Layout::from_size_align(info.size as usize, info.alignment as usize)
            .map_err(|_| E_INVALIDARG)?;
        let attributes = resource_attributes(info, heap_kind);
        let ptr =
            NonNull::new(alloc(layout.size(), attributes).cast::<u8>()).ok_or(E_OUTOFMEMORY)?;
        // SAFETY: the allocator handed out `size` bytes.
        unsafe { ptr.as_ptr().write_bytes(0, layout.size()) };
        Ok(Memory {
            ptr,
            layout,
            attributes: Some(attributes),
            virtual_watched: false,
        })
    }

    /// `size` zeroed bytes aligned to `alignment` (a power of two).
    pub(crate) fn zeroed(size: u64, alignment: u64) -> Result<Memory, HRESULT> {
        Memory::new(AllocationInfo { size, alignment })
    }

    pub(crate) fn address(&self) -> u64 {
        self.ptr.as_ptr() as u64
    }

    fn new(info: AllocationInfo) -> Result<Memory, HRESULT> {
        let layout = usize::try_from(info.size)
            .ok()
            .zip(usize::try_from(info.alignment).ok())
            .filter(|&(size, _)| size > 0)
            .and_then(|(size, align)| Layout::from_size_align(size, align).ok())
            .ok_or(E_INVALIDARG)?;
        // Big memory is write-watched, so that what the title wrote since a
        // host copy took it is known and only those pages are copied: the
        // a buffer of tens of megabytes that came from `alloc_zeroed` was
        // "unwatched" and was copied whole twice a frame, and the per-block
        // digest refuses it because
        // shaders write it. `VirtualAlloc` zeroes what it gives; its
        // allocations lie on 64 KiB, so no more alignment than that is asked
        // of it.
        if let Some(ptr) = watched_memory(layout) {
            if info.size >= 32 << 20 {
                tracing::info!(mb = info.size >> 20, at = ?ptr, "memory for the title from our allocator (write-watched)");
            }
            return Ok(Memory {
                ptr,
                layout,
                attributes: None,
                virtual_watched: true,
            });
        }
        // SAFETY: the size is non-zero.
        let ptr = NonNull::new(unsafe { alloc_zeroed(layout) }).ok_or(E_OUTOFMEMORY)?;
        // The big ones are named in the log: the memory line counts our own
        // allocations by size, and these are the title's memory given out
        // of ours (a heap it made through the device, a descriptor heap).
        if info.size >= 32 << 20 {
            tracing::info!(mb = info.size >> 20, at = ?ptr, "memory for the title from our allocator (a heap or a descriptor heap)");
        }
        Ok(Memory {
            ptr,
            layout,
            attributes: None,
            virtual_watched: false,
        })
    }
}

/// The least size that gets write-watched memory ([`watched_memory`]).
const WATCHED_FROM: usize = 1 << 20;

/// Whether memory of a title's own (not from the title's allocator) is
/// allocated write-watched when it is big (`VICTORY_TITLE_HEAP_WATCH`, on
/// unless `0`, read once; `0`: `alloc_zeroed`, as before).
fn title_heap_watch() -> bool {
    crate::toggled!("VICTORY_TITLE_HEAP_WATCH")
}

/// Whether memory of `layout` is allocated write-watched.
fn watched_size(layout: Layout) -> bool {
    layout.size() >= WATCHED_FROM && layout.align() <= crate::watch::PAGE
}

/// `layout`'s bytes, zeroed, committed with write watching (`GetWriteWatch`
/// answers for them), if it is big enough ([`watched_size`]) and the switch
/// is on. The allocation is a multiple of 64 KiB long: the watch asks about
/// whole 64 KiB pages, and the kernel refuses a range that leaves the
/// allocation.
fn watched_memory(layout: Layout) -> Option<NonNull<u8>> {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
    const MEM_WRITE_WATCH: u32 = 0x20_0000;
    if !watched_size(layout) || !title_heap_watch() {
        return None;
    }
    let size = layout.size().checked_next_multiple_of(crate::watch::PAGE)?;
    // SAFETY: a plain allocation, released in `drop`.
    let at = unsafe {
        VirtualAlloc(std::ptr::null(), size, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE)
    };
    NonNull::new(at.cast::<u8>())
}

impl Drop for Memory {
    fn drop(&mut self) {
        if self.virtual_watched {
            use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
            // SAFETY: the base of the allocation `watched_memory` made.
            unsafe { VirtualFree(self.ptr.as_ptr().cast(), 0, MEM_RELEASE) };
            return;
        }
        match (self.attributes, ALLOCATOR.get()) {
            (Some(attributes), Some(&(_, free))) => free(self.ptr.as_ptr().cast(), attributes),
            // SAFETY: allocated in `new` with this layout.
            _ => unsafe { dealloc(self.ptr.as_ptr(), self.layout) },
        }
    }
}

#[repr(C)]
struct Heap {
    object: Object,
    desc: HeapDesc,
    memory: Memory,
}

pub(crate) extern "system" fn create_heap(
    _: This,
    desc: *const HeapDesc,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    tracing::debug!(
        size = desc.size,
        kind = desc.properties.kind,
        flags = format_args!("{:#x}", desc.flags),
        "heap"
    );
    if desc.alignment != 0 && !desc.alignment.is_power_of_two() {
        return E_INVALIDARG;
    }
    let info = AllocationInfo {
        size: desc.size,
        alignment: desc.alignment.max(KIB_64),
    };
    let memory = match Memory::new(info) {
        Ok(memory) => memory,
        Err(hr) => return hr,
    };
    let heap = Box::new(Heap {
        object: Object::new(&HEAP_FUNCTIONS),
        desc,
        memory,
    });
    object::hand_out(HEAP, heap, iid, &HEAP_IIDS, out)
}

extern "system" fn heap_query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(HEAP, iid, object::is(iid, &HEAP_IIDS).then_some(this), out)
}
extern "system" fn heap_release(this: This) -> u32 {
    object::release_boxed::<Heap>(this)
}
extern "system" fn heap_desc(this: This, out: *mut HeapDesc) {
    if !out.is_null() {
        // SAFETY: `this` is a heap; the title passes room for a description.
        unsafe { out.write((*this.cast::<Heap>()).desc) };
    }
}

/// A resource's memory: its own, or a range of a heap it holds a reference
/// to.
enum Backing {
    Committed(Memory),
    Placed {
        heap: This,
        offset: u64,
    },
    /// Memory the title mapped itself, at this address.
    Address(*mut u8),
}

impl Drop for Backing {
    fn drop(&mut self) {
        if let Backing::Placed { heap, .. } = *self {
            heap_release(heap);
        }
    }
}

#[repr(C)]
struct Resource {
    object: Object,
    desc: ResourceDesc,
    heap: HeapProperties,
    heap_flags: u32,
    backing: Backing,
    /// Textures also live on the host GPU; buffers do not
    /// yet. With, for a depth texture the title also writes as unordered
    /// access, the colour texture shaders write instead ([`Twin`]).
    #[cfg(windows)]
    host: Hosted,
}

/// A title texture's host copy and the state it is in. The title's barriers
/// name console states (looser than the host's), so we transition from this
/// one. Lists that use the texture hold it until they are reset: the title
/// may release a resource whose commands it recorded but not yet submitted
/// (console memory outlives the object; a host texture must outlive the
/// lists that name it).
#[cfg(windows)]
pub(crate) struct HostTexture {
    pub(crate) texture: windows::Win32::Graphics::Direct3D12::ID3D12Resource,
    pub(crate) state: std::sync::atomic::AtomicU32,
    /// The bytes the host gives it.
    bytes: u64,
    /// Whether its contents come from title memory ([`fills`]): counted in
    /// [`KINDS`] while it lives.
    filled: bool,
    /// Made without memory of its own (`crate::tiles`): the tiles its
    /// levels have been given. Its `bytes` are 0: what it holds of the card
    /// is counted there.
    pub(crate) tiled: Option<crate::tiles::Tiled>,
    /// The title's level its level 0 is ([`held_only`]): larger levels have
    /// no place in it.
    pub(crate) base: u32,
    /// Made of only its texture's smallest levels while the card had no room
    /// for more ([`tail_only`]): replaced by one with the levels its views
    /// show once a pass with room asks for them ([`want_level`]).
    pub(crate) tail: bool,
    /// Tells host copies apart that one texture had one after the other.
    pub(crate) serial: u64,
    /// Made in place of one with fewer levels: the slots that show that one
    /// are made again once this one was filled whole.
    pub(crate) grown: std::sync::atomic::AtomicBool,
    /// What the last submitted upload put in it.
    pub(crate) holds: std::sync::Arc<Holds>,
    /// Who makes its first upload (`crate::texture::ahead`).
    pub(crate) ahead: std::sync::atomic::AtomicU8,
    /// The texture's flags (asked once: the runtime takes 100 ns to say, and
    /// every texture of every draw is asked about).
    flags: u32,
    /// Cut from a block of textures (`crate::pool`): memory another texture
    /// may have held, whose bytes it shows where nothing was uploaded.
    pub(crate) pooled: bool,
    /// The title's first level of the lowest upload into it a list recorded
    /// ([`note_recorded`]; `u32::MAX`: none yet): it holds nothing written
    /// below that level, and nothing at all without one.
    pub(crate) recorded: std::sync::atomic::AtomicU32,
    /// The card wrote it: a copy on the card was recorded into it (a view
    /// the title placed in the memory of a texture the card renders to, in
    /// another format, `crate::texture::alias`). Its texels are the card's,
    /// not title memory's: no upload ever goes into it.
    pub(crate) card_wrote: std::sync::atomic::AtomicBool,
}

/// An upload into `copy` of the title's levels from `first` on was recorded.
#[cfg(windows)]
pub(crate) fn note_recorded(copy: &HostTexture, first: u32) {
    copy.recorded.fetch_min(first, std::sync::atomic::Ordering::Relaxed);
}

/// A copy on the card into the host copy of title texture `this` was
/// recorded ([`HostTexture::card_wrote`]).
#[cfg(windows)]
pub(crate) fn note_card_wrote(this: This) {
    if let Some(copy) = made_copy(this) {
        copy.card_wrote.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// What a host copy holds (its contents), for whether a slot may show it
/// ([`show_written`], [`holds_of`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Contents {
    /// Memory of its own (zeros where nothing was written), or levels made
    /// level by level (`crate::tiles`, which zero what they give): never
    /// another texture's bytes.
    OwnMemory,
    /// A texture the card renders to or writes (target, depth, unordered
    /// access): its contents are the card's alone.
    CardTexture,
    /// The card wrote it (a copy on the card, [`HostTexture::card_wrote`])
    /// or the host copied into it (`crate::texture::host_wrote`).
    CardWrote,
    /// An upload into it was recorded: it holds the title's bytes from that
    /// upload's first level on.
    Uploaded,
    /// Cut from a block, filled from title memory, and nothing went into it
    /// yet: what it shows is what the block held before.
    Nothing,
}

/// [`Contents`] of a copy: `pooled` cut from a block; `filled` its contents
/// come from title memory; `tiled` made level by level; `recorded` the first
/// level of the lowest upload recorded into it (`u32::MAX`: none);
/// `card_wrote` / `host_wrote` the card or the host wrote it. Only
/// [`Contents::Nothing`] is ever given a stand-in. Before the copies the card
/// writes were told apart, a view placed in a target's memory in another
/// format (written by a copy on the card, never uploaded) counted as one
/// nothing went into, and was shown as the neutral texture wherever no
/// upload followed: a full-screen pass read it as grey, a veil over the
/// whole picture.
#[cfg(any(windows, test))]
pub(crate) fn holds_of(
    pooled: bool,
    filled: bool,
    tiled: bool,
    recorded: u32,
    card_wrote: bool,
    host_wrote: impl FnOnce() -> bool,
) -> Contents {
    if !filled {
        Contents::CardTexture
    } else if !pooled || tiled {
        Contents::OwnMemory
    } else if card_wrote {
        Contents::CardWrote
    } else if recorded != u32::MAX {
        Contents::Uploaded
    } else if host_wrote() {
        Contents::CardWrote
    } else {
        Contents::Nothing
    }
}

/// [`holds_of`] for host copy `copy` of title texture `this`.
#[cfg(windows)]
pub(crate) fn holds(this: This, copy: &HostTexture) -> Contents {
    use std::sync::atomic::Ordering::Relaxed;
    holds_of(
        copy.pooled,
        copy.filled,
        copy.tiled.is_some(),
        copy.recorded.load(Relaxed),
        copy.card_wrote.load(Relaxed),
        || crate::texture::host_wrote(this as usize),
    )
}

/// Whether a slot made from host copy `copy` of title texture `this` could
/// show bytes nothing wrote ([`show_written`]): [`Contents::Nothing`]. A copy
/// the card wrote, one of a texture the card writes, and one that holds an
/// upload are always shown as they are.
#[cfg(windows)]
pub(crate) fn unwritten(this: This, copy: &HostTexture) -> bool {
    holds(this, copy) == Contents::Nothing
}

/// Since the start: slots that would have been given a stand-in for a copy
/// cut from a block before the card's writes were told apart, left on their
/// copy because the card wrote it ([`Contents::CardWrote`]); slots of the
/// texture pass whose upload was held back, left on their copy because it
/// held an earlier upload ([`Contents::Uploaded`]).
#[cfg(windows)]
pub(crate) static LEFT_ON_COPY: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Counts a slot left on its copy (`holds`) where a stand-in was asked
/// about: [`LEFT_ON_COPY`]'s kinds; nothing for the others.
#[cfg(windows)]
pub(crate) fn left_on_copy(holds: Contents) {
    let at = match holds {
        Contents::CardWrote => 0,
        Contents::Uploaded => 1,
        _ => return,
    };
    LEFT_ON_COPY[at].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Whether nothing is shown that was never written (`VICTORY_SHOW_WRITTEN`,
/// on unless `0`: as before). A host copy cut from a block of textures holds
/// what the block held before (another texture's bytes) until its upload
/// has run, and below the first level an upload wrote. With it:
/// - a slot made where no upload follows in the same list (a slot flipped
///   after an upload ran, `descriptor/host.rs` `texture_slots_flip`; a slot
///   made again for a copy of more levels, `texture_views_remade`) does not
///   replace its texture's copy by a new one ([`want_level`]), shows a
///   stand-in for a copy nothing went into, and the levels an upload
///   went into for one whose view starts below them ([`NoUploadFollows`]);
///   the slot is made again by the next texture pass;
/// - a slot of the texture pass made before its upload, and a draw's table
///   slot, whose upload was held back (the commit guard), shows a stand-in;
/// - the first upload into a copy cut from a block writes zeros into the
///   levels below the first one it uploads (`crate::texture` `upload_from`).
///
/// Only a copy that holds nothing ([`Contents::Nothing`], [`holds_of`]) is
/// hidden so; the stand-in is a null view, which reads zeros, unless
/// `VICTORY_STAND_IN=neutral` (`descriptor/host.rs` `stand_in_for`).
///
/// Of a run on a card of 8 GB, textures showed dense coloured noise in rows: a
/// copy made with fewer levels for the band ([`detail`]) and uploaded was
/// shown by its slot once that upload had run, and showing it asked for a
/// copy of every level (the band's bound is the pass's alone): the uploaded
/// copy was let go and its slot showed a new one, from a block another
/// texture had held, that no upload went into until the title wrote the
/// texture again.
#[cfg(windows)]
pub(crate) fn show_written() -> bool {
    crate::toggled!("VICTORY_SHOW_WRITTEN")
}

#[cfg(windows)]
thread_local! {
    /// The thread makes slots that no upload follows in the same list
    /// ([`NoUploadFollows`]).
    static NO_UPLOAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// A slot made while [`NO_UPLOAD`] was set was given a stand-in
    /// ([`stood_in`]).
    static STOOD_IN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The title texture whose host copy a view was last made of on this
    /// thread while no upload had gone into it ([`made_unwritten`]; 0: none).
    static MADE_UNWRITTEN: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A view of title texture `this` was made of a host copy no upload went
/// into yet ([`unwritten`]): its caller's upload may be held back.
#[cfg(windows)]
pub(crate) fn made_unwritten(this: This) {
    MADE_UNWRITTEN.set(this as usize);
}

/// Whether [`made_unwritten`] named a texture on this thread since it was
/// last taken ([`take_made_unwritten`]).
#[cfg(windows)]
pub(crate) fn made_unwritten_pending() -> bool {
    MADE_UNWRITTEN.get() != 0
}

/// The texture [`made_unwritten`] last named on this thread, if its copy is
/// still one no upload went into; taken.
#[cfg(windows)]
pub(crate) fn take_made_unwritten() -> Option<This> {
    let this = MADE_UNWRITTEN.replace(0) as This;
    (!this.is_null() && made_copy(this).is_some_and(|copy| unwritten(this, &copy))).then_some(this)
}

/// Sets [`NO_UPLOAD`] on this thread while it lives ([`show_written`] on),
/// and gives it back what it was then.
#[cfg(windows)]
pub(crate) struct NoUploadFollows(bool);

#[cfg(windows)]
impl NoUploadFollows {
    pub(crate) fn new() -> NoUploadFollows {
        STOOD_IN.set(false);
        NoUploadFollows(NO_UPLOAD.replace(show_written()))
    }
}

#[cfg(windows)]
impl Drop for NoUploadFollows {
    fn drop(&mut self) {
        NO_UPLOAD.set(self.0);
    }
}

/// Whether slots made now have no upload after them ([`NoUploadFollows`]).
#[cfg(windows)]
pub(crate) fn no_upload_follows() -> bool {
    NO_UPLOAD.get()
}

/// A slot made under [`NoUploadFollows`] was given a stand-in or
/// fewer levels than its view: it is made again later.
#[cfg(windows)]
pub(crate) fn stand_in() {
    STOOD_IN.set(true);
}

/// Whether the slot made last under [`NoUploadFollows`] was given a stand-in
/// ([`stand_in`]); taken.
#[cfg(windows)]
pub(crate) fn stood_in() -> bool {
    STOOD_IN.replace(false)
}

/// Since the start ([`show_written`]): slots made where no upload follows
/// that showed a stand-in for a copy nothing went into; such
/// slots shown from the first level an upload wrote; copies kept that
/// showing a slot would have replaced by a new one ([`want_level`]); slots
/// of the texture pass, and draws' table slots, whose upload was held back
/// and that showed a stand-in; first uploads that wrote zeros
/// below their first level.
#[cfg(windows)]
pub(crate) static UNWRITTEN: [std::sync::atomic::AtomicU64; 6] = [const { std::sync::atomic::AtomicU64::new(0) }; 6];

/// Says, at most once a second and when they changed, the counts of
/// [`UNWRITTEN`].
#[cfg(windows)]
pub(crate) fn unwritten_line() {
    use std::sync::atomic::Ordering::Relaxed;
    static SAID: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];
    let now = crate::host::coarse_ms();
    let last = SAID[0].load(Relaxed);
    let n = UNWRITTEN.each_ref().map(|n| n.load(Relaxed));
    let sum: u64 = n.iter().sum::<u64>() + LEFT_ON_COPY.iter().map(|n| n.load(Relaxed)).sum::<u64>();
    if sum == SAID[1].load(Relaxed)
        || now < last.saturating_add(1000)
        || SAID[0].compare_exchange(last, now.max(1), Relaxed, Relaxed).is_err()
    {
        return;
    }
    SAID[1].store(sum, Relaxed);
    let left = LEFT_ON_COPY.each_ref().map(|n| n.load(Relaxed));
    tracing::info!(
        "host copies not shown before their bytes (VICTORY_SHOW_WRITTEN, stand-in VICTORY_STAND_IN), since the start: \
         slots made after an upload ran that showed a stand-in for a copy nothing went into {}, shown from the first \
         level uploaded {}; copies kept that a slot's view would have replaced by an empty one {}; slots of the texture \
         pass {} and draws' table slots {} that showed a stand-in as their upload was held back; first uploads that \
         wrote zeros below their first level {}; left on their copy: as the card wrote it {}, as it held an earlier \
         upload {}",
        n[0],
        n[1],
        n[2],
        n[3],
        n[4],
        n[5],
        left[0],
        left[1]
    );
}

/// What a host copy was filled with last: the first level of it and the
/// hash of the title memory it was read from.
#[cfg(windows)]
pub(crate) type Holds = std::sync::Mutex<Option<(u32, crate::texture::Digest)>>;

/// [`HostTexture::serial`] of the last one made.
#[cfg(windows)]
static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// What a title texture has on the host: its host copy and, for a depth
/// texture shaders also write, the colour twin.
#[cfg(windows)]
type Made = (std::sync::Arc<HostTexture>, Option<std::sync::Arc<Twin>>);

/// A title texture's host side. The host copy is made when something first
/// uses the texture, not when the title creates it: a title may create
/// thousands of textures (gigabytes on the host) before its first 3D frame,
/// of which a frame's lists name some hundreds. A title texture is memory
/// the title has either way; its host
/// copy is the card's.
#[cfg(windows)]
struct Hosted {
    /// The host copy (and twin) once made. Taken out again only by
    /// [`let_go_idle`] and when the title releases the resource.
    made: std::sync::Mutex<Option<Made>>,
    /// The state and clear value the title created the texture with.
    state: u32,
    clear: Option<[u32; 5]>,
    /// When the host copy was last asked for (`crate::host::moment`),
    /// written under `made`'s lock.
    used: std::sync::atomic::AtomicU64,
    /// The host refused to make it: not asked again before this
    /// `crate::host::coarse_ms` ([`texture_retry`]; `u64::MAX`: for good, 0:
    /// not refused). Written under `made`'s lock.
    refused_until: std::sync::atomic::AtomicU64,
    /// Refusals for want of memory in a row, at most [`RETRY_DOUBLINGS`]
    /// ([`refuse`]). Written under `made`'s lock.
    refusals: std::sync::atomic::AtomicU8,
    /// Its host copy was let go as idle once.
    gone: std::sync::atomic::AtomicBool,
    /// A texture of [`PLACED_TEXTURES`].
    placed: std::sync::atomic::AtomicBool,
    /// The lowest level a view asked for since it had no host copy
    /// ([`want_level`]; `u32::MAX`: none said).
    wanted: std::sync::atomic::AtomicU32,
    /// The next host copy takes the place of one with fewer levels.
    grown: std::sync::atomic::AtomicBool,
    /// The residency class of the host copy (`crate::host::residency`),
    /// written under `made`'s lock: [`NOT_OWN`] for one without memory of
    /// its own (a piece of a block, or tiles), or none.
    residency: std::sync::atomic::AtomicU8,
    /// The level the next host copy begins at at the earliest, as the sweep
    /// cut the last one down to it ([`cut_idle`]; 0: none). Taken by that
    /// copy's making.
    cut: std::sync::atomic::AtomicU32,
}

/// [`Hosted::residency`] of a host copy whose memory is a heap's (its
/// residency is the heap's), or of none.
#[cfg(windows)]
const NOT_OWN: u8 = u8::MAX;

/// Whether a texture the title places in memory is read, kept and uploaded
/// from the first level its views show on (`VICTORY_HELD_LEVELS=0`: whole,
/// as before). Such a title describes every level of a texture and views it
/// from the first one it holds; the memory of the larger ones is the next
/// texture's, or nothing. Of recorded frames the levels some slot shows are
/// a fifth of such host copies' bytes, and most of what was found "changed"
/// and uploaded again was other textures' bytes.
#[cfg(windows)]
pub(crate) fn held_only() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("VICTORY_HELD_LEVELS").is_none_or(|v| v != "0") && !crate::tiles::wanted()
    })
}

/// The first level a placed texture's view shows: of views of one 2D
/// texture (the others show whole textures). After victory-amd 4436f8f1.
pub(crate) fn first_mip(view: &[u32]) -> u32 {
    const TEXTURE_2D_VIEW: u32 = 4;
    match *view {
        [_, TEXTURE_2D_VIEW, _, _, first, ..] => first.min(15),
        _ => 0,
    }
}

/// The level from which on a texture of layout `layout` is read for a view
/// that starts at `first`: the smallest levels share a block and are read
/// together; 0 where the larger ones lie between the smaller ones.
pub(crate) fn read_from(layout: &victory_formats::texture::Layout, first: u32) -> u32 {
    if layout.slices != 1 || layout.mips.is_empty() {
        return 0;
    }
    let first = first.min(layout.first_tail_mip).min(layout.mips.len() as u32 - 1);
    if layout.bytes_from(first) < layout.size { first } else { 0 }
}

/// The level a host copy begins at for views that start at `first`: one the
/// host can have as a texture's level 0 ([`read_from`]; block-compressed
/// formats in whole blocks).
#[cfg(windows)]
fn base_level(desc: &ResourceDesc, first: u32) -> u32 {
    const TEXTURE_2D: u32 = 3;
    if first == 0
        || !fills(desc)
        || desc.dimension != TEXTURE_2D
        || desc.depth_or_array_size != 1
        || desc.mip_levels <= 1
        || desc.sample_count > 1
    {
        return 0;
    }
    let Some(t) = texture_layout(desc).filter(|t| t.planes.len() == 1) else {
        return 0;
    };
    let layout = &t.planes[0].layout;
    if layout.desc.swizzle.xor() {
        return 0;
    }
    let block = u64::from(layout.desc.block.max(1));
    let mut base = read_from(layout, first).min(u32::from(desc.mip_levels) - 1);
    let whole = |side: u64| side != 0 && side.is_multiple_of(block);
    while base > 0 && !(whole(desc.width >> base) && whole(u64::from(desc.height) >> base)) {
        base -= 1;
    }
    base
}

/// Whether the pass over textures picked by index (`draw.rs` `heap_textures`)
/// gives a texture the title placed, whose host copy would be new while the
/// card has no room for more ([`room`]), a host copy of only its smallest
/// levels, from the first of at most [`TAIL_SIDE`] texels a side on (a few
/// KB, made with no room test), and holds its slot until the card has room,
/// when the pass asks for the rest of its levels and the copy is replaced by
/// a whole one ([`want_level`]). `VICTORY_TEXTURE_TAIL=0`: no copy, the slot
/// waits, as before.
///
/// With the card without room, thousands of slots shaders pick by index were
/// due, the oldest written dozens of frames before, and each read zeros (or
/// showed another texture, [`crate::descriptor::host`]'s `keep_shown`) for
/// as long as it waited.
#[cfg(windows)]
pub(crate) fn tail_only() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_TAIL").is_none_or(|v| v != "0"))
}

/// The most texels a side of the largest level of a host copy of only a
/// texture's smallest levels ([`tail_only`]).
#[cfg(any(windows, test))]
const TAIL_SIDE: u64 = 64;

/// The first level of a texture `width` by `height` texels with `mips`
/// levels that has at most [`TAIL_SIDE`] texels a side (its last level if
/// none has).
#[cfg(any(windows, test))]
fn tail_level(width: u64, height: u64, mips: u32) -> u32 {
    side_level(width, height, mips, TAIL_SIDE)
}

/// The first level of a texture `width` by `height` texels with `mips`
/// levels that has at most `side` texels a side (its last level if none
/// has): [`tail_level`] for another side ([`detail`]).
#[cfg(any(windows, test))]
fn side_level(width: u64, height: u64, mips: u32, side: u64) -> u32 {
    let mut level = 0;
    while level + 1 < mips && (width >> level > side || height >> level > side) {
        level += 1;
    }
    level
}

/// [`tail_base`] for a host copy whose first level has at most `side`
/// texels a side ([`detail`]): the level such a copy begins at for views
/// that start at `first`, or 0 if none can begin later than the views do.
#[cfg(windows)]
fn side_base(desc: &ResourceDesc, first: u32, side: u64) -> u32 {
    let level = side_level(desc.width, u64::from(desc.height), u32::from(desc.mip_levels), side);
    base_level(desc, first.max(level))
}

/// How new host copies of textures picked by index are made while the card
/// fills ([`detail_for`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Detail {
    /// At every level the views show until the card has no room, then of
    /// their smallest levels only ([`tail_only`]), as before.
    Off,
    /// Of fewer levels the closer the card comes to the line of [`room`]
    /// ([`detail_side_as`]).
    Card,
    /// The same, and while the PC's commit is critical
    /// (`crate::host::commit::critical`) at most [`PC_SHORT_SIDE`] texels a
    /// side.
    CardAndPc,
}

/// [`Detail`] from `VICTORY_TEXTURE_DETAIL` as set and the card's budget:
/// `0` off (as before); `card`, or any other value but `pc`, the card's band
/// only ([`Detail::Card`]); `pc` the band and the PC's commit
/// ([`Detail::CardAndPc`], what unset and any other value but `0` and `card`
/// were before); unset, the card's band on a card whose budget is 8 GiB or
/// less, off on larger ones (as before).
///
/// On a card of 8 GB the copies the title's textures want came to more than
/// the card: every host copy was made whole until the card reached its line,
/// and from then on new ones were refused or made of their 64-texel tails
/// alone ([`tail_only`]), so a scene showed some textures sharp and most
/// blurred to nothing or missing. Leaving out a texture's largest level takes
/// three quarters of its bytes; two, fifteen sixteenths. Made with fewer of
/// them as the card fills, many more textures have a copy before it is full,
/// and what reaches the line later has more than its tail.
///
/// The PC's side is no longer the default: a host copy is the card's memory,
/// and cutting it frees none of the PC's. On a PC of 16 GB whose free memory
/// stood near nothing for most of a run, with the card under its budget and a
/// gigabyte of our blocks of textures free, new copies were cut for the PC's
/// commit as well as for the card. The PC's memory has its own guard for what
/// stages a texture (`crate::host::commit`: a pass held to 16 MiB while
/// commit is critical), which stays.
#[cfg(any(windows, test))]
fn detail_for(set: Option<&str>, budget: Option<u64>) -> Detail {
    match set.map(str::trim) {
        Some("0") => Detail::Off,
        Some("pc") => Detail::CardAndPc,
        Some(_) => Detail::Card,
        None if budget.is_some_and(|b| b > 0 && b <= 8 << 30) => Detail::Card,
        None => Detail::Off,
    }
}

/// [`card_short_on`] for the switch's value `set` and a card of `budget`
/// bytes.
#[cfg(any(windows, test))]
fn card_short_for(set: Option<&str>, budget: Option<u64>) -> bool {
    match set.map(str::trim) {
        Some("0") => false,
        Some(_) => true,
        None => budget.is_some_and(|b| b > 0 && b <= 8 << 30),
    }
}

/// Whether the detail cut ([`detail`]) and the texture queue's "no room"
/// ([`room`]) go by the card being short alone (`VICTORY_CARD_SHORT`; unset:
/// on a card whose budget is 8 GiB or less, off on larger ones; `0`: off, as
/// before; any other value: on). With it:
/// - neither happens while the system's own count of our use of the card,
///   without the room kept for what is not made yet, is under the bottom of
///   the band below its line ([`CardFill::short`]): the card's use well under
///   its budget is no shortage, whatever is kept;
/// - a texture whose whole copy fits the room our blocks of textures have
///   free takes none of the card's memory more, and is made whole however
///   full the card counts (`draw.rs` `heap_textures`, [`fits_pool`]).
///
/// On a card of 8 GB with the card's use under its budget and a gigabyte of
/// the blocks' room free, new copies were cut to 128 texels a side and the
/// card said to have no room hundreds of times: the line by the system's count
/// counts the blocks' free room as used, and the room kept for render targets
/// and buffer copies not made yet as used too.
#[cfg(windows)]
pub(crate) fn card_short_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let set = std::env::var("VICTORY_CARD_SHORT").ok();
        card_short_for(set.as_deref(), crate::host::host().and_then(|h| h.video_memory()).map(|m| m[0]))
    })
}

/// Why new host copies are cut or refused ([`CardFill::room_as`],
/// [`detail_side_as`]): for the queue's own measure past its line or in its
/// band, for the system's count past the line of the headroom or in its
/// band, for the PC's commit ([`Detail::CardAndPc`] only). Kept as an index
/// of [`DETAIL_WHY`] and [`NO_ROOM_WHY`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Why {
    None = 0,
    Queue = 1,
    Whole = 2,
    Pc = 3,
}

/// Whether a texture whose whole host copy takes `need` bytes fits the room
/// our blocks of textures have free: it fits the largest free range of one
/// (`hole`), and the bytes free in all (`free`) that the copies let through
/// so far this pass left.
#[cfg(any(windows, test))]
pub(crate) fn fits_pool(need: u64, hole: u64, free: u64) -> bool {
    need > 0 && need <= hole && need <= free
}

/// [`detail_for`] for this card, read once: `VICTORY_TEXTURE_DETAIL` `0`
/// off; `card`, or any other value but `pc`, the card's band alone (unset: so
/// on a card of 8 GiB or less, off on larger ones); `pc` the card's band and
/// the PC's commit, as unset was before on a card of 8 GiB or less.
#[cfg(windows)]
pub(crate) fn detail() -> Detail {
    static DETAIL: std::sync::OnceLock<Detail> = std::sync::OnceLock::new();
    *DETAIL.get_or_init(|| {
        let set = std::env::var("VICTORY_TEXTURE_DETAIL").ok();
        let budget = crate::host::host().and_then(|h| h.video_memory()).map(|m| m[0]);
        let detail = detail_for(set.as_deref(), budget);
        tracing::info!(
            ?detail,
            budget_mb = budget.unwrap_or(0) >> 20,
            headroom_pct = card_headroom(),
            cut = cut_on(),
            small_when_full_kb = small_when_full() >> 10,
            card_short = card_short_on(),
            "textures picked by index on a full card (VICTORY_TEXTURE_DETAIL, pc: for the PC's commit too; \
             VICTORY_CARD_HEADROOM_PCT, VICTORY_TEXTURE_CUT, VICTORY_SMALL_WHEN_FULL_KB, VICTORY_CARD_SHORT; each 0: \
             as before)"
        );
        detail
    })
}

/// The band below the lines of [`room`] in which new copies are made with
/// fewer levels ([`detail_side_as`]), in hundredths of the budget.
#[cfg(any(windows, test))]
const DETAIL_BAND_PCT: u64 = 10;

/// The most texels a side of a new copy's first level, from the bottom of
/// that band to its top: each step halves the side (a quarter of the bytes).
#[cfg(any(windows, test))]
const DETAIL_SIDES: [u64; 5] = [2048, 1024, 512, 256, 128];

/// The most texels a side of a new copy's first level while the PC's commit
/// is critical ([`Detail::CardAndPc`]).
#[cfg(any(windows, test))]
const PC_SHORT_SIDE: u64 = 1024;

/// Where the card stands for the texture queue: its budget and the system's
/// count of our use of it, what our blocks of textures have free, and the
/// room kept for what is not made yet (`crate::shadow::card_room_kept`,
/// [`targets_room_kept`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CardFill {
    pub(crate) budget: u64,
    pub(crate) used: u64,
    pub(crate) free_blocks: u64,
    pub(crate) kept: u64,
}

impl CardFill {
    /// The queue's own measure (the blocks' free bytes count as free: new
    /// copies take them first) and its line, `percent` of the budget.
    #[cfg(any(windows, test))]
    fn queue(&self, percent: u64) -> (u64, u64) {
        (self.used.saturating_sub(self.free_blocks) + self.kept, self.budget / 100 * percent)
    }

    /// The system's own count with what is kept, and its line: the budget
    /// less `headroom` hundredths of it (`None` without a headroom,
    /// [`card_headroom`]).
    #[cfg(any(windows, test))]
    fn whole(&self, headroom: u64) -> Option<(u64, u64)> {
        (headroom > 0).then(|| (self.used + self.kept, self.budget / 100 * (100 - headroom.min(100))))
    }

    /// Whether the queue may make more host copies: under its line, and
    /// with a headroom under that line too (as before [`card_short_for`]).
    #[cfg(test)]
    pub(crate) fn room(&self, percent: u64, headroom: u64) -> bool {
        self.room_as(percent, headroom, false).is_ok()
    }

    /// Whether the queue may make more host copies (under its line, and with a
    /// headroom under that line too), and with `card_short`
    /// ([`card_short_for`]) while the card is not short ([`CardFill::short`]);
    /// else why not.
    #[cfg(any(windows, test))]
    pub(crate) fn room_as(&self, percent: u64, headroom: u64, card_short: bool) -> Result<(), Why> {
        if card_short && !self.short(percent, headroom) {
            return Ok(());
        }
        let (queue, line) = self.queue(percent);
        if queue >= line {
            return Err(Why::Queue);
        }
        match self.whole(headroom) {
            Some((whole, line)) if whole >= line => Err(Why::Whole),
            _ => Ok(()),
        }
    }

    /// Whether the card is short ([`card_short_for`]): the system's own count
    /// of our use, without what is kept for later, has reached the bottom of
    /// the band of [`DETAIL_BAND_PCT`] below the line it is held under (the
    /// budget less the headroom, or the queue's line without one).
    #[cfg(any(windows, test))]
    pub(crate) fn short(&self, percent: u64, headroom: u64) -> bool {
        let line = if headroom > 0 { 100 - headroom.min(100) } else { percent.min(100) };
        let bottom = line.saturating_sub(DETAIL_BAND_PCT);
        self.budget > 0 && u128::from(self.used) * 100 >= u128::from(self.budget) * u128::from(bottom)
    }

    /// How far into the band of `band` bytes below the lines its use is: 0
    /// under the band, `band` at or past a line; and which line's band that
    /// is ([`Why::Queue`] when both are as deep).
    #[cfg(any(windows, test))]
    fn depth(&self, percent: u64, headroom: u64, band: u64) -> (u64, Why) {
        let into = |(used, line): (u64, u64)| used.saturating_add(band).saturating_sub(line).min(band);
        let queue = into(self.queue(percent));
        let whole = self.whole(headroom).map_or(0, into);
        if whole > queue { (whole, Why::Whole) } else { (queue, Why::Queue) }
    }
}

/// The most texels a side of the first level of a new host copy the texture
/// queue makes ([`detail`]): `u64::MAX` (no bound) under the band of
/// [`DETAIL_BAND_PCT`] below the lines of [`room`], then [`DETAIL_SIDES`]
/// step by step to the line, and [`PC_SHORT_SIDE`] at most while `pc_short`
/// ([`Detail::CardAndPc`]). Past the line no copy is made but the smallest
/// levels' ([`tail_only`]); what is there then is the pass's.
///
/// And why the side is bounded ([`Why::None`]: it is not); with
/// `card_short` ([`card_short_for`]) not bounded while the card is not short
/// ([`CardFill::short`]).
#[cfg(any(windows, test))]
fn detail_side_as(
    detail: Detail,
    fill: Option<CardFill>,
    percent: u64,
    headroom: u64,
    pc_short: bool,
    card_short: bool,
) -> (u64, Why) {
    if detail == Detail::Off {
        return (u64::MAX, Why::None);
    }
    let pc = if detail == Detail::CardAndPc && pc_short { (PC_SHORT_SIDE, Why::Pc) } else { (u64::MAX, Why::None) };
    let Some(fill) = fill.filter(|f| f.budget > 0) else {
        return pc;
    };
    if card_short && !fill.short(percent, headroom) {
        return pc;
    }
    let band = fill.budget / 100 * DETAIL_BAND_PCT;
    let (depth, why) = fill.depth(percent, headroom, band);
    if depth == 0 || band == 0 {
        return pc;
    }
    let steps = DETAIL_SIDES.len() as u64;
    let step = (u128::from(depth) * u128::from(steps) / u128::from(band)).min(u128::from(steps - 1)) as usize;
    if pc.0 < DETAIL_SIDES[step] { pc } else { (DETAIL_SIDES[step], why) }
}

/// [`detail_side_as`]'s side without the rule of [`card_short_for`], as before.
#[cfg(test)]
fn detail_side_in(detail: Detail, fill: Option<CardFill>, percent: u64, headroom: u64, pc_short: bool) -> u64 {
    detail_side_as(detail, fill, percent, headroom, pc_short, false).0
}

/// Where the card stands now ([`CardFill`]), from `memory`.
#[cfg(windows)]
fn card_fill(memory: Option<[u64; 4]>) -> Option<CardFill> {
    let [budget, used, ..] = memory?;
    Some(CardFill {
        budget,
        used,
        free_blocks: crate::pool::texture_room(),
        kept: crate::shadow::card_room_kept() + targets_room_kept(),
    })
}

/// [`detail_side_as`] for the card and the PC now.
#[cfg(windows)]
pub(crate) fn detail_side(memory: Option<[u64; 4]>) -> u64 {
    detail_side_why(memory).0
}

/// [`detail_side`] and why it is bounded ([`detail_side_as`]).
#[cfg(windows)]
pub(crate) fn detail_side_why(memory: Option<[u64; 4]>) -> (u64, Why) {
    let detail = detail();
    if detail == Detail::Off {
        return (u64::MAX, Why::None);
    }
    let pc_short = detail == Detail::CardAndPc && crate::host::commit::critical();
    detail_side_as(detail, card_fill(memory), room_percent(), card_headroom(), pc_short, card_short_on())
}

/// The share of the card's budget, in hundredths, kept free under what
/// [`room`] lets the texture queue fill by the system's own count of our use
/// (`VICTORY_CARD_HEADROOM_PCT`, 0 to 20; unset: 4 on a card whose budget is
/// 8 GiB or less, else 0; 0: none, as before).
///
/// The queue's own measure counts what our blocks of textures have free as
/// free, and that was hundreds of MB, up to about a gigabyte, of a card of
/// 8 GB (pieces given back rest before they are used again; a large texture
/// fits no hole left): with the queue at its line of nine tenths, the system counted our
/// use at or past the whole budget, by a few hundred MB. Past it the system
/// moves our memory out of the card, and making new memory resident waited
/// for that (over a second at times). With the headroom the queue stops before
/// that whichever count says so first.
#[cfg(any(windows, test))]
fn card_headroom_for(set: Option<&str>, budget: Option<u64>) -> u64 {
    match set.and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(pct) => pct.min(20),
        None if budget.is_some_and(|b| b > 0 && b <= 8 << 30) => 4,
        None => 0,
    }
}

/// [`card_headroom_for`] for this card, read once.
#[cfg(windows)]
fn card_headroom() -> u64 {
    static PCT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *PCT.get_or_init(|| {
        let set = std::env::var("VICTORY_CARD_HEADROOM_PCT").ok();
        card_headroom_for(set.as_deref(), crate::host::host().and_then(|h| h.video_memory()).map(|m| m[0]))
    })
}

/// The most bytes of a texture whose new host copy the texture queue makes
/// while the card has no room, whole, when no copy of its smallest levels
/// can be (`VICTORY_SMALL_WHEN_FULL_KB`; unset: 64 on a card whose budget is
/// 8 GiB or less, else 0; 0: none, they wait, as before). A copy of
/// [`tail_only`]'s needs a texture of more than 64 texels a side and of
/// more than one level: the small ones, and those of one level, waited for
/// room however few bytes they were, and read zeros meanwhile. Such copies
/// are as small as those of [`tail_only`]: of 64 KiB, a 128x128 texture of 4
/// bytes a texel.
#[cfg(any(windows, test))]
fn small_when_full_for(set: Option<&str>, budget: Option<u64>) -> u64 {
    match set.and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(kb) => kb.min(1024) << 10,
        None if budget.is_some_and(|b| b > 0 && b <= 8 << 30) => 64 << 10,
        None => 0,
    }
}

/// [`small_when_full_for`] for this card, read once.
#[cfg(windows)]
pub(crate) fn small_when_full() -> u64 {
    static BYTES: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *BYTES.get_or_init(|| {
        let set = std::env::var("VICTORY_SMALL_WHEN_FULL_KB").ok();
        small_when_full_for(set.as_deref(), crate::host::host().and_then(|h| h.video_memory()).map(|m| m[0]))
    })
}

/// The bytes of title memory texture object `this` takes (its console
/// layout's); `None` for a buffer.
#[cfg(windows)]
pub(crate) fn texture_bytes(this: This) -> Option<u64> {
    if this.is_null() {
        return None;
    }
    let r = resource(this);
    if r.desc.dimension == BUFFER {
        return None;
    }
    footprint(&r.desc).map(|f| f.size)
}

/// Whether the sweep cuts host copies that slots hold and nothing used for
/// a while down to the levels the card can afford ([`cut_idle`],
/// `VICTORY_TEXTURE_CUT`; unset: as [`detail`] is on; `0`: never, as
/// before).
///
/// A copy a slot shows is never let go (a shader may read it by index at any
/// time, and the slot keeps it), so on a full card the copies made whole
/// while there was room stayed whole, and the new ones waited. Cut down, a
/// copy of 4096 texels a side to 512 takes a sixty-fourth of its bytes, and
/// the slot shows it blurred instead of another texture missing.
#[cfg(windows)]
fn cut_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var("VICTORY_TEXTURE_CUT").ok().as_deref().map(str::trim) {
        Some("0") => false,
        Some(_) => true,
        None => detail_for(
            std::env::var("VICTORY_TEXTURE_DETAIL").ok().as_deref(),
            crate::host::host().and_then(|h| h.video_memory()).map(|m| m[0]),
        ) != Detail::Off,
    })
}

/// The fewest texels a side the sweep cuts a copy down to ([`cut_idle`]):
/// the band's last step is for new copies; one shown already keeps more.
#[cfg(any(windows, test))]
const CUT_SIDE_LEAST: u64 = 256;

/// The most copies one look of the sweep cuts down: each is uploaded again.
#[cfg(windows)]
const CUTS_A_LOOK: usize = 64;

/// The side the sweep cuts copies down to now (`u64::MAX`: none):
/// [`detail_side`], at least [`CUT_SIDE_LEAST`].
#[cfg(windows)]
fn cut_side(memory: Option<[u64; 4]>) -> u64 {
    if !cut_on() {
        return u64::MAX;
    }
    match detail_side(memory) {
        u64::MAX => u64::MAX,
        side => side.max(CUT_SIDE_LEAST),
    }
}

#[cfg(windows)]
thread_local! {
    /// While the pass over textures picked by index works on a slot with
    /// the card in the band below its line ([`detail_side`], [`DetailCap`]):
    /// the most texels a side the first level of a host copy of a placed
    /// texture made on this thread has (`u64::MAX`: no bound).
    static CAP: std::cell::Cell<u64> = const { std::cell::Cell::new(u64::MAX) };
    /// Why [`CAP`] bounds the side ([`Why`] as a number).
    static CAP_WHY: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

/// Sets [`CAP`] on this thread while it lives, and gives it back what it was
/// then.
#[cfg(windows)]
pub(crate) struct DetailCap(u64, u8);

#[cfg(windows)]
impl DetailCap {
    #[cfg(test)]
    pub(crate) fn new(side: u64) -> DetailCap {
        DetailCap::new_why(side, if side == u64::MAX { Why::None } else { Why::Queue })
    }

    /// [`DetailCap::new`], with why the side is bounded.
    pub(crate) fn new_why(side: u64, why: Why) -> DetailCap {
        DetailCap(CAP.replace(side), CAP_WHY.replace(why as u8))
    }
}

#[cfg(windows)]
impl Drop for DetailCap {
    fn drop(&mut self) {
        CAP.set(self.0);
        CAP_WHY.set(self.1);
    }
}

/// Since the start, by [`Why`]: host copies made with fewer levels for the
/// band ([`CAP`]); passes over textures picked by index that began with the
/// card without room ([`room`]). And new host copies made whole in the room
/// our blocks of textures had free while the card counted short
/// ([`card_short_on`], [`fits_pool`]).
#[cfg(windows)]
pub(crate) static DETAIL_WHY: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];
#[cfg(windows)]
pub(crate) static NO_ROOM_WHY: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];
#[cfg(windows)]
pub(crate) static POOL_WHOLE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The least [`CAP`] slots were held at since they were last let out
/// ([`held_at`], [`release_held`]): 0 for slots held without a copy.
#[cfg(windows)]
static HELD_UNDER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);

/// A slot is held by the texture pass while new copies have at most `side`
/// texels a side (0: it has no copy).
#[cfg(windows)]
pub(crate) fn held_at(side: u64) {
    HELD_UNDER.fetch_min(side, std::sync::atomic::Ordering::Relaxed);
}

/// Whether a pass with room lets the held slots out now (as it did at every
/// pass with room with [`detail`] off): when copies may have more levels
/// now than when some slot was held. Each such slot is looked at, which
/// costs a look at its memory: in the band the same few thousand were
/// otherwise looked at again at every pass.
#[cfg(windows)]
pub(crate) fn release_held(side: u64) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    if detail() == Detail::Off {
        return true;
    }
    let under = HELD_UNDER.load(Relaxed);
    if side > under {
        HELD_UNDER.store(u64::MAX, Relaxed);
        return true;
    }
    false
}

/// Since the start: host copies made with fewer levels for the band
/// ([`CAP`]) and their bytes; host copies cut down by the sweep
/// ([`cut_idle`]) and the bytes those had; new copies of small textures made
/// while the card had no room ([`small_when_full`]).
#[cfg(windows)]
pub(crate) static DETAILED: [std::sync::atomic::AtomicU64; 5] = [const { std::sync::atomic::AtomicU64::new(0) }; 5];

/// Says, at most every five seconds, how textures picked by index are made
/// on a card that fills ([`detail`]): the side new copies have at most now
/// (`side`, `room`: the pass's), and since the start what [`DETAILED`]
/// counts; `rechecks`: slots of the queue uploaded whole and waiting for a
/// look by fingerprint (`descriptor/host.rs` `TextureQueue::census`).
#[cfg(windows)]
pub(crate) fn detail_line(side: u64, room: bool, rechecks: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    static SAID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = crate::host::coarse_ms();
    let last = SAID.load(Relaxed);
    if now < last.saturating_add(5000) || SAID.compare_exchange(last, now.max(1), Relaxed, Relaxed).is_err() {
        return;
    }
    let [made, bytes, cut, cut_bytes, small] = [0, 1, 2, 3, 4].map(|i| DETAILED[i].load(Relaxed));
    let why = DETAIL_WHY.each_ref().map(|n| n.load(Relaxed));
    let no_room = NO_ROOM_WHY.each_ref().map(|n| n.load(Relaxed));
    let now_side = match (room, side) {
        (false, _) => format!("their smallest levels only ({TAIL_SIDE} texels a side)"),
        (true, u64::MAX) => "every level".to_owned(),
        (true, side) => format!("{side} texels a side at most"),
    };
    tracing::info!(
        "textures picked by index on a filling card (VICTORY_TEXTURE_DETAIL): new host copies now of {now_side}; since \
         the start {made} made with fewer levels ({} MB; {} for the queue's own count of the card, {} for the system's \
         count of it, {} for the PC's commit), {cut} cut down while idle ({} MB before; VICTORY_TEXTURE_CUT), \
         {small} small ones made while the card had no room (VICTORY_SMALL_WHEN_FULL_KB); passes that found the card \
         without room: {} by the queue's count, {} by the system's; {} made whole in the room our blocks of textures \
         had free (VICTORY_CARD_SHORT {}); {rechecks} slots of the queue uploaded and waiting for a look at their memory",
        bytes >> 20,
        why[Why::Queue as usize],
        why[Why::Whole as usize],
        why[Why::Pc as usize],
        cut_bytes >> 20,
        no_room[Why::Queue as usize],
        no_room[Why::Whole as usize],
        POOL_WHOLE.load(Relaxed),
        if card_short_on() { "on" } else { "off" }
    );
}

/// The level a host copy of only the smallest levels of a texture of `desc`
/// begins at ([`tail_only`]) for views that start at `first`: the first level
/// of at most [`TAIL_SIDE`] texels a side or a later one the views start at,
/// as [`base_level`] can begin a host copy there (it can at the console's
/// mip tail at the latest); 0 if the texture can have no such copy (one
/// level, not a plain 2D texture, a layout read whole).
#[cfg(windows)]
fn tail_base(desc: &ResourceDesc, first: u32) -> u32 {
    let tail = tail_level(desc.width, u64::from(desc.height), u32::from(desc.mip_levels));
    base_level(desc, first.max(tail))
}

/// [`tail_base`] for a texture the title placed with the resource
/// description `desc` (as words), for a view that starts at `first`: above
/// 0 if the pass over textures picked by index can give it a host copy of its
/// smallest levels only while the card has no room (`draw.rs`
/// `heap_textures`). 0 without [`held_only`], which such copies need.
#[cfg(windows)]
pub(crate) fn placed_tail_base(desc: &[u32; 14], first: u32) -> u32 {
    if !held_only() {
        return 0;
    }
    // SAFETY: 56 bytes in the description's layout.
    let d = unsafe { desc.as_ptr().cast::<ResourceDesc>().read_unaligned() };
    tail_base(&d, first)
}

#[cfg(windows)]
thread_local! {
    /// While the pass over textures picked by index works on a slot with the
    /// card out of room ([`tail_only`], [`TailOnly`]): a host copy of a placed
    /// texture made on this thread holds only its smallest levels, and one
    /// that does is not replaced by one with more of them ([`want_level`]).
    static TAIL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Sets [`TAIL`] on this thread while it lives, and gives it back what it was
/// then.
#[cfg(windows)]
pub(crate) struct TailOnly(bool);

#[cfg(windows)]
impl TailOnly {
    pub(crate) fn new(on: bool) -> TailOnly {
        TailOnly(TAIL.replace(on))
    }
}

#[cfg(windows)]
impl Drop for TailOnly {
    fn drop(&mut self) {
        TAIL.set(self.0);
    }
}

/// Whether texture `this` has a host copy of only its smallest levels
/// ([`tail_only`]); not made here.
#[cfg(windows)]
pub(crate) fn tail_copy(this: This) -> bool {
    made_copy(this).is_some_and(|copy| copy.tail)
}

/// Since the start: host copies of only the smallest levels made
/// ([`tail_only`]), their bytes, and such copies replaced by ones with more
/// levels.
#[cfg(windows)]
pub(crate) static TAILS: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// The bytes of title memory a placed texture with the resource description
/// `desc` is read from for a view that starts at level `first`; `None` as
/// [`placed_bytes`]. Remembered: asked for every slot that is looked at.
#[cfg(windows)]
pub(crate) fn placed_bytes_from(desc: &[u32; 14], first: u32) -> Option<u64> {
    if first == 0 || !held_only() {
        return placed_bytes(desc);
    }
    type Known = std::collections::HashMap<([u32; 14], u32), Option<u64>>;
    thread_local! {
        static KNOWN: std::cell::RefCell<Known> = std::cell::RefCell::default();
    }
    KNOWN.with(|known| {
        *known.borrow_mut().entry((*desc, first)).or_insert_with(|| {
            let whole = placed_bytes(desc)?;
            // SAFETY: 56 bytes in the description's layout.
            let d = unsafe { desc.as_ptr().cast::<ResourceDesc>().read_unaligned() };
            let Some(t) = texture_layout(&d).filter(|t| t.planes.len() == 1) else {
                return Some(whole);
            };
            let layout = &t.planes[0].layout;
            Some(layout.bytes_from(read_from(layout, first)).min(whole))
        })
    })
}

/// Host copies replaced by ones with more levels, for as long as a thread
/// that asked for one may still be about to record its use
/// ([`host_resource`]): when, and the copy.
#[cfg(windows)]
static REPLACED: std::sync::Mutex<Vec<(u64, Made)>> = std::sync::Mutex::new(Vec::new());

/// Since the start: host copies made with a first level other than the
/// title's, and their bytes; host copies replaced by one with more levels;
/// bytes of title memory such copies' textures take whole.
#[cfg(windows)]
pub(crate) static HELD: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// A view of the placed texture `this` that starts at level `first` is made
/// or brought up to date: a host copy without that level is replaced by one
/// with it (made and filled at the next use; the slots that show the old
/// one go on showing it until that is done).
#[cfg(windows)]
pub(crate) fn want_level(this: This, first: u32) {
    use std::sync::atomic::Ordering::Relaxed;
    if this.is_null() || !held_only() {
        return;
    }
    let r = resource(this);
    if r.desc.dimension == BUFFER || !r.host.placed.load(Relaxed) {
        return;
    }
    let mut made = r.host.made.lock().unwrap_or_else(|e| e.into_inner());
    let Some((host, _)) = made.as_ref() else {
        r.host.wanted.fetch_min(first, Relaxed);
        return;
    };
    if host.base == 0 || host.base <= base_level(&r.desc, first) {
        return;
    }
    // A slot made where no upload follows ([`NoUploadFollows`]) would show
    // the new copy before anything is in it: the copy stays, and the slot
    // shows its levels; the texture pass replaces it when it asks for more.
    if no_upload_follows() {
        UNWRITTEN[2].fetch_add(1, Relaxed);
        return;
    }
    // Of only the smallest levels, for a card without room ([`tail_only`]):
    // so while the pass that made it finds none.
    if host.tail && TAIL.get() {
        return;
    }
    // Of fewer levels for a card in the band below its line ([`detail`]),
    // or cut down by the sweep: replaced only by one with more levels than
    // it has, as the band allows now (the one made next is of those).
    let side = CAP.get();
    if host.tail && side != u64::MAX && side_base(&r.desc, first, side) >= host.base {
        return;
    }
    if host.tail {
        TAILS[2].fetch_add(1, Relaxed);
    }
    r.host.wanted.store(first, Relaxed);
    // Under the lock, as in `let_go_idle`.
    crate::texture::unstage(this);
    r.host.grown.store(true, Relaxed);
    HELD[2].fetch_add(1, Relaxed);
    if let Some(old) = made.take() {
        REPLACED.lock().unwrap_or_else(|e| e.into_inner()).push((crate::host::moment(), old));
    }
}

/// Counts for the memory line: host copies of textures the title fills
/// alive and their bytes; such copies let go as idle since the start and
/// their bytes; made again after that and their bytes; and host copies made
/// since the start.
#[cfg(windows)]
pub(crate) static KINDS: [std::sync::atomic::AtomicU64; 7] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 7];

/// Every title texture alive (not buffers), by address: for the sweep and
/// the memory line. A texture leaves the table before it is freed.
#[cfg(windows)]
static TEXTURES: std::sync::Mutex<std::collections::BTreeSet<usize>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

#[cfg(windows)]
fn textures() -> std::sync::MutexGuard<'static, std::collections::BTreeSet<usize>> {
    TEXTURES.lock().unwrap_or_else(|e| e.into_inner())
}

/// The title textures the host renders to or writes (targets, depth,
/// unordered access), by the address of their memory: what a view the
/// title places in that memory in another format shows ([`alias_of`]). A
/// texture leaves the table before it is freed.
#[cfg(windows)]
static TARGETS_AT: std::sync::Mutex<std::collections::BTreeMap<u64, usize>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// For `this`, a texture made for a view the title placed in memory
/// ([`texture_at`]): the title texture the host writes that lies in the
/// same memory, if there is one of the same size, levels and layers with
/// texels of the same width. The console reads such a texture through the
/// view as it is
/// (a title may pack light as R9G9B9E5 into R32_UINT textures with compute
/// shaders, which cannot write R9G9B9E5, and sample them through R9G9B9E5
/// views placed at the textures' addresses). On the
/// host the two are two textures and what the shaders wrote exists in one
/// of them only: the other was filled from title memory, which nothing
/// writes, so everything read through such a view was black.
/// Textures of several levels or layers, and volumes, count too: only
/// textures of one level and one layer were found at first, and the
/// programs behind the others read the title's unwritten memory - a second
/// stage wrote all zeros from a first stage that held light.
/// `VICTORY_ALIAS_COPY=0`: none is found, as before; `=1`: only textures
/// of one level and one layer, as at first.
///
/// The texture is not held: only whether there is one, or its address for a
/// line, may be had from here. What uses it takes [`alias_held`].
#[cfg(windows)]
pub(crate) fn alias_of(this: This) -> Option<This> {
    alias_with(this, Some)
}

/// A title texture held alive by a reference of ours ([`alias_held`]), let
/// go when this goes.
#[cfg(windows)]
pub(crate) struct Held {
    this: This,
    /// Whether the reference is ours (`VICTORY_ALIAS_PIN=0`: none, the
    /// texture as [`alias_of`] gives it).
    pinned: bool,
}

#[cfg(windows)]
impl Held {
    /// The texture.
    pub(crate) fn get(&self) -> This {
        self.this
    }
}

#[cfg(windows)]
impl Drop for Held {
    fn drop(&mut self) {
        if self.pinned {
            object::release_boxed::<Resource>(self.this);
        }
    }
}

/// [`alias_of`], with a reference of ours taken under the lock it is found
/// under, for a copy from it: the title may let go of its last reference
/// once the lock is let go (its threads may release a texture as soon as its
/// last use is recorded), and the texture was freed under the copy
/// (`crate::texture::alias` reads its host copy and records barriers of
/// it): a use after free, or a copy from the next texture made at that
/// address. One the title is releasing already is not given: the view is
/// filled from title memory, as when there is none. After fxceless #1336
/// (a463dcb; a candidate for corrupt textures). `VICTORY_ALIAS_PIN=0`: not
/// held, as before.
#[cfg(windows)]
pub(crate) fn alias_held(this: This) -> Option<Held> {
    static PIN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let pin = *PIN.get_or_init(|| std::env::var_os("VICTORY_ALIAS_PIN").is_none_or(|v| v != "0"));
    alias_with(this, |other| {
        if !pin {
            return Some(Held { this: other, pinned: false });
        }
        // (No guard is made before the reference is: one dropped unpinned
        // would let go of a reference never taken.)
        if !object::add_ref_alive(other) {
            return None;
        }
        Some(Held { this: other, pinned: true })
    })
}

/// [`alias_of`]'s search: `found` is given the texture found, under the lock
/// of [`TARGETS_AT`] (a texture leaves it before it is freed).
#[cfg(windows)]
fn alias_with<T>(this: This, found: impl FnOnce(This) -> Option<T>) -> Option<T> {
    use std::sync::atomic::Ordering::Relaxed;
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if this.is_null()
        || !*ON.get_or_init(|| std::env::var_os("VICTORY_ALIAS_COPY").is_none_or(|v| v != "0"))
    {
        return None;
    }
    static WHOLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let whole =
        *WHOLE.get_or_init(|| std::env::var_os("VICTORY_ALIAS_COPY").is_none_or(|v| v != "1"));
    let r = resource(this);
    // A level count of 0 asks for a full chain: not counted here.
    let one = |d: &ResourceDesc| {
        d.dimension != BUFFER
            && d.mip_levels >= 1
            && d.depth_or_array_size >= 1
            && (whole || (d.mip_levels == 1 && d.depth_or_array_size == 1))
            && d.sample_count <= 1
    };
    if !r.host.placed.load(Relaxed) || !fills(&r.desc) || !one(&r.desc) {
        return None;
    }
    // Under the table's lock: a texture leaves it before it is freed.
    let targets = TARGETS_AT.lock().unwrap_or_else(|e| e.into_inner());
    let other = *targets.get(&(r.data() as u64))? as This;
    let o = resource(other);
    const DEPTH: u32 = 0x2;
    let same = one(&o.desc)
        && o.desc.dimension == r.desc.dimension
        && o.desc.width == r.desc.width
        && o.desc.height == r.desc.height
        && o.desc.mip_levels == r.desc.mip_levels
        && o.desc.depth_or_array_size == r.desc.depth_or_array_size
        && o.desc.flags & DEPTH == 0
        && target_format(o.desc.format) == o.desc.format
        && element(o.desc.format).is_some_and(|e| e.1 == 1 && Some(e) == element(r.desc.format));
    if !same && r.desc.sample_count <= 1 {
        // What shaders wrote there is on the card in the other texture; this
        // one is filled from title memory, which they never reached. Each
        // kind of pair is said once.
        static SAID: std::sync::Mutex<std::collections::BTreeSet<[u32; 6]>> =
            std::sync::Mutex::new(std::collections::BTreeSet::new());
        let pair = [
            r.desc.width as u32,
            r.desc.height,
            r.desc.format,
            o.desc.width as u32,
            o.desc.height,
            o.desc.format,
        ];
        let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
        if said.len() < 64 && said.insert(pair) {
            tracing::warn!(
                view = ?this,
                texture = ?other,
                "a placed texture lies where a texture the host writes does, and is not like it (width, height, format of each): {pair:?}, levels {} and {}, layers {} and {}",
                r.desc.mip_levels, o.desc.mip_levels, r.desc.depth_or_array_size, o.desc.depth_or_array_size
            );
        }
    }
    // (The table's lock is still held.)
    let found = if same { found(other) } else { None };
    drop(targets);
    found
}

/// Whether host copies are made on first use. On by default;
/// `VICTORY_LAZY_TEXTURES=0` makes each when the title creates its texture,
/// as before.
#[cfg(windows)]
fn lazy() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LAZY_TEXTURES").is_none_or(|v| v != "0"))
}

/// Whether a texture described by `desc` takes its contents from title
/// memory: one the host neither renders to nor writes (no target, depth or
/// unordered access flag). Its host copy can be made again from that
/// memory; any other texture's contents exist on the host alone.
#[cfg(windows)]
fn fills(desc: &ResourceDesc) -> bool {
    const TARGET_DEPTH_UNORDERED: u32 = 0x7;
    desc.dimension != BUFFER && desc.flags & TARGET_DEPTH_UNORDERED == 0
}

/// Whether the title fills texture `this` itself ([`fills`]).
#[cfg(windows)]
pub(crate) fn title_fills(this: This) -> bool {
    !this.is_null() && fills(&resource(this).desc)
}

/// Whether `this` is a texture whose host copy is not made yet (and may
/// still be): a descriptor slot written with a view of it waits
/// (`crate::descriptor::make_waiting`).
#[cfg(windows)]
pub(crate) fn host_waits(this: This) -> bool {
    if this.is_null() || !lazy() {
        return false;
    }
    let r = resource(this);
    // (One the host had no memory for may still be: its slots wait for it as
    // for any other, and are made when it is, [`texture_retry`].)
    r.desc.dimension != BUFFER
        && r.host.refused_until.load(std::sync::atomic::Ordering::Relaxed) != u64::MAX
        && r.host.made.lock().unwrap_or_else(|e| e.into_inner()).is_none()
}

/// Whether a host texture refused for want of memory (`E_OUTOFMEMORY`, at its
/// making or at making it resident) is asked for again after [`RETRY_MS`],
/// twice as long after each such refusal in a row, at most 8 s
/// (`VICTORY_TEXTURE_RETRY`, on unless `0`; `0`: refused for good, as before).
/// Any other refusal stays for good. A texture refused while the card or the
/// memory it shares was full stayed without a host copy for the rest of the
/// run: the pass over textures picked by index took its slot as done, and its
/// null view was noted as made (`HostHeap::mirror`). After upstream's pull
/// request 287, 4e95a78 (upstream, for upstream); the longer
/// waits are ours: each refusal holds every upload back for half a second
/// (`crate::host::commit`), and one the card never has room for would
/// otherwise do so twice a second.
#[cfg(windows)]
fn texture_retry() -> bool {
    crate::toggled!("VICTORY_TEXTURE_RETRY")
}

/// How long a texture refused for want of memory waits before it is asked
/// for again the first time ([`texture_retry`]): as long as uploads wait
/// after a refusal (`crate::host::commit`).
#[cfg(windows)]
const RETRY_MS: u64 = 500;

/// How many times the wait of [`RETRY_MS`] doubles at most: 8 s.
#[cfg(windows)]
const RETRY_DOUBLINGS: u8 = 4;

/// Since the start ([`texture_retry`]): refusals for want of memory, the
/// textures refused so (their first such refusal), and those made later.
#[cfg(windows)]
static RETRIES: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

#[cfg(windows)]
thread_local! {
    /// The host's last refusal of a texture on this thread was for want of
    /// memory ([`host_texture_as`]; taken by [`refuse`]).
    static REFUSED_FOR_MEMORY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the host refused `r`'s copy and is not asked again yet: for good,
/// before its time, or while uploads are held back after an allocation was
/// refused (`crate::host::commit::upload_refused`), so that a full card is
/// not asked once per such texture every half second.
#[cfg(windows)]
fn refused(r: &Resource) -> bool {
    let until = r.host.refused_until.load(std::sync::atomic::Ordering::Relaxed);
    until != 0 && (until == u64::MAX || crate::host::coarse_ms() < until || crate::host::commit::upload_refused())
}

/// The host refused `r`'s copy just now ([`hosted`], under `made`'s lock):
/// asked for again after [`RETRY_MS`], doubled for each refusal in a row
/// before, if it had no memory for it ([`texture_retry`]); never again
/// otherwise.
#[cfg(windows)]
fn refuse(r: &Resource) {
    use std::sync::atomic::Ordering::Relaxed;
    let until = if REFUSED_FOR_MEMORY.replace(false) && texture_retry() {
        let before = r.host.refusals.load(Relaxed);
        r.host.refusals.store((before + 1).min(RETRY_DOUBLINGS), Relaxed);
        crate::host::coarse_ms().saturating_add(RETRY_MS << before)
    } else {
        u64::MAX
    };
    let was = r.host.refused_until.swap(until, Relaxed);
    if until == u64::MAX {
        return;
    }
    if was == 0 {
        RETRIES[1].fetch_add(1, Relaxed);
    }
    let n = RETRIES[0].fetch_add(1, Relaxed) + 1;
    if n.is_power_of_two() {
        tracing::info!(
            "host textures the host had no memory for, asked for again after {RETRY_MS} ms, longer after each refusal \
             in a row (VICTORY_TEXTURE_RETRY=0: refused for good, as before), since the start: {n} refusals of {} \
             textures, {} made later",
            RETRIES[1].load(Relaxed),
            RETRIES[2].load(Relaxed)
        );
    }
}

/// Whether title texture `this` has no host copy because the host had no
/// memory for it, and will be asked for again ([`texture_retry`]): a slot
/// that shows it is not done with (`draw.rs` `heap_textures`,
/// `HostHeap::mirror`).
#[cfg(windows)]
pub(crate) fn retrying(this: This) -> bool {
    if this.is_null() {
        return false;
    }
    let until = resource(this).host.refused_until.load(std::sync::atomic::Ordering::Relaxed);
    until != 0 && until != u64::MAX
}

/// The host side of title texture `this`: made now if it is not there and
/// `make` says so. `None` for buffers and for textures the host refuses.
#[cfg(windows)]
fn hosted(this: This, make: bool) -> Option<Made> {
    use std::sync::atomic::Ordering::Relaxed;
    let r = resource(this);
    if r.desc.dimension == BUFFER {
        return None;
    }
    let mut made = r.host.made.lock().unwrap_or_else(|e| e.into_inner());
    if made.is_none() {
        if !make || refused(r) {
            return None;
        }
        let clear = r
            .host
            .clear
            .as_ref()
            .map_or(std::ptr::null(), |c| std::ptr::from_ref(c).cast::<c_void>());
        // Levels on demand (`crate::tiles`): a texture the title places in
        // memory and fills, of a megabyte or more, that does not hold all
        // its levels now, is made without memory and given it level by
        // level. It does not hold them all if its memory is not all
        // mapped, if the next texture starts inside its span, or if a
        // view of it starts below level 0. Every other texture, and this
        // one if the card will not have it so, is made as before.
        const TEXTURE_2D: u32 = 3;
        let mut tiled = None;
        let mut texture = None;
        let shape_fits = fills(&r.desc)
            && r.desc.dimension == TEXTURE_2D
            && r.desc.depth_or_array_size == 1
            && r.desc.mip_levels > 1
            && r.desc.sample_count <= 1;
        if shape_fits && r.host.placed.load(Relaxed) && crate::tiles::wanted() {
            let bytes = footprint(&r.desc).map_or(0, |f| f.size);
            let start = r.data() as u64;
            // What the upload can read: a console layout, no XOR mode
            // (`crate::texture` `staging`).
            let readable = texture_layout(&r.desc).is_some_and(|t| !t.planes[0].layout.desc.swizzle.xor());
            let partly = readable
                && bytes >= crate::tiles::floor()
                && (!crate::texture::committed(r.data().cast_const(), bytes as usize)
                    || crate::descriptor::host::next_start(start, start.saturating_add(bytes)).is_some()
                    || crate::descriptor::host::first_viewed(start).is_some_and(|first| first > 0));
            let reserved = if partly { host_texture_as(&r.desc, r.host.state, clear, true).map(|(t, _)| t) } else { None };
            let cut = reserved
                .as_ref()
                .zip(crate::host::host())
                .and_then(|(t, host)| crate::tiles::Tiled::make(&host.device, t, u32::from(r.desc.mip_levels)));
            // (Both or neither: a reserved texture the card will not cut
            // into tiles is dropped here, and the old kind is made below.)
            if cut.is_some() {
                tiled = cut;
                texture = reserved;
            }
        }
        // The levels views show ([`held_only`]), those of the slots shaders
        // pick by index among them.
        let mut base = 0;
        let mut tail = false;
        let (mut detailed, mut smallest) = (false, false);
        if shape_fits && r.host.placed.load(Relaxed) && held_only() && alias_of(this).is_none() {
            let wanted = r.host.wanted.load(Relaxed);
            let viewed = crate::descriptor::host::first_viewed(r.data() as u64).unwrap_or(u32::MAX);
            let first = wanted.min(viewed);
            if first != u32::MAX {
                base = base_level(&r.desc, first);
            }
            // Only its smallest levels while the card has no room for more
            // ([`tail_only`]); not in the place of a copy with fewer levels,
            // which a view asked more of.
            if TAIL.get() && !r.host.grown.load(Relaxed) {
                let small = tail_base(&r.desc, if first == u32::MAX { 0 } else { first });
                if small > base {
                    (base, tail, smallest) = (small, true, true);
                }
            }
            // Fewer levels while the card is in the band below its line
            // ([`detail`]), in the place of a copy with fewer still too:
            // `want_level` replaced that one only because this side gives
            // more than it had.
            let side = CAP.get();
            if side != u64::MAX {
                let fewer = side_base(&r.desc, if first == u32::MAX { 0 } else { first }, side);
                if fewer > base {
                    (base, tail, detailed, smallest) = (fewer, true, true, false);
                }
            }
            // As the sweep cut the last one down ([`cut_idle`]).
            let cut = r.host.cut.swap(0, Relaxed);
            if cut > base && cut < u32::from(r.desc.mip_levels) {
                (base, tail, detailed, smallest) = (cut, true, false, false);
            }
        }
        let texture = match texture {
            Some(texture) => Some((texture, false)),
            None => {
                // (What a reserved texture's making said above is not this
                // one's: [`refuse`] goes by the last.)
                REFUSED_FOR_MEMORY.set(false);
                host_texture(&r.desc.levels_from(base), r.host.state, clear)
            }
        };
        let Some((texture, own)) = texture else {
            refuse(r);
            return None;
        };
        if r.host.refused_until.swap(0, Relaxed) != 0 {
            r.host.refusals.store(0, Relaxed);
            let n = RETRIES[2].fetch_add(1, Relaxed) + 1;
            if n.is_power_of_two() {
                tracing::info!(
                    "host textures made after the host had no memory for them (VICTORY_TEXTURE_RETRY), since the \
                     start: {n}, of {} refused so",
                    RETRIES[1].load(Relaxed)
                );
            }
        }
        // Its residency class ([`Hosted::residency`]): a target's was given
        // as it was made; one the title fills is the sweep's to give.
        let class = match (own, fills(&r.desc)) {
            (false, _) => NOT_OWN,
            (true, true) => crate::host::Host::RESIDENCY_UNSET,
            (true, false) => crate::host::Host::RESIDENCY_HIGH,
        };
        r.host.residency.store(class, Relaxed);
        let twin = twin_texture(&r.desc, &texture);
        // One line per depth texture with unordered access: its address is
        // the one target dumps and the captured frame's list of commands
        // name.
        if r.desc.flags & 6 == 6 {
            tracing::info!(
                resource = ?this,
                desc = ?r.desc,
                twin = twin.is_some(),
                "depth texture with unordered access"
            );
        }
        let state = crate::host::legal_state(&texture, crate::host::texture_state(r.host.state));
        let mut host = match tiled {
            Some(tiled) => HostTexture::new_tiled(texture, state, tiled),
            None => HostTexture::new(texture, state),
        };
        host.base = base;
        host.tail = tail;
        host.pooled = !own && host.tiled.is_none();
        host.grown.store(r.host.grown.swap(false, Relaxed), Relaxed);
        if base > 0 {
            HELD[0].fetch_add(1, Relaxed);
            HELD[1].fetch_add(host.bytes, Relaxed);
            HELD[3].fetch_add(footprint(&r.desc).map_or(0, |f| f.size), Relaxed);
        }
        if detailed {
            DETAILED[0].fetch_add(1, Relaxed);
            DETAILED[1].fetch_add(host.bytes, Relaxed);
            DETAIL_WHY[usize::from(CAP_WHY.get()).min(3)].fetch_add(1, Relaxed);
        }
        if smallest {
            let n = TAILS[0].fetch_add(1, Relaxed) + 1;
            let bytes = TAILS[1].fetch_add(host.bytes, Relaxed) + host.bytes;
            if n == 1 || n.is_multiple_of(2000) {
                tracing::info!(
                    "host copies of only the smallest levels of textures picked by index, made while the card \
                     had no room (VICTORY_TEXTURE_TAIL), since the start: {n}, {:.1} MB; {} of them replaced by \
                     ones with more levels",
                    bytes as f64 / 1e6,
                    TAILS[2].load(Relaxed)
                );
            }
        }
        host.filled = fills(&r.desc);
        if host.filled {
            KINDS[0].fetch_add(1, Relaxed);
            KINDS[1].fetch_add(host.bytes, Relaxed);
        }
        if r.host.gone.load(Relaxed) {
            KINDS[4].fetch_add(1, Relaxed);
            KINDS[5].fetch_add(host.bytes, Relaxed);
        }
        KINDS[6].fetch_add(1, Relaxed);
        *made = Some((std::sync::Arc::new(host), twin));
        // Slots the title wrote with views of it while it had no host copy
        // are made before the next submission runs.
        crate::descriptor::texture_made(this as usize);
    }
    let now = crate::host::moment();
    if r.host.used.load(Relaxed) != now {
        r.host.used.store(now, Relaxed);
    }
    made.clone()
}

/// The host copy of title texture `this` if it is made, without making it
/// (for diagnostics, which must not count as a use).
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn host_made<'a>(
    this: This,
) -> Option<&'a windows::Win32::Graphics::Direct3D12::ID3D12Resource> {
    if this.is_null() {
        return None;
    }
    let (host, _) = hosted(this, false)?;
    // SAFETY: as in `host_resource`.
    Some(unsafe { &*std::ptr::from_ref(&host.texture) })
}

/// Unconditional host-texture floor for a given local VRAM budget (`None` when
/// the adapter does not report one): up to 2 GiB, capped at a quarter of the
/// reported budget so 4–6 GiB cards do not bypass the budget check early.
#[cfg(any(windows, test))]
fn texture_floor_bytes(vram_budget: Option<u64>) -> u64 {
    const MAX_FLOOR: u64 = 2 << 30;
    match vram_budget {
        Some(budget) => MAX_FLOOR.min(budget / 4),
        None => MAX_FLOOR,
    }
}

/// Whether more host copies of textures the title fills may be made for
/// shaders that pick textures by index ([`no_room_why`] says why not).
#[cfg(windows)]
pub(crate) fn room(memory: Option<[u64; 4]>) -> bool {
    no_room_why(memory).is_none()
}

/// Whether more host copies of textures the title fills may be made for
/// shaders that pick textures by index (`draw.rs` `heap_textures`): always
/// up to [`texture_floor_bytes`] bytes of them, beyond that while the card's
/// memory in use by this process is under nine tenths of the budget the system
/// gives it ([`room_percent`] says another share; `VICTORY_TEXTURE_BUDGET_MB`
/// sets a limit on those copies' bytes instead). Past the budget the system moves the card's memory into the
/// PC's, and the run ends.
///
/// `None` with room, else why there is none ([`Why::Queue`] for a limit of
/// `VICTORY_TEXTURE_BUDGET_MB` or a card that says no budget). With
/// [`card_short_on`], there is room while the card is not short
/// ([`CardFill::room_as`]).
#[cfg(windows)]
pub(crate) fn no_room_why(memory: Option<[u64; 4]>) -> Option<Why> {
    use std::sync::atomic::Ordering::Relaxed;
    const WITHOUT_BUDGET: u64 = 4 << 30;
    let no = |room: bool| (!room).then_some(Why::Queue);
    static LIMIT: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    let limit = *LIMIT.get_or_init(|| {
        let mb = std::env::var("VICTORY_TEXTURE_BUDGET_MB").ok()?;
        mb.trim().parse::<u64>().ok().map(|mb| mb << 20)
    });
    // (With the tiles of the copies made level by level, whose own bytes
    // count as none: `crate::tiles`.)
    let filled = KINDS[1].load(Relaxed) + crate::tiles::given_total();
    if let Some(limit) = limit {
        return no(filled < limit);
    }
    let floor = texture_floor_bytes(memory.map(|[budget, ..]| budget));
    if filled < floor {
        return None;
    }
    // What is kept for card copies of title buffers not made yet
    // (`crate::shadow::card_room_kept`) and for the render targets a scene
    // makes in its first frame ([`targets_room_kept`]) counts as used; with
    // a headroom ([`card_headroom`]) the system's own count of our use is
    // held under its line too.
    match card_fill(memory) {
        Some(fill) => fill.room_as(room_percent(), card_headroom(), card_short_on()).err(),
        None => no(filled < WITHOUT_BUDGET),
    }
}

/// The bytes of the card's memory the texture queue leaves free for
/// render targets and other host copies the host itself writes that are
/// not made yet: `VICTORY_CARD_ROOM_FOR_TARGETS_MB` (3,072; 0: none, as
/// before) less what such copies hold now.
///
/// Measured: a few hundred MB of them before the first scene, over 2 GB
/// right after its first frame, up to 3 GB two minutes on, none of them
/// idle. They are made with no room test, in the same frame as the card
/// copies of title buffers, after the texture queue has filled the card to
/// its line with textures written while the scene loaded. The card then
/// stood at its whole budget, buffer copies were turned down for good (and
/// the frame took half as long again), and the queue was refused from the
/// scene's first frame on. With their room kept, the queue stops that much
/// earlier while the scene loads and gets it back as the targets are made.
///
/// What the targets did not take is given up after their burst
/// ([`targets_kept`], `VICTORY_TARGETS_ROOM_FADE=0`: kept for good, as
/// before): a title at a low resolution makes far fewer of them (about 1 GB
/// at 1080p), and what was left of the 3,072 MB stayed kept to the end, so
/// with gigabytes of the card free the queue counted the card full and host
/// copies the title had let go of waited for room that never came.
#[cfg(windows)]
pub(crate) fn targets_room_kept() -> u64 {
    use std::sync::atomic::Ordering::Relaxed;
    static MOST: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let most = *MOST.get_or_init(|| {
        std::env::var("VICTORY_CARD_ROOM_FOR_TARGETS_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map_or(3072 << 20, |mb| mb.min(8192) << 20)
    });
    static FADE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let fade = *FADE.get_or_init(|| std::env::var_os("VICTORY_TARGETS_ROOM_FADE").is_none_or(|v| v != "0"));
    // Every host texture's bytes less those of the copies the title fills.
    let targets = HOST_TEXTURES[1].load(Relaxed).saturating_sub(KINDS[1].load(Relaxed));
    if !fade {
        return most.saturating_sub(targets);
    }
    // The fewest targets seen, and when they first stood a burst above that.
    static LEAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);
    static BURST_AT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let least = LEAST.fetch_min(targets, Relaxed).min(targets);
    let now = crate::host::coarse_ms().max(1);
    if BURST_AT.load(Relaxed) == 0 && targets_burst(least, targets) {
        let _ = BURST_AT.compare_exchange(0, now, Relaxed, Relaxed);
        tracing::info!(
            least_mb = least >> 20,
            targets_mb = targets >> 20,
            kept_mb = most.saturating_sub(targets) >> 20,
            "render targets came in a burst: the card's room kept for more is given up in {} s \
             (VICTORY_TARGETS_ROOM_FADE=0: kept for good)",
            (TARGETS_HOLD_MS + TARGETS_FADE_MS) / 1000
        );
    }
    targets_kept(most, targets, BURST_AT.load(Relaxed), now)
}

/// How far the targets grow above the fewest seen for their burst (a scene's
/// first frames): some hundreds of MB to 2 GB within ten seconds, by
/// resolution.
const TARGETS_BURST: u64 = 512 << 20;
/// After the burst, how long all of the room left stays kept, and over how
/// long it then goes (ms): the targets of a scene's first minute come in
/// that time.
const TARGETS_HOLD_MS: u64 = 30_000;
const TARGETS_FADE_MS: u64 = 30_000;

/// Whether `targets` bytes of targets stand a burst above the `least` seen.
fn targets_burst(least: u64, targets: u64) -> bool {
    targets >= least.saturating_add(TARGETS_BURST)
}

/// The room kept for targets: of `most`, what `targets` have not taken; all
/// of it until [`TARGETS_HOLD_MS`] after `burst_at` (when the burst was seen,
/// [`crate::host::coarse_ms`]; 0: not yet), then less in step with the time
/// over [`TARGETS_FADE_MS`], then none.
fn targets_kept(most: u64, targets: u64, burst_at: u64, now: u64) -> u64 {
    let left = most.saturating_sub(targets);
    let since = now.saturating_sub(burst_at);
    if burst_at == 0 || since <= TARGETS_HOLD_MS {
        left
    } else if since >= TARGETS_HOLD_MS + TARGETS_FADE_MS {
        0
    } else {
        let to_go = TARGETS_HOLD_MS + TARGETS_FADE_MS - since;
        (u128::from(left) * u128::from(to_go) / u128::from(TARGETS_FADE_MS)) as u64
    }
}

/// [`room`] as it was at most a tenth of a second ago, for a question
/// asked of many textures a frame ([`room`] asks the system each time).
#[cfg(windows)]
pub(crate) fn room_lately() -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    // The tenth of a second of the last asking, counted from 1, and its
    // answer in the lowest bit.
    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tenth = crate::host::now_ms() / 100 + 1;
    let last = LAST.load(Relaxed);
    if last >> 1 == tenth {
        return last & 1 != 0;
    }
    let room = room(crate::host::host().and_then(|h| h.video_memory()));
    LAST.store((tenth << 1) | u64::from(room), Relaxed);
    room
}

/// The share of the card's budget, in percent, up to which host copies of
/// textures picked by index are made ([`room`]): 90 unless
/// `VICTORY_TEXTURE_ROOM_PERCENT` says another (50 to 97). At 90 about a
/// tenth of the budget stays unused while thousands of textures wait; the
/// card copies of title buffers from 1 MiB up take a few hundred MB more of
/// the card than before, and 94 gives the textures about that much back.
#[cfg(windows)]
fn room_percent() -> u64 {
    static PERCENT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *PERCENT.get_or_init(|| {
        std::env::var("VICTORY_TEXTURE_ROOM_PERCENT")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map_or(90, |percent| percent.clamp(50, 97))
    })
}

/// For the card accounting line ([`card_accounting`]): the title texture
/// made for a view of memory at `address` with the resource description
/// `desc`, if there is one ([`texture_at`] makes it; this does not).
#[cfg(windows)]
pub(crate) fn placed_known(address: u64, desc: &[u32; 14]) -> Option<This> {
    // Without the filler words and with them: whichever `texture_at` keeps.
    let mut key = *desc;
    key[1] = 0;
    key[13] = 0;
    let map = PLACED_TEXTURES.lock().unwrap_or_else(|e| e.into_inner());
    map.get(&(address, key)).or_else(|| map.get(&(address, *desc))).map(|&r| r as This)
}

/// For the card accounting line: whether title texture `this` has a host
/// copy now (2), had one that was let go and not made again (1), or never
/// had one (0); the bytes (its host copy's on the card, or without one
/// what its console layout takes in title memory); and where its title
/// memory is. `None` for buffers. Not a use of the texture.
#[cfg(windows)]
pub(crate) fn copy_state(this: This) -> Option<(u8, u64, u64)> {
    use std::sync::atomic::Ordering::Relaxed;
    if this.is_null() {
        return None;
    }
    let r = resource(this);
    if r.desc.dimension == BUFFER {
        return None;
    }
    let memory = r.data() as u64;
    let made = r.host.made.lock().unwrap_or_else(|e| e.into_inner());
    Some(match made.as_ref() {
        Some((host, _)) => (2, host.bytes, memory),
        None => (u8::from(r.host.gone.load(Relaxed)), footprint(&r.desc).map_or(0, |f| f.size), memory),
    })
}

/// One line of where the card's memory is and what the queue of textures
/// picked by index would need (`VICTORY_CARD_ACCOUNTING=1`; `draw.rs`
/// `card_accounting` says when, and hands over the queue's side: `room`
/// as the pass found it, `due` its counts, `never` and `gone` the memory
/// of the due textures without a host copy). The line holds
/// - the host copies of textures the title fills that are alive: how
///   many, their bytes, and the bytes of the levels whose title memory is
///   not mapped (a host copy is made with every level of the description;
///   a texture streaming in has its small levels only). A copy's bytes
///   are shared out to its levels by their texels, so that part is an
///   estimate;
/// - the due slots: how many, and their textures by what they have on
///   the card (a host copy alive: no card memory needed; never one; one
///   that was let go), with the bytes the last two would need and how
///   much of their memory is mapped now;
/// - the host copies let go, made again and made since the line before.
///
/// The textures are noted under the table's lock (their memory, their
/// description, their copy's bytes: about a millisecond for 18,000) and
/// the kernel is asked about their memory on a thread of its own, after
/// the lock is given back: one question for a texture that is all there
/// (up to 256 pages each; tens of thousands of textures are more than a
/// million pages, tens of thousands of questions), one per level and array
/// slice more for
/// one that is not. Not measured: a tenth to half a second is expected,
/// and the line says what it took. No line is begun while one is walking.
#[cfg(windows)]
pub(crate) fn card_accounting(room: bool, due: [u64; 5], never: Vec<(u64, u64)>, gone: Vec<(u64, u64)>) {
    use std::sync::atomic::Ordering::Relaxed;
    static WALKING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    // Host copies let go and their bytes, made again and their bytes, and
    // made in all, at the line before.
    static BEFORE: [std::sync::atomic::AtomicU64; 5] = [const { std::sync::atomic::AtomicU64::new(0) }; 5];
    if WALKING.swap(true, Relaxed) {
        return;
    }
    let began = std::time::Instant::now();
    // (The last: made level by level, its bytes are those of its tiles.)
    let mut copies: Vec<(usize, ResourceDesc, u64, bool)> = Vec::new();
    // Textures the title fills without a host copy: never one, one let go.
    let mut without = [0u64; 2];
    {
        let all = textures();
        for &at in all.iter() {
            // SAFETY: as in `let_go_idle`.
            let r = unsafe { &*(at as *const Resource) };
            if !fills(&r.desc) {
                continue;
            }
            let made = r.host.made.lock().unwrap_or_else(|e| e.into_inner());
            match made.as_ref() {
                Some((host, _)) => copies.push(match &host.tiled {
                    Some(tiled) => (r.data() as usize, r.desc, tiled.given_bytes(), true),
                    None => (r.data() as usize, r.desc, host.bytes, false),
                }),
                None => without[usize::from(r.host.gone.load(Relaxed))] += 1,
            }
        }
    }
    let kinds = KINDS.each_ref().map(|k| k.load(Relaxed));
    let mut since = [0u64; 5];
    for ((slot, now), before) in since.iter_mut().zip([kinds[2], kinds[3], kinds[4], kinds[5], kinds[6]]).zip(&BEFORE) {
        *slot = now.saturating_sub(before.swap(now, Relaxed));
    }
    let video = crate::host::host().and_then(|h| h.video_memory());
    let noted_us = began.elapsed().as_micros() as u64;
    let walk = move || {
        fn texels(m: &texture::Mip) -> u64 {
            u64::from(m.width) * u64::from(m.height) * u64::from(m.depth)
        }
        let began = std::time::Instant::now();
        // Copies; their bytes; the bytes of levels not mapped; copies with
        // a level not mapped; with the largest level not mapped; with no
        // level mapped; copies without a console layout (not asked).
        let mut c = [0u64; 7];
        // Of the copies: made level by level, and the bytes of their tiles.
        let mut on_demand = [0u64; 2];
        for &(memory, desc, bytes, by_level) in &copies {
            c[0] += 1;
            c[1] += bytes;
            if by_level {
                on_demand[0] += 1;
                on_demand[1] += bytes;
                continue;
            }
            let Some(layout) = texture_layout(&desc).map(|t| t.planes[0].layout.clone()) else {
                c[6] += 1;
                continue;
            };
            if layout.size == 0 || crate::texture::committed(memory as *const u8, layout.size as usize) {
                continue;
            }
            c[3] += 1;
            // Every slice holds the whole chain: the levels of the tail in
            // its first block, the others after it from the smallest up
            // (`victory_formats::texture::Layout`).
            let mips = &layout.mips;
            let tail = (layout.first_tail_mip as usize).min(mips.len());
            let there = |from: u64, to: u64| {
                (0..u64::from(layout.slices)).all(|slice| {
                    let at = memory as u64 + slice * layout.slice_size + from;
                    crate::texture::committed(at as *const u8, (to - from) as usize)
                })
            };
            let all: u64 = mips.iter().map(texels).sum::<u64>().max(1);
            let (mut missing, mut any) = (0u64, false);
            for (level, m) in mips.iter().enumerate().take(tail) {
                let end = if level == 0 { layout.slice_size } else { mips[level - 1].offset };
                if end <= m.offset {
                    continue;
                }
                if there(m.offset, end) {
                    any = true;
                } else {
                    missing += texels(m);
                    c[4] += u64::from(level == 0);
                }
            }
            if tail < mips.len() {
                if there(0, layout.alignment.min(layout.slice_size).max(1)) {
                    any = true;
                } else {
                    missing += mips[tail..].iter().map(texels).sum::<u64>();
                }
            }
            c[2] += (u128::from(bytes) * u128::from(missing) / u128::from(all)) as u64;
            c[5] += u64::from(!any);
        }
        // Of the due textures without a host copy: how many; the bytes of
        // their memory; with all of it mapped, and those bytes; with part
        // of it mapped, and the bytes that are; with none of it mapped.
        let mapped = |list: &[(u64, u64)]| {
            let mut n = [0u64; 7];
            for &(at, len) in list {
                n[0] += 1;
                n[1] += len;
                let there = crate::texture::mapped_bytes(at as *const u8, len as usize);
                if len > 0 && there >= len {
                    n[2] += 1;
                    n[3] += len;
                } else if there > 0 {
                    n[4] += 1;
                    n[5] += there;
                } else {
                    n[6] += 1;
                }
            }
            n
        };
        let (never, gone) = (mapped(&never), mapped(&gone));
        let [budget_mb, used_mb] = video.map_or([0, 0], |v| [v[0] >> 20, v[1] >> 20]);
        let [copies_n, copies_mb, unmapped_mb, partly, largest, nothing, unknown] =
            [c[0], c[1] >> 20, c[2] >> 20, c[3], c[4], c[5], c[6]];
        let [without_never, without_gone] = without;
        let [due_slots, due_uploaded, due_other, alive_n, alive_mb] = [due[0], due[1], due[2], due[3], due[4] >> 20];
        let [never_n, never_mb, never_whole, never_whole_mb, never_part, never_part_mb, never_none] =
            [never[0], never[1] >> 20, never[2], never[3] >> 20, never[4], never[5] >> 20, never[6]];
        let [gone_n, gone_mb, gone_whole, gone_whole_mb, gone_part, gone_part_mb, gone_none] =
            [gone[0], gone[1] >> 20, gone[2], gone[3] >> 20, gone[4], gone[5] >> 20, gone[6]];
        let [let_go, let_go_mb, again, again_mb, made] = [since[0], since[1] >> 20, since[2], since[3] >> 20, since[4]];
        let walk_ms = began.elapsed().as_millis() as u64;
        let [by_level_n, by_level_mb] = [on_demand[0], on_demand[1] >> 20];
        tracing::info!(
            "card accounting: room {room}, the card {used_mb} MB in use of a budget of {budget_mb} MB; host copies of textures the title \
             fills: {copies_n} alive {copies_mb} MB, of which {unmapped_mb} MB are levels whose title memory is not mapped ({partly} copies \
             with such a level, {largest} of them the largest level, {nothing} with no level mapped; {unknown} without a console layout, not \
             asked); title textures without a host copy: {without_never} never had one, {without_gone} had one let go; due slots: \
             {due_slots} ({due_uploaded} uploaded before, {due_other} of nothing the title fills), their textures: {alive_n} with a host \
             copy alive ({alive_mb} MB, no card memory needed); {never_n} that never had one ({never_mb} MB of title memory: {never_whole} \
             all mapped {never_whole_mb} MB, {never_part} in part {never_part_mb} MB mapped, {never_none} not mapped); {gone_n} whose host \
             copy was let go ({gone_mb} MB: {gone_whole} all mapped {gone_whole_mb} MB, {gone_part} in part {gone_part_mb} MB mapped, \
             {gone_none} not mapped); since the line before: {let_go} host copies let go {let_go_mb} MB, {again} made again {again_mb} MB, \
             {made} made in all; noting took {noted_us} us on the frame's thread, the walk {walk_ms} ms on its own; of the host \
             copies {by_level_n} are made level by level and hold {by_level_mb} MB of tiles (in the MB alive, not in the levels not mapped)"
        );
        WALKING.store(false, Relaxed);
    };
    if std::thread::Builder::new().name("card accounting".into()).spawn(walk).is_err() {
        WALKING.store(false, Relaxed);
    }
}

/// Lets go of the host copies of textures the title fills that nothing
/// names any more: placed textures were never released (one host texture
/// per address and description ever viewed: tens of thousands of them,
/// gigabytes, within minutes), nor those of texture objects before the
/// title
/// released them. A host copy goes when
/// - the title fills the texture ([`fills`]) and the host never copied
///   into it: its contents are in title memory;
/// - only the resource holds it: no list not yet reset names it (a list
///   holds the copies of the textures it uses, `HostList::transition`);
/// - only the host copy holds the host texture: no descriptor slot views
///   it (a slot keeps what it views, `HostHeap::kept`), so no shader can
///   read it through a slot made earlier, by a table or by an index;
/// - nothing asked for it for `IDLE_MS` and `IDLE_FRAMES` frames.
///
/// The next use makes the host copy anew and uploads the title's memory
/// again (its staging state goes with it). The title resource stays: its
/// address is what views and lists name.
/// shortcut: [`host_resource`] hands out the host texture without holding
/// it; a thread that asked for it and has not yet recorded its use (which
/// makes the list or the slot hold it) is protected by that idle time alone.
/// Every caller uses it at once, within the title's call.
/// Called before a submission starts, by the thread that submits.
/// `VICTORY_TEXTURE_SWEEP=0` lets nothing go.
#[cfg(windows)]
pub(crate) fn let_go_idle() {
    const IDLE_FRAMES: u64 = 4;
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_SWEEP").is_none_or(|v| v != "0")) {
        return;
    }
    // Copies replaced by ones with more levels, as any let go.
    {
        let mut replaced = REPLACED.lock().unwrap_or_else(|e| e.into_inner());
        let (gone, keep): (Vec<_>, Vec<_>) =
            replaced.drain(..).partition(|(at, _)| crate::host::long_ago(*at, 1000, IDLE_FRAMES));
        *replaced = keep;
        drop(replaced);
        for (_, made) in gone {
            crate::host::retire(made);
        }
    }
    let memory = crate::host::host().and_then(|h| h.video_memory());
    static KEEP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let keep = *KEEP
        .get_or_init(|| !matches!(std::env::var("VICTORY_TEXTURE_KEEP").as_deref(), Ok("0") | Ok("1")));
    let (idle_ms, most) = match memory {
        Some([budget, used, ..]) if keep => {
            let kept = if let_go_kept_room() { crate::shadow::card_room_kept() + targets_room_kept() } else { 0 };
            crate::host::vram_keep(budget, let_go_use(used, crate::pool::texture_room(), kept))
        }
        _ => (crate::host::vram_idle_ms(memory), u64::MAX),
    };
    match let_go_slice_ms() {
        0 => let_go_whole(idle_ms, most),
        // (Copies slots hold are cut down by this way only.)
        ms => let_go_sliced(idle_ms, most, ms, if most == u64::MAX { u64::MAX } else { cut_side(memory) }),
    }
}

/// Of title texture `r` (at `at`), cuts the host copy down to its levels of
/// at most `side` texels a side if it may be: the title places and fills it,
/// nothing used it for a while (`idle`, given when it was last asked for:
/// the sweep's idle time and four frames), no list holds it, a slot
/// does (one nothing holds goes whole, [`take_idle`]), and it has larger
/// levels than that. The copy goes as one replaced by `want_level` goes: the
/// slots that show it go on showing it, its texture's slots are due in the
/// texture pass (`crate::descriptor::host::remapped`), which makes the next
/// copy from the level cut to ([`Hosted::cut`]) and uploads it, and the
/// slots are made from that one once the upload has run (`grown`). Returns
/// the bytes the copy had.
/// shortcut: a slot's texture that shaders read by index is "used" when the
/// pass or a draw asks for it, not when a shader reads it: one on screen
/// may be cut too. Those idle longest go first.
#[cfg(windows)]
fn cut_idle(r: &Resource, at: usize, side: u64, idle: &dyn Fn(u64) -> bool) -> Option<u64> {
    use std::sync::atomic::Ordering::Relaxed;
    const TEXTURE_2D: u32 = 3;
    let desc = &r.desc;
    if side == u64::MAX
        || !held_only()
        || !r.host.placed.load(Relaxed)
        || !fills(desc)
        || desc.dimension != TEXTURE_2D
        || desc.depth_or_array_size != 1
        || desc.mip_levels <= 1
        || desc.sample_count > 1
    {
        return None;
    }
    let mut made = r.host.made.lock().unwrap_or_else(|e| e.into_inner());
    let (host, _) = made.as_ref()?;
    if host.tiled.is_some()
        || !idle(r.host.used.load(Relaxed))
        || std::sync::Arc::strong_count(host) != 1
        || crate::host::references(&host.texture) <= 1
        || alias_of(at as This).is_some()
    {
        return None;
    }
    let first = crate::descriptor::host::first_viewed(r.data() as u64).unwrap_or(0);
    let cut = side_base(desc, first, side);
    if cut <= host.base {
        return None;
    }
    let bytes = host.bytes;
    crate::texture::unstage(at as This);
    r.host.cut.store(cut, Relaxed);
    r.host.grown.store(true, Relaxed);
    let old = made.take()?;
    drop(made);
    REPLACED.lock().unwrap_or_else(|e| e.into_inner()).push((crate::host::moment(), old));
    // Its slots are looked at again by the next pass, which makes the copy.
    let span = footprint(desc).map_or(0, |f| f.size);
    crate::descriptor::host::remapped(r.data() as usize, span as usize);
    DETAILED[2].fetch_add(1, Relaxed);
    DETAILED[3].fetch_add(bytes, Relaxed);
    Some(bytes)
}

/// Whether [`let_go_idle`] counts the room the texture queue keeps for card
/// copies of title buffers and for render targets not made yet as used
/// (`VICTORY_LET_GO_KEPT_ROOM=1`, as before; by default it does not).
///
/// That room is a reason not to make more host copies (`room`), not one to
/// let go of those there: nothing waits for it. With two thirds of the card's
/// budget in use, the room kept for targets (most of it never used) made the
/// sweep count nine tenths and let tens of thousands of host copies of
/// textures the title fills go while the PC, not the
/// card, was short: they are the card's memory and free none of the PC's.
/// Host copies now go for the card's own shortage alone: its use, less what
/// our blocks of textures have free, past 88 % of its budget.
#[cfg(windows)]
fn let_go_kept_room() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LET_GO_KEPT_ROOM").is_some_and(|v| v == "1"))
}

/// The card's use [`let_go_idle`] goes by: `used` as the system says it,
/// less `free_blocks` (what our blocks of textures have free, which new
/// textures take first), with `kept` (room kept for what is not made yet,
/// [`let_go_kept_room`]) counted as used.
#[cfg_attr(not(windows), allow(dead_code))]
fn let_go_use(used: u64, free_blocks: u64, kept: u64) -> u64 {
    used.saturating_sub(free_blocks) + kept
}

/// How long one look of [`let_go_idle`] goes through the textures, in
/// milliseconds, before the next look goes on from where it stopped
/// (`VICTORY_LET_GO_SLICE_MS`, 1; 0: all of them each time, as before).
/// On a card of 8 GB the card's use is over its line nearly always, and
/// each look went through every title texture twice, holding the table
/// every texture's making and release waits for, on the submitting thread:
/// tens of milliseconds a look, and seconds at the longest.
#[cfg(windows)]
fn let_go_slice_ms() -> u64 {
    static MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MS.get_or_init(|| std::env::var("VICTORY_LET_GO_SLICE_MS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(1))
}

/// Of `all` (in order), visits the entries from `from` on while `more`
/// says so (asked before each but the first, with how many were visited),
/// and says where the next walk begins: at the first one not visited, or
/// at 0 once this walk reached the end (the next one begins a new round).
/// An entry there all round long is visited once in each round; one added
/// behind the walk waits for the next round.
fn walk_from(
    all: &std::collections::BTreeSet<usize>,
    from: usize,
    mut more: impl FnMut(usize) -> bool,
    mut visit: impl FnMut(usize),
) -> usize {
    for (looked, &at) in all.range(from..).enumerate() {
        if looked > 0 && !more(looked) {
            return at;
        }
        visit(at);
    }
    0
}

/// [`let_go_idle`] a part at a time ([`let_go_slice_ms`]): each look goes
/// through the textures from where the last stopped for `slice_ms`, and of
/// those it found idle lets the longest idle go first up to `most` bytes
/// (the whole look sorted them all). It gives host copies of their own
/// memory that the title fills their residency hints on the way
/// (`crate::host::residency`). Short of `most` then, it cuts idle copies
/// that slots hold down to `cut_side` texels a side ([`cut_idle`];
/// `u64::MAX`: none), the longest idle first.
#[cfg(windows)]
fn let_go_sliced(idle_ms: u64, most: u64, slice_ms: u64, cut_side: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    const IDLE_FRAMES: u64 = 4;
    static FROM: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let began = std::time::Instant::now();
    let budget = std::time::Duration::from_millis(slice_ms);
    let hints = crate::host::Host::residency_hints();
    let now = crate::host::moment();
    let mut idle: Vec<(u64, usize)> = Vec::new();
    // Textures whose residency class is to change, and how long they were idle.
    let mut moving: Vec<(usize, u64)> = Vec::new();
    let mut hinted: Vec<(windows::Win32::Graphics::Direct3D12::ID3D12Resource, u8)> = Vec::new();
    let (mut went, mut bytes, mut first) = (0u64, 0u64, None);
    {
        let all = textures();
        let next = walk_from(
            &all,
            FROM.load(Relaxed),
            |looked| !looked.is_multiple_of(64) || began.elapsed() < budget,
            |at| {
                // SAFETY: a texture leaves the table before it is freed,
                // under the lock held here (`Drop for Resource`).
                let r = unsafe { &*(at as *const Resource) };
                let used = r.host.used.load(Relaxed);
                if used == 0 || !fills(&r.desc) {
                    return;
                }
                if hints {
                    let class = r.host.residency.load(Relaxed);
                    let since = ((now & 0xffff_ffff).saturating_sub(used & 0xffff_ffff)) * 100;
                    if class != NOT_OWN && crate::host::Host::filled_residency(class, since) != class {
                        moving.push((at, since));
                    }
                }
                if crate::host::long_ago(used, idle_ms, IDLE_FRAMES) && !crate::texture::host_wrote(at) {
                    idle.push((used & 0xffff_ffff, at));
                }
            },
        );
        FROM.store(next, Relaxed);
        // With a limit, the longest idle first.
        if most != u64::MAX {
            idle.sort_unstable();
        }
        let mut held = Vec::new();
        for (_, at) in idle {
            if bytes >= most {
                break;
            }
            // SAFETY: as above; still in the table, whose lock is held.
            let r = unsafe { &*(at as *const Resource) };
            if let Some(host) = take_idle(r, at, idle_ms) {
                went += 1;
                bytes += host.bytes;
                first.get_or_insert(at);
                crate::host::retire(host);
            } else {
                held.push(at);
            }
        }
        // Still short: what slots hold is cut down (`VICTORY_TEXTURE_CUT`),
        // a few at a time (each is made and uploaded again).
        if cut_side != u64::MAX {
            let idle = |used: u64| crate::host::long_ago(used, idle_ms, IDLE_FRAMES);
            let (mut cuts, mut cut_bytes) = (0, 0);
            for at in held {
                if bytes + cut_bytes >= most || cuts >= CUTS_A_LOOK {
                    break;
                }
                // SAFETY: as above.
                let r = unsafe { &*(at as *const Resource) };
                if let Some(had) = cut_idle(r, at, cut_side, &idle) {
                    cuts += 1;
                    // (What is left of it is a few hundredths of that.)
                    cut_bytes += had;
                }
            }
        }
        // The hints of those that stay (after the letting go: a texture held
        // for its hint would not be let go).
        for (at, since) in moving {
            // SAFETY: as above.
            let r = unsafe { &*(at as *const Resource) };
            let made = r.host.made.lock().unwrap_or_else(|e| e.into_inner());
            let class = r.host.residency.load(Relaxed);
            if let Some((host, _)) = made.as_ref()
                && class != NOT_OWN
            {
                let new = crate::host::Host::filled_residency(class, since);
                if new != class {
                    r.host.residency.store(new, Relaxed);
                    hinted.push((host.texture.clone(), new));
                }
            }
        }
    }
    if let Some(host) = crate::host::host() {
        let objects: Vec<_> = hinted.iter().map(|(t, class)| (t, *class)).collect();
        host.hint_residency(&objects);
    }
    let us = began.elapsed().as_micros() as u64;
    SWEPT_US[0].fetch_add(1, Relaxed);
    SWEPT_US[1].fetch_add(us, Relaxed);
    SWEPT_US[2].fetch_max(us, Relaxed);
    if went > 0 {
        crate::host::trail(|| {
            format!(
                "sweep: {went} host copies of textures the title fills let go ({} KiB), each held by its title texture alone (no list, no slot); the first: title texture {:#x}",
                bytes >> 10,
                first.unwrap_or(0)
            )
        });
    }
}

/// Of title texture `r` (at `at`), takes the host copy if it may go: the
/// title fills it, nothing used it for `idle_ms` and four frames, and only
/// the resource holds the copy and only the copy its host texture. Under
/// the copy's lock, so the next use does not find the staging state of the
/// copy let go (it says "uploaded"). Counted as let go.
#[cfg(windows)]
fn take_idle(r: &Resource, at: usize, idle_ms: u64) -> Option<std::sync::Arc<HostTexture>> {
    use std::sync::atomic::Ordering::Relaxed;
    const IDLE_FRAMES: u64 = 4;
    let (host, _) = {
        let mut made = r.host.made.lock().unwrap_or_else(|e| e.into_inner());
        let idle = made.as_ref().is_some_and(|(host, _)| {
            crate::host::long_ago(r.host.used.load(Relaxed), idle_ms, IDLE_FRAMES)
                && std::sync::Arc::strong_count(host) == 1
                && crate::host::references(&host.texture) == 1
        });
        if !idle {
            return None;
        }
        crate::texture::unstage(at as This);
        r.host.wanted.store(u32::MAX, Relaxed);
        r.host.residency.store(NOT_OWN, Relaxed);
        made.take()?
    };
    r.host.gone.store(true, Relaxed);
    KINDS[2].fetch_add(1, Relaxed);
    KINDS[3].fetch_add(host.bytes, Relaxed);
    Some(host)
}

/// [`let_go_idle`] through all textures at once, as before
/// (`VICTORY_LET_GO_SLICE_MS=0`).
#[cfg(windows)]
fn let_go_whole(idle_ms: u64, most: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    const IDLE_FRAMES: u64 = 4;
    // For the trail (`crate::host::trail`): how many went, their bytes,
    // and the first one.
    let (mut went, mut bytes, mut first) = (0u64, 0u64, None);
    let all = textures();
    // With a limit, the longest idle first.
    let mut by_age: Vec<(u64, usize)> = Vec::new();
    if most != u64::MAX {
        for &at in all.iter() {
            // SAFETY: as below.
            let r = unsafe { &*(at as *const Resource) };
            let used = r.host.used.load(Relaxed);
            if used != 0 && fills(&r.desc) && crate::host::long_ago(used, idle_ms, IDLE_FRAMES) {
                by_age.push((used & 0xffff_ffff, at));
            }
        }
        by_age.sort_unstable();
    }
    // Without one, while textures are kept for minutes, a part of them at
    // each look, the next one at the next: a long session has over 100,000
    // (`VICTORY_TEXTURE_SWEEP=whole`: all each time, as before).
    const PART: usize = 8192;
    static FROM: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static WHOLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let whole = *WHOLE.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_SWEEP").is_some_and(|v| v == "whole"));
    let part = if most != u64::MAX {
        0
    } else if whole || idle_ms < 60_000 {
        usize::MAX
    } else {
        PART
    };
    let from = if part == PART { FROM.load(Relaxed) } else { 0 };
    let mut looked = 0;
    let began = std::time::Instant::now();
    for at in all.range(from..).take(part).copied().chain(by_age.iter().map(|&(_, at)| at)) {
        if bytes >= most {
            break;
        }
        looked += 1;
        if part == PART {
            FROM.store(if looked == PART { at + 1 } else { 0 }, Relaxed);
        }
        // SAFETY: a texture leaves the table before it is freed, under the
        // lock held here (`Drop for Resource`).
        let r = unsafe { &*(at as *const Resource) };
        // (Without its lock first: most were used lately or have none.)
        let used = r.host.used.load(Relaxed);
        if used == 0 || !fills(&r.desc) || !crate::host::long_ago(used, idle_ms, IDLE_FRAMES) || crate::texture::host_wrote(at) {
            continue;
        }
        let gone = {
            let mut made = r.host.made.lock().unwrap_or_else(|e| e.into_inner());
            let idle = made.as_ref().is_some_and(|(host, _)| {
                crate::host::long_ago(r.host.used.load(Relaxed), idle_ms, IDLE_FRAMES)
                    && std::sync::Arc::strong_count(host) == 1
                    && crate::host::references(&host.texture) == 1
            });
            if idle {
                // Under the lock: the next use must not find the staging
                // state of the texture let go (it says "uploaded").
                crate::texture::unstage(at as This);
                r.host.wanted.store(u32::MAX, Relaxed);
                made.take()
            } else {
                None
            }
        };
        if let Some((host, _)) = gone {
            r.host.gone.store(true, Relaxed);
            KINDS[2].fetch_add(1, Relaxed);
            KINDS[3].fetch_add(host.bytes, Relaxed);
            went += 1;
            bytes += host.bytes;
            if first.is_none() {
                first = Some(at);
            }
            crate::host::retire(host);
        }
    }
    SWEPT_US[0].fetch_add(1, Relaxed);
    SWEPT_US[1].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
    SWEPT_US[2].fetch_max(began.elapsed().as_micros() as u64, Relaxed);
    if went > 0 {
        crate::host::trail(|| {
            format!(
                "sweep: {went} host copies of textures the title fills let go ({} KiB), each held by its title texture alone (no list, no slot); the first: title texture {:#x}",
                bytes >> 10,
                first.unwrap_or(0)
            )
        });
    }
}

/// Since the start: looks of [`let_go_idle`], their microseconds, and the
/// longest one's.
#[cfg(windows)]
pub(crate) static SWEPT_US: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// For the memory line: the host copies the sweep never lets go
/// ([`let_go_idle`] takes only copies of textures the title fills that the
/// host never copied into), by what would say they are dead weight. A
/// count and the bytes (twins included) of: the copies the host renders to
/// or writes; of them, those whose title memory is not mapped now (its
/// first page); those nothing has used for 30 seconds and 60 frames and
/// that no list and no slot holds; those that are both; the copies of
/// textures the title fills that the host copied into; and, of the first,
/// the placed ones. Render-target copies grow by a third in a scene's first
/// minutes and never shrink, while tens of thousands of copies of filled
/// textures are let go to stay under the card's line.
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn targets_census() -> [u64; 12] {
    use std::sync::atomic::Ordering::Relaxed;
    let mut c = [0u64; 12];
    let all = textures();
    for &at in all.iter() {
        // SAFETY: as in `let_go_idle`.
        let r = unsafe { &*(at as *const Resource) };
        let made = r.host.made.lock().unwrap_or_else(|e| e.into_inner());
        let Some((host, twin)) = made.as_ref() else {
            continue;
        };
        let copied_into = host.filled && crate::texture::host_wrote(at);
        if host.filled && !copied_into {
            continue;
        }
        let bytes = host.bytes + twin.as_ref().map_or(0, |t| t.colour.bytes);
        let mut add = |i: usize| {
            c[i] += 1;
            c[i + 1] += bytes;
        };
        if copied_into {
            add(8);
            continue;
        }
        add(0);
        let placed = r.host.placed.load(Relaxed);
        // Its first page: what is there of a texture starts there.
        let unmapped = placed && !crate::texture::committed(r.data().cast_const(), 1);
        let idle = crate::host::long_ago(r.host.used.load(Relaxed), 30_000, 60)
            && std::sync::Arc::strong_count(host) == 1
            && crate::host::references(&host.texture) == 1;
        if unmapped {
            add(2);
        }
        if idle {
            add(4);
        }
        if unmapped && idle {
            add(6);
        }
        if placed {
            add(10);
        }
    }
    c
}

/// For the memory line: title textures with a host copy the host renders
/// to or writes and those copies' bytes (twins included); title textures
/// without a host copy and the bytes of title memory they are; host copies
/// of textures the title fills and their bytes; of all title textures,
/// the placed ones with a host copy; the bytes of the twins alone (the
/// layer's own: the console has no second texture for a depth texture
/// shaders write); and, of the host copies of placed textures the title
/// fills, the bytes of those whose title memory is not mapped any more and
/// of those a descriptor slot still shows (the card holds a texture the
/// title has given up for as long as a slot shows it). The last two are
/// estimates from every 16th such texture: asking Windows about the memory
/// of all 16,000 takes too long.
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn census() -> [u64; 10] {
    const EVERY: u64 = 16;
    let mut count = [0u64; 10];
    let mut placed_filled = 0u64;
    let all = textures();
    for &at in all.iter() {
        // SAFETY: as in `let_go_idle`.
        let r = unsafe { &*(at as *const Resource) };
        let made = r.host.made.lock().unwrap_or_else(|e| e.into_inner());
        match made.as_ref() {
            Some((host, twin)) => {
                let kind = if host.filled { 4 } else { 0 };
                count[kind] += 1;
                count[kind + 1] += host.bytes + twin.as_ref().map_or(0, |t| t.colour.bytes);
                count[7] += twin.as_ref().map_or(0, |t| t.colour.bytes);
                let placed = r.host.placed.load(std::sync::atomic::Ordering::Relaxed);
                if placed {
                    count[6] += 1;
                }
                if placed && host.filled {
                    placed_filled += 1;
                    // Its first page: a texture's smallest mips lie there,
                    // and are mapped first and unmapped last.
                    if placed_filled.is_multiple_of(EVERY) && !crate::texture::committed(r.data().cast_const(), 1) {
                        count[8] += host.bytes * EVERY;
                        if crate::host::references(&host.texture) > 1 {
                            count[9] += host.bytes * EVERY;
                        }
                    }
                }
            }
            None => {
                count[2] += 1;
                count[3] += footprint(&r.desc).map_or(0, |f| f.size);
            }
        }
    }
    count
}

/// Host textures alive (the title's and the twins) and the bytes the host
/// gives them, for the memory line.
#[cfg(windows)]
pub(crate) static HOST_TEXTURES: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// The bytes the host allocates for `texture`.
#[cfg(windows)]
fn host_bytes(texture: &windows::Win32::Graphics::Direct3D12::ID3D12Resource) -> u64 {
    let Some(host) = crate::host::host() else {
        return 0;
    };
    // SAFETY: plain queries on a live resource and device.
    unsafe {
        let desc = texture.GetDesc();
        host.device.GetResourceAllocationInfo(0, &[desc]).SizeInBytes
    }
}

#[cfg(windows)]
impl HostTexture {
    pub(crate) fn flags(&self) -> u32 {
        self.flags
    }

    fn new(texture: windows::Win32::Graphics::Direct3D12::ID3D12Resource, state: u32) -> HostTexture {
        use std::sync::atomic::Ordering::Relaxed;
        let bytes = host_bytes(&texture);
        // SAFETY: a live texture.
        let flags = unsafe { texture.GetDesc() }.Flags.0 as u32;
        HOST_TEXTURES[0].fetch_add(1, Relaxed);
        HOST_TEXTURES[1].fetch_add(bytes, Relaxed);
        HostTexture {
            texture,
            state: std::sync::atomic::AtomicU32::new(state),
            bytes,
            filled: false,
            tiled: None,
            base: 0,
            tail: false,
            serial: SERIAL.fetch_add(1, Relaxed) + 1,
            grown: Default::default(),
            holds: Default::default(),
            ahead: crate::texture::ahead::fresh(),
            flags,
            pooled: false,
            recorded: std::sync::atomic::AtomicU32::new(u32::MAX),
            card_wrote: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// A texture made without memory of its own (`crate::tiles`): counted
    /// as a host texture of no bytes.
    fn new_tiled(
        texture: windows::Win32::Graphics::Direct3D12::ID3D12Resource,
        state: u32,
        tiled: crate::tiles::Tiled,
    ) -> HostTexture {
        HOST_TEXTURES[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // SAFETY: a live texture.
        let flags = unsafe { texture.GetDesc() }.Flags.0 as u32;
        HostTexture {
            texture,
            state: std::sync::atomic::AtomicU32::new(state),
            bytes: 0,
            filled: false,
            tiled: Some(tiled),
            base: 0,
            tail: false,
            serial: SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1,
            grown: Default::default(),
            holds: Default::default(),
            ahead: crate::texture::ahead::fresh(),
            flags,
            pooled: false,
            recorded: std::sync::atomic::AtomicU32::new(u32::MAX),
            card_wrote: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

#[cfg(windows)]
impl Drop for HostTexture {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        HOST_TEXTURES[0].fetch_sub(1, Relaxed);
        HOST_TEXTURES[1].fetch_sub(self.bytes, Relaxed);
        if self.filled {
            KINDS[0].fetch_sub(1, Relaxed);
            KINDS[1].fetch_sub(self.bytes, Relaxed);
        }
    }
}

/// How many textures placed in title memory have a resource of ours
/// (never released), for the memory line.
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn placed_textures() -> u64 {
    PLACED_TEXTURES.lock().unwrap_or_else(|e| e.into_inner()).len() as u64
}

/// Moves the host texture to `wanted` (a host texture state); the host
/// resource and the state it leaves, if that is a change.
#[cfg(windows)]
pub(crate) fn host_transition(this: This, wanted: u32) -> Option<(*mut c_void, u32)> {
    use windows::core::Interface;
    let (host, _) = hosted(this, true)?;
    let before = host
        .state
        .swap(wanted, std::sync::atomic::Ordering::Relaxed);
    (before != wanted).then(|| (host.texture.as_raw(), before))
}

/// The host GPU's copy of a title resource, if it has one: made now if
/// this is the texture's first use ([`Hosted`]). Asking is a use; what must
/// not count as one asks `host_made`.
#[cfg(windows)]
pub(crate) fn host_resource<'a>(
    this: This,
) -> Option<&'a windows::Win32::Graphics::Direct3D12::ID3D12Resource> {
    if this.is_null() {
        return None;
    }
    let (host, _) = hosted(this, true)?;
    // SAFETY: the resource holds the host copy until the title releases it
    // (lists and slots that name the texture hold it past that), or until
    // `let_go_idle` takes it: only from a texture the title fills, that no
    // list and no slot holds and that nothing asked for in seconds (the
    // asking just made is noted under the same lock).
    Some(unsafe { &*std::ptr::from_ref(&host.texture) })
}

/// A title texture's host copy, shared, for a list to hold.
#[cfg(windows)]
pub(crate) fn host_copy(this: This) -> Option<std::sync::Arc<HostTexture>> {
    if this.is_null() {
        return None;
    }
    hosted(this, true).map(|made| made.0)
}

/// A title texture's host copy if it is made; not made here.
#[cfg(windows)]
pub(crate) fn made_copy(this: This) -> Option<std::sync::Arc<HostTexture>> {
    if this.is_null() {
        return None;
    }
    hosted(this, false).map(|made| made.0)
}

/// A host texture like the title's: same description, in GPU memory,
/// without the console's layout and flags; and whether it has memory of
/// its own (not a piece of a block, `crate::pool`).
#[cfg(windows)]
fn host_texture(
    desc: &ResourceDesc,
    state: u32,
    clear: *const c_void,
) -> Option<(windows::Win32::Graphics::Direct3D12::ID3D12Resource, bool)> {
    host_texture_as(desc, state, clear, false)
}

#[cfg(all(windows, test))]
thread_local! {
    /// The error the next texture [`host_texture_as`] makes on this thread
    /// fails with (upstream 4e95a78's test).
    static FAIL_NEXT: std::cell::Cell<Option<windows::core::HRESULT>> = const { std::cell::Cell::new(None) };
}

/// [`host_texture`]; `reserved`: without memory of its own, to be given
/// tiles level by level (`crate::tiles`).
#[cfg(windows)]
fn host_texture_as(
    desc: &ResourceDesc,
    state: u32,
    clear: *const c_void,
    reserved: bool,
) -> Option<(windows::Win32::Graphics::Direct3D12::ID3D12Resource, bool)> {
    use windows::Win32::Graphics::Direct3D12::*;
    const PC_FLAGS: u32 = 0xf; // render target, depth, UAV, no shader access
    if desc.dimension == BUFFER {
        return None;
    }
    let host = crate::host::host()?;
    let mut d = *desc;
    d.layout = 0;
    d.alignment = 0;
    d.flags &= PC_FLAGS;
    // PC refuses depth targets that are also unordered-access (the console
    // allows both; the title's shadow maps ask for it).
    // The host copy loses the unordered access: shaders write a colour
    // twin instead ([`Twin`]).
    const DEPTH: u32 = 2;
    const UNORDERED: u32 = 4;
    if d.flags & DEPTH != 0 {
        if d.flags & UNORDERED != 0 {
            DEPTH_UNORDERED[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        d.flags &= !UNORDERED;
    }
    d.format = pc_format(d.format);
    const TARGET: u32 = 1;
    if d.flags & (TARGET | UNORDERED) != 0 {
        d.format = target_format(d.format);
    }
    const DENY_SHADER_RESOURCE: u32 = 8;
    if d.flags & DENY_SHADER_RESOURCE == 0
        && let Some(&(_, typeless)) = DEPTH_FORMATS.iter().find(|&&(f, _)| f == d.format)
    {
        d.format = typeless;
    }
    // The host has no sRGB unordered access: such textures are typeless,
    // their unordered access views plain UNORM.
    if d.flags & UNORDERED != 0 && SRGB.iter().any(|&(srgb, _)| srgb == d.format) {
        d.format = family(d.format);
    }
    // The clear value names the format too: format, then color or depth
    // and stencil (5 words).
    let clear_value = (!clear.is_null()).then(|| {
        // SAFETY: the title passes a whole clear value.
        let mut value = unsafe { clear.cast::<[u32; 5]>().read_unaligned() };
        value[0] = pc_format(value[0]);
        if d.flags & (TARGET | UNORDERED) != 0 {
            value[0] = target_format(value[0]);
        }
        value
    });
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    // A texture the title fills, whose every subresource is uploaded before
    // anything reads it: cut from a block (what the piece held before shows
    // in no texel).
    if fills(desc)
        && d.sample_count <= 1
        && clear_value.is_none()
        && texture_layout(desc).is_some_and(|t| !t.planes[0].layout.desc.swizzle.xor())
        // SAFETY: the description has the host layout, as below.
        && let Some(t) = unsafe {
            crate::pool::texture(
                &host.device,
                (&raw const d).cast(),
                D3D12_RESOURCE_STATES(crate::host::legal_for(d.flags, crate::host::texture_state(state)) as i32),
            )
        }
    {
        crate::host::name(&t, || {
            format!("texture {}x{} f{} flags {:#x} (in a block)", d.width, d.height, d.format, d.flags)
        });
        return Some((t, false));
    }
    let mut texture: Option<ID3D12Resource> = None;
    let made = if reserved {
        // The card's own layout of 64 KiB tiles, which a reserved texture
        // must have.
        d.layout = D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE.0 as u32;
        // SAFETY: as below; a texture the title fills has no clear value.
        unsafe {
            host.device.CreateReservedResource(
                (&raw const d).cast(),
                D3D12_RESOURCE_STATES(
                    crate::host::legal_for(d.flags, crate::host::texture_state(state)) as i32,
                ),
                None,
                &mut texture,
            )
        }
    } else {
        // SAFETY: the description has the host layout (checked size, same
        // fields); the out pointer is ours.
        unsafe {
            host.device.CreateCommittedResource(
                &heap,
                crate::host::not_resident(),
                (&raw const d).cast(),
                D3D12_RESOURCE_STATES(
                    crate::host::legal_for(d.flags, crate::host::texture_state(state)) as i32,
                ),
                // The same layout: format, then color or depth and stencil.
                clear_value.as_ref().map(|v| std::ptr::from_ref(v).cast()),
                &mut texture,
            )
        }
    };
    // A test's refusal (`a_texture_refused_for_want_of_memory_is_made_later`).
    #[cfg(test)]
    let (made, made_up) = match FAIL_NEXT.take() {
        Some(code) => {
            texture = None;
            (Err(code.into()), true)
        }
        None => (made, false),
    };
    #[cfg(not(test))]
    let made_up = false;
    if let Err(e) = &made {
        // After upstream GPL-3.0 fork, commit
        // ab75ac3e02ba "Back off refused GPU pool growth and defer uploads
        // under low commit": an out-of-memory refusal holds uploads back for
        // half a second. Here: `crate::host::commit` (`VICTORY_COMMIT_GUARD=0`:
        // nothing is noted, as before). (Not a refusal a test made up: its
        // half second would hold back the uploads of the tests beside it.)
        if !made_up {
            crate::host::commit::allocation_error(e);
        }
        REFUSED_FOR_MEMORY.set(crate::host::out_of_memory(e));
        tracing::warn!(error = %e, desc = ?d, reserved, "no host texture");
    }
    if !reserved
        && made.is_ok()
        && let Some(t) = &texture
        && let Err(e) = crate::host::resident(&host.device, t, crate::host::NewMemory::Texture)
            .and_then(|()| crate::host::mapped(&host.device, crate::host::NewMemory::Texture))
    {
        // No room to make it resident is no memory for it too, for the
        // commit guard as for [`texture_retry`] (upstream 4e95a78).
        if texture_retry() {
            crate::host::commit::allocation_error(&e);
            REFUSED_FOR_MEMORY.set(crate::host::out_of_memory(&e));
        }
        tracing::warn!(error = %e, desc = ?d, "a host texture is not resident");
        texture = None;
    }
    if let Some(t) = &texture {
        crate::host::name(t, || {
            format!("texture {}x{} f{} flags {:#x}", d.width, d.height, d.format, d.flags)
        });
        // What the host renders to or writes stays on the card first.
        if !reserved && d.flags & (TARGET | DEPTH | UNORDERED) != 0 {
            host.hint_residency(&[(t, crate::host::Host::RESIDENCY_HIGH)]);
        }
    }
    texture.map(|t| (t, !reserved))
}

/// The host copy of title texture `this` if it is made and was made
/// without memory of its own (`crate::tiles`); it is not made here.
#[cfg(windows)]
pub(crate) fn host_tiled(this: This) -> Option<OnDemand> {
    if this.is_null() || !crate::tiles::wanted() {
        return None;
    }
    hosted(this, false).map(|(host, _)| host).filter(|host| host.tiled.is_some()).map(OnDemand)
}

/// The first level a view of title texture `this` may show, if its host
/// copy is made level by level and some level is held and has memory
/// (`crate::tiles::Tiled::first_shown`).
#[cfg(windows)]
pub(crate) fn first_shown(this: This) -> Option<u32> {
    host_tiled(this)?.tiled()?.first_shown()
}

/// A host copy made without memory of its own (`crate::tiles`), as a
/// submission-time step of a list holds it.
#[cfg(windows)]
#[derive(Clone)]
pub(crate) struct OnDemand(std::sync::Arc<HostTexture>);

// SAFETY: the host texture is named only in queue calls, which any thread
// may make; what it has been given is behind a lock of its own.
#[cfg(windows)]
unsafe impl Send for OnDemand {}
// SAFETY: as above.
#[cfg(windows)]
unsafe impl Sync for OnDemand {}

#[cfg(windows)]
impl OnDemand {
    pub(crate) fn tiled(&self) -> Option<&crate::tiles::Tiled> {
        self.0.tiled.as_ref()
    }

    /// [`crate::tiles::Tiled::uploaded`]; `memory`: where the texture's
    /// title memory starts (its slots are made again if the first level
    /// they may show changed).
    /// Held levels get tiles only while the card has room ([`room`]).
    /// Returns whether a level is still without its tiles: the caller
    /// records the upload again later.
    pub(crate) fn uploaded(&self, held: u32, memory: u64) -> bool {
        let Some(tiled) = &self.0.tiled else {
            return false;
        };
        let (changed, waiting) = tiled.uploaded(&self.0.texture, held, room(crate::host::host().and_then(|h| h.video_memory())));
        if changed {
            crate::descriptor::host::levels_changed(memory);
        }
        if waiting {
            crate::tiles::STATS[9].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        waiting
    }

    /// [`crate::tiles::Tiled::card_wrote`]; `memory` as above.
    pub(crate) fn card_wrote(&self, levels: u32, memory: u64) {
        if let Some(tiled) = &self.0.tiled
            && tiled.card_wrote(&self.0.texture, levels)
        {
            crate::descriptor::host::levels_changed(memory);
        }
    }
}

/// A texture's console memory and description; `None` for buffers.
pub(crate) fn texture_memory(this: This) -> Option<(*const u8, ResourceDesc)> {
    if this.is_null() {
        return None;
    }
    let r = resource(this);
    (r.desc.dimension != BUFFER).then(|| (r.data().cast_const(), r.desc))
}

/// A buffer's host memory and size; `None` for textures.
pub(crate) fn buffer_bytes(this: This) -> Option<(*mut u8, u64)> {
    if this.is_null() {
        return None;
    }
    let r = resource(this);
    (r.desc.dimension == BUFFER).then(|| (r.data(), r.desc.width))
}

impl Resource {
    fn data(&self) -> *mut u8 {
        match self.backing {
            Backing::Committed(ref memory) => memory.ptr.as_ptr(),
            // SAFETY: placement checked the range lies inside the heap.
            Backing::Placed { heap, offset } => unsafe {
                (*heap.cast::<Heap>())
                    .memory
                    .ptr
                    .as_ptr()
                    .add(offset as usize)
            },
            Backing::Address(at) => at,
        }
    }
}

fn resource<'a>(this: This) -> &'a Resource {
    // SAFETY: called from resource methods only.
    unsafe { &*this.cast::<Resource>() }
}

// The creation methods' own arguments, passed on.
#[allow(clippy::too_many_arguments)]
fn hand_out_resource(
    desc: ResourceDesc,
    heap: HeapProperties,
    heap_flags: u32,
    backing: Backing,
    state: u32,
    clear: *const c_void,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    #[cfg(not(windows))]
    let _ = (state, clear);
    let resource = Box::new(Resource {
        object: Object::new(&RESOURCE_FUNCTIONS),
        desc,
        heap,
        heap_flags,
        backing,
        #[cfg(windows)]
        host: Hosted {
            made: std::sync::Mutex::new(None),
            state,
            // SAFETY: the title passes a whole clear value (the format,
            // then color or depth and stencil: 5 words) or null.
            clear: (!clear.is_null()).then(|| unsafe { clear.cast::<[u32; 5]>().read_unaligned() }),
            used: std::sync::atomic::AtomicU64::new(0),
            refused_until: std::sync::atomic::AtomicU64::new(0),
            refusals: std::sync::atomic::AtomicU8::new(0),
            gone: std::sync::atomic::AtomicBool::new(false),
            placed: std::sync::atomic::AtomicBool::new(false),
            wanted: std::sync::atomic::AtomicU32::new(u32::MAX),
            grown: std::sync::atomic::AtomicBool::new(false),
            residency: std::sync::atomic::AtomicU8::new(NOT_OWN),
            cut: std::sync::atomic::AtomicU32::new(0),
        },
    });
    // The host copy waits for the texture's first use ([`Hosted`]).
    #[cfg(windows)]
    if desc.dimension != BUFFER {
        let at = std::ptr::from_ref::<Resource>(&resource) as usize;
        textures().insert(at);
        crate::texback::replaced(resource.data() as usize);
        if !fills(&desc) {
            TARGETS_AT
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(resource.data() as u64, at);
        }
        if !lazy() {
            let _ = hosted(at as This, true);
        }
    }
    #[cfg(windows)]
    if desc.dimension == BUFFER {
        let at = std::ptr::from_ref::<Resource>(&resource) as usize;
        crate::shadow::register(at, resource.data() as u64, desc.width);
    }
    object::hand_out(RESOURCE, resource, iid, &RESOURCE_IIDS, out)
}

#[cfg(windows)]
impl Drop for Resource {
    fn drop(&mut self) {
        // Out of the table of textures first: the sweep looks at the ones
        // in it, under its lock.
        if self.desc.dimension != BUFFER {
            let address = std::ptr::from_ref::<Resource>(self) as usize;
            crate::texback::released(address);
            textures().remove(&address);
            if !fills(&self.desc) {
                let mut targets = TARGETS_AT.lock().unwrap_or_else(|e| e.into_inner());
                let at = self.data() as u64;
                if targets.get(&at) == Some(&(std::ptr::from_ref::<Resource>(self) as usize)) {
                    targets.remove(&at);
                }
            }
        }
        let made = self
            .host
            .made
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        #[cfg(feature = "diagnostics")]
        if made
            .as_ref()
            .is_some_and(|(h, _)| std::sync::Arc::strong_count(h) > 1)
        {
            // Before lists held their textures, this freed a texture that
            // lists not yet run still named (a GPU page fault).
            static OUTLIVED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = OUTLIVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n.is_power_of_two() {
                tracing::info!(
                    n,
                    desc = ?self.desc,
                    "a title texture was released while a list not yet reset names its host copy"
                );
            }
        }
        // A running submission may still read the host copy (and a depth
        // texture's colour twin).
        if let Some((host, twin)) = made {
            crate::host::retire(host);
            if let Some(twin) = twin {
                crate::host::retire(twin);
            }
        }
        crate::texture::forget(std::ptr::from_ref::<Resource>(self) as usize);
        crate::descriptor::forget(std::ptr::from_ref::<Resource>(self) as usize);
        if self.desc.dimension == BUFFER {
            crate::shadow::unregister(
                std::ptr::from_ref::<Resource>(self) as usize,
                self.data() as u64,
            );
        }
    }
}

pub(crate) extern "system" fn create_committed(
    _: This,
    heap: *const HeapProperties,
    heap_flags: u32,
    desc: *const ResourceDesc,
    state: u32,
    clear: *const c_void,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes both descriptions (null is tolerated).
    let (Some(&heap), Some(&desc)) = (unsafe { (heap.as_ref(), desc.as_ref()) }) else {
        return E_INVALIDARG;
    };
    let Some(info) = footprint(&desc) else {
        tracing::warn!(?desc, "no size for this resource");
        return E_INVALIDARG;
    };
    tracing::debug!(
        dimension = desc.dimension,
        size = info.size,
        heap = heap.kind,
        "committed resource"
    );
    let memory = match Memory::for_resource(info, heap.kind) {
        Ok(memory) => memory,
        Err(hr) => return hr,
    };
    hand_out_resource(
        desc,
        heap,
        heap_flags,
        Backing::Committed(memory),
        state,
        clear,
        iid,
        out,
    )
}

/// Device method 0x3f8, inferred from the title's calls
/// (title code: its back buffers, heap flag "allow display"): the
/// committed resource creation of the enhanced-barrier interfaces,
/// `(heap, heap flags, desc1, initial layout, clear value, protected
/// session, castable format count, castable formats, iid, out)`. The host
/// copy starts in the common state, as layouts are not tracked.
#[allow(clippy::too_many_arguments)] // the method's own arguments
pub(crate) extern "system" fn create_committed3(
    this: This,
    heap: *const HeapProperties,
    heap_flags: u32,
    desc: *const ResourceDesc,
    _layout: u32,
    clear: *const c_void,
    _protected: This,
    _castable_count: u32,
    _castable: *const u32,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    const COMMON: u32 = 0;
    create_committed(this, heap, heap_flags, desc, COMMON, clear, iid, out)
}

/// Registered page pools (64 KiB pages the title maps into tiled
/// resources by page index): first address and page count; a handle is an
/// index plus one, a removed pool leaves `None`.
static PAGE_POOLS: std::sync::Mutex<Vec<Option<(u64, u32)>>> = std::sync::Mutex::new(Vec::new());

fn page_pools() -> std::sync::MutexGuard<'static, Vec<Option<(u64, u32)>>> {
    PAGE_POOLS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Device method 0x250: `(address, page count, handle out)`.
pub(crate) extern "system" fn register_page_pool(
    _: This,
    address: u64,
    pages: u32,
    out: *mut usize,
) -> HRESULT {
    if out.is_null() || pages == 0 || !address.is_multiple_of(KIB_64) {
        return E_INVALIDARG;
    }
    tracing::debug!(address = format_args!("{address:#x}"), pages, "page pool");
    let mut pools = page_pools();
    pools.push(Some((address, pages)));
    // SAFETY: checked non-null; the title passes room for a handle.
    unsafe { out.write(pools.len()) };
    S_OK
}

/// Device method 0x258.
pub(crate) extern "system" fn unregister_page_pool(_: This, handle: usize) -> HRESULT {
    match page_pools().get_mut(handle.wrapping_sub(1)) {
        Some(pool @ Some(_)) => {
            *pool = None;
            S_OK
        }
        _ => E_INVALIDARG,
    }
}

/// Device method 0x428, one of the `ID3D12Device9` to `ID3D12Device11`
/// additions. From the title's call: a resource at a virtual
/// address the title mapped itself, `(address, desc1, initial barrier
/// layout, clear value, iid, out)`, as `CreatePlacedResourceX` with
/// enhanced barriers. GPU addresses are CPU addresses, so the resource is
/// a view of that memory. `D3D12_RESOURCE_DESC1` starts like the plain
/// description; its sampler feedback region is not needed.
pub(crate) extern "system" fn create_at_address(
    _: This,
    address: u64,
    desc: *const ResourceDesc,
    layout: u32,
    clear: *const c_void,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    if address == 0 {
        return E_INVALIDARG;
    }
    tracing::debug!(
        address = format_args!("{address:#x}"),
        dimension = desc.dimension,
        width = desc.width,
        layout,
        "resource at an address"
    );
    const COMMON: u32 = 0;
    const DEFAULT: HeapProperties = HeapProperties {
        kind: 1,
        cpu_pages: 0,
        pool: 0,
        creation_nodes: 1,
        visible_nodes: 1,
    };
    hand_out_resource(
        desc,
        DEFAULT,
        0,
        Backing::Address(address as *mut u8),
        COMMON,
        clear,
        iid,
        out,
    )
}

/// Textures the title placed in its own memory and views without a
/// resource object (device 0x230, the title's streamed and
/// interface textures), by address and description: a resource at that
/// address, made the first time a draw reads one.
/// The resource is kept for the run (views and lists name its address);
/// its host texture is let go when nothing names it any more
/// ([`let_go_idle`]) and made again on the next use.
#[cfg(windows)]
static PLACED_TEXTURES: std::sync::Mutex<std::collections::BTreeMap<(u64, [u32; 14]), usize>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// The placed textures asked for with filler words set, as asked
/// (address and description): for [`PLACED_TWINS`].
#[cfg(windows)]
static PLACED_RAW: std::sync::Mutex<std::collections::BTreeSet<(u64, [u32; 14])>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// Placed textures asked for a second time with other filler words in
/// their description: each was a second resource and host copy before the
/// filler was left out of the key ([`texture_at`]).
#[cfg(windows)]
static PLACED_TWINS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// See [`PLACED_TWINS`].
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn placed_filler_twins() -> u64 {
    PLACED_TWINS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Width, height and format of texture `this` (`None` for a buffer), for
/// `crate::texback`'s kinds.
#[cfg(windows)]
pub(crate) fn texture_kind(this: This) -> Option<(u64, u32, u32)> {
    if this.is_null() {
        return None;
    }
    let r = resource(this);
    const TEXTURE_2D: u32 = 3;
    (r.desc.dimension == TEXTURE_2D && r.desc.sample_count <= 1)
        .then_some((r.desc.width, r.desc.height, r.desc.format))
}

/// Texture `this`'s console layout (one plane), where it lies in title
/// memory and the bytes that layout takes, for `crate::texback`; `None` for
/// a buffer or a texture with planes.
#[cfg(windows)]
pub(crate) fn title_texture(this: This) -> Option<(victory_formats::texture::Layout, usize, usize)> {
    if this.is_null() {
        return None;
    }
    let r = resource(this);
    if r.desc.dimension == BUFFER {
        return None;
    }
    let t = texture_layout(&r.desc)?;
    let [plane] = &t.planes[..] else {
        return None;
    };
    Some((plane.layout.clone(), r.data() as usize, t.size as usize))
}

/// The bytes of title memory a texture with the resource description
/// `desc` (as words) takes where the title placed it; `None` for a buffer
/// and for a description without a size.
#[cfg(windows)]
pub(crate) fn placed_bytes(desc: &[u32; 14]) -> Option<u64> {
    // SAFETY: 56 bytes in the description's layout.
    let d = unsafe { desc.as_ptr().cast::<ResourceDesc>().read_unaligned() };
    if d.dimension == BUFFER {
        return None;
    }
    footprint(&d).map(|f| f.size)
}

/// The resource [`texture_at`] made for a texture placed at `address` with
/// the description `desc`, if it has made one: never made here (for the
/// frame listing, which must not count as a use).
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn texture_at_made(address: u64, desc: &[u32; 14]) -> Option<This> {
    let mut key = *desc;
    key[1] = 0;
    key[13] = 0;
    let map = PLACED_TEXTURES.lock().unwrap_or_else(|e| e.into_inner());
    let found = map.get(&(address, key)).or_else(|| map.get(&(address, *desc)))?;
    Some(*found as This)
}

/// From which level on the title texture's host copy was filled, and from
/// which bytes (diagnostics).
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn texture_held(resource: This) -> Option<(u32, crate::texture::Digest)> {
    *made_copy(resource)?.holds.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether the answers of [`texture_at`] (and of `crate::texture`'s
/// `staging`) are found without the one lock of their table
/// (`VICTORY_TEXTURE_AT_SHARED=0`: under it every time, as before).
#[cfg(windows)]
pub(crate) fn texture_at_shared() -> bool {
    crate::toggled!("VICTORY_TEXTURE_AT_SHARED")
}

/// Shards of [`PLACED_FOUND`], by address.
#[cfg(windows)]
const PLACED_SHARDS: usize = 16;

/// The answers [`texture_at`] has given, by the ask as it came (address and
/// description, filler words and all), for readers to share: every view of
/// a placed texture asks on every recording thread, and all of them stood
/// in line for [`PLACED_TEXTURES`]' one lock, also while it was held to make
/// a new texture (tens of milliseconds of the recording threads' time a frame).
///
/// An ask is put here after [`texture_at_locked`] answered it, and never
/// taken out: `PLACED_TEXTURES` only grows, and its resources are kept for
/// the run (they are ours; the title never has them to release, and what it
/// creates and releases at the same addresses are other resources, which
/// are not in that table). So an answer once given is the answer for good,
/// here and in what a thread remembers ([`PLACED_LAST`]). An ask is first
/// seen under the lock, where the filler twins are counted.
#[cfg(windows)]
#[allow(clippy::type_complexity)]
static PLACED_FOUND: [std::sync::RwLock<std::collections::BTreeMap<(u64, [u32; 14]), usize>>; PLACED_SHARDS] =
    [const { std::sync::RwLock::new(std::collections::BTreeMap::new()) }; PLACED_SHARDS];

/// Answers a thread remembers of [`PLACED_FOUND`].
#[cfg(windows)]
const PLACED_LAST_MOST: usize = 1024;

#[cfg(windows)]
thread_local! {
    /// This thread's last answers of [`texture_at`], by a hash of the ask:
    /// (address, description, resource; 0: none).
    static PLACED_LAST: std::cell::RefCell<Vec<(u64, [u32; 14], usize)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Where an ask is in [`PLACED_FOUND`] (the shard) and in [`PLACED_LAST`].
#[cfg(windows)]
fn placed_at(address: u64, desc: &[u32; 14]) -> (usize, usize) {
    let word = |i: usize, by: u32| u64::from(desc[i]) << by;
    let words = word(4, 0) ^ word(6, 16) ^ word(7, 32) ^ word(8, 48) ^ (word(1, 0) ^ word(13, 32)).rotate_left(7);
    let h = (address ^ words.rotate_left(23)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let shard = (address >> 12).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 60;
    (shard as usize % PLACED_SHARDS, (h >> 54) as usize % PLACED_LAST_MOST)
}

/// The answer [`texture_at`] gave to the same ask before, from what this
/// thread remembers, else from what all threads share.
#[cfg(windows)]
fn placed_found(address: u64, desc: &[u32; 14]) -> Option<This> {
    let (shard, last) = placed_at(address, desc);
    let mine = PLACED_LAST.with(|l| {
        let l = l.borrow();
        l.get(last).filter(|(a, d, r)| *r != 0 && *a == address && d == desc).map(|&(_, _, r)| r)
    });
    if let Some(found) = mine {
        return Some(found as This);
    }
    let found = *PLACED_FOUND[shard].read().unwrap_or_else(|e| e.into_inner()).get(&(address, *desc))?;
    placed_last(last, address, desc, found);
    Some(found as This)
}

#[cfg(windows)]
fn placed_last(last: usize, address: u64, desc: &[u32; 14], found: usize) {
    PLACED_LAST.with(|l| {
        let mut l = l.borrow_mut();
        if l.is_empty() {
            l.resize(PLACED_LAST_MOST, (0, [0; 14], 0));
        }
        l[last] = (address, *desc, found);
    });
}

/// The resource for a texture placed at `address` with the resource
/// description `desc` (as words), made on first use.
#[cfg(windows)]
pub(crate) fn texture_at(address: u64, desc: &[u32; 14]) -> Option<This> {
    texture_at_by(address, desc, texture_at_shared())
}

/// [`texture_at`]; `shared`: [`texture_at_shared`].
#[cfg(windows)]
fn texture_at_by(address: u64, desc: &[u32; 14], shared: bool) -> Option<This> {
    if !shared {
        return texture_at_locked(address, desc);
    }
    if let Some(found) = placed_found(address, desc) {
        return Some(found);
    }
    // First seen (or no texture: a buffer's description, or none could be
    // made, which is asked again every time as before).
    let found = texture_at_locked(address, desc)?;
    let (shard, last) = placed_at(address, desc);
    PLACED_FOUND[shard].write().unwrap_or_else(|e| e.into_inner()).insert((address, *desc), found as usize);
    placed_last(last, address, desc, found as usize);
    Some(found)
}

/// [`texture_at`] under the table's lock, as it always was.
#[cfg(windows)]
fn texture_at_locked(address: u64, desc: &[u32; 14]) -> Option<This> {
    // Words 1 and 13 of the description are the filler of its layout
    // (after the dimension, and at the end): the title leaves there what
    // its stack held, and two views of one texture that differed only in
    // them each got a resource and a host copy of their own, on a card
    // that fills as a scene loads. They are not part of what a texture is.
    // `VICTORY_PLACED_FILLER=1`: kept in the key, as before.
    static KEPT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let raw = *desc;
    let mut key = *desc;
    if !*KEPT.get_or_init(|| std::env::var_os("VICTORY_PLACED_FILLER").is_some_and(|v| v == "1")) {
        key[1] = 0;
        key[13] = 0;
    }
    let desc = &key;
    let mut map = PLACED_TEXTURES.lock().unwrap_or_else(|e| e.into_inner());
    // How many resources the filler alone would have added (a count for
    // the log: `placed_filler_twins`).
    if raw != key {
        let mut seen = PLACED_RAW.lock().unwrap_or_else(|e| e.into_inner());
        if seen.insert((address, raw)) {
            let others = seen.range((address, [0; 14])..=(address, [u32::MAX; 14])).filter(|(_, r)| {
                let mut blank = *r;
                blank[1] = 0;
                blank[13] = 0;
                blank == key
            });
            if others.count() > 1 {
                PLACED_TWINS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    if let Some(&r) = map.get(&(address, *desc)) {
        return Some(r as This);
    }
    // SAFETY: 56 bytes in the description's layout.
    let d = unsafe { desc.as_ptr().cast::<ResourceDesc>().read_unaligned() };
    if d.dimension == BUFFER {
        return None;
    }
    let mut out = std::ptr::null();
    let made = create_at_address(
        std::ptr::null(),
        address,
        &d,
        0,
        std::ptr::null(),
        &RESOURCE_IIDS[0],
        &mut out,
    );
    if made != S_OK {
        return None;
    }
    map.insert((address, *desc), out as usize);
    crate::texture::placed_in_host_written(address, desc);
    resource(out)
        .host
        .placed
        .store(true, std::sync::atomic::Ordering::Relaxed);
    Some(out)
}

pub(crate) extern "system" fn create_placed(
    _: This,
    heap: This,
    offset: u64,
    desc: *const ResourceDesc,
    state: u32,
    clear: *const c_void,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    if heap.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title passes one of our heaps.
    let h = unsafe { &*heap.cast::<Heap>() };
    let fits = footprint(&desc).is_some_and(|info| {
        offset.is_multiple_of(info.alignment)
            && offset
                .checked_add(info.size)
                .is_some_and(|end| end <= h.desc.size)
    });
    if !fits {
        tracing::warn!(
            ?desc,
            offset,
            heap = h.desc.size,
            "placed resource does not fit"
        );
        return E_INVALIDARG;
    }
    object::add_ref(heap);
    let backing = Backing::Placed { heap, offset };
    hand_out_resource(
        desc,
        h.desc.properties,
        h.desc.flags,
        backing,
        state,
        clear,
        iid,
        out,
    )
}

extern "system" fn resource_query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(
        RESOURCE,
        iid,
        object::is(iid, &RESOURCE_IIDS).then_some(this),
        out,
    )
}
extern "system" fn resource_release(this: This) -> u32 {
    object::release_boxed::<Resource>(this)
}
/// Memory is always mapped (subresources are not told apart yet); a null
/// `out` only asks for residency.
extern "system" fn map(this: This, _sub: u32, _read: *const c_void, out: *mut *mut u8) -> HRESULT {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a pointer.
        unsafe { out.write(resource(this).data()) };
    }
    S_OK
}
extern "system" fn unmap(_: This, _sub: u32, _written: *const c_void) {}
extern "system" fn resource_desc(this: This, out: *mut ResourceDesc) {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a description.
        unsafe { out.write(resource(this).desc) };
    }
}
/// Device method `GetCustomHeapProperties`, returning its struct through a
/// hidden first argument: what a standard heap type is as a custom heap.
/// Memory is unified and cache-coherent (as the architecture query says),
/// so every type is in pool L0, default heaps are not CPU-visible and the
/// others are write-back.
pub(crate) extern "system" fn custom_heap_properties(
    out: *mut HeapProperties,
    _: This,
    nodes: u32,
    kind: u32,
) -> *mut HeapProperties {
    const CUSTOM: u32 = 4;
    const NOT_AVAILABLE: u32 = 1;
    const WRITE_BACK: u32 = 3;
    const L0: u32 = 1;
    let nodes = nodes.max(1);
    let cpu_pages = match kind {
        1 => NOT_AVAILABLE,
        2 | 3 => WRITE_BACK,
        _ => 0,
    };
    let properties = HeapProperties {
        kind: CUSTOM,
        cpu_pages,
        pool: if cpu_pages == 0 { 0 } else { L0 },
        creation_nodes: nodes,
        visible_nodes: nodes,
    };
    // SAFETY: the title passes room for the result (null is tolerated).
    if let Some(out) = unsafe { out.as_mut() } {
        *out = properties;
    }
    out
}

extern "system" fn gpu_address(this: This) -> u64 {
    let r = resource(this);
    let at = r.data() as u64;
    // The console's GPU addresses fit in 40 bits; its ray-tracing builder
    // refuses others (diagnostics: which memory breaks that).
    #[cfg(feature = "diagnostics")]
    if at >> 40 != 0 {
        static ONCE: [std::sync::Once; 2] = [std::sync::Once::new(), std::sync::Once::new()];
        let backing = match r.backing {
            Backing::Committed(ref m) => format!("committed, attributes {:?}", m.attributes),
            Backing::Placed { offset, .. } => format!("placed in a heap at {offset:#x}"),
            Backing::Address(_) => "at a title address".into(),
        };
        ONCE[usize::from(r.desc.dimension == BUFFER)].call_once(|| {
            tracing::warn!(address = format_args!("{at:#x}"), backing, desc = ?r.desc, "a GPU address above 40 bits")
        });
    }
    at
}
extern "system" fn heap_properties(
    this: This,
    properties: *mut HeapProperties,
    flags: *mut u32,
) -> HRESULT {
    let r = resource(this);
    // SAFETY: either pointer may be null; otherwise the title passes room.
    unsafe {
        if let Some(p) = properties.as_mut() {
            *p = r.heap;
        }
        if let Some(f) = flags.as_mut() {
            *f = r.heap_flags;
        }
    }
    S_OK
}

/// IUnknown 3, object 4, device child 1, heap 1.
static HEAP_FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<HEAP, 24>());
    f[0] = heap_query as *const ();
    f[2] = heap_release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    f[8] = heap_desc as *const (); // 0x40
    Table(f)
};

/// IUnknown 3, object 4, device child 1, resource 10 (7 as on PC, then 3
/// console ones).
static RESOURCE_FUNCTIONS: Table<32> = {
    let mut f = object::d3d12_object(object::missing_table::<RESOURCE, 32>());
    f[0] = resource_query as *const ();
    f[2] = resource_release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    f[8] = map as *const (); // 0x40
    f[9] = unmap as *const (); // 0x48
    f[10] = resource_desc as *const (); // 0x50
    f[11] = gpu_address as *const (); // 0x58
    f[14] = heap_properties as *const (); // 0x70
    Table(f)
};

#[cfg(test)]
pub(crate) fn test_buffer(width: u64) -> This {
    let heap = HeapProperties {
        kind: 1,
        cpu_pages: 0,
        pool: 0,
        creation_nodes: 0,
        visible_nodes: 0,
    };
    let desc = ResourceDesc {
        dimension: BUFFER,
        alignment: 0,
        width,
        height: 1,
        depth_or_array_size: 1,
        mip_levels: 1,
        format: 0,
        sample_count: 1,
        sample_quality: 0,
        layout: 1,
        flags: 0,
    };
    let mut out = std::ptr::null();
    let hr = create_committed(
        std::ptr::null(),
        &heap,
        0,
        &desc,
        0,
        std::ptr::null(),
        &RESOURCE_IIDS[0],
        &mut out,
    );
    assert_eq!(hr, S_OK);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;

    const MIB: u64 = 1 << 20;

    /// The sweep lets host copies of textures go for the card's own shortage:
    /// with two thirds of its budget in use the card kept nearly 2 GB for
    /// targets. Counted with that room it stood at 92 %, and at each look
    /// hundreds of MB of copies idle for 5 s went; by its use, 67 %, only
    /// copies idle for ten minutes
    /// go. With the card truly past its line they go as before.
    #[cfg(windows)]
    #[test]
    fn textures_are_let_go_for_the_cards_shortage_not_for_room_kept() {
        let budget = 7168 * MIB;
        let (used, free_blocks, kept) = (5100 * MIB, 300 * MIB, 1800 * MIB);
        let before = crate::host::vram_keep(budget, let_go_use(used, free_blocks, kept));
        assert_eq!(before, (5_000, 6600 * MIB - budget / 100 * 85));
        assert!(before.1 >= 500 * MIB, "{before:?}");
        assert_eq!(crate::host::vram_keep(budget, let_go_use(used, free_blocks, 0)), (600_000, u64::MAX));
        // A first let-go: 4,700 MB in use, just past 88 % with
        // the room kept counted, under 66 % without.
        assert_eq!(crate::host::vram_keep(budget, let_go_use(4700 * MIB, 300 * MIB, 1950 * MIB)).0, 5_000);
        assert_eq!(crate::host::vram_keep(budget, let_go_use(4700 * MIB, 300 * MIB, 0)).0, 600_000);
        // The card itself past 88 %: copies go, down to 85 %.
        let full = crate::host::vram_keep(budget, let_go_use(6600 * MIB, 100 * MIB, 0));
        assert_eq!(full.0, 5_000);
        assert_eq!(full.1, 6500 * MIB - budget / 100 * 85);
        assert_eq!(let_go_use(100, 200, 7), 7);
    }

    /// The room kept for render targets is all kept until 30 s after their
    /// burst, then goes over 30 s; without a burst it stays as before.
    #[test]
    fn the_room_kept_for_targets_goes_after_their_burst() {
        let most = 3072 * MIB;
        // A low-resolution title: a few hundred MB at the fewest, about 1 GB
        // after a scene's first frames.
        assert!(!targets_burst(219 * MIB, 383 * MIB));
        assert!(targets_burst(219 * MIB, 1009 * MIB));
        assert!(targets_burst(0, 512 * MIB) && !targets_burst(0, 512 * MIB - 1));
        let at = 100_000;
        // (targets, now, kept)
        let table = [
            (266 * MIB, 5_000, 2806 * MIB),
            (1140 * MIB, at, 1932 * MIB),
            (1140 * MIB, at + 30_000, 1932 * MIB),
            (1200 * MIB, at + 45_000, 936 * MIB),
            (1256 * MIB, at + 60_000, 0),
            (1256 * MIB, at + 600_000, 0),
            // Targets past what is kept for them: none, at any time.
            (3500 * MIB, at + 1_000, 0),
        ];
        for (i, &(targets, now, kept)) in table.iter().enumerate() {
            let burst_at = if now >= at { at } else { 0 };
            assert_eq!(targets_kept(most, targets, burst_at, now), kept, "row {i}");
        }
        // No burst yet: all that is left, however long.
        assert_eq!(targets_kept(most, 1256 * MIB, 0, 10_000_000), 1816 * MIB);
    }

    /// A host copy of only a texture's smallest levels begins at its first
    /// level of at most 64 texels a side, at the console's mip tail at the
    /// latest, or where its views begin if that is later; and only where a
    /// copy of the levels views show can begin at all.
    #[cfg(windows)]
    #[test]
    fn a_copy_of_the_smallest_levels_begins_at_64_texels_a_side() {
        assert_eq!(tail_level(4096, 4096, 13), 6);
        assert_eq!(tail_level(2048, 1024, 12), 5);
        assert_eq!(tail_level(100, 50, 7), 1);
        assert_eq!(tail_level(64, 64, 7), 0);
        assert_eq!(tail_level(8192, 16, 14), 7);
        // Fewer levels than reach 64 texels: the last.
        assert_eq!(tail_level(4096, 4096, 3), 2);
        assert_eq!(tail_level(4096, 4096, 1), 0);
        let desc = |width: u32, mips: u32, format: u32, layout: u32| {
            let words = [3u32, 0, 0, 0, width, 0, width, mips << 16 | 1, format, 1, 0, layout, 0, 0];
            // SAFETY: 56 bytes in the description's layout.
            unsafe { words.as_ptr().cast::<ResourceDesc>().read_unaligned() }
        };
        // BC1 textures of 512x512 placed in 4 KiB blocks: from level 3,
        // 64x64.
        let bc1 = desc(512, 8, 71, 0x105);
        assert_eq!(tail_base(&bc1, 0), 3);
        // BC7 of 4096x4096: from level 6; from a view's first level past it,
        // that one.
        let bc7 = desc(4096, 13, 98, 0x105);
        assert_eq!(tail_base(&bc7, 0), 6);
        assert_eq!(tail_base(&bc7, 7), 7);
        // In 64 KiB blocks the console's mip tail begins at larger levels: a
        // copy begins there at the latest (BC1 of 2048x2048: 256x256).
        assert_eq!(tail_base(&desc(2048, 12, 71, 0x109), 0), 3);
        // None of one level, of a layout a copy cannot be cut from, of an
        // array.
        assert_eq!(tail_base(&desc(512, 1, 71, 0x105), 0), 0);
        assert_eq!(tail_base(&desc(2048, 12, 71, 0x10d), 0), 0);
        let mut array = bc1;
        array.depth_or_array_size = 6;
        assert_eq!(tail_base(&array, 0), 0);
    }

    /// A placed texture's host copy made while the card has no room holds
    /// its smallest levels only, stays so while the pass that made it finds
    /// none, and gives way to one with every level the view shows when a pass
    /// with room asks for them (the slots that show it are made again from
    /// that one once its upload has run: `grown`).
    #[cfg(windows)]
    #[test]
    fn a_copy_of_the_smallest_levels_gives_way_to_a_whole_one_when_there_is_room() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
        if crate::host::host().is_none() || !held_only() {
            return;
        }
        // 256x256 BC7, nine levels, 4 KiB blocks.
        let desc = [3u32, 0, 0, 0, 256, 0, 256, 0x9_0001, 98, 1, 0, 0x105, 0, 0];
        let whole = placed_bytes(&desc).unwrap();
        // SAFETY: memory of our own that stays.
        let at = unsafe { VirtualAlloc(std::ptr::null(), whole as usize, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
        let texture = texture_at(at as u64, &desc).unwrap();
        let no_room = TailOnly::new(true);
        want_level(texture, 0);
        let small = host_copy(texture).unwrap();
        // SAFETY: a live texture.
        let d = unsafe { small.texture.GetDesc() };
        assert_eq!((small.tail, small.base, d.Width, d.MipLevels), (true, 2, 64, 7));
        assert!(tail_copy(texture));
        // Asked for all its levels while the card still has no room: kept.
        want_level(texture, 0);
        assert_eq!(host_copy(texture).unwrap().serial, small.serial);
        drop(no_room);
        want_level(texture, 0);
        let more = host_copy(texture).unwrap();
        // SAFETY: as above.
        let d = unsafe { more.texture.GetDesc() };
        assert_eq!((more.tail, more.base, d.Width, d.MipLevels), (false, 0, 256, 9));
        assert!(more.serial != small.serial && more.grown.load(std::sync::atomic::Ordering::Relaxed));
        assert!(!tail_copy(texture));
    }

    /// The switches of the policy for a full card: on by default on a card
    /// of 8 GiB or less only, and `0` is the old behaviour everywhere.
    #[test]
    fn the_full_card_policy_is_on_by_default_on_small_cards_only() {
        const GIB: u64 = 1 << 30;
        let small = Some(7 * GIB);
        let large = Some(16 * GIB);
        // The card's band alone by default; the PC's commit too only as asked.
        assert_eq!(detail_for(None, small), Detail::Card);
        assert_eq!(detail_for(None, Some(8 * GIB)), Detail::Card);
        assert_eq!(detail_for(None, large), Detail::Off);
        assert_eq!(detail_for(None, None), Detail::Off);
        assert_eq!(detail_for(Some("0"), small), Detail::Off);
        assert_eq!(detail_for(Some("card"), large), Detail::Card);
        assert_eq!(detail_for(Some("1"), large), Detail::Card);
        assert_eq!(detail_for(Some("pc"), small), Detail::CardAndPc);
        assert_eq!(detail_for(Some(" pc "), large), Detail::CardAndPc);
        // The card's shortage alone decides on small cards only by default.
        assert!(card_short_for(None, small));
        assert!(card_short_for(None, Some(8 * GIB)));
        assert!(!card_short_for(None, large));
        assert!(!card_short_for(None, None));
        assert!(!card_short_for(Some("0"), small));
        assert!(card_short_for(Some("1"), large));
        assert_eq!(card_headroom_for(None, small), 4);
        assert_eq!(card_headroom_for(None, large), 0);
        assert_eq!(card_headroom_for(Some("0"), small), 0);
        assert_eq!(card_headroom_for(Some("50"), large), 20);
        assert_eq!(small_when_full_for(None, small), 64 << 10);
        assert_eq!(small_when_full_for(None, large), 0);
        assert_eq!(small_when_full_for(Some("0"), small), 0);
        assert_eq!(small_when_full_for(Some("128"), large), 128 << 10);
    }

    /// The queue's room: its own measure (our blocks' free bytes count as
    /// free) under its line as before, and with a headroom the system's own
    /// count under the budget less it too.
    #[test]
    fn the_queue_stops_short_of_the_budget_by_either_count() {
        const MIB: u64 = 1 << 20;
        let fill = |used: u64, free_blocks: u64, kept: u64| CardFill { budget: 7000 * MIB, used, free_blocks, kept };
        // Under 90 % by the queue's measure: room without a headroom...
        let blocky = fill(6900 * MIB, 700 * MIB, 0);
        assert!(blocky.room(90, 0));
        // ...but by the system's own count 98.6 %: none with 4 % kept.
        assert!(!blocky.room(90, 4));
        assert!(fill(6000 * MIB, 700 * MIB, 0).room(90, 4));
        // What is kept for later counts by both.
        assert!(!fill(6000 * MIB, 0, 400 * MIB).room(90, 4));
        assert!(!fill(6400 * MIB, 0, 0).room(90, 0));
    }

    /// New copies lose a level a step across the band below the line, and
    /// none under it; the PC's shortage bounds them too.
    #[test]
    fn new_copies_have_fewer_levels_the_closer_the_card_is_to_its_line() {
        const MIB: u64 = 1 << 20;
        let budget = 10_000 * MIB;
        let at = |used: u64| Some(CardFill { budget, used: used * MIB, free_blocks: 0, kept: 0 });
        let side = |used: u64, pc: bool| detail_side_in(Detail::CardAndPc, at(used), 90, 0, pc);
        // The line is 9,000 MB, the band 1,000 MB below it.
        assert_eq!(side(7999, false), u64::MAX);
        assert_eq!(side(8000, false), u64::MAX);
        assert_eq!(side(8001, false), 2048);
        assert_eq!(side(8250, false), 1024);
        assert_eq!(side(8500, false), 512);
        assert_eq!(side(8700, false), 256);
        assert_eq!(side(8850, false), 128);
        assert_eq!(side(9500, false), 128);
        assert_eq!(side(1000, true), PC_SHORT_SIDE);
        assert_eq!(side(8700, true), 256);
        assert_eq!(detail_side_in(Detail::Card, at(1000), 90, 0, true), u64::MAX);
        assert_eq!(detail_side_in(Detail::Off, at(8700), 90, 0, true), u64::MAX);
        // With a headroom, the system's count of 9,300 MB (the queue's 8,000)
        // is near its line of 9,600.
        let blocks = Some(CardFill { budget, used: 9300 * MIB, free_blocks: 1300 * MIB, kept: 0 });
        assert_eq!(detail_side_in(Detail::Card, blocks, 90, 0, false), u64::MAX);
        assert_eq!(detail_side_in(Detail::Card, blocks, 90, 4, false), 256);
        // A side's first level: of a 4096 texture, at 512, level 3.
        assert_eq!(side_level(4096, 4096, 13, 512), 3);
        assert_eq!(side_level(4096, 1024, 13, 2048), 1);
        assert_eq!(side_level(256, 256, 9, 2048), 0);
        assert_eq!(side_level(4096, 4096, 13, TAIL_SIDE), tail_level(4096, 4096, 13));
    }

    /// The detail cut and the "no room" verdict as the card stands, its
    /// blocks' free room and the PC's memory (`VICTORY_CARD_SHORT`): with the
    /// rule, only the card being short cuts or refuses, never its use well
    /// under the budget whatever is kept for later, never the PC's commit
    /// unless `VICTORY_TEXTURE_DETAIL=pc`, and a texture that fits the free
    /// room of the blocks is let through; without it, as before.
    #[test]
    fn only_the_card_being_short_cuts_detail_or_finds_no_room() {
        const MIB: u64 = 1 << 20;
        let budget = 7000 * MIB;
        let fill = |used: u64, free_blocks: u64, kept: u64| CardFill {
            budget,
            used: used * MIB,
            free_blocks: free_blocks * MIB,
            kept: kept * MIB,
        };
        let decide = |f: CardFill, detail: Detail, pc_short: bool, rule: bool| {
            let room = f.room_as(90, 4, rule);
            let side =
                if room.is_ok() { detail_side_as(detail, Some(f), 90, 4, pc_short, rule) } else { (u64::MAX, Why::None) };
            (room, side)
        };
        // Use at 6,000 of 7,000 MB with 1,000 MB of the blocks free and 700
        // kept for targets not made yet: by the system's count with what is
        // kept, 6,700 against the headroom's line of 6,720.
        let filling = fill(6000, 1000, 700);
        assert_eq!(decide(filling, Detail::Card, false, false), (Ok(()), (128, Why::Whole)));
        // The card's own count (6,000) is under the band below its line
        // (86 %, 6,020): no cut, room.
        assert!(!filling.short(90, 4));
        assert_eq!(decide(filling, Detail::Card, false, true), (Ok(()), (u64::MAX, Why::None)));
        // With what is kept the queue's line is past: no room before, room
        // with the rule while the card's use is well under its budget.
        let kept_much = fill(5000, 0, 2000);
        assert_eq!(kept_much.room_as(90, 4, false), Err(Why::Queue));
        assert_eq!(kept_much.room_as(90, 4, true), Ok(()));
        // The PC's commit: no bound unless asked for.
        assert_eq!(decide(filling, Detail::Card, true, true), (Ok(()), (u64::MAX, Why::None)));
        assert_eq!(decide(filling, Detail::CardAndPc, true, true), (Ok(()), (PC_SHORT_SIDE, Why::Pc)));
        assert_eq!(decide(filling, Detail::CardAndPc, true, false), (Ok(()), (128, Why::Whole)));
        assert_eq!(decide(fill(1000, 0, 0), Detail::CardAndPc, true, false).1, (PC_SHORT_SIDE, Why::Pc));
        // The card short by its own count: cut and refused as before, and
        // said why.
        let short = fill(6350, 500, 0);
        assert!(short.short(90, 4));
        assert_eq!(decide(short, Detail::Card, false, true), (Ok(()), (512, Why::Whole)));
        assert_eq!(decide(short, Detail::Card, false, false), (Ok(()), (512, Why::Whole)));
        let full = fill(6900, 0, 0);
        assert_eq!(full.room_as(90, 4, true), Err(Why::Queue));
        assert_eq!(fill(6800, 600, 0).room_as(90, 4, true), Err(Why::Whole));
        // The queue's own band comes first where it is as deep.
        assert_eq!(detail_side_as(Detail::Card, Some(fill(5950, 0, 0)), 90, 0, false, true), (512, Why::Queue));
        // Without a headroom the band is below the queue's line (80 %).
        assert!(!fill(5500, 0, 0).short(90, 0) && fill(5600, 0, 0).short(90, 0));
        // A texture that fits the blocks' free room is let through: it
        // fits the largest free range and what is left of the free bytes.
        assert!(fits_pool(4 * MIB, 16 * MIB, 1000 * MIB));
        assert!(fits_pool(16 * MIB, 16 * MIB, 16 * MIB));
        assert!(!fits_pool(20 * MIB, 16 * MIB, 1000 * MIB));
        assert!(!fits_pool(4 * MIB, 16 * MIB, 2 * MIB));
        assert!(!fits_pool(0, 16 * MIB, 1000 * MIB));
        assert!(!fits_pool(4 * MIB, 0, 0));
    }

    /// What a copy holds decides whether a slot may be given a stand-in: only
    /// one cut from a block, filled from title memory, that no upload, copy
    /// on the card or host copy went into. A texture the card renders to or
    /// writes, one the card wrote (a view placed in a target's memory in
    /// another format), and one that holds an earlier upload are shown as
    /// they are, whatever else is true of them.
    #[test]
    fn only_a_copy_nothing_went_into_is_given_a_stand_in() {
        let none = u32::MAX;
        let host = |wrote: bool| move || wrote;
        // (pooled, filled, tiled, recorded, card wrote, host wrote)
        assert_eq!(holds_of(true, true, false, none, false, host(false)), Contents::Nothing);
        assert_eq!(holds_of(true, false, false, none, false, host(false)), Contents::CardTexture);
        assert_eq!(holds_of(false, false, false, none, false, host(false)), Contents::CardTexture);
        assert_eq!(holds_of(true, true, false, none, true, host(false)), Contents::CardWrote);
        assert_eq!(holds_of(true, true, false, none, false, host(true)), Contents::CardWrote);
        assert_eq!(holds_of(true, true, false, 0, false, host(false)), Contents::Uploaded);
        assert_eq!(holds_of(true, true, false, 3, false, host(false)), Contents::Uploaded);
        // Written by the card and uploaded too: the card's.
        assert_eq!(holds_of(true, true, false, 0, true, host(false)), Contents::CardWrote);
        // Memory of its own, or levels given level by level: zeros at worst.
        assert_eq!(holds_of(false, true, false, none, false, host(false)), Contents::OwnMemory);
        assert_eq!(holds_of(true, true, true, none, false, host(false)), Contents::OwnMemory);
        // The host's copies are asked about last, and only when it matters.
        let asked = std::cell::Cell::new(false);
        let _ = holds_of(true, true, false, 0, false, || {
            asked.set(true);
            true
        });
        assert!(!asked.get());
    }

    /// A host copy made with fewer levels for the band is not replaced by an
    /// empty one with every level where no upload follows (a slot flipped
    /// after its upload ran: `NoUploadFollows`), and the slot shows the levels
    /// it has; the texture pass, where its upload follows, replaces it as
    /// before. With `VICTORY_SHOW_WRITTEN=0` the guard is never set.
    #[cfg(windows)]
    #[test]
    fn a_copy_of_fewer_levels_is_not_replaced_where_no_upload_follows() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
        if crate::host::host().is_none() || !held_only() || !show_written() {
            return;
        }
        let _views = crate::test_views_lock();
        // 1024x1024 BC7, eleven levels, 4 KiB blocks.
        let desc = [3u32, 0, 0, 0, 1024, 0, 1024, 0xb_0001, 98, 1, 0, 0x105, 0, 0];
        let whole = placed_bytes(&desc).unwrap();
        // SAFETY: memory of our own that stays.
        let at = unsafe { VirtualAlloc(std::ptr::null(), whole as usize, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
        let texture = texture_at(at as u64, &desc).unwrap();
        let fewer = {
            let _band = DetailCap::new(256);
            want_level(texture, 0);
            host_copy(texture).unwrap()
        };
        assert_eq!((fewer.tail, fewer.base), (true, 2));
        let kept = UNWRITTEN[2].load(std::sync::atomic::Ordering::Relaxed);
        {
            let _alone = NoUploadFollows::new();
            assert!(no_upload_follows());
            want_level(texture, 0);
            assert_eq!(host_copy(texture).unwrap().serial, fewer.serial, "kept where no upload follows");
            assert!(UNWRITTEN[2].load(std::sync::atomic::Ordering::Relaxed) > kept);
        }
        assert!(!no_upload_follows());
        // No upload went into it: what a slot made of it would show is not
        // its memory.
        assert_eq!(fewer.recorded.load(std::sync::atomic::Ordering::Relaxed), u32::MAX);
        assert_eq!(unwritten(texture, &fewer), fewer.pooled && fewer.filled);
        note_recorded(&fewer, 2);
        assert!(!unwritten(texture, &fewer));
        // Where its upload follows: replaced by one of every level.
        want_level(texture, 0);
        let more = host_copy(texture).unwrap();
        assert!(more.serial != fewer.serial && more.base == 0);
    }

    /// More textures than fit a made-up budget, as the pass over textures
    /// picked by index makes them: every one gets a host copy, of fewer
    /// levels the fuller the card, of its smallest levels once the queue
    /// has no room; each copy holds the levels it says from its first on;
    /// the card's use stays under the budget; and with room again each is
    /// replaced by one of every level, and the slot shows those.
    #[cfg(windows)]
    #[test]
    fn more_textures_than_fit_all_get_copies_and_more_levels_later() {
        use std::sync::atomic::Ordering::Relaxed;
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
        if crate::host::host().is_none() || !held_only() {
            return;
        }
        const MIB: u64 = 1 << 20;
        const TEXTURES: usize = 48;
        // 1024x1024 BC7, eleven levels, 4 KiB blocks: 1.4 MB each whole.
        let desc = [3u32, 0, 0, 0, 1024, 0, 1024, 0xb_0001, 98, 1, 0, 0x105, 0, 0];
        let whole = placed_bytes(&desc).unwrap();
        // The made-up card: whole copies of all would be three times its
        // budget, of which the rest of the run holds a tenth.
        let budget = whole * TEXTURES as u64 / 3;
        let others = budget / 10;
        // SAFETY: memory of our own that stays.
        let at = unsafe { VirtualAlloc(std::ptr::null(), whole as usize * TEXTURES, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
        assert!(!at.is_null());
        let mut copies = Vec::new();
        let mut used = others;
        for i in 0..TEXTURES {
            let texture = texture_at(at as u64 + i as u64 * whole, &desc).unwrap();
            let fill = CardFill { budget, used, free_blocks: 0, kept: 0 };
            let room = fill.room(90, 4);
            let side = if room { detail_side_in(Detail::Card, Some(fill), 90, 4, false) } else { u64::MAX };
            let _tail = TailOnly::new(!room);
            let _fewer = DetailCap::new(side);
            want_level(texture, 0);
            let copy = host_copy(texture).expect("every texture gets a host copy");
            // SAFETY: a live texture.
            let d = unsafe { copy.texture.GetDesc() };
            let expected = match (room, side) {
                (false, _) => tail_level(1024, 1024, 11),
                (true, u64::MAX) => 0,
                (true, side) => side_level(1024, 1024, 11, side),
            };
            assert_eq!(copy.base, expected, "texture {i}: room {room}, side {side}");
            assert_eq!((d.Width, u32::from(d.MipLevels)), (1024 >> expected, 11 - expected), "texture {i}");
            assert_eq!(copy.tail, expected > 0);
            used += copy.bytes;
            copies.push((texture, copy));
        }
        let bases: Vec<u32> = copies.iter().map(|(_, c)| c.base).collect();
        assert!(bases.windows(2).all(|b| b[0] <= b[1]), "fewer levels as the card fills: {bases:?}");
        assert!(bases[0] == 0 && *bases.last().unwrap() == tail_level(1024, 1024, 11), "{bases:?}");
        assert!(bases.iter().any(|&b| b > 0 && b < tail_level(1024, 1024, 11)), "levels between: {bases:?}");
        assert!(used < budget, "{} MB of a budget of {} MB", used / MIB, budget / MIB);
        // Room again: each reduced one gives way to one of every level.
        let grown_before = HELD[2].load(Relaxed);
        for (texture, old) in &copies {
            if old.base == 0 {
                continue;
            }
            let _tail = TailOnly::new(false);
            let _fewer = DetailCap::new(u64::MAX);
            want_level(*texture, 0);
            let more = host_copy(*texture).unwrap();
            // SAFETY: a live texture.
            let d = unsafe { more.texture.GetDesc() };
            assert_eq!((more.base, more.tail, d.Width, d.MipLevels), (0, false, 1024, 11));
            assert!(more.serial != old.serial && more.grown.load(Relaxed));
        }
        assert!(HELD[2].load(Relaxed) > grown_before);
        // A copy of 512 texels a side asked for more while the band allows
        // only that much: kept; once it allows 1024, replaced by one of those.
        let (texture, _) = copies[0];
        let cut = side_level(1024, 1024, 11, 512);
        // As the sweep cuts it (`cut_idle`), without a slot here.
        resource(texture).host.cut.store(cut, Relaxed);
        {
            let made = resource(texture).host.made.lock().unwrap().take();
            REPLACED.lock().unwrap().push((crate::host::moment(), made.unwrap()));
        }
        let small = host_copy(texture).unwrap();
        assert_eq!((small.base, small.tail), (cut, true));
        {
            let _fewer = DetailCap::new(512);
            want_level(texture, 0);
            assert_eq!(host_copy(texture).unwrap().serial, small.serial);
        }
        {
            let _fewer = DetailCap::new(1024);
            want_level(texture, 0);
            let more = host_copy(texture).unwrap();
            assert_eq!((more.base, more.tail), (0, false), "1024 a side is all of it");
        }
        // SAFETY: a plain query.
        assert!(unsafe { crate::host::host().unwrap().device.GetDeviceRemovedReason() }.is_ok());
    }

    /// The sweep cuts a whole copy a slot holds down to the levels the card
    /// affords once nothing used it for a while: the slot keeps the old one
    /// until it is made again, and the next copy holds those levels only. A
    /// copy no slot holds, or one used lately, it leaves alone.
    #[cfg(windows)]
    #[test]
    fn an_idle_copy_a_slot_holds_is_cut_down() {
        use std::sync::atomic::Ordering::Relaxed;
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
        if crate::host::host().is_none() || !held_only() {
            return;
        }
        // 1024x1024 BC7, eleven levels, 4 KiB blocks.
        let desc = [3u32, 0, 0, 0, 1024, 0, 1024, 0xb_0001, 98, 1, 0, 0x105, 0, 0];
        let whole = placed_bytes(&desc).unwrap();
        // SAFETY: memory of our own that stays.
        let at = unsafe { VirtualAlloc(std::ptr::null(), whole as usize * 2, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
        let held = texture_at(at as u64, &desc).unwrap();
        let free = texture_at(at as u64 + whole, &desc).unwrap();
        let (held_copy, free_copy) = (host_copy(held).unwrap(), host_copy(free).unwrap());
        assert_eq!((held_copy.base, free_copy.base), (0, 0));
        // A slot shows the first: it holds its texture.
        let slot = held_copy.texture.clone();
        let serial = held_copy.serial;
        drop((held_copy, free_copy));
        let side = 256;
        // Whether it is idle is the caller's to say: the sweep's time and
        // frames. (Not by moving the process's frame count: every test's
        // idle rule reads it, and windows other tests left became idle and
        // were let go under them.)
        let (idle, used) = (|_: u64| true, |_: u64| false);
        // Used lately: left alone.
        assert!(cut_idle(resource(held), held as usize, side, &used).is_none());
        // No slot holds it: the sweep lets that one go whole, not here.
        assert!(cut_idle(resource(free), free as usize, side, &idle).is_none());
        let cuts = DETAILED[2].load(Relaxed);
        assert!(cut_idle(resource(held), held as usize, side, &idle).is_some_and(|had| had > 0));
        assert!(DETAILED[2].load(Relaxed) > cuts);
        assert!(made_copy(held).is_none(), "the old copy is out; the slot holds its texture");
        assert!(crate::host::references(&slot) >= 1);
        let level = side_level(1024, 1024, 11, side);
        let cut = host_copy(held).unwrap();
        // SAFETY: a live texture.
        let d = unsafe { cut.texture.GetDesc() };
        assert_eq!((cut.base, cut.tail, d.Width, u32::from(d.MipLevels)), (level, true, 1024 >> level, 11 - level));
        assert!(cut.serial != serial && cut.grown.load(Relaxed), "its slots are made from it after its upload");
        // Down already: not again.
        drop(cut);
        assert!(cut_idle(resource(held), held as usize, side, &idle).is_none());
        drop(slot);
    }

    /// `texture_at` from eight threads, four of them through what readers
    /// share and what a thread remembers and four under the table's lock as
    /// before, while a ninth creates and releases textures at the same
    /// addresses as the title does: every thread gets the same resource for
    /// the same ask every time, whichever way; asks that differ in the
    /// filler words only get one resource, another format at the same
    /// address another, a buffer's description none; and the table has
    /// exactly those.
    #[cfg(windows)]
    #[test]
    fn placed_textures_are_the_same_found_shared_or_under_the_lock() {
        use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
        const PLACES: usize = 24;
        const ROUNDS: usize = 150;
        // 256x256, nine levels, 4 KiB blocks: BC7 and BC1.
        let bc7 = [3u32, 0, 0, 0, 256, 0, 256, 0x9_0001, 98, 1, 0, 0x105, 0, 0];
        let whole = placed_bytes(&bc7).unwrap() as usize;
        // SAFETY: memory of our own that stays.
        let all = unsafe { VirtualAlloc(std::ptr::null(), whole * PLACES, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
        let at = all as usize;
        assert!(at != 0);
        // For each place: BC7, the same with filler words (two kinds), BC1,
        // and a buffer.
        let mut asks: Vec<(u64, [u32; 14])> = Vec::new();
        for place in 0..PLACES {
            let address = (at + place * whole) as u64;
            let with = |change: &dyn Fn(&mut [u32; 14])| {
                let mut desc = bc7;
                change(&mut desc);
                (address, desc)
            };
            asks.push(with(&|_| ()));
            asks.push(with(&|d| d[1] = 0xdead_0000 + place as u32));
            asks.push(with(&|d| (d[1], d[13]) = (7, 0x1234_5678)));
            asks.push(with(&|d| d[8] = 71));
            asks.push(with(&|d| d[0] = BUFFER));
        }
        let placed = || PLACED_TEXTURES.lock().unwrap_or_else(|e| e.into_inner()).len();
        let before = placed();
        let stop = AtomicBool::new(false);
        let (asks, stop) = (&asks, &stop);
        let answers: Vec<Vec<usize>> = std::thread::scope(|s| {
            let title = s.spawn(move || {
                let mut made = 0u32;
                while !stop.load(Relaxed) {
                    for place in 0..PLACES {
                        // SAFETY: 56 bytes in the description's layout.
                        let d = unsafe { bc7.as_ptr().cast::<ResourceDesc>().read_unaligned() };
                        let mut out = std::ptr::null();
                        let address = (at + place * whole) as u64;
                        let (none, iid) = (std::ptr::null(), &RESOURCE_IIDS[0]);
                        assert_eq!(create_at_address(none, address, &d, 0, std::ptr::null(), iid, &mut out), S_OK);
                        assert_eq!(resource_release(out), 0);
                        made += 1;
                    }
                }
                made
            });
            let threads: Vec<_> = (0..8usize)
                .map(|t| {
                    s.spawn(move || {
                        let shared = t % 2 == 0;
                        let mut first = vec![usize::MAX; asks.len()];
                        for round in 0..ROUNDS {
                            for n in 0..asks.len() {
                                // Each thread in an order of its own.
                                let i = (n * (2 * t + 1) + round * 7) % asks.len();
                                let found = texture_at_by(asks[i].0, &asks[i].1, shared).map_or(0, |r| r as usize);
                                if first[i] == usize::MAX {
                                    first[i] = found;
                                }
                                assert_eq!(found, first[i], "ask {i}, round {round}, shared {shared}");
                            }
                        }
                        first
                    })
                })
                .collect();
            let answers = threads.into_iter().map(|thread| thread.join()).collect::<Vec<_>>();
            stop.store(true, Relaxed);
            assert!(title.join().expect("the title's thread") > 0);
            answers.into_iter().map(|answer| answer.expect("an asking thread")).collect()
        });
        // The same on every thread, shared or locked.
        assert!(answers.iter().all(|answer| *answer == answers[0]));
        let found = &answers[0];
        let mut resources = std::collections::BTreeSet::new();
        for place in 0..PLACES {
            let [plain, filler, other_filler, bc1, buffer] = [0, 1, 2, 3, 4].map(|k| found[place * 5 + k]);
            assert!(plain != 0 && bc1 != 0 && bc1 != plain && buffer == 0);
            // (With `VICTORY_PLACED_FILLER=1` the filler is part of the ask.)
            let twins = placed_known(asks[place * 5 + 1].0, &asks[place * 5 + 1].1) == Some(plain as This);
            assert!(!twins || (filler == plain && other_filler == plain));
            assert_eq!(placed_known(asks[place * 5].0, &asks[place * 5].1), Some(plain as This));
            resources.extend([plain, filler, other_filler, bc1]);
            // What a thread asks afterwards is the same again, both ways.
            for shared in [true, false, true] {
                assert_eq!(texture_at_by(asks[place * 5].0, &asks[place * 5].1, shared), Some(plain as This));
                assert_eq!(texture_at_by(asks[place * 5 + 4].0, &asks[place * 5 + 4].1, shared), None);
            }
        }
        // (Other tests place textures too: at least these.)
        assert!(resources.len() >= 2 * PLACES && placed() - before >= resources.len());
        assert!(resources.iter().all(|&r| resource(r as This).host.placed.load(Relaxed)));
    }

    /// However a round of the sweep is cut into looks, every texture is
    /// looked at once in it, and the round ends with the last.
    #[test]
    fn every_entry_is_visited_once_a_round() {
        use std::collections::{BTreeMap, BTreeSet};
        let all: BTreeSet<usize> = (1..=1000).map(|i| i * 16).collect();
        for per in [1, 7, 64, 999, 1000, 5000] {
            let mut seen: BTreeMap<usize, u32> = BTreeMap::new();
            let (mut from, mut looks) = (0, 0);
            loop {
                from = walk_from(&all, from, |looked| looked < per, |at| *seen.entry(at).or_default() += 1);
                looks += 1;
                if from == 0 {
                    break;
                }
            }
            assert_eq!(seen.len(), all.len(), "{per}");
            assert!(seen.values().all(|&n| n == 1), "{per}");
            assert_eq!(looks, all.len().div_ceil(per), "{per}");
        }
        // A look goes on however little time it has: one at least.
        assert_eq!(walk_from(&all, 0, |_| false, |_| {}), 32);
        assert_eq!(walk_from(&BTreeSet::new(), 0, |_| true, |_| {}), 0);
    }

    /// Textures that come and go between the looks: those there all round
    /// long are looked at once; one made behind the round waits for the
    /// next, one made ahead of it is looked at in this one.
    #[test]
    fn entries_made_and_gone_between_looks_keep_the_round_whole() {
        use std::collections::{BTreeMap, BTreeSet};
        let stay: BTreeSet<usize> = (0..500).map(|i| 1000 + i * 8).collect();
        let mut all = stay.clone();
        let mut seen: BTreeMap<usize, u32> = BTreeMap::new();
        let (mut from, mut look) = (0, 0);
        loop {
            from = walk_from(&all, from, |looked| looked < 37, |at| *seen.entry(at).or_default() += 1);
            if from == 0 {
                break;
            }
            // Made behind the walk and ahead of it; and one that stays goes.
            all.insert(1 + look);
            all.insert(100_000 + look);
            if look == 3 {
                all.remove(&(1000 + 499 * 8));
            }
            look += 1;
        }
        for at in &stay {
            let want = u32::from(*at != 1000 + 499 * 8);
            assert_eq!(seen.get(at).copied().unwrap_or(0), want, "{at}");
        }
        assert!(seen.keys().all(|&at| at >= 1000), "none made behind the walk");
        for l in 0..look {
            assert_eq!(seen.get(&(100_000 + l)), Some(&1), "{l}");
        }
    }

    #[test]
    fn a8_unorm_is_not_in_the_r8_typeless_family() {
        // DXGI_FORMAT values: 60 R8_TYPELESS, 61 R8_UNORM, 62 R8_UINT,
        // 63 R8_SNORM, 64 R8_SINT, 65 A8_UNORM. The R8 typeless family is
        // 60..=64; A8_UNORM has no typeless family and cannot be cast to R8,
        // so it must stand alone rather than fold into R8_TYPELESS.
        assert_eq!(family(60), 60);
        for f in 61..=64 {
            assert_eq!(family(f), 60);
        }
        assert_eq!(family(65), 65);
        // Boundaries: 59 belongs to the preceding family, 66 is unmapped.
        assert_eq!(family(59), 53);
        assert_eq!(family(66), 66);
    }

    type Release = extern "system" fn(This) -> u32;
    type Map = extern "system" fn(This, u32, *const c_void, *mut *mut u8) -> HRESULT;
    type Address = extern "system" fn(This) -> u64;

    const PROPERTIES: HeapProperties = HeapProperties {
        kind: 1,
        cpu_pages: 0,
        pool: 0,
        creation_nodes: 0,
        visible_nodes: 0,
    };

    fn buffer(width: u64) -> ResourceDesc {
        ResourceDesc {
            dimension: BUFFER,
            alignment: 0,
            width,
            height: 1,
            depth_or_array_size: 1,
            mip_levels: 1,
            format: 0,
            sample_count: 1,
            sample_quality: 0,
            layout: 1,
            flags: 0,
        }
    }

    fn texture(width: u64, height: u32, format: u32, mips: u16) -> ResourceDesc {
        ResourceDesc {
            dimension: 3,
            width,
            height,
            format,
            mip_levels: mips,
            layout: 0,
            ..buffer(0)
        }
    }

    fn heap(size: u64) -> This {
        let desc = HeapDesc {
            size,
            properties: PROPERTIES,
            alignment: 0,
            flags: 0,
        };
        let mut heap = std::ptr::null();
        assert_eq!(
            create_heap(std::ptr::null(), &desc, &HEAP_IIDS[0], &mut heap),
            S_OK
        );
        heap
    }

    #[test]
    fn heaps_are_zeroed_aligned_memory() {
        let heap = heap(1 << 20);
        // SAFETY: a heap we just made.
        let memory = unsafe { &(*heap.cast::<Heap>()).memory };
        assert_eq!(memory.ptr.as_ptr() as u64 % KIB_64, 0);
        let mut back = std::mem::MaybeUninit::<HeapDesc>::uninit();
        slot::<extern "system" fn(This, *mut HeapDesc)>(heap, 0x40)(heap, back.as_mut_ptr());
        // SAFETY: written above.
        assert_eq!(unsafe { back.assume_init() }.size, 1 << 20);
        assert_eq!(slot::<Release>(heap, 0x10)(heap), 0);

        let bad = HeapDesc {
            size: 1 << 20,
            properties: PROPERTIES,
            alignment: 3,
            flags: 0,
        };
        let mut out = std::ptr::null();
        assert_eq!(
            create_heap(std::ptr::null(), &bad, &HEAP_IIDS[0], &mut out),
            E_INVALIDARG
        );
    }

    #[test]
    fn device_setup_heap_properties_and_resources_at_addresses() {
        let device = crate::device::device();
        // GetCustomHeapProperties (0xd0): the struct comes back through the
        // hidden first argument, which is also returned.
        type Custom =
            extern "system" fn(*mut HeapProperties, This, u32, u32) -> *mut HeapProperties;
        let mut p = PROPERTIES;
        let back = slot::<Custom>(device, 0xd0)(&mut p, device, 1, 2);
        assert_eq!(back, &raw mut p);
        assert_eq!(
            (
                p.kind,
                p.cpu_pages,
                p.pool,
                p.creation_nodes,
                p.visible_nodes
            ),
            (4, 3, 1, 1, 1),
            "upload: custom, write-back, L0"
        );
        slot::<Custom>(device, 0xd0)(&mut p, device, 1, 1);
        assert_eq!(p.cpu_pages, 1, "default: not CPU-visible");

        // 0x428: a buffer over memory the title owns, as the title asks.
        type At = extern "system" fn(
            This,
            u64,
            *const ResourceDesc,
            u32,
            *const c_void,
            *const Iid,
            *mut This,
        ) -> HRESULT;
        let memory = vec![0u8; 0x1_0000];
        let address = memory.as_ptr() as u64;
        let mut r = std::ptr::null();
        let create = slot::<At>(device, 0x428);
        let desc = buffer(0x1_0000);
        let null = std::ptr::null();
        assert_eq!(
            create(
                device,
                address,
                &desc,
                u32::MAX,
                null,
                &RESOURCE_IIDS[0],
                &mut r
            ),
            0
        );
        assert_eq!(slot::<Address>(r, 0x58)(r), address);
        assert_eq!(slot::<Release>(r, 0x10)(r), 0);
        // The title's memory is still there after the release.
        assert_eq!(memory[0xffff], 0);
        assert_ne!(
            create(device, 0, &desc, u32::MAX, null, &RESOURCE_IIDS[0], &mut r),
            0
        );
    }

    #[test]
    fn sizes_cover_every_mip() {
        let size = |d: &ResourceDesc| footprint(d).map(|i| i.size);
        assert_eq!(size(&buffer(256)), Some(KIB_64));
        // Textures take their console layout and its alignment (4 KiB
        // standard swizzle when the description leaves it to the library):
        // 512x512 BC7 with 10 mips has one size, 4 KiB aligned, as on
        // the console.
        let layout = |d: &ResourceDesc| texture_layout(d).unwrap().size;
        let rgba = texture(256, 256, 28, 0);
        assert_eq!(size(&rgba), Some(layout(&rgba).next_multiple_of(4096)));
        let bc7 = footprint(&texture(512, 512, 98, 10));
        let console = AllocationInfo {
            size: 0x56000,
            alignment: 0x1000,
        };
        assert_eq!(bc7, Some(console));
        assert!(layout(&rgba) >= 4 * (256 * 256 * 4) / 3, "every mip");
        // BC1 is 8 bytes per 4x4 block: 1024x1024, one mip = 512 KiB.
        assert_eq!(size(&texture(1024, 1024, 71, 1)), Some(512 << 10));
        assert_eq!(size(&texture(16, 16, 66, 1)), None, "1-bit format");
        // Linear layout (1) and an explicit mode (64 KiB display, 0x10a).
        let linear = ResourceDesc {
            layout: 1,
            ..texture(100, 60, 28, 1)
        };
        assert_eq!(
            texture_layout(&linear).unwrap().planes[0]
                .layout
                .desc
                .swizzle
                .0,
            0
        );
        let display = ResourceDesc {
            layout: 0x10a,
            ..texture(100, 60, 28, 1)
        };
        assert_eq!(
            texture_layout(&display).unwrap().planes[0]
                .layout
                .desc
                .swizzle
                .0,
            10
        );

        let mut info = AllocationInfo {
            size: 0,
            alignment: 0,
        };
        let descs = [buffer(100), texture(256, 256, 28, 0)];
        allocation_info(&mut info, std::ptr::null(), 0, 2, descs.as_ptr());
        assert_eq!(
            info,
            AllocationInfo {
                size: KIB_64 + size(&descs[1]).unwrap(),
                alignment: KIB_64
            }
        );
        allocation_info(&mut info, std::ptr::null(), 0, 1, &texture(1, 1, 999, 1));
        assert_eq!(info.size, u64::MAX);
    }

    #[test]
    fn upload_footprints_follow_the_pc_rules() {
        let null = std::ptr::null();
        let mut layouts = [NO_FOOTPRINT; 2];
        let (mut rows, mut sizes, mut total) = ([0; 2], [0; 2], 0);
        let mut footprints = |desc: &ResourceDesc, first, count| {
            let (l, r, s) = (layouts.as_mut_ptr(), rows.as_mut_ptr(), sizes.as_mut_ptr());
            copyable_footprints(null, desc, first, count, 0, l, r, s, &mut total);
            (layouts, rows, sizes, total)
        };
        // 100x60 RGBA8, 2 mips: 400-byte rows pitched to 512.
        let (l, r, s, t) = footprints(&texture(100, 60, 28, 2), 0, 2);
        assert_eq!((l[0].row_pitch, r[0], s[0]), (512, 60, 400));
        // Mip 1 (50x30, 200-byte rows) starts after 59 pitched rows and one
        // packed row, rounded up to 512.
        assert_eq!(l[1].offset, (512 * 59 + 400u64).next_multiple_of(512));
        assert_eq!((l[1].width, l[1].height, r[1]), (50, 30, 30));
        assert_eq!(t, l[1].offset + 256 * 29 + 200);
        // BC1: rows of 4x4 blocks, 8 bytes each.
        let (_, r, s, _) = footprints(&texture(64, 64, 71, 1), 0, 1);
        assert_eq!((r[0], s[0]), (16, 128));
        let (_, _, _, t) = footprints(&texture(64, 64, 71, 1), 1, 1);
        assert_eq!(t, u64::MAX, "no second subresource");
    }

    #[test]
    fn depth_stencil_footprints_follow_the_pc_plane_order() {
        // Native D3D12 reports the same layout for both depth/stencil families
        // and their view formats. Mips vary fastest, then array slices, then planes.
        let expected = [
            (512, 39, 16, 8),
            (4_608, 39, 8, 4),
            (6_656, 39, 16, 8),
            (10_752, 39, 8, 4),
            (12_800, 60, 16, 8),
            (16_896, 60, 8, 4),
            (18_944, 60, 16, 8),
            (23_040, 60, 8, 4),
        ];
        for format in (19..=22).chain(44..=47) {
            let desc = ResourceDesc {
                depth_or_array_size: 2,
                ..texture(16, 8, format, 2)
            };
            let mut layouts = [NO_FOOTPRINT; 8];
            let (mut rows, mut sizes, mut total) = ([0; 8], [0; 8], 0);
            copyable_footprints(
                std::ptr::null(),
                &desc,
                0,
                8,
                512,
                layouts.as_mut_ptr(),
                rows.as_mut_ptr(),
                sizes.as_mut_ptr(),
                &mut total,
            );

            assert_eq!(
                layouts.map(|l| (l.offset, l.format, l.width, l.height)),
                expected,
                "format {format}"
            );
            assert!(
                layouts.iter().all(|l| l.depth == 1 && l.row_pitch == 512),
                "format {format}"
            );
            assert_eq!(rows, [8, 4, 8, 4, 8, 4, 8, 4], "format {format}");
            assert_eq!(sizes, [64, 32, 64, 32, 16, 8, 16, 8], "format {format}");
            assert_eq!(total, 24_072, "format {format}");
            assert!(subresource(&desc, 8).is_none(), "format {format}");
        }
    }

    /// A texture the host had no memory for is made once its wait is over,
    /// which doubles with each such refusal in a row; one the host refused
    /// for another reason stays refused (upstream 4e95a78, upstream).
    /// The refusals are made up in the real making ([`FAIL_NEXT`]).
    #[cfg(windows)]
    #[test]
    fn a_texture_refused_for_want_of_memory_is_made_later() {
        use windows::Win32::Foundation::{E_INVALIDARG, E_OUTOFMEMORY};
        if crate::host::host().is_none() || !lazy() || !texture_retry() {
            return; // no host GPU here, host copies made at creation, or the switch off
        }
        // Past `ms`, and past what another test's refusal holds back.
        let wait = |ms: u64| {
            std::thread::sleep(std::time::Duration::from_millis(ms));
            let began = std::time::Instant::now();
            while crate::host::commit::upload_refused() && began.elapsed() < std::time::Duration::from_secs(10) {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        let null = std::ptr::null();
        for (error, again) in [(E_OUTOFMEMORY, true), (E_INVALIDARG, false)] {
            // A render target: made on its own, not cut from a block.
            let mut desc = texture(4, 4, 28, 1);
            desc.flags = 1;
            let mut image = null;
            assert_eq!(
                create_committed(null, &PROPERTIES, 0, &desc, 4, null.cast(), &RESOURCE_IIDS[0], &mut image),
                S_OK
            );
            FAIL_NEXT.set(Some(error));
            assert!(hosted(image, true).is_none(), "{error:?}");
            assert_eq!(FAIL_NEXT.get(), None, "the refusal was not made");
            assert_eq!(retrying(image), again, "{error:?}");
            // Slots written with a view of it wait for it if it may still be made.
            assert_eq!(host_waits(image), again, "{error:?}");
            // Not asked again before its time.
            assert!(hosted(image, true).is_none(), "{error:?}");
            wait(RETRY_MS + 100);
            if again {
                // Refused again when asked again: the next wait is twice as long.
                FAIL_NEXT.set(Some(error));
                assert!(hosted(image, true).is_none());
                assert_eq!(FAIL_NEXT.get(), None, "asked again after its wait");
                // (A plain sleep: held back by another test's refusal, it is
                // not asked either.)
                std::thread::sleep(std::time::Duration::from_millis(RETRY_MS + 100));
                assert!(hosted(image, true).is_none(), "asked again before twice the wait");
                wait(RETRY_MS);
            }
            assert_eq!(hosted(image, true).is_some(), again, "{error:?}");
            assert!(!retrying(image), "{error:?}");
            assert_eq!(slot::<Release>(image, 0x10)(image), 0);
        }
    }

    /// A 4x4 texture filled from a title buffer through our list and queue,
    /// then read back from the host GPU.
    #[cfg(windows)]
    #[test]
    fn buffer_data_reaches_the_host_texture() {
        use windows::Win32::Graphics::Direct3D12::*;
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let null = std::ptr::null();
        let iid = &RESOURCE_IIDS[0];
        const COPY_DEST: u32 = 0x400;
        let (mut image, mut source) = (null, null);
        let rgba = texture(4, 4, 28, 1);
        assert_eq!(
            create_committed(
                null,
                &PROPERTIES,
                0,
                &rgba,
                COPY_DEST,
                null.cast(),
                iid,
                &mut image
            ),
            S_OK
        );
        assert_eq!(
            create_committed(
                null,
                &PROPERTIES,
                0,
                &buffer(1024),
                0,
                null.cast(),
                iid,
                &mut source
            ),
            S_OK
        );
        let (data, _) = buffer_bytes(source).unwrap();
        // SAFETY: the buffer holds 1024 bytes.
        let bytes = unsafe { std::slice::from_raw_parts_mut(data, 1024) };
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }

        let queue_iid = guid(
            0x0ec870a6,
            0x5d7e,
            0x4c22,
            [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed],
        );
        let allocator_iid = guid(
            0x6102dee4,
            0xaf59,
            0x4b09,
            [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24],
        );
        let list_iid = guid(
            0x5b160d0f,
            0xac1b,
            0x4185,
            [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55],
        );
        let (mut queue, mut allocator, mut list) = (null, null, null);
        let queue_desc = [0u32; 4];
        assert_eq!(
            crate::queue::create(null, queue_desc.as_ptr().cast(), &queue_iid, &mut queue),
            S_OK
        );
        assert_eq!(
            crate::list::create_allocator(null, 0, &allocator_iid, &mut allocator),
            S_OK
        );
        assert_eq!(
            crate::list::create_list(null, 0, 0, allocator, null, &list_iid, &mut list),
            S_OK
        );

        #[repr(C)]
        struct Location {
            resource: This,
            kind: u32,
            footprint: PlacedFootprint,
        }
        let rows = PlacedFootprint {
            offset: 0,
            format: 28,
            width: 4,
            height: 4,
            depth: 1,
            row_pitch: 256,
        };
        let to = Location {
            resource: image,
            kind: 0,
            footprint: PlacedFootprint { offset: 0, ..rows },
        };
        let from = Location {
            resource: source,
            kind: 1,
            footprint: rows,
        };
        type Copy =
            extern "system" fn(This, *const Location, u32, u32, u32, *const Location, *const u8);
        slot::<Copy>(list, 0x80)(list, &to, 0, 0, 0, &from, null.cast());
        slot::<extern "system" fn(This) -> HRESULT>(list, 0x48)(list);
        slot::<extern "system" fn(This, u32, *const This)>(queue, 0x50)(queue, 1, &list);

        // Read the host texture back into a readback buffer.
        let readback_heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let readback_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: 1024,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut readback: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a readback buffer.
        unsafe {
            host.device
                .CreateCommittedResource(
                    &readback_heap,
                    D3D12_HEAP_FLAG_NONE,
                    &readback_desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut readback,
                )
                .unwrap();
        }
        let readback = readback.unwrap();
        let host_image = host_resource(image).unwrap();
        let back = host.new_list().unwrap();
        let (_, before) = host_transition(image, 0x800).unwrap();
        // The upload list holds the texture until it is reset.
        assert!(std::sync::Arc::strong_count(&host_copy(image).unwrap()) > 2);
        use windows::core::Interface;
        let out_to = Location {
            resource: readback.as_raw().cast_const().cast(),
            kind: 1,
            footprint: rows,
        };
        let out_from = Location {
            resource: host_image.as_raw().cast_const().cast(),
            kind: 0,
            footprint: to.footprint,
        };
        // SAFETY: an open list, live resources, locations in the host layout.
        unsafe {
            back.list.ResourceBarrier(&[crate::host::transition(
                host_image,
                D3D12_RESOURCE_STATES(before as i32),
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            back.list.CopyTextureRegion(
                (&raw const out_to).cast(),
                0,
                0,
                0,
                (&raw const out_from).cast(),
                None,
            );
        }
        back.close();
        host.execute(&[&back]);
        let mut mapped = std::ptr::null_mut();
        // SAFETY: the copy finished (execute waits); the buffer is 1024 bytes.
        let read = unsafe {
            readback
                .Map(
                    0,
                    Some(&D3D12_RANGE {
                        Begin: 0,
                        End: 1024,
                    }),
                    Some(&mut mapped),
                )
                .unwrap();
            std::slice::from_raw_parts(mapped.cast::<u8>(), 1024)
        };
        for row in 0..4 {
            let at = row * 256;
            assert_eq!(read[at..at + 16], bytes[at..at + 16], "row {row}");
        }
    }

    /// A view placed in the memory of a texture the host writes (an R32_UINT
    /// one its shaders wrote packed light into, read as R9G9B9E5), and that
    /// texture, made as the title makes them: what `alias_of` finds.
    #[cfg(windows)]
    fn alias_pair() -> (This, This) {
        use std::sync::atomic::Ordering::Relaxed;
        let null = std::ptr::null();
        let (mut source, mut view) = (null, null);
        let source_desc = ResourceDesc {
            flags: 4,
            ..texture(4, 4, 42, 1)
        };
        assert_eq!(
            create_committed(null, &PROPERTIES, 0, &source_desc, 0, null.cast(), &RESOURCE_IIDS[0], &mut source),
            S_OK
        );
        let at = resource(source).data() as u64;
        assert_eq!(
            create_at_address(null, at, &texture(4, 4, 67, 1), 0, null.cast(), &RESOURCE_IIDS[0], &mut view),
            S_OK
        );
        resource(view).host.placed.store(true, Relaxed);
        (source, view)
    }

    /// The texture a placed view aliases is held while the caller copies
    /// from it: the title may let go of its own reference between the lookup
    /// and the copy, and the copy still needs it (fxceless #1336, a463dcb,
    /// whose test this is).
    #[cfg(windows)]
    #[test]
    fn an_alias_source_is_held_after_the_title_releases_it() {
        if crate::host::host().is_none() {
            return;
        }
        let (source, view) = alias_pair();
        assert_eq!(alias_of(view), Some(source));
        let held = alias_held(view).expect("an alias source");
        assert_eq!(held.get(), source);
        // The title lets go: the pin, not the title's reference, keeps the
        // source (and the copy about to be made from it) alive.
        resource_release(source);
        assert!(host_resource(held.get()).is_some(), "a held source is still copyable");
        assert!(textures().contains(&(source as usize)), "the held source is not freed");
        drop(held);
        assert!(!textures().contains(&(source as usize)), "the last reference frees it");
        assert_eq!(alias_of(view), None, "a freed texture is no source");
        resource_release(view);
    }

    /// An upload of a placed view from title memory earlier in a list does
    /// not keep the copy from the texture the host wrote out of it: the view
    /// shows that texture's texels, not title memory's zeros (fxceless #1241,
    /// 19a45d1, whose test this is; its author read zeros before and the
    /// packed light after, on a 32-lane GPU).
    #[cfg(windows)]
    #[test]
    fn alias_after_title_upload_copies_the_gpu_atlas() {
        use windows::Win32::Graphics::Direct3D12::*;

        let Some(host) = crate::host::host() else {
            return;
        };
        let (source, view) = alias_pair();
        assert_eq!(alias_of(view), Some(source));
        let written = host_resource(source).unwrap();
        let sampled = host_resource(view).unwrap();
        let (upload, data) = host.upload_buffer(1024).unwrap();
        let seed = host.new_list().unwrap();
        let packed_light = 0x8c02_0100_u32;
        // SAFETY: four rows of four texels in the upload's 1024 mapped bytes;
        // an open list and live resources.
        unsafe {
            for row in 0..4 {
                for x in 0..4 {
                    data.add(row * 256 + x * 4).cast::<u32>().write_unaligned(packed_light);
                }
            }
            let dst = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(written),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            let src = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&upload),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: written.GetDesc().Format,
                            Width: 4,
                            Height: 4,
                            Depth: 1,
                            RowPitch: 256,
                        },
                    },
                },
            };
            crate::list::ensure_state(&seed, source, D3D12_RESOURCE_STATE_COPY_DEST.0 as u32);
            seed.list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
        }
        seed.close();
        host.execute(&[&seed]);

        let list = host.new_list().unwrap();
        crate::texture::upload(&list, view, sampled);
        assert!(crate::texture::alias(&list, view, source));
        crate::list::ensure_state(&list, view, D3D12_RESOURCE_STATE_COPY_SOURCE.0 as u32);
        // The sampled texture's first texel, read back (`crate::capture` is in
        // diagnostics builds only).
        let readback = {
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_READBACK,
                ..Default::default()
            };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: 1024,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            let mut readback: Option<ID3D12Resource> = None;
            // SAFETY: plain creation of a readback buffer; an open list and
            // live resources, the footprint inside the buffer.
            unsafe {
                let state = D3D12_RESOURCE_STATE_COPY_DEST;
                host.device
                    .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut readback)
                    .unwrap();
                let readback = readback.unwrap();
                let to = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&readback),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                            Offset: 0,
                            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                                Format: sampled.GetDesc().Format,
                                Width: 4,
                                Height: 4,
                                Depth: 1,
                                RowPitch: 256,
                            },
                        },
                    },
                };
                let from = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(sampled),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
                };
                list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
                readback
            }
        };
        list.close();
        host.execute(&[&list]);
        let mut mapped = std::ptr::null_mut();
        // SAFETY: the completed copy holds at least one 32-bit texel.
        let actual = unsafe {
            readback.Map(0, None, Some(&mut mapped)).unwrap();
            let texel = mapped.cast::<u32>().read_unaligned();
            readback.Unmap(0, None);
            texel
        };
        resource_release(view);
        resource_release(source);
        assert_eq!(actual, packed_light, "the sampled texture kept title memory's upload, not the card's texels");
    }

    /// A 16-bit depth texture the title also asks unordered access for gets
    /// a colour twin, and the host copies one into the other, both ways: a
    /// pattern put into the twin is copied to the depth texture, the twin
    /// is overwritten with zeros, and the pattern comes back from the depth
    /// texture. A copy the host does not allow ends the test (with the
    /// debug layer's message under `VICTORY_D3D12_DEBUG=1` in a diagnostics
    /// build).
    #[cfg(windows)]
    #[test]
    fn a_depth_texture_and_its_twin_copy_into_each_other() {
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let null = std::ptr::null();
        const DEPTH_WRITE: u32 = 0x10;
        let desc = ResourceDesc {
            flags: 6,
            ..texture(4, 4, 55, 1)
        };
        let mut depth = null;
        assert_eq!(
            create_committed(
                null,
                &PROPERTIES,
                0,
                &desc,
                DEPTH_WRITE,
                null.cast(),
                &RESOURCE_IIDS[0],
                &mut depth
            ),
            S_OK
        );
        let host_depth = host_resource(depth).unwrap();
        let Some(twin) = twin_resource(depth) else {
            return; // twins are turned off
        };
        // SAFETY: live resources.
        let (d, t) = unsafe { (host_depth.GetDesc(), twin.GetDesc()) };
        // Both R16 typeless: one a depth texture, one with unordered access.
        assert_eq!((d.Format.0, d.Flags.0), (53, 2));
        assert_eq!((t.Format.0, t.Flags.0), (53, 4));

        // Four rows of four 16-bit texels, 256 bytes apart as copies place
        // them: the pattern, then as many zeros.
        let mut pattern = vec![0u8; 1024];
        for row in 0..4 {
            for b in 0..8 {
                pattern[row * 256 + b] = (1 + row * 8 + b) as u8;
            }
        }
        let (upload, data) = host.upload_buffer(2048).unwrap();
        // SAFETY: the buffer's mapping, 2048 bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(pattern.as_ptr(), data, 1024);
            std::ptr::write_bytes(data.add(1024), 0, 1024);
        }
        let readback_heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let readback_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: 1024,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut readback: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a readback buffer.
        unsafe {
            host.device
                .CreateCommittedResource(
                    &readback_heap,
                    D3D12_HEAP_FLAG_NONE,
                    &readback_desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut readback,
                )
                .unwrap();
        }
        let readback = readback.unwrap();

        // A texture's one subresource; the four rows of a buffer at `offset`.
        let whole = |texture: &ID3D12Resource| D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        let rows = |buffer: &ID3D12Resource, offset: u64| D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: offset,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: DXGI_FORMAT(53),
                        Width: 4,
                        Height: 4,
                        Depth: 1,
                        RowPitch: 256,
                    },
                },
            },
        };
        let (written, tested, source, dest) = (
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_DEPTH_WRITE,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        let moved = crate::host::transition;
        let list = host.new_list().unwrap();
        // SAFETY: an open list and live resources; the two textures are one
        // size in one format family.
        unsafe {
            let l = &list.list;
            // The pattern into the twin, the twin into the depth texture.
            l.ResourceBarrier(&[moved(twin, written, dest), moved(host_depth, tested, dest)]);
            l.CopyTextureRegion(&whole(twin), 0, 0, 0, &rows(&upload, 0), None);
            l.ResourceBarrier(&[moved(twin, dest, source)]);
            l.CopyResource(host_depth, twin);
            // Zeros over the twin, then the depth texture back into it.
            l.ResourceBarrier(&[moved(twin, source, dest)]);
            l.CopyTextureRegion(&whole(twin), 0, 0, 0, &rows(&upload, 1024), None);
            l.ResourceBarrier(&[moved(host_depth, dest, source)]);
            l.CopyResource(twin, host_depth);
            // The twin read back; both textures to the states they had.
            l.ResourceBarrier(&[moved(twin, dest, source)]);
            l.CopyTextureRegion(&rows(&readback, 0), 0, 0, 0, &whole(twin), None);
            l.ResourceBarrier(&[
                moved(twin, source, written),
                moved(host_depth, source, tested),
            ]);
        }
        list.close();
        host.execute(&[&list]);
        let mut mapped = std::ptr::null_mut();
        // SAFETY: the copies finished (execute waits); the buffer is 1024 bytes.
        let read = unsafe {
            readback
                .Map(
                    0,
                    Some(&D3D12_RANGE {
                        Begin: 0,
                        End: 1024,
                    }),
                    Some(&mut mapped),
                )
                .unwrap();
            std::slice::from_raw_parts(mapped.cast::<u8>(), 1024)
        };
        for row in 0..4 {
            let at = row * 256;
            assert_eq!(read[at..at + 8], pattern[at..at + 8], "row {row}");
        }
    }

    /// A depth texture with a stencil plane that the title also asks
    /// unordered access for gets a twin of the stencil plane, and what
    /// shaders write there is in the stencil plane once the list is closed:
    /// a pattern put into the twin is read back from the plane.
    #[cfg(windows)]
    #[test]
    fn a_stencil_planes_twin_reaches_the_plane() {
        use std::sync::atomic::Ordering::Relaxed;
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let null = std::ptr::null();
        const DEPTH_WRITE: u32 = 0x10;
        let desc = ResourceDesc {
            flags: 6,
            ..texture(4, 4, 45, 1)
        };
        let mut depth = null;
        assert_eq!(
            create_committed(
                null,
                &PROPERTIES,
                0,
                &desc,
                DEPTH_WRITE,
                null.cast(),
                &RESOURCE_IIDS[0],
                &mut depth
            ),
            S_OK
        );
        let host_depth = host_resource(depth).unwrap();
        let Some(twin) = twin_resource(depth) else {
            return; // twins are turned off
        };
        // SAFETY: live resources.
        let (d, t) = unsafe { (host_depth.GetDesc(), twin.GetDesc()) };
        // R24G8 typeless with depth, R8 typeless with unordered access.
        assert_eq!((d.Format.0, d.Flags.0), (44, 2));
        assert_eq!((t.Format.0, t.Flags.0), (60, 4));

        // Four rows of four texels, 256 bytes apart as copies place them.
        let mut pattern = vec![0u8; 1024];
        for row in 0..4 {
            for b in 0..4 {
                pattern[row * 256 + b] = (1 + row * 4 + b) as u8;
            }
        }
        let (upload, data) = host.upload_buffer(1024).unwrap();
        // SAFETY: the buffer's mapping, 1024 bytes.
        unsafe { std::ptr::copy_nonoverlapping(pattern.as_ptr(), data, 1024) };
        let readback_heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let readback_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            // Room for the rows as the host places them (512 bytes apart
            // for the stencil plane on NVIDIA, not 256).
            Width: 4096,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut readback: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a readback buffer.
        unsafe {
            host.device
                .CreateCommittedResource(
                    &readback_heap,
                    D3D12_HEAP_FLAG_NONE,
                    &readback_desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut readback,
                )
                .unwrap();
        }
        let readback = readback.unwrap();
        // How the host lays the stencil plane (subresource 1) out in a buffer.
        let mut plane = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        // SAFETY: a plain query writing one footprint.
        unsafe {
            host.device
                .GetCopyableFootprints(&d, 1, 1, 0, Some(&mut plane), None, None, None);
        }
        let pattern_rows = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
            Offset: 0,
            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                Format: DXGI_FORMAT(60),
                Width: 4,
                Height: 4,
                Depth: 1,
                RowPitch: 256,
            },
        };

        // A subresource of a texture; rows of a buffer.
        let whole = |texture: &ID3D12Resource, index: u32| D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: index,
            },
        };
        let rows = |buffer: &ID3D12Resource, placed: D3D12_PLACED_SUBRESOURCE_FOOTPRINT| {
            D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: as above.
                pResource: unsafe { std::mem::transmute_copy(buffer) },
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: placed,
                },
            }
        };
        let (written, source, dest) = (
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        let moved = crate::host::transition;

        // The pattern into the twin, where a shader would write it.
        let fill = host.new_list().unwrap();
        // SAFETY: an open list and live resources.
        unsafe {
            let l = &fill.list;
            l.ResourceBarrier(&[moved(twin, written, dest)]);
            l.CopyTextureRegion(&whole(twin, 0), 0, 0, 0, &rows(&upload, pattern_rows), None);
            l.ResourceBarrier(&[moved(twin, dest, written)]);
        }
        fill.close();
        host.execute(&[&fill]);

        // A list in which a shader writes the twin: closing it copies the
        // twin into the stencil plane.
        let list = host.new_list().unwrap();
        assert!(list.twin_use(depth, true, true));
        list.twins_written();
        list.close();
        host.execute(&[&list]);

        // The stencil plane read back from the state the list left it in.
        let state = host_copy(depth).unwrap().state.load(Relaxed);
        let state = D3D12_RESOURCE_STATES(state as i32);
        let back = host.new_list().unwrap();
        // SAFETY: an open list and live resources; a whole subresource.
        unsafe {
            let l = &back.list;
            l.ResourceBarrier(&[moved(host_depth, state, source)]);
            let (to, from) = (rows(&readback, plane), whole(host_depth, 1));
            l.CopyTextureRegion(&to, 0, 0, 0, &from, None);
            l.ResourceBarrier(&[moved(host_depth, source, state)]);
        }
        back.close();
        host.execute(&[&back]);
        let mut mapped = std::ptr::null_mut();
        // SAFETY: the copies finished (execute waits); the buffer is 4096 bytes.
        let read = unsafe {
            readback
                .Map(
                    0,
                    Some(&D3D12_RANGE {
                        Begin: 0,
                        End: 4096,
                    }),
                    Some(&mut mapped),
                )
                .unwrap();
            std::slice::from_raw_parts(mapped.cast::<u8>(), 4096)
        };
        let pitch = plane.Footprint.RowPitch as usize;
        for row in 0..4 {
            let (at, from) = (row * pitch, row * 256);
            assert_eq!(read[at..at + 4], pattern[from..from + 4], "row {row}");
        }
    }

    /// The depth plane of a depth-stencil texture is drawn from the plane's
    /// twin when the list that wrote the twin closes: class numbers are put
    /// into the twin of a texture in the console's 16-bit depth-stencil
    /// format, and draws with the depth test "equal" at the depth the
    /// title's lighting computes for a class (the number times the float
    /// nearest 1/65535) then cover the texels of that class and no other.
    #[cfg(windows)]
    #[test]
    fn a_depth_planes_twin_is_drawn_into_the_plane() {
        use std::sync::atomic::Ordering::Relaxed;
        use windows::Win32::Foundation::RECT;
        use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY;
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
        use windows::core::s;
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let null = std::ptr::null();
        const DEPTH_WRITE: u32 = 0x10;
        let desc = ResourceDesc {
            flags: 6,
            ..texture(4, 4, 118, 1)
        };
        let mut depth = null;
        assert_eq!(
            create_committed(
                null,
                &PROPERTIES,
                0,
                &desc,
                DEPTH_WRITE,
                null.cast(),
                &RESOURCE_IIDS[0],
                &mut depth
            ),
            S_OK
        );
        let host_depth = host_resource(depth).unwrap();
        let Some(plane) = twin_plane_resource(depth) else {
            return; // these twins are turned off
        };
        // SAFETY: live resources.
        let (d, t) = unsafe { (host_depth.GetDesc(), plane.GetDesc()) };
        // R24G8 typeless with depth, R16 typeless with unordered access.
        assert_eq!((d.Format.0, d.Flags.0), (44, 2));
        assert_eq!((t.Format.0, t.Flags.0), (53, 4));

        // Four rows of four class numbers (16 bits each), 256 bytes apart
        // as copies place them. 128 and 129 are the classes whose 24-bit
        // depth lies nearest a half step.
        const CLASSES: [u16; 16] = [
            8, 65, 128, 129, 65, 8, 129, 128, 0, 255, 65, 64, 129, 130, 8, 65,
        ];
        let mut pattern = vec![0u8; 1024];
        for (i, class) in CLASSES.into_iter().enumerate() {
            let at = i / 4 * 256 + i % 4 * 2;
            pattern[at..at + 2].copy_from_slice(&class.to_le_bytes());
        }
        let (upload, data) = host.upload_buffer(1024).unwrap();
        // SAFETY: the buffer's mapping, 1024 bytes.
        unsafe { std::ptr::copy_nonoverlapping(pattern.as_ptr(), data, 1024) };
        let buffer = |kind: D3D12_HEAP_TYPE, state: D3D12_RESOURCE_STATES| {
            let heap = D3D12_HEAP_PROPERTIES {
                Type: kind,
                ..Default::default()
            };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: 1024,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            let mut made: Option<ID3D12Resource> = None;
            // SAFETY: plain creation of a buffer.
            unsafe {
                host.device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        state,
                        None,
                        &mut made,
                    )
                    .unwrap();
            }
            made.unwrap()
        };
        let readback = buffer(D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
        // The colour target the draws cover: 4x4, 8 bits a texel.
        let target = {
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: 4,
                Height: 4,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT(61),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                ..Default::default()
            };
            let mut made: Option<ID3D12Resource> = None;
            // SAFETY: plain creation of a texture.
            unsafe {
                host.device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        D3D12_RESOURCE_STATE_RENDER_TARGET,
                        None,
                        &mut made,
                    )
                    .unwrap();
            }
            made.unwrap()
        };
        let rows_of = |format: i32| D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
            Offset: 0,
            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                Format: DXGI_FORMAT(format),
                Width: 4,
                Height: 4,
                Depth: 1,
                RowPitch: 256,
            },
        };
        // A texture's one subresource; rows of a buffer.
        let whole = |texture: &ID3D12Resource| D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        let rows = |buffer: &ID3D12Resource, placed: D3D12_PLACED_SUBRESOURCE_FOOTPRINT| {
            D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: as above.
                pResource: unsafe { std::mem::transmute_copy(buffer) },
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: placed,
                },
            }
        };
        let (written, source, dest, tested, drawn) = (
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_DEPTH_WRITE,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        );
        let moved = crate::host::transition;

        // The class numbers into the twin, where a shader would write them.
        let fill = host.new_list().unwrap();
        // SAFETY: an open list and live resources.
        unsafe {
            let l = &fill.list;
            l.ResourceBarrier(&[moved(plane, written, dest)]);
            l.CopyTextureRegion(&whole(plane), 0, 0, 0, &rows(&upload, rows_of(53)), None);
            l.ResourceBarrier(&[moved(plane, dest, written)]);
        }
        fill.close();
        host.execute(&[&fill]);

        // A list in which a shader writes the twins: closing it draws the
        // depth plane's twin into the plane.
        let list = host.new_list().unwrap();
        assert!(list.twin_use(depth, true, true));
        list.twins_written();
        list.close();
        host.execute(&[&list]);

        // The draws: a triangle over the target at the depth of a class,
        // as the title's lighting places its rectangles.
        const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants = 1, b0)"
cbuffer Drawn : register(b0) { uint number; };
[RootSignature(RS)]
float4 corner(uint i : SV_VertexID) : SV_Position {
    float z = float(number) * asfloat(0x37800080);
    return float4(float(i & 1) * 4.0 - 1.0, float(i >> 1) * 4.0 - 1.0, z, 1.0);
}
float4 lit() : SV_Target { return 1.0; }
"#;
        let vs = crate::host::compile(SOURCE, s!("corner"), s!("vs_5_1")).unwrap();
        let ps = crate::host::compile(SOURCE, s!("lit"), s!("ps_5_1")).unwrap();
        let code = |bytes: &[u8]| D3D12_SHADER_BYTECODE {
            pShaderBytecode: bytes.as_ptr().cast(),
            BytecodeLength: bytes.len(),
        };
        let mut blend = D3D12_BLEND_DESC::default();
        blend.RenderTarget[0].RenderTargetWriteMask = 0xf;
        let mut formats = [DXGI_FORMAT(0); 8];
        formats[0] = DXGI_FORMAT(61);
        // SAFETY: plain creation; the description points at live data.
        let (signature, pipeline) = unsafe {
            let signature: ID3D12RootSignature =
                host.device.CreateRootSignature(0, &vs[..]).unwrap();
            let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&signature),
                VS: code(&vs[..]),
                PS: code(&ps[..]),
                BlendState: blend,
                SampleMask: u32::MAX,
                RasterizerState: D3D12_RASTERIZER_DESC {
                    FillMode: D3D12_FILL_MODE_SOLID,
                    CullMode: D3D12_CULL_MODE_NONE,
                    DepthClipEnable: true.into(),
                    ..Default::default()
                },
                // As the title's lighting: test "equal", nothing written.
                DepthStencilState: D3D12_DEPTH_STENCIL_DESC {
                    DepthEnable: true.into(),
                    DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ZERO,
                    DepthFunc: D3D12_COMPARISON_FUNC_EQUAL,
                    ..Default::default()
                },
                PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                NumRenderTargets: 1,
                RTVFormats: formats,
                DSVFormat: DXGI_FORMAT(45),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                ..Default::default()
            };
            let pipeline: ID3D12PipelineState =
                host.device.CreateGraphicsPipelineState(&desc).unwrap();
            (signature, pipeline)
        };
        let state = host_copy(depth).unwrap().state.load(Relaxed);
        let state = D3D12_RESOURCE_STATES(state as i32);
        let colour = host.rtv(Some(&target), None);
        let depth_view = host.dsv(
            host_depth,
            Some(&D3D12_DEPTH_STENCIL_VIEW_DESC {
                Format: DXGI_FORMAT(45),
                ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2D,
                ..Default::default()
            }),
        );
        let back = host.new_list().unwrap();
        // SAFETY: an open list and live resources and views.
        unsafe {
            let l = &back.list;
            if state != tested {
                l.ResourceBarrier(&[moved(host_depth, state, tested)]);
            }
            l.ClearRenderTargetView(colour, &[0.0; 4], None);
            l.SetGraphicsRootSignature(&signature);
            l.SetPipelineState(&pipeline);
            l.OMSetRenderTargets(1, Some(&colour), false, Some(&depth_view));
            l.RSSetViewports(&[D3D12_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: 4.0,
                Height: 4.0,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]);
            l.RSSetScissorRects(&[RECT {
                left: 0,
                top: 0,
                right: 4,
                bottom: 4,
            }]);
            l.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(4));
            // Classes 65 and 129 are drawn; the others are not.
            for class in [65u32, 129] {
                l.SetGraphicsRoot32BitConstant(0, class, 0);
                l.DrawInstanced(3, 1, 0, 0);
            }
            l.ResourceBarrier(&[moved(&target, drawn, source)]);
            let (to, from) = (rows(&readback, rows_of(61)), whole(&target));
            l.CopyTextureRegion(&to, 0, 0, 0, &from, None);
        }
        back.close();
        host.execute(&[&back]);
        let mut mapped = std::ptr::null_mut();
        // SAFETY: the copy finished (execute waits); the buffer is 1024 bytes.
        let read = unsafe {
            readback
                .Map(
                    0,
                    Some(&D3D12_RANGE {
                        Begin: 0,
                        End: 1024,
                    }),
                    Some(&mut mapped),
                )
                .unwrap();
            std::slice::from_raw_parts(mapped.cast::<u8>(), 1024)
        };
        for (i, class) in CLASSES.into_iter().enumerate() {
            let covered = read[i / 4 * 256 + i % 4] != 0;
            let drawn = matches!(class, 65 | 129);
            assert_eq!(covered, drawn, "texel {i}, class {class}");
        }
    }

    #[test]
    fn committed_and_placed_resources_map_at_their_gpu_address() {
        let null = std::ptr::null();
        let mut committed = null;
        let hr = create_committed(
            null,
            &PROPERTIES,
            0,
            &buffer(256),
            0,
            null.cast(),
            &RESOURCE_IIDS[0],
            &mut committed,
        );
        assert_eq!(hr, S_OK);
        let mut data = std::ptr::null_mut();
        assert_eq!(
            slot::<Map>(committed, 0x40)(committed, 0, null.cast(), &mut data),
            S_OK
        );
        assert_eq!(slot::<Address>(committed, 0x58)(committed), data as u64);
        // SAFETY: the buffer is at least 256 zeroed bytes.
        assert_eq!(unsafe { *data.add(255) }, 0);
        assert_eq!(slot::<Release>(committed, 0x10)(committed), 0);

        let heap = heap(4 * KIB_64);
        let place = |offset, desc: &ResourceDesc, out: &mut This| {
            create_placed(
                null,
                heap,
                offset,
                desc,
                0,
                null.cast(),
                &RESOURCE_IIDS[0],
                out,
            )
        };
        let mut placed = null;
        assert_eq!(place(KIB_64, &buffer(KIB_64), &mut placed), S_OK);
        // SAFETY: a heap we made.
        let base = unsafe { (*heap.cast::<Heap>()).memory.ptr.as_ptr() } as u64;
        assert_eq!(slot::<Address>(placed, 0x58)(placed), base + KIB_64);
        let mut other = null;
        assert_eq!(
            place(4 * KIB_64, &buffer(1), &mut other),
            E_INVALIDARG,
            "past the end"
        );
        assert_eq!(
            place(100, &buffer(1), &mut other),
            E_INVALIDARG,
            "misaligned"
        );
        // The resource keeps the heap alive.
        assert_eq!(slot::<Release>(heap, 0x10)(heap), 1);
        assert_eq!(slot::<Release>(placed, 0x10)(placed), 0);
    }

    #[test]
    fn texture_floor_scales_with_vram_budget() {
        const GIB: u64 = 1 << 30;
        assert_eq!(texture_floor_bytes(None), 2 * GIB);
        assert_eq!(texture_floor_bytes(Some(4 * GIB)), GIB);
        assert_eq!(texture_floor_bytes(Some(6 * GIB)), 6 * GIB / 4);
        assert_eq!(texture_floor_bytes(Some(8 * GIB)), 2 * GIB);
        assert_eq!(texture_floor_bytes(Some(12 * GIB)), 2 * GIB);
        assert_eq!(texture_floor_bytes(Some(24 * GIB)), 2 * GIB);
        assert_eq!(texture_floor_bytes(Some(32 * GIB)), 2 * GIB);
    }

    /// A placed texture viewed from a later level has a host copy of that
    /// and the smaller levels; a view from an earlier one gets another.
    #[cfg(windows)]
    #[test]
    fn a_placed_texture_is_kept_from_the_level_viewed() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
        if crate::host::host().is_none() || !held_only() {
            return;
        }
        // 256x256 BC7, nine levels, 4 KiB blocks.
        let desc = [3u32, 0, 0, 0, 256, 0, 256, 0x9_0001, 98, 1, 0, 0x105, 0, 0];
        let whole = placed_bytes(&desc).unwrap();
        let part = placed_bytes_from(&desc, 2).unwrap();
        assert!(part * 8 < whole, "{part} of {whole}");
        assert_eq!(placed_bytes_from(&desc, 0), Some(whole));
        // SAFETY: memory of our own that stays.
        let at = unsafe { VirtualAlloc(std::ptr::null(), whole as usize, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
        let texture = texture_at(at as u64, &desc).unwrap();
        want_level(texture, 2);
        let copy = host_copy(texture).unwrap();
        // SAFETY: a live texture.
        let d = unsafe { copy.texture.GetDesc() };
        assert_eq!((copy.base, d.Width, d.Height, d.MipLevels), (2, 64, 64, 7));
        want_level(texture, 5);
        assert_eq!(host_copy(texture).unwrap().serial, copy.serial);
        want_level(texture, 0);
        let more = host_copy(texture).unwrap();
        // SAFETY: as above.
        let d = unsafe { more.texture.GetDesc() };
        assert_eq!((more.base, d.Width, d.MipLevels), (0, 256, 9));
        assert!(more.serial != copy.serial && more.grown.load(std::sync::atomic::Ordering::Relaxed));
    }

    /// A title's own memory of 1 MiB and more is write-watched: the kernel
    /// answers for exactly the pages written, and it is zeroed, on 64 KiB,
    /// and a size that is no multiple of 64 KiB can be asked of to its last
    /// 64 KiB.
    #[test]
    fn big_title_memory_answers_get_write_watch_with_the_pages_written() {
        use windows_sys::Win32::System::Memory::GetWriteWatch;
        const PAGES: usize = 4096;
        let size = (20 << 20) + 0x2200;
        let memory = Memory::zeroed(size, 1 << 16).unwrap();
        assert!(memory.virtual_watched);
        let base = memory.address() as usize;
        assert_eq!(base % (1 << 16), 0);
        // SAFETY: the memory is ours and `size` long.
        let bytes = unsafe { std::slice::from_raw_parts_mut(memory.ptr.as_ptr(), size as usize) };
        assert!(bytes.iter().step_by(4096).all(|&b| b == 0));
        let ask = |first: usize, len: usize| -> Vec<usize> {
            let mut found = vec![std::ptr::null_mut(); len / PAGES + 1];
            let (mut count, mut granularity) = (found.len(), 0u32);
            // SAFETY: room for `count` addresses; the range lies in the allocation.
            let failed = unsafe { GetWriteWatch(1, (base + first) as _, len, found.as_mut_ptr(), &mut count, &mut granularity) };
            assert_eq!(failed, 0);
            assert_eq!(granularity, 4096);
            found[..count].iter().map(|&at| at as usize - base).collect()
        };
        // The first asking finds everything written (a fresh commit counts as
        // that, and the first copy takes it all anyway) and clears it.
        assert_eq!(ask(0, size as usize).len(), (size as usize).div_ceil(4096));
        bytes[5 * 4096 + 7] = 1;
        bytes[300 * 4096] = 2;
        let last = size as usize - 1;
        bytes[last] = 3;
        let written = ask(0, size as usize);
        assert_eq!(written, vec![5 * 4096, 300 * 4096, last / 4096 * 4096]);
        // Asked again, nothing is left: the asking cleared them.
        assert!(ask(0, size as usize).is_empty());
        // The 64 KiB pages the watch asks about, to the last one, are in it.
        let end = (base + size as usize).next_multiple_of(1 << 16) - base;
        assert!(ask(0, end).is_empty());
        // Small memory and memory asked to lie on more than 64 KiB are not.
        assert!(!Memory::zeroed(512 << 10, 1 << 16).unwrap().virtual_watched);
        assert!(!Memory::zeroed(2 << 20, 2 << 20).unwrap().virtual_watched);
    }
}
