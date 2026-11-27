//! `victory selftest`: the frame path, end to end, driven by a stand-in
//! "title" made of our own code, so that it can be run on any PC, in the
//! tests and in CI, without a single file from a title.
//!
//! The stand-in is `victory_standin`, which the open demo title
//! (`tests/fixtures/demo-title`) links too: it calls what a title's calls
//! arrive at and nothing else. The device comes from the device-creation
//! export a title imports (the caller resolves it through the runtime's
//! registry and passes it in), and every object after that is reached
//! through the function tables the device hands out, at the offsets a
//! title's code calls (the tables of `device.rs`, `queue.rs`, `list.rs`,
//! `resource.rs`, `descriptor.rs`, `fence.rs`). The structures it passes
//! are laid out as on PC, which is what a title passes. Only the checks
//! here read behind the tables: a picture is read back from the host
//! texture the layer made, what the swap chain did is asked of the host
//! (`host/selfcheck.rs`), and a pipeline's first use is waited for until
//! its host pipeline is there.
//!
//! The stages, each reported as it finishes:
//!
//! 1. the host device, a queue, a list, a fence and the window;
//! 2. frames cleared to known colours and presented through the queue's
//!    present, read back and compared;
//! 3. title memory: a buffer the title writes is copied by a list and
//!    read back by the title, then changed and copied again; a texture the
//!    title writes in the console's tiled layout (`victory_formats`) is
//!    copied into the render target, presented and read back, then partly
//!    rewritten and copied again;
//! 4. a translated shader: a compute program assembled by
//!    `victory_shader::native::synth`, in a container, made into a host
//!    pipeline the way a title's is, dispatched, and its stores read back
//!    from title memory (twice, with other constants the second time);
//! 5. a translated draw: a vertex and a pixel program, also from `synth`,
//!    with the signatures they link by and the pixel program's
//!    export patch, made into a host graphics pipeline from a pipeline-state
//!    stream, drawn from a vertex buffer in title memory into the render
//!    target with root constants for each stage, presented and read back
//!    pixel by pixel (one list of one draw, one of two).
//!
//! With `bench`, a few of the layer's own hot paths are timed on synthetic
//! data ([`Rate`]); see `docs/selftest.md` for what they mean.
//!
//! Nothing here runs unless `victory selftest` asks.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use victory_standin::pc::{COMMON, COPY_SOURCE, Obj, RENDER_TARGET, Rect};
use victory_standin::scene::{BufferCopy, Compute, DrawCall, Drawing, LEFT_HALF, PIECE, PIECE_AT, TiledTexture, WHOLE, step};
use victory_standin::{SIDE, Title, hr_text};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

use crate::object::This;

pub use victory_standin::{CreateDevice, Imports, SerializeRoot};

#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// The software adapter (`VICTORY_D3D12_WARP=1`).
    pub warp: bool,
    /// Frames of stage 2.
    pub frames: u32,
    pub bench: bool,
    /// The longest any one step may take before the run is given up as a
    /// hang (`hung` is then called).
    pub patience: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            warp: false,
            frames: 120,
            bench: false,
            patience: Duration::from_secs(60),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Ok(String),
    Fail(String),
    Skip(String),
}

#[derive(Clone, Debug)]
pub struct Stage {
    pub name: &'static str,
    pub outcome: Outcome,
}

/// Why a stage was skipped, in fixed words: the texts are built from these.
const NEEDS_DEVICE: &str = "needs the device stage";
const NEEDS_PRESENT: &str = "needs clear + present";
const NO_SHADER_MODEL: &str = "the adapter has shader model";

impl Stage {
    /// Our fixed wording of why the stage was skipped, for the self-test's
    /// report, which holds no text built at run time (the outcome's own text
    /// holds the adapter's figures); `None` for a stage that ran.
    pub fn skip_reason(&self) -> Option<&'static str> {
        let Outcome::Skip(why) = &self.outcome else {
            return None;
        };
        Some(match why.as_str() {
            NEEDS_DEVICE => NEEDS_DEVICE,
            NEEDS_PRESENT => NEEDS_PRESENT,
            why if why.starts_with(NO_SHADER_MODEL) => "the adapter has no shader model 6.6",
            _ => "skipped",
        })
    }
}

/// A measured rate: `name value unit`.
#[derive(Clone, Debug)]
pub struct Rate {
    pub name: &'static str,
    pub value: f64,
    pub unit: &'static str,
    /// What was measured, in a few words.
    pub what: &'static str,
}

#[derive(Debug, Default)]
pub struct Report {
    pub stages: Vec<Stage>,
    pub rates: Vec<Rate>,
    /// Whether the failure was that there is no host device at all (a PC or
    /// runner without Direct3D 12): callers may treat that apart.
    pub no_device: bool,
}

impl Report {
    pub fn passed(&self) -> bool {
        self.stages.iter().all(|s| !matches!(s.outcome, Outcome::Fail(_)))
    }

    pub fn first_failure(&self) -> Option<&Stage> {
        self.stages.iter().find(|s| matches!(s.outcome, Outcome::Fail(_)))
    }
}

