//! rustgd: real-time resize reflow via R-side `later` polling.
//!
//! Architecture:
//!   - The viewer writes /tmp/rustgd-<pid>/resize.txt on every resize event
//!   - R polls that file every 5ms via the `later` package, scheduled from
//!     rustgd_poll_resize in R/rustgd.R
//!   - When the file appears, the R input handler reads the new dimensions,
//!     calls rustgd_set_size, and re-evaluates the captured plot expressions
//!     in replay mode so each plot's output overwrites its existing SVG file
//!
//! Expressions are captured by an R task callback. NEW_PAGE_FIRED and
//! DREW_SOMETHING are atomic flags set by drawing callbacks; the task
//! callback reads and resets them after each user-issued top-level
//! expression to decide whether a new plot was started or an existing one
//! was overlaid.
//!
//! Clipping: every primitive in the DrawBuffer is tagged with the clip rect
//! that was active at emission time. render_svg() emits a <clipPath> def
//! for each unique rect and wraps consecutive primitives sharing the same
//! clip in <g clip-path="url(#cpN)"> groups. extendr's default
//! CLIPPING_STRATEGY is DeviceAndEngine, so canClip is set to TRUE
//! automatically at device creation and R calls our clip() callback
//! throughout each plot.
//!
//! Rasters are PNG-encoded in memory, base64'd, and embedded as
//! <image href="data:image/png;base64,..."/> elements that participate in
//! the same clip-group machinery as every other primitive.

use extendr_api::{
    prelude::*,
    Error,
};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

// ----------------------------------------------------------------------
// Global state
// ----------------------------------------------------------------------

const DEFAULT_W: f64 = 800.0;
const DEFAULT_H: f64 = 600.0;

static CURRENT_SIZE: Mutex<(f64, f64)> = Mutex::new((DEFAULT_W, DEFAULT_H));
// Session directory, provided by R at activation so both sides agree on the
// path (R picks it with tempdir(), which is portable). None until recorded.
static SESSION_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
static NEW_PAGE_FIRED: AtomicBool = AtomicBool::new(false);
static DREW_SOMETHING: AtomicBool = AtomicBool::new(false);

/// Set to true by R-side before a resize-triggered replay, false after.
/// When true, new_page() does not advance the page counter — the replay
/// overwrites the existing page's file instead of creating a new entry.
static REPLAY_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

/// Global page counter. Reset to 0 by rustgd_activate; incremented by
/// new_page() when not in replay mode. R can also write to it directly
/// via rustgd_set_current_page() to direct replay output to a specific
/// page during multi-plot history replay.
static CURRENT_PAGE: AtomicUsize = AtomicUsize::new(0);

// ----------------------------------------------------------------------
// libc bindings
// ----------------------------------------------------------------------

#[cfg(unix)]
#[link(name = "c")]
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

#[cfg(unix)]
unsafe fn libc_kill_zero(pid: i32) -> bool {
    unsafe { kill(pid, 0) == 0 }
}

// ----------------------------------------------------------------------
// R graphics engine FFI
// ----------------------------------------------------------------------

// ----------------------------------------------------------------------
// FFI bridge to the C-level device wrapper
// ----------------------------------------------------------------------
//
// The C file src/rustgd_device.c implements R's graphics device callbacks
// against pDevDesc directly, then calls into the rustgd_cb_* functions
// defined below for each draw operation. This replaces extendr-api 0.9's
// DeviceDriver trampoline, which had been causing heap corruption when
// R's display list machinery interacted with our device.
//
// rustgd_register_device: called from rustgd_activate to allocate a
// pDevDesc, wire up our callbacks, and register with R's graphics engine.
// Returns 0 on success, nonzero on failure (in which case the caller
// retains ownership of the Rust device and must drop it).
//
// rustgd_set_device_size: called from rustgd_set_size to update the
// current device's stored extent on viewer resize. Done in C to avoid
// reproducing pDevDesc's layout in Rust.

extern "C" {
    fn rustgd_register_device(
        rust_dev: *mut c_void,
        width: f64,
        height: f64,
        name: *const c_char,
    ) -> i32;
    fn rustgd_set_device_size(width: f64, height: f64);
}

// ----------------------------------------------------------------------
// Session directory & orphan recovery
// ----------------------------------------------------------------------

fn session_dir() -> PathBuf {
    if let Some(dir) = SESSION_DIR.lock().unwrap().clone() {
        return dir;
    }
    // Fallback if activation has not recorded a directory yet: the platform
    // temp directory with the per-process name.
    let pid = std::process::id();
    std::env::temp_dir().join(format!("rustgd-{}", pid))
}

fn sweep_orphans() {
    let tmp = std::env::temp_dir();
    let entries = match fs::read_dir(&tmp) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if let Some(pid_str) = name.strip_prefix("rustgd-") {
            if let Ok(pid) = pid_str.parse::<i32>() {
                if !pid_is_alive(pid) {
                    let _ = fs::remove_dir_all(&path);
                }
            }
        }
    }
}

// Whether a process is still running. Unix uses kill(pid, 0); on other
// platforms we cannot cheaply check, so we conservatively report alive and
// leave the directory in place (the owning session's finalizer and the OS
// temp cleanup remove it).
#[cfg(unix)]
fn pid_is_alive(pid: i32) -> bool {
    unsafe { libc_kill_zero(pid) }
}

#[cfg(not(unix))]
fn pid_is_alive(_pid: i32) -> bool {
    true
}

// ----------------------------------------------------------------------
// SVG helpers
// ----------------------------------------------------------------------

