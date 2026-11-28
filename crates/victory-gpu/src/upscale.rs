//! The host temporal upscaler: a title that upscales through the public
//! FidelityFX API gets its upscale done by an upscaler for the PC, on our
//! host device, instead of its console upscaler run as translated compute.
//!
//! The title's side is `victory_hle::modules::fidelityfx`: its imports (and
//! `GetProcAddress` look-ups) of the API's entry points are bound to shims.
//! The title's own DLL keeps every context it is asked to create, and
//! answers every call the shims do not take, so whenever the host upscaler
//! cannot do one dispatch the title's own does it, exactly as without us.
//! What the shims take is an upscale dispatch of a context the title created
//! for upscaling ([`ffx::DISPATCH_DESC_TYPE_UPSCALE`]): it becomes a step of
//! ours in the title's command list ([`record`]), which, at its place among
//! the title's commands when the list is recorded on the host (at once, or
//! when the list's stream is translated with `VICTORY_THIN=1`), brings the
//! title's textures to the states the upscaler reads and writes them in and
//! records the host upscaler's dispatch on our host list.
//!
//! Backends: AMD FSR 3.1 through the PC FidelityFX API DLL the user supplies
//! ([`fsr`], found by [`find`]). [`Backend`] is the place for another (an
//! upscaler reached through a vendor's own interposer DLL, supplied the same
//! way); none is built yet. Nothing proprietary is in the repository or a
//! release: the DLL is the user's (docs/upscaling.md).
//!
//! The parameters pass through as the title gives them: jitter offset and
//! motion-vector scale (both in the API's own units: render pixels, and the
//! factor that makes the title's vectors render pixels), the render and
//! upscale sizes, near and far planes, vertical field of view, frame time,
//! pre-exposure, sharpening, the reset flag. Both sides speak the same
//! public API description (the same type id and layout), so nothing is
//! converted; only the flags no host upscaler should see are left out
//! (debug checking and the debug view).

pub mod ffx;
pub mod find;
#[cfg(windows)]
mod fsr;

pub use find::{DLL_NAME, Found, Places, Source, discover};

use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;

/// Where the DLL is looked for, given by the front end before the title's
/// imports are bound.
static PLACES: OnceLock<Places> = OnceLock::new();

/// Sets where the user's upscaler DLL is looked for ([`find::discover`]);
/// the first call counts.
pub fn use_places(places: Places) {
    let _ = PLACES.set(places);
}

/// Whether the host upscaler may be used at all. On unless
/// `VICTORY_HOST_UPSCALER=0`: then a title's FidelityFX calls all go to its
/// own DLL, as before the host upscaler existed (and nothing is looked for).
pub fn wanted() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        // `VICTORY_HOST_UPSCALER=0` turns the host upscaler off: no shims are
        // bound and the title's own upscaler runs. Unset (or any other
        // value): on, when the user's upscaler DLL is found.
        let on = std::env::var_os("VICTORY_HOST_UPSCALER").is_none_or(|v| v != "0");
        if !on {
            set_state(State::Off);
            tracing::info!("host upscaler: off (VICTORY_HOST_UPSCALER=0); a title's FidelityFX calls go to its own DLL");
        }
        on
    })
}

/// The user's upscaler DLL, looked for once ([`find::discover`] in the
/// places given to [`use_places`]); one line in the log says what was found.
pub fn dll() -> Option<&'static std::path::Path> {
    static FOUND: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    FOUND
        .get_or_init(|| {
            let places = PLACES.get().cloned().unwrap_or_default();
            let found = discover(&places, &|p| p.is_file(), &|p| p.is_dir());
            match &found.dll {
                Some((path, source)) => {
                    tracing::info!(dll = %path.display(), ?source, "host upscaler: found the upscaler DLL");
                }
                None => {
                    set_state(State::NoDll);
                    let looked: Vec<String> = found.missing.iter().map(|p| p.display().to_string()).collect();
                    tracing::info!(
                        looked = %looked.join("; "),
                        "host upscaler: no {DLL_NAME} found (docs/upscaling.md); a title's FidelityFX calls go to its own DLL"
                    );
                }
            }
            found.dll.map(|(path, _)| path)
        })
        .as_deref()
}

