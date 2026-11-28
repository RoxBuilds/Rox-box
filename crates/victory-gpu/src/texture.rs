//! Texture contents the title writes itself:
//! streamed pages, layout-library copies, movie frames, glyph
//! atlases. Such a texture lives in console memory in its tiled layout;
//! before a submission samples its host copy, the memory is detiled into a
//! staging buffer when its content changed, and the staging buffer is
//! copied into the host texture.
//!
//! "Changed" means a page of the memory was written since the last detiling
//! ([`crate::watch`]), in write-watched memory; in physical-page views,
//! which cannot be watched, a content hash of the whole memory at every
//! submission that samples it.
//!
//! shortcut: a change detiles the whole texture, not the pages written.
//! Only textures read through descriptor tables are brought up to date
//! (directly indexed views are not known per draw); plane 0 is uploaded
//! (every mip and array slice), depth and stencil planes are not.

pub(crate) mod ahead;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::*;

use crate::host::{HostList, host};
use crate::object::This;

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

/// A texture's upload state: the staging buffer laid out as the host
/// texture's subresources, and what was last detiled.
pub(crate) struct Staging {
    /// A buffer of its own, or the block [`Staging::piece`] is part of.
    buffer: ID3D12Resource,
    /// Where in `buffer` it begins (the footprints count from there).
    base: u64,
    /// Back in the pool with the last list that names it.
    _piece: Option<crate::pool::StagingPiece>,
    /// Its bytes.
    #[cfg(feature = "diagnostics")]
    size: u64,
    data: *mut u8,
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    layout: victory_formats::texture::Layout,
    /// Mips per array slice, of the title's texture.
    mips: u32,
    /// The first of them that is read and uploaded
    /// (`crate::resource::held_only`), the one the host copy begins at, and
    /// the bytes of title memory read.
    first: u32,
    level0: u32,
    read: usize,
    /// Which host copy it uploads into, and what that is told to hold.
    serial: u64,
    holds: Arc<crate::resource::Holds>,
    /// Made by the pass over textures picked by index, for one upload
    /// ([`once`]): read when the list is submitted, and never looked at again.
    once: std::sync::atomic::AtomicBool,
    /// The staging buffer's bytes (counted in `STAGED` while it lives).
    bytes: u64,
    memory: *const u8,
    last: Mutex<Last>,
    /// Whether the staging buffer was detiled since a submitted list last
    /// uploaded it, and whether one has uploaded it at all: a list records
    /// the upload only then. (Every list that sampled a title texture
    /// uploaded all of it again, changed or not: thousands of subresource
    /// copies a frame.)
    dirty: std::sync::atomic::AtomicBool,
    uploaded: std::sync::atomic::AtomicBool,
    /// Whether the last refresh detiled the texture (set by the detiling,
    /// taken by [`Staging::refresh`]), and for how many refreshes in a row
    /// one did after the host copy was uploaded (saturating): while that is
    /// more than 0 every use uploads again ([`streak_on`]).
    changed: std::sync::atomic::AtomicBool,
    streak: std::sync::atomic::AtomicU8,
    /// When a list last recorded a use of it (`crate::host::moment`).
    used: std::sync::atomic::AtomicU64,
    /// How many bytes of the texture's memory were mapped when a list last
    /// recorded an upload of it ([`fill_unlocked`]; 0: not asked).
    expected: std::sync::atomic::AtomicU64,
    /// Levels on demand (`crate::tiles`): the texture's host copy was made
    /// without memory of its own when the staging buffer was made.
    on_demand: bool,
    /// For such a texture: the levels that were the title's at the last
    /// detiling (a bit a level; the staging buffer holds zeros for the
    /// others), and what that was judged by ([`Staging::judgement`]).
    detiled: std::sync::atomic::AtomicU32,
    judged: [std::sync::atomic::AtomicU64; 2],
    /// For such a texture: when an upload last left a held level without
    /// tiles (`crate::host::moment`, never 0; 0: none did). The upload is
    /// recorded again about a second later if the card has room by then
    /// ([`upload`]).
    waiting: std::sync::atomic::AtomicU64,
    /// Recorded for a one-shot decoration bake (`BAKING`): the refresh at
    /// submission reads the memory as it is then, and waits for the runtime
    /// to stop remapping it, instead of passing the texture over as
    /// unwritten, taking the read that may fail while pages are remapped
    /// ([`no_wait`]) or waiting out the 30 frames between reads of a partly
    /// mapped one ([`refreshes`]). Taken by the refresh.
    // After upstream GPL-3.0 fork, commit f4b48ad766dd "Publish the pareto-residency
    // Windows build and portable launcher" (texture.rs `exact`): the flag. Here: set only while `BAKING` is, which
    // `VICTORY_BAKE_EXACT=0` leaves unset (as before).
    exact: std::sync::atomic::AtomicBool,
    /// Whether the texture is a movie plane that is uploaded from a buffer
    /// of the list's own at every use ([`movie_planes`]): set once its shape
    /// matches and the title rewrote it after an upload ([`uses_plane`]),
    /// and kept.
    planed: std::sync::atomic::AtomicBool,
}

/// Whether a texture that was detiled at its last use or uses is uploaded at
/// every use (`VICTORY_TEXTURE_STREAK`; on unless `0`, which skips an upload
/// whenever `dirty` is clear, as before). The note that an upload took what
/// the staging buffer held ran after the detiling of the same submission
/// when `VICTORY_JOIN_PARTS` detiles ahead of the steps, and cleared
/// `dirty`: a texture rewritten before every use, a video's ring of three,
/// had its host copy updated on every second use only ("texture uploads: 2
/// recorded, 3 left out" a frame while all the bytes hashed had changed).
/// Costs one upload more than
/// needed when such a texture stops changing.
fn streak_on() -> bool {
    crate::toggled!("VICTORY_TEXTURE_STREAK")
}

/// The refreshes in a row that found a texture detiled, after its host copy
/// was uploaded: one more if this one did, none if it did not.
fn next_streak(streak: u8, changed: bool, uploaded: bool) -> u8 {
    match (changed, uploaded) {
        (true, true) => streak.saturating_add(1),
        (true, false) => streak,
        (false, _) => 0,
    }
}

/// Whether a list leaves out the upload: the host copy has what the staging
/// buffer holds, and the texture did not change at its last use.
fn skips_upload(uploaded: bool, dirty: bool, streak: u8) -> bool {
    uploaded && !dirty && streak == 0
}

/// Whether a refresh looks at the texture's memory at all: it was made for a
/// bake (`exact`), or it is not one made for a single upload (`alone`) that
/// was filled already (`hashed`), and the write guard saw a write since the
/// last look or it is the texture's turn to be hashed or it is hot
/// (`unwritten`, `turn`, `hot`). The watched path passes `turn` and `hot` as
/// false: it asks the write-watch alone.
fn refreshes(exact: bool, alone: bool, hashed: bool, unwritten: bool, turn: bool, hot: bool) -> bool {
    exact || if alone { !hashed } else { !unwritten || turn || hot }
}

/// Which textures are uploaded from a buffer of their list's own, filled
/// from title memory at the list's place in the submission
/// (`VICTORY_MOVIE_PLANES`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Planes {
    /// None (`=0`): every texture goes through its staging buffer.
    Off,
    /// A texture of the shape a decoder's plane has that was rewritten by
    /// the title after an upload (the default).
    Latch,
    /// Every texture of that shape, rewritten or not (`=all`, as upstream GPL-3.0 fork).
    All,
}

/// The mode a value of `VICTORY_MOVIE_PLANES` names; anything else is the
/// default.
fn planes_mode(value: Option<&str>) -> Planes {
    match value {
        Some("0") => Planes::Off,
        Some("all") => Planes::All,
        _ => Planes::Latch,
    }
}

/// `VICTORY_MOVIE_PLANES`, read once. Every list copies a texture from the
/// one staging buffer the next submission's refresh overwrites, with up to
/// 18 submissions in the air, and a texture is refreshed once per submission
/// even when two lists sample it across a title write. A movie's plane, a
/// linear single-level 8-bit texture the decoder writes into a ring of
/// buffers, is uploaded from a buffer of the list's own instead, filled from
/// title memory at the list's steps. By default only once the title is seen
/// to rewrite the texture after an upload ([`uses_plane`]: a static 8-bit
/// texture of that shape never is, and would be uploaded again at every
/// use); `=all` takes every one of that shape, as upstream GPL-3.0 fork does; `=0`
/// never, as before. `VICTORY_TEXTURE_STREAK=0` leaves the default without
/// the count that sets the latch.
fn movie_planes() -> Planes {
    static MODE: std::sync::OnceLock<Planes> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| planes_mode(std::env::var("VICTORY_MOVIE_PLANES").ok().as_deref()))
}

/// The refreshes in a row that found a texture rewritten after its upload
/// ([`next_streak`]) from which it is a movie plane. A texture's first
/// refresh, run after its first upload was noted, counts one by itself, so
/// one is no sign of a rewrite; a texture rewritten before every use counts
/// two at its second use.
const PLANE_STREAK: u8 = 2;

/// Whether a use of a texture is uploaded from a buffer of the list's own:
/// by `mode`, its `shape` ([`plane_shape`]), its `streak` and whether it was
/// taken for one before (`latched`).
fn uses_plane(mode: Planes, shape: bool, streak: u8, latched: bool) -> bool {
    shape
        && match mode {
            Planes::Off => false,
            Planes::All => true,
            Planes::Latch => latched || streak >= PLANE_STREAK,
        }
}

/// Whether a texture has the shape of a decoder's plane: linear, one level
/// of one slice of one sample, 8 bits a texel, a single subresource at the
/// start of its staging buffer, and not on demand.
fn plane_shape(
    desc: &victory_formats::texture::Desc,
    footprints: &[D3D12_PLACED_SUBRESOURCE_FOOTPRINT],
    on_demand: bool,
) -> bool {
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8_UNORM;
    !on_demand
        && desc.dimension == victory_formats::texture::Dimension::Tex2d
        && desc.swizzle.0 == 0
        && desc.depth == 1
        && desc.mips == 1
        && desc.samples == 1
        && footprints.len() == 1
        && footprints[0].Offset == 0
        && footprints[0].Footprint.Format == DXGI_FORMAT_R8_UNORM
}

/// Whether the first upload into a texture is preceded by an aliasing
/// barrier: the texture lies in a block (or `VICTORY_ALIAS_ALWAYS=1` says
/// so) or its host copy is reserved, and it was not uploaded before.
fn needs_alias(in_block: bool, on_demand: bool, uploaded: bool) -> bool {
    !uploaded && (in_block || on_demand)
}

/// `VICTORY_ALIAS_ALWAYS=1`: the aliasing barrier before the first upload
/// goes to every texture while the pool is on, committed ones too, which
/// fails the list's `Close`.
fn alias_always() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_ALIAS_ALWAYS"))
}

/// Uploads recorded only because the texture changed at its last use
/// ([`streak_on`]) since the last line, and when that was
/// (`crate::host::now_ms`, the frame).
static STREAK_UPLOADS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static STREAK_LINE: Mutex<(u64, u64)> = Mutex::new((0, 0));

/// Counts one, and says how many a frame there were about once a second.
fn streak_upload() {
    STREAK_UPLOADS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (now, frame) = (crate::host::now_ms(), crate::frame::current());
    let mut line = STREAK_LINE.lock().unwrap_or_else(|e| e.into_inner());
    if line.0 == 0 {
        *line = (now.max(1), frame);
    } else if now >= line.0 + 1000 {
        let frames = frame.saturating_sub(line.1).max(1);
        let n = STREAK_UPLOADS.swap(0, std::sync::atomic::Ordering::Relaxed);
        *line = (now, frame);
        tracing::info!(
            "uploads recorded for textures that changed at their last use (VICTORY_TEXTURE_STREAK): {:.1} per frame",
            n as f64 / frames as f64
        );
    }
}

/// What a texture's staging buffer holds: the hash of the memory detiled
/// (memory not watched), or the write-watch asking it was detiled after.
#[derive(Default)]
struct Last {
    hash: Option<Digest>,
    asked: u64,
    /// The frame a partly mapped texture was last read in.
    partial: Option<u64>,
    /// Until this frame a guarded texture is hashed at every check: its
    /// first checks, and after the hash found a change the guard had not
    /// seen ([`hot_textures`]). 0: not checked under the guard yet.
    hot_until: u64,
}

/// Frames a guarded texture is hashed at every check from its first check,
/// and after the hash found a change the guard did not see.
const HOT_FRAMES: u64 = 64;

/// Whether a guarded texture whose memory the hash found changed although
/// the guard saw no write is hashed at every check for a while
/// ([`HOT_FRAMES`]), not every eighth frame. A title that writes such a
/// texture through another view of its pages (the guard sees none of it)
/// every frame, a video's frames in a ring of three, showed each one up
/// to eight frames late and in turns: videos flickered between old and new
/// pictures (the presented picture cycled through three, a new one every
/// eight frames). A texture also starts hot, for the ones written that way
/// from the start (which the hash never caught changing).
/// `VICTORY_TEXTURE_HOT=0`: every eighth frame
/// only, as before.
fn hot_textures() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_HOT").is_none_or(|v| v != "0"))
}

// SAFETY: the staging mapping is written only at submission, under
// `last`'s lock; the title memory is only read.
unsafe impl Send for Staging {}
// SAFETY: as above.
unsafe impl Sync for Staging {}

static STAGING: Mutex<Option<HashMap<usize, Arc<Staging>>>> = Mutex::new(None);

/// How often [`STAGING`] changed: a staging buffer came in, was taken out,
/// or was looked at for taking out. It moves only with the table's lock
/// held ([`staging_moved`]).
static STAGING_CHANGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// [`STAGING`] changes: what threads remember of it ([`STAGING_FOUND`]) is
/// of before. Called with the table's lock held, before a staging buffer is
/// judged idle or taken out, and when one is put in. (The fence: see
/// `crate::shadow`'s `loose_moved`, which this is after.)
fn staging_moved() {
    STAGING_CHANGED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
}

/// Answers a thread remembers of [`STAGING`].
const STAGING_FOUND_MOST: usize = 1024;

