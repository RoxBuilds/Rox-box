//! The console's audio decoder (`xapu.dll`): Opus packets in, float PCM out.
//!
//! A title connects once for one kind of processing, gets a block of memory
//! for its packets and output, and then queues per-stream commands
//! (activate, process, reset, deactivate) and dequeues one result per
//! command. We decode each command when it is queued, so its result is ready
//! at once; the order of results is the order of commands.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::alloc::Layout;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::Mutex;

use opus_pure::OpusDecoder;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent};

use super::xasync::E_INVALIDARG;
use super::xgameruntime::{HRESULT, S_OK};
use crate::{HleModule, hle_export};

const E_NOTIMPL: HRESULT = 0x8000_4001_u32 as i32;
const E_OUTOFMEMORY: HRESULT = 0x8007_000E_u32 as i32;
const QUEUE_FULL: HRESULT = 0x8AC8_1000_u32 as i32;
/// No result yet, which titles poll for.
pub(crate) const PENDING_RESULTS: HRESULT = 0x8AC8_1001_u32 as i32;
const INVALID_COMMAND: HRESULT = 0x8AC8_1002_u32 as i32;
const INVALID_STREAM: HRESULT = 0x8AC8_1003_u32 as i32;
const ALREADY_ACTIVE: HRESULT = 0x8AC8_1005_u32 as i32;
const BAD_PACKET: HRESULT = 0x8AC8_1008_u32 as i32;

/// Processing kinds; only plain decoding is implemented.
const DECODE_OPUS: u32 = 1;

const ACTIVATE: u32 = 1;
const DEACTIVATE: u32 = 2;
const PROCESS: u32 = 3;
const NOTIFY: u32 = 4;
const RESET: u32 = 5;

/// Command option: signal the connection's event when the result is ready.
const SIGNAL: u32 = 1;
/// Connect options.
const DEINTERLEAVED_OUTPUT: u32 = 4;
const RESULT_EVENT: u32 = 8;

/// The longest Opus packet duration at 48 kHz (120 ms).
const MAX_FRAMES: usize = 5760;

#[repr(C)]
pub struct ConnectInput {
    process: u32,
    streams: u32,
    commands_per_stream: u32,
    memory: u32,
    frames_per_output: u32,
    options: u32,
}

#[repr(C)]
pub struct ConnectOutput {
    memory: *mut c_void,
    result_event: HANDLE,
}

/// What every command starts with, and what its result echoes.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct CommandId {
    kind: u32,
    stream: u32,
    sequence: u32,
    options: u32,
    context: u64,
}

#[repr(C, packed)]
struct Activate {
    id: CommandId,
    channels: u32,
    init_length: u32,
    init: *const c_void,
    start_pitch: f32,
    processing_length: u32,
    processing: *mut c_void,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Process {
    id: CommandId,
    pitch_ramp: f32,
    target_pitch: f32,
    first_frame: u32,
    frame_count: u32,
    input_length: u32,
    input: *const u8,
    output_length: u32,
    output: *mut u8,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct ApuResult {
    id: CommandId,
    state: u32,
    state_data: u64,
    usec: u64,
    error: HRESULT,
    frames: u32,
    output_length: u32,
    output: *mut u8,
    pitch: f32,
    input_length: u32,
    input: *const u8,
}

// SAFETY: the raw pointers are title addresses carried through, never
// dereferenced after the result is made.
unsafe impl Send for ApuResult {}

pub struct Connection {
    memory: *mut u8,
    layout: Layout,
    event: HANDLE,
    options: u32,
    per_stream: u32,
    state: Mutex<State>,
}

struct State {
    streams: Vec<Option<OpusDecoder>>,
    /// Results not yet dequeued, per stream.
    queued: Vec<u32>,
    results: VecDeque<ApuResult>,
    commands: u32,
    dequeued: u32,
    /// What the title asked of the decoder in the second that is running
    /// (`VICTORY_APU_LOG`).
    stats: Stats,
    /// Where PROCESS commands decode their packet, kept from one command to
    /// the next ([`kept_scratch`]). One for the whole connection: its
    /// commands run one at a time, under this state's lock.
    pcm: Vec<f32>,
}

/// `VICTORY_APU_SCRATCH=0` decodes every PROCESS command into a buffer of its
/// own, allocated, zeroed and freed each time, as before. By default the
/// connection keeps one, grown the first time a decoder needs more (46 KB for
/// stereo), avoiding per-command heap allocation contention across threads.
fn kept_scratch() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_APU_SCRATCH").is_none_or(|v| v != "0"))
}

/// `VICTORY_APU_LOG=0` leaves out the line said once a second of what the
/// title asked of the decoder (on by default: one line a second while the
/// title decodes).
fn log_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_APU_LOG").is_none_or(|v| v != "0"))
}

/// `VICTORY_APU_PITCH=echo` answers a PROCESS command that has no ramp with its `target_pitch` instead of 1.0 (diagnostic
/// mode to echo the command's target rate/pitch field). Default off: 1.0 always.
fn echo_pitch_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_APU_PITCH").is_some_and(|v| v == "echo"))
}

/// The `pitch` a PROCESS result carries: the command's target when echoing and the command has no ramp (so a
/// title that sends 1.0 sees no change), else 1.0.
fn result_pitch(echo: bool, ramp: f32, target: f32) -> f32 {
    if echo && ramp == 0.0 { target } else { 1.0 }
}

