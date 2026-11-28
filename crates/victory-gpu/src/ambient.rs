//! A stand-in for the indirect light a title traces rays for.
//!
//! With the ray-tracing structure builder off (`crate::pipeline::builder_runs`)
//! the title's rays find nothing. When an external shader profile
//! (`VICTORY_SHADER_PROFILE`) identifies a title's screen-probe resolve, surface
//! radiance cache, and local air-inscattering compute shaders along with their
//! root-table bindings, the resolved diffuse light is raised to an estimate
//! computed from screen-space radiance and depth.
//!
//! That a stand-in can be inserted there comes from upstream GPL-3.0-or-later
//! work (`upstream AMD fork`). This implementation estimates directional
//! harmonics in 64x36 screen cells at 160x90 probe points weighted by depth
//! similarity, keeping a slow temporal mean across frames so turning toward a
//! dark corner fades smoothly instead of snapping black.
//!
//! `VICTORY_AMBIENT`:
//! - unset or `0`/`off`: the title's own output.
//! - a number: that share of the estimate (1: all of it).
//! - `constant:<level>`: a fixed ambient constant level.
//!
//! `VICTORY_AMBIENT_SHADE=<strength>,<radius in metres>` sets the crease shading.
//! `VICTORY_AMBIENT_FLOOR=<level>` ([`floor`]) sets the minimum cell luminance
//! blended toward the surrounding sky colour (white indoors).
//! `VICTORY_AMBIENT_AIR=<multiple>` bounds volumetric air scattering by a
//! multiple of surrounding ambient light.
//!
//! Reflections are traced too: rays a screen-space march resolves take the
//! light seen on screen; the others go to the structures, find nothing, and
//! the title's miss program gives each the sky's light, indoors too (far
//! brighter than the room, and blue). `VICTORY_AMBIENT_SPECULAR` ([`specular`])
//! puts in, right after that program, the same estimate taken along the
//! reflected direction ([`Role::Miss`]); a screen-space resolve that comes
//! next still writes what it found over it. The profile names the program
//! (`miss_substring`) and may name its bindings and surface layers.

use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::s;

use crate::host::{HostList, host};
use crate::shader_profile::AmbientProfile;

/// Programs of the title the stand-in follows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Role {
    /// Resolves the diffuse light.
    Resolve,
    /// Keeps the light visible surfaces send out.
    Radiance,
    /// Adds local light to the volumetric air scattering buffer.
    Air,
    /// Gives the reflection rays that found nothing the sky's light.
    Miss,
}

fn role_with(profile: &AmbientProfile, container: &[u8]) -> Option<Role> {
    let has = |sub: &str| {
        !sub.is_empty()
            && container
                .windows(sub.len())
                .any(|w| w == sub.as_bytes())
    };
    if has(&profile.resolve_substring) {
        Some(Role::Resolve)
    } else if has(&profile.radiance_substring) {
        Some(Role::Radiance)
    } else if has(&profile.air_substring) {
        Some(Role::Air)
    } else if has(&profile.miss_substring) && !profile.miss_not_substrings.iter().any(|n| has(n)) {
        Some(Role::Miss)
    } else {
        None
    }
}

/// The program's role in ambient estimation, matched against the configured
/// external shader profile (`VICTORY_SHADER_PROFILE`).
pub(crate) fn role(container: &[u8]) -> Option<Role> {
    role_with(&crate::shader_profile::current().ambient, container)
}

/// Root-table binding `(root_slot, table_offset_dwords)` configured for `role`
/// in the active shader profile, if any.
pub(crate) fn binding(role: Role) -> Option<(u8, u32)> {
    let ambient = &crate::shader_profile::current().ambient;
    match role {
        Role::Resolve => ambient.light_binding,
        Role::Radiance => ambient.radiance_binding,
        Role::Air => ambient.air_binding,
        Role::Miss => ambient.miss_light_binding,
    }
}

/// Root-table binding of the depth the miss program reads, if configured.
pub(crate) fn miss_depth_binding() -> Option<(u8, u32)> {
    crate::shader_profile::current().ambient.miss_depth_binding
}

/// Whether a dispatch of the miss program whose first root constant is
/// `layer` (`None`: it has none) is followed by the stand-in for
/// reflections: by the profile's `miss_layers`, and `miss_alpha_layers` with
/// [`specular_alpha`]; every dispatch when it names none.
pub(crate) fn miss_layer_followed(layer: Option<u32>) -> bool {
    let a = &crate::shader_profile::current().ambient;
    layer_followed(&a.miss_layers, &a.miss_alpha_layers, specular_alpha(), layer)
}

fn layer_followed(layers: &[u32], alpha: &[u32], alpha_too: bool, layer: Option<u32>) -> bool {
    if layers.is_empty() && alpha.is_empty() {
        return true;
    }
    layer.is_some_and(|l| layers.contains(&l) || (alpha_too && alpha.contains(&l)))
}

/// `VICTORY_AMBIENT_SPECULAR_ALPHA=1`: the stand-in for reflections follows
/// the profile's `miss_alpha_layers` too. Off by default: the rays of that
/// layer are of transparent surfaces (glass, foam on water), while the depth
/// the stand-in reads is of the opaque surface behind them; on water that is
/// the bottom, the reflected direction made of it is wrong and the estimate
/// dark. Ported from trustno1-coder/VIctory-NVIDIA 9f6fb77.
fn specular_alpha() -> bool {
    static SET: OnceLock<bool> = OnceLock::new();
    *SET.get_or_init(|| std::env::var("VICTORY_AMBIENT_SPECULAR_ALPHA").is_ok_and(|v| v.trim() == "1"))
}

