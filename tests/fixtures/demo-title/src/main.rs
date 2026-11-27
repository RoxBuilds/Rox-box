//! The open demo title: an executable of our own that does what a small
//! console title does, so that `victory run <demo-title.exe> --execute` is a
//! whole run (loader, import binding, the title's thread, the runtime, the
//! graphics layer, the run report) that anyone may perform and CI performs
//! on every change (docs/demo-title.md).
//!
//! Like a title, it imports the console's graphics entry points from
//! `d3d12_xs.dll`, which no PC has, so only Rox-box can run it; it reads its
//! arguments (the ones after `--`) both from the process's command line
//! (`GetCommandLineW`, through Rust's `std::env::args`, and
//! `GetCommandLineA`) and from its C runtime (`__p___argc`, `__p___argv`),
//! and stops if they disagree; it sets the graphics address range up
//! (`D3D12ConfigureVirtualMemoryX`) before it makes its device, paces its
//! frames with the device's frame interval and frame-origin waits, presents
//! each frame with the token its wait handed out, and ends by returning
//! from `main` (its C runtime then calls `exit`), or as asked: with a code
//! returned from `main`, through the C runtime's `exit`, or through
//! `ExitProcess`, which skips the C runtime.
//!
//! Each frame is the self-test's scene in one list (`victory_standin`, the
//! code `victory selftest` drives too): a 1 MiB buffer the title changes in
//! three places, copied into another; a translated compute dispatch storing
//! into title memory, then a piece copied into the same buffer; the tiled
//! texture, partly rewritten by the title, copied into the render target;
//! part of the target cleared; two translated vertex and pixel draws, each
//! cut to a quarter by its scissor; then the present. One submission, one
//! dispatch and two draws a frame. After each frame it checks what a title
//! can check by its own means, the values that came back into its own
//! memory, and stops at the first mismatch with one line saying what and
//! where (exit code 1).
//!
//! `--frames <n>`: frames to render (default 300, five seconds at the 60 a
//! second it asks for). After them, `--return <code>` returns `code` from
//! `main`, `--exit <code>` calls the C runtime's `exit(code)`, and
//! `--exit-process <code>` calls `ExitProcess(code)` (one of the three).
//! `--alive`: it ends so with two threads waiting for ever, the late DLL
//! loaded and a last frame submitted and not waited for.
//!
//! Its picture is checked by its own means too: each frame the render
//! target is copied into a buffer it maps (`CopyTextureRegion` into a
//! read-back heap) and compared, pixel by pixel, with what it drew.
//!
//! Beside its frames it runs threads of its own (`threads.rs`: made by
//! `CreateThread`, `_beginthreadex` and `std::thread`, ended by returning,
//! `ExitThread` and `_endthreadex`), each with static TLS in the executable
//! and in two DLLs of its own (`dlls.rs`): `demo_static.dll`, which it
//! imports, and `demo_late.dll`, which it loads by name, calls through
//! `GetProcAddress` and frees. It checks what the images' TLS callbacks and
//! `DllMain`s heard of the process and of each thread, and, at its very end
//! (`tls.rs`), that each DLL heard of the process's end once.

mod dlls;
mod threads;
mod tls;
mod win;

use std::ffi::{CStr, c_char, c_void};
use std::io::Write;
use std::process::ExitCode;

use victory_standin::pc::{
    COMMON, COPY_DEST, COPY_SOURCE, DEFAULT_HEAP, HRESULT, Iid, Obj, RENDER_TARGET, Rect,
    UPLOAD_HEAP, VirtualMemory,
};
use victory_standin::scene::{
    BufferCopy, Compute, DrawCall, Drawing, ReadBack, TiledTexture, first_far, step,
};
use victory_standin::{Failed, Imports, SIDE, Title, check};

#[link(name = "d3d12_xs", kind = "raw-dylib")]
unsafe extern "system" {
    fn D3D12ConfigureVirtualMemoryX(config: *mut VirtualMemory) -> HRESULT;
    fn D3D12XboxCreateDevice(
        adapter: *const c_void,
        parameters: *const u32,
        iid: *const Iid,
        out: *mut Obj,
    ) -> HRESULT;
    fn D3D12SerializeRootSignature(
        desc: *const c_void,
        version: u32,
        blob: *mut *mut c_void,
        error: *mut *mut c_void,
    ) -> HRESULT;
}