/// For `victory check`: the user's upscaler DLL in `places`, loaded and
/// asked which upscaler versions it has for a device of the adapter a run
/// uses. `Ok(None)`: none found (optional); `Ok(Some((path, version)))`:
/// a run would use it; `Err((path, why))`: found, and a run would fall back
/// to the title's own upscaler for that reason.
#[cfg(windows)]
pub fn probe(places: &Places) -> Result<Option<(std::path::PathBuf, String)>, (std::path::PathBuf, String)> {
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Direct3D12::{D3D12CreateDevice, ID3D12Device};
    use windows::core::Interface;
    let found = discover(places, &|p| p.is_file(), &|p| p.is_dir());
    let Some((path, _)) = found.dll else { return Ok(None) };
    // The adapter a run would use (crate::adapter::choose), else the default one.
    let mut device: Option<ID3D12Device> = crate::adapter::choose().map(|c| c.device);
    // SAFETY: an out pointer for the device; no adapter: the default one.
    if device.is_none()
        && let Err(e) = unsafe { D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device) }
    {
        return Err((path, format!("no Direct3D 12 device to ask it with ({e})")));
    }
    let Some(device) = device else { return Err((path, "no Direct3D 12 device to ask it with".to_owned())) };
    match fsr::Fsr::load(&path, device.as_raw()) {
        Ok(fsr) => Ok(Some((path, fsr.version))),
        Err(why) => Err((path, why)),
    }
}

/// Whether a title's FidelityFX entry points are worth binding to shims:
/// asked for and a DLL found.
pub fn interposing() -> bool {
    wanted() && dll().is_some()
}

/// What the host upscaler is, for the run report.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
enum State {
    /// Nothing asked yet (the title made no upscale context).
    NotLooked = 0,
    Off = 1,
    NoDll = 2,
    /// The DLL was refused (does not load, wrong version, no host device).
    Refused = 3,
    /// Loaded; contexts are made on the host.
    Ready = 4,
}

static STATE: AtomicU64 = AtomicU64::new(State::NotLooked as u64);

fn set_state(state: State) {
    STATE.store(state as u64, Relaxed);
}

/// Since the start: host contexts made, upscale dispatches recorded on the
/// host, upscale dispatches the title's own DLL did instead, host
/// dispatches that failed (at creation or recording), dispatches timed on
/// the card, their nanoseconds, and the longest.
static COUNTS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
const CONTEXTS: usize = 0;
const HOST_DISPATCHES: usize = 1;
const TITLE_DISPATCHES: usize = 2;
const FAILURES: usize = 3;
const TIMED: usize = 4;
const CARD_NS: usize = 5;
const CARD_NS_MOST: usize = 6;

/// An upscale dispatch the title's own DLL did, the host upscaler having
/// not taken it.
pub fn title_dispatched() {
    COUNTS[TITLE_DISPATCHES].fetch_add(1, Relaxed);
}

/// Says `why` the title's own upscaler runs, once per reason.
pub fn fell_back(why: &str) {
    static SAID: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if said.len() < 32 && !said.iter().any(|s| s == why) {
        said.push(why.to_owned());
        tracing::warn!("host upscaler: the title's own upscaler runs: {why}");
    }
}

/// The figures the run report gives (`[upscaler]`).
#[cfg(windows)]
pub(crate) fn report() {
    timing::collect();
    let c = |i: usize| COUNTS[i].load(Relaxed);
    victory_pipeline::report::upscaler_set(victory_pipeline::report::Upscaler {
        state: STATE.load(Relaxed),
        contexts: c(CONTEXTS),
        host_dispatches: c(HOST_DISPATCHES),
        title_dispatches: c(TITLE_DISPATCHES),
        failures: c(FAILURES),
        timed: c(TIMED),
        card_ns: c(CARD_NS),
        card_ns_most: c(CARD_NS_MOST),
    });
}

/// What a title asked of an upscale context it created: its flags and the
/// largest render and upscale sizes it will dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sizes {
    pub flags: u32,
    pub max_render: ffx::Dimensions,
    pub max_upscale: ffx::Dimensions,
}

impl Sizes {
    /// From the title's creation description.
    pub fn of(desc: &ffx::CreateUpscale) -> Sizes {
        Sizes { flags: desc.flags, max_render: desc.max_render, max_upscale: desc.max_upscale }
    }
}