/// What one stream did in the second that is running: frames returned, when its last PROCESS command came (kept
/// across seconds) and the longest gap between two of them.
#[derive(Default, Debug, PartialEq, Clone, Copy)]
struct StreamSecond {
    commands: u32,
    frames: u64,
    last_ms: Option<u64>,
    max_gap_ms: u64,
}

/// What the PROCESS commands of one second asked for and got. The decoder
/// answers at once with `usec` 0, `state` 0 and `pitch` 1.0, and ignores
/// `pitch_ramp` and `target_pitch`; a title that takes the position of
/// speech (lip-sync) from those results would see none while audio plays.
#[derive(Default, Debug, PartialEq)]
struct Stats {
    process: u32,
    /// Bit `n % 64` of word `n / 64`: stream `n` was processed.
    streams: Vec<u64>,
    min_pitch: Option<f32>,
    max_pitch: Option<f32>,
    /// Commands whose `target_pitch` is not 1.0, and those with a ramp.
    pitch_not_one: u32,
    ramped: u32,
    /// Commands that start at a frame other than 0.
    offset: u32,
    asked_frames: u64,
    got_frames: u64,
    /// Commands that failed.
    errors: u32,
    /// Activates whose `start_pitch` is not 1.0.
    start_pitch_not_one: u32,
    /// Per stream, to tell a title that time-scales its audio (few frames a second from every stream) from
    /// starved threads (long gaps between the commands of one stream).
    per_stream: std::collections::BTreeMap<u32, StreamSecond>,
}

impl Stats {
    /// Counts a PROCESS command that came at `now` (milliseconds) and got `got` frames.
    fn process(&mut self, command: &Process, got: u32, error: HRESULT, now: u64) {
        let (stream, ramp, target) = (command.id.stream, command.pitch_ramp, command.target_pitch);
        let (first, asked) = (command.first_frame, command.frame_count);
        self.process += 1;
        let (word, bit) = (stream as usize / 64, stream % 64);
        if self.streams.len() <= word {
            self.streams.resize(word + 1, 0);
        }
        self.streams[word] |= 1 << bit;
        self.min_pitch = Some(self.min_pitch.map_or(target, |m| m.min(target)));
        self.max_pitch = Some(self.max_pitch.map_or(target, |m| m.max(target)));
        self.pitch_not_one += u32::from(target != 1.0);
        self.ramped += u32::from(ramp != 0.0);
        self.offset += u32::from(first != 0);
        self.asked_frames += u64::from(asked);
        self.got_frames += u64::from(got);
        self.errors += u32::from(error != S_OK);
        let one = self.per_stream.entry(stream).or_default();
        one.commands += 1;
        one.frames += u64::from(got);
        if let Some(last) = one.last_ms {
            one.max_gap_ms = one.max_gap_ms.max(now.saturating_sub(last));
        }
        one.last_ms = Some(now);
    }

    /// The fewest and the most frames any stream returned, and the longest gap (ms) between two commands of one
    /// stream.
    fn stream_extremes(&self) -> (u64, u64, u64) {
        // Only the streams that had a command in the second count (a stream gone quiet is not a slow one).
        let frames = self.per_stream.values().filter(|s| s.commands > 0).map(|s| s.frames);
        let min = frames.clone().min().unwrap_or(0);
        let max = frames.max().unwrap_or(0);
        let gap = self.per_stream.values().map(|s| s.max_gap_ms).max().unwrap_or(0);
        (min, max, gap)
    }

    fn distinct_streams(&self) -> u32 {
        self.streams.iter().map(|w| w.count_ones()).sum()
    }

    /// The line for this second, if anything was processed; the counts start
    /// again.
    fn line(&mut self) -> Option<String> {
        if self.process == 0 {
            return None;
        }
        let (min_frames, max_frames, max_gap_ms) = self.stream_extremes();
        let line = format!(
            "apu: {} PROCESS commands, {} streams, target_pitch {:.3}..{:.3} ({} not 1.0, {} ramped), \
             frames returned {} of {} asked, {} from a first frame past 0, {} errors, {} activates with \
             start_pitch not 1.0, output frames a second per stream {}..{}, longest gap between two commands of \
             one stream {} ms (usec 0, state 0, pitch 1.0 returned unless VICTORY_APU_PITCH=echo)",
            self.process,
            self.distinct_streams(),
            self.min_pitch.unwrap_or(1.0),
            self.max_pitch.unwrap_or(1.0),
            self.pitch_not_one,
            self.ramped,
            self.got_frames,
            self.asked_frames,
            self.offset,
            self.errors,
            self.start_pitch_not_one,
            min_frames,
            max_frames,
            max_gap_ms,
        );
        // The time of each stream's last command carries over, so a gap across the second is seen.
        let carried = std::mem::take(&mut self.per_stream)
            .into_iter()
            .map(|(stream, s)| (stream, StreamSecond { last_ms: s.last_ms, ..Default::default() }))
            .collect();
        *self = Stats { per_stream: carried, ..Stats::default() };
        Some(line)
    }
}

/// Milliseconds since the first call, for the once-a-second line.
fn now_ms() -> u64 {
    static BEGAN: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    BEGAN.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

/// Whether a second has passed since `*said` (then it is set to `now`).
fn second_passed(said: &mut Option<u64>, now: u64) -> bool {
    match *said {
        Some(at) if now.saturating_sub(at) < 1000 => false,
        _ => {
            *said = Some(now);
            true
        }
    }
}

fn lock(c: &Connection) -> std::sync::MutexGuard<'_, State> {
    c.state.lock().unwrap_or_else(|e| e.into_inner())
}

