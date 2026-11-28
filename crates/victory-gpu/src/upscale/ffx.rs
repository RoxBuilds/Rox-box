//! The public AMD FidelityFX API (the `ffx*` calls and their descriptions),
//! as far as an upscale needs it: written in Rust from the layouts of the
//! SDK's public headers (`ffx_api.h`, `ffx_api_types.h`, `ffx_upscale.h`,
//! `dx12/ffx_api_dx12.h` of FidelityFX SDK v1.1.4; Copyright (C) 2024
//! Advanced Micro Devices, Inc., MIT-style licence, see
//! THIRD_PARTY_NOTICES.md). No header text is copied; only the layout, names
//! and numbers an implementation must agree on.
//!
//! The same API is what a title calls on its own upscaler DLL and what the
//! PC upscaler DLL the user supplies answers, so one set of structures
//! serves both sides (`crate::upscale`, and `victory_hle::modules::fidelityfx`).
//!
//! Booleans of the C structures are `u8` here: a title may hand any byte,
//! and a Rust `bool` that is not 0 or 1 is undefined behaviour.

use std::ffi::c_void;

/// `ffxReturnCode_t` values.
pub const RETURN_OK: u32 = 0;
pub const RETURN_ERROR: u32 = 1;
pub const RETURN_ERROR_UNKNOWN_DESCTYPE: u32 = 2;
pub const RETURN_ERROR_RUNTIME_ERROR: u32 = 3;
pub const RETURN_NO_PROVIDER: u32 = 4;
pub const RETURN_ERROR_MEMORY: u32 = 5;
pub const RETURN_ERROR_PARAMETER: u32 = 6;

/// Description types (`ffxStructType_t`) used here.
pub const CREATE_CONTEXT_DESC_TYPE_BACKEND_DX12: u64 = 0x0000_0002;
pub const QUERY_DESC_TYPE_GET_VERSIONS: u64 = 4;
pub const CREATE_CONTEXT_DESC_TYPE_UPSCALE: u64 = 0x0001_0000;
pub const DISPATCH_DESC_TYPE_UPSCALE: u64 = 0x0001_0001;

/// `FfxApiCreateContextUpscaleFlags` bits whose meaning has been the same in
/// every version of the API: HDR colour, display-resolution motion vectors,
/// jitter cancellation in the vectors, inverted depth, infinite depth, auto
/// exposure, dynamic resolution. Debug checking (bit 7) and later bits are
/// not passed to the host.
pub const UPSCALE_CREATE_FLAGS_KEPT: u32 = 0x7f;
pub const UPSCALE_ENABLE_HIGH_DYNAMIC_RANGE: u32 = 1 << 0;
pub const UPSCALE_ENABLE_DEPTH_INVERTED: u32 = 1 << 3;
pub const UPSCALE_ENABLE_AUTO_EXPOSURE: u32 = 1 << 5;

/// `FfxApiDispatchFsrUpscaleFlags` passed on: non-linear colour (sRGB, PQ).
/// The debug view (bit 0) is never drawn by the host.
pub const UPSCALE_DISPATCH_FLAGS_KEPT: u32 = (1 << 1) | (1 << 2);

/// `FfxApiResourceState`.
pub const STATE_COMMON: u32 = 1 << 0;
pub const STATE_UNORDERED_ACCESS: u32 = 1 << 1;
pub const STATE_COMPUTE_READ: u32 = 1 << 2;
pub const STATE_PIXEL_READ: u32 = 1 << 3;
pub const STATE_PIXEL_COMPUTE_READ: u32 = STATE_PIXEL_READ | STATE_COMPUTE_READ;
pub const STATE_COPY_SRC: u32 = 1 << 4;
pub const STATE_COPY_DEST: u32 = 1 << 5;
pub const STATE_RENDER_TARGET: u32 = 1 << 8;

