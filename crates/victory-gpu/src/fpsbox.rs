//! The frame rate, drawn into the top left corner of every frame shown.
//!
//! The window's caption has the frame rate, but a capture of the window's
//! picture (a stream) does not carry the caption. Here the number is part
//! of the picture: a small box of text copied onto the swap chain's back
//! buffer right after the title's frame, before it is presented. The
//! title's own frame is not touched, so saved frames and everything read
//! back from it are as before.
//!
//! The text is drawn on the CPU with a font of its own (5 by 7 dots a
//! glyph) into a buffer the card copies from; a buffer is made once for a
//! text and never written again, so a present still in the air reads what
//! it was recorded with. On by default; `VICTORY_FPS_COUNTER=0` leaves the
//! frame as the title drew it.
//!
//! The panel (on by default; F10 shows or hides it, `VICTORY_FPS_PANEL=0`
//! keeps the box above instead) sits in the top right corner: the frame
//! rate, the 1% low (the rate of the slowest hundredth of the last 1,000
//! frames), the last second's time a frame and draws a frame, and a graph
//! of the last 120 frames' times (green within the 30 FPS budget, yellow to
//! 100 ms, red past it; the cyan line is 33.3 ms). It changes every frame,
//! so it is drawn into one of a ring of buffers, the oldest of which a
//! present has long read.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// The frame rate in tenths of a frame a second, as last measured (once a
/// second, by the presenter's meter); `u32::MAX` before the first.
static TENTHS: AtomicU32 = AtomicU32::new(u32::MAX);

/// Whether the counter is drawn on what is shown (saved frames are then
/// taken from there, so they carry it).
pub(crate) fn drawn() -> bool {
    on() && TENTHS.load(Ordering::Relaxed) != u32::MAX
}

/// Whether the counter is drawn.
fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_FPS_COUNTER").is_none_or(|v| v != "0"))
}

/// The frame rate just measured.
pub(crate) fn set(fps: f64) {
    if fps.is_finite() && fps >= 0.0 {
        TENTHS.store((fps * 10.0).round().min(99_999.0) as u32, Ordering::Relaxed);
    }
}

/// Draws a frame over the last second, for the panel.
static DRAWS: AtomicU32 = AtomicU32::new(0);
/// Milliseconds a frame over the last second, in tenths.
static MS_TENTHS: AtomicU32 = AtomicU32::new(0);

/// The last second's draws a frame and milliseconds a frame.
pub(crate) fn set_frame(draws: f64, ms: f64) {
    if draws.is_finite() && ms.is_finite() {
        DRAWS.store(draws.max(0.0).min(9_999_999.0) as u32, Ordering::Relaxed);
        MS_TENTHS.store((ms * 10.0).max(0.0).min(9_999_999.0) as u32, Ordering::Relaxed);
    }
}

/// Of a frame over the last second: ms preparing on the CPU, ms waiting
/// for the GPU, draws dropped for a shader without a host pipeline; and
/// the pipelines being made now.
static DETAIL: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];

/// The last second's detail (see [`DETAIL`]).
pub(crate) fn set_detail(cpu_ms: f64, gpu_ms: f64, dropped: f64, making: f64) {
    for (slot, v) in DETAIL.iter().zip([cpu_ms, gpu_ms, dropped, making]) {
        if v.is_finite() {
            slot.store(v.round().clamp(0.0, 9_999_999.0) as u32, Ordering::Relaxed);
        }
    }
}