pub extern "system" fn XApuConnect(
    input: *const ConnectInput,
    output: *mut ConnectOutput,
    handle: *mut *mut Connection,
) -> HRESULT {
    if input.is_null() || output.is_null() || handle.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null.
    let input = unsafe { &*input };
    if !(1..=500).contains(&input.streams)
        || !(1..=16).contains(&input.commands_per_stream)
        || input.memory == 0
    {
        return E_INVALIDARG;
    }
    if input.process != DECODE_OPUS {
        // shortcut: rate conversion (decode-convert, PCM convert) is not
        // implemented; add a resampler if a title connects for it.
        tracing::warn!(
            process = input.process,
            "audio decoder: processing kind not implemented"
        );
        return E_NOTIMPL;
    }
    if input.frames_per_output != 0 {
        return E_INVALIDARG;
    }
    let Ok(layout) = Layout::from_size_align(input.memory as usize, 16) else {
        return E_INVALIDARG;
    };
    // SAFETY: non-zero size.
    let memory = unsafe { std::alloc::alloc_zeroed(layout) };
    if memory.is_null() {
        return E_OUTOFMEMORY;
    }
    let event = if input.options & RESULT_EVENT != 0 {
        // SAFETY: plain auto-reset event, no name.
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        if event.is_null() {
            // SAFETY: allocated above with `layout`.
            unsafe { std::alloc::dealloc(memory, layout) };
            return E_OUTOFMEMORY;
        }
        event
    } else {
        std::ptr::null_mut()
    };
    let streams = input.streams as usize;
    tracing::info!(
        streams,
        memory = input.memory,
        options = input.options,
        "audio decoder connected"
    );
    let connection = Box::new(Connection {
        memory,
        layout,
        event,
        options: input.options,
        per_stream: input.commands_per_stream,
        state: Mutex::new(State {
            streams: (0..streams).map(|_| None).collect(),
            queued: vec![0; streams],
            results: VecDeque::new(),
            commands: 0,
            dequeued: 0,
            stats: Stats::default(),
            pcm: Vec::new(),
        }),
    });
    // SAFETY: checked non-null.
    unsafe {
        output.write(ConnectOutput {
            memory: memory.cast(),
            result_event: event,
        });
        handle.write(Box::into_raw(connection));
    }
    S_OK
}

pub extern "system" fn XApuDisconnect(handle: *mut Connection) -> HRESULT {
    if handle.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: made by `XApuConnect`; disconnected once.
    let connection = unsafe { Box::from_raw(handle) };
    if !connection.event.is_null() {
        // SAFETY: our event.
        unsafe { CloseHandle(connection.event) };
    }
    // SAFETY: allocated in `XApuConnect` with this layout.
    unsafe { std::alloc::dealloc(connection.memory, connection.layout) };
    S_OK
}

pub extern "system" fn XApuEnqueueCommand(
    handle: *mut Connection,
    command: *const CommandId,
    total: *mut u32,
) -> HRESULT {
    if handle.is_null() || command.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: a connection from `XApuConnect`; every command starts with an id.
    let (connection, id) = unsafe { (&*handle, command.read_unaligned()) };
    let mut state = lock(connection);
    let stream = id.stream as usize;
    if stream >= state.streams.len() {
        return INVALID_STREAM;
    }
    if state.queued[stream] >= connection.per_stream {
        return QUEUE_FULL;
    }
    let mut result = ApuResult {
        id,
        state: 0,
        state_data: 0,
        usec: 0,
        error: S_OK,
        frames: 0,
        output_length: 0,
        output: std::ptr::null_mut(),
        pitch: 1.0,
        input_length: 0,
        input: std::ptr::null(),
    };
    let State { streams, stats, pcm, .. } = &mut *state;
    let decoder = &mut streams[stream];
    match id.kind {
        ACTIVATE => {
            // SAFETY: an activate command is this struct.
            let activate = unsafe { command.cast::<Activate>().read_unaligned() };
            let channels = activate.channels;
            stats.start_pitch_not_one += u32::from(activate.start_pitch != 1.0);
            tracing::debug!(stream, channels, "Opus stream activated");
            result.error = if decoder.is_some() {
                ALREADY_ACTIVE
            } else if !(1..=2).contains(&channels) {
                E_INVALIDARG
            } else {
                match OpusDecoder::new(48_000, channels as usize) {
                    Ok(d) => {
                        *decoder = Some(d);
                        S_OK
                    }
                    Err(_) => E_INVALIDARG,
                }
            };
        }
        DEACTIVATE => *decoder = None,
        RESET => {
            if let Some(d) = decoder {
                let _ = d.reset_state();
            }
        }
        NOTIFY => {}
        PROCESS => {
            // SAFETY: a process command is this struct.
            let command = unsafe { command.cast::<Process>().read_unaligned() };
            let kept = kept_scratch().then_some(pcm);
            match decoder {
                Some(d) => process(d, connection.options, &command, &mut result, kept),
                None => result.error = INVALID_STREAM,
            }
            result.pitch = result_pitch(echo_pitch_on(), command.pitch_ramp, command.target_pitch);
            stats.process(&command, result.frames, result.error, now_ms());
        }
        _ => return INVALID_COMMAND,
    }
    if log_on() {
        static SAID: Mutex<Option<u64>> = Mutex::new(None);
        let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
        if second_passed(&mut said, now_ms())
            && let Some(line) = state.stats.line()
        {
            tracing::info!("{line}");
        }
    }
    state.queued[stream] += 1;
    state.commands = state.commands.wrapping_add(1);
    state.results.push_back(result);
    if !total.is_null() {
        // SAFETY: optional out parameter.
        unsafe { total.write(state.commands) };
    }
    if id.options & SIGNAL != 0 && !connection.event.is_null() {
        // SAFETY: our event.
        unsafe { SetEvent(connection.event) };
    }
    S_OK
}