thread_local! {
    /// The staging states this thread was last given, by a hash of the
    /// resource: (resource, [`STAGING_CHANGED`] then, the state). Every list
    /// that samples a title texture asks for its staging state once, and
    /// all recording threads stood in line for the table's one lock (11 ms
    /// of their time a frame).
    static STAGING_FOUND: std::cell::RefCell<Vec<(usize, u64, std::sync::Weak<Staging>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn staging_found_at(resource: usize) -> usize {
    ((resource as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 54) as usize % STAGING_FOUND_MOST
}

/// The staging state this thread remembers for `resource`, if the table
/// did not change since. It is held first and the count read again after:
/// the sweep moves the count before it looks whether a list holds a staging
/// buffer ([`let_go_idle`]), so either this thread sees the count moved and
/// asks the table, or the sweep sees the buffer held and leaves it.
fn staging_found(resource: usize) -> Option<Arc<Staging>> {
    use std::sync::atomic::Ordering::SeqCst;
    let changed = STAGING_CHANGED.load(SeqCst);
    let found = STAGING_FOUND.with(|f| {
        let f = f.borrow();
        let (r, at, found) = f.get(staging_found_at(resource))?;
        ((*r, *at) == (resource, changed)).then(|| found.upgrade()).flatten()
    })?;
    std::sync::atomic::fence(SeqCst);
    (STAGING_CHANGED.load(SeqCst) == changed).then_some(found)
}

/// Remembers the table's answer for `resource`, given while `changed` was
/// the count (read under the table's lock).
fn staging_remember(resource: usize, changed: u64, found: &Arc<Staging>) {
    STAGING_FOUND.with(|f| {
        let mut f = f.borrow_mut();
        if f.is_empty() {
            f.resize_with(STAGING_FOUND_MOST, || (0, 0, std::sync::Weak::new()));
        }
        f[staging_found_at(resource)] = (resource, changed, Arc::downgrade(found));
    });
}

/// The buffer a texture the host writes is copied through into the texture
/// of a view the title placed in its memory in another format
/// ([`alias`]): every level of every layer as each of the two formats lays
/// it out for a copy (the same bytes: the texels are as wide), the written
/// texture's layout first.
struct Through {
    buffer: ID3D12Resource,
    parts: Vec<(
        D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
        D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    )>,
}

/// The most levels times layers [`alias`] copies (a cube of nine levels).
const ALIAS_PARTS: u32 = 54;

// SAFETY: a device-local buffer only lists record copies with.
unsafe impl Send for Through {}
// SAFETY: as above.
unsafe impl Sync for Through {}

/// The copy buffers of [`alias`], by the placed view's texture.
static THROUGH: Mutex<Option<HashMap<usize, Arc<Through>>>> = Mutex::new(None);

/// Whether [`alias`] copies into a view whenever its list made no copy into
/// it from that source yet (`VICTORY_ALIAS_EACH_SOURCE`, on unless `0`, read
/// once; `0`: at the view's first use in the list only, as before).
fn alias_each_source() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_ALIAS_EACH_SOURCE").is_none_or(|v| v != "0"))
}

/// Brings `placed`, the texture of a view the title placed in the memory
/// of `source` in another format (`crate::resource::alias_of`), up to date
/// with what the host wrote to `source`: the texels of every level and
/// layer are copied as they are, through a buffer (the host copies between
/// two formats of one texel width in no other way). Recorded the first time
/// the list reads the view from `source` (`VICTORY_ALIAS_EACH_SOURCE`)
/// and again whenever the list used `source` in another way since (wrote
/// it, or read it itself). Leaves `placed` in the copy destination state,
/// as [`upload`] does. False if the copy cannot be made: the caller fills
/// the texture from title memory, as before.
pub(crate) fn alias(list: &HostList, placed: This, source: This) -> bool {
    const TARGET_OR_UNORDERED: u32 = 0x4 | 0x8;
    const COPY_DEST: u32 = 0x400;
    const COPY_SOURCE: u32 = 0x800;
    // (All of its levels: the copy is level by level.)
    crate::resource::want_level(placed, 0);
    let (Some(to), Some(from)) = (
        crate::resource::host_resource(placed),
        crate::resource::host_resource(source),
    ) else {
        return false;
    };
    let Some(host) = host() else { return false };
    let through = {
        let mut map = THROUGH.lock().unwrap_or_else(|e| e.into_inner());
        let map = map.get_or_insert_with(HashMap::new);
        match map.get(&(placed as usize)) {
            Some(through) => through.clone(),
            None => {
                // SAFETY: live textures.
                let (of_from, of_to) = unsafe { (from.GetDesc(), to.GetDesc()) };
                // Levels times layers (a volume's slices are one part).
                let count = |d: &D3D12_RESOURCE_DESC| {
                    let layers = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
                        1
                    } else {
                        u32::from(d.DepthOrArraySize)
                    };
                    u32::from(d.MipLevels) * layers
                };
                let parts = count(&of_from);
                if parts == 0 || parts > ALIAS_PARTS || parts != count(&of_to) {
                    return false;
                }
                let mut rows = [(); 2]
                    .map(|()| vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); parts as usize]);
                let mut bytes = [0u64; 2];
                // SAFETY: plain queries; the out pointers are ours and
                // hold `parts` layouts each.
                unsafe {
                    host.device.GetCopyableFootprints(
                        &of_from,
                        0,
                        parts,
                        0,
                        Some(rows[0].as_mut_ptr()),
                        None,
                        None,
                        Some(&mut bytes[0]),
                    );
                    host.device.GetCopyableFootprints(
                        &of_to,
                        0,
                        parts,
                        0,
                        Some(rows[1].as_mut_ptr()),
                        None,
                        None,
                        Some(&mut bytes[1]),
                    );
                }
                let alike = rows[0].iter().zip(&rows[1]).all(|(a, b)| {
                    let (fa, fb) = (a.Footprint, b.Footprint);
                    a.Offset == b.Offset
                        && (fa.Width, fa.Height, fa.Depth, fa.RowPitch)
                            == (fb.Width, fb.Height, fb.Depth, fb.RowPitch)
                });
                if bytes[0] == 0 || bytes[0] != bytes[1] || !alike {
                    return false;
                }
                let Some(buffer) = host.uav_buffer(bytes[0]) else {
                    return false;
                };
                crate::host::name(&buffer, || {
                    format!(
                        "alias copy {}x{} f{} to f{}",
                        of_from.Width, of_from.Height, of_from.Format.0, of_to.Format.0
                    )
                });
                tracing::info!(
                    view = ?placed,
                    texture = ?source,
                    width = of_from.Width,
                    height = of_from.Height,
                    parts,
                    written_as = of_from.Format.0,
                    read_as = of_to.Format.0,
                    "a view placed in the memory of a texture the host writes, in another format: read from that texture by a copy on the host"
                );
                let [of_written, of_read] = rows;
                let through = Arc::new(Through {
                    buffer,
                    parts: of_written.into_iter().zip(of_read).collect(),
                });
                map.insert(placed as usize, through.clone());
                through
            }
        }
    };
    let first = list.first_use(placed as usize);
    // Owed if the list made no copy into the view from this source yet, not
    // only at the view's first use in the list: an upload from title memory
    // earlier in it (a copy of the view's texture, say) was that use, and,
    // with no change of the source's state since, the copy was left out and
    // the shader read the uploaded bytes. After fxceless #1241 (19a45d1; its
    // author read zeros before and the packed light after, on a 32-lane GPU);
    // `VICTORY_ALIAS_EACH_SOURCE=0`: at the first use only, as before.
    let owed = if alias_each_source() { list.alias_source_changed(to, from) } else { first };
    // The source's first use in the list records nothing (its prelude
    // brings it to the state); a use in another way since the last copy
    // shows as a change of state.
    let before = list.transition(source, COPY_SOURCE).map(|(texture, before)| {
        let state = crate::host::legal_state(texture, COPY_SOURCE);
        let barrier = crate::host::transition(
            texture,
            D3D12_RESOURCE_STATES(before as i32),
            D3D12_RESOURCE_STATES(state as i32),
        );
        // SAFETY: an open list and a live texture.
        unsafe { list.list.ResourceBarrier(&[barrier]) };
        before
    });
    if !owed && before.is_none() {
        return true;
    }
    card_writes(list, placed, to, None);
    crate::list::ensure_state(list, placed, COPY_DEST);
    let part = |texture: &ID3D12Resource, index: usize| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: index as u32,
        },
    };
    let rows = |row: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(&through.buffer) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: *row,
        },
    };
    let (common, read, written) = (
        D3D12_RESOURCE_STATE_COMMON,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_COPY_DEST,
    );
    #[cfg(feature = "diagnostics")]
    list.note_op(|| format!("alias copy of {source:?} into {placed:?}"));
    // SAFETY: an open list and live resources; the buffer is as big as
    // the footprints and rests in the common state between copies.
    unsafe {
        list.list
            .ResourceBarrier(&[crate::host::transition(&through.buffer, common, written)]);
        for (index, (of_written, _)) in through.parts.iter().enumerate() {
            list.list
                .CopyTextureRegion(&rows(of_written), 0, 0, 0, &part(from, index), None);
        }
        list.list
            .ResourceBarrier(&[crate::host::transition(&through.buffer, written, read)]);
        for (index, (_, of_read)) in through.parts.iter().enumerate() {
            list.list
                .CopyTextureRegion(&part(to, index), 0, 0, 0, &rows(of_read), None);
        }
        list.list
            .ResourceBarrier(&[crate::host::transition(&through.buffer, read, common)]);
    }
    list.keep(&through.buffer);
    // Its texels are the card's from now on: no stand-in hides them
    // (`crate::resource::Contents::CardWrote`).
    crate::resource::note_card_wrote(placed);
    // A draw that also writes the source (a view of it handled before
    // this one) needs it in that state again; the next read of the alias
    // then copies once more, as it must after a write.
    if let Some(before) = before
        && before & TARGET_OR_UNORDERED != 0
    {
        crate::list::ensure_state(list, source, before);
    }
    true
}

/// Whether a texture refresh that finds the runtime remapping title memory
/// goes on without waiting for it (`VICTORY_TEXTURE_NO_WAIT=1`; off by
/// default).
///
/// A texture whose memory is all mapped is read in place, under a lock
/// the runtime takes alone while it changes a mapping
/// (`crate::watch::title_memory_reading`), so that no page goes away under
/// the read. While a title streams the runtime remaps all the time, and
/// each of a frame's hundreds of refreshes waited for it: frame lines with
/// hundreds to thousands of milliseconds of "textures", and on every one of
/// them the wait for this lock is nearly the whole of it (the write-watch
/// askings a few milliseconds, the texture's own lock none).
///
/// With the switch the refresh asks for the lock without waiting. If the
/// runtime has it, the texture is read through the kernel instead, which
/// needs no lock: it fails if a page is not there, and then the texture is
/// left as it is and read at its next refresh, a frame later. A read
/// through the kernel costs about 0.6 ms a megabyte against 0.3 in place.
///
/// What changes for the picture: a texture that arrives while the runtime
/// is remapping may show a frame later. A staging buffer that holds nothing
/// yet is filled with what is there all the same ([`fill_unlocked`]): a list
/// has recorded the upload from it by then.
fn no_wait() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_NO_WAIT").is_some_and(|v| v == "1"))
}

/// Whether a refresh that does not wait ([`no_wait`]) and finds a page
/// missing fills a staging buffer that was never filled from the pages that
/// are there (`VICTORY_TEXTURE_FILL=0`: it was left as it was made).
///
/// Left for the next refresh, it was uploaded as it was: the upload is
/// recorded before the refresh, and a texture picked by index has no next
/// refresh (its staging buffer goes with the upload). A texture the title
/// holds only the smaller levels of is never all there, so whenever the
/// runtime was remapping at that moment, its levels became zeros on the
/// card, good ones too: of the textures a recorded frame's slots show, a
/// seventh had zeros in the first level shown, and a sixth of that frame's
/// repeated uploads put zeros over a filled level.
fn fill_unlocked() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_FILL").is_none_or(|v| v != "0"))
}

/// Since the last frame-rate line, with [`no_wait`]: refreshes that read
/// through the kernel because title memory was being remapped, and those
/// left for the next refresh because a page was not there; staging buffers
/// filled from the pages there ([`fill_unlocked`]), and of them those that
/// got less than was mapped (their slots are looked at again).
pub(crate) static UNLOCKED: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// Since the last frame-rate line, of texture refreshes on every thread, in
/// microseconds: waiting for the texture's own lock (another thread is
/// refreshing the same texture), asking the write watch and the guards,
/// waiting for title memory to hold still (the lock the runtime takes
/// alone while it remaps, `crate::watch::title_memory_reading`), and the
/// refreshes in all.
///
/// While a title streams, the frame line's "textures" is seconds a frame in
/// some runs and tenths in others for the same megabytes, and the
/// submitting thread is not running while it lasts (a few hundred
/// milliseconds of its own time a frame beside more than a second of
/// "textures"): it waits.
/// These say for what.
pub(crate) static REFRESH_WAITS: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// `f`'s result, its time added to [`REFRESH_WAITS`]' place `slot`.
fn waited<T>(slot: usize, f: impl FnOnce() -> T) -> T {
    let started = std::time::Instant::now();
    let out = f();
    REFRESH_WAITS[slot].fetch_add(started.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
    out
}

thread_local! {
    /// Room to read a texture's memory into: one buffer per submitting
    /// thread. Each texture kept its own, a second copy in memory of every
    /// write-watched or partly mapped texture ever sampled, for the run.
    static READ: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Calls `f` with `size` bytes of this thread's read buffer (their
/// contents are whatever the last texture left).
fn with_read<R>(size: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
    READ.with(|read| {
        let mut read = read.borrow_mut();
        if read.len() < size {
            read.resize(size, 0);
        }
        f(&mut read[..size])
    })
}

/// Whether a texture all of whose memory is mapped is read through the
/// kernel's copy instead of in place (`VICTORY_TEXTURE_COPY=1`; off by
/// default). An earlier form read every such texture that way to be safe
/// from pages taken away under the read; it cost twice the time per MB of
/// the read in place (three times the "textures" time while a title
/// streams), and the runs with it lost the graphics
/// device, for a reason not found. The read in place is safe now for
/// another reason (`crate::watch::title_memory_reading`); this switch is
/// left to tell whether the copy was what lost the device.
fn through_copy() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_COPY").is_some_and(|v| v == "1"))
}

/// Calls `f` with the `size` bytes of title memory at `memory`, read
/// through the kernel into this thread's read buffer; pages that are not
/// there read as zeros. The bytes are those a read in place would see.
fn with_title<R>(memory: *const u8, size: usize, f: impl FnOnce(&[u8]) -> R) -> R {
    with_read(size, |read| {
        crate::host::copy_mapped(read.as_mut_ptr(), memory, size, false);
        f(read)
    })
}

/// Staging buffers alive (also those only a list still holds) and their
/// bytes.
static STAGED: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

impl Drop for Staging {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        STAGED[0].fetch_sub(1, Relaxed);
        STAGED[1].fetch_sub(self.bytes, Relaxed);
    }
}

/// For the memory line: staging buffers alive, their bytes, and how many
/// of them their texture still has (the others wait for a list's reset).
#[cfg(feature = "diagnostics")]
pub(crate) fn memory() -> [u64; 3] {
    use std::sync::atomic::Ordering::Relaxed;
    let kept = STAGING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map_or(0, |map| map.len() as u64);
    [STAGED[0].load(Relaxed), STAGED[1].load(Relaxed), kept]
}

/// For the run report: the bytes of staging buffers alive (pieces of the
/// pool's blocks among them).
pub(crate) fn report_staged() -> u64 {
    STAGED[1].load(std::sync::atomic::Ordering::Relaxed)
}

/// Staging buffers let go as idle since the start, and their bytes.
static LET_GO: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// For the memory line: staging buffers let go as idle, and their bytes.
#[cfg(feature = "diagnostics")]
pub(crate) fn let_go() -> [u64; 2] {
    use std::sync::atomic::Ordering::Relaxed;
    [LET_GO[0].load(Relaxed), LET_GO[1].load(Relaxed)]
}

/// Lets go of the staging buffers no list recorded a use of for `IDLE_MS`
/// and `IDLE_FRAMES` frames and no list holds: a staging buffer is system memory the size of its
/// texture, kept so that an unchanged texture is not uploaded again, and
/// those of textures no longer sampled were kept for the run (thousands of
/// them, gigabytes). The next list that samples such a
/// texture makes a staging buffer anew, detiles and uploads once more.
/// Called before a submission starts, by the thread that submits. How long
/// idle, and how many bytes of them go at most (the longest idle first), is
/// `crate::shadow::ram_rule`'s.
pub(crate) fn let_go_idle() {
    use std::sync::atomic::Ordering::Relaxed;
    const IDLE_FRAMES: u64 = 4;
    // (Asked before the lock: it asks the system.)
    let (idle_ms, most) = crate::shadow::ram_rule();
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let Some(map) = map.as_mut() else {
        return;
    };
    if map.is_empty() {
        return;
    }
    // (Before the look: a thread that takes a remembered one from here on
    // asks the table instead, `staging_found`.)
    staging_moved();
    // A list that recorded an upload or a check holds the staging until it
    // is reset: one only the table holds is named by no list, recorded or
    // running.
    let mut idle: Vec<(u64, usize)> = map
        .iter()
        .filter(|(_, s)| Arc::strong_count(s) == 1 && crate::host::long_ago(s.used.load(Relaxed), idle_ms, IDLE_FRAMES))
        .map(|(&key, s)| (s.used.load(Relaxed) & 0xffff_ffff, key))
        .collect();
    if most != u64::MAX {
        idle.sort_unstable();
    }
    let (mut went, mut bytes) = (0u64, 0u64);
    for (_, key) in idle {
        if bytes >= most {
            break;
        }
        if let Some(s) = map.remove(&key) {
            LET_GO[0].fetch_add(1, Relaxed);
            LET_GO[1].fetch_add(s.bytes, Relaxed);
            went += 1;
            bytes += s.bytes;
        }
    }
    if went > 0 {
        crate::host::trail(|| format!("sweep: {went} staging buffers of textures let go ({} KiB), held by no list", bytes >> 10));
    }
}

/// Textures the host GPU wrote by a copy or resolve: their contents are on
/// the host, not in title memory, which stays as it was.
static HOST_WRITTEN: Mutex<Option<std::collections::HashSet<usize>>> = Mutex::new(None);

/// A host copy or resolve writes `resource`.
pub(crate) fn host_written(resource: This) {
    let new = {
        let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
        set.get_or_insert_with(Default::default).insert(resource as usize)
    };
    // Diagnostics: where its title memory is, for placed views
    // the title reads that memory through ([`placed_in_host_written`]).
    if new && let Some((at, desc)) = crate::resource::texture_memory(resource) {
        // SAFETY: 56 bytes in the description's layout.
        let words = unsafe { std::ptr::from_ref(&desc).cast::<[u32; 14]>().read_unaligned() };
        let bytes = crate::resource::placed_bytes(&words).unwrap_or(0);
        if !at.is_null() && bytes > 0 {
            HOST_WRITTEN_AT
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(at as u64, (bytes, words[4], words[6], words[7], words[8], resource as usize));
        }
    }
}

/// The title memory of textures the host copied into, by address: bytes
/// and description words 4, 6, 7 and 8 (width, height, layers and levels,
/// format).
static HOST_WRITTEN_AT: Mutex<std::collections::BTreeMap<u64, (u64, u32, u32, u32, u32, usize)>> =
    Mutex::new(std::collections::BTreeMap::new());

/// A texture the title placed at `address` (description words `desc`) was
/// made: says so once if it lies in the memory of a texture the host
/// copied into ([`host_written`]). Its own host copy is filled from title
/// memory, which the copy never reached: the copied texels are on the
/// card in the other texture, where no shader that reads this view finds
/// them.
pub(crate) fn placed_in_host_written(address: u64, desc: &[u32; 14]) {
    let hit = {
        let map = HOST_WRITTEN_AT.lock().unwrap_or_else(|e| e.into_inner());
        map.range(..=address)
            .next_back()
            .filter(|(at, d)| address < **at + d.0)
            .map(|(at, d)| (*at, *d))
    };
    if let Some((at, (bytes, width, height, layers_levels, format, _))) = hit {
        static SAID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 200 {
            tracing::warn!(
                view_address = format_args!("{address:#x}"),
                view_offset = address - at,
                view_width = desc[4],
                view_height = desc[6],
                view_layers_levels = format_args!("{:#x}", desc[7]),
                view_format = desc[8],
                copied_address = format_args!("{at:#x}"),
                copied_bytes = bytes,
                copied_width = width,
                copied_height = height,
                copied_layers_levels = format_args!("{layers_levels:#x}"),
                copied_format = format,
                "HERO LEAD: a placed texture view lies in the memory of a texture the host copied into: it is filled from title memory, which the copy never reached"
            );
        }
    }
}

/// Whether the host GPU wrote `resource` by a copy or resolve
/// ([`host_written`]): its host copy cannot be made again from title
/// memory.
pub(crate) fn host_wrote(resource: usize) -> bool {
    HOST_WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(&resource))
}