fn r_color_to_svg(color: i32) -> String {
    if color == i32::MIN {
        return "none".to_string();
    }
    let r = (color & 0xFF) as u8;
    let g = ((color >> 8) & 0xFF) as u8;
    let b = ((color >> 16) & 0xFF) as u8;
    let a = ((color >> 24) & 0xFF) as u8;
    if a == 0 {
        return "none".to_string();
    }
    if a == 255 {
        format!("rgb({},{},{})", r, g, b)
    } else {
        format!("rgba({},{},{},{:.3})", r, g, b, a as f32 / 255.0)
    }
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

const FAM_SANS: usize = 0;
const FAM_SERIF: usize = 1;
const FAM_MONO: usize = 2;
const NUM_FAMILIES: usize = 3;
const NUM_FACES: usize = 4;

const FONT_FILES: [[&str; NUM_FACES]; NUM_FAMILIES] = [
    [
        "LiberationSans-Regular.ttf",
        "LiberationSans-Bold.ttf",
        "LiberationSans-Italic.ttf",
        "LiberationSans-BoldItalic.ttf",
    ],
    [
        "LiberationSerif-Regular.ttf",
        "LiberationSerif-Bold.ttf",
        "LiberationSerif-Italic.ttf",
        "LiberationSerif-BoldItalic.ttf",
    ],
    [
        "LiberationMono-Regular.ttf",
        "LiberationMono-Bold.ttf",
        "LiberationMono-Italic.ttf",
        "LiberationMono-BoldItalic.ttf",
    ],
];

const FAMILY_SVG_NAMES: [&str; NUM_FAMILIES] =
    ["Liberation Sans", "Liberation Serif", "Liberation Mono"];

fn family_index(family: &str) -> usize {
    let lower = family.to_lowercase();
    match lower.as_str() {
        "serif" | "times" | "times new roman" | "liberation serif" => FAM_SERIF,
        "mono" | "monospace" | "courier" | "courier new" | "liberation mono" => FAM_MONO,
        _ => FAM_SANS,
    }
}

fn face_index(fontface: i32) -> usize {
    match fontface {
        2 => 1,
        3 => 2,
        4 => 3,
        _ => 0,
    }
}

fn face_svg_attrs(face_idx: usize) -> (&'static str, &'static str) {
    match face_idx {
        1 => ("bold", "normal"),
        2 => ("normal", "italic"),
        3 => ("bold", "italic"),
        _ => ("normal", "normal"),
    }
}

// ----------------------------------------------------------------------
// Clip tracking
// ----------------------------------------------------------------------

/// One clip rectangle in R device coordinates (y-up: y0=bottom, y1=top).
#[derive(Clone, Copy)]
struct ClipRect {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

/// Hashable key for clip dedup. f64 isn't Hash, so we bit-cast to u64.
/// Two clips that are bit-exact equal share an id; R typically re-issues
/// the same clip with bit-exact values, so this catches the common case
/// cleanly without paying for fuzzy equality.
type ClipKey = (u64, u64, u64, u64);

fn clip_key(r: &ClipRect) -> ClipKey {
    (r.x0.to_bits(), r.y0.to_bits(), r.x1.to_bits(), r.y1.to_bits())
}

struct DrawBuffer {
    /// All SVG fragments concatenated back-to-back into one growing
    /// buffer. Drawing callbacks push_str into this; nothing per-element
    /// ever gets its own heap allocation that would sit in storage across
    /// a display-list-replay or recordPlot/replayPlot boundary, which is
    /// where the historic heap corruption was happening on macOS tiny zone.
    raw_content: String,
    /// Per-fragment metadata, parallel to byte ranges in raw_content.
    /// Each entry is (clip_id, start_byte, end_byte). clip_id of None
    /// means the primitive was drawn before any clip() callback fired
    /// this page and should render ungrouped.
    element_meta: Vec<(Option<usize>, usize, usize)>,
    /// Dedup: clip rect (by bits) -> id. Same rect issued twice gets one id.
    clip_defs: HashMap<ClipKey, usize>,
    /// Ordered list of unique clip rects, indexed by id.
    clip_rects: Vec<ClipRect>,
    /// The clip rect active right now; tagged onto each pushed primitive.
    current_clip_id: Option<usize>,
    has_content: bool,
    width: f64,
    height: f64,
}

impl DrawBuffer {
    fn new() -> Self {
        Self {
            raw_content: String::new(),
            element_meta: Vec::new(),
            clip_defs: HashMap::new(),
            clip_rects: Vec::new(),
            current_clip_id: None,
            has_content: false,
            width: DEFAULT_W,
            height: DEFAULT_H,
        }
    }

    fn clear(&mut self, width: f64, height: f64) {
        // raw_content.clear() resets len to 0 but keeps the allocated
        // capacity, so we never drop the underlying String buffer here.
        // element_meta.clear() drops integer tuples only. No per-fragment
        // String drops occur in this path, which is the entire point of
        // the refactor: external code can no longer free a fragment's
        // buffer behind our back and cause a double-free on clear.
        self.raw_content.clear();
        self.element_meta.clear();
        self.clip_defs.clear();
        self.clip_rects.clear();
        self.current_clip_id = None;
        self.has_content = false;
        self.width = width;
        self.height = height;
    }

    fn push(&mut self, element: String) {
        // Copy the fragment's bytes into raw_content, record the
        // (start, end) byte range, then let the temporary String go
        // out of scope so its heap buffer is freed by us immediately.
        // After this returns, the only heap holding fragment bytes
        // is raw_content's single growing buffer.
        let start = self.raw_content.len();
        self.raw_content.push_str(&element);
        let end = self.raw_content.len();
        self.element_meta.push((self.current_clip_id, start, end));
        self.has_content = true;
    }

    /// Called by the clip() callback. Looks up or assigns an id for this
    /// rect and updates current_clip_id. Subsequent push()es will be
    /// tagged with this id until set_clip is called again.
    fn set_clip(&mut self, rect: ClipRect) {
        let key = clip_key(&rect);
        let id = match self.clip_defs.get(&key) {
            Some(&id) => id,
            None => {
                let id = self.clip_rects.len();
                self.clip_rects.push(rect);
                self.clip_defs.insert(key, id);
                id
            }
        };
        self.current_clip_id = Some(id);
    }

    fn render_svg(&self) -> String {
        let w = self.width;
        let h = self.height;
        let mut out = String::new();
        out.push_str(&format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <svg xmlns=\"http://www.w3.org/2000/svg\" \
             viewBox=\"0 0 {w} {h}\" width=\"{w}\" height=\"{h}\">\n"
        ));

        // Emit <clipPath> defs first, only if any clips were issued.
        // R device coords are y-up (y0=bottom, y1=top numerically smaller);
        // SVG is y-down. We use min/max so the math is robust against either
        // ordering of the corners.
        if !self.clip_rects.is_empty() {
            out.push_str("<defs>\n");
            for (i, rect) in self.clip_rects.iter().enumerate() {
                let x_left = rect.x0.min(rect.x1);
                let x_right = rect.x0.max(rect.x1);
                let y_bot_r = rect.y0.min(rect.y1);
                let y_top_r = rect.y0.max(rect.y1);
                let svg_x = x_left;
                let svg_y = h - y_top_r;
                let svg_w = (x_right - x_left).max(0.0);
                let svg_h = (y_top_r - y_bot_r).max(0.0);
                out.push_str(&format!(
                    "<clipPath id=\"cp{}\"><rect x=\"{:.3}\" y=\"{:.3}\" \
                     width=\"{:.3}\" height=\"{:.3}\"/></clipPath>\n",
                    i, svg_x, svg_y, svg_w, svg_h
                ));
            }
            out.push_str("</defs>\n");
        }

        // White background, always full device extent, never clipped.
        // Tagged with a stable id so the viewer can strip it for
        // transparent export without disturbing any background the
        // plot itself draws.
        out.push_str(&format!(
            "<rect id=\"rustgd-bg\" width=\"{w}\" height=\"{h}\" fill=\"#ffffff\"/>\n"
        ));

        // Walk element_meta; open a <g clip-path="..."> whenever the
        // active clip id changes, close it before opening the next one.
        // None for clip_id means "no clip group" so the primitive renders
        // bare. Each element's bytes come from raw_content[start..end].
        let mut current_open: Option<usize> = None;
        for &(clip_id, start, end) in &self.element_meta {
            if clip_id != current_open {
                if current_open.is_some() {
                    out.push_str("</g>\n");
                }
                if let Some(id) = clip_id {
                    out.push_str(&format!("<g clip-path=\"url(#cp{})\">\n", id));
                }
                current_open = clip_id;
            }
            out.push_str(&self.raw_content[start..end]);
            out.push('\n');
        }
        if current_open.is_some() {
            out.push_str("</g>\n");
        }

        out.push_str("</svg>\n");
        out
    }
}

// ----------------------------------------------------------------------
// The device
// ----------------------------------------------------------------------

struct RustgdDevice {
    dir: PathBuf,
    _viewer_child: std::process::Child,
    buffer: DrawBuffer,
    font_bytes: [[Vec<u8>; NUM_FACES]; NUM_FAMILIES],
}

impl RustgdDevice {
    fn new() -> Result<Self, Error> {
        sweep_orphans();
        let dir = session_dir();
        fs::create_dir_all(&dir).map_err(|e| Error::Other(format!("create session dir: {e}")))?;

        let viewer = locate_viewer()?;
        let fonts_dir = locate_fonts_dir()?;

        let mut font_bytes: [[Vec<u8>; NUM_FACES]; NUM_FAMILIES] = Default::default();
        for (fam_idx, fam_files) in FONT_FILES.iter().enumerate() {
            for (face_idx, name) in fam_files.iter().enumerate() {
                let path = fonts_dir.join(name);
                if !path.exists() {
                    return Err(Error::Other(format!(
                        "missing font file: {}",
                        path.display()
                    )));
                }
                let bytes =
                    fs::read(&path).map_err(|e| Error::Other(format!("read {name}: {e}")))?;
                ttf_parser::Face::parse(&bytes, 0)
                    .map_err(|e| Error::Other(format!("parse {name}: {e:?}")))?;
                font_bytes[fam_idx][face_idx] = bytes;
            }
        }

        let child = Command::new(viewer)
            .arg(&dir)
            .arg(&fonts_dir)
            .spawn()
            .map_err(|e| Error::Other(format!("launch viewer: {e}")))?;

        Ok(Self {
            dir,
            _viewer_child: child,
            buffer: DrawBuffer::new(),
            font_bytes,
        })
    }

    fn flush(&mut self) {
        if !self.buffer.has_content {
            return;
        }
        let n = CURRENT_PAGE.load(Ordering::SeqCst);
        if n == 0 {
            // No page has been started yet (no new_page() has fired);
            // nothing meaningful to write.
            return;
        }
        let path = self.dir.join(format!("plot-{:04}.svg", n));
        let svg = self.buffer.render_svg();
        let _ = fs::write(path, svg);
        self.buffer.has_content = false;
    }

    fn flip_y(&self, y: f64) -> f64 {
        self.buffer.height - y
    }

    fn compute_text_width(
        &self,
        text: &str,
        font_size_px: f64,
        family_idx: usize,
        face_idx: usize,
    ) -> f64 {
        if text.is_empty() {
            return 0.0;
        }
        let bytes = &self.font_bytes[family_idx][face_idx];
        let face = match ttf_parser::Face::parse(bytes, 0) {
            Ok(f) => f,
            Err(_) => return text.chars().count() as f64 * font_size_px * 0.5,
        };
        let units_per_em = face.units_per_em() as f64;
        if units_per_em <= 0.0 {
            return text.chars().count() as f64 * font_size_px * 0.5;
        }
        let mut total: f64 = 0.0;
        for c in text.chars() {
            let glyph_id = face.glyph_index(c).unwrap_or(ttf_parser::GlyphId(0));
            let advance = face.glyph_hor_advance(glyph_id).unwrap_or(0) as f64;
            total += advance;
        }
        (total / units_per_em) * font_size_px
    }
}

fn locate_viewer() -> Result<PathBuf, Error> {
    let pkg_path: String = R!(r#"system.file(package = "rustgd")"#)?
        .as_str()
        .ok_or_else(|| Error::Other("system.file returned non-string".into()))?
        .to_string();
    let viewer_name = if cfg!(windows) {
        "rustgd-viewer.exe"
    } else {
        "rustgd-viewer"
    };
    let bin = PathBuf::from(pkg_path).join("bin").join(viewer_name);
    if !bin.exists() {
        return Err(Error::Other(format!(
            "viewer binary not found at {}",
            bin.display()
        )));
    }
    Ok(bin)
}

fn locate_fonts_dir() -> Result<PathBuf, Error> {
    let pkg_path: String = R!(r#"system.file(package = "rustgd")"#)?
        .as_str()
        .ok_or_else(|| Error::Other("system.file returned non-string".into()))?
        .to_string();
    let dir = PathBuf::from(pkg_path).join("fonts");
    if !dir.exists() {
        return Err(Error::Other(format!(
            "fonts dir not found at {}",
            dir.display()
        )));
    }
    Ok(dir)
}

// ----------------------------------------------------------------------
// Drawing logic
// ----------------------------------------------------------------------
//
// Each method below corresponds to one R graphics device callback.
// They take individual scalar arguments rather than an R_GE_gcontext
// struct, so Rust never has to interact with R's reference-counted
// SEXP types (in particular gc->patternFill) or with any value-copied
// trampoline state. The C wrapper in src/rustgd_device.c reads
// gcontext fields and passes the few we care about as plain scalars.
// The extern "C" rustgd_cb_* exports further below are thin: they
// reconstruct &mut self from the raw device pointer and dispatch
// to the methods here.

impl RustgdDevice {
    fn handle_new_page(&mut self, width: f64, height: f64) {
        // Flush any pending content for the previous page first.
        // Safety net: catches drawing that happened after the last
        // mode(0) but before this new_page (rare, but harmless if so).
        self.flush();
        // If this new_page is happening inside an R-side replay (a
        // resize-triggered re-evaluation of the captured plot
        // expression), don't advance the page counter: we want the
        // replay to overwrite the existing page's file rather than
        // create a new entry.
        if !REPLAY_IN_PROGRESS.load(Ordering::SeqCst) {
            CURRENT_PAGE.fetch_add(1, Ordering::SeqCst);
        }
        NEW_PAGE_FIRED.store(true, Ordering::SeqCst);
        DREW_SOMETHING.store(true, Ordering::SeqCst);
        let w = width.max(1.0);
        let h = height.max(1.0);
        self.buffer.clear(w, h);
    }

    fn handle_mode(&mut self, mode: i32) {
        if mode == 0 {
            self.flush();
        }
    }

    fn handle_line(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, col: i32, lwd: f64) {
        DREW_SOMETHING.store(true, Ordering::SeqCst);
        let y1f = self.flip_y(y1);
        let y2f = self.flip_y(y2);
        let stroke = r_color_to_svg(col);
        let el = format!(
            "<line x1=\"{:.3}\" y1=\"{:.3}\" x2=\"{:.3}\" y2=\"{:.3}\" \
             stroke=\"{}\" stroke-width=\"{:.3}\" fill=\"none\"/>",
            x1, y1f, x2, y2f, stroke, lwd
        );
        self.buffer.push(el);
    }

    fn handle_polyline(&mut self, xs: &[f64], ys: &[f64], col: i32, lwd: f64) {
        DREW_SOMETHING.store(true, Ordering::SeqCst);
        let n = xs.len().min(ys.len());
        if n == 0 {
            return;
        }
        let stroke = r_color_to_svg(col);
        let points: Vec<String> = (0..n)
            .map(|i| format!("{:.3},{:.3}", xs[i], self.flip_y(ys[i])))
            .collect();
        let el = format!(
            "<polyline points=\"{}\" stroke=\"{}\" stroke-width=\"{:.3}\" fill=\"none\"/>",
            points.join(" "),
            stroke,
            lwd
        );
        self.buffer.push(el);
    }

    fn handle_rect(
        &mut self,
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        col: i32,
        fill: i32,
        lwd: f64,
    ) {
        DREW_SOMETHING.store(true, Ordering::SeqCst);
        let xa = x0.min(x1);
        let xb = x0.max(x1);
        let y0f = self.flip_y(y0);
        let y1f = self.flip_y(y1);
        let y_top = y0f.min(y1f);
        let width = xb - xa;
        let height = (y0f - y1f).abs();
        let stroke = r_color_to_svg(col);
        let fillstr = r_color_to_svg(fill);
        let el = format!(
            "<rect x=\"{:.3}\" y=\"{:.3}\" width=\"{:.3}\" height=\"{:.3}\" \
             stroke=\"{}\" fill=\"{}\" stroke-width=\"{:.3}\"/>",
            xa, y_top, width, height, stroke, fillstr, lwd
        );
        self.buffer.push(el);
    }

    fn handle_polygon(&mut self, xs: &[f64], ys: &[f64], col: i32, fill: i32, lwd: f64) {
        DREW_SOMETHING.store(true, Ordering::SeqCst);
        let n = xs.len().min(ys.len());
        if n == 0 {
            return;
        }
        let stroke = r_color_to_svg(col);
        let fillstr = r_color_to_svg(fill);
        let points: Vec<String> = (0..n)
            .map(|i| format!("{:.3},{:.3}", xs[i], self.flip_y(ys[i])))
            .collect();
        let el = format!(
            "<polygon points=\"{}\" stroke=\"{}\" fill=\"{}\" stroke-width=\"{:.3}\"/>",
            points.join(" "),
            stroke,
            fillstr,
            lwd
        );
        self.buffer.push(el);
    }

    fn handle_circle(&mut self, x: f64, y: f64, r: f64, col: i32, fill: i32, lwd: f64) {
        DREW_SOMETHING.store(true, Ordering::SeqCst);
        let cy = self.flip_y(y);
        let stroke = r_color_to_svg(col);
        let fillstr = r_color_to_svg(fill);
        let el = format!(
            "<circle cx=\"{:.3}\" cy=\"{:.3}\" r=\"{:.3}\" \
             stroke=\"{}\" fill=\"{}\" stroke-width=\"{:.3}\"/>",
            x, cy, r, stroke, fillstr, lwd
        );
        self.buffer.push(el);
    }

    fn handle_clip(&mut self, x0: f64, x1: f64, y0: f64, y1: f64) {
        self.buffer.set_clip(ClipRect { x0, y0, x1, y1 });
    }

    fn handle_path(
        &mut self,
        xs: &[f64],
        ys: &[f64],
        nper: &[i32],
        winding: bool,
        col: i32,
        fill: i32,
        lwd: f64,
    ) {
        DREW_SOMETHING.store(true, Ordering::SeqCst);
        let stroke = r_color_to_svg(col);
        let fillstr = r_color_to_svg(fill);
        let fill_rule = if winding { "nonzero" } else { "evenodd" };

        let mut d_parts: Vec<String> = Vec::new();
        let mut offset: usize = 0;
        for &count in nper {
            let count = count.max(0) as usize;
            if count == 0 {
                continue;
            }
            let end = offset + count;
            if end > xs.len() || end > ys.len() {
                break;
            }
            let (x0, y0) = (xs[offset], self.flip_y(ys[offset]));
            d_parts.push(format!("M{:.3},{:.3}", x0, y0));
            for i in (offset + 1)..end {
                d_parts.push(format!("L{:.3},{:.3}", xs[i], self.flip_y(ys[i])));
            }
            d_parts.push("Z".to_string());
            offset = end;
        }

        if d_parts.is_empty() {
            return;
        }

        let d = d_parts.join(" ");
        let el = format!(
            "<path d=\"{}\" stroke=\"{}\" fill=\"{}\" stroke-width=\"{:.3}\" \
             fill-rule=\"{}\"/>",
            d, stroke, fillstr, lwd, fill_rule
        );
        self.buffer.push(el);
    }

    fn handle_raster(
        &mut self,
        pixels: &[u32],
        w: u32,
        h: u32,
        x: f64,
        y: f64,
        target_w: f64,
        target_h: f64,
        angle: f64,
        interpolate: bool,
    ) {
        DREW_SOMETHING.store(true, Ordering::SeqCst);

        if pixels.is_empty() || w == 0 || h == 0 {
            return;
        }

        // Extract RGBA bytes via shifts (R packs as R | G<<8 | B<<16 | A<<24).
        // Writing this out explicitly keeps the code correct on any
        // architecture and makes the byte layout self-documenting.
        let mut rgba: Vec<u8> = Vec::with_capacity(pixels.len() * 4);
        for &p in pixels {
            rgba.push((p & 0xFF) as u8);
            rgba.push(((p >> 8) & 0xFF) as u8);
            rgba.push(((p >> 16) & 0xFF) as u8);
            rgba.push(((p >> 24) & 0xFF) as u8);
        }

        // Encode RGBA to PNG in memory. Any failure here drops the
        // raster silently rather than crashing the device, which is
        // the same policy svglite uses for malformed bitmaps.
        let mut png_buf: Vec<u8> = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png_buf, w, h);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = match encoder.write_header() {
                Ok(w) => w,
                Err(_) => return,
            };
            if writer.write_image_data(&rgba).is_err() {
                return;
            }
        }

        let b64 = BASE64_STANDARD.encode(&png_buf);

        // Coordinate mapping. R passes bottom-left in y-up device
        // coords; SVG <image>'s x/y is top-left in y-down. abs() on
        // the target size is defensive against any device convention
        // that might pass a signed height.
        let tw = target_w.abs();
        let th = target_h.abs();
        let svg_x = x;
        let svg_y = self.flip_y(y) - th;

        let interp_attr = if interpolate {
            ""
        } else {
            " image-rendering=\"pixelated\""
        };

        // R rotates anticlockwise around the bottom-left in y-up coords;
        // SVG rotate() is clockwise in y-down coords, so we negate the
        // angle. The rotation center stays at the bottom-left, which
        // after y-flip is at (x, flip_y(y)) in SVG coords. This sign
        // convention matches our text() rotation.
        let transform_attr = if angle.abs() > 0.001 {
            format!(
                " transform=\"rotate({:.3}, {:.3}, {:.3})\"",
                -angle,
                x,
                self.flip_y(y)
            )
        } else {
            String::new()
        };

        let el = format!(
            "<image x=\"{:.3}\" y=\"{:.3}\" width=\"{:.3}\" height=\"{:.3}\" \
             preserveAspectRatio=\"none\"{}{} \
             href=\"data:image/png;base64,{}\"/>",
            svg_x, svg_y, tw, th, interp_attr, transform_attr, b64
        );
        self.buffer.push(el);
    }

    fn handle_text(
        &mut self,
        x: f64,
        y: f64,
        text: &str,
        rot: f64,
        hadj: f64,
        col: i32,
        ps: f64,
        cex: f64,
        fontface: i32,
        fontfamily: &str,
    ) {
        DREW_SOMETHING.store(true, Ordering::SeqCst);
        if text.is_empty() {
            return;
        }
        let (family_idx, face_idx) = if fontface == 5 {
            (FAM_SANS, 0)
        } else {
            (family_index(fontfamily), face_index(fontface))
        };

        let yf = self.flip_y(y);
        let point_size = ps * cex;
        let font_size = point_size * (96.0 / 72.0);
        let fill = r_color_to_svg(col);

        let anchor = if hadj < 0.25 {
            "start"
        } else if hadj > 0.75 {
            "end"
        } else {
            "middle"
        };

        let transform = if rot.abs() > 0.001 {
            format!(" transform=\"rotate({:.3} {:.3} {:.3})\"", -rot, x, yf)
        } else {
            String::new()
        };

        let (weight, style) = face_svg_attrs(face_idx);
        let weight_attr = if weight == "bold" {
            " font-weight=\"bold\""
        } else {
            ""
        };
        let style_attr = if style == "italic" {
            " font-style=\"italic\""
        } else {
            ""
        };

        let family_name = FAMILY_SVG_NAMES[family_idx];

        let el = format!(
            "<text x=\"{:.3}\" y=\"{:.3}\" font-family=\"{}\" \
             font-size=\"{:.3}\"{}{} fill=\"{}\" text-anchor=\"{}\"{}>{}</text>",
            x,
            yf,
            family_name,
            font_size,
            weight_attr,
            style_attr,
            fill,
            anchor,
            transform,
            escape_xml(text)
        );
        self.buffer.push(el);
    }

    fn handle_strwidth(
        &self,
        text: &str,
        ps: f64,
        cex: f64,
        fontface: i32,
        fontfamily: &str,
    ) -> f64 {
        if text.is_empty() {
            return 0.0;
        }
        let (family_idx, face_idx) = if fontface == 5 {
            (FAM_SANS, 0)
        } else {
            (family_index(fontfamily), face_index(fontface))
        };
        let point_size = ps * cex;
        let font_size = point_size * (96.0 / 72.0);
        self.compute_text_width(text, font_size, family_idx, face_idx)
    }

    fn handle_char_metric(
        &self,
        c: i32,
        ps: f64,
        cex: f64,
        fontface: i32,
        fontfamily: &str,
    ) -> (f64, f64, f64) {
        let ch = match char::from_u32(c as u32) {
            Some(ch) => ch,
            None => return (0.0, 0.0, 0.0),
        };
        let (family_idx, face_idx) = if fontface == 5 {
            (FAM_SANS, 0)
        } else {
            (family_index(fontfamily), face_index(fontface))
        };

        let bytes = &self.font_bytes[family_idx][face_idx];
        let face = match ttf_parser::Face::parse(bytes, 0) {
            Ok(f) => f,
            Err(_) => return (0.0, 0.0, 0.0),
        };
        let units_per_em = face.units_per_em() as f64;
        if units_per_em <= 0.0 {
            return (0.0, 0.0, 0.0);
        }

        let point_size = ps * cex;
        let font_size = point_size * (96.0 / 72.0);
        let scale = font_size / units_per_em;

        let glyph_id = face.glyph_index(ch).unwrap_or(ttf_parser::GlyphId(0));
        let width = face.glyph_hor_advance(glyph_id).unwrap_or(0) as f64 * scale;
        let (ascent, descent) = if let Some(bbox) = face.glyph_bounding_box(glyph_id) {
            let asc = (bbox.y_max as f64 * scale).max(0.0);
            let desc = (-(bbox.y_min as f64) * scale).max(0.0);
            (asc, desc)
        } else {
            (0.0, 0.0)
        };

        (ascent, descent, width)
    }
}

// ----------------------------------------------------------------------
// FFI exports: the C wrapper in src/rustgd_device.c calls these
// ----------------------------------------------------------------------
//
// These are thin: null-check the device pointer, reconstruct
// &mut RustgdDevice (or &RustgdDevice for the read-only metrics
// queries), then dispatch to the handle_* method. Coordinate-array
// slices are constructed from (ptr, len) pairs and never outlive
// the FFI call. String slices come from CStr::to_str() against the
// const char* values R passes (UTF-8 per the textUTF8 contract).

unsafe fn cstr_or_empty<'a>(ptr: *const c_char) -> &'a str {
    if ptr.is_null() {
        ""
    } else {
        CStr::from_ptr(ptr).to_str().unwrap_or("")
    }
}

#[no_mangle]
pub unsafe extern "C" fn rustgd_cb_activate(_dev: *mut c_void) {}

#[no_mangle]
pub unsafe extern "C" fn rustgd_cb_deactivate(_dev: *mut c_void) {}

#[no_mangle]
pub unsafe extern "C" fn rustgd_cb_close(dev: *mut c_void) {
    if dev.is_null() {
        return;
    }
    // Take ownership back from the raw pointer. Explicit session-dir
    // cleanup happens here before the Box's Drop runs. After this
    // function returns, the Rust device's heap allocations (DrawBuffer,
    // font_bytes, dir, Child handle) have all been released. The Child
    // handle's Drop does not kill the viewer process; the viewer keeps
    // running until the user closes its window.
    let boxed = Box::from_raw(dev as *mut RustgdDevice);
    let _ = fs::remove_dir_all(&boxed.dir);
}

#[no_mangle]
pub unsafe extern "C" fn rustgd_cb_new_page(
    dev: *mut c_void,
    width: f64,
    height: f64,
    _fill: i32,
) {
    if dev.is_null() {
        return;
    }
    let device = &mut *(dev as *mut RustgdDevice);
    device.handle_new_page(width, height);
}

#[no_mangle]
pub unsafe extern "C" fn rustgd_cb_mode(dev: *mut c_void, mode: i32) {
    if dev.is_null() {
        return;
    }
    let device = &mut *(dev as *mut RustgdDevice);
    device.handle_mode(mode);
}

#[no_mangle]
pub unsafe extern "C" fn rustgd_cb_line(
    dev: *mut c_void,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    col: i32,
    lwd: f64,
    _lty: i32,
) {
    if dev.is_null() {
        return;
    }
    let device = &mut *(dev as *mut RustgdDevice);
    device.handle_line(x1, y1, x2, y2, col, lwd);
}

#[no_mangle]
pub unsafe extern "C" fn rustgd_cb_polyline(
    dev: *mut c_void,
    n: i32,
    x_ptr: *const f64,
    y_ptr: *const f64,
    col: i32,
    lwd: f64,
    _lty: i32,
) {
    if dev.is_null() || x_ptr.is_null() || y_ptr.is_null() || n <= 0 {
        return;
    }
    let device = &mut *(dev as *mut RustgdDevice);
    let n = n as usize;
    let xs = std::slice::from_raw_parts(x_ptr, n);
    let ys = std::slice::from_raw_parts(y_ptr, n);
    device.handle_polyline(xs, ys, col, lwd);
}

#[no_mangle]
pub unsafe extern "C" fn rustgd_cb_rect(
    dev: *mut c_void,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    col: i32,
    fill: i32,
    lwd: f64,
    _lty: i32,
) {
    if dev.is_null() {
        return;
    }
    let device = &mut *(dev as *mut RustgdDevice);
    device.handle_rect(x0, y0, x1, y1, col, fill, lwd);
}

#[no_mangle]
pub unsafe extern "C" fn rustgd_cb_polygon(
    dev: *mut c_void,
    n: i32,
    x_ptr: *const f64,
    y_ptr: *const f64,
    col: i32,
    fill: i32,
    lwd: f64,
    _lty: i32,
) {
    if dev.is_null() || x_ptr.is_null() || y_ptr.is_null() || n <= 0 {
        return;
    }
    let device = &mut *(dev as *mut RustgdDevice);
    let n = n as usize;
    let xs = std::slice::from_raw_parts(x_ptr, n);
    let ys = std::slice::from_raw_parts(y_ptr, n);
    device.handle_polygon(xs, ys, col, fill, lwd);
}

#[no_mangle]
pub unsafe extern "C" fn rustgd_cb_circle(
    dev: *mut c_void,
    x: f64,
    y: f64,
    r: f64,
    col: i32,
    fill: i32,
    lwd: f64,
    _lty: i32,
) {
    if dev.is_null() {
        return;
    }
    let device = &mut *(dev as *mut RustgdDevice);
    device.handle_circle(x, y, r, col, fill, lwd);
}

#[no_mangle]
pub unsafe extern "C" fn rustgd_cb_path(
    dev: *mut c_void,
    x_ptr: *const f64,
    y_ptr: *const f64,
    npoly: i32,
    nper_ptr: *const i32,
    winding: i32,
    col: i32,
    fill: i32,
    lwd: f64,
    _lty: i32,
) {
    if dev.is_null()
        || x_ptr.is_null()
        || y_ptr.is_null()
        || nper_ptr.is_null()
        || npoly <= 0
    {
        return;
    }
    let device = &mut *(dev as *mut RustgdDevice);
    let nper = std::slice::from_raw_parts(nper_ptr, npoly as usize);
    let total: usize = nper.iter().map(|&n| n.max(0) as usize).sum();
    if total == 0 {
        return;
    }
    let xs = std::slice::from_raw_parts(x_ptr, total);
    let ys = std::slice::from_raw_parts(y_ptr, total);
    device.handle_path(xs, ys, nper, winding != 0, col, fill, lwd);
}

#[no_mangle]
pub unsafe extern "C" fn rustgd_cb_raster(
    dev: *mut c_void,
    raster_ptr: *const u32,
    w: i32,
    h: i32,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    rot: f64,
    interpolate: i32,
) {
    if dev.is_null() || raster_ptr.is_null() || w <= 0 || h <= 0 {
        return;
    }
    let device = &mut *(dev as *mut RustgdDevice);
    let total = (w as usize) * (h as usize);
    let pixels = std::slice::from_raw_parts(raster_ptr, total);
    device.handle_raster(
        pixels,
        w as u32,
        h as u32,
        x,
        y,
        width,
        height,
        rot,
        interpolate != 0,
    );
}

#[no_mangle]
pub unsafe extern "C" fn rustgd_cb_text(
    dev: *mut c_void,
    x: f64,
    y: f64,
    str_ptr: *const c_char,
    rot: f64,
    hadj: f64,
    col: i32,
    ps: f64,
    cex: f64,
    fontface: i32,
    fontfamily_ptr: *const c_char,
) {
    if dev.is_null() || str_ptr.is_null() {
        return;
    }
    let device = &mut *(dev as *mut RustgdDevice);
    let text = cstr_or_empty(str_ptr);
    let fontfamily = cstr_or_empty(fontfamily_ptr);
    device.handle_text(x, y, text, rot, hadj, col, ps, cex, fontface, fontfamily);
}

#[no_mangle]
pub unsafe extern "C" fn rustgd_cb_strwidth(
    dev: *mut c_void,
    str_ptr: *const c_char,
    ps: f64,
    cex: f64,
    fontface: i32,
    fontfamily_ptr: *const c_char,
) -> f64 {
    if dev.is_null() || str_ptr.is_null() {
        return 0.0;
    }
    let device = &*(dev as *mut RustgdDevice);
    let text = cstr_or_empty(str_ptr);
    let fontfamily = cstr_or_empty(fontfamily_ptr);
    device.handle_strwidth(text, ps, cex, fontface, fontfamily)
}

#[no_mangle]
pub unsafe extern "C" fn rustgd_cb_char_metric(
    dev: *mut c_void,
    c: i32,
    ps: f64,
    cex: f64,
    fontface: i32,
    fontfamily_ptr: *const c_char,
    ascent: *mut f64,
    descent: *mut f64,
    width: *mut f64,
) {
    if dev.is_null() || ascent.is_null() || descent.is_null() || width.is_null() {
        return;
    }
    let device = &*(dev as *mut RustgdDevice);
    let fontfamily = cstr_or_empty(fontfamily_ptr);
    let (a, d, w) = device.handle_char_metric(c, ps, cex, fontface, fontfamily);
    *ascent = a;
    *descent = d;
    *width = w;
}

#[no_mangle]
pub unsafe extern "C" fn rustgd_cb_clip(
    dev: *mut c_void,
    x0: f64,
    x1: f64,
    y0: f64,
    y1: f64,
) {
    if dev.is_null() {
        return;
    }
    let device = &mut *(dev as *mut RustgdDevice);
    device.handle_clip(x0, x1, y0, y1);
}


// ----------------------------------------------------------------------
// extendr entry points
// ----------------------------------------------------------------------

#[extendr]
fn rustgd_activate(session_dir: &str) {
    *SESSION_DIR.lock().unwrap() = Some(PathBuf::from(session_dir));
    *CURRENT_SIZE.lock().unwrap() = (DEFAULT_W, DEFAULT_H);
    NEW_PAGE_FIRED.store(false, Ordering::SeqCst);
    DREW_SOMETHING.store(false, Ordering::SeqCst);
    REPLAY_IN_PROGRESS.store(false, Ordering::SeqCst);
    CURRENT_PAGE.store(0, Ordering::SeqCst);

    // Construct the Rust device. On success, transfer ownership to a
    // raw pointer that the C side will store in dd->deviceSpecific.
    // The matching Box::from_raw on close happens inside rustgd_cb_close.
    let device_box = match RustgdDevice::new() {
        Ok(d) => Box::new(d),
        Err(e) => {
            extendr_api::throw_r_error(&format!("rustgd activation failed: {e:?}"));
        }
    };
    let raw_ptr = Box::into_raw(device_box) as *mut c_void;

    let name = std::ffi::CString::new("rustgd").unwrap();
    let result = unsafe {
        rustgd_register_device(raw_ptr, DEFAULT_W, DEFAULT_H, name.as_ptr())
    };
    if result != 0 {
        // Registration failed before R adopted the pointer; reclaim
        // ownership and drop the device cleanly before erroring out.
        unsafe {
            let _ = Box::from_raw(raw_ptr as *mut RustgdDevice);
        }
        extendr_api::throw_r_error(
            "rustgd: failed to register device with R graphics engine",
        );
    }
}

#[extendr]
fn rustgd_set_size(width: f64, height: f64) {
    let w = width.max(1.0);
    let h = height.max(1.0);
    *CURRENT_SIZE.lock().unwrap() = (w, h);
    // Update R's stored dd extent through the C helper, which calls
    // GEcurrentDevice() and writes the four bounds + four clip bounds.
    unsafe {
        rustgd_set_device_size(w, h);
    }
}

#[extendr]
fn rustgd_check_new_page() -> bool {
    NEW_PAGE_FIRED.swap(false, Ordering::SeqCst)
}

#[extendr]
fn rustgd_check_drew() -> bool {
    DREW_SOMETHING.swap(false, Ordering::SeqCst)
}

/// Toggle replay mode. R-side sets this to true before re-evaluating
/// captured plot expressions in response to a resize, and back to false
/// after. While true, new_page() does not advance the page counter, so
/// the replay overwrites the existing page's file.
#[extendr]
fn rustgd_set_replay_mode(on: bool) {
    REPLAY_IN_PROGRESS.store(on, Ordering::SeqCst);
}

/// Set the current page number directly. R-side uses this during
/// multi-plot history replay to direct each replayed plot's flushes
/// to a specific plot-NNNN.svg file. Only meaningful in conjunction
/// with replay mode.
#[extendr]
fn rustgd_set_current_page(page: i32) {
    let n = if page < 0 { 0 } else { page as usize };
    CURRENT_PAGE.store(n, Ordering::SeqCst);
}

extendr_module! {
    mod rustgd;
    fn rustgd_activate;
    fn rustgd_set_size;
    fn rustgd_check_new_page;
    fn rustgd_check_drew;
    fn rustgd_set_replay_mode;
    fn rustgd_set_current_page;
}