/// Why an upscale dispatch of the title's is not one the host takes: what a
/// host upscaler needs that it lacks. `None`: it can be taken.
pub fn refused(d: &ffx::DispatchUpscale, sizes: &Sizes) -> Option<&'static str> {
    let within = |s: ffx::Dimensions, most: ffx::Dimensions| {
        s.width > 0 && s.height > 0 && s.width <= most.width && s.height <= most.height
    };
    if d.header.kind != ffx::DISPATCH_DESC_TYPE_UPSCALE {
        return Some("not an upscale dispatch");
    }
    if d.command_list.is_null() {
        return Some("a dispatch without a command list");
    }
    if [d.color, d.depth, d.motion_vectors, d.output].iter().any(|r| r.resource.is_null()) {
        return Some("a dispatch without colour, depth, motion vectors or output");
    }
    if !within(d.render_size, sizes.max_render) || !within(d.upscale_size, sizes.max_upscale) {
        return Some("a dispatch of sizes beyond its context's");
    }
    let numbers = [
        d.jitter_offset.x,
        d.jitter_offset.y,
        d.motion_vector_scale.x,
        d.motion_vector_scale.y,
        d.sharpness,
        d.frame_time_delta,
        d.pre_exposure,
        d.camera_near,
        d.camera_far,
        d.camera_fov_angle_vertical,
        d.view_space_to_meters_factor,
    ];
    if numbers.iter().any(|v| !v.is_finite()) {
        return Some("a dispatch with a parameter that is not a number");
    }
    None
}

/// The host upscaler's dispatch of the title's `d`: its numbers as the
/// title gave them, on `list` (our host list) and the host resources `res`
/// (colour, depth, motion vectors, exposure, reactive, transparency and
/// composition, output), without the flags the host leaves out.
pub fn host_dispatch(d: &ffx::DispatchUpscale, list: *mut std::ffi::c_void, res: [ffx::Resource; 7]) -> ffx::DispatchUpscale {
    let [color, depth, motion_vectors, exposure, reactive, transparency_and_composition, output] = res;
    ffx::DispatchUpscale {
        header: ffx::Header { kind: ffx::DISPATCH_DESC_TYPE_UPSCALE, next: std::ptr::null_mut() },
        command_list: list,
        color,
        depth,
        motion_vectors,
        exposure,
        reactive,
        transparency_and_composition,
        output,
        enable_sharpening: u8::from(d.enable_sharpening != 0),
        reset: u8::from(d.reset != 0),
        flags: d.flags & ffx::UPSCALE_DISPATCH_FLAGS_KEPT,
        ..*d
    }
}

#[cfg(windows)]
pub use host::{Context, context, record};

#[cfg(windows)]
mod host {
    use super::*;
    use std::sync::{Arc, Mutex};

    use windows::Win32::Graphics::Direct3D12::ID3D12Resource;
    use windows::core::Interface;

    use crate::host::HostList;
    use crate::object::This;

    /// The backends there are. One is built: FSR through the user's PC
    /// FidelityFX API DLL.
    enum Backend {
        Fsr(fsr::Fsr),
    }

    impl Backend {
        fn name(&self) -> String {
            match self {
                Backend::Fsr(f) => format!("FSR {}", f.version),
            }
        }
    }

    /// The backend, loaded at the first upscale context a title creates;
    /// why not, if it cannot be.
    fn backend() -> Result<&'static Backend, &'static str> {
        static BACKEND: OnceLock<Result<Backend, String>> = OnceLock::new();
        BACKEND
            .get_or_init(|| {
                let loaded = (|| {
                    let path = super::dll().ok_or_else(|| format!("no {DLL_NAME} was found"))?;
                    let host = crate::host::host().ok_or("there is no host device")?;
                    fsr::Fsr::load(path, host.device.as_raw())
                        .map(Backend::Fsr)
                        .map_err(|why| format!("{} is refused: {why}", path.display()))
                })();
                match &loaded {
                    Ok(b) => {
                        set_state(State::Ready);
                        tracing::info!(backend = %b.name(), "host upscaler: ready; a title's FidelityFX upscales run on it");
                    }
                    Err(why) => {
                        set_state(State::Refused);
                        tracing::warn!("host upscaler: {why}; a title's FidelityFX calls go to its own DLL");
                    }
                }
                loaded
            })
            .as_ref()
            .map_err(|_| "the upscaler DLL is refused")
    }