/// Decodes one packet into the command's output buffer, keeping the frames
/// from `first_frame` on, at most `frame_count` of them. The packet is decoded
/// into `kept`, the connection's buffer ([`kept_scratch`]); without it into
/// one allocated for this command, as before.
fn process(
    decoder: &mut OpusDecoder,
    options: u32,
    command: &Process,
    result: &mut ApuResult,
    kept: Option<&mut Vec<f32>>,
) {
    let channels = decoder.channels();
    let (input, input_length) = (command.input, command.input_length);
    let (output, output_length) = (command.output, command.output_length);
    result.input = input;
    result.input_length = input_length;
    result.output = output;
    if input.is_null() || output.is_null() || input_length == 0 {
        result.error = E_INVALIDARG;
        return;
    }
    // SAFETY: the title's packet in its connection memory.
    let packet = unsafe { std::slice::from_raw_parts(input, input_length as usize) };
    let mut own;
    let pcm = match kept {
        Some(kept) => scratch(kept, packet, channels),
        None => {
            own = vec![0f32; MAX_FRAMES * channels];
            &mut own[..]
        }
    };
    let decoded = match decoder.decode(packet, MAX_FRAMES, pcm) {
        Ok(n) => n,
        Err(_) => {
            result.error = BAD_PACKET;
            return;
        }
    };
    let first = (command.first_frame as usize).min(decoded);
    let frames = (decoded - first)
        .min(command.frame_count as usize)
        .min(output_length as usize / (4 * channels));
    let kept = &pcm[first * channels..(first + frames) * channels];
    let bytes = frames * channels * 4;
    if bytes > 0 {
        // SAFETY: `output` is non-null and holds at least `bytes <= output_length` bytes;
        // writing raw bytes avoids requiring 4-byte alignment on the title's `*mut u8`.
        let out = unsafe { std::slice::from_raw_parts_mut(output, bytes) };
        if options & DEINTERLEAVED_OUTPUT != 0 && channels == 2 {
            let (left, right) = out.split_at_mut(frames * 4);
            let (left, _) = left.as_chunks_mut::<4>();
            let (right, _) = right.as_chunks_mut::<4>();
            for (i, frame) in kept.as_chunks::<2>().0.iter().enumerate() {
                left[i] = frame[0].to_le_bytes();
                right[i] = frame[1].to_le_bytes();
            }
        } else {
            for (dst, &sample) in out.as_chunks_mut::<4>().0.iter_mut().zip(kept) {
                *dst = sample.to_le_bytes();
            }
        }
    }
    result.frames = frames as u32;
    result.output_length = bytes as u32;
}

/// The kept buffer ready for a decode of `packet` with `channels`: grown to
/// `MAX_FRAMES` frames the first time (the only allocation), and zero where
/// the decoder works, as a fresh buffer is. The decoder writes and reads back
/// only the frames the packet holds, which its first bytes say (a packet
/// whose length cannot be read that way gets all of it zeroed); what lies
/// past them, left from a longer packet, is never read.
fn scratch<'a>(kept: &'a mut Vec<f32>, packet: &[u8], channels: usize) -> &'a mut [f32] {
    let need = MAX_FRAMES * channels;
    if kept.len() < need {
        kept.resize(need, 0.0);
    }
    let frames = opus_pure::packet::samples_48k(packet).map_or(MAX_FRAMES, |n| n.min(MAX_FRAMES));
    let pcm = &mut kept[..need];
    pcm[..frames * channels].fill(0.0);
    pcm
}

pub extern "system" fn XApuDequeueResult(handle: *mut Connection, out: *mut ApuResult) -> HRESULT {
    if handle.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: a connection from `XApuConnect`.
    let mut state = lock(unsafe { &*handle });
    let Some(result) = state.results.pop_front() else {
        return PENDING_RESULTS;
    };
    let stream = result.id.stream as usize;
    state.queued[stream] -= 1;
    state.dequeued = state.dequeued.wrapping_add(1);
    // SAFETY: the title passes room for one result.
    unsafe { out.write_unaligned(result) };
    S_OK
}