/// The stage names, in order.
pub const STAGES: [&str; 5] = ["Device", "Clear + present", "Unified memory", "Translated shader", "Translated draw"];

/// Runs every stage, telling `said` of each as it finishes. `hung` is
/// called from another thread if a step takes longer than the patience
/// (with the stage it was in); it must end the process.
pub fn run(imports: &Imports, options: &Options, said: &mut dyn FnMut(&Stage), hung: fn(&'static str) -> !) -> Report {
    if options.warp {
        // SAFETY: before the host device exists, on the thread that makes
        // it; nothing of ours reads the environment concurrently yet.
        unsafe { std::env::set_var("VICTORY_D3D12_WARP", "1") };
    }
    Watchdog::start(options.patience, hung);
    let mut report = Report::default();
    let mut bench = Bench::default();
    let mut tell = |report: &mut Report, name: &'static str, outcome: Outcome| {
        let stage = Stage { name, outcome };
        said(&stage);
        report.stages.push(stage);
    };

    Watchdog::arm(STAGES[0]);
    let title = match device(imports, options) {
        Ok((title, detail)) => {
            tell(&mut report, STAGES[0], Outcome::Ok(detail));
            title
        }
        Err(Failed { reason, no_device }) => {
            report.no_device = no_device;
            tell(&mut report, STAGES[0], Outcome::Fail(reason));
            for name in &STAGES[1..] {
                tell(&mut report, name, Outcome::Skip(NEEDS_DEVICE.into()));
            }
            Watchdog::disarm();
            return report;
        }
    };

    Watchdog::arm(STAGES[1]);
    let outcome = clear_and_present(&title, options.frames, &mut bench).into_outcome();
    let presented = matches!(outcome, Outcome::Ok(_));
    tell(&mut report, STAGES[1], outcome);

    Watchdog::arm(STAGES[2]);
    let outcome = if presented {
        unified_memory(&title).into_outcome()
    } else {
        Outcome::Skip(NEEDS_PRESENT.into())
    };
    tell(&mut report, STAGES[2], outcome);

    Watchdog::arm(STAGES[3]);
    let outcome = translated_shader(&title, &mut bench);
    tell(&mut report, STAGES[3], outcome);

    Watchdog::arm(STAGES[4]);
    let outcome = if presented {
        translated_draw(&title, &mut bench)
    } else {
        Outcome::Skip(NEEDS_PRESENT.into())
    };
    tell(&mut report, STAGES[4], outcome);

    if options.bench {
        Watchdog::arm("Benchmark");
        report.rates = rates(&title, bench);
    }
    Watchdog::disarm();
    report
}

/// A stage's failure, in our words.
struct Failed {
    reason: String,
    no_device: bool,
}

impl From<String> for Failed {
    fn from(reason: String) -> Self {
        Failed { reason, no_device: false }
    }
}

impl From<&str> for Failed {
    fn from(reason: &str) -> Self {
        reason.to_owned().into()
    }
}

impl From<victory_standin::Failed> for Failed {
    fn from(failed: victory_standin::Failed) -> Self {
        failed.0.into()
    }
}

trait IntoOutcome {
    fn into_outcome(self) -> Outcome;
}

impl IntoOutcome for Result<String, Failed> {
    fn into_outcome(self) -> Outcome {
        match self {
            Ok(detail) => Outcome::Ok(detail),
            Err(f) => Outcome::Fail(f.reason),
        }
    }
}

/// Gives the run up when a step takes longer than its patience: a hung
/// driver or a submission that never completes would otherwise hang the
/// self-test, and the player learns nothing.
struct Watchdog;