    enum Made {
        Fsr(fsr::FsrContext),
    }

    // SAFETY: a context of the DLL's is used by one thread at a time (behind
    // `Context::made`) and let go of once.
    unsafe impl Send for Made {}

    /// A host upscale context, standing beside one of the title's.
    pub struct Context {
        made: Mutex<Option<Made>>,
        sizes: Sizes,
        failed: std::sync::atomic::AtomicBool,
    }

    impl Context {
        /// Whether a host dispatch of it failed: from then on the title's own
        /// upscaler does its dispatches.
        pub fn failed(&self) -> bool {
            self.failed.load(Relaxed)
        }

        pub fn sizes(&self) -> Sizes {
            self.sizes
        }

        fn fail(&self, why: &str) {
            COUNTS[FAILURES].fetch_add(1, Relaxed);
            if !self.failed.swap(true, Relaxed) {
                super::fell_back(why);
            }
        }
    }

    impl Drop for Context {
        /// What the card runs may still use the context: it goes once every
        /// submission running now has finished.
        fn drop(&mut self) {
            let made = self.made.get_mut().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(made) = made {
                crate::host::retire(made);
            }
        }
    }

    /// A host context for a title's upscale context of `sizes`: made on our
    /// host device by the backend, loaded now if it was not.
    pub fn context(sizes: Sizes) -> Result<Arc<Context>, String> {
        let backend = backend().map_err(str::to_owned)?;
        let host = crate::host::host().ok_or("there is no host device")?;
        let made = match backend {
            Backend::Fsr(fsr) => fsr.context(host.device.as_raw(), sizes.flags, sizes.max_render, sizes.max_upscale).map(Made::Fsr),
        }
        .inspect_err(|_| {
            COUNTS[FAILURES].fetch_add(1, Relaxed);
        })?;
        COUNTS[CONTEXTS].fetch_add(1, Relaxed);
        tracing::info!(
            render = format_args!("{}x{}", sizes.max_render.width, sizes.max_render.height),
            upscale = format_args!("{}x{}", sizes.max_upscale.width, sizes.max_upscale.height),
            flags = format_args!("{:#x}", sizes.flags),
            "host upscaler: a context"
        );
        Ok(Arc::new(Context { made: Mutex::new(Some(made)), sizes, failed: false.into() }))
    }

    /// The title's dispatch and the context, carried to where the list is
    /// recorded on the host.
    struct Step {
        context: Arc<Context>,
        title: ffx::DispatchUpscale,
    }

    // SAFETY: the description's pointers name the title's resources, which
    // it keeps alive until the list has run, as for any command; its own
    // list pointer is not used.
    unsafe impl Send for Step {}

    /// Takes the title's upscale dispatch `d` into its command list `list`:
    /// recorded on the host list now, or written down to be (`VICTORY_THIN=1`).
    /// An error says why it was not taken, and nothing was recorded: the
    /// title's own upscaler must do it.
    pub fn record(context: &Arc<Context>, d: &ffx::DispatchUpscale) -> Result<(), String> {
        if context.failed() {
            return Err("an earlier host dispatch of the context failed".into());
        }
        if let Some(why) = refused(d, &context.sizes) {
            return Err(why.into());
        }
        let this = d.command_list as This;
        // Each texture named is the host's (asking makes its host copy).
        let named = [d.color, d.depth, d.motion_vectors, d.exposure, d.reactive, d.transparency_and_composition, d.output];
        if named
            .iter()
            .any(|r| !r.resource.is_null() && crate::resource::host_resource(r.resource as This).is_none())
        {
            return Err("a texture of the dispatch has no host texture".into());
        }
        let step = Step { context: context.clone(), title: *d };
        if crate::list::written_down(this) {
            let ok = crate::list::write_down(
                this,
                Box::new(move |list, state| {
                    if let Err(why) = step.run(list, state) {
                        // Too late for the title's own: this frame's output
                        // is not upscaled, and the context's later dispatches
                        // are the title's.
                        step.context.fail(&format!("a host dispatch failed: {why}"));
                    }
                }),
            );
            return if ok { Ok(()) } else { Err("the command list is not open".into()) };
        }
        match crate::list::run_now(this, |list, state| step.run(list, state)) {
            Some(Ok(())) => Ok(()),
            Some(Err(why)) => {
                let why = format!("a host dispatch failed: {why}");
                context.fail(&why);
                Err(why)
            }
            None => Err("the command list has no host list".into()),
        }
    }