/// What the runtime's other parts tell the panel ([`note`]), summed until
/// the next second's lines take them (g6-vt2: the branch owner asked for the panel to
/// show texture loading and what the frames spend time on, in game).
#[derive(Clone, Copy)]
pub enum Note {
    /// Bytes DirectStorage put in title memory.
    Streamed = 0,
    /// DirectStorage requests done.
    StreamRequests = 1,
    /// Pages of the GPU's page-mapping copies (`CopyPageMappings`).
    PageCopies = 2,
    /// Microseconds spent making them.
    PageCopyUs = 3,
    /// Frames a host upscaler (DLSS or FSR) recorded ([`upscaled`]).
    Upscales = 4,
    /// Draws and dispatches dropped because ray tracing is left out (the
    /// structure builder off, hit shading or ray activation without it).
    DropRays = 5,
    /// Dropped because their pipeline is skipped on purpose (a list).
    DropSkipped = 6,
    /// Dropped while their pipeline is being made, or for any other reason.
    DropOther = 7,
    /// Pages of title memory by address shaders touched with no copy.
    Faults = 8,
    /// Of those, pages that stayed without one (the reads gave 0).
    FaultsLeft = 9,
    /// List uploads and memory by address placed outside the claimed slots.
    UploadOutside = 10,
    /// Of those, placements that found no room at all.
    UploadNoRoom = 11,
}

static NOTES: [std::sync::atomic::AtomicU64; 12] = [const { std::sync::atomic::AtomicU64::new(0) }; 12];

/// Adds `n` to what the panel shows for `what`.
pub fn note(what: Note, n: u64) {
    NOTES[what as usize].fetch_add(n, Ordering::Relaxed);
}

/// The sums since the last take, from now on 0.
pub(crate) fn take_notes() -> [u64; 12] {
    NOTES.each_ref().map(|n| n.swap(0, Ordering::Relaxed))
}

/// The host upscaler chosen at start (`DLSS QUALITY`, `FSR 3.1.6`), empty
/// while the title's own runs; and the one that recorded last, with its
/// render and output sizes (g6-vt2: the branch owner asked for the panel to show
/// that DLSS is on).
static UPSCALER: Mutex<(String, &str, [u32; 4])> = Mutex::new((String::new(), "", [0; 4]));

/// Names the host upscaler that serves the title.
pub fn set_upscaler(name: String) {
    UPSCALER.lock().unwrap_or_else(|e| e.into_inner()).0 = name;
}

/// `by` (`DLSS`, `FSR`) recorded a frame from `render` to `output` pixels.
pub fn upscaled(by: &'static str, render: [u32; 2], output: [u32; 2]) {
    let mut u = UPSCALER.lock().unwrap_or_else(|e| e.into_inner());
    u.1 = by;
    u.2 = [render[0], render[1], output[0], output[1]];
    drop(u);
    note(Note::Upscales, 1);
}

/// The panel's upscaler lines for `per_second` recorded frames: what was
/// chosen and whether it ran, then the sizes it ran at.
pub(crate) fn upscaler_lines(per_second: f64) -> Vec<String> {
    let u = UPSCALER.lock().unwrap_or_else(|e| e.into_inner());
    if u.0.is_empty() && u.1.is_empty() {
        return vec!["UPSCALER TITLE OWN".to_owned()];
    }
    let chosen = if u.0.is_empty() { "HOST" } else { u.0.as_str() };
    let state = match u.1 {
        "" => "ON NOT RUN YET".to_owned(),
        _ if per_second < 0.5 => "ON IDLE".to_owned(),
        by if !chosen.starts_with(by) => format!("FELL BACK {by} {per_second:.0}/S"),
        _ => format!("ON {per_second:.0}/S"),
    };
    let mut lines = vec![format!("{chosen} {state}")];
    if !u.1.is_empty() {
        let [rw, rh, ow, oh] = u.2;
        lines.push(format!("{} {rw}X{rh} TO {ow}X{oh}", u.1));
    }
    lines
}

/// The second's lines beyond the first ones ([`set_more`]).
static MORE: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The last second's extra lines: texture uploads, copies, card memory,
/// streaming, page mappings.
pub(crate) fn set_more(lines: Vec<String>) {
    *MORE.lock().unwrap_or_else(|e| e.into_inner()) = lines;
}