/// `FfxApiResourceType` (sequential in the header).
pub const RESOURCE_TYPE_BUFFER: u32 = 0;
pub const RESOURCE_TYPE_TEXTURE1D: u32 = 1;
pub const RESOURCE_TYPE_TEXTURE2D: u32 = 2;
pub const RESOURCE_TYPE_TEXTURE_CUBE: u32 = 3;
pub const RESOURCE_TYPE_TEXTURE3D: u32 = 4;

/// `FfxApiResourceUsage`.
pub const USAGE_READ_ONLY: u32 = 0;
pub const USAGE_UAV: u32 = 1 << 1;
pub const USAGE_DEPTHTARGET: u32 = 1 << 2;
pub const USAGE_STENCILTARGET: u32 = 1 << 5;

/// `ffxApiHeader`: every description starts with its type and the next
/// description of a chain.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Header {
    pub kind: u64,
    pub next: *mut Header,
}

/// `FfxApiDimensions2D`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Dimensions {
    pub width: u32,
    pub height: u32,
}

/// `FfxApiFloatCoords2D`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Coords {
    pub x: f32,
    pub y: f32,
}

/// `FfxApiResourceDescription` (`width`/`size`, `height`/`stride`,
/// `depth`/`alignment` are unions of one word each).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResourceDescription {
    pub kind: u32,
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub mip_count: u32,
    pub flags: u32,
    pub usage: u32,
}

/// `FfxApiResource`: on D3D12 an `ID3D12Resource*`, what it is, and the
/// state it is in when the dispatch is recorded.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resource {
    pub resource: *mut c_void,
    pub description: ResourceDescription,
    pub state: u32,
}

impl Default for Resource {
    fn default() -> Self {
        Resource { resource: std::ptr::null_mut(), description: ResourceDescription::default(), state: 0 }
    }
}

/// `ffxApiMessage`: `(type, wide text)`.
pub type Message = Option<unsafe extern "system" fn(u32, *const u16)>;

/// `ffxCreateContextDescUpscale`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CreateUpscale {
    pub header: Header,
    pub flags: u32,
    pub max_render: Dimensions,
    pub max_upscale: Dimensions,
    pub message: Message,
}

/// `ffxCreateBackendDX12Desc`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CreateBackendDx12 {
    pub header: Header,
    pub device: *mut c_void,
}

/// `ffxQueryDescGetVersions`: `count` is the room in the arrays going in and
/// the number of versions coming out.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct QueryVersions {
    pub header: Header,
    pub create_kind: u64,
    pub device: *mut c_void,
    pub count: *mut u64,
    pub ids: *mut u64,
    pub names: *mut *const std::ffi::c_char,
}

/// `ffxDispatchDescUpscale`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DispatchUpscale {
    pub header: Header,
    pub command_list: *mut c_void,
    pub color: Resource,
    pub depth: Resource,
    pub motion_vectors: Resource,
    pub exposure: Resource,
    pub reactive: Resource,
    pub transparency_and_composition: Resource,
    pub output: Resource,
    pub jitter_offset: Coords,
    pub motion_vector_scale: Coords,
    pub render_size: Dimensions,
    pub upscale_size: Dimensions,
    pub enable_sharpening: u8,
    pub sharpness: f32,
    pub frame_time_delta: f32,
    pub pre_exposure: f32,
    pub reset: u8,
    pub camera_near: f32,
    pub camera_far: f32,
    pub camera_fov_angle_vertical: f32,
    pub view_space_to_meters_factor: f32,
    pub flags: u32,
}

/// The API's five entry points.
pub type CreateContext = unsafe extern "system" fn(*mut *mut c_void, *mut Header, *const c_void) -> u32;
pub type DestroyContext = unsafe extern "system" fn(*mut *mut c_void, *const c_void) -> u32;
pub type Configure = unsafe extern "system" fn(*mut *mut c_void, *const Header) -> u32;
pub type Query = unsafe extern "system" fn(*mut *mut c_void, *mut Header) -> u32;
pub type Dispatch = unsafe extern "system" fn(*mut *mut c_void, *const Header) -> u32;

