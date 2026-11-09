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
//! Drawing. Every callback appends a record to the page's display list,
//! defined in display_list.rs. Each write turns that list into the
//! page's files. It always becomes SVG text, and with
//! options(rustgd.transport = "binary") the list itself is also written
//! as a .rgd file. extendr's default CLIPPING_STRATEGY is DeviceAndEngine,
//! so canClip is set to TRUE automatically at device creation and R calls
//! our clip() callback throughout each plot.

use extendr_api::{
    prelude::*,
    Error,
};
use std::ffi::{c_char, c_void, CStr};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

mod display_list;
use display_list::{RecordWriter, Style, SvgWriter};

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

/// Set when the device reopened a closed window and cleared the old
/// plots. R reads and resets it after each command and drops its plot
/// history to match.
static GALLERY_RESET: AtomicBool = AtomicBool::new(false);

/// Whether devices also write each page's display list as a .rgd file.
/// R sets it from options(rustgd.transport) right before a device opens,
/// and the redraw devices R opens later follow the same setting.
static TRANSPORT_BINARY: AtomicBool = AtomicBool::new(false);

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

/// The stroke fields of R's graphics context, copied out as plain scalars
/// by the C wrapper. Grouping them keeps the handle_* signatures short.
#[derive(Clone, Copy)]
struct StrokeStyle {
    lwd: f64,
    lty: i32,
    lend: i32,
    ljoin: i32,
    lmitre: f64,
}

impl StrokeStyle {
    fn new(lwd: f64, lty: i32, lend: i32, ljoin: i32, lmitre: f64) -> Self {
        Self { lwd, lty, lend, ljoin, lmitre }
    }

    /// The display list's Style record for a shape drawn in these colors.
    fn record(&self, col: i32, fill: i32) -> Style {
        Style {
            col,
            fill,
            lwd: self.lwd as f32,
            lty: self.lty,
            lend: self.lend as u8,
            ljoin: self.ljoin as u8,
            lmitre: self.lmitre as f32,
        }
    }
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

// ----------------------------------------------------------------------
// The page buffer
// ----------------------------------------------------------------------

/// The page being drawn. Its records are the single source for every file
/// written for the page. The SVG writer turns them into SVG incrementally,
/// so writing a growing page again only formats what is new.
struct PageBuffer {
    records: RecordWriter,
    svg: SvgWriter,
    has_content: bool,
    width: f64,
    height: f64,
}

impl PageBuffer {
    fn new() -> Self {
        Self {
            records: RecordWriter::new(),
            svg: SvgWriter::new(),
            has_content: false,
            width: DEFAULT_W,
            height: DEFAULT_H,
        }
    }