/// Root-table binding `(root_slot, table_offset_dwords)` for scene depth in the
/// active shader profile, if configured.
pub(crate) fn depth_binding() -> Option<(u8, u32)> {
    crate::shader_profile::current().ambient.depth_binding
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Set {
    Off,
    Estimate(f32),
    Constant(f32),
}

fn parse(set: &str) -> Set {
    let set = set.trim();
    let number = |t: &str| t.trim().parse::<f32>().ok().filter(|v| v.is_finite() && *v > 0.0);
    match set.strip_prefix("constant:") {
        Some(level) => number(level).map_or(Set::Off, Set::Constant),
        None => number(set).map_or(Set::Off, Set::Estimate),
    }
}

fn set() -> Set {
    static SET: OnceLock<Set> = OnceLock::new();
    *SET.get_or_init(|| parse(&std::env::var("VICTORY_AMBIENT").unwrap_or_default()))
}

/// Whether a stand-in is on (for the diffuse light or for reflections):
/// set, and the title's rays find nothing.
pub(crate) fn on() -> bool {
    (set() != Set::Off || specular().is_some()) && !crate::pipeline::builder_runs()
}

/// The share of the estimate and of the sky ([`specular`]).
#[derive(Clone, Copy, PartialEq, Debug)]
struct Specular {
    share: f32,
    sky: f32,
}

fn parse_specular(set: &str) -> Option<Specular> {
    let mut parts = set.split(',').map(|t| t.trim().parse::<f32>().ok().filter(|v| v.is_finite()));
    let share = parts.next().flatten().filter(|v| *v > 0.0)?;
    let sky = parts.next().flatten().map_or(0.0, |v| v.clamp(0.0, 1.0));
    Some(Specular { share, sky })
}

/// `VICTORY_AMBIENT_SPECULAR=<share>[,<sky>]`: reflections whose rays found
/// nothing take `share` of the estimate along the reflected direction
/// instead of the sky's light; `sky` (0 to 1, 0 if not given) of them stays
/// the title's sky, made no brighter than the estimate. Unset or 0: the
/// title's sky. Off by default (an approximation). Needs the profile's
/// radiance role and binding (the light of a frame is kept there) and its
/// `miss_substring`. Ported from trustno1-coder/VIctory-NVIDIA 2db59d4,
/// f89b79d and 9f6fb77.
fn specular() -> Option<Specular> {
    static SET: OnceLock<Option<Specular>> = OnceLock::new();
    *SET.get_or_init(|| parse_specular(&std::env::var("VICTORY_AMBIENT_SPECULAR").unwrap_or_default()))
}

/// `VICTORY_AMBIENT_CLEAN=1`: the raise drops what the resolve left that is
/// far from any light's colour before it bounds it: magenta and violet
/// (green far under red and blue; blue over 2.5 times green and over red).
/// The resolve may leave such leftovers of other memory in the light image,
/// and the raise bounds what it keeps by luminance only, which weighs blue
/// least, so a blue-heavy leftover passes and glows on bright surfaces. Sky
/// light (green above 0.4 times blue) and sunsets (low blue) are not
/// touched. Off by default (a filter by colour). Ported from
/// trustno1-coder/VIctory-NVIDIA 826ba15 (from Belobug/VIctory-Belobug-AMD,
/// its filter of magenta probes).
fn clean() -> bool {
    static SET: OnceLock<bool> = OnceLock::new();
    *SET.get_or_init(|| std::env::var("VICTORY_AMBIENT_CLEAN").is_ok_and(|v| v.trim() == "1"))
}

/// The most the air scatters, as a multiple of the light around
/// (`VICTORY_AMBIENT_AIR=<multiple>`; unset or 0: what the title has).
fn air() -> f32 {
    static SET: OnceLock<f32> = OnceLock::new();
    *SET.get_or_init(|| {
        let set = std::env::var("VICTORY_AMBIENT_AIR").unwrap_or_default();
        set.trim().parse().ok().filter(|v: &f32| v.is_finite() && *v > 0.0).unwrap_or(0.0)
    })
}

/// The night floor, by default: in the units of the light the title keeps
/// for its screen traces, which the `keep` pass counts up to 20 a sample
/// ("lamps and glints"), so that a lit day scene's slow mean is a few units;
/// 0.05 is about a fiftieth of that. A guess from the code, not a reading of
/// a frame: set `VICTORY_AMBIENT_FLOOR` to tune it.
const FLOOR: f32 = 0.05;

/// `VICTORY_AMBIENT_FLOOR`: unset or empty, [`FLOOR`]; `0` or `off`, no
/// floor (as before); a number of 0 or more, that floor; anything else,
/// [`FLOOR`].
fn parse_floor(set: Option<&str>) -> f32 {
    match set.map(str::trim) {
        None | Some("") => FLOOR,
        Some("off") => 0.0,
        Some(v) => v.parse::<f32>().ok().filter(|v| v.is_finite() && *v >= 0.0).unwrap_or(FLOOR),
    }
}

/// The least a cell of the estimate sends out, in the units of the light
/// the title keeps for its screen traces; read once. The light added to a
/// cell below it has the colour of what the resolve left around that cell
/// (the sky's light outdoors) and is white where it left next to nothing
/// (indoors); creases and `VICTORY_AMBIENT=<share>` still scale it.
fn floor() -> f32 {
    static SET: OnceLock<f32> = OnceLock::new();
    *SET.get_or_init(|| parse_floor(std::env::var("VICTORY_AMBIENT_FLOOR").ok().as_deref()))
}

/// Strength and radius of the creases' shade.
fn shade() -> [f32; 2] {
    static SET: OnceLock<[f32; 2]> = OnceLock::new();
    *SET.get_or_init(|| {
        let mut shade = [1.0, 0.6];
        let set = std::env::var("VICTORY_AMBIENT_SHADE").unwrap_or_default();
        for (to, v) in shade.iter_mut().zip(set.split(',')) {
            if let Ok(v) = v.trim().parse() {
                *to = v;
            }
        }
        shade
    })
}

const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=12, b0), DescriptorTable(SRV(t0)), DescriptorTable(UAV(u0)), UAV(u1), UAV(u2)"
#define CX 64
#define CY 36
#define CELLS (CX * CY)
#define PX 160
#define PY 90
cbuffer Set : register(b0) {
    float level; float constant; float strength; float radius;
    float near; float tan_y; uint width; uint height;
    float follow; float most; float air; float lowest;
};
Texture2D<float> depth : register(t0);
// Cells: the light they send out (raised to the night floor by `place`),
// then, at CELLS, the slow mean (taken before the floor), from CELLS + 1 on
// where they are.
RWStructuredBuffer<float4> cells : register(u1);
// Three a point: red, green and blue, each a mean and a direction.
RWStructuredBuffer<float4> points : register(u2);
static const float3 LUMA = float3(0.2126, 0.7152, 0.0722);