/// The title released a resource.
pub(crate) fn forget(resource: usize) {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = map.as_mut()
        && map.contains_key(&resource)
    {
        staging_moved();
        map.remove(&resource);
    }
    let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(set) = set.as_mut() {
        set.remove(&resource);
    }
    // Its memory may be given to other textures now (most of the
    // "HERO LEAD" lines were views in memory a released texture had held).
    HOST_WRITTEN_AT.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, d| d.5 != resource);
    let mut through = THROUGH.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(through) = through.as_mut() {
        through.remove(&resource);
    }
}

/// Whether `resource`'s host copy takes its contents from title memory:
/// a texture the host GPU does not render to or write.
pub(crate) fn title_written(texture: &ID3D12Resource) -> bool {
    const TARGET_DEPTH_UNORDERED: i32 = 0x7;
    // SAFETY: a live resource.
    unsafe { texture.GetDesc() }.Flags.0 & TARGET_DEPTH_UNORDERED == 0
}

/// Records, once per list, the copy of `resource`'s staging buffer into
/// its host copy, and the detiling that refreshes the staging buffer at
/// submission if the memory changed. Leaves the texture in the copy
/// destination state.
pub(crate) fn upload(list: &HostList, resource: This, _texture: &ID3D12Resource) {
    upload_from(list, resource, 0);
}

thread_local! {
    /// The thread records uploads for a draw that bakes from them.
    pub(crate) static BAKING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The thread records the upload of a texture picked by index, of which
    /// that many bytes were found mapped just now (`u64::MAX`: not asked).
    pub(crate) static PICKING: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// Whether a staging buffer made for one upload of a texture picked by index
/// is filled without asking the write watch and the guards
/// (`VICTORY_TEXTURE_ONCE=0`: with, as before). What they say is for the next
/// refresh, and there is none: the buffer goes with the upload. Asking guards
/// the texture's blocks (a change of protection, under the lock every other
/// refresh wants), and the title's next write there faults to open them.
fn once() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_ONCE").is_none_or(|v| v != "0"))
}

/// A 128-bit digest of `bytes`, to tell a texture's memory changed: four
/// lanes of folded multiplications over 64 bytes at a time. (Not a
/// cryptographic one, which took 0.27 ms a megabyte, four times the detiling
/// it was to spare; nobody makes up textures to collide.)
pub(crate) fn digest(bytes: &[u8]) -> Digest {
    const K: [u64; 8] = [
        0xa076_1d64_78bd_642f,
        0xe703_7ed1_a0b4_28db,
        0x8ebc_6af0_9c88_c6e3,
        0x5899_65cc_7537_4cc3,
        0x1d8e_4e27_c47d_124f,
        0xeb44_acca_b455_d165,
        0x9e37_79b9_7f4a_7c15,
        0xc2b2_ae3d_27d4_eb4f,
    ];
    let fold = |a: u64, b: u64| {
        let p = u128::from(a) * u128::from(b);
        p as u64 ^ (p >> 64) as u64
    };
    let word = |b: &[u8]| {
        let mut w = [0; 8];
        w[..b.len()].copy_from_slice(b);
        u64::from_le_bytes(w)
    };
    let mut lanes = [K[0], K[1], K[2], K[3]];
    let mut step = |block: &[u8; 64]| {
        for (i, lane) in lanes.iter_mut().enumerate() {
            let a = word(&block[16 * i..16 * i + 8]);
            let b = word(&block[16 * i + 8..16 * i + 16]);
            // (What went in is added again: a product with a factor of zero
            // forgets neither the lane nor the block.)
            *lane = fold(a ^ K[4 + i], b ^ *lane).wrapping_add(a.rotate_left(23) ^ lane.rotate_left(41)) ^ b;
        }
    };
    let mut blocks = bytes.chunks_exact(64);
    for block in &mut blocks {
        step(block.try_into().unwrap_or(&[0; 64]));
    }
    let rest = blocks.remainder();
    let mut last = [0u8; 64];
    last[..rest.len()].copy_from_slice(rest);
    step(&last);
    let len = bytes.len() as u64;
    let low = fold(lanes[0] ^ K[1], lanes[1] ^ len) ^ fold(lanes[2] ^ K[3], lanes[3] ^ K[5]);
    let high = fold(lanes[0].rotate_left(32) ^ K[6], lanes[2] ^ len) ^ fold(lanes[1].rotate_left(32) ^ K[7], lanes[3] ^ K[2]);
    Digest(u128::from(low) << 64 | u128::from(high))
}

/// See [`digest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Digest(u128);

/// [`upload`] of the levels from `first` on, the ones a view shows: of a
/// texture the title placed there may be no memory of the larger ones, and
/// what lies there is another texture's. (After victory-amd 4436f8f1.)
pub(crate) fn upload_from(list: &HostList, resource: This, first: u32) {
    let first = if crate::resource::held_only() { first } else { 0 };
    let host_written = HOST_WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(&(resource as usize)));
    // shortcut: a texture the host copied into keeps the host's contents
    // even if the title later writes its memory too.
    if host_written {
        return;
    }
    // Once a list for each first level; not again for one that the list's
    // upload of larger ones held too.
    if !list.first_use(1 << 63 | (resource as usize) << 4 | first as usize & 15) {
        return;
    }
    if !list.first_use(resource as usize) && staged_from(resource) <= first {
        return;
    }
    let Some(copy) = crate::resource::host_copy(resource) else {
        return;
    };
    ahead::listed(&copy);
    let Some(staging) = staging(resource, &copy, first) else {
        return;
    };
    use std::sync::atomic::Ordering::Relaxed;
    let baking = BAKING.get();
    if baking {
        staging.exact.store(true, Relaxed);
    }
    let picking = PICKING.get();
    if picking.is_none() {
        // A draw samples it through a table: kept and checked from here on.
        staging.once.store(false, Relaxed);
    }
    staging.used.store(crate::host::moment(), Relaxed);
    // Levels on demand: a view of the texture now starts at another
    // level, or another texture now starts inside its span, with no byte
    // of its memory changed. The levels are judged anew: detiled again at
    // this list's submission, and uploaded by this list.
    if staging.on_demand && staging.judgement() != staging.judged() {
        staging.forget();
        staging.dirty.store(true, Relaxed);
    }
    // Levels on demand: the last upload left a held level without tiles
    // (the card had no room, or no heap could be had), and nothing else
    // would record an upload again until the title writes the texture: the
    // level stayed empty, the texture blurred or black. Asked again about
    // once a second, and only when the card has room; the staging buffer
    // still holds the level's bytes.
    if staging.on_demand {
        let since = staging.waiting.load(Relaxed);
        if since != 0 && crate::host::long_ago(since, 1000, 2) {
            if crate::resource::room_lately() {
                staging.waiting.store(0, Relaxed);
                staging.dirty.store(true, Relaxed);
                crate::tiles::STATS[10].fetch_add(1, Relaxed);
            } else {
                staging.waiting.store(crate::host::moment().max(1), Relaxed);
            }
        }
    }
    // The video decoder cycles CPU-written linear plane buffers: the list copies from a buffer of its own, filled
    // from title memory at this list's place in the submission, not from the staging buffer every list shares,
    // which the next submission's refresh overwrites while an earlier flight may still read it.
    // After upstream GPL-3.0 fork, commit 8cc31ab37afc "Port NVIDIA Wave32 lowering,
    // smooth movie-plane uploads, XMA audio fixes, and 32GB RAM residency budgets": the branch. Here: taken only
    // for a texture the title was seen to rewrite after an upload (`VICTORY_MOVIE_PLANES`, [`movie_planes`]; `=all`:
    // by shape alone, as there), and no `streak_upload` count on this path.
    let (uploaded, dirty, streak) =
        (staging.uploaded.load(Relaxed), staging.dirty.load(Relaxed), staging.streak.load(Relaxed));
    let planed = staging.planed.load(Relaxed);
    if uses_plane(movie_planes(), staging.movie_plane(), streak, planed) {
        if !planed {
            staging.planed.store(true, Relaxed);
        }
        let (at, len) = staging.memory();
        let pitch = staging.footprints[0].Footprint.RowPitch as usize;
        let bytes = pitch * staging.rows[0] as usize;
        let plane = staging.clone();
        let Some(source) = list.plane_upload(bytes as u64, (at, len), move |dst| {
            fill_plane(&plane.layout, at, len, dst, bytes, pitch);
        }) else {
            once!("no upload buffer for a CPU-written movie plane");
            return;
        };
        // The host copy no longer holds what a hash of the memory says ([`unchanged`]).
        *staging.holds.lock().unwrap_or_else(|e| e.into_inner()) = None;
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexUploads, 1);
        record_upload(list, resource, &copy, &staging, &source, 0, uploaded);
        // Noted when the list is submitted, as the other uploads are: a list recorded before it was submitted
        // carries the aliasing barrier too, which twice does no harm.
        list.defer_host(move || staging.uploaded.store(true, Relaxed));
        return;
    }
    // With `VICTORY_PRELUDE_UPLOADS=1`: whether to upload is decided when
    // the list is submitted, after the check that refreshes the staging
    // buffer, and the copy goes on the list's prelude ([`PreludeUpload`]).
    if prelude_uploads() && !baking {
        if no_wait() && fill_unlocked() {
            let there = match picking {
                Some(there) if there != u64::MAX => there,
                _ => mapped_bytes(staging.memory, staging.read),
            };
            staging.expected.store(there, Relaxed);
        }
        let on_demand = if staging.on_demand { crate::resource::host_tiled(resource) } else { None };
        // (Recorded on the list's prelude at its submission, ahead of what
        // the list reads: as good as recorded here for what shows it.)
        crate::resource::note_recorded(&copy, staging.first);
        list.defer_prelude_upload(PreludeUpload { staging, copy, on_demand });
        return;
    }
    // The host texture has the staging buffer's contents: only the check
    // at submission is left, and what it finds changed the next list that
    // samples the texture uploads (a frame late where every list of the
    // frame was recorded before that check). A bake records the copy anyway,
    // so that a change found at its submission lands before its draw runs
    // (`exact`).
    if !baking && skips_upload(uploaded, dirty, streak) {
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexSkipped, 1);
        list.defer_texture(staging);
        return;
    }
    if uploaded && !dirty {
        // Only the texture's last use found it changed ([`streak_on`]).
        streak_upload();
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::TexUploads, 1);
    if no_wait() && fill_unlocked() {
        let there = match picking {
            Some(there) if there != u64::MAX => there,
            _ => mapped_bytes(staging.memory, staging.read),
        };
        staging.expected.store(there, Relaxed);
    }
    record_upload(list, resource, &copy, &staging, &staging.buffer, staging.base, uploaded);
    // When the list is submitted, its copy takes what the staging buffer
    // holds by then. This is meant to run before the check of the same
    // submission, which sets `dirty` again if it detiles (the next list then
    // uploads once more), but with `VICTORY_JOIN_PARTS` the textures are
    // detiled ahead of the steps (`Batch::refresh_front`) and it runs after
    // that, clearing what the detiling set: a texture rewritten before every
    // use then skipped every second one (a video, frames A B C A B
    // C D E F D E F). `streak` ([`streak_on`]) keeps uploading such a texture
    // in either order.
    {
        let staging = staging.clone();
        list.defer_host(move || {
            staging.uploaded.store(true, Relaxed);
            staging.dirty.store(false, Relaxed);
        });
    }
    // A host copy that took the place of one with fewer levels: the slots
    // that show that one are made again once this upload has run.
    if staging.first == staging.level0 && copy.grown.swap(false, Relaxed) {
        let start = staging.memory as u64;
        list.defer_host(move || crate::descriptor::host::levels_changed_after(start));
    }
    let noted = staging.clone();
    // Levels on demand (`crate::tiles`): after the check below, in the
    // order of this list's steps, and before the list is executed, the
    // levels the staging buffer holds as the title's are given the card's
    // memory if they have none. The list copies every level, so each one
    // given here is written whole by it (the rest are copies into no
    // memory, which the card drops).
    let on_demand = if staging.on_demand { crate::resource::host_tiled(resource) } else { None };
    let Some(on_demand) = on_demand else {
        // Once per submission however many of its lists sample the texture.
        list.defer_texture(staging);
        // Then what the list's copy puts in the host copy is known.
        list.defer_host(move || noted.note_held());
        return;
    };
    list.defer_texture(staging.clone());
    list.defer_host(move || {
        let waiting = on_demand.uploaded(staging.detiled.load(Relaxed), staging.memory as u64);
        staging.waiting.store(if waiting { crate::host::moment().max(1) } else { 0 }, Relaxed);
        // A texture picked by index has no staging buffer after this
        // upload: its slots are made due again by the texture pass.
        if waiting {
            crate::descriptor::host::levels_waiting(staging.memory as u64);
        }
    });
}

/// Whether title textures that lists sample are uploaded on each list's
/// prelude at submission, once the check that refreshes the staging buffer
/// has run, if it changed or was never uploaded (`VICTORY_PRELUDE_UPLOADS=1`;
/// off: the list records the upload when it samples the texture, if the
/// staging buffer was changed then, as before; bakes and movie planes keep
/// that either way).
///
/// Recorded when the list samples a texture, an upload is left out if the
/// staging buffer held what the host copy holds at that moment; the check at
/// submission may then find the memory changed and detile it, and only the
/// next list that samples the texture uploads that: a frame late when every
/// list of the frame was recorded before the check (a ring of video buffers
/// then shows an older picture, a texture streamed in shows the one before
/// for a frame). Decided at submission, the copy takes what the check found,
/// in the submission that samples it. Off by default: when textures are
/// uploaded changes. Ported from trustno1-coder/VIctory-NVIDIA 91df493 (from
/// Belobug/VIctory-Belobug-AMD 5d6bd84).
pub(crate) fn prelude_uploads() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PRELUDE_UPLOADS").is_some_and(|v| v == "1"))
}

/// Whether the prelude of the list being submitted records the upload into
/// a host texture: `Some(first upload)` when it was never uploaded or its
/// staging buffer was detiled since the last upload; `None` when the host
/// texture holds what the staging buffer holds. Both marks are taken.
fn claim_upload(uploaded: &std::sync::atomic::AtomicBool, dirty: &std::sync::atomic::AtomicBool) -> Option<bool> {
    use std::sync::atomic::Ordering::AcqRel;
    let was_uploaded = uploaded.swap(true, AcqRel);
    let was_dirty = dirty.swap(false, AcqRel);
    (!was_uploaded || was_dirty).then_some(!was_uploaded)
}

/// [`claim_upload`] taken back (the list did not run): uploaded again by
/// the next list that samples the texture.
fn unclaim_upload(uploaded: &std::sync::atomic::AtomicBool, dirty: &std::sync::atomic::AtomicBool, first: bool) {
    use std::sync::atomic::Ordering::Release;
    if first {
        uploaded.store(false, Release);
    }
    dirty.store(true, Release);
}

/// A title texture a list samples, its upload decided at the list's
/// submission ([`prelude_uploads`]): its staging buffer, its host copy (both
/// held by the list until it is reset), and its levels on demand.
#[derive(Clone)]
pub(crate) struct PreludeUpload {
    staging: Arc<Staging>,
    pub(crate) copy: Arc<crate::resource::HostTexture>,
    on_demand: Option<crate::resource::OnDemand>,
}

impl PreludeUpload {
    /// The staging buffer, for the submission's check (`Step::Texture`).
    pub(crate) fn staging(&self) -> Arc<Staging> {
        self.staging.clone()
    }

    /// On the submitting thread, once the check that refreshes the staging
    /// buffer has run for this submission: `Some(first upload)` if the
    /// list's prelude records the copy into the host texture (then
    /// [`PreludeUpload::recorded`] once it has, or
    /// [`PreludeUpload::unclaim`] if the list does not run).
    pub(crate) fn claim(&self) -> Option<bool> {
        let claimed = claim_upload(&self.staging.uploaded, &self.staging.dirty);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(
            if claimed.is_some() { crate::copystat::C::TexUploads } else { crate::copystat::C::TexSkipped },
            1,
        );
        claimed
    }

    /// The claim of [`PreludeUpload::claim`] taken back.
    pub(crate) fn unclaim(&self, first: bool) {
        unclaim_upload(&self.staging.uploaded, &self.staging.dirty, first);
    }

    /// Whether the first upload into the host copy needs the aliasing
    /// barrier before it, as [`record_upload`] decides.
    pub(crate) fn aliasing(&self, first: bool) -> bool {
        crate::pool::on()
            && needs_alias(
                alias_always() || (first && crate::pool::in_block(&self.copy.texture)),
                self.staging.on_demand,
                !first,
            )
    }

    /// Once the prelude holds the copy, before the list runs: what the
    /// list's own upload notes (a host copy grown, the levels on demand
    /// given memory, what the host copy holds).
    pub(crate) fn recorded(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        // A host copy that took the place of one with fewer levels: the slots
        // that show that one are made again once this upload has run.
        if self.staging.first == self.staging.level0 && self.copy.grown.swap(false, Relaxed) {
            crate::descriptor::host::levels_changed_after(self.staging.memory as u64);
        }
        // Levels on demand (`crate::tiles`): the held levels the staging
        // buffer holds as the title's are given the card's memory before the
        // prelude copies into them; else what the host copy now holds is
        // noted.
        match &self.on_demand {
            Some(on_demand) => {
                let waiting = on_demand.uploaded(self.staging.detiled.load(Relaxed), self.staging.memory as u64);
                self.staging.waiting.store(if waiting { crate::host::moment().max(1) } else { 0 }, Relaxed);
                // A texture picked by index has no staging buffer after this
                // upload: its slots are made due again by the texture pass.
                if waiting {
                    crate::descriptor::host::levels_waiting(self.staging.memory as u64);
                }
            }
            None => self.staging.note_held(),
        }
    }