/// The slowest frame of the last second (or of the last frames, if fewer).
fn worst_of_second(ms: &[f32]) -> f32 {
    let mut total = 0.0;
    let mut worst = 0.0f32;
    for &m in ms.iter().rev() {
        if total >= 1000.0 {
            break;
        }
        total += m;
        worst = worst.max(m);
    }
    worst
}

/// The time of each of the last frames, in milliseconds, newest last.
static TIMES: Mutex<(Option<std::time::Instant>, VecDeque<f32>)> = Mutex::new((None, VecDeque::new()));
/// Frames the 1% low is taken over.
const KEPT: usize = 1000;
/// Frames the graph shows.
const GRAPH: usize = 120;

/// A frame was presented.
pub(crate) fn presented() {
    let now = std::time::Instant::now();
    let mut times = TIMES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(last) = times.0.replace(now) {
        times.1.push_back(now.duration_since(last).as_secs_f32() * 1000.0);
        if times.1.len() > KEPT {
            times.1.pop_front();
        }
    }
}

/// The 1% low: the frame rate of the mean of the slowest hundredth of
/// `ms` (at least one frame), or none without frames.
fn one_percent_low(ms: &[f32]) -> Option<f32> {
    if ms.is_empty() {
        return None;
    }
    let mut sorted = ms.to_vec();
    sorted.sort_by(|a, b| b.total_cmp(a));
    let n = sorted.len().div_ceil(100);
    let mean = sorted[..n].iter().sum::<f32>() / n as f32;
    (mean > 0.0).then(|| 1000.0 / mean)
}

/// Whether the panel is shown: F10 switches it (`VICTORY_FPS_PANEL=0`:
/// hidden at the start).
fn panel_shown() -> bool {
    static SHOWN: std::sync::OnceLock<AtomicBool> = std::sync::OnceLock::new();
    static DOWN: AtomicBool = AtomicBool::new(false);
    let shown = SHOWN.get_or_init(|| AtomicBool::new(std::env::var_os("VICTORY_FPS_PANEL").is_none_or(|v| v != "0")));
    // SAFETY: a plain query of the keyboard's state.
    let down = unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(0x79) } as u16 & 0x8000 != 0;
    if down && !DOWN.swap(true, Ordering::Relaxed) {
        shown.fetch_xor(true, Ordering::Relaxed);
    } else if !down {
        DOWN.store(false, Ordering::Relaxed);
    }
    shown.load(Ordering::Relaxed)
}

/// A glyph's seven rows, five dots each (bit 4 is the left one).
fn glyph(c: char) -> [u8; 7] {
    match c {
        '0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        '2' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
        '3' => [0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        '6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
        '.' => [0, 0, 0, 0, 0, 0b01100, 0b01100],
        'F' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000],
        'P' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
        'S' => [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
        'L' => [0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111],
        'O' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'W' => [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001],
        'R' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],
        'A' => [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'M' => [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001],
        'E' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
        'D' => [0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110],
        '%' => [0b11001, 0b11010, 0b00010, 0b00100, 0b01000, 0b01011, 0b10011],
        'C' => [0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110],
        'U' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'G' => [0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111],
        'H' => [0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'N' => [0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001],
        'I' => [0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        'B' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110],
        'J' => [0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100],
        'K' => [0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001],
        'Q' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101],
        'T' => [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
        'V' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
        'X' => [0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001],
        'Y' => [0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100],
        'Z' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111],
        '-' => [0, 0, 0, 0b11111, 0, 0, 0],
        ':' => [0, 0b01100, 0b01100, 0, 0b01100, 0b01100, 0],
        '/' => [0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000],
        _ => [0; 7],
    }
}

/// The text for a frame rate in tenths.
fn text(tenths: u32) -> String {
    format!("{}.{} FPS", tenths / 10, tenths % 10)
}

/// Bytes a pixel of a swap chain format this layer presents in, or none.
fn pixel_bytes(format: DXGI_FORMAT) -> Option<usize> {
    match format {
        DXGI_FORMAT_R8G8B8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_R10G10B10A2_UNORM => Some(4),
        DXGI_FORMAT_R16G16B16A16_FLOAT => Some(8),
        _ => None,
    }
}

/// What a pixel of the box or panel shows.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Ink {
    Box,
    Text,
    /// A frame within the 30 FPS budget.
    Good,
    /// A frame up to 100 ms.
    Warn,
    Bad,
    /// The 33.3 ms line.
    Line,
}