pub extern "system" fn XApuGetResultCount(handle: *mut Connection, total: *mut u32) -> u32 {
    if handle.is_null() {
        return 0;
    }
    // SAFETY: a connection from `XApuConnect`.
    let state = lock(unsafe { &*handle });
    if !total.is_null() {
        // SAFETY: optional out parameter.
        unsafe { total.write(state.dequeued) };
    }
    state.results.len() as u32
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "apu",
        dlls: &["xapu.dll"],
        exports: vec![
            hle_export!(XApuConnect),
            hle_export!(XApuDisconnect),
            hle_export!(XApuEnqueueCommand),
            hle_export!(XApuDequeueResult),
            hle_export!(XApuGetResultCount),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opus_pure::{Application, OpusEncoder};

    fn id(kind: u32, stream: u32) -> CommandId {
        CommandId {
            kind,
            stream,
            sequence: 7,
            options: 0,
            context: 0x1234,
        }
    }

    #[test]
    fn decodes_opus_as_the_title_drives_it() {
        assert_eq!(size_of::<ApuResult>(), 0x50);
        // 256 mono/stereo streams, two commands each, as the title connects.
        let input = ConnectInput {
            process: DECODE_OPUS,
            streams: 256,
            commands_per_stream: 2,
            memory: 0x10_4000,
            frames_per_output: 0,
            options: 1,
        };
        let mut output = ConnectOutput {
            memory: std::ptr::null_mut(),
            result_event: std::ptr::null_mut(),
        };
        let mut handle = std::ptr::null_mut();
        assert_eq!(XApuConnect(&input, &mut output, &mut handle), S_OK);
        let base = output.memory.cast::<u8>();

        let activate = Activate {
            id: id(ACTIVATE, 3),
            channels: 2,
            init_length: 0,
            init: std::ptr::null(),
            start_pitch: 1.0,
            processing_length: 0,
            processing: std::ptr::null_mut(),
        };
        assert_eq!(
            XApuEnqueueCommand(handle, (&raw const activate).cast(), std::ptr::null_mut()),
            S_OK
        );

        // A 20 ms stereo packet of a 440 Hz tone.
        let tone: Vec<f32> = (0..960 * 2)
            .map(|i| ((i / 2) as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5)
            .collect();
        let mut encoder = OpusEncoder::new(48_000, 2, Application::Audio).unwrap();
        let mut packet = [0u8; 1500];
        let n = encoder.encode(&tone, 960, &mut packet).unwrap();
        // SAFETY: inside the connection memory.
        unsafe { std::ptr::copy_nonoverlapping(packet.as_ptr(), base, n) };
        let process = Process {
            id: id(PROCESS, 3),
            pitch_ramp: 0.0,
            target_pitch: 1.0,
            first_frame: 0,
            frame_count: 960,
            input_length: n as u32,
            input: base,
            output_length: 0xf00 * 2,
            output: base.wrapping_add(0x14000),
        };
        let mut total = 0;
        assert_eq!(
            XApuEnqueueCommand(handle, (&raw const process).cast(), &mut total),
            S_OK
        );
        assert_eq!(total, 2);
        // Two results queued for stream 3: its queue is full.
        assert_eq!(
            XApuEnqueueCommand(handle, (&raw const process).cast(), &mut total),
            QUEUE_FULL
        );
        assert_eq!(XApuGetResultCount(handle, std::ptr::null_mut()), 2);

        let mut result = std::mem::MaybeUninit::<ApuResult>::zeroed();
        assert_eq!(XApuDequeueResult(handle, result.as_mut_ptr()), S_OK);
        // SAFETY: written above.
        let first = unsafe { result.assume_init() };
        let (kind, error, context) = (first.id.kind, first.error, first.id.context);
        assert_eq!((kind, error, context), (ACTIVATE, S_OK, 0x1234));
        assert_eq!(XApuDequeueResult(handle, result.as_mut_ptr()), S_OK);
        // SAFETY: written above.
        let decoded = unsafe { result.assume_init() };
        let (kind, error, frames, length) = (
            decoded.id.kind,
            decoded.error,
            decoded.frames,
            decoded.output_length,
        );
        assert_eq!((kind, error, frames, length), (PROCESS, S_OK, 960, 960 * 8));
        // SAFETY: the decoder wrote 960 stereo frames there.
        let pcm =
            unsafe { std::slice::from_raw_parts(base.wrapping_add(0x14000).cast::<f32>(), 1920) };
        let energy: f32 = pcm.iter().map(|s| s * s).sum::<f32>() / 1920.0;
        assert!(energy > 0.01, "decoded a tone, energy {energy}");
        assert_eq!(
            XApuDequeueResult(handle, result.as_mut_ptr()),
            PENDING_RESULTS
        );
        let mut dequeued = 0;
        assert_eq!(XApuGetResultCount(handle, &mut dequeued), 0);
        assert_eq!(dequeued, 2);

        // A process on a stream that was never activated fails in its result.
        let mut stray = process;
        stray.id.stream = 4;
        assert_eq!(
            XApuEnqueueCommand(handle, (&raw const stray).cast(), &mut total),
            S_OK
        );
        assert_eq!(XApuDequeueResult(handle, result.as_mut_ptr()), S_OK);
        // SAFETY: written above.
        let error = unsafe { result.assume_init() }.error;
        assert_eq!(error, INVALID_STREAM);
        assert_eq!(XApuDisconnect(handle), S_OK);
    }

    #[test]
    fn decodes_into_unaligned_output_interleaved_and_deinterleaved() {
        let tone: Vec<f32> = (0..960 * 2)
            .map(|i| {
                let phase = (i / 2) as f32 * std::f32::consts::TAU / 48_000.0;
                if i % 2 == 0 {
                    (phase * 440.0).sin() * 0.5
                } else {
                    (phase * 880.0).sin() * 0.25
                }
            })
            .collect();
        let mut encoder = OpusEncoder::new(48_000, 2, Application::Audio).unwrap();
        let mut packet = [0u8; 1500];
        let n = encoder.encode(&tone, 960, &mut packet).unwrap();

        for (options, deinterleaved) in [(0u32, false), (DEINTERLEAVED_OUTPUT, true)] {
            let input = ConnectInput {
                process: DECODE_OPUS,
                streams: 4,
                commands_per_stream: 4,
                memory: 0x4_0000,
                frames_per_output: 0,
                options,
            };
            let mut output = ConnectOutput {
                memory: std::ptr::null_mut(),
                result_event: std::ptr::null_mut(),
            };
            let mut handle = std::ptr::null_mut();
            assert_eq!(XApuConnect(&input, &mut output, &mut handle), S_OK);
            let base = output.memory.cast::<u8>();

            // Invalid channel counts fail cleanly without activating the stream.
            for bad_channels in [0u32, 3, u32::MAX] {
                let bad = Activate {
                    id: id(ACTIVATE, 0),
                    channels: bad_channels,
                    init_length: 0,
                    init: std::ptr::null(),
                    start_pitch: 1.0,
                    processing_length: 0,
                    processing: std::ptr::null_mut(),
                };
                assert_eq!(
                    XApuEnqueueCommand(handle, (&raw const bad).cast(), std::ptr::null_mut()),
                    S_OK
                );
                let mut res = std::mem::MaybeUninit::<ApuResult>::zeroed();
                assert_eq!(XApuDequeueResult(handle, res.as_mut_ptr()), S_OK);
                // SAFETY: written by `XApuDequeueResult`.
                let err = unsafe { res.assume_init() }.error;
                assert_eq!(err, E_INVALIDARG, "channels={bad_channels}");
            }

            // Activate two identical stereo decoders so aligned and unaligned outputs match bit-for-bit.
            for stream in [0u32, 1] {
                let activate = Activate {
                    id: id(ACTIVATE, stream),
                    channels: 2,
                    init_length: 0,
                    init: std::ptr::null(),
                    start_pitch: 1.0,
                    processing_length: 0,
                    processing: std::ptr::null_mut(),
                };
                assert_eq!(
                    XApuEnqueueCommand(handle, (&raw const activate).cast(), std::ptr::null_mut()),
                    S_OK
                );
                let mut res = std::mem::MaybeUninit::<ApuResult>::zeroed();
                assert_eq!(XApuDequeueResult(handle, res.as_mut_ptr()), S_OK);
                // SAFETY: written by `XApuDequeueResult`.
                let err = unsafe { res.assume_init() }.error;
                assert_eq!(err, S_OK);
            }

            // SAFETY: inside the connection memory.
            unsafe { std::ptr::copy_nonoverlapping(packet.as_ptr(), base, n) };
            let aligned_out = base.wrapping_add(0x10000);
            let unaligned_out = base.wrapping_add(0x20001); // 1 mod 4: misaligned for f32

            for (stream, out_ptr, frame_count) in [
                (0u32, aligned_out, 960u32),
                (1u32, unaligned_out, 960u32),
                (1u32, base.wrapping_add(0x30003), 0u32), // zero frames at misaligned ptr
            ] {
                let proc_cmd = Process {
                    id: id(PROCESS, stream),
                    pitch_ramp: 0.0,
                    target_pitch: 1.0,
                    first_frame: 0,
                    frame_count,
                    input_length: n as u32,
                    input: base,
                    output_length: 960 * 8,
                    output: out_ptr,
                };
                assert_eq!(
                    XApuEnqueueCommand(handle, (&raw const proc_cmd).cast(), std::ptr::null_mut()),
                    S_OK
                );
                let mut res = std::mem::MaybeUninit::<ApuResult>::zeroed();
                assert_eq!(XApuDequeueResult(handle, res.as_mut_ptr()), S_OK);
                // SAFETY: written by `XApuDequeueResult`.
                let decoded = unsafe { res.assume_init() };
                let (err, frames, len) = (decoded.error, decoded.frames, decoded.output_length);
                assert_eq!((err, frames, len), (S_OK, frame_count, frame_count * 8));
            }

            // SAFETY: both output regions are inside connection memory and hold 960 * 8 bytes.
            let (aligned_bytes, unaligned_bytes) = unsafe {
                (
                    std::slice::from_raw_parts(aligned_out, 960 * 8),
                    std::slice::from_raw_parts(unaligned_out, 960 * 8),
                )
            };
            assert_eq!(
                unaligned_bytes, aligned_bytes,
                "unaligned output must match aligned output (deinterleaved={deinterleaved})"
            );
            let samples: Vec<f32> = unaligned_bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| f32::from_le_bytes(*c))
                .collect();
            let energy: f32 = samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32;
            assert!(
                energy > 0.01,
                "expected decoded tone (deinterleaved={deinterleaved}), energy={energy}"
            );
            assert_eq!(XApuDisconnect(handle), S_OK);
        }
    }

    /// Packets of a tone with some noise in it, from one encoder whose rate and
    /// frame length change between them: speech (SILK), hybrid and music
    /// (CELT) packets and the switches between those, of 10 to 60 ms.
    fn packets(channels: usize) -> Vec<Vec<u8>> {
        let mut encoder = OpusEncoder::new(48_000, channels, Application::Voip).unwrap();
        let steps = [
            (12_000, 960),
            (12_000, 1920),
            (24_000, 960),
            (32_000, 480),
            (96_000, 960),
            (96_000, 480),
            (16_000, 2880),
            (64_000, 960),
            (10_000, 960),
            (48_000, 1920),
            (128_000, 960),
            (20_000, 960),
        ];
        let (mut noise, mut at) = (0x1234_5678u32, 0usize);
        let mut all = Vec::new();
        for (bitrate, frames) in steps.into_iter().cycle().take(36) {
            encoder.bitrate_bps = bitrate;
            let pcm: Vec<f32> = (0..frames * channels)
                .map(|i| {
                    let t = (at + i / channels) as f32 / 48_000.0;
                    noise = noise.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let tone = (t * 440.0 * std::f32::consts::TAU).sin() * 0.4;
                    tone + ((noise >> 8) as f32 / (1 << 24) as f32 - 0.5) * 0.05
                })
                .collect();
            at += frames;
            let mut packet = vec![0u8; opus_pure::MAX_PACKET_BYTES];
            let n = encoder.encode(&pcm, frames, &mut packet).unwrap();
            packet.truncate(n);
            all.push(packet);
        }
        all
    }

    /// The kept buffer is zeroed over the frames the packet holds and left as it
    /// was past them; all of it for a packet whose length does not read.
    #[test]
    fn the_kept_buffer_is_zeroed_where_the_packet_decodes() {
        let packet = &packets(2)[0];
        let frames = opus_pure::packet::samples_48k(packet).unwrap();
        assert_eq!(frames, 960);
        let mut kept = vec![7.5f32; 100];
        let pcm = scratch(&mut kept, packet, 2);
        assert_eq!(pcm.len(), MAX_FRAMES * 2);
        assert!(pcm[..frames * 2].iter().all(|&s| s == 0.0));
        assert!(pcm[100..frames * 2].iter().all(|&s| s == 0.0), "grown with zeros");
        pcm.fill(7.5);
        let pcm = scratch(&mut kept, packet, 2);
        assert!(pcm[..frames * 2].iter().all(|&s| s == 0.0));
        assert!(pcm[frames * 2..].iter().all(|&s| s == 7.5), "past the packet: as it was");
        // A code-3 packet that says it has no frames.
        let pcm = scratch(&mut kept, &[0x03, 0x00], 1);
        assert_eq!(pcm.len(), MAX_FRAMES);
        assert!(pcm.iter().all(|&s| s == 0.0));
        assert_eq!(kept.len(), MAX_FRAMES * 2, "not made again for fewer channels");
    }

    /// A result as the enqueue starts it.
    fn empty_result() -> ApuResult {
        // SAFETY: integers, raw pointers and a float: all zero is a value of each.
        unsafe { std::mem::MaybeUninit::<ApuResult>::zeroed().assume_init() }
    }

    /// Every packet decodes into the kept buffer to the same PCM and result as
    /// into a buffer of its own (the old way), whatever the kept one held
    /// before; it is made once, and no decode after the first calls the heap
    /// for it.
    #[test]
    fn a_kept_buffer_decodes_as_a_fresh_one_and_is_made_once() {
        let mut modes = Vec::new();
        let mut heard = 0;
        for (channels, options) in [(1, 0), (2, 0), (2, DEINTERLEAVED_OUTPUT)] {
            let mut old = OpusDecoder::new(48_000, channels).unwrap();
            let mut new = OpusDecoder::new(48_000, channels).unwrap();
            let mut kept = Vec::new();
            let mut made = None;
            for (n, packet) in packets(channels).iter().enumerate() {
                let mode = opus_pure::packet::mode(packet).unwrap();
                if !modes.contains(&mode) {
                    modes.push(mode);
                }
                // Some commands start past the first frame, or want fewer.
                let (first, wanted) = [(0, 5760), (100, 5760), (0, 300), (37, 960)][n % 4];
                let mut outputs = [vec![0u8; MAX_FRAMES * 8], vec![0u8; MAX_FRAMES * 8]];
                let command = |output: &mut Vec<u8>| Process {
                    id: id(PROCESS, 0),
                    pitch_ramp: 0.0,
                    target_pitch: 1.0,
                    first_frame: first,
                    frame_count: wanted,
                    input_length: packet.len() as u32,
                    input: packet.as_ptr(),
                    output_length: output.len() as u32,
                    output: output.as_mut_ptr(),
                };
                let [a, b] = &mut outputs;
                let (one, two) = (command(a), command(b));
                let mut results = [empty_result(), empty_result()];
                // What a longer packet left there, or anything: it must not matter.
                kept.fill(7.5);
                let ((), calls_old) = counting::count(|| process(&mut old, options, &one, &mut results[0], None));
                let ((), calls_new) =
                    counting::count(|| process(&mut new, options, &two, &mut results[1], Some(&mut kept)));
                let [a, b] = &results;
                let (fa, la, ea) = (a.frames, a.output_length, a.error);
                let (fb, lb, eb) = (b.frames, b.output_length, b.error);
                let bytes = packet.len();
                let what = format!("packet {n} ({mode:?}, {bytes} bytes, {channels} channels, options {options})");
                assert_eq!((fa, la, ea), (fb, lb, eb), "{what}");
                assert_eq!(ea, S_OK, "{what}");
                assert!(outputs[0] == outputs[1], "{what}: the PCM differs");
                heard += usize::from(outputs[0].iter().any(|&b| b != 0));
                // The old way allocates and frees its buffer each time; the kept
                // one is allocated at the first decode and never again (the
                // decoder's own heap calls, if any, are the same for both).
                if let (Some(old), Some(new)) = (calls_old, calls_new) {
                    assert_eq!(old, new + if n == 0 { 1 } else { 2 }, "{what}: heap calls");
                    if n > 0 && old == 2 {
                        assert_eq!(new, 0, "{what}");
                    }
                }
                let here = (kept.as_ptr(), kept.capacity());
                assert_eq!(here.1, MAX_FRAMES * channels, "{what}");
                if let Some(before) = made {
                    assert_eq!(here, before, "{what}: the kept buffer was made again");
                }
                made = Some(here);
            }
        }
        for mode in [opus_pure::OpusMode::SilkOnly, opus_pure::OpusMode::Hybrid, opus_pure::OpusMode::CeltOnly] {
            assert!(modes.contains(&mode), "no {mode:?} packet among {modes:?}");
        }
        assert!(heard >= 90, "{heard} of 108 decodes gave sound");
    }
}

/// The heap calls of the thread that asks ([`count`]), for the test that a
/// kept buffer is made once. Diagnostics builds have victory-gpu's counting
/// allocator in front of the heap: there only the buffer's place and size are
/// looked at.
#[cfg(all(test, not(feature = "diagnostics")))]
mod counting {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        /// Heap calls of this thread since it began to count; `None`: it does not.
        static CALLS: Cell<Option<u64>> = const { Cell::new(None) };
    }

    struct Counting;

    // SAFETY: every call goes to the system's allocator unchanged; only a counter
    // of the calling thread is kept beside it.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            called();
            // SAFETY: the caller's contract, passed on.
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            called();
            // SAFETY: as above.
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn realloc(&self, at: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            called();
            // SAFETY: as above.
            unsafe { System.realloc(at, layout, size) }
        }

        unsafe fn dealloc(&self, at: *mut u8, layout: Layout) {
            called();
            // SAFETY: as above.
            unsafe { System.dealloc(at, layout) }
        }
    }

    #[global_allocator]
    static HEAP: Counting = Counting;

    fn called() {
        // (A thread being torn down has no counter left.)
        let _ = CALLS.try_with(|c| c.set(c.get().map(|n| n + 1)));
    }

    /// What `f` returns, and how many heap calls it made on this thread.
    pub(super) fn count<R>(f: impl FnOnce() -> R) -> (R, Option<u64>) {
        CALLS.set(Some(0));
        let r = f();
        (r, CALLS.replace(None))
    }
}