    /// Records the copy of the staging buffer into the host texture on
    /// `list` (the prelude), the host texture in the copy destination state.
    pub(crate) fn record(&self, list: &ID3D12GraphicsCommandList) {
        crate::host::gpu_split::count(crate::host::gpu_split::Volume::TextureBytes, self.staging.bytes);
        let texture = &self.copy.texture;
        crate::resource::note_recorded(&self.copy, self.staging.first);
        // The staging buffer lives as long as `self.staging`, which the
        // list's steps hold until it is reset.
        for (i, footprint) in self.staging.footprints.iter().enumerate() {
            let to = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: a borrowed pointer the location never releases.
                pResource: unsafe { std::mem::transmute_copy(texture) },
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: self.staging.first - self.staging.level0 + i as u32,
                },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: as above.
                pResource: unsafe { std::mem::transmute_copy(&self.staging.buffer) },
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: self.staging.base + footprint.Offset,
                        ..*footprint
                    },
                },
            };
            // SAFETY: an open list; both locations name live resources.
            unsafe { list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
        }
    }
}

/// Records the copies of `source` (from `base`, laid out as the staging
/// buffer's footprints say) into the host copy of `resource`, with the
/// aliasing barrier that precedes a texture's first upload. `uploaded`:
/// whether an upload was recorded before.
// After upstream GPL-3.0 fork, commit 8cc31ab37afc "Port NVIDIA Wave32 lowering,
// smooth movie-plane uploads, XMA audio fixes, and 32GB RAM residency budgets": the copy loop of `upload_from`
// pulled out to take its `source` and `base`. Here: it keeps the aliasing rule (`needs_alias`, `in_block`)
// and takes `uploaded` as it was read before this upload was recorded.
fn record_upload(
    list: &HostList,
    resource: This,
    copy: &crate::resource::HostTexture,
    staging: &Staging,
    source: &ID3D12Resource,
    base: u64,
    uploaded: bool,
) {
    const COPY_DEST: u32 = 0x400;
    let texture = &copy.texture;
    // The first upload into a copy cut from a block, from a later level than
    // its first: what the block held below that level is not shown, the
    // levels get zeros (`crate::resource::show_written`).
    let below = staging.first.saturating_sub(staging.level0);
    let cover = below > 0
        && copy.pooled
        && copy.recorded.load(std::sync::atomic::Ordering::Relaxed) == u32::MAX
        && !copy.card_wrote.load(std::sync::atomic::Ordering::Relaxed)
        && crate::resource::show_written();
    // A texture cut from a block may lie where another did: what the card
    // holds of that one is done with before the first upload into this one.
    // Not one made as a committed resource of its own (a piece too large for
    // a block, 16 MiB at 8 GB cards): the runtime fails the list's `Close`
    // with E_INVALIDARG for an aliasing barrier on it ("a host command list
    // did not close"; checked by the
    // test `an_aliasing_barrier_on_a_committed_texture_and_on_a_placed_one`).
    // One whose host copy is reserved (levels on demand) has always had it.
    // `VICTORY_ALIAS_ALWAYS=1`: every texture, as before.
    if crate::pool::on()
        && needs_alias(alias_always() || (!uploaded && crate::pool::in_block(texture)), staging.on_demand, uploaded)
    {
        let aliasing = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_ALIASING,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Aliasing: std::mem::ManuallyDrop::new(D3D12_RESOURCE_ALIASING_BARRIER {
                    pResourceBefore: std::mem::ManuallyDrop::new(None),
                    // SAFETY: a borrowed pointer the barrier never releases.
                    pResourceAfter: unsafe { std::mem::transmute_copy(texture) },
                }),
            },
        };
        // SAFETY: an open list and a live texture.
        unsafe { list.list.ResourceBarrier(&[aliasing]) };
    }
    crate::list::ensure_state(list, resource, COPY_DEST);
    // The card's time of texture uploads, and their bytes (`host::gpu_split`).
    list.gpu_kind(crate::host::gpu_split::Kind::Textures);
    crate::host::gpu_split::count(crate::host::gpu_split::Volume::TextureBytes, staging.bytes);
    if cover && let Some(host) = host() {
        zeros_below(list, host, texture, below);
    }
    crate::resource::note_recorded(copy, staging.first);
    // The staging buffer lives as long as `staging`, which the deferred
    // refresh the caller records holds until the list is reset; a movie plane's
    // buffer, until the list is reset ([`HostList::plane_upload`]).
    for (i, footprint) in staging.footprints.iter().enumerate() {
        #[cfg(feature = "diagnostics")]
        {
            let f = footprint.Footprint;
            // SAFETY: live resources.
            let (gpu, d) = unsafe { (source.GetGPUVirtualAddress(), texture.GetDesc()) };
            let len = u64::from(f.RowPitch) * u64::from(staging.rows[i]) * u64::from(f.Depth);
            list.note_read(
                || {
                    format!(
                        "texture upload {resource:?} ({}x{}x{} f{} mips {}) sub {i} of {} from staging {gpu:#x}+{:#x}: offset {:#x} {}x{}x{} pitch {}",
                        d.Width,
                        d.Height,
                        d.DepthOrArraySize,
                        d.Format.0,
                        d.MipLevels,
                        staging.footprints.len(),
                        staging.size,
                        base + footprint.Offset,
                        f.Width,
                        f.Height,
                        f.Depth,
                        f.RowPitch
                    )
                },
                (gpu + base + footprint.Offset, len),
            );
        }
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: staging.first - staging.level0 + i as u32,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(source) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: base + footprint.Offset,
                    ..*footprint
                },
            },
        };
        // SAFETY: an open list; both locations name live resources.
        unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
}

/// Fills `bytes` at `dst` (a buffer of the list's own) with the linear plane
/// at `at` in title memory (`len` bytes of it), rows `pitch` bytes apart: in
/// one read when the title's rows are as far apart, else row by row through
/// the layout. Zeros if the layout cannot be read.
// After upstream GPL-3.0 fork, commit 8cc31ab37afc "Port NVIDIA Wave32 lowering,
// smooth movie-plane uploads, XMA audio fixes, and 32GB RAM residency budgets": `fill_plane`. Here: the same.
fn fill_plane(layout: &victory_formats::texture::Layout, at: usize, len: usize, dst: usize, bytes: usize, pitch: usize) {
    let _pin = crate::watch::title_memory_reading();
    // SAFETY: the list owns this whole upload buffer until its work on the card is done.
    let out = unsafe { std::slice::from_raw_parts_mut(dst as *mut u8, bytes) };
    let mip = &layout.mips[0];
    if mip.offset == 0
        && mip.pitch as usize * layout.desc.element_bytes as usize == pitch
        && bytes <= len
        && crate::host::read_into(at as *const _, out)
    {
        return;
    }
    with_read(len, |read| {
        crate::host::copy_mapped(read.as_mut_ptr(), at as *const u8, len, false);
        if let Err(error) = layout.detile(read, 0, 0, out, pitch, bytes) {
            out.fill(0);
            once!("could not copy the linear plane into the list's upload buffer: {error:?}");
        }
    });
}

/// Levels on demand: the levels a list claimed for its zeros
/// (`crate::tiles::Tiled::claim`), held by the list's submission-time
/// step. A list that is reset without having been submitted gives the
/// claim up, or the next list that writes those levels would send no zeros
/// and what its write does not cover would be what the tiles' last owner
/// left.
struct Claimed {
    on_demand: crate::resource::OnDemand,
    fresh: u32,
    ran: std::sync::atomic::AtomicBool,
}

impl Drop for Claimed {
    fn drop(&mut self) {
        if self.fresh != 0
            && !*self.ran.get_mut()
            && let Some(tiled) = self.on_demand.tiled()
        {
            tiled.unclaim(self.fresh);
        }
    }
}

/// Records copies of zeros into every subresource of `texture`, which is in
/// the copy destination state.
pub(crate) fn zeros(list: &HostList, host: &crate::host::Host, texture: &ID3D12Resource) {
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let layers = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D { 1 } else { u32::from(d.DepthOrArraySize) };
    let count = u32::from(d.MipLevels) * layers;
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
    let mut bytes = 0u64;
    // SAFETY: outputs for `count` subresources.
    unsafe {
        host.device
            .GetCopyableFootprints(&d, 0, count, 0, Some(footprints.as_mut_ptr()), None, None, Some(&mut bytes));
    }
    if bytes == 0 || bytes == u64::MAX {
        return;
    }
    let Some((buffer, at, data)) = list.upload_at(host, bytes) else {
        return;
    };
    // SAFETY: a mapped range of `bytes` bytes, the list's own.
    unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
    for footprint in &mut footprints {
        footprint.Offset += at;
    }
    for (i, footprint) in footprints.iter().enumerate() {
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: i as u32 },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above; the list holds the buffer until it is reset.
            pResource: unsafe { std::mem::transmute_copy(&buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: *footprint },
        };
        // SAFETY: an open list; both locations name live resources.
        unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
}

/// The subresources of a texture of `mips` levels and `layers` array slices
/// (1 for a 3D texture) that are its first `below` levels: those an upload
/// from level `below` on leaves out ([`zeros_below`]).
pub(crate) fn levels_below(below: u32, mips: u32, layers: u32) -> Vec<u32> {
    let below = below.min(mips);
    (0..layers).flat_map(|layer| (0..below).map(move |level| layer * mips + level)).collect()
}

/// Records copies of zeros into the first `below` levels of `texture` (every
/// array slice), which is in the copy destination state.
fn zeros_below(list: &HostList, host: &crate::host::Host, texture: &ID3D12Resource, below: u32) {
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let layers = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D { 1 } else { u32::from(d.DepthOrArraySize) };
    let subresources = levels_below(below, u32::from(d.MipLevels), layers);
    if subresources.is_empty() {
        return;
    }
    crate::resource::UNWRITTEN[5].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    for index in subresources {
        let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut bytes = 0u64;
        // SAFETY: outputs for one subresource.
        unsafe {
            host.device.GetCopyableFootprints(&d, index, 1, 0, Some(&mut footprint), None, None, Some(&mut bytes));
        }
        if bytes == 0 || bytes == u64::MAX {
            continue;
        }
        let Some((buffer, at, data)) = list.upload_at(host, bytes) else {
            return;
        };
        // SAFETY: a mapped range of `bytes` bytes, the list's own.
        unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
        footprint.Offset += at;
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: index },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above; the list holds the buffer until it is reset.
            pResource: unsafe { std::mem::transmute_copy(&buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: footprint },
        };
        // SAFETY: an open list; both locations name live resources.
        unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
}

/// Levels on demand (`crate::tiles`): the card itself is about to write
/// title texture `resource` in `list` (a copy or a resolve into level
/// `level`, or into every level). If its host copy `texture` was made
/// without memory, the levels written get theirs when the list is
/// submitted, ahead of it on the queue; the ones that have none yet are
/// first written with zeros, here in the same list, so that what the
/// card's write does not cover is not what the tiles' last owner left.
/// To be called before the list is brought to the state of that write.
/// shortcut: two lists that write the same new level, submitted in the
/// other order than recorded, lose the first one's write under the zeros
/// of the other; so does an upload recorded and not yet submitted.
pub(crate) fn card_writes(list: &HostList, resource: This, texture: &ID3D12Resource, level: Option<u32>) {
    let Some(on_demand) = crate::resource::host_tiled(resource) else {
        return;
    };
    let Some(tiled) = on_demand.tiled() else {
        return;
    };
    let levels = match level {
        Some(level) => tiled.with_level(level),
        None => tiled.every_level(),
    };
    if levels == 0 {
        return;
    }
    let fresh = tiled.claim(levels);
    // Of them, the ones whose zeros could not be recorded.
    let mut unzeroed = if host().is_none() { fresh } else { 0 };
    if fresh != 0
        && let Some(host) = host()
    {
        const COPY_DEST: u32 = 0x400;
        crate::list::ensure_state(list, resource, COPY_DEST);
        // SAFETY: a live resource.
        let d = unsafe { texture.GetDesc() };
        for level in (0..u32::from(d.MipLevels).min(32)).filter(|&level| fresh & (1u32 << level) != 0) {
            let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
            let mut bytes = 0u64;
            // SAFETY: outputs for one subresource.
            unsafe {
                host.device
                    .GetCopyableFootprints(&d, level, 1, 0, Some(&mut footprint), None, None, Some(&mut bytes));
            }
            let Some((buffer, at, data)) = list.upload_at(host, bytes) else {
                // (With the levels that share its tiles, if it is one of
                // the small ones.)
                unzeroed |= tiled.with_level(level);
                continue;
            };
            footprint.Offset += at;
            // SAFETY: a mapped range of `bytes` bytes, the list's own.
            unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
            let to = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: a borrowed pointer the location never releases.
                pResource: unsafe { std::mem::transmute_copy(texture) },
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: level },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: as above; the list holds the buffer until it is reset.
                pResource: unsafe { std::mem::transmute_copy(&buffer) },
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: footprint },
            };
            // SAFETY: an open list; both locations name live resources.
            unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
        }
    }
    // A new level without its zeros gets no tiles from this list: the
    // card's write into it is lost (the level reads zeros, as before the
    // write), which is less wrong than a part write over what the tiles'
    // last owner left. The next list that writes it claims it again.
    unzeroed &= fresh;
    if unzeroed != 0 {
        tiled.unclaim(unzeroed);
    }
    let levels = levels & !unzeroed;
    if levels == 0 {
        return;
    }
    let memory = crate::resource::texture_memory(resource).map_or(0, |(memory, _)| memory as u64);
    let step = Claimed {
        on_demand,
        fresh: fresh & !unzeroed,
        ran: std::sync::atomic::AtomicBool::new(false),
    };
    list.defer_host(move || {
        step.ran.store(true, std::sync::atomic::Ordering::Relaxed);
        step.on_demand.card_wrote(levels, memory);
    });
}

impl Staging {
    /// Levels on demand: what says, beside the mapping, which levels of
    /// the texture are the title's now. First the lowest level a live view
    /// of the texture shows (0 without a view that says): the title's own
    /// word, a texture it holds from level 3 on is viewed from level 3.
    /// Then where the next texture placed in this one's span starts, in
    /// bytes from this one's start (`u64::MAX`: none does): for a texture
    /// viewed from level 0 although a neighbour sits where its large
    /// levels would be.
    fn judgement(&self) -> [u64; 2] {
        let start = self.memory as u64;
        let first = crate::descriptor::host::first_viewed(start).map_or(0, u64::from);
        let limit = crate::descriptor::host::next_start(start, start.saturating_add(self.layout.size))
            .map_or(u64::MAX, |at| at - start);
        [first, limit]
    }

    /// What [`Staging::judgement`] said at the last detiling.
    fn judged(&self) -> [u64; 2] {
        use std::sync::atomic::Ordering::Relaxed;
        [self.judged[0].load(Relaxed), self.judged[1].load(Relaxed)]
    }

    /// Levels on demand: the levels that are the title's now, a bit a
    /// level. A level is held if no view starts above it, its bytes end
    /// before the next texture starts, and all of them are mapped. The
    /// smallest levels share the texture's first block and are held
    /// together. (The console layout: that block first, then the other
    /// levels from the smallest up, level 0 last:
    /// `victory_formats::texture::Layout`.) A texture this cannot tell
    /// the levels of holds them all.
    fn held_levels(&self, [first, limit]: [u64; 2]) -> u32 {
        let mips = &self.layout.mips;
        if self.layout.slices != 1 || mips.is_empty() || mips.len() > 32 {
            return u32::MAX;
        }
        let tail = (self.layout.first_tail_mip as usize).min(mips.len());
        let there = |from: u64, to: u64| {
            to > from && to <= limit && committed(self.memory.wrapping_add(from as usize), (to - from) as usize)
        };
        let mut held = 0u32;
        for (level, m) in mips.iter().enumerate().take(tail) {
            let end = if level == 0 { self.layout.slice_size } else { mips[level - 1].offset };
            if level as u64 >= first && there(m.offset, end) {
                held |= 1 << level;
            }
        }
        if tail < mips.len() && there(0, self.layout.alignment.min(self.layout.slice_size)) {
            for level in tail..mips.len() {
                if level as u64 >= first {
                    held |= 1 << level;
                }
            }
        }
        held
    }

    /// Forgets what the staging buffer was last filled from: the next
    /// refresh detiles anew, whether the memory changed or not.
    fn forget(&self) {
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = Last::default();
    }

    /// The title memory the texture is read from: address and bytes.
    pub(crate) fn memory(&self) -> (usize, usize) {
        (self.memory as usize, self.read)
    }

    /// The title's level and the array slice of the staging buffer's part
    /// `i`.
    fn part(&self, i: usize) -> (u32, u32) {
        let mips = self.mips - self.first;
        (self.first + i as u32 % mips, i as u32 / mips)
    }

    /// The host copy holds what the staging buffer was filled from
    /// ([`unchanged`]).
    fn note_held(&self) {
        let hash = self.last.lock().unwrap_or_else(|e| e.into_inner()).hash;
        *self.holds.lock().unwrap_or_else(|e| e.into_inner()) = hash.map(|hash| (self.first, hash));
    }

    /// Detiles the texture memory into the staging buffer if it changed
    /// since the last time (and is mapped at all).
    pub(crate) fn refresh(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        let started = std::time::Instant::now();
        let _texture = crate::timeline::span("a texture's refresh");
        let read = self.refresh_now();
        if streak_on() {
            let changed = self.changed.swap(false, Relaxed);
            let streak = next_streak(self.streak.load(Relaxed), changed, self.uploaded.load(Relaxed));
            self.streak.store(streak, Relaxed);
        }
        let preparing = &crate::host::PREPARING;
        preparing[2].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
        preparing[3].fetch_add(read as u64, Relaxed);
    }

    /// Whether the texture has the shape of a decoder's plane
    /// ([`plane_shape`]).
    fn movie_plane(&self) -> bool {
        plane_shape(&self.layout.desc, &self.footprints, self.on_demand)
    }