/// One pixel of `format` in `ink`.
fn pixel(format: DXGI_FORMAT, ink: Ink, out: &mut [u8]) {
    // The text: a bright yellow. The box: nearly black.
    let (r, g, b): (f32, f32, f32) = match ink {
        Ink::Box => (0.03, 0.03, 0.03),
        Ink::Text => (1.0, 0.92, 0.16),
        Ink::Good => (0.2, 0.85, 0.3),
        Ink::Warn => (0.95, 0.75, 0.1),
        Ink::Bad => (0.95, 0.2, 0.15),
        Ink::Line => (0.2, 0.8, 0.95),
    };
    let byte = |v: f32| (v * 255.0).round() as u8;
    match format {
        DXGI_FORMAT_R8G8B8A8_UNORM => out.copy_from_slice(&[byte(r), byte(g), byte(b), 255]),
        DXGI_FORMAT_B8G8R8A8_UNORM => out.copy_from_slice(&[byte(b), byte(g), byte(r), 255]),
        DXGI_FORMAT_R10G10B10A2_UNORM => {
            let ten = |v: f32| (v * 1023.0).round() as u32;
            out.copy_from_slice(&(ten(r) | ten(g) << 10 | ten(b) << 20 | 3 << 30).to_le_bytes());
        }
        _ => {
            for (i, v) in [r, g, b, 1.0].into_iter().enumerate() {
                out[i * 2..i * 2 + 2].copy_from_slice(&half(v).to_le_bytes());
            }
        }
    }
}

/// A value from 0 to 1 as a 16-bit float.
fn half(v: f32) -> u16 {
    let bits = v.clamp(0.0, 1.0).to_bits();
    if v <= 0.0 {
        return 0;
    }
    // Sign 0; the exponent moves from a bias of 127 to one of 15; the top
    // ten bits of the fraction stay. Values under 2^-14 (none of ours) are 0.
    let exponent = ((bits >> 23) & 0xFF) as i32 - 127 + 15;
    if exponent <= 0 {
        return 0;
    }
    ((exponent as u32) << 10 | (bits >> 13) & 0x3FF) as u16
}

/// The box for `text` at `scale` dots a pixel: its width and height in
/// pixels and, row by row from the top, whether each pixel is text.
fn bitmap(text: &str, scale: u32) -> (u32, u32, Vec<bool>) {
    let chars: Vec<char> = text.chars().collect();
    let pad = 2 * scale;
    let width = 2 * pad + chars.len() as u32 * 6 * scale - scale;
    let height = 2 * pad + 7 * scale;
    let mut lit = vec![false; (width * height) as usize];
    for (i, &c) in chars.iter().enumerate() {
        let rows = glyph(c);
        for (row, bits) in rows.into_iter().enumerate() {
            for column in 0..5u32 {
                if bits >> (4 - column) & 1 == 0 {
                    continue;
                }
                let (x0, y0) = (pad + (i as u32 * 6 + column) * scale, pad + row as u32 * scale);
                for y in y0..y0 + scale {
                    for x in x0..x0 + scale {
                        lit[(y * width + x) as usize] = true;
                    }
                }
            }
        }
    }
    (width, height, lit)
}