/// The exported names of the entry points, in the order [`Entry`] numbers
/// them.
pub const ENTRY_NAMES: [&str; 5] = ["ffxCreateContext", "ffxDestroyContext", "ffxConfigure", "ffxQuery", "ffxDispatch"];

/// An entry point, by its place in [`ENTRY_NAMES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    CreateContext = 0,
    DestroyContext = 1,
    Configure = 2,
    Query = 3,
    Dispatch = 4,
}

impl Entry {
    pub fn named(name: &str) -> Option<Entry> {
        [Entry::CreateContext, Entry::DestroyContext, Entry::Configure, Entry::Query, Entry::Dispatch]
            .into_iter()
            .find(|e| ENTRY_NAMES[*e as usize] == name)
    }
}

/// The description of type `kind` in the chain that starts at `first`.
///
/// # Safety
/// `first` is null or the start of a chain of API descriptions, each
/// starting with a [`Header`] whose `next` is null or another one.
pub unsafe fn find(first: *const Header, kind: u64) -> Option<*const Header> {
    let mut at = first;
    // A chain is a handful long; a loop in a broken one ends here.
    for _ in 0..64 {
        if at.is_null() {
            return None;
        }
        // SAFETY: guaranteed by the caller.
        let header = unsafe { at.read_unaligned() };
        if header.kind == kind {
            return Some(at);
        }
        at = header.next;
    }
    None
}

/// The FidelityFX surface format of a DXGI format, as the SDK's DX12 helper
/// maps them (depth formats as the colour format of their depth plane);
/// 0 (unknown) for the rest.
pub fn surface_format(dxgi: u32) -> u32 {
    // FfxApiSurfaceFormat, sequential from 0 in the header.
    const R32G32B32A32_TYPELESS: u32 = 1;
    const R32G32B32A32_UINT: u32 = 2;
    const R32G32B32A32_FLOAT: u32 = 3;
    const R16G16B16A16_FLOAT: u32 = 4;
    const R32G32_FLOAT: u32 = 6;
    const R8_UINT: u32 = 7;
    const R32_UINT: u32 = 8;
    const R8G8B8A8_TYPELESS: u32 = 9;
    const R8G8B8A8_UNORM: u32 = 10;
    const R8G8B8A8_SNORM: u32 = 11;
    const R8G8B8A8_SRGB: u32 = 12;
    const B8G8R8A8_TYPELESS: u32 = 13;
    const B8G8R8A8_UNORM: u32 = 14;
    const B8G8R8A8_SRGB: u32 = 15;
    const R11G11B10_FLOAT: u32 = 16;
    const R10G10B10A2_UNORM: u32 = 17;
    const R16G16_FLOAT: u32 = 18;
    const R16G16_UINT: u32 = 19;
    const R16_FLOAT: u32 = 21;
    const R16_UINT: u32 = 22;
    const R16_UNORM: u32 = 23;
    const R16_SNORM: u32 = 24;
    const R8_UNORM: u32 = 25;
    const R8G8_UNORM: u32 = 26;
    const R8G8_UINT: u32 = 27;
    const R32_FLOAT: u32 = 28;
    const R9G9B9E5_SHAREDEXP: u32 = 29;
    const R16G16B16A16_TYPELESS: u32 = 30;
    const R32G32_TYPELESS: u32 = 31;
    const R10G10B10A2_TYPELESS: u32 = 32;
    const R16G16_TYPELESS: u32 = 33;
    const R16_TYPELESS: u32 = 34;
    const R8_TYPELESS: u32 = 35;
    const R8G8_TYPELESS: u32 = 36;
    const R32_TYPELESS: u32 = 37;
    // DXGI_FORMAT numbers.
    match dxgi {
        1 => R32G32B32A32_TYPELESS,
        2 => R32G32B32A32_FLOAT,
        3 => R32G32B32A32_UINT,
        9 => R16G16B16A16_TYPELESS,
        10 => R16G16B16A16_FLOAT,
        15 => R32G32_TYPELESS,
        16 => R32G32_FLOAT,
        // R32G8X24_TYPELESS, D32_FLOAT_S8X24_UINT, R32_FLOAT_X8X24_TYPELESS.
        19..=21 => R32_FLOAT,
        // X32_TYPELESS_G8X24_UINT.
        22 => R8_UINT,
        23 => R10G10B10A2_TYPELESS,
        24 => R10G10B10A2_UNORM,
        26 => R11G11B10_FLOAT,
        27 => R8G8B8A8_TYPELESS,
        28 => R8G8B8A8_UNORM,
        29 => R8G8B8A8_SRGB,
        31 => R8G8B8A8_SNORM,
        33 => R16G16_TYPELESS,
        34 => R16G16_FLOAT,
        36 => R16G16_UINT,
        39 => R32_TYPELESS,
        // D32_FLOAT, R32_FLOAT.
        40 | 41 => R32_FLOAT,
        42 => R32_UINT,
        // R24G8_TYPELESS, D24_UNORM_S8_UINT, R24_UNORM_X8_TYPELESS.
        44..=46 => R32_UINT,
        // X24_TYPELESS_G8_UINT.
        47 => R8_UINT,
        48 => R8G8_TYPELESS,
        49 => R8G8_UNORM,
        50 => R8G8_UINT,
        53 => R16_TYPELESS,
        54 => R16_FLOAT,
        // D16_UNORM, R16_UNORM.
        55 | 56 => R16_UNORM,
        57 => R16_UINT,
        58 => R16_SNORM,
        60 => R8_TYPELESS,
        // R8_UNORM, A8_UNORM.
        61 | 65 => R8_UNORM,
        62 => R8_UINT,
        67 => R9G9B9E5_SHAREDEXP,
        87 => B8G8R8A8_UNORM,
        90 => B8G8R8A8_TYPELESS,
        91 => B8G8R8A8_SRGB,
        _ => 0,
    }
}