/// [`count`] where the heap calls are not counted (diagnostics builds).
#[cfg(all(test, feature = "diagnostics"))]
mod counting {
    pub(super) fn count<R>(f: impl FnOnce() -> R) -> (R, Option<u64>) {
        (f(), None)
    }
}

#[cfg(test)]
mod stats_tests {
    use super::*;

    fn command(stream: u32, ramp: f32, target: f32, first: u32, frames: u32) -> Process {
        Process {
            id: CommandId { kind: PROCESS, stream, sequence: 0, options: 0, context: 0 },
            pitch_ramp: ramp,
            target_pitch: target,
            first_frame: first,
            frame_count: frames,
            input_length: 0,
            input: std::ptr::null(),
            output_length: 0,
            output: std::ptr::null_mut(),
        }
    }

    #[test]
    fn stats_count_commands_streams_pitch_and_frames_and_start_again() {
        let mut s = Stats::default();
        assert!(s.line().is_none());
        s.process(&command(3, 0.0, 1.0, 0, 960), 960, S_OK, 100);
        s.process(&command(3, 0.5, 1.25, 0, 960), 480, S_OK, 140);
        s.process(&command(70, 0.0, 0.5, 10, 960), 0, BAD_PACKET, 150);
        assert_eq!(s.process, 3);
        assert_eq!(s.distinct_streams(), 2);
        assert_eq!((s.min_pitch, s.max_pitch), (Some(0.5), Some(1.25)));
        assert_eq!((s.pitch_not_one, s.ramped, s.offset, s.errors), (2, 1, 1, 1));
        assert_eq!((s.got_frames, s.asked_frames), (1440, 2880));
        assert_eq!(s.stream_extremes(), (0, 1440, 40));
        let line = s.line().unwrap();
        assert!(line.contains("output frames a second per stream 0..1440") && line.contains("gap between two commands of one stream 40 ms"), "{line}");
        assert!(line.contains("3 PROCESS commands, 2 streams") && line.contains("frames returned 1440 of 2880"), "{line}");
        assert_eq!(s.process, 0);
        assert!(s.line().is_none());
    }