    impl Step {
        fn run(&self, list: &HostList, state: &mut crate::draw::State) -> Result<(), String> {
            let d = &self.title;
            let res = [
                input(list, &d.color)?,
                depth(list, &d.depth)?,
                input(list, &d.motion_vectors)?,
                input(list, &d.exposure)?,
                input(list, &d.reactive)?,
                input(list, &d.transparency_and_composition)?,
                output(list, &d.output)?,
            ];
            let desc = host_dispatch(d, list.list.as_raw(), res);
            let made = self.context.made.lock().unwrap_or_else(|e| e.into_inner());
            let Some(made) = made.as_ref() else {
                return Err("the context is gone".into());
            };
            #[cfg(feature = "diagnostics")]
            list.note_op(|| "host upscaler dispatch".to_owned());
            list.gpu_kind(crate::host::gpu_split::Kind::Passes);
            let slot = timing::begin(&list.list);
            match made {
                Made::Fsr(c) => c.dispatch(&desc)?,
            }
            if let Some(slot) = slot {
                timing::end(&list.list, slot);
            }
            // The upscaler set its own heaps, root signature and pipeline:
            // the title's are set again before its next draw or dispatch.
            list.state_lost();
            state.copied();
            COUNTS[HOST_DISPATCHES].fetch_add(1, Relaxed);
            first_dispatch(d);
            Ok(())
        }
    }