static ARMED: Mutex<Option<(Instant, &'static str)>> = Mutex::new(None);

impl Watchdog {
    fn start(patience: Duration, hung: fn(&'static str) -> !) {
        static STARTED: AtomicBool = AtomicBool::new(false);
        if STARTED.swap(true, Ordering::Relaxed) {
            return;
        }
        let _ = std::thread::Builder::new().name("victory-selftest-watchdog".into()).spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(200));
                let armed = *ARMED.lock().unwrap_or_else(|e| e.into_inner());
                if let Some((since, stage)) = armed
                    && since.elapsed() > patience
                {
                    hung(stage);
                }
            }
        });
    }

    /// From now on, `stage` is being run; the clock starts again.
    fn arm(stage: &'static str) {
        *ARMED.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), stage));
    }

    /// A step that made progress: the clock starts again.
    fn pet() {
        if let Some((since, _)) = ARMED.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            *since = Instant::now();
        }
    }

    fn disarm() {
        *ARMED.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Stage 1: the device through the title's import, the host device behind
/// it (on the software adapter if asked for), the title's objects, and one
/// empty frame presented, so that the swap chain and the window exist.
fn device(imports: &Imports, options: &Options) -> Result<(Title, String), Failed> {
    let device = Title::create_device(imports)?;
    let Some(host) = crate::host::host() else {
        return Err(Failed {
            reason: "no host Direct3D 12 device at feature level 12_0 (the warning above says why)".into(),
            no_device: true,
        });
    };
    let facts = host.adapter_facts();
    let software = facts.as_ref().is_some_and(|a| a.software);
    if options.warp && !software {
        return Err("asked for the software adapter (WARP) and got another".into());
    }
    let adapter = match &facts {
        Some(a) if a.software => format!("{} (software)", a.name),
        Some(a) if a.driver.is_empty() => format!("{}, {} MB", a.name, a.vram_mb),
        Some(a) => format!("{}, driver {}, {} MB", a.name, a.driver, a.vram_mb),
        None => "an adapter DXGI does not describe".into(),
    };
    let title = Title::on(device, *imports, Watchdog::pet)?;
    title.frame(|t| t.clear([0.0, 0.0, 0.0, 1.0]))?;
    title.present()?;
    title.sync()?;
    let presents = host.presents().ok_or("the first present made no swap chain")?;
    if !presents.window {
        return Err("the swap chain has no window".into());
    }
    Ok((title, format!("{adapter}; queue, list, fence and a {}x{} window", presents.size.0, presents.size.1)))
}

fn clear_and_present(title: &Title, frames: u32, bench: &mut Bench) -> Result<String, Failed> {
    let host = crate::host::host().ok_or("no host")?;
    let before = host.presents().map_or(0, |p| p.count);
    let frames = frames.max(1);
    let looked = [0, frames / 2, frames - 1];
    let mut intervals = Vec::with_capacity(frames as usize);
    let mut last = Instant::now();
    for f in 0..frames {
        let bytes = colour_of(f);
        let colour = bytes.map(|b| f32::from(b) / 255.0);
        title.frame(|t| t.clear(colour))?;
        title.present()?;
        title.sync()?;
        let now = Instant::now();
        intervals.push(now - last);
        last = now;
        if looked.contains(&f) {
            let (_, _, pixels) = read_back(title.target)?;
            if let Some(at) = pixels.chunks_exact(4).position(|p| !near(p, &bytes)) {
                let p = &pixels[at * 4..at * 4 + 4];
                return Err(format!(
                    "frame {f}: pixel ({}, {}) is {p:?}, the clear was {bytes:?}",
                    at as u32 % SIDE,
                    at as u32 / SIDE
                )
                .into());
            }
            last = Instant::now();
        }
    }
    let presents = host.presents().ok_or("no swap chain")?;
    let shown = presents.count.wrapping_sub(before);
    if shown < frames {
        return Err(format!(
            "the swap chain took {shown} of {frames} presents (last {})",
            hr_text(presents.last.0)
        )
        .into());
    }
    // The first interval holds the stage's start, the ones after a
    // read-back are left out above.
    intervals.remove(0);
    bench.present = median(&mut intervals);
    Ok(format!("{frames} frames cleared and presented; 3 of them read back and matched"))
}

fn unified_memory(title: &Title) -> Result<String, Failed> {
    let buffer = buffer_round_trip(title)?;
    let texture = texture_round_trip(title)?;
    Ok(format!("{buffer}; {texture}"))
}

/// A buffer the title writes is copied into another by a list; the title
/// reads the copy, changes the first and the change must be there after
/// the next submission. (Neither buffer has a host copy yet, so the
/// layer copies at submission, `HostList::defer_copy`; the copy on the
/// card and its landing are stage 4's.)
fn buffer_round_trip(title: &Title) -> Result<String, Failed> {
    let mut buffers = BufferCopy::new(title, 1 << 20)?;
    title.frame(|t| {
        buffers.record(t);
        Ok(())
    })?;
    title.sync()?;
    if let Some(at) = buffers.differs() {
        return Err(format!("the buffer copy differs at byte {at}").into());
    }
    buffers.change(0);
    title.frame(|t| {
        buffers.record(t);
        Ok(())
    })?;
    title.sync()?;
    if let Some(at) = buffers.differs() {
        return Err(format!("after the title changed the source, the copy differs at byte {at}").into());
    }
    Ok("a 1 MiB buffer copied, changed and copied again".into())
}

/// A texture the title fills in the console's tiled layout is copied into
/// the render target, presented and read back; then part of it is
/// rewritten in title memory and the next copy must show that part.
fn texture_round_trip(title: &Title) -> Result<String, Failed> {
    let mut texture = TiledTexture::new(title)?;
    let copied = |t: &Title, texture: &TiledTexture| {
        texture.record_copy(t);
        Ok(())
    };
    title.frame(|t| copied(t, &texture))?;
    title.present()?;
    title.sync()?;
    let (_, _, pixels) = read_back(title.target)?;
    if let Some(at) = first_far(&pixels, texture.picture()) {
        return Err(format!("the tiled texture came through wrong at pixel ({}, {})", at % SIDE, at / SIDE).into());
    }
    let changed = texture.rewrite_block([250, 10, 200, 255])?;
    title.frame(|t| copied(t, &texture))?;
    title.present()?;
    title.sync()?;
    let (_, _, pixels) = read_back(title.target)?;
    if let Some(at) = first_far(&pixels, texture.picture()) {
        return Err(format!(
            "after the title rewrote {changed} bytes of the texture, pixel ({}, {}) is still old",
            at % SIDE,
            at / SIDE
        )
        .into());
    }
    Ok(format!("a {SIDE}x{SIDE} tiled texture uploaded, presented, partly rewritten and seen again"))
}

/// Why translated shaders cannot run on this adapter, if they cannot.
fn no_translated_shaders() -> Option<String> {
    let Some(host) = crate::host::host() else {
        return Some("no device".into());
    };
    let mut model = D3D12_FEATURE_DATA_SHADER_MODEL { HighestShaderModel: D3D_SHADER_MODEL_6_6 };
    // SAFETY: a plain query into our structure.
    let asked = unsafe {
        host.device.CheckFeatureSupport(D3D12_FEATURE_SHADER_MODEL, (&raw mut model).cast(), size_of_val(&model) as u32)
    };
    (asked.is_err() || model.HighestShaderModel.0 < D3D_SHADER_MODEL_6_6.0).then(|| {
        format!(
            "{NO_SHADER_MODEL} {}.{}; translated shaders need 6.6",
            model.HighestShaderModel.0 >> 4,
            model.HighestShaderModel.0 & 15
        )
    })
}

fn translated_shader(title: &Title, bench: &mut Bench) -> Outcome {
    if let Some(why) = no_translated_shaders() {
        return Outcome::Skip(why);
    }
    compute(title, bench).into_outcome()
}

fn translated_draw(title: &Title, bench: &mut Bench) -> Outcome {
    if let Some(why) = no_translated_shaders() {
        return Outcome::Skip(why);
    }
    draw(title, bench).into_outcome()
}

/// A pipeline's first use, then a wait for its host pipeline: it is made
/// on first use, by the pipeline workers (`pipeline/make.rs`), and a draw
/// or dispatch before it is ready would be dropped, as a title's is. How
/// long it took.
fn first_use(title: &Title, pipeline: Obj, refused: &str) -> Result<Duration, Failed> {
    let began = Instant::now();
    title.frame(|t| {
        t.set_pipeline(pipeline);
        Ok(())
    })?;
    title.sync()?;
    while crate::pipeline::host_pipeline(pipeline.cast()).is_none() {
        if began.elapsed() > Duration::from_secs(30) {
            return Err(refused.into());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(began.elapsed())
}

/// The synthetic vertex and pixel programs, from their containers to
/// the pixels they draw: a pipeline-state stream given to the device's
/// `CreatePipelineState`, then targets, viewport, scissor, topology, a
/// vertex buffer in title memory, root constants and draws through the
/// list's table, presented and read back. Twice: one draw over the
/// whole target, then two (other constants; the second cut to the left
/// half by its scissor) in one list.
fn draw(title: &Title, bench: &mut Bench) -> Result<String, Failed> {
    let drawing = Drawing::new(title)?;
    bench.graphics = Some(first_use(
        title,
        drawing.pipeline,
        "no host graphics pipeline 30 s after the pipeline's first use (the host refused it or a program did not \
         translate, see the log)",
    )?);
    let rounds: [&[DrawCall]; 2] = [
        &[DrawCall { vs: [step(40), step(200), step(90), step(17)], ps: step(230), scissor: WHOLE }],
        &[
            DrawCall { vs: [step(250), step(5), step(128), step(0)], ps: step(64), scissor: WHOLE },
            DrawCall { vs: [step(16), step(96), step(240), step(255)], ps: step(255), scissor: LEFT_HALF },
        ],
    ];
    let mut draws = 0;
    for (round, draws_of) in rounds.iter().enumerate() {
        let ran_before = crate::draw::drawn();
        title.frame(|t| {
            t.transition(t.target, COMMON, RENDER_TARGET);
            t.clear_target([0.0, 0.0, 0.0, 0.0]);
            drawing.record(t, draws_of);
            t.transition(t.target, RENDER_TARGET, COMMON);
            Ok(())
        })?;
        title.present()?;
        title.sync()?;
        let ran = crate::draw::drawn() - ran_before;
        if ran < draws_of.len() as u64 {
            return Err(format!(
                "list {}: {ran} of {} draws ran on the host; the others were dropped",
                round + 1,
                draws_of.len()
            )
            .into());
        }
        draws += draws_of.len();
        let (_, _, pixels) = read_back(title.target)?;
        for (at, pixel) in pixels.chunks_exact(4).enumerate() {
            let (x, y) = (at as u32 % SIDE, at as u32 / SIDE);
            // The last draw whose scissor holds the pixel.
            let inside = |r: &Rect| (x as i32) < r.right && (y as i32) < r.bottom;
            let Some(last) = draws_of.iter().rev().find(|d| inside(&d.scissor)) else { continue };
            let want = last.colour();
            if !near(pixel, &want) {
                return Err(format!(
                    "list {}: pixel ({x}, {y}) is {pixel:?}, the programs draw {want:?} there",
                    round + 1
                )
                .into());
            }
        }
    }
    Ok(format!(
        "a {}-byte vertex and a {}-byte pixel container translated, made a host graphics pipeline from a \
         pipeline stream; {draws} draws in 2 lists presented and read back, every pixel matched",
        drawing.vs.len(),
        drawing.ps.len()
    ))
}

/// The synthetic compute program, from its container to the values it
/// stores in title memory.
fn compute(title: &Title, bench: &mut Bench) -> Result<String, Failed> {
    use victory_shader::native::synth;
    let container = synth::container(&[synth::pattern_store()], &[]);
    // The translation a pipeline's first use makes, timed apart (the
    // pipeline below translates again: there is no cache on disk here).
    let began = Instant::now();
    crate::shader_cache::host_shader(&container).map_err(|e| format!("the translator refused our program: {e}"))?;
    bench.translate = Some(began.elapsed());
    let mut compute = Compute::new(title)?;
    bench.pipeline = Some(first_use(
        title,
        compute.pipeline,
        "no host pipeline 30 s after the pipeline's first use (the host refused it, see the log)",
    )?);
    for (round, constants) in [[0x1234_5678u32, 7, 0, 0], [0x0bad_f00d, 1000, 0, 0]].into_iter().enumerate() {
        title.frame(|t| {
            compute.record_dispatch(t, constants);
            Ok(())
        })?;
        title.sync()?;
        compute.check_stores(constants, &format!("dispatch {}", round + 1))?;
    }
    // The shader's buffer has a host copy now, so a list's copy into it
    // is made on the card at its place in the list (`host/place.rs`) and
    // must come back into title memory as the shader's stores did.
    let on_card = crate::shadow::peek(compute.memory().address() + PIECE_AT as u64, PIECE as u64).is_some();
    compute.write_piece(0);
    title.frame(|t| {
        compute.record_copy_in(t);
        Ok(())
    })?;
    title.sync()?;
    compute.check_copy_in()?;
    Ok(format!(
        "a {}-byte compute container translated, made a host pipeline, dispatched twice; its stores and a list's copy \
         {} landed in title memory",
        container.len(),
        if on_card { "on the card" } else { "at submission" }
    ))
}

fn rates(title: &Title, measured: Bench) -> Vec<Rate> {
    let mut rates = Vec::new();
    if let Some(d) = measured.present {
        rates.push(Rate { name: "present_interval", value: ms(d), unit: "ms", what: "one cleared 64x64 frame, presented and waited for (median)" });
    }
    if let Ok(r) = bench_copy(title) {
        rates.push(r);
    }
    if let Some(r) = bench_watch() {
        rates.push(r);
    }
    if let Some(r) = bench_compare() {
        rates.push(r);
    }
    if let Ok(r) = bench_recording(title) {
        rates.push(r);
    }
    if let Some(d) = measured.translate {
        rates.push(Rate { name: "shader_translation", value: ms(d), unit: "ms", what: "the synthetic compute container to DXIL" });
    }
    if let Some(d) = measured.pipeline {
        rates.push(Rate { name: "pipeline_creation", value: ms(d), unit: "ms", what: "first use to a host compute pipeline, translation included" });
    }
    if let Some(d) = measured.graphics {
        rates.push(Rate {
            name: "graphics_pipeline_creation",
            value: ms(d),
            unit: "ms",
            what: "first use to a host vertex and pixel pipeline, both translations included",
        });
    }
    rates
}

/// A 64 MiB title buffer, all of it changed by the title before each
/// copy, copied by a list into another and waited for.
fn bench_copy(title: &Title) -> Result<Rate, Failed> {
    const LEN: usize = 64 << 20;
    let mut buffers = BufferCopy::new(title, LEN)?;
    let mut times = Vec::new();
    for round in 0..6u8 {
        buffers.fill(round);
        let began = Instant::now();
        title.frame(|t| {
            buffers.record(t);
            Ok(())
        })?;
        title.sync()?;
        times.push(began.elapsed());
    }
    times.remove(0);
    let t = median(&mut times).ok_or("no rounds")?;
    Ok(Rate {
        name: "buffer_copy",
        value: LEN as f64 / (1 << 20) as f64 / t.as_secs_f64(),
        unit: "MB/s",
        what: "64 MiB the title rewrote, copied by a list into another title buffer (median of 5)",
    })
}

/// Commands a list records per second: render-target clears of one
/// pixel, which go through the list's table and onto the host list.
fn bench_recording(title: &Title) -> Result<Rate, Failed> {
    const CALLS: u32 = 20_000;
    let mut took = Duration::ZERO;
    title.frame(|t| {
        t.transition(t.target, COMMON, RENDER_TARGET);
        let one = [Rect { left: 0, top: 0, right: 1, bottom: 1 }];
        let began = Instant::now();
        for i in 0..CALLS {
            let c = (i % 255) as f32 / 255.0;
            t.clear_rects([c, c, c, 1.0], &one);
        }
        took = began.elapsed();
        t.transition(t.target, RENDER_TARGET, COMMON);
        Ok(())
    })?;
    title.sync()?;
    Ok(Rate {
        name: "list_recording",
        value: f64::from(CALLS) / took.as_secs_f64(),
        unit: "calls/s",
        what: "ClearRenderTargetView calls through the list's table onto the host list",
    })
}

/// What stage 2 and the shader stages measured for the benchmark.
#[derive(Default)]
struct Bench {
    present: Option<Duration>,
    translate: Option<Duration>,
    pipeline: Option<Duration>,
    graphics: Option<Duration>,
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn median(times: &mut [Duration]) -> Option<Duration> {
    times.sort();
    times.get(times.len() / 2).copied()
}

/// The colour of frame `f`: different from one frame to the next.
fn colour_of(f: u32) -> [u8; 4] {
    [(f * 37 % 256) as u8, (255 - f * 11 % 256) as u8, (f * 101 % 256) as u8, 255]
}

/// Within one unit of each channel (a conversion of `n / 255` may round).
fn near(a: &[u8], b: &[u8]) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(*y) <= 1)
}

/// The first pixel of `a` not [`near`] the same one of `b`.
fn first_far(a: &[u8], b: &[u8]) -> Option<u32> {
    a.chunks_exact(4).zip(b.chunks_exact(4)).position(|(x, y)| !near(x, y)).map(|i| i as u32)
}

/// The host copy of a title texture of RGBA8, read back: width, height and
/// tight rows. The title's work must have been waited for.
fn read_back(texture: Obj) -> Result<(u32, u32, Vec<u8>), Failed> {
    let texture: This = texture.cast();
    let host = crate::host::host().ok_or("no host")?;
    let image = crate::resource::host_resource(texture).ok_or("the texture has no host copy")?;
    // SAFETY: a live resource.
    let desc = unsafe { image.GetDesc() };
    let (width, height) = (desc.Width as u32, desc.Height);
    let pitch = (width * 4).next_multiple_of(D3D12_TEXTURE_DATA_PITCH_ALIGNMENT);
    let size = u64::from(pitch) * u64::from(height);
    let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
    let buffer = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut back: Option<ID3D12Resource> = None;
    // SAFETY: plain creation of a readback buffer.
    unsafe {
        host.device
            .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &buffer, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut back)
            .map_err(|e| format!("no readback buffer: {e}"))?;
    }
    let back = back.ok_or("no readback buffer")?;
    let list = host.new_list().map_err(|e| format!("no host list: {e}"))?;
    let before = crate::resource::host_transition(texture, COPY_SOURCE).map_or(COPY_SOURCE, |(_, b)| b);
    let state = |s: u32| D3D12_RESOURCE_STATES(s as i32);
    // SAFETY: an open list, live resources, locations in the host layout;
    // the borrowed pointers in the locations are never released by them.
    unsafe {
        if before != COPY_SOURCE {
            list.list.ResourceBarrier(&[crate::host::transition(image, state(before), D3D12_RESOURCE_STATE_COPY_SOURCE)]);
        }
        let to = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&back),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: desc.Format, Width: width, Height: height, Depth: 1, RowPitch: pitch },
                },
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(image),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
        };
        list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
        if before != COPY_SOURCE {
            list.list.ResourceBarrier(&[crate::host::transition(image, D3D12_RESOURCE_STATE_COPY_SOURCE, state(before))]);
        }
    }
    if before != COPY_SOURCE {
        crate::resource::host_transition(texture, before);
    }
    list.close();
    host.execute(&[&list]);
    Watchdog::pet();
    let mut at = std::ptr::null_mut();
    // SAFETY: the copy has run (`execute` waits); the buffer holds `size`
    // bytes.
    let rows = unsafe {
        back.Map(0, Some(&D3D12_RANGE { Begin: 0, End: size as usize }), Some(&mut at)).map_err(|e| format!("readback Map: {e}"))?;
        std::slice::from_raw_parts(at.cast::<u8>(), size as usize)
    };
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height as usize {
        let row = y * pitch as usize;
        pixels.extend_from_slice(&rows[row..row + width as usize * 4]);
    }
    // SAFETY: mapped above.
    unsafe { back.Unmap(0, None) };
    Ok((width, height, pixels))
}