/// The panel at `scale` dots a pixel: `lines` of text, then a graph of
/// `ms` (frame times, oldest first) at most [`GRAPH`] bars wide. Its width
/// and height and, row by row, each pixel's ink.
fn panel(lines: &[String], ms: &[f32], scale: u32) -> (u32, u32, Vec<Ink>) {
    let pad = 2 * scale;
    let columns = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0).max(12) as u32;
    let width = 2 * pad + columns * 6 * scale - scale;
    let line_height = 9 * scale;
    let graph_height = 24 * scale;
    let height = 2 * pad + lines.len() as u32 * line_height + graph_height;
    let mut ink = vec![Ink::Box; (width * height) as usize];
    for (n, line) in lines.iter().enumerate() {
        let (w, h, lit) = bitmap(line, scale);
        let top = n as u32 * line_height;
        for y in 0..h.min(height - top) {
            for x in 0..w.min(width) {
                if lit[(y * w + x) as usize] {
                    ink[((top + y) * width + x) as usize] = Ink::Text;
                }
            }
        }
    }
    // The graph: bars from the bottom, a pixel's height per `top / graph`
    // ms, the scale's top the slowest frame shown or 100 ms.
    let graph_top = pad + lines.len() as u32 * line_height;
    let inner = width - 2 * pad;
    let shown = &ms[ms.len().saturating_sub(GRAPH)..];
    let top_ms = shown.iter().copied().fold(100.0f32, f32::max);
    let bar = (inner / GRAPH as u32).max(1);
    let to_rows = |v: f32| ((v / top_ms) * graph_height as f32).round().clamp(1.0, graph_height as f32) as u32;
    let budget = graph_height - to_rows(1000.0 / 30.0);
    for (i, &v) in shown.iter().enumerate() {
        let x0 = pad + inner - (shown.len() - i) as u32 * bar;
        let colour = if v <= 1000.0 / 30.0 + 0.5 { Ink::Good } else if v <= 100.0 { Ink::Warn } else { Ink::Bad };
        let rows = to_rows(v);
        for y in graph_height - rows..graph_height {
            for x in x0..x0 + bar {
                ink[((graph_top + y) * width + x) as usize] = colour;
            }
        }
    }
    for x in pad..pad + inner {
        ink[((graph_top + budget) * width + x) as usize] = Ink::Line;
    }
    (width, height, ink)
}

/// The panel's lines.
fn panel_lines(ms: &[f32]) -> Vec<String> {
    let tenths = TENTHS.load(Ordering::Relaxed);
    let fps = if tenths == u32::MAX { "-".to_owned() } else { format!("{}.{}", tenths / 10, tenths % 10) };
    let low = one_percent_low(ms).map_or_else(|| "-".to_owned(), |v| format!("{v:.1}"));
    // The same over the last ten seconds only: the 1,000 frames above can
    // still hold a loading screen from a minute ago.
    let mut total = 0.0;
    let recent = ms.iter().rev().take_while(|&&m| {
        total += m;
        total <= 10_000.0
    }).count();
    let low_10s = one_percent_low(&ms[ms.len() - recent..]).map_or_else(|| "-".to_owned(), |v| format!("{v:.1}"));
    let frame = MS_TENTHS.load(Ordering::Relaxed);
    let [cpu, gpu, dropped, making] = DETAIL.each_ref().map(|v| v.load(Ordering::Relaxed));
    let mut lines = vec![
        format!("FPS {fps}"),
        format!("1% LOW {low} 10S {low_10s}"),
        format!("FRAME {} MS", frame / 10),
        format!("WORST {:.0} MS", worst_of_second(ms)),
        format!("CPU {cpu} GPU {gpu}"),
        format!("DRAWS {}", DRAWS.load(Ordering::Relaxed)),
        format!("SHADERS BUILDING {making}"),
        format!("NO SHADER {dropped}"),
    ];
    lines.extend(MORE.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned());
    lines.push(build_line().to_owned());
    lines.push(switch_line().to_owned());
    lines
}