// The point seen at a texel, in metres from the eye (x right, y up, z away).
float3 seen(int2 p, float2 size) {
    float d = depth.Load(int3(clamp(p, int2(0, 0), int2(size) - 1), 0));
    float z = min(near / max(d, 1e-7), 1000.0);
    float2 ndc = (float2(p) + 0.5) / size * 2.0 - 1.0;
    return float3(ndc.x * tan_y * size.x / size.y * z, -ndc.y * tan_y * z, z);
}

#ifdef KEEP
RWTexture2D<float3> radiance : register(u0);
[RootSignature(RS)]
[numthreads(8, 4, 1)]
void keep(uint3 at : SV_DispatchThreadID) {
    uint w, h;
    radiance.GetDimensions(w, h);
    float3 sum = 0;
    for (uint y = 0; y < 5; y++) for (uint x = 0; x < 5; x++) {
        uint2 p = uint2((float2(at.xy) + (float2(x, y) + 0.5) / 5.0) * float2(w, h) / float2(CX, CY));
        float3 c = max(radiance[p], 0.0);
        c = any(isnan(c)) || any(isinf(c)) ? 0.0 : c;
        // Lamps and glints count, but not for everything.
        sum += c / max(1.0, dot(c, LUMA) / 20.0);
    }
    cells[at.y * CX + at.x] = float4(sum / 25.0, 1.0);
}
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void mean() {
    float3 sum = 0;
    for (uint i = 0; i < CELLS; i++) sum += cells[i].rgb;
    float4 was = cells[CELLS];
    float3 now = sum / CELLS;
    cells[CELLS] = float4(was.a > 0.0 ? lerp(was.rgb, now, follow) : now, 1.0);
}
#endif

#if defined(PLACE) || defined(RAISE)
// The diffuse light the resolve wrote, R9G9B9E5 as 32-bit integers.
RWTexture2D<uint> light : register(u0);
float3 unpack(uint v) {
    return float3(v & 511, (v >> 9) & 511, (v >> 18) & 511) * exp2(float(v >> 27) - 24.0);
}
#endif

#ifdef PLACE
// The night floor: a cell sends out no less than `lowest`. The light added
// has the colour of `sky`, what the resolve left around the cell (the sky's
// light outdoors), made paler until no channel is past 4 times its
// brightness, and is white where `sky` is next to nothing (indoors). As
// `floored` in ambient.rs, which the tests run.
float3 floored(float3 l, float3 sky) {
    float has = dot(l, LUMA);
    if (has >= lowest) return l;
    float s = dot(sky, LUMA);
    float3 c = sky / max(s, 1e-9);
    float top = max(c.r, max(c.g, c.b));
    float t = (top > 4.0 ? 3.0 / (top - 1.0) : 1.0) * saturate(s / (0.1 * lowest));
    return l + (lowest - has) * lerp(float3(1.0, 1.0, 1.0), c, t);
}
[RootSignature(RS)]
[numthreads(8, 4, 1)]
void place(uint3 at : SV_DispatchThreadID) {
    float2 size = float2(width, height);
    float3 p = seen(int2((float2(at.xy) + 0.5) * size / float2(CX, CY)), size);
    uint i = at.y * CX + at.x;
    cells[CELLS + 1 + i] = float4(p, p.z * p.z);
    if (lowest > 0.0) {
        // What the resolve left, read before the raise writes over it. A
        // floored cell floors to itself, so a second resolve before the
        // radiance pass writes the cells again changes nothing; the slow
        // mean is taken from those, without the floor.
        float3 sky = 0;
        for (uint y = 0; y < 4; y++) for (uint x = 0; x < 4; x++) {
            uint2 q = uint2((float2(at.xy) + (float2(x, y) + 0.5) / 4.0) * size / float2(CX, CY));
            sky += unpack(light[min(q, uint2(width, height) - 1)]);
        }
        cells[i] = float4(floored(cells[i].rgb, sky / 16.0), 1.0);
    }
}
#endif

#ifdef GATHER
[RootSignature(RS)]
[numthreads(8, 2, 1)]
void gather(uint3 at : SV_DispatchThreadID) {
    float2 size = float2(width, height);
    float3 p = seen(int2((float2(at.xy) + 0.5) * size / float2(PX, PY)), size);
    float r2 = 0.25 + 0.0025 * p.z * p.z;
    float whole = 0;
    float3 toward = 0, light = 0, red = 0, green = 0, blue = 0;
    for (uint i = 0; i < CELLS; i++) {
        float4 c = cells[CELLS + 1 + i];
        float3 L = cells[i].rgb;
        float3 d = c.xyz - p;
        float dd = dot(d, d);
        float w = c.w / (dd + r2);
        float3 dir = d / sqrt(dd + 0.25 * r2);
        whole += w;
        toward += w * dir;
        light += w * L;
        red += w * L.r * dir;
        green += w * L.g * dir;
        blue += w * L.b * dir;
    }
    whole = max(whole, 1e-6);
    light /= whole;
    toward /= whole;
    uint at0 = (at.y * PX + at.x) * 3;
    points[at0] = float4(red / whole - light.r * toward, light.r);
    points[at0 + 1] = float4(green / whole - light.g * toward, light.g);
    points[at0 + 2] = float4(blue / whole - light.b * toward, light.b);
}
#endif