    /// Returns the bytes of title memory read.
    fn refresh_now(&self) -> usize {
        let whole = std::time::Instant::now();
        let read = self.refresh_timed();
        REFRESH_WAITS[3].fetch_add(whole.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
        read
    }

    fn refresh_timed(&self) -> usize {
        let size = self.read;
        let start = self.memory as usize;
        let mut last = waited(0, || self.last.lock().unwrap_or_else(|e| e.into_inner()));
        let last = &mut *last;
        // After upstream GPL-3.0 fork, commit f4b48ad766dd "Publish the pareto-residency
        // Windows build and portable launcher": a texture of a bake is read exactly: none of the early-outs below
        // ([`refreshes`]), no read that may fail while title memory is remapped, no pause between reads of a partly
        // mapped one. Here: the flag is set only while `BAKING` is, which `VICTORY_BAKE_EXACT=0` leaves unset.
        let exact = self.exact.swap(false, std::sync::atomic::Ordering::Relaxed);
        let alone = self.once.load(std::sync::atomic::Ordering::Relaxed);
        if !refreshes(exact, alone, last.hash.is_some(), false, false, false) {
            return 0;
        }
        if !alone && let Some((now, written)) = waited(1, || crate::watch::writes(start, start + size, false)) {
            let unwritten = last.asked != 0 && written.iter().all(|&(_, w)| w <= last.asked);
            if !refreshes(exact, false, false, unwritten, false, false) {
                return 0;
            }
            last.asked = now;
            // Read through the copy, which stands unmapped pages: memory of
            // big pools is too slow to ask page by page.
            with_read(size, |read| {
                crate::host::copy_mapped(read.as_mut_ptr(), self.memory, size, false);
                self.detile(read);
            });
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexWatched, size as u64);
            return size;
        }
        // View memory has no write-watch, and a texture there was hashed
        // whole at every check: hundreds of MB and tens of milliseconds a
        // frame in a view where two textures change. It is guarded now, as
        // buffers
        // in view memory are: a write of the title's to it faults once and
        // is noted, a mapping is announced by the runtime. While neither
        // happened the texture is as it was. What the guard cannot see (a
        // write to the same pages through another view) is still found by
        // the hash, which is taken every eighth frame only, by the
        // texture's address, so the textures take turns.
        // `VICTORY_TEXTURE_GUARD=0` hashes at every check, as before.
        static GUARD: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        // The guard saw no write: a change the hash finds came another way.
        let mut blind = false;
        if !alone
            && *GUARD.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_GUARD").is_none_or(|v| v != "0"))
            && let Some((now, written)) = waited(1, || crate::watch::writes(start, start + size, true))
        {
            const HASH_EVERY: u64 = 8;
            let unwritten = last.asked != 0 && written.iter().all(|&(_, w)| w <= last.asked);
            last.asked = now;
            let frame = crate::frame::current();
            let turn = frame
                .wrapping_add((start >> 16) as u64)
                .is_multiple_of(HASH_EVERY);
            // A texture starts hot: one written that way from its first
            // frames on changes before the hash has ever caught it.
            if last.hot_until == 0 {
                last.hot_until = frame + HOT_FRAMES;
            }
            let hot = hot_textures() && frame < last.hot_until;
            if !refreshes(exact, false, false, unwritten, turn, hot) {
                return 0;
            }
            blind = unwritten;
        }
        // A change the guard did not see: hashed at every check for a while.
        let found_blind = |last: &mut Last| {
            if blind && hot_textures() {
                last.hot_until = crate::frame::current() + HOT_FRAMES;
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::TexHot, 1);
            }
        };
        // From here on the texture's memory may be read in place. Held until
        // that read is done: the runtime changes what title memory shows
        // only with this taken alone, so what `committed` finds mapped
        // stays mapped (see `crate::watch::title_memory_reading`; without
        // it a page went between the asking and the read).
        let _mapped = if no_wait() && !exact {
            // Not waited for: while the runtime remaps title memory the
            // texture is read through the kernel instead, whole or not at
            // all (`no_wait`).
            let Some(mapped) = crate::watch::title_memory_reading_now() else {
                return self.refresh_unlocked(last, size);
            };
            mapped
        } else {
            waited(2, crate::watch::title_memory_reading)
        };
        if !committed(self.memory, size) {
            // Partly mapped: a streamed texture with only some mips' pages
            // there, the usual state while it streams in. Read through the
            // copy, which stands unmapped pages (zeros), and upload what
            // is there; nothing at all mapped changes nothing.
            // shortcut: read again at most every 30 frames (reading around
            // unmapped pages is slow, and nothing tells us when pages
            // arrive): a mip that streams in shows up to 30 frames late.
            let frame = crate::frame::current();
            if !exact && last.partial.is_some_and(|at| frame < at + 30) {
                return 0;
            }
            last.partial = Some(frame);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
            let changed = with_read(size, |read| {
                crate::host::copy_mapped(read.as_mut_ptr(), self.memory, size, false);
                let hash = digest(read);
                if last.hash == Some(hash) {
                    return false;
                }
                last.hash = Some(hash);
                if read.iter().all(|&b| b == 0) {
                    return false;
                }
                self.detile(read);
                true
            });
            #[cfg(feature = "diagnostics")]
            if changed {
                crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
            }
            if changed {
                found_blind(last);
            }
            return size;
        }
        // (A texture with pages in views the runtime keeps in its books
        // alone, `VICTORY_SOFT_VIEWS`, has no memory to read in place: its
        // bytes come from the pages, through the copy. `_mapped` is held: no
        // view becomes one of those before the read in place below is done.)
        if through_copy() || !crate::watch::soft_runs(self.memory as usize, size).is_empty() {
            return with_title(self.memory, size, |tiled| {
                let hash = digest(tiled);
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
                if last.hash == Some(hash) {
                    return size;
                }
                last.hash = Some(hash);
                found_blind(last);
                self.detile(tiled);
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
                size
            });
        }
        // SAFETY: committed memory of the texture's layout size, and it
        // stays committed while `_mapped` is held.
        let tiled = unsafe { std::slice::from_raw_parts(self.memory, size) };
        let hash = digest(tiled);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
        if last.hash == Some(hash) {
            return size;
        }
        last.hash = Some(hash);
        found_blind(last);
        self.detile(tiled);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
        size
    }

    /// A refresh while the runtime is remapping title memory ([`no_wait`]):
    /// the texture is read through the kernel, which fails if a page of it
    /// is not there at this moment. Then a staging buffer that was filled
    /// before keeps what it holds and the texture is looked at again at its
    /// next refresh (what was learnt about writes to it is forgotten, so
    /// that it is not passed over as unwritten); one never filled takes the
    /// pages that are there ([`Staging::fill`]). Returns the bytes read.
    fn refresh_unlocked(&self, last: &mut Last, size: usize) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        let read = with_read(size, |read| {
            if !crate::host::read_into(self.memory, read) {
                return false;
            }
            let hash = digest(read);
            if last.hash != Some(hash) {
                last.hash = Some(hash);
                self.detile(read);
            }
            true
        });
        if read {
            UNLOCKED[0].fetch_add(1, Relaxed);
            size
        } else if last.hash.is_none() && fill_unlocked() {
            self.fill(last, size)
        } else {
            UNLOCKED[1].fetch_add(1, Relaxed);
            last.asked = 0;
            0
        }
    }

    /// The first filling of the staging buffer while the runtime is
    /// remapping title memory, of a texture not all of which can be read:
    /// with the pages that are there, zeros for the others, as a partly
    /// mapped texture is filled under the lock. Pages that are away only
    /// for the moment of a change (the runtime unmaps the rest of a view to
    /// change one page of it) are read when that change is over. If less
    /// was read than was mapped when the upload was recorded, or is mapped
    /// now, the texture's slots are looked at again.
    fn fill(&self, last: &mut Last, size: usize) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        let start = self.memory as u64;
        let mut there = 0;
        let got = with_read(size, |read| {
            let mut got = 0;
            for _ in 0..4 {
                got = crate::host::read_there(self.memory, read) as u64;
                there = mapped_bytes(self.memory, size);
                if got >= there.max(self.expected.load(Relaxed)) {
                    break;
                }
                crate::watch::change_over();
            }
            if got != 0 {
                last.hash = Some(digest(read));
                self.detile(read);
            }
            got
        });
        last.partial = Some(crate::frame::current());
        UNLOCKED[2].fetch_add(1, Relaxed);
        if got < there.max(self.expected.load(Relaxed)) {
            UNLOCKED[3].fetch_add(1, Relaxed);
            last.asked = 0;
            crate::descriptor::host::read_short(start);
        } else {
            crate::descriptor::host::read_all(start);
        }
        size
    }

    /// Detiles `tiled` (the texture's memory) into the staging buffer.
    fn detile(&self, tiled: &[u8]) {
        self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        self.changed.store(true, std::sync::atomic::Ordering::Relaxed);
        // Levels on demand: which levels are the title's is judged here,
        // once, for what the staging buffer holds from now on. The step
        // that gives levels their memory goes by this answer.
        let held = if self.on_demand {
            use std::sync::atomic::Ordering::Relaxed;
            let judged = self.judgement();
            self.judged[0].store(judged[0], Relaxed);
            self.judged[1].store(judged[1], Relaxed);
            let held = self.held_levels(judged);
            self.detiled.store(held, Relaxed);
            held
        } else {
            u32::MAX
        };
        #[cfg(feature = "diagnostics")]
        if tiled.iter().all(|&b| b == 0) {
            tracing::debug!(
                memory = ?self.memory,
                desc = ?self.layout.desc,
                "a sampled texture's memory is all zero (not streamed yet, or a failed BCPack read)"
            );
        }
        for (i, (footprint, &rows)) in self.footprints.iter().zip(&self.rows).enumerate() {
            let (mip, slice) = self.part(i);
            let f = footprint.Footprint;
            let pitch = f.RowPitch as usize;
            // The last row of the last subresource is not padded to its pitch
            // (VIctory 4fc9d70): to the end of the buffer at most, where a
            // block of the pool goes on with another texture's.
            let len = (pitch * rows as usize * f.Depth as usize)
                .min(self.bytes.saturating_sub(footprint.Offset) as usize);
            // SAFETY: the subresource's part of the mapped staging buffer.
            let linear = unsafe {
                std::slice::from_raw_parts_mut(self.data.add(footprint.Offset as usize), len)
            };
            if mip < 32 && held & (1 << mip) == 0 {
                // Not the title's: zeros, not what lies in that memory.
                linear.fill(0);
                continue;
            }
            if let Err(e) =
                self.layout
                    .detile(tiled, mip, slice, linear, pitch, pitch * rows as usize)
            {
                tracing::warn!(?e, mip, slice, "texture not detiled");
                return;
            }
        }
    }
}

/// Lets go of a title texture's staging buffer (lists that recorded an
/// upload from it hold it until they have run); the next upload makes one
/// anew.
pub(crate) fn unstage(resource: This) {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = map.as_mut()
        && map.contains_key(&(resource as usize))
    {
        staging_moved();
        map.remove(&(resource as usize));
    }
}

/// Where a staged title texture's memory is: address and bytes.
pub(crate) fn staged_memory(resource: This) -> Option<(u64, u64)> {
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let (at, len) = map.as_ref()?.get(&(resource as usize))?.memory();
    Some((at as u64, len as u64))
}

/// A fingerprint of the title memory `[at, at + len)`, never 0: of all of
/// it up to 48 KiB, else of 16 KiB at its start, its middle and its end
/// (a texture streams in whole mips; the small ones lie first). Read
/// through the kernel, so pages not there count as zeros.
pub(crate) fn fingerprint(at: u64, len: u64) -> u64 {
    const WINDOW: usize = 16 << 10;
    thread_local! {
        static READ: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    let len = len as usize;
    let mut print = 0u128;
    READ.with(|read| {
        let mut read = read.borrow_mut();
        read.resize(3 * WINDOW, 0);
        let mut take = |from: usize, bytes: usize| {
            read[..bytes].fill(0);
            crate::host::copy_mapped(read.as_mut_ptr(), (at as usize + from) as *const u8, bytes, false);
            print = print.rotate_left(37) ^ digest(&read[..bytes]).0;
        };
        if len <= 3 * WINDOW {
            take(0, len);
        } else {
            take(0, WINDOW);
            take((len / 2) & !0xfff, WINDOW);
            take(len - WINDOW, WINDOW);
        }
    });
    let first = (print as u64 ^ (print >> 64) as u64).to_le_bytes();
    u64::from_le_bytes(first) | 1
}

/// Whether a title texture has a staging buffer: a list recorded its
/// upload, and the lists that sample it check it at submission.
/// For the frame listing: whether a list has uploaded `resource`'s staging
/// buffer, and whether it was detiled since. None: it has none.
#[cfg(feature = "diagnostics")]
pub(crate) fn staged_state(resource: This) -> Option<(bool, bool)> {
    use std::sync::atomic::Ordering::Relaxed;
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let staging = map.as_ref()?.get(&(resource as usize))?;
    Some((staging.uploaded.load(Relaxed), staging.dirty.load(Relaxed)))
}

/// The first level a title texture's staging buffer holds; `u32::MAX`
/// without one.
fn staged_from(resource: This) -> u32 {
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    map.as_ref()
        .and_then(|map| map.get(&(resource as usize)))
        .map_or(u32::MAX, |s| s.first)
}

/// Whether the levels from `first` on of `resource`'s host copy were filled
/// from the bytes title memory holds now: an upload would bring nothing new.
/// (Of 787 repeated uploads of a frame that streams 398 were equal in every
/// byte, and a texture uploaded again costs a staging buffer, a detiling and
/// a copy more than this look.) `VICTORY_TEXTURE_SAME=0`: not asked.
pub(crate) fn unchanged(resource: This, first: u32) -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_SAME").is_none_or(|v| v != "0")) {
        return false;
    }
    let Some(copy) = crate::resource::made_copy(resource) else {
        return false;
    };
    let Some((from, held)) = *copy.holds.lock().unwrap_or_else(|e| e.into_inner()) else {
        return false;
    };
    let Some((memory, desc)) = crate::resource::texture_memory(resource) else {
        return false;
    };
    let Some(layout) = crate::resource::texture_layout(&desc) else {
        return false;
    };
    let layout = &layout.planes[0].layout;
    if from > crate::resource::read_from(layout, first).max(copy.base) {
        return false;
    }
    let size = layout.bytes_from(from) as usize;
    with_read(size, |read| {
        if crate::host::read_there(memory, read) == 0 {
            return false;
        }
        digest(read) == held
    })
}

/// Where the levels from `first` on of a title texture are: address and
/// bytes.
pub(crate) fn title_span(resource: This, first: u32) -> Option<(u64, u64)> {
    let (memory, desc) = crate::resource::texture_memory(resource)?;
    let layout = crate::resource::texture_layout(&desc)?;
    let layout = &layout.planes[0].layout;
    Some((memory as u64, layout.bytes_from(crate::resource::read_from(layout, first))))
}

pub(crate) fn is_staged(resource: This) -> bool {
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    map.as_ref().is_some_and(|map| map.contains_key(&(resource as usize)))
}

/// The bytes of a title texture's memory, and whether all of it is mapped,
/// if the texture has been staged for upload.
/// (Whether it is mapped is asked only if `ask`; else it counts as mapped.)
pub(crate) fn staged(resource: This, ask: bool) -> Option<(u64, bool)> {
    let s = {
        let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        map.as_ref()?.get(&(resource as usize))?.clone()
    };
    Some((s.read as u64, !ask || committed(s.memory, s.read)))
}

/// Whether `[at, at + len)` is committed memory (streamed textures may be
/// unmapped until their pages arrive).
pub(crate) fn committed(at: *const u8, len: usize) -> bool {
    if crate::watch::soft().is_some() {
        // Views the runtime keeps in its books alone (`VICTORY_SOFT_VIEWS`)
        // are committed as far as the title goes, and stay as they are;
        // Windows is asked about what lies between them.
        let runs = crate::watch::soft_runs(at as usize, len);
        if runs.is_empty() {
            return committed_there(at, len);
        }
        let (mut from, end) = (at as usize, at as usize + len);
        for (run, bytes) in runs {
            if run > from && !committed_there(from as *const u8, run - from) {
                return false;
            }
            from = run + bytes;
        }
        return from >= end || committed_there(from as *const u8, end - from);
    }
    // Memory the runtime keeps in its books alone (`VICTORY_LAZY_VIEWS`) is
    // committed as far as the title goes: made a mapping before Windows is
    // asked.
    crate::watch::materialize(at as usize, len);
    committed_there(at, len)
}

/// [`committed`], of memory the host has mapped or has not.
fn committed_there(at: *const u8, len: usize) -> bool {
    if by_query() {
        return committed_by_query(at, len);
    }
    if len == 0 {
        return true;
    }
    let (there, of) = readable(at as usize, len, true);
    there == of
}

/// Whether the two questions above and below are put to `VirtualQuery`, as
/// they were (`VICTORY_COMMITTED_QUERY=1`). `VirtualQuery` answers with the
/// length of the run of pages in the same state, which it finds by walking
/// the page tables to the run's end: milliseconds for one question in a
/// block of a gigabyte. The pass over the heap's views asks three or more
/// of them for every texture slot that is due, thousands of slots a frame
/// when the card has no room: that pass then ran hundreds of milliseconds,
/// and while it ran the submitting thread's write-watch askings waited
/// behind the walks.
fn by_query() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_COMMITTED_QUERY").is_some_and(|v| v == "1"))
}