/// The build: the folder the executable was built into (`target-vt3` shows
/// as `VT3`) and four hex digits of a hash of the executable's bytes, so a
/// screenshot names the exact binary.
fn build_line() -> &'static str {
    static LINE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    LINE.get_or_init(|| {
        let exe = std::env::current_exe().unwrap_or_default();
        let folder = exe
            .parent()
            .and_then(|d| d.parent())
            .and_then(|d| d.file_name())
            .map(|n| n.to_string_lossy().to_uppercase())
            .unwrap_or_default();
        let folder = folder.strip_prefix("TARGET-").unwrap_or(&folder).to_owned();
        // FNV-1a over the file: enough to tell builds apart.
        let hash = std::fs::read(&exe)
            .map(|b| b.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &x| (h ^ u64::from(x)).wrapping_mul(0x100_0000_01b3)))
            .unwrap_or(0);
        format!("BUILD {folder} {:04X}", hash & 0xffff)
    })
}

/// The switches that change the picture most: native 32-lane waves, lazy
/// page mapping, buffers first, precompiled pipelines (1 on, 0 off).
fn switch_line() -> &'static str {
    static LINE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    LINE.get_or_init(|| {
        let on = |k: &str, default: bool| match std::env::var(k).ok().as_deref().map(str::trim) {
            Some("0") => 0,
            Some(_) => 1,
            None => u8::from(default),
        };
        format!(
            "N{} L{} B{} P{}",
            on("VICTORY_WAVE_NATIVE", true),
            on("VICTORY_LAZY_ALIAS", true),
            on("VICTORY_CARD_BUFFERS_FIRST", true),
            on("VICTORY_PRECOMPILED", false)
        )
    })
}

/// Buffers the panel is drawn into in turn.
const RING: usize = 8;
/// The next slot, and per slot: the buffer, its format and scale, and
/// where it is mapped.
static RING_BUFFERS: Mutex<(usize, Vec<Option<(Made, i32, u32, usize)>>)> = Mutex::new((0, Vec::new()));

/// Copies the panel onto `back` in its top right corner (see [`draw`]).
fn draw_panel(host: &crate::host::Host, list: &ID3D12GraphicsCommandList, back: &ID3D12Resource, (width, height, format): (u32, u32, DXGI_FORMAT), bytes: usize) {
    let scale = (height / 360).clamp(2, 12);
    let ms: Vec<f32> = TIMES.lock().unwrap_or_else(|e| e.into_inner()).1.iter().copied().collect();
    let (w, h, ink) = panel(&panel_lines(&ms), &ms, scale);
    let margin = 4 * scale;
    if w + 2 * margin > width || h + 2 * margin > height {
        return;
    }
    let pitch = (w as usize * bytes).next_multiple_of(D3D12_TEXTURE_DATA_PITCH_ALIGNMENT as usize);
    let mut ring = RING_BUFFERS.lock().unwrap_or_else(|e| e.into_inner());
    if ring.1.is_empty() {
        ring.1.resize_with(RING, || None);
    }
    let slot = ring.0;
    ring.0 = (slot + 1) % RING;
    // A buffer is made again when the panel's size or format changes.
    let fits = matches!(&ring.1[slot], Some((m, f, s, _)) if m.width == w && m.height == h && *f == format.0 && *s == scale);
    if !fits {
        let Some((buffer, data)) = host.shared_buffer((pitch * h as usize) as u64) else {
            return;
        };
        ring.1[slot] = Some((Made { buffer, width: w, height: h, pitch: pitch as u32 }, format.0, scale, data as usize));
    }
    let Some((m, _, _, data)) = &ring.1[slot] else {
        return;
    };
    // SAFETY: the buffer's own mapped bytes, `pitch * h` of them; the
    // present that last read them is `RING` presents back.
    let out = unsafe { std::slice::from_raw_parts_mut(*data as *mut u8, pitch * h as usize) };
    for y in 0..h as usize {
        for x in 0..w as usize {
            let at = y * pitch + x * bytes;
            pixel(format, ink[y * w as usize + x], &mut out[at..at + bytes]);
        }
    }
    copy_box(list, back, m, format, width - w - margin, margin);
}