#ifdef AIR
RWTexture3D<float3> scattered : register(u0);
// No more than a multiple of the light around: of the room as the last
// seconds showed it, and of the surface seen in this direction (towards a
// window the air outside may be as bright as the sky is).
[RootSignature(RS)]
[numthreads(8, 8, 2)]
void clear_air(uint3 at : SV_DispatchThreadID) {
    uint w, h, d;
    scattered.GetDimensions(w, h, d);
    if (at.x >= w || at.y >= h || at.z >= d) return;
    uint2 q = min(uint2((float2(at.xy) + 0.5) / float2(w, h) * float2(PX, PY)), uint2(PX - 1, PY - 1));
    uint at0 = (q.y * PX + q.x) * 3;
    float3 seen_there = float3(points[at0].w, points[at0 + 1].w, points[at0 + 2].w);
    float far = float(at.z) / float(d);
    float cap = air * lerp(dot(cells[CELLS].rgb, LUMA), dot(seen_there, LUMA), far);
    float3 c = scattered[at];
    float has = dot(c, LUMA);
    if (has > cap) scattered[at] = c * (cap / has);
}
#endif

#ifdef REFLECT
RWTexture2D<float3> reflected : register(u0);
// Where the cells are, from the depth given: `place` without its floor (u0
// is the reflections' image here, not the resolve's light).
[RootSignature(RS)]
[numthreads(8, 4, 1)]
void place_only(uint3 at : SV_DispatchThreadID) {
    float2 size = float2(width, height);
    float3 p = seen(int2((float2(at.xy) + 0.5) * size / float2(CX, CY)), size);
    cells[CELLS + 1 + at.y * CX + at.x] = float4(p, p.z * p.z);
}
// Each texel of the reflections' image (any size; the depth's is `width`
// by `height`): the light the points around send along the reflected
// direction, as `raise` takes it along the normal; `level` is the share,
// `constant` the share of the title's sky kept, made no brighter than that.
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void reflect_light(uint3 at : SV_DispatchThreadID) {
    uint w, h;
    reflected.GetDimensions(w, h);
    if (at.x >= w || at.y >= h) return;
    float2 size = float2(width, height);
    int2 p = int2((float2(at.xy) + 0.5) * size / float2(w, h));
    float3 c = seen(p, size);
    // Nothing is reflected by the sky.
    if (c.z >= 999.0) return;
    float3 l = seen(p + int2(-1, 0), size), r = seen(p + int2(1, 0), size);
    float3 u = seen(p + int2(0, -1), size), d = seen(p + int2(0, 1), size);
    float3 dx = abs(l.z - c.z) < abs(r.z - c.z) ? c - l : r - c;
    float3 dy = abs(u.z - c.z) < abs(d.z - c.z) ? c - u : d - c;
    float3 n = normalize(cross(dy, dx) + float3(0, 0, -1e-9));
    if (dot(n, c) > 0.0) n = -n;
    float3 v = normalize(c);
    float3 ray = v - 2.0 * dot(v, n) * n;
    float2 g = (float2(p) + 0.5) / size * float2(PX, PY) - 0.5;
    int2 g0 = int2(floor(g));
    float2 f = g - float2(g0);
    float4 sr = 0, sg = 0, sb = 0;
    float sum = 0;
    for (int k = 0; k < 4; k++) {
        int2 q = clamp(g0 + int2(k & 1, k >> 1), int2(0, 0), int2(PX, PY) - 1);
        float z = seen(int2((float2(q) + 0.5) * size / float2(PX, PY)), size).z;
        float2 b = float2(k & 1 ? f.x : 1 - f.x, k >> 1 ? f.y : 1 - f.y);
        float wgt = (b.x * b.y + 1e-3) / (0.02 + abs(z - c.z) / c.z);
        uint at0 = (q.y * PX + q.x) * 3;
        sr += wgt * points[at0];
        sg += wgt * points[at0 + 1];
        sb += wgt * points[at0 + 2];
        sum += wgt;
    }
    float3 all = float3(sr.w, sg.w, sb.w) / sum;
    // Radiance from the first moments: three times the direction's share
    // for an even spread of directions, a little less for a smooth image.
    float3 found = all + 2.0 * float3(dot(sr.xyz, ray), dot(sg.xyz, ray), dot(sb.xyz, ray)) / sum;
    found = clamp(found, 0.1 * all, 3.0 * all);
    found = lerp(found, cells[CELLS].rgb, 0.2) * level;
    found = any(isnan(found)) || any(isinf(found)) ? 0.0 : max(found, 0.0);
    float3 was = reflected[at.xy];
    was = any(isnan(was)) || any(isinf(was)) ? 0.0 : max(was, 0.0);
    float has = dot(was, LUMA);
    was *= min(1.0, dot(found, LUMA) / max(has, 1e-6));
    reflected[at.xy] = lerp(found, was, constant);
}
#endif