    /// Start a new page at the given size. Allocations are kept.
    fn clear(&mut self, width: f64, height: f64) {
        self.records.clear();
        self.svg.clear();
        self.has_content = false;
        self.width = width;
        self.height = height;
    }
}

/// Write a file under a temporary name and rename it into place, so the
/// viewer never reads a half-written page.
fn write_atomic(path: &std::path::Path, bytes: &[u8]) {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    if fs::write(&tmp, bytes).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

// ----------------------------------------------------------------------
// The device
// ----------------------------------------------------------------------

/// Read and check the bundled fonts the device measures text with.
fn load_fonts(fonts_dir: &std::path::Path) -> Result<[[Vec<u8>; NUM_FACES]; NUM_FAMILIES], Error> {
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
    Ok(font_bytes)
}

struct RustgdDevice {
    dir: PathBuf,
    /// The viewer binary and fonts folder, kept so the window can be
    /// reopened if it is closed before R notices.
    viewer_bin: PathBuf,
    fonts_dir: PathBuf,
    /// The viewer process. None for a redraw device, which opens no window.
    viewer_child: Option<std::process::Child>,
    /// Set on a short-lived redraw device. It always writes this page's
    /// file and leaves the page counter and session folder alone.
    offscreen_page: Option<usize>,
    /// Also write each page's display list as a .rgd file. Fixed when the
    /// device opens, from options(rustgd.transport).
    binary: bool,
    page: PageBuffer,
    font_bytes: [[Vec<u8>; NUM_FACES]; NUM_FAMILIES],
}

impl RustgdDevice {
    fn new() -> Result<Self, Error> {
        sweep_orphans();
        let dir = session_dir();
        fs::create_dir_all(&dir).map_err(|e| Error::Other(format!("create session dir: {e}")))?;

        let viewer = locate_viewer()?;
        let fonts_dir = locate_fonts_dir()?;

        let font_bytes = load_fonts(&fonts_dir)?;

        let child = Command::new(&viewer)
            .arg(&dir)
            .arg(&fonts_dir)
            .spawn()
            .map_err(|e| Error::Other(format!("launch viewer: {e}")))?;

        Ok(Self {
            dir,
            viewer_bin: viewer,
            fonts_dir,
            viewer_child: Some(child),
            offscreen_page: None,
            binary: TRANSPORT_BINARY.load(Ordering::SeqCst),
            page: PageBuffer::new(),
            font_bytes,
        })
    }

    /// A redraw device. R opens one briefly to redraw a plot other than the
    /// one the main device is on. It writes that plot's file at the size R
    /// registers it with, launches no viewer, and shares the session folder
    /// and fonts with the main device.
    fn new_offscreen(page: usize) -> Result<Self, Error> {
        let dir = session_dir();
        fs::create_dir_all(&dir).map_err(|e| Error::Other(format!("create session dir: {e}")))?;
        let fonts_dir = locate_fonts_dir()?;
        let font_bytes = load_fonts(&fonts_dir)?;
        Ok(Self {
            dir,
            viewer_bin: PathBuf::new(),
            fonts_dir,
            viewer_child: None,
            offscreen_page: Some(page),
            binary: TRANSPORT_BINARY.load(Ordering::SeqCst),
            page: PageBuffer::new(),
            font_bytes,
        })
    }

    /// Reopen the viewer if its window has been closed but R has not yet
    /// retired the device. That happens when a plot is sent while R is
    /// still busy, before the idle-time poll sees the close. Without this
    /// the plot would be drawn into a device whose window is gone.
    ///
    /// Closing the window means starting over, the same as when R notices
    /// the close itself. So the old plots are deleted, the page count goes
    /// back to zero, and R is told to drop its plot history.
    fn ensure_viewer(&mut self) {
        let exited = matches!(
            self.viewer_child.as_mut().map(|child| child.try_wait()),
            Some(Ok(Some(_)))
        );
        if exited {
            if let Ok(entries) = fs::read_dir(&self.dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with("plot-")
                        && (name.ends_with(".svg") || name.ends_with(".rgd"))
                    {
                        let _ = fs::remove_file(entry.path());
                    }
                }
            }
            // Clear the signals the closed window left behind. The close
            // marker would make the R-side poll shut the device down under
            // the new window, and an unread clear request would delete the
            // plot that is about to be drawn.
            for name in [
                "viewer_closed",
                "resize.txt",
                "view.txt",
                "clear_plot.txt",
                "clear_all.txt",
            ] {
                let _ = fs::remove_file(self.dir.join(name));
            }
            CURRENT_PAGE.store(0, Ordering::SeqCst);
            GALLERY_RESET.store(true, Ordering::SeqCst);
            if let Ok(child) = Command::new(&self.viewer_bin)
                .arg(&self.dir)
                .arg(&self.fonts_dir)
                .spawn()
            {
                self.viewer_child = Some(child);
            }
        }
    }

    fn flush(&mut self) {
        if !self.page.has_content {
            return;
        }
        let n = self
            .offscreen_page
            .unwrap_or_else(|| CURRENT_PAGE.load(Ordering::SeqCst));
        if n == 0 {
            // No page has been started yet (no new_page() has fired);
            // nothing meaningful to write.
            return;
        }
        let w = self.page.width as f32;
        let h = self.page.height as f32;
        if self.binary {
            let records = self.page.records.bytes();
            let mut file = Vec::with_capacity(display_list::HEADER_LEN + records.len());
            display_list::write_header(
                &mut file,
                w,
                h,
                display_list::WHITE,
                self.page.records.count(),
            );
            file.extend_from_slice(records);
            write_atomic(&self.dir.join(format!("plot-{:04}.rgd", n)), &file);
        }
        // The viewer reads SVG until it learns to draw .rgd pages, so every
        // page is written as SVG as well for now.
        if self.page.svg.catch_up(self.page.records.bytes()).is_ok() {
            let svg = self.page.svg.render(w, h, display_list::WHITE);
            write_atomic(&self.dir.join(format!("plot-{:04}.svg", n)), svg.as_bytes());
        }
        self.page.has_content = false;
    }

    fn flip_y(&self, y: f64) -> f64 {
        self.page.height - y
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
        if self.offscreen_page.is_none() && !REPLAY_IN_PROGRESS.load(Ordering::SeqCst) {
            // A user-issued plot. Make sure it has a window to land in.
            self.ensure_viewer();
            CURRENT_PAGE.fetch_add(1, Ordering::SeqCst);
        }
        NEW_PAGE_FIRED.store(true, Ordering::SeqCst);
        DREW_SOMETHING.store(true, Ordering::SeqCst);
        let w = width.max(1.0);
        let h = height.max(1.0);
        self.page.clear(w, h);
    }

    fn handle_mode(&mut self, mode: i32) {
        if mode == 0 {
            self.flush();
        }
    }

    fn handle_line(
        &mut self,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        col: i32,
        style: StrokeStyle,
    ) {
        DREW_SOMETHING.store(true, Ordering::SeqCst);
        let y1f = self.flip_y(y1);
        let y2f = self.flip_y(y2);
        self.page.records.style(style.record(col, 0));
        self.page
            .records
            .line(x1 as f32, y1f as f32, x2 as f32, y2f as f32);
        self.page.has_content = true;
    }

    fn handle_polyline(&mut self, xs: &[f64], ys: &[f64], col: i32, style: StrokeStyle) {
        DREW_SOMETHING.store(true, Ordering::SeqCst);
        let n = xs.len().min(ys.len());
        if n == 0 {
            return;
        }
        let h = self.page.height;
        self.page.records.style(style.record(col, 0));
        self.page
            .records
            .polyline((0..n).map(|i| (xs[i] as f32, (h - ys[i]) as f32)));
        self.page.has_content = true;
    }

    fn handle_rect(
        &mut self,
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        col: i32,
        fill: i32,
        style: StrokeStyle,
    ) {
        DREW_SOMETHING.store(true, Ordering::SeqCst);
        let xa = x0.min(x1);
        let xb = x0.max(x1);
        let y0f = self.flip_y(y0);
        let y1f = self.flip_y(y1);
        let y_top = y0f.min(y1f);
        let width = xb - xa;
        let height = (y0f - y1f).abs();
        self.page.records.style(style.record(col, fill));
        self.page
            .records
            .rect(xa as f32, y_top as f32, width as f32, height as f32);
        self.page.has_content = true;
    }

    fn handle_polygon(
        &mut self,
        xs: &[f64],
        ys: &[f64],
        col: i32,
        fill: i32,
        style: StrokeStyle,
    ) {
        DREW_SOMETHING.store(true, Ordering::SeqCst);
        let n = xs.len().min(ys.len());
        if n == 0 {
            return;
        }
        let h = self.page.height;
        self.page.records.style(style.record(col, fill));
        self.page
            .records
            .polygon((0..n).map(|i| (xs[i] as f32, (h - ys[i]) as f32)));
        self.page.has_content = true;
    }

    fn handle_circle(
        &mut self,
        x: f64,
        y: f64,
        r: f64,
        col: i32,
        fill: i32,
        style: StrokeStyle,
    ) {
        DREW_SOMETHING.store(true, Ordering::SeqCst);
        let cy = self.flip_y(y);
        self.page.records.style(style.record(col, fill));
        self.page.records.circle(x as f32, cy as f32, r as f32);
        self.page.has_content = true;
    }

    /// R gives the clip corners in y-up device coordinates. The record
    /// keeps them flipped to y-down like everything else on the page.
    fn handle_clip(&mut self, x0: f64, x1: f64, y0: f64, y1: f64) {
        let y0f = self.flip_y(y0);
        let y1f = self.flip_y(y1);
        self.page
            .records
            .clip(x0 as f32, y0f as f32, x1 as f32, y1f as f32);
    }

    fn handle_path(
        &mut self,
        xs: &[f64],
        ys: &[f64],
        nper: &[i32],
        winding: bool,
        col: i32,
        fill: i32,
        style: StrokeStyle,
    ) {
        DREW_SOMETHING.store(true, Ordering::SeqCst);
        // Keep each subpath that has points, and stop at one that would run
        // past the coordinates R passed.
        let mut counts: Vec<u32> = Vec::with_capacity(nper.len());
        let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(nper.len());
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
            counts.push(count as u32);
            ranges.push((offset, end));
            offset = end;
        }
        if counts.is_empty() {
            return;
        }
        let h = self.page.height;
        self.page.records.style(style.record(col, fill));
        let points = ranges
            .iter()
            .flat_map(|&(a, b)| (a..b).map(move |i| (xs[i] as f32, (h - ys[i]) as f32)));
        self.page.records.path(winding, &counts, points);
        self.page.has_content = true;
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
        // A raster whose pixel count does not match its size cannot be
        // encoded, so it is dropped, as the PNG encoder always has.
        if pixels.len() != (w as usize) * (h as usize) {
            return;
        }
        let mut rgba: Vec<u8> = Vec::with_capacity(pixels.len() * 4);
        for &p in pixels {
            rgba.push((p & 0xFF) as u8);
            rgba.push(((p >> 8) & 0xFF) as u8);
            rgba.push(((p >> 16) & 0xFF) as u8);
            rgba.push(((p >> 24) & 0xFF) as u8);
        }
        // R anchors and rotates the image at its bottom-left corner. The
        // record keeps that corner, flipped to y-down, and the drawn size.
        // abs() guards against a device convention that passes a signed
        // height.
        let corner_y = self.flip_y(y);
        self.page.records.raster(&display_list::Raster {
            x: x as f32,
            y: corner_y as f32,
            w: target_w.abs() as f32,
            h: target_h.abs() as f32,
            angle: angle as f32,
            interpolate,
            pw: w,
            ph: h,
            pixels: &rgba,
        });
        self.page.has_content = true;
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
        // Plotmath symbols (fontface 5) are drawn from the sans family.
        let (family_idx, face_idx) = if fontface == 5 {
            (FAM_SANS, 0)
        } else {
            (family_index(fontfamily), face_index(fontface))
        };
        let yf = self.flip_y(y);
        let point_size = ps * cex;
        let font_size = point_size * (96.0 / 72.0);
        self.page.records.text(&display_list::Text {
            x: x as f32,
            y: yf as f32,
            rot: rot as f32,
            hadj: hadj as f32,
            size: font_size as f32,
            col,
            family: family_idx as u8,
            face: face_idx as u8,
            text,
        });
        self.page.has_content = true;
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
        // R encodes the character in c with two conventions, the same ones
        // R's own cairo device (Cairo_MetricInfo) honors. A negative c means
        // Unicode code point -c, and in a UTF-8 locale plotmath sends every
        // character this way, including spaces and the Greek letters of the
        // symbol font. A c of 0 asks for the metrics of 'M'. Reading c as a
        // plain u32 turned every negative code into an invalid char, which
        // collapsed plotmath glyph widths to zero and stacked them on top of
        // each other.
        let code = if c == 0 { 77 } else { c.unsigned_abs() };
        let ch = match char::from_u32(code) {
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
    // function returns, the Rust device's heap allocations (page buffer,
    // font_bytes, dir, Child handle) have all been released. The Child
    // handle's Drop does not kill the viewer process; the viewer keeps
    // running until the user closes its window.
    let mut boxed = Box::from_raw(dev as *mut RustgdDevice);
    if boxed.offscreen_page.is_some() {
        // A redraw device writes its page and leaves the session folder,
        // which belongs to the main device, alone.
        boxed.flush();
    } else {
        let _ = fs::remove_dir_all(&boxed.dir);
    }
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
    lty: i32,
    lend: i32,
    ljoin: i32,
    lmitre: f64,
) {
    if dev.is_null() {
        return;
    }
    let device = &mut *(dev as *mut RustgdDevice);
    device.handle_line(x1, y1, x2, y2, col, StrokeStyle::new(lwd, lty, lend, ljoin, lmitre));
}

#[no_mangle]
pub unsafe extern "C" fn rustgd_cb_polyline(
    dev: *mut c_void,
    n: i32,
    x_ptr: *const f64,
    y_ptr: *const f64,
    col: i32,
    lwd: f64,
    lty: i32,
    lend: i32,
    ljoin: i32,
    lmitre: f64,
) {
    if dev.is_null() || x_ptr.is_null() || y_ptr.is_null() || n <= 0 {
        return;
    }
    let device = &mut *(dev as *mut RustgdDevice);
    let n = n as usize;
    let xs = std::slice::from_raw_parts(x_ptr, n);
    let ys = std::slice::from_raw_parts(y_ptr, n);
    device.handle_polyline(xs, ys, col, StrokeStyle::new(lwd, lty, lend, ljoin, lmitre));
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
    lty: i32,
    lend: i32,
    ljoin: i32,
    lmitre: f64,
) {
    if dev.is_null() {
        return;
    }
    let device = &mut *(dev as *mut RustgdDevice);
    device.handle_rect(x0, y0, x1, y1, col, fill, StrokeStyle::new(lwd, lty, lend, ljoin, lmitre));
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
    lty: i32,
    lend: i32,
    ljoin: i32,
    lmitre: f64,
) {
    if dev.is_null() || x_ptr.is_null() || y_ptr.is_null() || n <= 0 {
        return;
    }
    let device = &mut *(dev as *mut RustgdDevice);
    let n = n as usize;
    let xs = std::slice::from_raw_parts(x_ptr, n);
    let ys = std::slice::from_raw_parts(y_ptr, n);
    device.handle_polygon(xs, ys, col, fill, StrokeStyle::new(lwd, lty, lend, ljoin, lmitre));
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
    lty: i32,
    lend: i32,
    ljoin: i32,
    lmitre: f64,
) {
    if dev.is_null() {
        return;
    }
    let device = &mut *(dev as *mut RustgdDevice);
    device.handle_circle(x, y, r, col, fill, StrokeStyle::new(lwd, lty, lend, ljoin, lmitre));
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
    lty: i32,
    lend: i32,
    ljoin: i32,
    lmitre: f64,
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
    device.handle_path(xs, ys, nper, winding != 0, col, fill, StrokeStyle::new(lwd, lty, lend, ljoin, lmitre));
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
    GALLERY_RESET.store(false, Ordering::SeqCst);

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

// Toggle replay mode. R-side sets this to true before re-evaluating
// captured plot expressions in response to a resize, and back to false
// after. While true, new_page() does not advance the page counter, so
// the replay overwrites the existing page's file.
#[extendr]
fn rustgd_set_replay_mode(on: bool) {
    REPLAY_IN_PROGRESS.store(on, Ordering::SeqCst);
}

// Set the current page number directly. R-side uses this during
// multi-plot history replay to direct each replayed plot's flushes
// to a specific plot-NNNN.svg file. Only meaningful in conjunction
// with replay mode.
#[extendr]
fn rustgd_set_current_page(page: i32) {
    let n = if page < 0 { 0 } else { page as usize };
    CURRENT_PAGE.store(n, Ordering::SeqCst);
}

// True once after the device reopened a closed window and cleared the old
// plots. The R side then drops its plot history to match.
#[extendr]
fn rustgd_take_gallery_reset() -> bool {
    GALLERY_RESET.swap(false, Ordering::SeqCst)
}

// The page the main device is on. R stores it with each plot so a plot
// always maps to its own file.
#[extendr]
fn rustgd_current_page() -> i32 {
    CURRENT_PAGE.load(Ordering::SeqCst) as i32
}

// The main device's size in device units, as R last set it.
#[extendr]
fn rustgd_device_size() -> Vec<f64> {
    let (w, h) = *CURRENT_SIZE.lock().unwrap();
    vec![w, h]
}

// Choose how devices opened from now on write their pages. R calls this
// with options(rustgd.transport) right before it opens a device.
#[extendr]
fn rustgd_set_transport(binary: bool) {
    TRANSPORT_BINARY.store(binary, Ordering::SeqCst);
}

// Check every .rgd page in a folder against the .svg written beside it.
// Each .rgd is decoded and turned back into SVG, which must match the .svg
// byte for byte. Returns how many pages were checked and which pages
// differ, fail to decode, or have no .svg.
#[extendr]
fn rustgd_binary_check(dir: &str) -> Robj {
    let mut checked = 0i32;
    let mut mismatched: Vec<i32> = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(page) = name
                .strip_prefix("plot-")
                .and_then(|s| s.strip_suffix(".rgd"))
                .and_then(|s| s.parse::<i32>().ok())
            else {
                continue;
            };
            checked += 1;
            let svg_path = std::path::Path::new(dir).join(format!("plot-{:04}.svg", page));
            let same = match (fs::read(entry.path()), fs::read(&svg_path)) {
                (Ok(rgd), Ok(svg)) => display_list::file_to_svg(&rgd)
                    .map(|s| s.into_bytes() == svg)
                    .unwrap_or(false),
                _ => false,
            };
            if !same {
                mismatched.push(page);
            }
        }
    }
    mismatched.sort_unstable();
    list!(checked = checked, mismatched = mismatched).into()
}

// Open a short-lived redraw device for one page at the given size and make
// it R's current device. R closes it with dev.off() once the plot is drawn.
#[extendr]
fn rustgd_open_offscreen(page: i32, width: f64, height: f64) {
    let page = if page < 1 { 1 } else { page as usize };
    let device_box = match RustgdDevice::new_offscreen(page) {
        Ok(d) => Box::new(d),
        Err(e) => {
            extendr_api::throw_r_error(&format!("rustgd redraw device failed: {e:?}"));
        }
    };
    let raw_ptr = Box::into_raw(device_box) as *mut c_void;
    let name = std::ffi::CString::new("rustgd_offscreen").unwrap();
    let result =
        unsafe { rustgd_register_device(raw_ptr, width.max(1.0), height.max(1.0), name.as_ptr()) };
    if result != 0 {
        unsafe {
            let _ = Box::from_raw(raw_ptr as *mut RustgdDevice);
        }
        extendr_api::throw_r_error("rustgd: failed to register the redraw device");
    }
}

extendr_module! {
    mod rustgd;
    fn rustgd_activate;
    fn rustgd_set_size;
    fn rustgd_check_new_page;
    fn rustgd_check_drew;
    fn rustgd_set_replay_mode;
    fn rustgd_set_current_page;
    fn rustgd_take_gallery_reset;
    fn rustgd_current_page;
    fn rustgd_device_size;
    fn rustgd_open_offscreen;
    fn rustgd_set_transport;
    fn rustgd_binary_check;
}