/// Records a copy of `m` onto `back` at (`x`, `y`).
fn copy_box(list: &ID3D12GraphicsCommandList, back: &ID3D12Resource, m: &Made, format: DXGI_FORMAT, x: u32, y: u32) {
    // SAFETY: an open list; the back buffer is a copy's destination in it
    // and the box lies inside it; our buffer holds `pitch * height` bytes
    // and is kept for as long as the process runs.
    unsafe {
        let from = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&m.buffer),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: format, Width: m.width, Height: m.height, Depth: 1, RowPitch: m.pitch },
                },
            },
        };
        let to = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(back),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
        };
        list.CopyTextureRegion(&to, x, y, 0, &from, None);
    }
}

/// A buffer the card copies a text's box from, with its footprint.
struct Made {
    buffer: ID3D12Resource,
    width: u32,
    height: u32,
    pitch: u32,
}

// SAFETY: a free-threaded D3D12 object and plain numbers.
unsafe impl Send for Made {}

/// The boxes made so far, by text, format and scale.
static MADE: Mutex<Option<HashMap<(u32, i32, u32), Made>>> = Mutex::new(None);

/// Copies the frame rate's box onto `back`, a back buffer of `width` by
/// `height` pixels of `format` that `list` holds as a copy's destination.
/// Nothing if the counter is off, no rate was measured yet, the format is
/// not one of ours or the frame is too small for the box.
pub(crate) fn draw(host: &crate::host::Host, list: &ID3D12GraphicsCommandList, back: &ID3D12Resource, (width, height, format): (u32, u32, DXGI_FORMAT)) {
    if !on() {
        return;
    }
    let tenths = TENTHS.load(Ordering::Relaxed);
    let Some(bytes) = pixel_bytes(format).filter(|_| tenths != u32::MAX) else {
        return;
    };
    if panel_shown() {
        draw_panel(host, list, back, (width, height, format), bytes);
        return;
    }
    // About a fortieth of the frame's height a glyph: 6 dots a pixel at
    // 2160 lines, 3 at 1080.
    let scale = (height / 360).clamp(2, 12);
    let mut made = MADE.lock().unwrap_or_else(|e| e.into_inner());
    let made = made.get_or_insert_with(HashMap::new);
    if made.len() > 2048 {
        // (Every text is a buffer of a few hundred KiB; a rate that wanders
        // over thousands of values is not expected.)
        made.clear();
    }
    let key = (tenths, format.0, scale);
    if !made.contains_key(&key) {
        let (w, h, lit) = bitmap(&text(tenths), scale);
        let pitch = (w as usize * bytes).next_multiple_of(D3D12_TEXTURE_DATA_PITCH_ALIGNMENT as usize);
        let Some((buffer, data)) = host.shared_buffer((pitch * h as usize) as u64) else {
            return;
        };
        // SAFETY: the buffer's own mapped bytes, `pitch * h` of them.
        let out = unsafe { std::slice::from_raw_parts_mut(data, pitch * h as usize) };
        for y in 0..h as usize {
            for x in 0..w as usize {
                let at = y * pitch + x * bytes;
                let ink = if lit[y * w as usize + x] { Ink::Text } else { Ink::Box };
                pixel(format, ink, &mut out[at..at + bytes]);
            }
        }
        made.insert(key, Made { buffer, width: w, height: h, pitch: pitch as u32 });
    }
    let Some(m) = made.get(&key) else {
        return;
    };
    let margin = 4 * scale;
    if m.width + 2 * margin > width || m.height + 2 * margin > height {
        return;
    }
    copy_box(list, back, m, format, margin, margin);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_text_is_the_rate_to_a_tenth() {
        assert_eq!(text(103), "10.3 FPS");
        assert_eq!(text(7), "0.7 FPS");
        assert_eq!(text(300), "30.0 FPS");
    }

    #[test]
    fn the_box_holds_every_glyph_with_a_margin() {
        let (w, h, lit) = bitmap("10.3 FPS", 3);
        // Eight glyphs of five dots and seven gaps of one, a pad of two dots all round.
        assert_eq!((w, h), ((8 * 5 + 7 + 4) * 3, (7 + 4) * 3));
        assert_eq!(lit.len(), (w * h) as usize);
        // The pad is box, not text; the "1" has its stem lit.
        assert!(lit[..(w * 6) as usize].iter().all(|&l| !l), "the top pad");
        assert!((0..h).all(|y| !lit[(y * w) as usize] && !lit[(y * w + w - 1) as usize]), "the side pads");
        let at = |x: u32, y: u32| lit[(y * w + x) as usize];
        // Glyph 0 ("1"): dot column 2 of row 0 is lit, column 0 is not.
        assert!(at(6 + 2 * 3, 6) && !at(6, 6));
        // A space lights nothing: glyph 4.
        assert!((0..7 * 3).all(|y| (0..5 * 3).all(|x| !at(6 + 4 * 18 + x, 6 + y))));
        // Every glyph used has at least one dot, but the space.
        for c in "0123456789.FPS".chars() {
            assert!(glyph(c).iter().any(|&r| r != 0), "{c}");
        }
    }

    #[test]
    fn pixels_are_in_the_formats_the_layer_presents_in() {
        let mut four = [0u8; 4];
        pixel(DXGI_FORMAT_R8G8B8A8_UNORM, Ink::Text, &mut four);
        assert_eq!(four, [255, 235, 41, 255]);
        pixel(DXGI_FORMAT_B8G8R8A8_UNORM, Ink::Text, &mut four);
        assert_eq!(four, [41, 235, 255, 255]);
        pixel(DXGI_FORMAT_R10G10B10A2_UNORM, Ink::Text, &mut four);
        let v = u32::from_le_bytes(four);
        assert_eq!((v & 1023, v >> 10 & 1023, v >> 20 & 1023, v >> 30), (1023, 941, 164, 3));
        let mut eight = [0u8; 8];
        pixel(DXGI_FORMAT_R16G16B16A16_FLOAT, Ink::Text, &mut eight);
        assert_eq!(u16::from_le_bytes([eight[0], eight[1]]), 0x3C00, "1.0 as a half");
        assert_eq!(u16::from_le_bytes([eight[6], eight[7]]), 0x3C00);
        assert_eq!(half(0.5), 0x3800);
        assert_eq!(half(0.0), 0);
        assert_eq!(pixel_bytes(DXGI_FORMAT_R16G16B16A16_FLOAT), Some(8));
        assert_eq!(pixel_bytes(DXGI_FORMAT_D32_FLOAT), None);
    }

    #[test]
    fn the_one_percent_low_is_the_slowest_hundredth() {
        assert_eq!(one_percent_low(&[]), None);
        // 200 frames: two of 100 ms, the rest 20 ms: the slowest two.
        let mut ms = vec![20.0f32; 198];
        ms.extend([100.0, 100.0]);
        assert_eq!(one_percent_low(&ms), Some(10.0));
        // Fewer than 100 frames: the slowest one.
        assert_eq!(one_percent_low(&[20.0, 50.0]), Some(20.0));
    }

    #[test]
    fn the_panel_holds_its_lines_and_graph() {
        let lines: Vec<String> = ["FPS 5.2", "1% LOW 2.1", "FRAME 190 MS", "DRAWS 3200"].map(String::from).to_vec();
        let ms: Vec<f32> = (0..300).map(|i| [20.0, 60.0, 250.0][i % 3]).collect();
        let (w, h, ink) = panel(&lines, &ms, 3);
        assert_eq!(ink.len(), (w * h) as usize);
        for kind in [Ink::Text, Ink::Good, Ink::Warn, Ink::Bad, Ink::Line] {
            assert!(ink.contains(&kind), "{kind:?}");
        }
        for c in "FPS1%LOWRAMEDS.0123456789CUGHNIB".chars() {
            assert!(glyph(c).iter().any(|&r| r != 0), "{c}");
        }
    }
}