/// Of the kernel's pages that hold `[at, at + len)` (`len > 0`): how many
/// can be read, and how many there are; with `all`, it stops at the first
/// that cannot. One question to the kernel for up to 256 pages at once
/// (`QueryWorkingSetEx`: the state of each page asked about, no walk); a
/// page not in the working set may still be there (never touched, or
/// trimmed), and a one-byte read through the kernel says.
/// A page that is committed and may not be read counts as not there, which
/// is what the callers ask for: they read or write the memory next.
fn readable(at: usize, len: usize, all: bool) -> (usize, usize) {
    use windows_sys::Win32::System::ProcessStatus::{K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    const KERNEL_PAGE: usize = 4096;
    let (first, last) = (at / KERNEL_PAGE, at.saturating_add(len - 1) / KERNEL_PAGE);
    let of = last - first + 1;
    let mut there = 0;
    // SAFETY: plain data.
    let mut info: [PSAPI_WORKING_SET_EX_INFORMATION; 256] = unsafe { std::mem::zeroed() };
    let mut page = first;
    while page <= last {
        let n = (last - page + 1).min(info.len());
        for (i, entry) in info[..n].iter_mut().enumerate() {
            entry.VirtualAddress = ((page + i) * KERNEL_PAGE) as *mut _;
        }
        // SAFETY: `n` entries of the size given; our own process.
        let asked = unsafe {
            K32QueryWorkingSetEx(
                GetCurrentProcess(),
                info.as_mut_ptr().cast(),
                (n * size_of::<PSAPI_WORKING_SET_EX_INFORMATION>()) as u32,
            )
        } != 0;
        for (i, entry) in info[..n].iter().enumerate() {
            // Bit 0 of the answer: the page is in the working set.
            // SAFETY: plain data the query filled in.
            let valid = asked && unsafe { entry.VirtualAttributes.Flags } & 1 != 0;
            let mut byte = [0u8];
            if valid || crate::host::read_into(((page + i) * KERNEL_PAGE) as *const u8, &mut byte) {
                there += 1;
            } else if all {
                return (there, of);
            }
        }
        page += n;
    }
    (there, of)
}

fn committed_by_query(at: *const u8, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    let mut p = at as usize;
    let end = p + len;
    while p < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
        if n == 0 || info.State != MEM_COMMIT {
            return false;
        }
        p = info.BaseAddress as usize + info.RegionSize;
    }
    true
}

/// How many bytes of `[at, at + len)` are committed memory: all of a
/// texture that is there, some of one streaming in (its smaller mips lie
/// at the start of its memory, and their pages are mapped first), none of
/// one not there. With neither the first nor the last page committed the
/// answer is none without a look at the pages between (two queries for the
/// slots whose texture is away, which are asked again and again).
pub(crate) fn mapped_bytes(at: *const u8, len: usize) -> u64 {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    if len == 0 {
        return 0;
    }
    let start = at as usize;
    let end = start.saturating_add(len);
    // Streamed textures lie in views of physical pages, of which the runtime
    // knows which are there: one look, where the host was asked for every
    // 64 KiB of the texture (and once more for each that is not there).
    // `VICTORY_MAPPED_BY_VIEWS=0`: the host is asked, as before.
    static BY_VIEWS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *BY_VIEWS.get_or_init(|| std::env::var_os("VICTORY_MAPPED_BY_VIEWS").is_none_or(|v| v != "0"))
        && let Some(there) = crate::inplace::shown_bytes(start, len)
    {
        return there;
    }
    if !committed(at, 1) && !committed((end - 1) as *const u8, 1) {
        return 0;
    }
    if !by_query() {
        static BY_PAGE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *BY_PAGE.get_or_init(|| std::env::var_os("VICTORY_MAPPED_BY_PAGE").is_some_and(|v| v == "1")) {
            // By page, without the walk: whole pages, cut to the range.
            let (there, _) = readable(start, len, false);
            return (there as u64 * 4096).min(len as u64);
        }
        // By 64 KiB block, one page asked of each: the title's memory is
        // mapped in blocks of that size (the console's page), and the
        // answer is a measure of how much of a texture has arrived, not a
        // promise that every byte can be read (the upload reads through the
        // kernel). By page, a texture half there cost a failed kernel read
        // for each of its missing pages, every frame it stayed due: over a
        // thousand textures "uploaded in part" kept the pass over the heap's
        // views at a hundred milliseconds a frame and more.
        // `VICTORY_MAPPED_BY_PAGE=1`: by page.
        const BLOCK: usize = 64 << 10;
        let mut there = 0u64;
        let mut block = start & !(BLOCK - 1);
        while block < end {
            let (lo, hi) = (block.max(start), block.saturating_add(BLOCK).min(end));
            if readable(lo, 1, true).0 == 1 {
                there += (hi - lo) as u64;
            }
            block = block.saturating_add(BLOCK);
        }
        return there;
    }
    let (mut p, mut there) = (start, 0u64);
    while p < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
        let next = (info.BaseAddress as usize).saturating_add(info.RegionSize).min(end);
        if n == 0 || next <= p {
            break;
        }
        if info.State == MEM_COMMIT {
            there += (next - p) as u64;
        }
        p = next;
    }
    there
}

/// The staging state of a title texture, made on first use.
fn staging(resource: This, copy: &crate::resource::HostTexture, first: u32) -> Option<Arc<Staging>> {
    staging_by(resource, copy, first, crate::resource::texture_at_shared())
}

/// [`staging`]; `quick`: what this thread was last given for the texture is
/// taken without the table's lock while the table has not changed
/// (`crate::resource::texture_at_shared`; `false`: under the lock every
/// time, as before).
fn staging_by(resource: This, copy: &crate::resource::HostTexture, first: u32, quick: bool) -> Option<Arc<Staging>> {
    let texture = &copy.texture;
    // (The table's own entry for the texture: good for this ask by the same
    // rule as below, or the table is asked and makes another.)
    if quick
        && let Some(s) = staging_found(resource as usize)
        && s.first <= first.max(copy.base)
        && s.serial == copy.serial
    {
        return Some(s);
    }
    // Asked before the table is locked: the sweep lets go of a staging
    // buffer with its texture's host side locked, in that order.
    let on_demand = crate::resource::host_tiled(resource).is_some();
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(HashMap::new);
    if let Some(s) = map.get(&(resource as usize))
        && s.first <= first.max(copy.base)
        && s.serial == copy.serial
    {
        if quick {
            staging_remember(resource as usize, STAGING_CHANGED.load(std::sync::atomic::Ordering::SeqCst), s);
        }
        return Some(s.clone());
    }
    let (memory, desc) = crate::resource::texture_memory(resource)?;
    let Some(layout) = crate::resource::texture_layout(&desc).map(|t| t.planes[0].layout.clone())
    else {
        once!(
            ?desc,
            "texture contents without a console layout not uploaded"
        );
        return None;
    };
    if layout.desc.swizzle.xor() {
        once!(?desc, "texture contents in an XOR mode not uploaded");
        return None;
    }
    let host = host()?;
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let level0 = copy.base;
    let mips = level0 + u32::from(d.MipLevels);
    let first = crate::resource::read_from(&layout, first).max(level0).min(mips - 1);
    let read = layout.bytes_from(first) as usize;
    let count = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        mips - first
    } else {
        (mips - first) * u32::from(d.DepthOrArraySize)
    };
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
    let mut rows = vec![0u32; count as usize];
    let mut total = 0;
    // SAFETY: outputs for `count` subresources.
    unsafe {
        host.device.GetCopyableFootprints(
            &d,
            first - level0,
            count,
            0,
            Some(footprints.as_mut_ptr()),
            Some(rows.as_mut_ptr()),
            None,
            Some(&mut total),
        );
    }
    let (piece, buffer, base, data) = match crate::pool::staging(host, total) {
        Some((piece, buffer, base, data)) => (Some(piece), buffer, base, data),
        None => {
            // After upstream GPL-3.0 fork, commits 18bdf7932b79 "Scope bindless
            // refreshes to their heap and bound speculative staging" and ab75ac3e02ba "Back off refused GPU pool
            // growth and defer uploads under low commit": no buffer of its own while an allocation was just
            // refused, nor for a texture picked ahead of its use (not a bake) while commit is short; the caller
            // finds the texture later. Here: `crate::host::commit` (`VICTORY_COMMIT_GUARD`), which lets everything
            // through when it is off.
            let speculative = PICKING.get().is_some() && !BAKING.get();
            if crate::host::commit::upload_refused()
                || (speculative && !crate::host::commit::allow_staging_growth(false))
            {
                crate::host::commit::note_staging_deferred();
                return None;
            }
            let (buffer, data) = host.upload_buffer(total)?;
            crate::host::name(&buffer, || {
                format!(
                    "staging of {resource:?} {}x{}x{} f{}",
                    d.Width, d.Height, d.DepthOrArraySize, d.Format.0
                )
            });
            #[cfg(feature = "diagnostics")]
            crate::book::tag(&buffer, "staging of textures, of their own");
            (None, buffer, 0, data)
        }
    };
    tracing::debug!(?desc, total, "texture contents uploaded from title memory");
    STAGED[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    STAGED[1].fetch_add(total, std::sync::atomic::Ordering::Relaxed);
    let s = Arc::new(Staging {
        buffer,
        base,
        _piece: piece,
        #[cfg(feature = "diagnostics")]
        size: total,
        data,
        footprints,
        rows,
        layout,
        mips,
        first,
        level0,
        read,
        serial: copy.serial,
        holds: copy.holds.clone(),
        once: std::sync::atomic::AtomicBool::new(once() && !on_demand && PICKING.get().is_some()),
        bytes: total,
        memory,
        last: Mutex::default(),
        dirty: std::sync::atomic::AtomicBool::new(true),
        uploaded: std::sync::atomic::AtomicBool::new(false),
        changed: std::sync::atomic::AtomicBool::new(false),
        streak: std::sync::atomic::AtomicU8::new(0),
        used: std::sync::atomic::AtomicU64::new(crate::host::moment()),
        expected: std::sync::atomic::AtomicU64::new(0),
        on_demand,
        detiled: std::sync::atomic::AtomicU32::new(0),
        judged: [const { std::sync::atomic::AtomicU64::new(0) }; 2],
        waiting: std::sync::atomic::AtomicU64::new(0),
        exact: std::sync::atomic::AtomicBool::new(BAKING.get()),
        planed: std::sync::atomic::AtomicBool::new(false),
    });
    // (Another may have been there, for other levels or another host copy.)
    staging_moved();
    map.insert(resource as usize, s.clone());
    if quick {
        staging_remember(resource as usize, STAGING_CHANGED.load(std::sync::atomic::Ordering::SeqCst), &s);
    }
    Some(s)
}

#[cfg(test)]
mod title_memory_tests {
    use super::with_title;