    /// The first host dispatch's numbers, once in the log.
    fn first_dispatch(d: &ffx::DispatchUpscale) {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::info!(
                render = format_args!("{}x{}", d.render_size.width, d.render_size.height),
                upscale = format_args!("{}x{}", d.upscale_size.width, d.upscale_size.height),
                jitter = format_args!("{},{}", d.jitter_offset.x, d.jitter_offset.y),
                mv_scale = format_args!("{},{}", d.motion_vector_scale.x, d.motion_vector_scale.y),
                near = d.camera_near,
                far = d.camera_far,
                fov = d.camera_fov_angle_vertical,
                frame_ms = d.frame_time_delta,
                exposure = !d.exposure.resource.is_null(),
                reactive = !d.reactive.resource.is_null(),
                transparency = !d.transparency_and_composition.resource.is_null(),
                "host upscaler: the first upscale ran on the host"
            );
        });
    }

    const UNORDERED_ACCESS: u32 = 0x8;
    const SHADER_RESOURCE: u32 = 0x40 | 0x80;

    /// A texture the upscaler reads: its host copy up to date (a texture the
    /// title fills, uploaded; a depth texture shaders wrote, given what they
    /// wrote) and readable by shaders. Absent (null) stays absent.
    fn input(list: &HostList, r: &ffx::Resource) -> Result<ffx::Resource, String> {
        if r.resource.is_null() {
            return Ok(ffx::Resource::default());
        }
        let this = r.resource as This;
        let copy = crate::resource::host_copy(this).ok_or("a texture has no host copy")?;
        if copy.flags() & 0x7 == 0 {
            crate::texture::upload_from(list, this, 0);
        }
        list.twin_use(this, false, false);
        crate::list::ensure_state(list, this, SHADER_RESOURCE);
        let state = crate::host::legal_for(copy.flags(), SHADER_RESOURCE);
        if state & SHADER_RESOURCE == 0 {
            return Err("a texture the upscaler reads cannot be read by shaders".into());
        }
        let texture = crate::resource::host_resource(this).ok_or("a texture has no host texture")?;
        Ok(described(texture, r, state))
    }

    /// The depth texture: its colour twin if it has one (what shaders wrote
    /// of it is there, and it is a plain colour texture), else as an input.
    fn depth(list: &HostList, r: &ffx::Resource) -> Result<ffx::Resource, String> {
        let this = r.resource as This;
        if list.twin_use(this, true, false) {
            let twin = crate::resource::twin_resource(this).ok_or("a depth texture's twin is gone")?;
            return Ok(described(twin, r, UNORDERED_ACCESS));
        }
        input(list, r)
    }

    /// The texture the upscaler writes, through unordered access.
    fn output(list: &HostList, r: &ffx::Resource) -> Result<ffx::Resource, String> {
        let this = r.resource as This;
        let copy = crate::resource::host_copy(this).ok_or("the output has no host copy")?;
        list.texture_written(this);
        if list.twin_use(this, true, true) {
            let twin = crate::resource::twin_resource(this).ok_or("the output's twin is gone")?;
            return Ok(described(twin, r, UNORDERED_ACCESS));
        }
        crate::list::ensure_state(list, this, UNORDERED_ACCESS);
        if crate::host::legal_for(copy.flags(), UNORDERED_ACCESS) & UNORDERED_ACCESS == 0 {
            return Err("the output cannot be written through unordered access".into());
        }
        let texture = crate::resource::host_resource(this).ok_or("the output has no host texture")?;
        Ok(described(texture, r, UNORDERED_ACCESS))
    }

    /// `texture` as the API describes it (as the SDK's DX12 helper does), in
    /// host state `state`; the title's format where the host's has no name
    /// in the API.
    fn described(texture: &ID3D12Resource, title: &ffx::Resource, state: u32) -> ffx::Resource {
        // SAFETY: a plain query of a live resource.
        let d = unsafe { texture.GetDesc() };
        const ALLOW_UNORDERED_ACCESS: i32 = 0x4;
        let format = d.Format.0 as u32;
        let usage = match format {
            // D16_UNORM, D32_FLOAT.
            55 | 40 => ffx::USAGE_DEPTHTARGET,
            // D24_UNORM_S8_UINT, D32_FLOAT_S8X24_UINT.
            45 | 20 => ffx::USAGE_DEPTHTARGET | ffx::USAGE_STENCILTARGET,
            _ => ffx::USAGE_READ_ONLY,
        } | if d.Flags.0 & ALLOW_UNORDERED_ACCESS != 0 { ffx::USAGE_UAV } else { 0 };
        // D3D12_RESOURCE_DIMENSION: 2 TEXTURE1D, 3 TEXTURE2D, 4 TEXTURE3D.
        let kind = match d.Dimension.0 {
            2 => ffx::RESOURCE_TYPE_TEXTURE1D,
            3 if d.DepthOrArraySize == 6 => ffx::RESOURCE_TYPE_TEXTURE_CUBE,
            4 => ffx::RESOURCE_TYPE_TEXTURE3D,
            _ => ffx::RESOURCE_TYPE_TEXTURE2D,
        };
        let format = match ffx::surface_format(format) {
            0 => title.description.format,
            known => known,
        };
        ffx::Resource {
            resource: texture.as_raw(),
            description: ffx::ResourceDescription {
                kind,
                format,
                width: d.Width as u32,
                height: d.Height,
                depth: u32::from(d.DepthOrArraySize),
                mip_count: u32::from(d.MipLevels),
                flags: 0,
                usage,
            },
            state: ffx::resource_state(state),
        }
    }

    /// The card's time of the host upscaler's dispatches: a timestamp before
    /// and after each, read when its slot is used again (long done) or for
    /// the run report.
    pub(super) mod timing {
        use super::super::*;
        use std::sync::Mutex;
        use windows::Win32::Graphics::Direct3D12::*;

        /// Dispatches stamped before a slot is used again.
        const SLOTS: u32 = 64;

        struct Stamps {
            heap: ID3D12QueryHeap,
            back: ID3D12Resource,
            data: usize,
            next: u32,
            ticks_per_ns: f64,
        }

        // SAFETY: free-threaded objects, and a mapping read behind the lock.
        unsafe impl Send for Stamps {}

        static STAMPS: Mutex<Option<Option<Stamps>>> = Mutex::new(None);

        fn make() -> Option<Stamps> {
            let host = crate::host::host()?;
            // SAFETY: plain object creation; the mapping stays for the run.
            unsafe {
                let mut heap: Option<ID3D12QueryHeap> = None;
                host.device
                    .CreateQueryHeap(
                        &D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: 2 * SLOTS, NodeMask: 0 },
                        &mut heap,
                    )
                    .ok()?;
                let back = host.read_back_buffer(16 * u64::from(SLOTS))?;
                let mut data = std::ptr::null_mut();
                back.Map(0, None, Some(&mut data)).ok()?;
                std::ptr::write_bytes(data.cast::<u8>(), 0, 16 * SLOTS as usize);
                let frequency = host.queue().GetTimestampFrequency().ok().filter(|&f| f > 0)?;
                Some(Stamps { heap: heap?, back, data: data as usize, next: 0, ticks_per_ns: frequency as f64 / 1e9 })
            }
        }

        /// Takes the pair at `slot` if the card wrote it.
        fn take(s: &Stamps, slot: u32) {
            // SAFETY: inside the mapping; the card writes whole words.
            let (start, end) = unsafe {
                let at = (s.data as *mut u64).add(2 * slot as usize);
                let read = (at.read_volatile(), at.add(1).read_volatile());
                if read.1 != 0 {
                    at.write_volatile(0);
                    at.add(1).write_volatile(0);
                }
                read
            };
            if start == 0 || end <= start {
                return;
            }
            let ns = ((end - start) as f64 / s.ticks_per_ns) as u64;
            // A second or more is no dispatch's time (a slot of long ago).
            if ns >= 1_000_000_000 {
                return;
            }
            COUNTS[TIMED].fetch_add(1, Relaxed);
            COUNTS[CARD_NS].fetch_add(ns, Relaxed);
            COUNTS[CARD_NS_MOST].fetch_max(ns, Relaxed);
        }

        /// A stamp in `list` before a dispatch: its slot.
        pub(crate) fn begin(list: &ID3D12GraphicsCommandList) -> Option<u32> {
            let mut stamps = STAMPS.lock().unwrap_or_else(|e| e.into_inner());
            let s = stamps.get_or_insert_with(make).as_mut()?;
            let slot = s.next;
            s.next = (slot + 1) % SLOTS;
            take(s, slot);
            // SAFETY: an open list and our heap.
            unsafe { list.EndQuery(&s.heap, D3D12_QUERY_TYPE_TIMESTAMP, 2 * slot) };
            Some(slot)
        }

        /// The stamp after the dispatch of `slot`, and both read back.
        pub(crate) fn end(list: &ID3D12GraphicsCommandList, slot: u32) {
            let stamps = STAMPS.lock().unwrap_or_else(|e| e.into_inner());
            let Some(Some(s)) = stamps.as_ref() else { return };
            // SAFETY: an open list, our heap and our read-back buffer (in the
            // copy destination state it is made in).
            unsafe {
                list.EndQuery(&s.heap, D3D12_QUERY_TYPE_TIMESTAMP, 2 * slot + 1);
                list.ResolveQueryData(&s.heap, D3D12_QUERY_TYPE_TIMESTAMP, 2 * slot, 2, &s.back, 16 * u64::from(slot));
            }
        }

        /// Takes every pair the card has written (for the run report).
        pub(crate) fn collect() {
            let stamps = STAMPS.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(Some(s)) = stamps.as_ref() {
                (0..SLOTS).for_each(|slot| take(s, slot));
            }
        }
    }
}

