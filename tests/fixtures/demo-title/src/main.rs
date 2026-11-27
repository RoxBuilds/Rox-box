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

use std::ffi::{CStr, c_char, c_void};
use std::io::Write;
use std::process::ExitCode;

use victory_standin::pc::{
    COMMON, COPY_DEST, HRESULT, Iid, Obj, RENDER_TARGET, Rect, VirtualMemory,
};
use victory_standin::scene::{BufferCopy, Compute, DrawCall, Drawing, TiledTexture, step};
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
    let (frames, end) = match asked(&crt[1..]) {
        Ok(asked) => asked,
        Err(problem) => {
            println!(
                "demo title: {problem} (it takes --frames <n>, 1 or more, and one of --return, --exit \
                 or --exit-process <code>)"
            );
            return ExitCode::from(2);
        }
    };
    match run(frames) {
        Ok(()) => {
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

fn asked(args: &[String]) -> Result<(u32, End), String> {
    let mut args = args.iter();
    let mut frames = FRAMES;
    let mut end = None;
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
            other => return Err(format!("unknown argument {other:?}")),
        };
        if let Some(ends) = ends {
            if end.is_some() {
                return Err("only one of --return, --exit and --exit-process".to_owned());
            }
            end = Some(ends);
        }
    }
    Ok((frames, end.unwrap_or(End::Return(0))))
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

fn run(frames: u32) -> Result<(), Failed> {
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
    for f in 0..frames {
        let token = title.next_frame(1_000)?;
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
            t.transition(t.target, RENDER_TARGET, COMMON);
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
    }
    Ok(())
}