#ifdef RAISE
// (As `pack` in ambient.rs, which the tests run.)
uint pack(float3 c) {
    c = clamp(c, 0.0, 65408.0);
    float top = max(c.r, max(c.g, c.b));
    int e = clamp(int(floor(log2(max(top, 1e-9)))) + 16, 0, 31);
    float scale = exp2(float(24 - e));
    if (top * scale >= 511.5) { e = min(e + 1, 31); scale = exp2(float(24 - e)); }
    uint3 m = min(uint3(c * scale + 0.5), 511u);
    return m.r | (m.g << 9) | (m.b << 18) | (uint(e) << 27);
}
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void raise(uint3 at : SV_DispatchThreadID) {
    float2 size = float2(width, height);
    if (at.x >= width || at.y >= height) return;
    int2 p = int2(at.xy);
    float3 c = seen(p, size);
    float3 l = seen(p + int2(-1, 0), size), r = seen(p + int2(1, 0), size);
    float3 u = seen(p + int2(0, -1), size), d = seen(p + int2(0, 1), size);
    float3 dx = abs(l.z - c.z) < abs(r.z - c.z) ? c - l : r - c;
    float3 dy = abs(u.z - c.z) < abs(d.z - c.z) ? c - u : d - c;
    float3 n = normalize(cross(dy, dx) + float3(0, 0, -1e-9));
    if (dot(n, c) > 0.0) n = -n;
    // Crease shade openness (after upstream GPL-3.0 work).
    float open = 1.0;
    float px = min(radius * size.y / (2.0 * tan_y * c.z), 128.0);
    float noise = frac(52.9829189 * frac(dot(float2(p), float2(0.06711056, 0.00583715))));
    if (strength > 0.0 && px >= 1.5 && c.z < 400.0) {
        float sum = 0.0;
        const int N = 12;
        for (int i = 0; i < N; i++) {
            float a = i * 2.399963 + noise * 6.2831853;
            float2 o = float2(cos(a), sin(a)) * (sqrt((i + 0.5) / N) * px);
            float3 v = seen(p + int2(round(o)), size) - c;
            float vv = dot(v, v);
            float over = dot(v, n) - 0.004 * c.z - 0.005;
            sum += max(over, 0.0) / sqrt(vv + 1e-6) * saturate(1.0 - vv / (radius * radius));
        }
        open = saturate(1.0 - strength * 2.0 * sum / N);
    }
    float3 found;
    if (constant > 0.0) {
        found = float3(1.0, 0.98, 0.94) * constant * open;
        light[p] = pack(found);
        return;
    }
    // Between the four points around, those at its own depth first; which
    // four is dithered a little.
    float2 g = (float2(p) + 0.5) / size * float2(PX, PY) - 0.5 + (noise - 0.5) * 0.5;
    int2 g0 = int2(floor(g));
    float2 f = g - float2(g0);
    float4 sr = 0, sg = 0, sb = 0;
    float sum = 0;
    for (int k = 0; k < 4; k++) {
        int2 q = clamp(g0 + int2(k & 1, k >> 1), int2(0, 0), int2(PX, PY) - 1);
        float z = seen(int2((float2(q) + 0.5) * size / float2(PX, PY)), size).z;
        float2 b = float2(k & 1 ? f.x : 1 - f.x, k >> 1 ? f.y : 1 - f.y);
        float w = (b.x * b.y + 1e-3) / (0.02 + abs(z - c.z) / c.z);
        uint at0 = (q.y * PX + q.x) * 3;
        sr += w * points[at0];
        sg += w * points[at0 + 1];
        sb += w * points[at0 + 2];
        sum += w;
    }
    float3 all = float3(sr.w, sg.w, sb.w) / sum;
    found = all + 1.2 * float3(dot(sr.xyz, n), dot(sg.xyz, n), dot(sb.xyz, n)) / sum;
    found = clamp(found, 0.2 * all, 2.5 * all);
    found = lerp(found, cells[CELLS].rgb, 0.2) * level * open;
    float3 was = unpack(light[p]);
#ifdef CLEAN
    // Magenta and violet leftovers of other memory (`clean` in ambient.rs).
    if (was.g < 0.5 * min(was.r, was.b) && max(was.r, was.b) > 0.02) was = 0.0;
    if (was.b > 2.5 * was.g && was.b > was.r && was.b > 0.02) was = 0.0;
#endif
    // What the program left counts as far as it can be true: the sky's light
    // outdoors, not the blocks of other memory.
    float top = max(dot(found, LUMA) * most, 1e-6);
    float has = dot(was, LUMA);
    was *= min(1.0, top / max(has, 1e-6));
    light[p] = pack(max(was, found));
}
#endif
"#;

/// Bytes of the cells' buffer and of the points'.
const CELLS: u64 = 64 * 36;
const CELL_BYTES: u64 = (2 * CELLS + 1) * 16;
const POINT_BYTES: u64 = 160 * 90 * 48;

struct Pass {
    root: ID3D12RootSignature,
    keep: ID3D12PipelineState,
    mean: ID3D12PipelineState,
    place: ID3D12PipelineState,
    gather: ID3D12PipelineState,
    raise: ID3D12PipelineState,
    air: ID3D12PipelineState,
    place_only: ID3D12PipelineState,
    reflect: ID3D12PipelineState,
    cells: ID3D12Resource,
    points: ID3D12Resource,
}
// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Pass {}
// SAFETY: as above.
unsafe impl Sync for Pass {}

fn make(device: &ID3D12Device, buffer: impl Fn(u64) -> Option<ID3D12Resource>) -> windows::core::Result<Pass> {
    let mut root: Option<ID3D12RootSignature> = None;
    let mut pipeline = |part: &str, entry| -> windows::core::Result<ID3D12PipelineState> {
        let code = crate::host::compile(&format!("#define {part}\n{SOURCE}"), entry, s!("cs_5_1"))?;
        // SAFETY: the root signature is inside the shader; plain creations.
        unsafe {
            if root.is_none() {
                root = Some(device.CreateRootSignature(0, &code)?);
            }
            let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE { pShaderBytecode: code.as_ptr().cast(), BytecodeLength: code.len() },
                ..Default::default()
            };
            device.CreateComputePipelineState(&desc)
        }
    };
    let keep = pipeline("KEEP", s!("keep"))?;
    let mean = pipeline("KEEP", s!("mean"))?;
    let place = pipeline("PLACE", s!("place"))?;
    let gather = pipeline("GATHER", s!("gather"))?;
    let raise = pipeline(if clean() { "RAISE\n#define CLEAN" } else { "RAISE" }, s!("raise"))?;
    let air = pipeline("AIR", s!("clear_air"))?;
    let place_only = pipeline("REFLECT", s!("place_only"))?;
    let reflect = pipeline("REFLECT", s!("reflect_light"))?;
    let made = |bytes| buffer(bytes).ok_or_else(windows::core::Error::empty);
    Ok(Pass {
        root: root.ok_or_else(windows::core::Error::empty)?,
        keep,
        mean,
        place,
        gather,
        raise,
        air,
        place_only,
        reflect,
        cells: made(CELL_BYTES)?,
        points: made(POINT_BYTES)?,
    })
}

fn pass() -> Option<&'static Pass> {
    static PASS: OnceLock<Option<Pass>> = OnceLock::new();
    PASS.get_or_init(|| {
        let host = host()?;
        make(&host.device, |bytes| host.uav_buffer(bytes))
            .inspect_err(|e| tracing::warn!(error = %e, "no stand-in for the indirect light"))
            .ok()
    })
    .as_ref()
}