/// What asking the write watch costs: `crate::watch::writes` over 256 MiB of
/// write-watched memory with one byte written every 64 KiB, per GiB
/// tracked.
fn bench_watch() -> Option<Rate> {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree};
    const MEM_WRITE_WATCH: u32 = 0x20_0000;
    const LEN: usize = 256 << 20;
    // SAFETY: a fresh allocation of ours, written inside its bounds and
    // freed at the end.
    unsafe {
        let at = VirtualAlloc(std::ptr::null(), LEN, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE).cast::<u8>();
        if at.is_null() {
            return None;
        }
        let mut times = Vec::new();
        let mut answered = true;
        for round in 0..8u8 {
            for i in (0..LEN).step_by(64 << 10) {
                at.add(i).write(round);
            }
            let began = Instant::now();
            answered &= crate::watch::writes(at as usize, at as usize + LEN, false).is_some();
            times.push(began.elapsed());
        }
        VirtualFree(at.cast(), 0, MEM_RELEASE);
        if !answered {
            return None;
        }
        times.remove(0);
        let t = median(&mut times)?;
        Some(Rate {
            name: "write_watch_query",
            value: t.as_secs_f64() * 1e6 * f64::from(1u32 << 30) / LEN as f64,
            unit: "us/GiB",
            what: "asking which pages of write-watched memory were written (4096 of 65536 pages)",
        })
    }
}