    #[test]
    fn stream_gaps_carry_across_seconds_and_frames_start_again() {
        let mut s = Stats::default();
        s.process(&command(1, 0.0, 1.0, 0, 960), 960, S_OK, 10);
        s.process(&command(2, 0.0, 1.0, 0, 960), 480, S_OK, 20);
        s.process(&command(1, 0.0, 1.0, 0, 960), 960, S_OK, 30);
        assert_eq!(s.stream_extremes(), (480, 1920, 20));
        s.line().unwrap();
        assert_eq!(s.stream_extremes(), (0, 0, 0));
        // Stream 1's next command is 1,000 ms after its last one, in the next second.
        s.process(&command(1, 0.0, 1.0, 0, 960), 960, S_OK, 1030);
        assert_eq!(s.stream_extremes(), (960, 960, 1000));
    }

    #[test]
    fn the_pitch_echo_rule() {
        assert_eq!(result_pitch(false, 0.0, 48000.0), 1.0);
        assert_eq!(result_pitch(true, 0.0, 48000.0), 48000.0);
        assert_eq!(result_pitch(true, 0.0, 1.0), 1.0);
        assert_eq!(result_pitch(true, 0.25, 48000.0), 1.0);
    }

    #[test]
    fn a_line_is_said_once_a_second() {
        let mut said = None;
        assert!(second_passed(&mut said, 5));
        assert!(!second_passed(&mut said, 900));
        assert!(second_passed(&mut said, 1005));
    }
}