fn uav_barrier(list: &ID3D12GraphicsCommandList) {
    let barrier = D3D12_RESOURCE_BARRIER { Type: D3D12_RESOURCE_BARRIER_TYPE_UAV, ..Default::default() };
    // SAFETY: an open list; the barrier names no resource.
    unsafe { list.ResourceBarrier(&[barrier]) };
}

/// Whether the light of a frame has been kept: the estimate has something
/// to begin with.
static KEPT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What the passes read and write, as views in the heap the list has set.
pub(crate) struct Views {
    /// The image the title's program just wrote.
    pub(crate) image: D3D12_GPU_DESCRIPTOR_HANDLE,
    /// The scene's depth, for [`Role::Resolve`].
    pub(crate) depth: D3D12_GPU_DESCRIPTOR_HANDLE,
    /// The size of the image, and how many slices it has.
    pub(crate) size: [u32; 2],
    pub(crate) slices: u32,
    /// The size of the depth (the image's but for [`Role::Miss`]).
    pub(crate) depth_size: [u32; 2],
}

/// For the line every ten seconds: what the stand-in for reflections did.
struct Said {
    /// Dispatches of the miss program it followed, the frames they were in,
    /// and those it left as they were: of another layer, and before a
    /// frame's light was kept (or with other tables).
    ran: AtomicU64,
    frames: AtomicU64,
    other_layer: AtomicU64,
    not_ready: AtomicU64,
    last_frame: AtomicU64,
    /// The layers followed: a bit each for 0 to 30, bit 31 for the rest
    /// and for none.
    layers: AtomicU64,
}

static SAID_MISS: Said = Said {
    ran: AtomicU64::new(0),
    frames: AtomicU64::new(0),
    other_layer: AtomicU64::new(0),
    not_ready: AtomicU64::new(0),
    last_frame: AtomicU64::new(u64::MAX),
    layers: AtomicU64::new(0),
};

/// What came of a dispatch of the miss program, counted for the line.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum MissNote {
    Ran(Option<u32>),
    OtherLayer,
    NotReady,
}

/// Counts what came of a dispatch of the miss program; every ten seconds an
/// info line says it, so a run shows that the stand-in ran and where.
pub(crate) fn note_miss(note: MissNote) {
    use AtomicOrdering::Relaxed;
    if specular().is_none() {
        return;
    }
    let s = &SAID_MISS;
    match note {
        MissNote::Ran(layer) => {
            s.ran.fetch_add(1, Relaxed);
            s.layers.fetch_or(1 << layer.map_or(31, |l| l.min(31)), Relaxed);
            let frame = crate::frame::current();
            if s.last_frame.swap(frame, Relaxed) != frame {
                s.frames.fetch_add(1, Relaxed);
            }
        }
        MissNote::OtherLayer => {
            s.other_layer.fetch_add(1, Relaxed);
        }
        MissNote::NotReady => {
            s.not_ready.fetch_add(1, Relaxed);
        }
    }
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let now = std::time::Instant::now();
    let Some(since) = *last else {
        *last = Some(now);
        return;
    };
    if now.duration_since(since).as_secs_f32() < 10.0 {
        return;
    }
    *last = Some(now);
    let layers = s.layers.swap(0, Relaxed);
    let named: Vec<String> = (0..32)
        .filter(|bit| layers & (1 << bit) != 0)
        .map(|bit| if bit == 31 { "other".to_owned() } else { bit.to_string() })
        .collect();
    let Specular { share, sky } = specular().unwrap_or(Specular { share: 0.0, sky: 0.0 });
    tracing::info!(
        "a stand-in for reflections (VICTORY_AMBIENT_SPECULAR={share},{sky}) in the last {:.0} s: \
         {} frames, {} dispatches of the miss program followed (layers: {}), each by an estimate along \
         the reflected direction put in right after it; {} of other layers and {} before a frame's light \
         was kept (or with other tables) left as the title had them",
        now.duration_since(since).as_secs_f32(),
        s.frames.swap(0, Relaxed),
        s.ran.swap(0, Relaxed),
        if named.is_empty() { "none".to_owned() } else { named.join(", ") },
        s.other_layer.swap(0, Relaxed),
        s.not_ready.swap(0, Relaxed),
    );
}