#[cfg(windows)]
use host::timing;

#[cfg(test)]
mod tests {
    use super::*;

    fn texture(n: usize) -> ffx::Resource {
        ffx::Resource {
            resource: n as *mut std::ffi::c_void,
            description: ffx::ResourceDescription { kind: 2, format: 4, width: 1920, height: 1080, ..Default::default() },
            state: ffx::STATE_COMPUTE_READ,
        }
    }

    fn title_dispatch() -> ffx::DispatchUpscale {
        ffx::DispatchUpscale {
            header: ffx::Header { kind: ffx::DISPATCH_DESC_TYPE_UPSCALE, next: std::ptr::null_mut() },
            command_list: 0x1000 as *mut _,
            color: texture(0x10),
            depth: texture(0x20),
            motion_vectors: texture(0x30),
            exposure: ffx::Resource::default(),
            reactive: texture(0x50),
            transparency_and_composition: ffx::Resource::default(),
            output: texture(0x70),
            jitter_offset: ffx::Coords { x: -0.25, y: 0.375 },
            motion_vector_scale: ffx::Coords { x: 1920.0, y: -1080.0 },
            render_size: ffx::Dimensions { width: 1280, height: 720 },
            upscale_size: ffx::Dimensions { width: 1920, height: 1080 },
            enable_sharpening: 7,
            sharpness: 0.2,
            frame_time_delta: 33.3,
            pre_exposure: 1.5,
            reset: 0,
            camera_near: 10000.0,
            camera_far: 0.1,
            camera_fov_angle_vertical: 1.0,
            view_space_to_meters_factor: 1.0,
            flags: 0b111,
        }
    }