/// The FidelityFX state of a host D3D12 resource state: what the API is
/// told a resource is in when the host dispatch is recorded.
pub fn resource_state(d3d12: u32) -> u32 {
    const RENDER_TARGET: u32 = 0x4;
    const UNORDERED_ACCESS: u32 = 0x8;
    const NON_PIXEL_SHADER_RESOURCE: u32 = 0x40;
    const PIXEL_SHADER_RESOURCE: u32 = 0x80;
    const COPY_DEST: u32 = 0x400;
    const COPY_SOURCE: u32 = 0x800;
    if d3d12 & UNORDERED_ACCESS != 0 {
        return STATE_UNORDERED_ACCESS;
    }
    if d3d12 & RENDER_TARGET != 0 {
        return STATE_RENDER_TARGET;
    }
    let read = match (d3d12 & NON_PIXEL_SHADER_RESOURCE != 0, d3d12 & PIXEL_SHADER_RESOURCE != 0) {
        (true, true) => STATE_PIXEL_COMPUTE_READ,
        (true, false) => STATE_COMPUTE_READ,
        (false, true) => STATE_PIXEL_READ,
        (false, false) => 0,
    };
    let copy = if d3d12 & COPY_SOURCE != 0 { STATE_COPY_SRC } else { 0 };
    match read | copy {
        0 if d3d12 & COPY_DEST != 0 => STATE_COPY_DEST,
        0 => STATE_COMMON,
        both => both,
    }
}