// The C runtime's argument list and its `exit` (`api-ms-win-crt-runtime`).
unsafe extern "C" {
    fn __p___argc() -> *mut i32;
    fn __p___argv() -> *mut *mut *mut c_char;
    fn exit(code: i32) -> !;
}

// The process's command line, and its end without the C runtime.
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCommandLineA() -> *const c_char;
    fn GetCommandLineW() -> *const u16;
    fn ExitProcess(code: u32) -> !;
}

extern "system" fn create_device(
    adapter: *const c_void,
    parameters: *const u32,
    iid: *const Iid,
    out: *mut Obj,
) -> HRESULT {
    // SAFETY: the import, with the arguments the stand-in passes it.
    unsafe { D3D12XboxCreateDevice(adapter, parameters, iid, out) }
}

extern "system" fn serialize_root(
    desc: *const c_void,
    version: u32,
    blob: *mut *mut c_void,
    error: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: as above.
    unsafe { D3D12SerializeRootSignature(desc, version, blob, error) }
}

const FRAMES: u32 = 300;
/// 60 frames a second.
const INTERVAL_US: u32 = 16_667;

fn main() -> ExitCode {
    // A title's command line reads the same however it is read: the C
    // runtime's list and the process's line (which Rust's `std::env::args`
    // parses, as the C runtime does).
    let (crt, win32) = (crt_args(), win32_args());
    if crt != win32 {
        println!("demo title failed: its command line (GetCommandLineW) and its C runtime's arguments disagree");
        return ExitCode::FAILURE;
    }
    if !lines_agree() {
        println!("demo title failed: GetCommandLineA and GetCommandLineW say different command lines");
        return ExitCode::FAILURE;
    }
    if crt.first().is_none_or(|program| program.contains(['\\', '/'])) {
        println!("demo title failed: its command line does not start with its own name alone");
        return ExitCode::FAILURE;
    }
    let (frames, end, alive) = match asked(&crt[1..]) {
        Ok(asked) => asked,
        Err(problem) => {
            println!(
                "demo title: {problem} (it takes --frames <n>, 1 or more, one of --return, --exit \
                 or --exit-process <code>, and --alive)"
            );
            return ExitCode::from(2);
        }
    };
    match run(frames, alive) {
        Ok(lines) => {
            for line in lines {
                println!("demo title: {line}");
            }
            println!(
                "demo title: {frames} frames rendered and presented; every value it read back from its own memory matched"
            );
            let _ = std::io::stdout().flush();
            match end {
                End::Return(code) => ExitCode::from(code),
                // SAFETY: the C runtime's `exit`; nothing of ours runs after it.
                End::Exit(code) => unsafe { exit(code) },
                // SAFETY: as above, without the C runtime's exit functions.
                End::ExitProcess(code) => unsafe { ExitProcess(code) },
            }
        }
        Err(Failed(reason)) => {
            println!("demo title failed: {reason}");
            ExitCode::FAILURE
        }
    }
}

/// How the demo ends once its frames are done.
enum End {
    Return(u8),
    Exit(i32),
    ExitProcess(u32),
}

/// The program's name and the arguments, from the C runtime.
fn crt_args() -> Vec<String> {
    // SAFETY: the C runtime's own list, made before `main` and never
    // changed: `argc` NUL-terminated strings.
    unsafe {
        let (argc, argv) = (*__p___argc(), *__p___argv());
        if argv.is_null() {
            return Vec::new();
        }
        (0..argc.max(0) as usize)
            .map(|i| *argv.add(i))
            .filter(|a| !a.is_null())
            .map(|a| CStr::from_ptr(a).to_string_lossy().into_owned())
            .collect()
    }
}