/// The AVX2 compare that finds what changed (`crate::safecopy::differ`),
/// over two equal 64 MiB buffers, so that it reads all of both.
fn bench_compare() -> Option<Rate> {
    if !std::arch::is_x86_feature_detected!("avx2") {
        return None;
    }
    const LEN: usize = 64 << 20;
    let a = vec![0x5au8; LEN];
    let b = a.clone();
    let mut times = Vec::new();
    for _ in 0..6 {
        let began = Instant::now();
        // SAFETY: two buffers of ours of `LEN` bytes; the processor has AVX2.
        let differ = unsafe { crate::safecopy::differ(a.as_ptr(), b.as_ptr(), LEN) };
        times.push(began.elapsed());
        if differ {
            return None;
        }
    }
    times.remove(0);
    let t = median(&mut times)?;
    Some(Rate {
        name: "avx2_compare",
        value: 2.0 * LEN as f64 / 1e9 / t.as_secs_f64(),
        unit: "GB/s",
        what: "bytes read comparing two equal 64 MiB buffers (median of 5)",
    })
}

#[cfg(test)]
mod tests {
    use std::mem::offset_of;

    use victory_standin::pc;

    use super::*;

    /// The stand-in's structures are laid out as the host's bindings of the
    /// D3D12 headers say, field for field where a title's code reads them.
    #[test]
    fn structures_have_the_pc_layouts() {
        use pc::Sub;
        assert_eq!(size_of::<pc::ResourceDesc>(), size_of::<D3D12_RESOURCE_DESC>());
        assert_eq!(offset_of!(pc::ResourceDesc, layout), offset_of!(D3D12_RESOURCE_DESC, Layout));
        assert_eq!(size_of::<pc::Barrier>(), size_of::<D3D12_RESOURCE_BARRIER>());
        assert_eq!(size_of::<pc::RawUav>(), 40);
        assert_eq!(size_of::<pc::ComputeDesc>(), size_of::<D3D12_COMPUTE_PIPELINE_STATE_DESC>());
        assert_eq!(offset_of!(pc::ComputeDesc, flags), offset_of!(D3D12_COMPUTE_PIPELINE_STATE_DESC, Flags));
        assert_eq!(offset_of!(pc::Planes, resources), 0x10);
        assert_eq!(size_of::<pc::Planes>(), 0x90);
        assert_eq!(size_of::<pc::QueueDesc>(), size_of::<D3D12_COMMAND_QUEUE_DESC>());
        assert_eq!(size_of::<pc::HeapProperties>(), size_of::<D3D12_HEAP_PROPERTIES>());
        assert_eq!(size_of::<pc::VertexView>(), size_of::<D3D12_VERTEX_BUFFER_VIEW>());
        assert_eq!(size_of::<pc::Code>(), size_of::<D3D12_SHADER_BYTECODE>());
        assert_eq!(size_of::<pc::Viewport>(), size_of::<D3D12_VIEWPORT>());
        // `D3D12_RECT` is `RECT`: four 32-bit edges.
        assert_eq!(size_of::<pc::Rect>(), 16);
        // The demo title's read back of its picture (`scene::ReadBack`).
        assert_eq!(size_of::<pc::PlacedFootprint>(), size_of::<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>());
        assert_eq!(size_of::<pc::CopyLocation>(), size_of::<D3D12_TEXTURE_COPY_LOCATION>());
        assert_eq!(offset_of!(pc::CopyLocation, footprint), offset_of!(D3D12_TEXTURE_COPY_LOCATION, Anonymous));
        assert_eq!(pc::READBACK_HEAP, D3D12_HEAP_TYPE_READBACK.0 as u32);
        assert_eq!(pc::COPY_FOOTPRINT, D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT.0 as u32);
        // The two sides of one header: what the stand-in passes and what
        // the layer checked its layouts against.
        assert_eq!(victory_standin::INTERFACE_VERSION, crate::device::CHECKED_VERSION);
        assert_eq!(size_of::<pc::VirtualMemory>(), size_of::<crate::device::VirtualMemory>());
        assert_eq!(size_of::<pc::BlendTarget>(), size_of::<D3D12_RENDER_TARGET_BLEND_DESC>());
        assert_eq!(offset_of!(pc::BlendTarget, write_mask), offset_of!(D3D12_RENDER_TARGET_BLEND_DESC, RenderTargetWriteMask));
        assert_eq!(size_of::<pc::BlendDesc>(), size_of::<D3D12_BLEND_DESC>());
        assert_eq!(size_of::<pc::RasterizerDesc>(), size_of::<D3D12_RASTERIZER_DESC>());
        assert_eq!(offset_of!(pc::RasterizerDesc, depth_clip), offset_of!(D3D12_RASTERIZER_DESC, DepthClipEnable));
        assert_eq!(size_of::<pc::DepthStencilDesc1>(), size_of::<D3D12_DEPTH_STENCIL_DESC1>());
        assert_eq!(offset_of!(pc::DepthStencilDesc1, front), offset_of!(D3D12_DEPTH_STENCIL_DESC1, FrontFace));
        assert_eq!(offset_of!(pc::DepthStencilDesc1, depth_bounds_test), offset_of!(D3D12_DEPTH_STENCIL_DESC1, DepthBoundsTestEnable));
        assert_eq!(size_of::<pc::InputElement>(), size_of::<D3D12_INPUT_ELEMENT_DESC>());
        assert_eq!(offset_of!(pc::InputElement, class), offset_of!(D3D12_INPUT_ELEMENT_DESC, InputSlotClass));
        assert_eq!(size_of::<pc::InputLayout>(), size_of::<D3D12_INPUT_LAYOUT_DESC>());
        assert_eq!(size_of::<pc::TargetFormats>(), size_of::<D3D12_RT_FORMAT_ARRAY>());
        assert_eq!(size_of::<pc::SampleDesc>(), size_of::<DXGI_SAMPLE_DESC>());
        assert_eq!(size_of::<pc::DescriptorRange>(), size_of::<D3D12_DESCRIPTOR_RANGE>());
        assert_eq!(size_of::<pc::RootParameter>(), size_of::<D3D12_ROOT_PARAMETER>());
        assert_eq!(offset_of!(pc::RootParameter, visibility), offset_of!(D3D12_ROOT_PARAMETER, ShaderVisibility));
        assert_eq!(size_of::<pc::RootDesc>(), size_of::<D3D12_ROOT_SIGNATURE_DESC>());
        assert_eq!(offset_of!(pc::RootDesc, flags), offset_of!(D3D12_ROOT_SIGNATURE_DESC, Flags));
        // Each subobject is as long as the layer's walk of a stream takes
        // it to be (`pipeline::subobject`, D3D12's padding to a pointer).
        assert_eq!(size_of::<Sub<pc::Obj>>(), 16);
        assert_eq!(size_of::<Sub<pc::Code>>(), 24);
        assert_eq!(size_of::<Sub<pc::BlendDesc>>(), 336);
        assert_eq!(size_of::<Sub<u32>>(), 8);
        assert_eq!(size_of::<Sub<pc::RasterizerDesc>>(), 48);
        assert_eq!(size_of::<Sub<pc::DepthStencilDesc1>>(), 64);
        assert_eq!(size_of::<Sub<pc::InputLayout>>(), 24);
        assert_eq!(size_of::<Sub<pc::TargetFormats>>(), 40);
        assert_eq!(size_of::<Sub<pc::SampleDesc>>(), 16);
        assert_eq!(offset_of!(pc::GraphicsStream, samples), 0x258);
        assert_eq!(size_of::<pc::GraphicsStream>(), 0x268);
    }