    /// Title memory whose tail is reserved and not committed reads through
    /// the copy as its bytes and then zeros, without a fault; so does
    /// memory a page of which is taken away between two reads, and the
    /// zeros are zeros whatever the read buffer held before.
    #[test]
    fn an_uncommitted_tail_reads_as_zeros() {
        use windows_sys::Win32::System::Memory::*;
        const HEAD: usize = 0xb_0000;
        const SIZE: usize = 0x20_0000;
        // SAFETY: a private reservation of our own; only committed pages
        // are written in place, and nothing else reads them in place.
        unsafe {
            // The read buffer holds something else first.
            let other = vec![0x77u8; SIZE];
            with_title(other.as_ptr(), SIZE, |bytes| assert!(bytes.iter().all(|&b| b == 0x77)));
            let base = VirtualAlloc(std::ptr::null(), SIZE, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            assert_eq!(VirtualAlloc(base.cast(), HEAD, MEM_COMMIT, PAGE_READWRITE), base.cast());
            base.write_bytes(0x5a, HEAD);
            with_title(base, SIZE, |bytes| {
                assert!(bytes[..HEAD].iter().all(|&b| b == 0x5a), "what is mapped");
                assert!(bytes[HEAD..].iter().all(|&b| b == 0), "what is not: zeros");
            });
            // The title takes a page in the middle away: zeros from then
            // on, the rest as before.
            assert_ne!(VirtualFree(base.add(0x4_0000).cast(), 0x1_0000, MEM_DECOMMIT), 0);
            with_title(base, SIZE, |bytes| {
                assert!(bytes[..0x4_0000].iter().all(|&b| b == 0x5a));
                assert!(bytes[0x4_0000..0x5_0000].iter().all(|&b| b == 0));
                assert!(bytes[0x5_0000..HEAD].iter().all(|&b| b == 0x5a));
                assert!(bytes[HEAD..].iter().all(|&b| b == 0));
            });
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }

    /// A texture device-loss runs named last (16 texels wide, 4 or 8 high,
    /// format 10, layout 0x100; its memory began 0x300 or 0x700 into a page,
    /// and here also 0xf00, so that a page line runs through it): what the
    /// detiling is
    /// given and what it writes are the same read in place and read through
    /// the copy, and the rows it writes end where the staging buffer the
    /// host sizes for it ends (896 bytes for 4 rows of 128 bytes 256 apart,
    /// 1,920 for 8).
    #[test]
    fn the_small_texture_of_runs_72_and_73_detiles_the_same_both_ways() {
        use windows_sys::Win32::System::Memory::*;
        for (height, within) in [(4u32, 0x300usize), (8, 0x700), (4, 0xf00), (8, 0xf00)] {
            // The description as the log printed it, word for word.
            let words: [u32; 14] = [3, 0, 0, 0, 16, 0, height, 65537, 10, 1, 0, 256, 3_145_728, 0];
            // SAFETY: fourteen words are a resource description (56 bytes,
            // plain numbers).
            let desc: crate::resource::ResourceDesc = unsafe { std::mem::transmute(words) };
            let layout = crate::resource::texture_layout(&desc)
                .expect("a console layout")
                .planes[0]
                .layout
                .clone();
            let size = layout.size as usize;
            let (row, pitch) = (16 * 8, 256usize);
            assert!(size >= row * height as usize, "the layout holds every texel");
            // Committed pages and a reserved one after them; the texture
            // begins `within` bytes into the first, as the title placed it.
            // SAFETY: our own reservation; the committed part is written
            // and read in place, by this thread alone.
            unsafe {
                let pages = size.div_ceil(4096) + 1;
                let base = VirtualAlloc(std::ptr::null(), (pages + 1) * 4096, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
                assert!(!base.is_null());
                assert_eq!(VirtualAlloc(base.cast(), pages * 4096, MEM_COMMIT, PAGE_READWRITE), base.cast());
                for i in 0..pages * 4096 {
                    base.add(i).write((i * 7 + i / 251) as u8);
                }
                let memory = base.add(within);
                let in_place = std::slice::from_raw_parts(memory, size);
                // The bytes: the same in place and through the copy.
                with_title(memory, size, |copied| assert_eq!(copied, in_place));
                // What the detiling writes: the same, and inside the bytes
                // the host's staging buffer for this texture has.
                let staged = (height as usize - 1) * pitch + row;
                assert_eq!(staged, if height == 4 { 896 } else { 1920 });
                let detile = |tiled: &[u8]| {
                    // The slice `Staging::detile` hands over: whole rows.
                    let mut linear = vec![0xeeu8; pitch * height as usize];
                    layout
                        .detile(tiled, 0, 0, &mut linear, pitch, pitch * height as usize)
                        .expect("detiled");
                    linear
                };
                let direct = detile(in_place);
                let through = with_title(memory, size, |copied| detile(copied));
                assert_eq!(direct, through);
                for y in 0..height as usize {
                    assert!(direct[y * pitch..][..row].iter().any(|&b| b != 0xee), "row {y} written");
                    assert!(direct[y * pitch + row..(y + 1) * pitch].iter().all(|&b| b == 0xee), "nothing past row {y}");
                }
                // A buffer of exactly the staged bytes is enough.
                let mut exact = vec![0u8; staged];
                layout
                    .detile(in_place, 0, 0, &mut exact, pitch, pitch * height as usize)
                    .expect("the staging buffer's bytes are enough");
                assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
            }
        }
    }
}

/// [`claim_upload`] (`VICTORY_PRELUDE_UPLOADS=1`; ported from
/// trustno1-coder/VIctory-NVIDIA 91df493, from Belobug/VIctory-Belobug-AMD
/// 5d6bd84). Off, the uploads are decided as before and none of this runs.
#[cfg(test)]
mod levels_below_tests {
    use super::levels_below;

    /// The first upload into a copy cut from a block, from a later level
    /// than its first, writes zeros into the levels before that one in every
    /// array slice (`crate::resource::show_written`): those subresources.
    #[test]
    fn the_levels_an_upload_leaves_out_are_named_in_every_slice() {
        assert!(levels_below(0, 10, 1).is_empty());
        assert_eq!(levels_below(2, 10, 1), [0, 1]);
        assert_eq!(levels_below(2, 10, 3), [0, 1, 10, 11, 20, 21]);
        // Never past the copy's own levels.
        assert_eq!(levels_below(12, 4, 1), [0, 1, 2, 3]);
        assert!(levels_below(3, 10, 0).is_empty());
    }
}

#[cfg(test)]
mod prelude_upload_tests {
    use super::{claim_upload, unclaim_upload};
    use std::sync::atomic::{AtomicBool, Ordering::Release};

    /// The first upload is claimed, a second list of the submission skips,
    /// an unchanged texture skips, a changed one is claimed in the same
    /// submission, every time it changes.
    #[test]
    fn first_upload_clean_skip_and_reupload_in_the_same_submission() {
        let uploaded = AtomicBool::new(false);
        let dirty = AtomicBool::new(true);
        assert_eq!(claim_upload(&uploaded, &dirty), Some(true));
        assert_eq!(claim_upload(&uploaded, &dirty), None);
        assert_eq!(claim_upload(&uploaded, &dirty), None);
        dirty.store(true, Release);
        assert_eq!(claim_upload(&uploaded, &dirty), Some(false));
        dirty.store(true, Release);
        assert_eq!(claim_upload(&uploaded, &dirty), Some(false));
    }

    /// A claim taken back (the list was left out for want of memory) is
    /// claimed again by the next list, as a first upload if it was one.
    #[test]
    fn a_claim_taken_back_is_claimed_again() {
        let uploaded = AtomicBool::new(false);
        let dirty = AtomicBool::new(false);
        assert_eq!(claim_upload(&uploaded, &dirty), Some(true));
        unclaim_upload(&uploaded, &dirty, true);
        assert_eq!(claim_upload(&uploaded, &dirty), Some(true));
        assert_eq!(claim_upload(&uploaded, &dirty), None);
        dirty.store(true, Release);
        assert_eq!(claim_upload(&uploaded, &dirty), Some(false));
        unclaim_upload(&uploaded, &dirty, false);
        assert_eq!(claim_upload(&uploaded, &dirty), Some(false));
    }

    /// A ring of three buffers written in turn (a video's) never shows a
    /// picture older than the one its buffer holds, a held picture
    /// included.
    #[test]
    fn a_ring_of_three_buffers_never_shows_an_older_picture() {
        let ring: [(AtomicBool, AtomicBool); 3] = std::array::from_fn(|_| (AtomicBool::new(false), AtomicBool::new(true)));
        let mut shown = [0u32; 3];
        let schedule = [
            (0, 1, true),
            (1, 2, true),
            (2, 3, true),
            (0, 4, true),
            (1, 5, true),
            (1, 5, false),
            (2, 6, true),
            (0, 7, true),
            (1, 8, true),
            (2, 9, true),
            (0, 10, true),
            (1, 11, true),
        ];
        for (slot, picture, changed) in schedule {
            let (uploaded, dirty) = &ring[slot];
            if changed {
                dirty.store(true, Release);
            }
            if claim_upload(uploaded, dirty).is_some() {
                shown[slot] = picture;
            }
            assert_eq!(shown[slot], picture, "slot {slot} shows picture {} instead of {picture}", shown[slot]);
        }
    }
}

/// How long this PC's card takes to make the allocations a staged texture needs, for
/// `cargo test -p victory-gpu allocation_costs -- --ignored --nocapture`.
#[cfg(test)]
mod allocation_costs {
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::*;

    fn texture(size: u32) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: u64::from(size),
            Height: size,
            DepthOrArraySize: 1,
            MipLevels: 0,
            Format: DXGI_FORMAT_BC7_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        }
    }

    fn buffer(size: u64) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        }
    }

    fn committed(device: &ID3D12Device, kind: D3D12_HEAP_TYPE, flags: D3D12_HEAP_FLAGS, desc: &D3D12_RESOURCE_DESC) -> ID3D12Resource {
        let heap = D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() };
        let state = if kind == D3D12_HEAP_TYPE_UPLOAD { D3D12_RESOURCE_STATE_GENERIC_READ } else { D3D12_RESOURCE_STATE_COMMON };
        let mut out = None;
        // SAFETY: plain creation.
        unsafe { device.CreateCommittedResource(&heap, flags, desc, state, None, &mut out) }.expect("committed");
        out.unwrap()
    }

    fn timed(what: &str, n: usize, threads: usize, make: &(dyn Fn(usize) -> ID3D12Resource + Sync)) {
        let began = std::time::Instant::now();
        let all: Vec<_> = std::thread::scope(|s| {
            (0..threads)
                .map(|t| s.spawn(move || (t..n).step_by(threads).map(make).collect::<Vec<_>>()))
                .collect::<Vec<_>>()
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect()
        });
        let took = began.elapsed().as_secs_f64() * 1e3;
        println!("{what}, {threads} thread(s): {n} in {took:.1} ms, {:.0} us each", took * 1e3 / n as f64);
        drop(all);
    }

    /// What a look at how much of a texture is there costs: by questions to
    /// the host, and by the runtime's record of its views.
    #[test]
    #[ignore = "a measurement, not a check"]
    fn look_costs() {
        let _views = crate::test_views_lock();
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
        const BLOCK: usize = 64 << 10;
        for (name, blocks, there) in [("350 KB, all there", 6, 6), ("22 MB, all there", 350, 350), ("22 MB, 1 MB there", 350, 16)] {
            let make = || {
                // SAFETY: a reservation of our own, of which the first part
                // is committed and touched.
                unsafe {
                    let at = VirtualAlloc(std::ptr::null(), blocks * BLOCK, MEM_RESERVE, PAGE_READWRITE);
                    VirtualAlloc(at, there * BLOCK, MEM_COMMIT, PAGE_READWRITE);
                    std::ptr::write_bytes(at.cast::<u8>(), 1, there * BLOCK);
                    at as usize
                }
            };
            let (asked, noted) = (make(), make());
            for block in 0..there {
                crate::title_memory_view(noted + block * BLOCK, BLOCK, 9, 0);
            }
            for (how, at) in [("asking the host", asked), ("by the views", noted)] {
                const N: u32 = 200;
                let began = std::time::Instant::now();
                for _ in 0..N {
                    assert_eq!(super::mapped_bytes(at as *const u8, blocks * BLOCK), (there * BLOCK) as u64);
                }
                println!("{name}, {how}: {:.1} us a look", began.elapsed().as_secs_f64() * 1e6 / f64::from(N));
            }
            crate::inplace::views_gone(noted, blocks * BLOCK);
        }
    }

    /// The digest tells every length and every changed byte apart, zeros
    /// among them.
    #[test]
    fn a_digest_tells_bytes_apart() {
        use super::digest;
        let zeros = [0u8; 400];
        let mut seen: Vec<super::Digest> = (0..400).map(|n| digest(&zeros[..n])).collect();
        for at in 0..400 {
            for value in [1u8, 0x80, 0xff] {
                let mut bytes = zeros;
                bytes[at] = value;
                seen.push(digest(&bytes));
            }
        }
        let text: Vec<u8> = (0..400u32).map(|i| ((i * 37) >> 2) as u8).collect();
        for at in 0..399 {
            let mut bytes = text.clone();
            bytes.swap(at, at + 1);
            if bytes != text {
                seen.push(digest(&bytes));
            }
        }
        seen.push(digest(&text));
        let n = seen.len();
        seen.sort_unstable_by_key(|d| d.0);
        seen.dedup();
        assert_eq!(seen.len(), n);
        assert_eq!(digest(&text), digest(&text.clone()));
    }

    /// What a texture met for the first time costs, by step, in
    /// milliseconds a megabyte on one thread: for whether a store of
    /// detiled copies on disk could be worth it (its key is a hash of the
    /// title's bytes, so reading and hashing them stay; `VICTORY_BENCH_FILE`
    /// is a big file to read past the system's cache).
    #[test]
    #[ignore = "a measurement, not a check"]
    fn cold_texture_costs() {
        let Some(host) = crate::host::host() else { return };
        let desc = [3u32, 0, 0, 0, 1024, 0, 1024, 0xb_0001, 98, 1, 0, 0x105, 0, 0];
        // SAFETY: 56 bytes in the description's layout.
        let desc = unsafe { desc.as_ptr().cast::<crate::resource::ResourceDesc>().read_unaligned() };
        let layout = crate::resource::texture_layout(&desc).unwrap().planes[0].layout.clone();
        let size = layout.size as usize;
        let title: Vec<u8> = (0..size).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        let (_buffer, up) = host.upload_buffer(2 * layout.size).unwrap();
        // SAFETY: the mapping.
        let up = unsafe { std::slice::from_raw_parts_mut(up, 2 * size) };
        let mut read = vec![0u8; size];
        const N: u32 = 200;
        let per_mb = |t: std::time::Duration| t.as_secs_f64() * 1e3 / (f64::from(N) * size as f64 / 1e6);
        let timed = |what: &str, f: &mut dyn FnMut()| {
            f();
            let started = std::time::Instant::now();
            (0..N).for_each(|_| f());
            println!("{what}: {:.3} ms/MB", per_mb(started.elapsed()));
        };
        timed("read through the kernel or a safe copy", &mut || {
            std::hint::black_box(crate::host::read_into(title.as_ptr(), &mut read));
        });
        timed("blake3", &mut || {
            std::hint::black_box(blake3::hash(&title));
        });
        timed("digest", &mut || {
            std::hint::black_box(super::digest(&title));
        });
        timed("detiled into upload memory", &mut || {
            let mut at = 0;
            for (mip, m) in layout.mips.iter().enumerate() {
                let pitch = (m.width as usize * 16).next_multiple_of(256);
                let len = pitch * m.height as usize;
                layout.detile(&title, mip as u32, 0, &mut up[at..at + len], pitch, len).unwrap();
                at += len.next_multiple_of(512);
            }
        });
        timed("copied into upload memory", &mut || up[..size].copy_from_slice(&title));
        if let Some(file) = std::env::var_os("VICTORY_BENCH_FILE") {
            use std::os::windows::fs::{FileExt, OpenOptionsExt};
            const NO_BUFFERING: u32 = 0x2000_0000;
            let file = std::fs::OpenOptions::new().read(true).custom_flags(NO_BUFFERING).open(file).unwrap();
            let len = file.metadata().unwrap().len();
            let mut block = vec![0u8; size + 4096];
            let skew = block.as_ptr().align_offset(4096);
            let n = size / 4096 * 4096;
            let mut at = 0u64;
            timed("read from the disk, one request at a time", &mut || {
                at = (at + 7919 * 4096 * 64) % (len - n as u64) / 4096 * 4096;
                file.seek_read(&mut block[skew..skew + n], at).unwrap();
            });
        }
    }

    #[test]
    #[ignore = "a measurement, not a check"]
    fn allocation_costs() {
        let Some(host) = crate::host::host() else { return };
        let device = &host.device;
        let not_zeroed = D3D12_HEAP_FLAG_CREATE_NOT_ZEROED;
        const N: usize = 1000;
        for size in [128u32, 512, 1024] {
            let (t, b) = (texture(size), buffer(u64::from(size * size) * 4 / 3 + 65536));
            println!("-- BC7 {size}x{size}");
            for threads in [1, 4, 8] {
                timed("texture committed", N, threads, &|_| committed(device, D3D12_HEAP_TYPE_DEFAULT, D3D12_HEAP_FLAG_NONE, &t));
                timed("texture committed not zeroed", N, threads, &|_| committed(device, D3D12_HEAP_TYPE_DEFAULT, not_zeroed, &t));
                let mapped = |flags| {
                    let b = &b;
                    move |_| {
                        let r = committed(device, D3D12_HEAP_TYPE_UPLOAD, flags, b);
                        let mut data = std::ptr::null_mut();
                        // SAFETY: a live upload buffer.
                        unsafe { r.Map(0, None, Some(&mut data)) }.unwrap();
                        // SAFETY: the first byte of the mapping (its first touch).
                        unsafe { data.cast::<u8>().write_volatile(1) };
                        r
                    }
                };
                timed("staging committed and mapped", N, threads, &mapped(D3D12_HEAP_FLAG_NONE));
                timed("staging committed and mapped, not zeroed", N, threads, &mapped(not_zeroed));
            }
            // SAFETY: plain query.
            let info = unsafe { device.GetResourceAllocationInfo(0, &[t]) };
            let per_heap = ((64u64 << 20) / info.SizeInBytes).max(1) as usize;
            let heaps: Vec<ID3D12Heap> = (0..N.div_ceil(per_heap))
                .map(|_| {
                    let desc = D3D12_HEAP_DESC {
                        SizeInBytes: per_heap as u64 * info.SizeInBytes,
                        Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                        Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES | not_zeroed,
                        ..Default::default()
                    };
                    let mut heap = None;
                    // SAFETY: plain creation.
                    unsafe { device.CreateHeap(&desc, &mut heap) }.expect("heap");
                    heap.unwrap()
                })
                .collect();
            let place = |i: usize| {
                let mut out: Option<ID3D12Resource> = None;
                let at = (i % per_heap) as u64 * info.SizeInBytes;
                // SAFETY: a part of a live heap no other texture has.
                unsafe { device.CreatePlacedResource(&heaps[i / per_heap], at, &t, D3D12_RESOURCE_STATE_COMMON, None, &mut out) }.expect("placed");
                out.unwrap()
            };
            timed("texture placed in 64 MB heaps", N, 1, &place);
            timed("texture placed in 64 MB heaps", N, 8, &place);
        }
    }
}