/// The same from the process's command line (`GetCommandLineW`, which
/// Rust's standard library parses).
fn win32_args() -> Vec<String> {
    std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

/// Whether the narrow and the wide command line are the same text (where
/// it is plain ASCII, as the demo's is: the narrow one is in the ANSI code
/// page).
fn lines_agree() -> bool {
    // SAFETY: both return the process's NUL-terminated command line, which
    // lives as long as the process.
    unsafe {
        let narrow = CStr::from_ptr(GetCommandLineA()).to_bytes();
        let wide = GetCommandLineW();
        let wide = std::slice::from_raw_parts(wide, (0..).take_while(|&i| *wide.add(i) != 0).count());
        !narrow.is_ascii() || narrow.iter().map(|&b| u16::from(b)).eq(wide.iter().copied())
    }
}

fn asked(args: &[String]) -> Result<(u32, End, bool), String> {
    let mut args = args.iter();
    let mut frames = FRAMES;
    let mut end = None;
    let mut alive = false;
    let code = |what: &str, arg: Option<&String>| -> Result<u32, String> {
        arg.and_then(|n| n.parse().ok())
            .filter(|&n: &u32| n <= 255)
            .ok_or_else(|| format!("{what} needs a code from 0 to 255"))
    };
    while let Some(arg) = args.next() {
        let ends = match arg.as_str() {
            "--frames" => {
                frames = args
                    .next()
                    .and_then(|n| n.parse().ok())
                    .filter(|&n| n > 0)
                    .ok_or("--frames needs a number")?;
                None
            }
            "--return" => Some(End::Return(code(arg, args.next())? as u8)),
            "--exit" => Some(End::Exit(code(arg, args.next())? as i32)),
            "--exit-process" => Some(End::ExitProcess(code(arg, args.next())?)),
            "--alive" => {
                alive = true;
                None
            }
            other => return Err(format!("unknown argument {other:?}")),
        };
        if let Some(ends) = ends {
            if end.is_some() {
                return Err("only one of --return, --exit and --exit-process".to_owned());
            }
            end = Some(ends);
        }
    }
    Ok((frames, end.unwrap_or(End::Return(0)), alive))
}

/// The quarters of the target: the two draws take the top ones, the clear
/// the bottom left, and the texture shows in the bottom right.
const HALF: i32 = SIDE as i32 / 2;
const TOP_LEFT: Rect = Rect {
    left: 0,
    top: 0,
    right: HALF,
    bottom: HALF,
};
const TOP_RIGHT: Rect = Rect {
    left: HALF,
    top: 0,
    right: 2 * HALF,
    bottom: HALF,
};
const BOTTOM_LEFT: Rect = Rect {
    left: 0,
    top: HALF,
    right: HALF,
    bottom: 2 * HALF,
};

/// The frames, and beside them its threads and DLLs; the lines that say
/// what was checked (without the demo's name), or the first mismatch.
fn run(frames: u32, alive: bool) -> Result<Vec<String>, Failed> {
    // SAFETY: a plain query of the calling thread.
    let main_thread = unsafe { win::GetCurrentThreadId() };
    // Before anything uses them: the executable heard of its process's
    // start once, and its imported DLL was attached once, on this thread
    // (the loader attaches it on the title's thread, before the entry point).
    let attaches = tls::process_attaches();
    if attaches != 1 {
        return Err(format!("its executable's TLS callback heard of the process's start {attaches} times, not once").into());
    }
    let statics = dlls::Dll::imported();
    let static_counts = dlls::attached_before_use(&statics, main_thread)?;
    let mut range = VirtualMemory {
        flags: 0,
        page_table_pages: 0,
        preserved_bytes: 0,
        lowest: 0,
        highest: 0,
    };
    // SAFETY: the import, given a structure of its layout.
    check("D3D12ConfigureVirtualMemoryX", unsafe {
        D3D12ConfigureVirtualMemoryX(&mut range)
    })?;
    if range.lowest >= range.highest {
        return Err(format!(
            "D3D12ConfigureVirtualMemoryX gave no address range ({:#x} to {:#x})",
            range.lowest, range.highest
        )
        .into());
    }
    let imports = Imports {
        create_device,
        serialize_root,
    };
    let device = Title::create_device(&imports)?;
    let title = Title::on(device, imports, || {})?;
    title.pace(INTERVAL_US)?;
    // Loading: everything the frames use is made before the first.
    let mut buffers = BufferCopy::new(&title, 1 << 20)?;
    let mut texture = TiledTexture::new(&title)?;
    let mut compute = Compute::new(&title)?;
    let drawing = Drawing::new(&title)?;
    let mut picture = ReadBack::new(&title)?;
    // The buffer the workers fill (an upload heap), and the one the GPU
    // copies it into (a default heap), which the title reads back.
    let parts = threads::WAYS.len() * threads::PART;
    let filled = title.buffer(UPLOAD_HEAP, parts as u64, 0)?;
    let copied = title.buffer(DEFAULT_HEAP, parts as u64, 0)?;
    let mut fill_memory = Title::mapped(filled, parts)?;
    fill_memory.bytes_mut().fill(0);
    let mut copy_memory = Title::mapped(copied, parts)?;
    copy_memory.bytes_mut().fill(0xee);

    // The threads, one before the late DLL is loaded; then the late DLL.
    let stuck: &[threads::Way] = if alive {
        &[threads::Way::StuckWin32, threads::Way::StuckStd]
    } else {
        &[]
    };
    let mut workers = threads::Workers::begin(statics, stuck.len() as u32)?;
    let (late, late_module) = dlls::Dll::load(dlls::LATE)?;
    let late_counts = dlls::attached_before_use(&late, main_thread)?;
    // A second reference, for the worker that gives it back with
    // `FreeLibraryAndExitThread`: the same module, not attached again.
    let late_name = win::wide(dlls::LATE);
    // SAFETY: a NUL-terminated name.
    let reference = unsafe { win::LoadLibraryW(late_name.as_ptr()) };
    if reference != late_module || late.counts().process_attaches != 1 {
        return Err(format!("a second LoadLibraryW of {} did not give the loaded module back as it was", dlls::LATE).into());
    }
    // From here on, the end of the process is checked (`tls::at_exit`).
    statics.report_to(tls::STATIC_AT_EXIT.place());
    late.report_to(tls::LATE_AT_EXIT.place());
    tls::CHECK_EXIT.store(true, std::sync::atomic::Ordering::SeqCst);
    let seen = dlls::names((&statics, &static_counts), (&late, &late_counts, late_module))?;
    let twin = dlls::twin((&late, late_module))?;
    workers.start_rest(late, reference, fill_memory.address() as usize, stuck)?;
    let mut lines = vec![
        format!(
            "its DLLs {} (imported) and {} (LoadLibraryW, GetProcAddress) were each attached once, on its main thread, \
             before its first use; GetModuleHandle finds each by name and by its code, and GetModuleFileName names each \
             beside the executable, as {seen}",
            dlls::STATIC,
            dlls::LATE
        ),
        format!("{}, a copy of {} under another name, is {twin}", dlls::TWIN, dlls::LATE),
    ];

    // The workers are let go at the second frame, and their buffer goes
    // to the GPU at the third (both at the last of a shorter run).
    let go_at = 1u32.min(frames - 1);
    let hand_at = 2u32.min(frames - 1);
    for f in 0..frames {
        let token = title.next_frame(1_000)?;
        if f == go_at {
            workers.go()?;
        }
        let hand = f == hand_at;
        if hand {
            workers.filled()?;
        }
        picture.poison(0x5c);
        // What the title writes into its memory this frame.
        if f > 0 {
            buffers.change(f);
            texture.rewrite_block([(f * 7 % 256) as u8, 128, (255 - f * 3 % 256) as u8, 255])?;
        }
        compute.write_piece(f);
        let constants = [
            0x1234_5678 ^ f.wrapping_mul(0x9e37_79b9),
            7 + f % 1000,
            0,
            0,
        ];
        let draws = [
            DrawCall {
                vs: [step(f * 5 % 256), step(200), step(90), step(17)],
                ps: step(230),
                scissor: TOP_RIGHT,
            },
            DrawCall {
                vs: [step(250), step(f * 3 % 256), step(128), step(0)],
                ps: step(255),
                scissor: TOP_LEFT,
            },
        ];
        let clear = [0.1, (f % 60) as f32 / 60.0, 0.4, 1.0];
        title.frame(|t| {
            buffers.record(t);
            compute.record_dispatch(t, constants);
            compute.record_copy_in(t);
            t.transition(t.target, COMMON, COPY_DEST);
            texture.record_copy(t);
            t.transition(t.target, COPY_DEST, RENDER_TARGET);
            t.clear_rects(clear, &[BOTTOM_LEFT]);
            drawing.record(t, &draws);
            if hand {
                t.copy_buffer(copied, 0, filled, 0, parts as u64);
            }
            // The picture, into the title's own memory.
            t.transition(t.target, RENDER_TARGET, COPY_SOURCE);
            picture.record(t);
            t.transition(t.target, COPY_SOURCE, COMMON);
            Ok(())
        })?;
        title.present_with(token)?;
        title.sync()?;
        // What came back into the title's own memory.
        let frame = f + 1;
        if let Some(at) = buffers.differs() {
            return Err(format!(
                "frame {frame}: the buffer copy differs from what the title wrote at byte {at}"
            )
            .into());
        }
        compute.check_stores(constants, &format!("frame {frame}: the dispatch"))?;
        compute
            .check_copy_in()
            .map_err(|Failed(e)| format!("frame {frame}: {e}"))?;
        let want = drawn(texture.picture(), &draws, clear);
        let got = picture.picture();
        if let Some((x, y)) = first_far(&got, &want) {
            let at = (y * SIDE + x) as usize * 4;
            return Err(format!(
                "frame {frame}: its picture, read back into its memory, is {:?} at pixel ({x}, {y}), where it drew {:?}",
                &got[at..at + 4],
                &want[at..at + 4]
            )
            .into());
        }
        if hand {
            let wrong = copy_memory
                .bytes()
                .iter()
                .enumerate()
                .position(|(i, &b)| b != threads::pattern(i / threads::PART, i % threads::PART));
            if let Some(at) = wrong {
                return Err(format!(
                    "frame {frame}: the buffer its threads filled came back from the GPU wrong at byte {at}"
                )
                .into());
            }
        }
    }
    lines.push(
        "its picture, copied into a buffer it maps (CopyTextureRegion), matched what it drew in every frame".into(),
    );
    if alive {
        workers.stuck()?;
    }
    lines.push(workers.join()?);
    dlls::still_held(&late)?;
    if alive {
        // Two threads wait for ever, the late DLL stays loaded, and a frame
        // is submitted and presented but not waited for: the end must
        // still be the title's.
        let token = title.next_frame(1_000)?;
        title.frame(|t| {
            t.transition(t.target, COMMON, RENDER_TARGET);
            t.clear_target([0.5, 0.25, 0.75, 1.0]);
            t.transition(t.target, RENDER_TARGET, COMMON);
            Ok(())
        })?;
        title.present_with(token)?;
        lines.push(format!(
            "ends with {} threads waiting, {} loaded and a frame in flight",
            stuck.len(),
            dlls::LATE
        ));
    } else if dlls::free(&late, late_module, &tls::LATE_AT_EXIT)? {
        // Gone: loaded again it must start afresh (its statics, its
        // process attach), and go again at its last `FreeLibrary`.
        tls::LATE_AT_EXIT.clear();
        let (again, module) = dlls::Dll::load(dlls::LATE)?;
        dlls::attached_before_use(&again, main_thread)
            .map_err(|e| format!("loaded again after it was unloaded: {e}"))?;
        again.report_to(tls::LATE_AT_EXIT.place());
        if !dlls::free(&again, module, &tls::LATE_AT_EXIT)? {
            return Err(format!("{} loaded again was not unloaded by its last FreeLibrary", dlls::LATE).into());
        }
        lines.push(format!(
            "{}: its last FreeLibrary ran its process detach and unloaded it (a thread's FreeLibraryAndExitThread \
             before had left it loaded); loaded again, it was attached afresh, and unloaded again",
            dlls::LATE
        ));
    } else {
        lines.push(format!(
            "{}: FreeLibrary left it loaded; its process detach comes at the process's end",
            dlls::LATE
        ));
    }
    Ok(lines)
}

/// The picture it drew in a frame, as rows of [`SIDE`] RGBA8 pixels: the
/// two draws in the top quarters (`draws[1]` the left, `draws[0]` the
/// right), the clear in the bottom left and the texture's picture in the
/// bottom right.
fn drawn(texture: &[u8], draws: &[DrawCall; 2], clear: [f32; 4]) -> Vec<u8> {
    let clear = clear.map(|c| (c * 255.0).round() as u8);
    let half = SIDE / 2;
    (0..SIDE * SIDE)
        .flat_map(|i| {
            let (x, y) = (i % SIDE, i / SIDE);
            let at = i as usize * 4;
            match (x < half, y < half) {
                (true, true) => draws[1].colour(),
                (false, true) => draws[0].colour(),
                (true, false) => clear,
                (false, false) => [texture[at], texture[at + 1], texture[at + 2], texture[at + 3]],
            }
        })
        .collect()
}