    fn sizes() -> Sizes {
        Sizes {
            flags: 0,
            max_render: ffx::Dimensions { width: 1920, height: 1080 },
            max_upscale: ffx::Dimensions { width: 1920, height: 1080 },
        }
    }

    /// Every number goes to the host as the title gave it; the list and the
    /// resources are the host's; only the debug view's flag is left out.
    #[test]
    fn parameters_pass_through_exactly() {
        let d = title_dispatch();
        let host: [ffx::Resource; 7] = std::array::from_fn(|i| texture(0x9000 + i));
        let h = host_dispatch(&d, 0x5000 as *mut _, host);
        assert_eq!(h.header.kind, ffx::DISPATCH_DESC_TYPE_UPSCALE);
        assert!(h.header.next.is_null());
        assert_eq!(h.command_list as usize, 0x5000);
        assert_eq!(h.color, host[0]);
        assert_eq!(h.depth, host[1]);
        assert_eq!(h.motion_vectors, host[2]);
        assert_eq!(h.exposure, host[3]);
        assert_eq!(h.transparency_and_composition, host[5]);
        assert_eq!(h.output, host[6]);
        assert_eq!(h.jitter_offset, d.jitter_offset);
        assert_eq!(h.motion_vector_scale, d.motion_vector_scale);
        assert_eq!(h.render_size, d.render_size);
        assert_eq!(h.upscale_size, d.upscale_size);
        assert_eq!(
            [h.sharpness, h.frame_time_delta, h.pre_exposure, h.camera_near, h.camera_far, h.camera_fov_angle_vertical, h.view_space_to_meters_factor],
            [0.2, 33.3, 1.5, 10000.0, 0.1, 1.0, 1.0]
        );
        // A C bool the title set to something other than 1 is still true.
        assert_eq!((h.enable_sharpening, h.reset), (1, 0));
        assert_eq!(h.flags, 0b110);
        let mut reset = d;
        reset.reset = 1;
        assert_eq!(host_dispatch(&reset, std::ptr::null_mut(), host).reset, 1);
    }

    #[test]
    fn a_dispatch_the_host_can_take_is_not_refused() {
        assert_eq!(refused(&title_dispatch(), &sizes()), None);
    }

    #[test]
    fn what_a_host_upscaler_cannot_do_is_refused() {
        let s = sizes();
        let mut d = title_dispatch();
        d.header.kind = ffx::CREATE_CONTEXT_DESC_TYPE_UPSCALE;
        assert!(refused(&d, &s).is_some());
        let mut d = title_dispatch();
        d.output = ffx::Resource::default();
        assert!(refused(&d, &s).is_some());
        let mut d = title_dispatch();
        d.command_list = std::ptr::null_mut();
        assert!(refused(&d, &s).is_some());
        let mut d = title_dispatch();
        d.render_size.width = 2000;
        assert!(refused(&d, &s).is_some());
        let mut d = title_dispatch();
        d.upscale_size.height = 0;
        assert!(refused(&d, &s).is_some());
        let mut d = title_dispatch();
        d.jitter_offset.x = f32::NAN;
        assert!(refused(&d, &s).is_some());
    }

    #[test]
    fn a_context_keeps_the_titles_sizes() {
        let desc = ffx::CreateUpscale {
            header: ffx::Header { kind: ffx::CREATE_CONTEXT_DESC_TYPE_UPSCALE, next: std::ptr::null_mut() },
            flags: ffx::UPSCALE_ENABLE_DEPTH_INVERTED | (1 << 7),
            max_render: ffx::Dimensions { width: 2560, height: 1440 },
            max_upscale: ffx::Dimensions { width: 3840, height: 2160 },
            message: None,
        };
        let s = Sizes::of(&desc);
        assert_eq!((s.max_render.width, s.max_upscale.height), (2560, 2160));
        // Debug checking is not passed to the host (`fsr::Fsr::context`).
        assert_eq!(s.flags & ffx::UPSCALE_CREATE_FLAGS_KEPT, ffx::UPSCALE_ENABLE_DEPTH_INVERTED);
    }
}