#[cfg(test)]
mod barrier_tests {
    use super::*;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC};

    /// The software adapter's device, if there is one.
    fn warp() -> Option<ID3D12Device> {
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter, IDXGIFactory4};
        // SAFETY: plain DXGI queries and device creation on the software adapter; a card is never opened.
        unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
            let adapter: IDXGIAdapter = factory.EnumWarpAdapter().ok()?;
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
            device
        }
    }

    fn desc(side: u32) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: u64::from(side),
            Height: side,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        }
    }

    /// Records an aliasing barrier on `texture` (or none) into a list and closes it.
    fn closes(device: &ID3D12Device, texture: &ID3D12Resource, barrier: bool) -> windows::core::Result<()> {
        // SAFETY: objects of our own; the list is never executed.
        unsafe {
            let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
            let list: ID3D12GraphicsCommandList =
                device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).unwrap();
            if barrier {
                let aliasing = D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_ALIASING,
                    Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                    Anonymous: D3D12_RESOURCE_BARRIER_0 {
                        Aliasing: std::mem::ManuallyDrop::new(D3D12_RESOURCE_ALIASING_BARRIER {
                            pResourceBefore: std::mem::ManuallyDrop::new(None),
                            pResourceAfter: std::mem::transmute_copy(texture),
                        }),
                    },
                };
                list.ResourceBarrier(&[aliasing]);
            }
            list.Close()
        }
    }

    /// Whether an aliasing barrier on a committed texture makes `Close` fail ("a host command list did not
    /// close"), and that one on a placed texture, or none, does not.
    #[test]
    fn an_aliasing_barrier_on_a_committed_texture_and_on_a_placed_one() {
        let Some(device) = warp() else { return };
        let d = desc(256);
        let mut committed: Option<ID3D12Resource> = None;
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
        // SAFETY: plain creation.
        unsafe {
            device
                .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &d, D3D12_RESOURCE_STATE_COMMON, None, &mut committed)
                .unwrap();
        }
        let committed = committed.unwrap();
        // SAFETY: plain query and creation.
        let placed = unsafe {
            let info = device.GetResourceAllocationInfo(0, &[d]);
            let hd = D3D12_HEAP_DESC {
                SizeInBytes: info.SizeInBytes.next_multiple_of(65536),
                Properties: heap,
                Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES,
                ..Default::default()
            };
            let mut h: Option<ID3D12Heap> = None;
            device.CreateHeap(&hd, &mut h).unwrap();
            let mut p: Option<ID3D12Resource> = None;
            device.CreatePlacedResource(&h.unwrap(), 0, &d, D3D12_RESOURCE_STATE_COMMON, None, &mut p).unwrap();
            p.unwrap()
        };
        println!("committed with the barrier: {:?}", closes(&device, &committed, true));
        println!("placed with the barrier: {:?}", closes(&device, &placed, true));
        assert!(closes(&device, &committed, false).is_ok());
        assert!(closes(&device, &placed, true).is_ok());
        assert!(closes(&device, &committed, true).is_err(), "Close() accepted an aliasing barrier on a committed texture");
    }

    /// The first upload's aliasing barrier goes to textures in a block and to reserved ones, not to committed ones
    /// and not to a texture uploaded before.
    #[test]
    fn the_aliasing_barrier_is_for_textures_in_a_block_or_reserved() {
        for (in_block, on_demand, uploaded, want) in [
            (true, false, false, true),
            (false, false, false, false),
            (false, true, false, true),
            (true, true, false, true),
            (true, false, true, false),
            (false, true, true, false),
        ] {
            assert_eq!(needs_alias(in_block, on_demand, uploaded), want, "{in_block} {on_demand} {uploaded}");
        }
    }

    /// `pool::in_block` finds a texture cut from a block and not a committed one. The placed texture is in a heap
    /// of the test's own, marked as the pool marks its pieces: a block of the shared pool made on the test's device
    /// stayed there for the other tests of the process, which then lost their device.
    #[test]
    fn in_block_tells_a_placed_texture_from_a_committed_one() {
        let Some(device) = warp() else { return };
        let d = desc(256);
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
        let mut committed: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe {
            device
                .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &d, D3D12_RESOURCE_STATE_COMMON, None, &mut committed)
                .unwrap();
        }
        assert!(!crate::pool::in_block(&committed.unwrap()));
        // SAFETY: plain query and creation.
        let placed = unsafe {
            let info = device.GetResourceAllocationInfo(0, &[d]);
            let hd = D3D12_HEAP_DESC {
                SizeInBytes: info.SizeInBytes.next_multiple_of(65536),
                Properties: heap,
                Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES,
                ..Default::default()
            };
            let mut h: Option<ID3D12Heap> = None;
            device.CreateHeap(&hd, &mut h).unwrap();
            let mut p: Option<ID3D12Resource> = None;
            device.CreatePlacedResource(&h.unwrap(), 0, &d, D3D12_RESOURCE_STATE_COMMON, None, &mut p).unwrap();
            p.unwrap()
        };
        assert!(!crate::pool::in_block(&placed));
        crate::pool::mark_in_block(&placed);
        assert!(crate::pool::in_block(&placed));
        assert!(closes(&device, &placed, true).is_ok());
    }

    /// A texture's upload state as the lists and the submission drive it: the staging buffer's and the host
    /// copy's content version.
    #[derive(Default)]
    struct Sim {
        uploaded: bool,
        dirty: bool,
        changed: bool,
        streak: u8,
        staging: u32,
        host: u32,
        uploads: u32,
        /// Taken for a movie plane ([`Staging::planed`]).
        planed: bool,
    }

    impl Sim {
        /// One use under title content `title`: the list records an upload or not, then the submission refreshes
        /// (detiles if the title changed) and notes the upload, in one order or the other.
        fn used(&mut self, title: u32, refresh_first: bool, streak_on: bool) {
            let up = !skips_upload(self.uploaded, self.dirty, if streak_on { self.streak } else { 0 });
            let refresh = |s: &mut Sim| {
                if s.staging != title {
                    s.staging = title;
                    s.dirty = true;
                    s.changed = true;
                }
                if streak_on {
                    let changed = std::mem::take(&mut s.changed);
                    s.streak = next_streak(s.streak, changed, s.uploaded);
                }
            };
            let note = |s: &mut Sim| {
                if up {
                    s.uploaded = true;
                    s.dirty = false;
                }
            };
            if refresh_first {
                refresh(self);
                note(self);
            } else {
                note(self);
                refresh(self);
            }
            if up {
                // The card's copy takes what the staging buffer holds when the list runs.
                self.host = self.staging;
                self.uploads += 1;
            }
        }
    }

    impl Sim {
        /// One use of a texture of a plane's shape under `mode`: from the use it is taken for a plane on, the
        /// list's own buffer is filled from title memory at the list's step, so the host copy has `title` whatever
        /// the staging buffer holds, and the refresh and the notes of the staging buffer are not asked for.
        fn used_in(&mut self, title: u32, refresh_first: bool, mode: Planes) {
            if uses_plane(mode, true, self.streak, self.planed) {
                self.planed = true;
                self.uploaded = true;
                self.host = title;
                self.uploads += 1;
            } else {
                self.used(title, refresh_first, true);
            }
        }
    }

    /// A ring of three textures rewritten before every use (a video): from the third use of each on, the
    /// host copy has what the title holds at every use, whichever of the refresh and the note runs first; without
    /// the streak the refresh-first order skips every second use (the bug).
    #[test]
    fn a_ring_of_rewritten_textures_is_uploaded_at_every_use() {
        for refresh_first in [true, false] {
            let mut ring = [Sim::default(), Sim::default(), Sim::default()];
            for use_ in 0..30u32 {
                let t = &mut ring[(use_ % 3) as usize];
                t.used(use_ + 1, refresh_first, true);
                if use_ >= 6 {
                    assert_eq!(t.host, use_ + 1, "use {use_}, refresh first: {refresh_first}");
                }
            }
            let mut old = Sim::default();
            let mut stale = 0;
            for use_ in 0..30u32 {
                old.used(use_ + 1, refresh_first, false);
                stale += (old.host != use_ + 1) as u32;
            }
            assert_eq!(stale > 0, refresh_first, "today's behaviour: stale uses {stale}, refresh first: {refresh_first}");
        }
    }

    /// A texture the title writes once costs at most one upload more than it needs, and it is current from its
    /// second use on.
    #[test]
    fn a_texture_changed_once_gets_at_most_one_extra_upload() {
        for refresh_first in [true, false] {
            let mut t = Sim::default();
            for use_ in 0..20 {
                t.used(7, refresh_first, true);
                if use_ >= 1 {
                    assert_eq!(t.host, 7);
                }
            }
            assert!(t.uploads <= 2, "{} uploads, refresh first: {refresh_first}", t.uploads);
            let before = t.uploads;
            t.used(7, refresh_first, true);
            assert_eq!(t.uploads, before, "no upload once nothing changes");
            // Changed again after a long rest.
            for _ in 0..3 {
                t.used(8, refresh_first, true);
            }
            assert_eq!(t.host, 8);
            assert!(t.uploads <= before + 3);
        }
    }

    #[test]
    fn the_streak_counts_changes_after_an_upload_and_ends_with_the_first_quiet_refresh() {
        assert_eq!(next_streak(0, true, false), 0);
        assert_eq!(next_streak(0, true, true), 1);
        assert_eq!(next_streak(254, true, true), 255);
        assert_eq!(next_streak(255, true, true), 255);
        assert_eq!(next_streak(9, false, true), 0);
        assert!(skips_upload(true, false, 0));
        assert!(!skips_upload(true, false, 1));
        assert!(!skips_upload(true, true, 0));
        assert!(!skips_upload(false, false, 0));
    }

    /// What a refresh looks at: a bake's texture always; a texture made for one upload only until it is filled;
    /// any other when the guard saw a write, or it is its turn or it is hot.
    #[test]
    fn a_refresh_looks_at_the_memory_when_exact_or_written_or_its_turn() {
        // (exact, alone, hashed, unwritten, turn, hot) -> refreshes
        let table = [
            // The old decisions, one after the other.
            (false, true, true, false, false, false, false),
            (false, true, false, false, false, false, true),
            (false, false, false, false, false, false, true),
            (false, false, false, true, false, false, false),
            (false, false, false, true, true, false, true),
            (false, false, false, true, false, true, true),
            (false, false, true, true, false, false, false),
            // A bake's texture never passes up a look.
            (true, true, true, false, false, false, true),
            (true, false, false, true, false, false, true),
            (true, false, true, true, false, false, true),
            (true, true, false, true, false, false, true),
        ];
        for (exact, alone, hashed, unwritten, turn, hot, want) in table {
            assert_eq!(
                refreshes(exact, alone, hashed, unwritten, turn, hot),
                want,
                "exact {exact} alone {alone} hashed {hashed} unwritten {unwritten} turn {turn} hot {hot}"
            );
        }
        // Without a bake the decision is the one the code had: skipped only when alone and filled, or unwritten
        // and neither its turn nor hot.
        for bits in 0..32u32 {
            let [alone, hashed, unwritten, turn, hot] = std::array::from_fn(|i| bits >> i & 1 == 1);
            let before = !((alone && hashed) || (!alone && unwritten && !turn && !hot));
            let now = refreshes(false, alone, hashed, if alone { false } else { unwritten }, turn, hot);
            assert_eq!(now, before, "{bits:05b}");
        }
    }

    fn plane_desc() -> victory_formats::texture::Desc {
        victory_formats::texture::Desc {
            dimension: victory_formats::texture::Dimension::Tex2d,
            width: 1920,
            height: 1080,
            depth: 1,
            mips: 1,
            element_bytes: 1,
            block: 1,
            samples: 1,
            swizzle: victory_formats::texture::Swizzle::LINEAR,
        }
    }

    fn plane_footprint(format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT, offset: u64) -> D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
        D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
            Offset: offset,
            Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: format, Width: 1920, Height: 1080, Depth: 1, RowPitch: 1920 },
        }
    }

    /// Only a linear single-level 8-bit 2D texture with one subresource at the start of its staging buffer, not
    /// on demand, has a movie plane's shape.
    #[test]
    fn plane_shape_is_a_linear_single_level_8_bit_2d_texture() {
        use victory_formats::texture::{Dimension, Swizzle};
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8_UNORM};
        let one = [plane_footprint(DXGI_FORMAT_R8_UNORM, 0)];
        assert!(plane_shape(&plane_desc(), &one, false));
        assert!(!plane_shape(&plane_desc(), &one, true), "on demand");
        let with = |f: &dyn Fn(&mut victory_formats::texture::Desc)| {
            let mut d = plane_desc();
            f(&mut d);
            plane_shape(&d, &one, false)
        };
        assert!(!with(&|d| d.swizzle = Swizzle::SW_64KB_S), "tiled");
        assert!(!with(&|d| d.dimension = Dimension::Tex3d), "3D");
        assert!(!with(&|d| d.dimension = Dimension::Tex1d), "1D");
        assert!(!with(&|d| d.depth = 2), "an array");
        assert!(!with(&|d| d.mips = 4), "mips");
        assert!(!with(&|d| d.samples = 2), "samples");
        assert!(!plane_shape(&plane_desc(), &[plane_footprint(DXGI_FORMAT_R8G8_UNORM, 0)], false), "two channels");
        assert!(!plane_shape(&plane_desc(), &[plane_footprint(DXGI_FORMAT_R8_UNORM, 256)], false), "not at the start");
        assert!(!plane_shape(&plane_desc(), &[], false), "no subresource");
        assert!(!plane_shape(&plane_desc(), &[one[0], one[0]], false), "two subresources");
    }

    #[test]
    fn the_switch_names_off_all_or_the_latch() {
        assert_eq!(planes_mode(None), Planes::Latch);
        assert_eq!(planes_mode(Some("1")), Planes::Latch);
        assert_eq!(planes_mode(Some("0")), Planes::Off);
        assert_eq!(planes_mode(Some("all")), Planes::All);
        // (The shape comes first, whatever the mode.)
        for mode in [Planes::Off, Planes::Latch, Planes::All] {
            assert!(!uses_plane(mode, false, 255, true), "{mode:?}");
        }
        assert!(!uses_plane(Planes::Off, true, 255, true));
        assert!(uses_plane(Planes::All, true, 0, false));
        assert!(!uses_plane(Planes::Latch, true, 0, false));
        assert!(!uses_plane(Planes::Latch, true, PLANE_STREAK - 1, false));
        assert!(uses_plane(Planes::Latch, true, PLANE_STREAK, false));
        assert!(uses_plane(Planes::Latch, true, 0, true), "sticky");
    }

    /// A ring of three planes rewritten before every use is taken for planes by its second use each, after which
    /// the host copy has what the title holds at every use, in either order of the refresh and the note.
    #[test]
    fn a_latched_ring_has_the_titles_frame_at_every_use() {
        for refresh_first in [true, false] {
            let mut ring = [Sim::default(), Sim::default(), Sim::default()];
            let mut first_latched = [None; 3];
            for use_ in 0..60u32 {
                let i = (use_ % 3) as usize;
                ring[i].used_in(use_ + 1, refresh_first, Planes::Latch);
                if ring[i].planed && first_latched[i].is_none() {
                    first_latched[i] = Some(use_);
                }
                if let Some(at) = first_latched[i] {
                    assert!(use_ >= at);
                    assert_eq!(ring[i].host, use_ + 1, "use {use_}, refresh first: {refresh_first}");
                }
            }
            for (i, at) in first_latched.iter().enumerate() {
                assert!(at.is_some_and(|at| at <= 12), "ring {i} latched at {at:?}, refresh first: {refresh_first}");
            }
            // By shape alone (`=all`), from the first use.
            let mut all = Sim::default();
            for use_ in 0..10u32 {
                all.used_in(use_ + 1, refresh_first, Planes::All);
                assert_eq!(all.host, use_ + 1);
            }
            // Off: the old path, as before.
            let mut off = Sim::default();
            for use_ in 0..10u32 {
                off.used_in(use_ + 1, refresh_first, Planes::Off);
            }
            assert!(!off.planed);
        }
    }

    /// A texture the title writes once (or never) is never taken for a plane: its first refresh after the first
    /// upload counts one by itself in one order of the refresh and the note, and that is not enough.
    #[test]
    fn a_texture_changed_once_never_latches() {
        for refresh_first in [true, false] {
            let mut t = Sim::default();
            let mut most = 0;
            for _ in 0..40 {
                t.used_in(7, refresh_first, Planes::Latch);
                most = most.max(t.streak);
            }
            assert!(!t.planed, "refresh first: {refresh_first}");
            assert!(most < PLANE_STREAK, "streak {most}, refresh first: {refresh_first}");
            // Written again after a long rest, and once more: still not a plane.
            for title in [8, 8, 8, 9, 9, 9] {
                t.used_in(title, refresh_first, Planes::Latch);
            }
            assert!(!t.planed, "refresh first: {refresh_first}");
            assert_eq!(t.host, 9);
            // (A threshold of 1 would have taken it for one in the order the note comes first.)
            let mut t = Sim::default();
            let mut one = false;
            for _ in 0..4 {
                t.used(7, refresh_first, true);
                one |= t.streak >= 1;
            }
            assert_eq!(one, !refresh_first);
        }
    }

    /// Title memory of a plane 100 texels wide and 4 rows high, rows of 256 bytes as a linear layout has them,
    /// each byte telling its row and column.
    fn plane_memory() -> (victory_formats::texture::Layout, Vec<u8>) {
        let mut desc = plane_desc();
        (desc.width, desc.height) = (100, 4);
        let layout = victory_formats::texture::Layout::new(&desc).unwrap();
        assert_eq!(layout.mips[0].pitch, 256);
        let mut title = vec![0u8; layout.size as usize];
        for (i, b) in title.iter_mut().enumerate() {
            *b = (i / 256 * 16 + i % 256 % 16) as u8 ^ 0x5a;
        }
        (layout, title)
    }

    /// Rows as far apart as the title's: one read of the whole plane, padding included.
    #[test]
    fn fill_plane_reads_a_plane_with_the_titles_pitch_in_one_go() {
        let (layout, title) = plane_memory();
        let bytes = 256 * 4;
        let mut out = vec![0xeeu8; bytes];
        fill_plane(&layout, title.as_ptr() as usize, title.len(), out.as_mut_ptr() as usize, bytes, 256);
        assert_eq!(out, title[..bytes]);
    }

    /// Rows farther apart in the upload buffer (a texture whose footprint is wider than the layout's pitch):
    /// each row's texels are detiled to their place.
    #[test]
    fn fill_plane_spreads_the_rows_when_the_pitch_differs() {
        let (layout, title) = plane_memory();
        let pitch = 512;
        let bytes = pitch * 4;
        let mut out = vec![0xeeu8; bytes];
        fill_plane(&layout, title.as_ptr() as usize, title.len(), out.as_mut_ptr() as usize, bytes, pitch);
        for row in 0..4 {
            assert_eq!(out[row * pitch..row * pitch + 100], title[row * 256..row * 256 + 100], "row {row}");
        }
        // What is not a texel is left as it was.
        assert!(out[100..pitch].iter().all(|&b| b == 0xee));
    }
}

#[cfg(test)]
mod staging_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

    /// The staging state a thread remembers is the table's own: the same
    /// one whichever way it is asked for, and another once the table let go
    /// of it (`unstage`, `forget`), also while a list still holds the old
    /// one; from eight threads, four each way, while a ninth lets go again
    /// and again, every answer is the one the table has for its texture.
    #[test]
    fn a_remembered_staging_state_is_the_tables_own() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
        if crate::host::host().is_none() {
            return;
        }
        const TEXTURES: usize = 6;
        // 256x256 BC7, nine levels, 4 KiB blocks.
        let desc = [3u32, 0, 0, 0, 256, 0, 256, 0x9_0001, 98, 1, 0, 0x105, 0, 0];
        let whole = crate::resource::placed_bytes(&desc).unwrap() as usize;
        // SAFETY: memory of our own that stays.
        let all = unsafe { VirtualAlloc(std::ptr::null(), whole * TEXTURES, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
        let at = all as usize;
        let textures: Vec<(usize, Arc<crate::resource::HostTexture>)> = (0..TEXTURES)
            .map(|i| {
                let texture = crate::resource::texture_at((at + i * whole) as u64, &desc).unwrap();
                crate::resource::want_level(texture, 0);
                (texture as usize, crate::resource::host_copy(texture).unwrap())
            })
            .collect();
        let ask = |i: usize, quick: bool| staging_by(textures[i].0 as This, &textures[i].1, 0, quick);
        let in_table = |i: usize| STAGING.lock().unwrap().as_ref().and_then(|map| map.get(&textures[i].0).cloned());
        // (No staging memory now, as when commit is short: nothing to tell.)
        if ask(0, false).is_none() {
            return;
        }
        for i in 0..TEXTURES {
            let first = ask(i, false).unwrap();
            for quick in [true, true, false, true] {
                assert!(Arc::ptr_eq(&ask(i, quick).unwrap(), &first));
            }
            // Let go by the table while `first` is held, as a list holds it.
            unstage(textures[i].0 as This);
            let second = ask(i, true).unwrap();
            assert!(!Arc::ptr_eq(&second, &first));
            assert!(Arc::ptr_eq(&ask(i, false).unwrap(), &second) && Arc::ptr_eq(&ask(i, true).unwrap(), &second));
            forget(textures[i].0);
            let third = ask(i, true).unwrap();
            assert!(!Arc::ptr_eq(&third, &second) && !Arc::ptr_eq(&third, &first));
            assert!(Arc::ptr_eq(&in_table(i).unwrap(), &third));
        }
        // Each texture's turn: asks share it, a letting go has it alone, so
        // that an ask can look at the table before the next letting go.
        let turns: Vec<std::sync::RwLock<()>> = (0..TEXTURES).map(|_| std::sync::RwLock::new(())).collect();
        let stop = AtomicBool::new(false);
        let (turns, stop, ask, in_table) = (&turns, &stop, &ask, &in_table);
        let textures = &textures;
        std::thread::scope(|s| {
            let letting_go = s.spawn(move || {
                let mut n = 0usize;
                while !stop.load(Relaxed) {
                    let i = n % TEXTURES;
                    {
                        let _turn = turns[i].write().unwrap();
                        unstage(textures[i].0 as This);
                    }
                    n += 1;
                    std::thread::sleep(std::time::Duration::from_micros(300));
                }
                n
            });
            let asking: Vec<_> = (0..8usize)
                .map(|t| {
                    s.spawn(move || {
                        for n in 0..3000usize {
                            let i = (n * (2 * t + 1)) % TEXTURES;
                            let _turn = turns[i].read().unwrap();
                            let Some(found) = ask(i, t % 2 == 0) else {
                                continue;
                            };
                            assert_eq!(found.serial, textures[i].1.serial);
                            let there = in_table(i);
                            assert!(there.is_some_and(|there| Arc::ptr_eq(&there, &found)), "texture {i}, thread {t}");
                        }
                    })
                })
                .collect();
            let asked: Vec<_> = asking.into_iter().map(|thread| thread.join()).collect();
            stop.store(true, Relaxed);
            assert!(letting_go.join().expect("the letting go") > 0);
            assert!(asked.iter().all(|thread| thread.is_ok()), "an asking thread failed");
        });
        for texture in textures {
            unstage(texture.0 as This);
        }
    }
}