// The layouts the headers give (x64).
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<Header>() == 16);
    assert!(size_of::<ResourceDescription>() == 32);
    assert!(size_of::<Resource>() == 48);
    assert!(offset_of!(Resource, state) == 40);
    assert!(size_of::<CreateUpscale>() == 48);
    assert!(offset_of!(CreateUpscale, flags) == 16);
    assert!(offset_of!(CreateUpscale, max_render) == 20);
    assert!(offset_of!(CreateUpscale, max_upscale) == 28);
    assert!(offset_of!(CreateUpscale, message) == 40);
    assert!(size_of::<CreateBackendDx12>() == 24);
    assert!(size_of::<QueryVersions>() == 56);
    assert!(offset_of!(DispatchUpscale, command_list) == 16);
    assert!(offset_of!(DispatchUpscale, color) == 24);
    assert!(offset_of!(DispatchUpscale, output) == 312);
    assert!(offset_of!(DispatchUpscale, jitter_offset) == 360);
    assert!(offset_of!(DispatchUpscale, motion_vector_scale) == 368);
    assert!(offset_of!(DispatchUpscale, render_size) == 376);
    assert!(offset_of!(DispatchUpscale, upscale_size) == 384);
    assert!(offset_of!(DispatchUpscale, enable_sharpening) == 392);
    assert!(offset_of!(DispatchUpscale, sharpness) == 396);
    assert!(offset_of!(DispatchUpscale, reset) == 408);
    assert!(offset_of!(DispatchUpscale, camera_near) == 412);
    assert!(offset_of!(DispatchUpscale, flags) == 428);
    assert!(size_of::<DispatchUpscale>() == 432);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_points_are_found_by_their_exported_names() {
        assert_eq!(Entry::named("ffxDispatch"), Some(Entry::Dispatch));
        assert_eq!(Entry::named("ffxCreateContext"), Some(Entry::CreateContext));
        assert_eq!(Entry::named("ffxdispatch"), None);
        assert_eq!(Entry::named("ffxCreateContextEx"), None);
    }

    #[test]
    fn a_description_is_found_in_its_chain() {
        let mut last = Header { kind: CREATE_CONTEXT_DESC_TYPE_BACKEND_DX12, next: std::ptr::null_mut() };
        let first = Header { kind: CREATE_CONTEXT_DESC_TYPE_UPSCALE, next: &mut last };
        // SAFETY: a chain of two made here.
        unsafe {
            assert_eq!(find(&first, CREATE_CONTEXT_DESC_TYPE_UPSCALE), Some(&raw const first));
            assert_eq!(find(&first, CREATE_CONTEXT_DESC_TYPE_BACKEND_DX12), Some(&raw const last));
            assert_eq!(find(&first, DISPATCH_DESC_TYPE_UPSCALE), None);
            assert_eq!(find(std::ptr::null(), DISPATCH_DESC_TYPE_UPSCALE), None);
        }
    }

    #[test]
    fn a_chain_that_loops_ends() {
        let mut a = Header { kind: 1, next: std::ptr::null_mut() };
        a.next = &mut a;
        // SAFETY: a chain of one that names itself.
        assert_eq!(unsafe { find(&a, 2) }, None);
    }

    #[test]
    fn host_formats_are_the_sdks() {
        // R16G16B16A16_FLOAT, R11G11B10_FLOAT, R16G16_FLOAT, D32_FLOAT, R32_TYPELESS, R8_UNORM.
        assert_eq!(surface_format(10), 4);
        assert_eq!(surface_format(26), 16);
        assert_eq!(surface_format(34), 18);
        assert_eq!(surface_format(40), 28);
        assert_eq!(surface_format(39), 37);
        assert_eq!(surface_format(61), 25);
        // B8G8R8A8_UNORM_SRGB; BC1 is none of them.
        assert_eq!(surface_format(91), 15);
        assert_eq!(surface_format(71), 0);
    }

    #[test]
    fn host_states_are_told_as_the_api_names_them() {
        assert_eq!(resource_state(0x40 | 0x80), STATE_PIXEL_COMPUTE_READ);
        assert_eq!(resource_state(0x40), STATE_COMPUTE_READ);
        assert_eq!(resource_state(0x8), STATE_UNORDERED_ACCESS);
        assert_eq!(resource_state(0), STATE_COMMON);
        assert_eq!(resource_state(0x400), STATE_COPY_DEST);
        assert_eq!(resource_state(0x800 | 0x40), STATE_COPY_SRC | STATE_COMPUTE_READ);
        assert_eq!(resource_state(0x4), STATE_RENDER_TARGET);
    }
}