    /// The values the stand-in writes into those structures are the
    /// headers' (it names them by number, having no bindings).
    #[test]
    fn values_are_the_pc_ones() {
        assert_eq!(D3D12_BLEND_ONE.0, 2);
        assert_eq!(D3D12_BLEND_ZERO.0, 1);
        assert_eq!(D3D12_BLEND_OP_ADD.0, 1);
        assert_eq!(D3D12_LOGIC_OP_NOOP.0, 4);
        assert_eq!(D3D12_FILL_MODE_SOLID.0, 3);
        assert_eq!(D3D12_CULL_MODE_NONE.0, 1);
        assert_eq!(D3D12_STENCIL_OP_KEEP.0, 1);
        assert_eq!(D3D12_COMPARISON_FUNC_ALWAYS.0, 8);
        assert_eq!(D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE.0 as u32, pc::ROOT_TABLE);
        assert_eq!(D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS.0 as u32, pc::ROOT_CONSTANTS);
        assert_eq!(D3D12_DESCRIPTOR_RANGE_TYPE_UAV.0 as u32, pc::RANGE_UAV);
        assert_eq!(D3D12_SHADER_VISIBILITY_VERTEX.0 as u32, pc::VISIBLE_VERTEX);
        assert_eq!(D3D12_SHADER_VISIBILITY_PIXEL.0 as u32, pc::VISIBLE_PIXEL);
        assert_eq!(D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT.0 as u32, pc::ROOT_INPUT_LAYOUT);
        assert_eq!(D3D12_RESOURCE_STATE_COPY_DEST.0 as u32, pc::COPY_DEST);
        assert_eq!(D3D12_RESOURCE_STATE_COPY_SOURCE.0 as u32, pc::COPY_SOURCE);
        assert_eq!(D3D12_RESOURCE_STATE_RENDER_TARGET.0 as u32, pc::RENDER_TARGET);
        assert_eq!(windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R32G32_FLOAT.0 as u32, pc::RG32_FLOAT);
        assert_eq!(windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM.0 as u32, pc::RGBA8);
    }

    #[test]
    fn frames_differ_in_colour() {
        assert!((0..120).all(|f| colour_of(f) != colour_of(f + 1)));
        assert!(near(&[10, 20, 30, 255], &[11, 19, 30, 255]));
        assert!(!near(&[10, 20, 30, 255], &[12, 20, 30, 255]));
    }
}