/// After the title's program of that role, in `list` with the program's view
/// heap set. Replaces the list's compute root signature and pipeline; false
/// if it recorded nothing.
pub(crate) fn after(list: &HostList, role: Role, views: &Views) -> bool {
    use std::sync::atomic::Ordering;
    let Some(pass) = pass() else {
        return false;
    };
    // The share of the estimate, and the constant (for reflections: the
    // share of the title's sky kept); the keep runs for either stand-in.
    let (level, constant) = match (role, set(), specular()) {
        (Role::Miss, _, Some(s)) => (s.share, s.sky),
        (Role::Miss, _, None) => return false,
        (Role::Radiance, Set::Off, Some(_)) => (0.0, 0.0),
        (_, Set::Off, _) => return false,
        (_, Set::Estimate(level), _) => (level, 0.0f32),
        (_, Set::Constant(level), _) => (0.0, level),
    };
    let estimate = role == Role::Miss || constant == 0.0;
    if role != Role::Radiance && estimate && !KEPT.load(Ordering::Relaxed) {
        if role == Role::Miss {
            note_miss(MissNote::NotReady);
        }
        return false;
    }
    if role == Role::Air && (constant > 0.0 || air() == 0.0) {
        return false;
    }
    static SAID: Mutex<u32> = Mutex::new(0);
    {
        let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
        if *said < 5 {
            *said += 1;
            let floor = if constant > 0.0 || role == Role::Miss { 0.0 } else { floor() };
            tracing::info!(
                ?role,
                size = ?views.size,
                level,
                constant,
                floor,
                "a stand-in for the indirect light (floor: VICTORY_AMBIENT_FLOOR, 0 none)"
            );
        }
    }
    list.keep(&pass.cells);
    list.keep(&pass.points);
    let [strength, radius] = shade();
    let floats = [level, constant, strength, radius, 0.1, 0.45];
    let mut words = [0u32; 12];
    for (to, v) in words.iter_mut().zip(floats) {
        *to = v.to_bits();
    }
    words[6] = views.depth_size[0];
    words[7] = views.depth_size[1];
    words[8] = 0.04f32.to_bits();
    words[9] = 4.0f32.to_bits();
    words[10] = air().to_bits();
    // (No floor for reflections: their `place_only` reads no light.)
    words[11] = if role == Role::Miss { 0 } else { floor().to_bits() };
    // A pass of ours on the card (`host::gpu_split`).
    list.gpu_kind(crate::host::gpu_split::Kind::Passes);
    let l = &list.list;
    // SAFETY: an open list with the heap of the views set; the title's
    // program has just used both views, so their textures are in the states
    // these views need; buffers the list keeps.
    unsafe {
        uav_barrier(l);
        l.SetComputeRootSignature(&pass.root);
        l.SetComputeRoot32BitConstants(0, 12, words.as_ptr().cast(), 0);
        l.SetComputeRootDescriptorTable(1, views.depth);
        l.SetComputeRootDescriptorTable(2, views.image);
        l.SetComputeRootUnorderedAccessView(3, pass.cells.GetGPUVirtualAddress());
        l.SetComputeRootUnorderedAccessView(4, pass.points.GetGPUVirtualAddress());
        let run = |pso: &ID3D12PipelineState, x, y, z| {
            l.SetPipelineState(pso);
            l.Dispatch(x, y, z);
            uav_barrier(l);
        };
        match role {
            Role::Radiance => {
                run(&pass.keep, 8, 9, 1);
                run(&pass.mean, 1, 1, 1);
                KEPT.store(true, Ordering::Relaxed);
            }
            Role::Air => run(&pass.air, views.size[0].div_ceil(8), views.size[1].div_ceil(8), views.slices.div_ceil(2)),
            Role::Resolve => {
                if constant == 0.0 {
                    run(&pass.place, 8, 9, 1);
                    run(&pass.gather, 20, 45, 1);
                }
                run(&pass.raise, views.size[0].div_ceil(8), views.size[1].div_ceil(8), 1);
            }
            Role::Miss => {
                // The points afresh, from this program's depth: the diffuse
                // stand-in makes them later in the frame.
                run(&pass.place_only, 8, 9, 1);
                run(&pass.gather, 20, 45, 1);
                run(&pass.reflect, views.size[0].div_ceil(8), views.size[1].div_ceil(8), 1);
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings() {
        assert_eq!(parse(""), Set::Off);
        assert_eq!(parse("off"), Set::Off);
        assert_eq!(parse("0"), Set::Off);
        assert_eq!(parse("0.8"), Set::Estimate(0.8));
        // scripts\play.ps1's default: all of the estimate.
        assert_eq!(parse("1"), Set::Estimate(1.0));
        assert_eq!(parse(" 1.0 "), Set::Estimate(1.0));
        assert_eq!(parse("constant:0.3"), Set::Constant(0.3));
        assert_eq!(parse("constant:nan"), Set::Off);
    }

    #[test]
    fn specular_settings() {
        assert_eq!(parse_specular(""), None);
        assert_eq!(parse_specular("0"), None);
        assert_eq!(parse_specular("off"), None);
        assert_eq!(parse_specular("1"), Some(Specular { share: 1.0, sky: 0.0 }));
        assert_eq!(parse_specular("0.6, 0.25"), Some(Specular { share: 0.6, sky: 0.25 }));
        assert_eq!(parse_specular("0.6,7"), Some(Specular { share: 0.6, sky: 1.0 }));
        assert_eq!(parse_specular("0.6,x"), Some(Specular { share: 0.6, sky: 0.0 }));
    }

    /// The miss program is found by the profile's names alone: none named,
    /// none found; a variant named as not it is not it.
    #[test]
    fn the_miss_program_by_the_profiles_names() {
        let mut profile = AmbientProfile::default();
        assert_eq!(role_with(&profile, b"..Miss_0123abcd_Wrapped.."), None);
        profile.miss_substring = "Miss_".into();
        profile.miss_not_substrings = vec!["Miss_Fog".into()];
        assert_eq!(role_with(&profile, b"..Miss_0123abcd_Wrapped.."), Some(Role::Miss));
        assert_eq!(role_with(&profile, b"..Miss_Fog_0123abcd_Wrapped.."), None);
        assert_eq!(role_with(&profile, b"..Resolve.."), None);
    }

    /// Which dispatches of the miss program are followed, by their first
    /// root constant: all when the profile names no layer; else its layers,
    /// and its alpha layers with `VICTORY_AMBIENT_SPECULAR_ALPHA=1` only.
    #[test]
    fn miss_layers_followed() {
        for layer in [None, Some(0), Some(7)] {
            assert!(layer_followed(&[], &[], false, layer));
        }
        assert!(layer_followed(&[0], &[1], false, Some(0)));
        assert!(!layer_followed(&[0], &[1], false, Some(1)));
        assert!(layer_followed(&[0], &[1], true, Some(1)));
        assert!(!layer_followed(&[0], &[1], true, Some(2)));
        assert!(!layer_followed(&[0], &[1], true, None));
    }

    #[test]
    fn the_floor_is_read_as_a_level() {
        assert_eq!(parse_floor(None), FLOOR);
        assert_eq!(parse_floor(Some("")), FLOOR);
        assert_eq!(parse_floor(Some("0")), 0.0);
        assert_eq!(parse_floor(Some(" off ")), 0.0);
        assert_eq!(parse_floor(Some("0.2")), 0.2);
        assert_eq!(parse_floor(Some(" 0.01 ")), 0.01);
        for set in ["-1", "nan", "inf", "bright"] {
            assert_eq!(parse_floor(Some(set)), FLOOR, "{set:?}");
        }
    }

    /// The shader's `unpack`.
    fn unpack(v: u32) -> [f32; 3] {
        let scale = 2f32.powi((v >> 27) as i32 - 24);
        [v & 511, (v >> 9) & 511, (v >> 18) & 511].map(|m| m as f32 * scale)
    }

    /// The shader's `pack` (its `clamp` takes a NaN for 0, as `max` does).
    fn pack(c: [f32; 3]) -> u32 {
        let c = c.map(|v| v.max(0.0).min(65408.0));
        let top = c[0].max(c[1]).max(c[2]);
        let mut e = (top.max(1e-9).log2().floor() as i32 + 16).clamp(0, 31);
        let mut scale = 2f32.powi(24 - e);
        if top * scale >= 511.5 {
            e = (e + 1).min(31);
            scale = 2f32.powi(24 - e);
        }
        let m = c.map(|v| ((v * scale + 0.5) as u32).min(511));
        m[0] | (m[1] << 9) | (m[2] << 18) | ((e as u32) << 27)
    }

    #[test]
    fn the_light_packs_and_unpacks_as_the_resolve_writes_it() {
        // What the format holds comes back bit for bit.
        for v in [
            0,
            (1 << 27) | 256,
            (15 << 27) | 300 | (400 << 9) | (511 << 18),
            (20 << 27) | (256 << 18),
            (31 << 27) | 511 | (511 << 9) | (511 << 18),
        ] {
            assert_eq!(pack(unpack(v)), v, "{v:#x}");
        }
        // A colour comes back to within half a step of its brightest
        // channel's 9 bits.
        let colours = [[0.05, 0.04, 0.03], [1.0, 0.5, 0.25], [12.5, 30.0, 0.001], [0.0; 3], [65408.0, 1.0, 0.0]];
        for c in colours.into_iter().chain([[1e-5, 2e-5, 3e-5]]) {
            let back = unpack(pack(c));
            let top = c[0].max(c[1]).max(c[2]);
            for k in 0..3 {
                assert!((back[k] - c[k]).abs() <= top / 500.0 + 3e-8, "{c:?} came back as {back:?}");
            }
        }
        // Past the format's top, its top; below 0 or not a number, 0.
        assert_eq!(unpack(pack([1e9, -1.0, f32::NAN])), [65408.0, 0.0, 0.0]);
    }

    /// The shader's `floored`.
    fn floored(l: [f32; 3], sky: [f32; 3], lowest: f32) -> [f32; 3] {
        let has = luma(l);
        if has >= lowest {
            return l;
        }
        let s = luma(sky);
        let c = sky.map(|v| v / s.max(1e-9));
        let top = c[0].max(c[1]).max(c[2]);
        let pale = if top > 4.0 { 3.0 / (top - 1.0) } else { 1.0 };
        let t = pale * (s / (0.1 * lowest)).clamp(0.0, 1.0);
        std::array::from_fn(|k| l[k] + (lowest - has) * (1.0 + (c[k] - 1.0) * t))
    }

    fn luma(c: [f32; 3]) -> f32 {
        0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
    }

    #[test]
    fn the_floor_is_applied_only_below_it() {
        // A moonlit sky's light, bluish.
        let sky = [0.02, 0.03, 0.06];
        // At or above the floor, or with no floor: as it was.
        for l in [[0.06, 0.06, 0.06], [1.0, 0.2, 0.1], [0.0, 0.0, 0.7]] {
            assert_eq!(floored(l, sky, 0.05), l);
        }
        for l in [[0.0; 3], [0.001, 0.002, 0.0]] {
            assert_eq!(floored(l, sky, 0.0), l);
        }
        // Below it: brought up to it, nothing taken away.
        for l in [[0.0; 3], [0.01, 0.002, 0.004], [0.0, 0.0, 0.3]] {
            let up = floored(l, sky, 0.05);
            assert!((luma(up) - 0.05).abs() < 1e-6, "{l:?} rose to {up:?}");
            assert!((0..3).all(|k| up[k] >= l[k]), "{l:?} rose to {up:?}");
        }
        // In the colour of what the resolve left: the sky's, white where it
        // left nothing, and a deep blue made paler.
        let up = floored([0.0; 3], sky, 0.05);
        for k in 0..3 {
            assert!((up[k] - sky[k] * 0.05 / luma(sky)).abs() < 1e-6, "{up:?}");
        }
        let white = floored([0.0; 3], [0.0; 3], 0.05);
        assert!(white.iter().all(|v| (v - 0.05).abs() < 1e-6), "{white:?}");
        let blue = floored([0.0; 3], [0.0, 0.0, 1.0], 0.05);
        assert!(blue[2] <= 4.0 * 0.05 + 1e-6 && blue[0] > 0.0 && blue[0] == blue[1], "{blue:?}");
        assert!((luma(blue) - 0.05).abs() < 1e-6, "{blue:?}");
    }

    #[test]
    fn roles_by_name() {
        let profile = AmbientProfile {
            resolve_substring: "CS_Test_Resolve".into(),
            radiance_substring: "CS_Test_Radiance".into(),
            air_substring: "CS_Test_Air".into(),
            ..Default::default()
        };
        assert_eq!(role_with(&profile, b"..CS_Test_Resolve_a1b2.."), Some(Role::Resolve));
        assert_eq!(role_with(&profile, b"..CS_Test_Radiance_c3d4.."), Some(Role::Radiance));
        assert_eq!(role_with(&profile, b"CS_Test_Air_e5f6"), Some(Role::Air));
        assert_eq!(role_with(&profile, b"CS_Unrelated"), None);
        // Empty profile matches nothing by default.
        assert_eq!(role_with(&AmbientProfile::default(), b"CS_Test_Resolve"), None);
    }

    /// The passes compile with the system's compiler and the host takes them.
    #[test]
    fn the_passes_build() {
        let Some(host) = host() else {
            return;
        };
        make(&host.device, |bytes| host.uav_buffer(bytes)).expect("the passes");
        // The raise with and without the filter of leftovers (`VICTORY_AMBIENT_CLEAN`).
        for part in ["RAISE", "RAISE\n#define CLEAN"] {
            crate::host::compile(&format!("#define {part}\n{SOURCE}"), s!("raise"), s!("cs_5_1")).expect(part);
        }
    }
}
