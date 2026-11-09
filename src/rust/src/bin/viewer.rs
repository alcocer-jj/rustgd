//! rustgd-viewer, the standalone window the device's R side launches.
//! Watches a directory passed as argv[1] and displays the plots in it.
//! A page arrives as an SVG file or as a .rgd display list, which is drawn
//! directly without parsing anything.
//! Loads a bundled fonts directory from argv[2]. Liberation Sans is
//! registered as egui's primary font so the toolbar's arrows render
//! correctly and the UI typography matches the SVG plot typography.
//!
//! Parsing and rasterizing happen on a background render thread, never on
//! the window thread, so the window keeps responding to clicks, resizing,
//! and the close button while a heavy plot renders. The render thread
//! always works on the newest request and drops stale ones. Large files
//! get a quick low-resolution pass first so they appear sooner.
//!
//! Detects window resize and writes resize.txt into the watch directory
//! once the size has held still, so the R side can apply the new
//! dimensions and replay the plots. While a resize is in progress the
//! last image is stretched to fit instead of being redrawn. It also
//! writes view.txt whenever the plot on screen changes, so R knows
//! which plot to redraw first.
//!
//! On exit (user closes the window), writes a `viewer_closed`
//! signal file into the watch directory so the R-side poller
//! can call `dev.off()` and clean up the session.
//!
//! Gallery view. All pages in the watch directory are sorted
//! ascending and navigable with prev/next buttons in the top toolbar,
//! or with the Left/Right/Home/End keys. When a new plot arrives, the
//! view auto-jumps to it. A changed file modification time (an in-place
//! overwrite such as a resize replay) triggers a new render.
//!
//! The UI follows the OS appearance setting (Light or Dark on macOS),
//! including runtime changes. The plot itself keeps R's native
//! background, typically white, regardless of UI theme.
#![windows_subsystem = "windows"]

use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

use eframe::egui;
use tiny_skia::{Pixmap, Transform};

// The display list format and its renderer, shared with the device.
#[allow(dead_code)]
#[path = "../display_list.rs"]
mod display_list;
#[path = "../rgd_render.rs"]
mod rgd_render;

/// On-screen supersampling factor. The viewer rasterizes each SVG into a
/// pixmap this many times larger than the displayed area in each
/// dimension, then displays the resulting texture at the logical area
/// size. egui's default `Linear` texture filter performs the downsample
/// on the GPU at draw time, yielding crisper text edges and thinner
/// line antialiasing than tiny_skia's analytical AA produces on its
/// own. A factor of 2.0 means 4x more pixels rasterized and uploaded
/// per render. Export-to-PNG is unaffected; it has its own DPI-driven
/// resolution choice.
const SUPERSAMPLE: f32 = 2.0;

/// How long the plot area must keep one size before the viewer redraws
/// at that size and tells R to replay. Until then the last image is
/// stretched to fit on the GPU, which costs nothing.
const RESIZE_SETTLE: Duration = Duration::from_millis(150);

/// For heavy plots, a resize alone waits this long before redrawing the
/// old file at the new size, because R's replay at the new size usually
/// lands first and makes that redraw unnecessary.
const HEAVY_RESIZE_SETTLE: Duration = Duration::from_millis(1500);

/// SVG files at least this large get a quick low-resolution pass before
/// the full one, so a heavy plot shows up right away. Setting it to
/// u64::MAX turns the quick pass off.
const PREVIEW_MIN_BYTES: u64 = 2_000_000;

/// The same for .rgd pages. They draw several times faster than the SVG
/// of the same plot, since nothing has to be parsed, so they count as
/// heavy only from a much larger size.
const PREVIEW_MIN_BYTES_RGD: u64 = 24_000_000;

/// How long a render may run before the "Rendering" note appears, so
/// fast plots never flash it.
const BUSY_NOTE_DELAY: Duration = Duration::from_millis(250);

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let watch_dir = match args.get(1) {
        Some(d) => PathBuf::from(d),
        None => {
            eprintln!("usage: rustgd-viewer <watch_directory> [<fonts_directory>]");
            std::process::exit(1);
        }
    };
    let fonts_dir = args.get(2).map(PathBuf::from);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("rustgd")
            .with_inner_size([900.0, 700.0]),
        ..Default::default()
    };

    // Keep a copy of the watch directory for the post-run signal.
    let watch_dir_for_close = watch_dir.clone();

    let _ = eframe::run_native(
        "rustgd",
        options,
        Box::new(move |cc| {
            setup_egui_fonts(&cc.egui_ctx, fonts_dir.as_deref());
            // Follow the OS appearance setting (Light / Dark). egui
            // re-evaluates the system theme on relevant events, so a
            // mid-session toggle of macOS Appearance updates the UI
            // automatically. The default in egui is Dark, which is
            // why the viewer started out dark before this opt-in.
            cc.egui_ctx.options_mut(|opts| {
                opts.theme_preference = egui::ThemePreference::System;
            });
            // The app is built here because its render thread needs the
            // egui context to wake the window when an image is ready.
            Ok(Box::new(ViewerApp::new(
                watch_dir,
                fonts_dir,
                cc.egui_ctx.clone(),
            )))
        }),
    );

    // After run_native returns (user closed the window), drop a signal
    // file so the R-side poller can detect the close and call dev.off().
    // If the directory is already gone (R closed the device externally),
    // the write fails silently and there's nothing to do anyway.
    let _ = std::fs::write(watch_dir_for_close.join("viewer_closed"), b"");
}

/// Register Liberation Sans (bundled in the package's fonts dir) as
/// egui's primary proportional font. This gives the UI proper glyph
/// coverage — egui's default font lacks arrow characters like ← and →,
/// which would otherwise render as tofu boxes.
fn setup_egui_fonts(ctx: &egui::Context, fonts_dir: Option<&Path>) {
    let Some(dir) = fonts_dir else { return };
    let font_path = dir.join("LiberationSans-Regular.ttf");
    let Ok(bytes) = std::fs::read(&font_path) else {
        return;
    };

    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "liberation_sans".to_owned(),
        egui::FontData::from_owned(bytes),
    );
    if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        family.insert(0, "liberation_sans".to_owned());
    }
    ctx.set_fonts(fonts);
}

struct ViewerApp {
    watch_dir: PathBuf,
    fonts_dir: Option<PathBuf>,
    /// The pages in the watch directory, in page order. The last entry is
    /// the most recent plot.
    pages: Vec<PageFiles>,
    /// Index into `pages` of the plot currently displayed. Valid only
    /// when `pages` is non-empty.
    current_index: usize,
    /// Draw a page from its SVG even when its .rgd exists. Only matters
    /// with options(rustgd.debug_svg = TRUE), which writes both.
    show_svg: bool,
    /// What the texture currently holds. None until the first image
    /// arrives from the render thread.
    displayed: Option<Displayed>,
    /// The single GPU texture the plot is drawn into. Reused across
    /// renders via `TextureHandle::set` rather than allocating a new
    /// texture each frame, so a resize never holds the old and new
    /// textures alive at the same time. None until the first render,
    /// and released when there is no plot to show.
    texture: Option<egui::TextureHandle>,
    /// Hand-off slot to the render thread, which always takes the newest
    /// job and drops any it never started.
    jobs: Arc<JobSlot>,
    /// Finished images coming back from the render thread.
    results: mpsc::Receiver<RenderResult>,
    /// The last job sent, so the same work is never asked for twice.
    requested: Option<RenderKey>,
    /// When the last job was sent, for the "Rendering" note.
    requested_at: Instant,
    /// Id for the next job. Results from jobs sent before the shown plot
    /// last changed have ids below accept_from and are dropped.
    next_job_id: u64,
    accept_from: u64,
    /// Set when the newest job for the shown plot could not be parsed.
    last_failed: Option<RenderKey>,
    /// The raster size seen on the last frame, when it last changed, and
    /// the size once it has held still for RESIZE_SETTLE.
    seen_target: (u32, u32),
    target_changed_at: Instant,
    settled_target: Option<(u32, u32)>,
    last_poll: Instant,
    poll_interval: Duration,
    /// Last available size we wrote a resize signal for. Used to
    /// detect when the window has changed.
    last_signaled_size: Option<(u32, u32)>,
    /// Page number last sent to R in view.txt.
    last_signaled_view: Option<u32>,
    /// When did we last detect a size change? Used for debouncing.
    size_change_pending: Option<(Instant, (u32, u32))>,
    /// The plot area's logical-pixel size as of the last frame.
    /// Updated each tick inside the central panel; used by the
    /// export dialog to pre-fill dimensions in inches.
    last_plot_area_logical: (u32, u32),
    /// Whether the export dialog is currently open.
    export_dialog_open: bool,
    /// User-entered export width in inches.
    export_width_in: f32,
    /// User-entered export height in inches.
    export_height_in: f32,
    /// User-entered export DPI. Combined with the inches dimensions,
    /// this determines the actual pixel count of the saved PNG, and
    /// is also written into the PNG file's pHYs metadata chunk so
    /// print software knows the intended resolution.
    export_dpi: u32,
    /// User-selected export format.
    export_format: ExportFormat,
    /// Whether the export should strip the device background rect so
    /// the saved PNG/SVG has a transparent background.
    export_transparent: bool,
    /// Whether zoom mode is toggled on. While on, scroll and pinch
    /// zoom the displayed plot and drag pans it; while off, the plot
    /// is always shown at fit.
    zoom_mode: bool,
    /// Current zoom factor applied to the fit size. 1.0 is fit.
    /// Only meaningful while `zoom_mode` is on.
    zoom: f32,
    /// Pan offset in logical points, applied to the image center.
    /// Only meaningful while `zoom_mode` is on.
    pan: egui::Vec2,
}

/// The files R has written for one page. A page has an .svg, an .rgd, or
/// both when the SVG debug option is on.
#[derive(Clone, PartialEq)]
struct PageFiles {
    svg: Option<PathBuf>,
    rgd: Option<PathBuf>,
}

fn is_rgd(path: &Path) -> bool {
    path.extension().map_or(false, |e| e == "rgd")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExportFormat {
    Png,
    Svg,
}

/// Which file version and size the displayed image came from.
struct Displayed {
    path: PathBuf,
    mtime: SystemTime,
    target: (u32, u32),
    /// False while a quick low-resolution pass is on screen.
    full_quality: bool,
    /// Pixel size of the image. Drawing only uses its aspect ratio.
    rendered_size: egui::Vec2,
    /// How long the render thread took to read and draw it.
    took: Duration,
}

/// One render request. A file version (path plus mtime) at a raster size.
#[derive(Clone, PartialEq)]
struct RenderKey {
    path: PathBuf,
    mtime: SystemTime,
    target: (u32, u32),
}

struct RenderJob {
    id: u64,
    key: RenderKey,
    /// Pixel size for a quick first pass, set only for heavy files.
    preview: Option<(u32, u32)>,
}

struct RenderResult {
    id: u64,
    key: RenderKey,
    /// False for the quick first pass.
    full_quality: bool,
    /// None when the file could not be read or parsed.
    image: Option<(egui::ColorImage, egui::Vec2)>,
    took: Duration,
}

/// Holds at most one waiting job. Sending a new one replaces it.
struct JobSlot {
    job: Mutex<Option<RenderJob>>,
    wake: Condvar,
}

impl ViewerApp {
    fn new(watch_dir: PathBuf, fonts_dir: Option<PathBuf>, ctx: egui::Context) -> Self {
        let jobs = Arc::new(JobSlot {
            job: Mutex::new(None),
            wake: Condvar::new(),
        });
        let (result_tx, result_rx) = mpsc::channel();
        {
            let jobs = Arc::clone(&jobs);
            let fonts_dir = fonts_dir.clone();
            let _ = std::thread::Builder::new()
                .name("rustgd-render".to_string())
                .spawn(move || render_thread(jobs, result_tx, ctx, fonts_dir));
        }
        let now = Instant::now();
        Self {
            watch_dir,
            fonts_dir,
            pages: Vec::new(),
            current_index: 0,
            show_svg: false,
            displayed: None,
            texture: None,
            jobs,
            results: result_rx,
            requested: None,
            requested_at: now,
            next_job_id: 0,
            accept_from: 0,
            last_failed: None,
            seen_target: (0, 0),
            target_changed_at: now,
            settled_target: None,
            last_poll: now,
            poll_interval: Duration::from_millis(10),
            last_signaled_size: None,
            last_signaled_view: None,
            size_change_pending: None,
            last_plot_area_logical: (0, 0),
            export_dialog_open: false,
            export_width_in: 7.0,
            export_height_in: 5.0,
            export_dpi: 300,
            export_format: ExportFormat::Png,
            export_transparent: false,
            zoom_mode: false,
            zoom: 1.0,
            pan: egui::Vec2::ZERO,
        }
    }

    fn poll_directory(&mut self) {
        if !self.watch_dir.exists() {
            std::process::exit(0);
        }

        let entries = match std::fs::read_dir(&self.watch_dir) {
            Ok(e) => e,
            Err(_) => return,
        };

        // R writes each page as plot-NNNN.svg, plot-NNNN.rgd, or both.
        let mut by_page: std::collections::BTreeMap<u32, PageFiles> =
            std::collections::BTreeMap::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(rest) = name.strip_prefix("plot-") else {
                continue;
            };
            let (number, rgd) = if let Some(n) = rest.strip_suffix(".svg") {
                (n, false)
            } else if let Some(n) = rest.strip_suffix(".rgd") {
                (n, true)
            } else {
                continue;
            };
            let Ok(page) = number.parse::<u32>() else {
                continue;
            };
            let files = by_page.entry(page).or_insert(PageFiles {
                svg: None,
                rgd: None,
            });
            if rgd {
                files.rgd = Some(path);
            } else {
                files.svg = Some(path);
            }
        }
        let new_pages: Vec<PageFiles> = by_page.into_values().collect();

        if new_pages != self.pages {
            // Auto-jump to the latest plot whenever the file list
            // changes, which matches the R console and httpgd behavior.
            self.pages = new_pages;
            self.current_index = self.pages.len().saturating_sub(1);
            self.view_changed();
            self.reset_zoom();
        }
    }

    fn prev(&mut self) {
        if self.current_index > 0 {
            self.current_index -= 1;
            self.view_changed();
            self.reset_zoom();
        }
    }

    fn next(&mut self) {
        if self.current_index + 1 < self.pages.len() {
            self.current_index += 1;
            self.view_changed();
            self.reset_zoom();
        }
    }

    fn goto_first(&mut self) {
        if !self.pages.is_empty() && self.current_index != 0 {
            self.current_index = 0;
            self.view_changed();
            self.reset_zoom();
        }
    }

    fn goto_latest(&mut self) {
        let last = self.pages.len().saturating_sub(1);
        if !self.pages.is_empty() && self.current_index != last {
            self.current_index = last;
            self.view_changed();
            self.reset_zoom();
        }
    }

    /// The file the plot on screen is drawn from. A page's .rgd is used
    /// when it has one, unless the SVG toggle asks for its SVG.
    fn current_path(&self) -> Option<&PathBuf> {
        let page = self.pages.get(self.current_index)?;
        if self.show_svg {
            page.svg.as_ref().or(page.rgd.as_ref())
        } else {
            page.rgd.as_ref().or(page.svg.as_ref())
        }
    }

    /// Whether the plot on screen has both an .svg and an .rgd.
    fn current_has_both(&self) -> bool {
        self.pages
            .get(self.current_index)
            .map_or(false, |p| p.svg.is_some() && p.rgd.is_some())
    }

    /// Page number of the plot on screen, read from its file name. R keys
    /// its history by page, which stays right even when numbering has gaps.
    fn current_page_number(&self) -> Option<u32> {
        let stem = self.current_path()?.file_stem()?.to_str()?;
        stem.strip_prefix("plot-")?.parse().ok()
    }

    /// Write a signal file for the R side. Writing to a temporary name and
    /// renaming means R never reads a half-written file.
    fn write_signal(&self, name: &str, contents: &str) {
        let tmp = self.watch_dir.join(format!("{name}.tmp"));
        if std::fs::write(&tmp, contents).is_ok() {
            let _ = std::fs::rename(&tmp, self.watch_dir.join(name));
        }
    }

    /// Return zoom and pan to the fit view. Called whenever the
    /// displayed plot changes so a new plot is never shown through a
    /// stale zoom or pan offset.
    fn reset_zoom(&mut self) {
        self.zoom = 1.0;
        self.pan = egui::Vec2::ZERO;
    }

    /// Write a clear-all marker file. The R-side poller picks it up,
    /// empties plot_history, deletes every page file, and resets the
    /// page counter. The viewer's directory poll then sees the empty
    /// dir and shows "no plots".
    fn signal_clear_all(&self) {
        self.write_signal("clear_all.txt", "");
    }

    /// Write a clear-plot marker file holding the page number of the
    /// plot on screen. The R-side poller removes that plot from its
    /// history, deletes its file, and moves higher pages down by one.
    fn signal_clear_active(&self) {
        if let Some(page) = self.current_page_number() {
            self.write_signal("clear_plot.txt", &page.to_string());
        }
    }

    /// Pre-fill the export dialog from the current window logical
    /// size and the currently selected DPI, then open it. The inches
    /// are computed so that `inches × DPI` matches the on-screen
    /// pixel count — i.e. a fresh "Use window size" snapshot.
    fn open_export_dialog(&mut self) {
        let (lw, lh) = self.last_plot_area_logical;
        if lw > 0 && lh > 0 && self.export_dpi > 0 {
            self.export_width_in = lw as f32 / self.export_dpi as f32;
            self.export_height_in = lh as f32 / self.export_dpi as f32;
        }
        self.export_dialog_open = true;
    }

    /// Compute the actual pixel dimensions of the exported PNG given
    /// the user's inches × DPI inputs. The SVG's intrinsic aspect
    /// ratio is preserved: if the user's bounding box doesn't match
    /// the SVG aspect, the output fits inside the box without
    /// distortion (one dimension may end up smaller than requested).
    fn computed_output_size(&self) -> (u32, u32) {
        let target_w_px = (self.export_width_in * self.export_dpi as f32).max(1.0);
        let target_h_px = (self.export_height_in * self.export_dpi as f32).max(1.0);

        if let Some(d) = &self.displayed {
            if d.rendered_size.x > 0.0 && d.rendered_size.y > 0.0 {
                let svg_aspect = d.rendered_size.x / d.rendered_size.y;
                let target_aspect = target_w_px / target_h_px;
                return if target_aspect > svg_aspect {
                    // Box is wider than the plot; height is the limit.
                    let h = target_h_px;
                    let w = h * svg_aspect;
                    (w as u32, h as u32)
                } else {
                    // Box is taller than (or matches) the plot; width limits.
                    let w = target_w_px;
                    let h = w / svg_aspect;
                    (w as u32, h as u32)
                };
            }
        }
        (target_w_px as u32, target_h_px as u32)
    }

    /// Render the floating Export dialog if it's open. Handles the
    /// Save flow (opens a native file picker via rfd, runs the actual
    /// export), the Cancel flow, and Escape-to-close.
    fn show_export_dialog(&mut self, ctx: &egui::Context) {
        if !self.export_dialog_open {
            return;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.export_dialog_open = false;
            return;
        }

        let mut save_clicked = false;
        let mut cancel_clicked = false;
        let logical_window = self.last_plot_area_logical;
        let preview = self.computed_output_size();

        egui::Window::new("Export plot")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label("Dimensions (inches):");
                ui.horizontal(|ui| {
                    ui.label("Width:");
                    ui.add(
                        egui::DragValue::new(&mut self.export_width_in)
                            .range(0.1f32..=100.0f32)
                            .speed(0.1)
                            .suffix(" in"),
                    );
                    ui.add_space(8.0);
                    ui.label("Height:");
                    ui.add(
                        egui::DragValue::new(&mut self.export_height_in)
                            .range(0.1f32..=100.0f32)
                            .speed(0.1)
                            .suffix(" in"),
                    );
                });

                if logical_window.0 > 0 && logical_window.1 > 0 {
                    if ui.button("Use window size").clicked() {
                        if self.export_dpi > 0 {
                            self.export_width_in = logical_window.0 as f32 / self.export_dpi as f32;
                            self.export_height_in =
                                logical_window.1 as f32 / self.export_dpi as f32;
                        }
                    }
                }

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("DPI:");
                    ui.add(
                        egui::DragValue::new(&mut self.export_dpi)
                            .range(1u32..=2400u32)
                            .speed(1.0),
                    );
                });

                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(format!("Output: {} × {} px", preview.0, preview.1)).weak(),
                );

                ui.separator();
                ui.label("Format:");
                ui.horizontal(|ui| {
                    ui.radio_value(&mut self.export_format, ExportFormat::Png, "PNG");
                    ui.radio_value(&mut self.export_format, ExportFormat::Svg, "SVG");
                });

                ui.add_space(4.0);
                ui.checkbox(&mut self.export_transparent, "Transparent background");

                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Save...").clicked() {
                        save_clicked = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel_clicked = true;
                    }
                });
            });

        if cancel_clicked {
            self.export_dialog_open = false;
        }
        if save_clicked {
            self.run_export();
            self.export_dialog_open = false;
        }
    }

    /// Open the native save dialog, then perform the export. SVG is
    /// a file copy; PNG goes through resvg at the chosen dimensions.
    /// Errors are silently swallowed for now — there's no UI surface
    /// for them. If the user cancels the file picker, nothing happens.
    fn run_export(&self) {
        let Some(source) = self.current_path().cloned() else {
            return;
        };

        let (filter_name, ext) = match self.export_format {
            ExportFormat::Png => ("PNG image", "png"),
            ExportFormat::Svg => ("SVG vector", "svg"),
        };

        let Some(mut dest) = rfd::FileDialog::new()
            .add_filter(filter_name, &[ext])
            .set_file_name(&format!("plot.{ext}"))
            .save_file()
        else {
            return; // user cancelled
        };

        // Ensure the chosen path has the right extension; users
        // sometimes type a name without one.
        if dest.extension().map_or(true, |e| e != ext) {
            dest.set_extension(ext);
        }

        match self.export_format {
            ExportFormat::Svg if is_rgd(&source) => {
                // A binary page has no SVG file, so build it with the same
                // writer the device uses for its SVG pages.
                let svg = std::fs::read(&source)
                    .ok()
                    .and_then(|bytes| display_list::file_to_svg(&bytes).ok());
                if let Some(svg) = svg {
                    let svg = if self.export_transparent {
                        strip_background_rect(&svg)
                    } else {
                        svg
                    };
                    let _ = std::fs::write(&dest, svg);
                }
            }
            ExportFormat::Svg => {
                let source_svg = &source;
                if self.export_transparent {
                    // Strip the device background rect and write the
                    // modified SVG. Fall back to a plain copy if the
                    // file cannot be read as text for any reason.
                    match std::fs::read_to_string(source_svg) {
                        Ok(s) => {
                            let _ = std::fs::write(&dest, strip_background_rect(&s));
                        }
                        Err(_) => {
                            let _ = std::fs::copy(source_svg, &dest);
                        }
                    }
                } else {
                    let _ = std::fs::copy(source_svg, &dest);
                }
            }
            ExportFormat::Png => {
                let _ = self.export_as_png(&source, &dest);
            }
        }
    }

    /// Rasterize the given page at `export_width_in × export_height_in`
    /// inches at `export_dpi` and write a PNG. Aspect ratio is preserved
    /// (the rendered image fits within the chosen bounding box). The
    /// PNG file is tagged with the chosen DPI in its pHYs metadata
    /// chunk so print software handles it at the intended resolution.
    fn export_as_png(&self, source: &Path, dest: &Path) -> Result<(), String> {
        if is_rgd(source) {
            return self.export_rgd_as_png(source, dest);
        }
        let svg_string = std::fs::read_to_string(source).map_err(|e| e.to_string())?;
        // When exporting transparent, drop the device background rect
        // so the pixmap (which starts fully transparent) shows through.
        let svg_string = if self.export_transparent {
            strip_background_rect(&svg_string)
        } else {
            svg_string
        };

        let mut opt = usvg::Options::default();
        if let Some(dir) = &self.fonts_dir {
            opt.fontdb_mut().load_fonts_dir(dir);
        }
        opt.fontdb_mut().load_system_fonts();
        opt.font_family = "Liberation Sans".to_string();

        let tree = usvg::Tree::from_data(svg_string.as_bytes(), &opt)
            .map_err(|e| format!("parse svg: {e}"))?;

        let svg_size = tree.size();
        let svg_w = svg_size.width();
        let svg_h = svg_size.height();
        if svg_w <= 0.0 || svg_h <= 0.0 {
            return Err("svg has zero size".to_string());
        }

        // Inches × DPI → target pixel bounding box.
        let target_w = (self.export_width_in * self.export_dpi as f32).max(1.0);
        let target_h = (self.export_height_in * self.export_dpi as f32).max(1.0);

        let scale_x = target_w / svg_w;
        let scale_y = target_h / svg_h;
        let scale = scale_x.min(scale_y);

        let pixmap_w = (svg_w * scale).max(1.0) as u32;
        let pixmap_h = (svg_h * scale).max(1.0) as u32;

        let mut pixmap = Pixmap::new(pixmap_w, pixmap_h)
            .ok_or_else(|| "failed to allocate pixmap".to_string())?;
        let transform = Transform::from_scale(scale, scale);
        resvg::render(&tree, transform, &mut pixmap.as_mut());

        write_png_with_dpi(&pixmap, dest, self.export_dpi)
    }

    /// The PNG export for a .rgd page, drawn by the same renderer as the
    /// window and sized the same way as the SVG export above.
    fn export_rgd_as_png(&self, source: &Path, dest: &Path) -> Result<(), String> {
        let bytes = std::fs::read(source).map_err(|e| e.to_string())?;
        let page = rgd_render::Page::new(bytes).map_err(|e| format!("read page: {e:?}"))?;
        if page.width <= 0.0 || page.height <= 0.0 {
            return Err("page has zero size".to_string());
        }

        let mut opt = usvg::Options::default();
        if let Some(dir) = &self.fonts_dir {
            opt.fontdb_mut().load_fonts_dir(dir);
        }
        opt.fontdb_mut().load_system_fonts();
        opt.font_family = "Liberation Sans".to_string();

        let target_w = (self.export_width_in * self.export_dpi as f32).max(1.0);
        let target_h = (self.export_height_in * self.export_dpi as f32).max(1.0);
        let scale = (target_w / page.width).min(target_h / page.height);
        let pixmap_w = (page.width * scale).max(1.0) as u32;
        let pixmap_h = (page.height * scale).max(1.0) as u32;

        let mut pixmap = Pixmap::new(pixmap_w, pixmap_h)
            .ok_or_else(|| "failed to allocate pixmap".to_string())?;
        let mut texts = rgd_render::TextCache::new();
        rgd_render::draw(
            &page,
            &mut pixmap,
            scale,
            self.export_transparent,
            &opt,
            &mut texts,
        );
        write_png_with_dpi(&pixmap, dest, self.export_dpi)
    }

    /// Write the current plot-area dimensions to resize.txt so the
    /// R side can replay the plots at the new size. The write waits until
    /// the size has held for RESIZE_SETTLE, so a drag does not make R
    /// replay at every size it passes through.
    fn maybe_signal_resize(&mut self, current_size: (u32, u32)) {
        const DEBOUNCE: Duration = RESIZE_SETTLE;

        if Some(current_size) == self.last_signaled_size {
            return;
        }

        let now = Instant::now();
        match self.size_change_pending {
            Some((_, pending_size)) if pending_size == current_size => {
                // Same pending size; check if it's been stable long enough.
            }
            _ => {
                self.size_change_pending = Some((now, current_size));
                return;
            }
        }

        let (changed_at, pending_size) = self.size_change_pending.unwrap();
        if now.duration_since(changed_at) >= DEBOUNCE {
            // The page number on the end tells R which plot to redraw first.
            let contents = match self.current_page_number() {
                Some(page) => format!("{},{},{}", pending_size.0, pending_size.1, page),
                None => format!("{},{}", pending_size.0, pending_size.1),
            };
            self.write_signal("resize.txt", &contents);
            self.last_signaled_size = Some(pending_size);
            self.size_change_pending = None;
        }
    }

    /// Tell R which plot is on screen whenever that changes, by paging or
    /// by jumping to a new plot. After a resize, R redraws that plot first.
    fn maybe_signal_view(&mut self) {
        let page = self.current_page_number();
        if page.is_some() && page != self.last_signaled_view {
            if let Some(p) = page {
                self.write_signal("view.txt", &p.to_string());
            }
            self.last_signaled_view = page;
        }
    }

    /// Called when the shown plot changes. Results still in flight for
    /// the previous plot are dropped, and the old image stays up until the
    /// new one arrives, so paging never flashes an empty window.
    fn view_changed(&mut self) {
        self.accept_from = self.next_job_id;
        self.requested = None;
        self.last_failed = None;
    }

    /// Hand a job to the render thread, replacing any job it has not
    /// started yet. Only the newest request matters.
    fn submit_job(&mut self, key: RenderKey, preview: Option<(u32, u32)>) {
        let id = self.next_job_id;
        self.next_job_id += 1;
        if let Ok(mut slot) = self.jobs.job.lock() {
            *slot = Some(RenderJob {
                id,
                key: key.clone(),
                preview,
            });
        }
        self.jobs.wake.notify_one();
        self.requested = Some(key);
        self.requested_at = Instant::now();
    }

    /// Take finished images from the render thread and show the newest one
    /// that still belongs to the plot on screen. The texture is updated in
    /// place, so only one GPU texture is ever live.
    fn receive_results(&mut self, ctx: &egui::Context) {
        while let Ok(result) = self.results.try_recv() {
            if result.id < self.accept_from || self.current_path() != Some(&result.key.path) {
                continue;
            }
            let Some((image, rendered_size)) = result.image else {
                self.last_failed = Some(result.key);
                continue;
            };
            // A quick pass never replaces a sharp image of the same version.
            let keep_sharp = !result.full_quality
                && self.displayed.as_ref().map_or(false, |d| {
                    d.full_quality && d.path == result.key.path && d.mtime == result.key.mtime
                });
            if keep_sharp {
                continue;
            }
            match self.texture.as_mut() {
                Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
                None => {
                    self.texture = Some(ctx.load_texture(
                        "rustgd-svg",
                        image,
                        egui::TextureOptions::LINEAR,
                    ));
                }
            }
            self.displayed = Some(Displayed {
                path: result.key.path,
                mtime: result.key.mtime,
                target: result.key.target,
                full_quality: result.full_quality,
                rendered_size,
                took: result.took,
            });
            self.last_failed = None;
        }
    }
}

/// A plot file read into memory, ready to rasterize at any size.
enum Parsed {
    Svg(usvg::Tree),
    Rgd(rgd_render::Page),
}

/// The render thread. Parses and rasterizes plots away from the window
/// thread. It sleeps until a job arrives, always takes the newest one, and
/// sends finished images back over the results channel.
fn render_thread(
    jobs: Arc<JobSlot>,
    results: mpsc::Sender<RenderResult>,
    ctx: egui::Context,
    fonts_dir: Option<PathBuf>,
) {
    // Load fonts once, here, instead of on every render. Scanning the
    // system font folders is slow on macOS.
    let mut opt = usvg::Options::default();
    if let Some(dir) = &fonts_dir {
        opt.fontdb_mut().load_fonts_dir(dir);
    }
    opt.fontdb_mut().load_system_fonts();
    opt.font_family = "Liberation Sans".to_string();

    // Label layouts for .rgd pages, kept across pages and redraws.
    let mut texts = rgd_render::TextCache::new();

    // The last parsed plot. A pure resize of the same file version reuses
    // it and skips the parse.
    let mut parsed: Option<(PathBuf, SystemTime, Parsed)> = None;

    loop {
        let job = {
            let Ok(mut slot) = jobs.job.lock() else {
                return;
            };
            loop {
                if let Some(job) = slot.take() {
                    break job;
                }
                slot = match jobs.wake.wait(slot) {
                    Ok(guard) => guard,
                    Err(_) => return,
                };
            }
        };

        let started = Instant::now();
        let reuse = matches!(
            &parsed,
            Some((path, mtime, _)) if *path == job.key.path && *mtime == job.key.mtime
        );
        if !reuse {
            // Free the old tree before parsing the next one.
            parsed = None;
            let loaded = std::fs::read(&job.key.path).ok().and_then(|bytes| {
                if is_rgd(&job.key.path) {
                    rgd_render::Page::new(bytes).ok().map(Parsed::Rgd)
                } else {
                    usvg::Tree::from_data(&bytes, &opt).ok().map(Parsed::Svg)
                }
            });
            match loaded {
                Some(page) => parsed = Some((job.key.path.clone(), job.key.mtime, page)),
                None => {
                    // Missing or half written. When R finishes writing, the
                    // mtime changes and the window asks again.
                    let _ = results.send(RenderResult {
                        id: job.id,
                        key: job.key,
                        full_quality: true,
                        image: None,
                        took: started.elapsed(),
                    });
                    ctx.request_repaint();
                    continue;
                }
            }
        }
        let Some((_, _, page)) = parsed.as_ref() else {
            continue;
        };

        if let Some((w, h)) = job.preview {
            if let Some(image) = rasterize(page, w, h, &opt, &mut texts) {
                let _ = results.send(RenderResult {
                    id: job.id,
                    key: job.key.clone(),
                    full_quality: false,
                    image: Some(image),
                    took: started.elapsed(),
                });
                ctx.request_repaint();
            }
            // If a newer job arrived meanwhile, the sharp pass for this one
            // would be thrown away, so skip straight to the newer job.
            if jobs.job.lock().map_or(false, |slot| slot.is_some()) {
                continue;
            }
        }

        let image = rasterize(page, job.key.target.0, job.key.target.1, &opt, &mut texts);
        let _ = results.send(RenderResult {
            id: job.id,
            key: job.key,
            full_quality: true,
            image,
            took: started.elapsed(),
        });
        ctx.request_repaint();
    }
}

/// Rasterize a plot to fit inside target_w by target_h pixels and return
/// it as an egui image plus its pixel size. The tiny-skia pixmap is freed
/// before returning, so it and the image handed to egui are never both
/// resident for long.
fn rasterize(
    page: &Parsed,
    target_w: u32,
    target_h: u32,
    opt: &usvg::Options,
    texts: &mut rgd_render::TextCache,
) -> Option<(egui::ColorImage, egui::Vec2)> {
    let pixmap = match page {
        Parsed::Svg(tree) => rasterize_svg(tree, target_w, target_h)?,
        Parsed::Rgd(page) => rgd_render::render_fit(page, target_w, target_h, opt, texts)?,
    };
    let rendered = egui::Vec2::new(pixmap.width() as f32, pixmap.height() as f32);
    let color_image = {
        let pixels: Vec<egui::Color32> = pixmap
            .pixels()
            .iter()
            .map(|p| {
                egui::Color32::from_rgba_premultiplied(p.red(), p.green(), p.blue(), p.alpha())
            })
            .collect();
        egui::ColorImage {
            size: [pixmap.width() as usize, pixmap.height() as usize],
            pixels,
        }
    };
    drop(pixmap);
    Some((color_image, rendered))
}

/// Draw a parsed SVG to fit inside target_w by target_h pixels.
fn rasterize_svg(tree: &usvg::Tree, target_w: u32, target_h: u32) -> Option<Pixmap> {
    let svg_size = tree.size();
    let svg_w = svg_size.width();
    let svg_h = svg_size.height();
    if svg_w <= 0.0 || svg_h <= 0.0 {
        return None;
    }

    let scale_x = target_w as f32 / svg_w;
    let scale_y = target_h as f32 / svg_h;
    let scale = scale_x.min(scale_y);

    let pixmap_w = (svg_w * scale).max(1.0) as u32;
    let pixmap_h = (svg_h * scale).max(1.0) as u32;

    let mut pixmap = Pixmap::new(pixmap_w, pixmap_h)?;
    let transform = Transform::from_scale(scale, scale);
    resvg::render(tree, transform, &mut pixmap.as_mut());
    Some(pixmap)
}

/// Remove the device background rect (the one rustgd tags with
/// `id="rustgd-bg"`) from an SVG string. Returns the SVG unchanged if
/// the rect is not present. The rect is a self-closing element, so we
/// find its start and the next `/>` and splice it out. Only the device
/// background is removed; any background the plot itself draws stays.
fn strip_background_rect(svg: &str) -> String {
    const MARKER: &str = "<rect id=\"rustgd-bg\"";
    if let Some(start) = svg.find(MARKER) {
        if let Some(rel_end) = svg[start..].find("/>") {
            let end = start + rel_end + 2; // include the closing "/>"
            let mut out = String::with_capacity(svg.len());
            out.push_str(&svg[..start]);
            out.push_str(&svg[end..]);
            return out;
        }
    }
    svg.to_string()
}

/// Write a `Pixmap` as PNG with a `pHYs` chunk specifying the image's
/// physical resolution in DPI. Tiny-skia's `Pixmap` stores RGBA in
/// premultiplied form; we demultiply before encoding so colors render
/// correctly outside of compositing pipelines.
fn write_png_with_dpi(pixmap: &Pixmap, dest: &Path, dpi: u32) -> Result<(), String> {
    // Demultiply RGBA so consumers see straight alpha. For typical
    // plots (opaque white background) most pixels have alpha 255 and
    // this is a no-op, but it matters at anti-aliased edges and any
    // transparent regions.
    let mut rgba = Vec::with_capacity(pixmap.data().len());
    for pixel in pixmap.pixels() {
        let c = pixel.demultiply();
        rgba.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }

    let file = std::fs::File::create(dest).map_err(|e| e.to_string())?;
    let writer = std::io::BufWriter::new(file);

    let mut encoder = png::Encoder::new(writer, pixmap.width(), pixmap.height());
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);

    // pHYs chunk: pixels per unit, with unit = meter.
    // 1 inch = 0.0254 m, so PPM = DPI / 0.0254.
    let ppm = (dpi as f64 / 0.0254).round() as u32;
    encoder.set_pixel_dims(Some(png::PixelDimensions {
        xppu: ppm,
        yppu: ppm,
        unit: png::Unit::Meter,
    }));

    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(&rgba).map_err(|e| e.to_string())?;
    Ok(())
}

impl eframe::App for ViewerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.last_poll.elapsed() >= self.poll_interval {
            self.poll_directory();
            self.last_poll = Instant::now();
        }
        ctx.request_repaint_after(self.poll_interval);

        // Show any image the render thread has finished.
        self.receive_results(ctx);
        self.maybe_signal_view();

        // Keyboard navigation. Sample the input flags before mutating
        // self so we don't hold a borrow on ctx.input across mutations.
        let go_prev = ctx.input(|i| i.key_pressed(egui::Key::ArrowLeft));
        let go_next = ctx.input(|i| i.key_pressed(egui::Key::ArrowRight));
        let go_first = ctx.input(|i| i.key_pressed(egui::Key::Home));
        let go_last = ctx.input(|i| i.key_pressed(egui::Key::End));
        let flip_renderer =
            !self.export_dialog_open && ctx.input(|i| i.key_pressed(egui::Key::S));
        if go_prev {
            self.prev();
        }
        if go_next {
            self.next();
        }
        if go_first {
            self.goto_first();
        }
        if go_last {
            self.goto_latest();
        }
        if flip_renderer && self.current_has_both() {
            self.show_svg = !self.show_svg;
        }

        // Top toolbar: prev / next / counter on the left, action
        // buttons (Export, Clear plot, Clear all) on the right.
        egui::TopBottomPanel::top("rustgd_toolbar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                let at_first = self.current_index == 0 || self.pages.is_empty();
                let at_last = self.current_index + 1 >= self.pages.len();
                let has_plots = !self.pages.is_empty();

                // Filled-triangle nav arrows at 16pt so the icon
                // reads at a glance, matching RStudio's plot pane.
                if ui
                    .add_enabled(
                        !at_first,
                        egui::Button::new(egui::RichText::new("◀ Prev").size(16.0)),
                    )
                    .clicked()
                {
                    self.prev();
                }
                if ui
                    .add_enabled(
                        !at_last,
                        egui::Button::new(egui::RichText::new("Next ▶").size(16.0)),
                    )
                    .clicked()
                {
                    self.next();
                }
                ui.separator();

                let counter_text = if self.pages.is_empty() {
                    "no plots".to_string()
                } else {
                    format!("Plot {} / {}", self.current_index + 1, self.pages.len())
                };
                ui.label(counter_text);
                ui.separator();

                // Zoom toggle. While on, scroll/pinch zoom the plot and
                // drag pans it; the label shows the current factor once
                // zoomed past fit. Toggling off (handled in the central
                // panel) snaps back to fit.
                let zoom_label = if self.zoom_mode && self.zoom > 1.001 {
                    format!("Zoom {}%", (self.zoom * 100.0).round() as i32)
                } else {
                    "Zoom".to_string()
                };
                ui.add_enabled_ui(has_plots, |ui| {
                    ui.toggle_value(&mut self.zoom_mode, zoom_label);
                });
                // With options(rustgd.debug_svg = TRUE) R writes each page
                // both ways. The SVG toggle, or the S key, flips between
                // drawing the page from its .rgd and from its SVG, and the
                // label says which one is on screen and how long it took.
                if self.current_has_both() {
                    ui.separator();
                    ui.toggle_value(&mut self.show_svg, "SVG");
                    if let Some(d) = &self.displayed {
                        let source = if is_rgd(&d.path) { ".rgd" } else { "SVG" };
                        ui.label(format!("{source} {} ms", d.took.as_millis()));
                    }
                }

                // Action buttons pinned to the right edge. Right-to-
                // left layout means we add in reverse visual order:
                // Clear all first (rightmost), then Clear plot, then
                // Export... (leftmost of the right-side group).
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(8.0);

                    if ui
                        .add_enabled(has_plots, egui::Button::new("Clear all"))
                        .clicked()
                    {
                        self.signal_clear_all();
                    }
                    if ui
                        .add_enabled(has_plots, egui::Button::new("Clear plot"))
                        .clicked()
                    {
                        self.signal_clear_active();
                    }
                    if ui
                        .add_enabled(has_plots, egui::Button::new("Export..."))
                        .clicked()
                    {
                        self.open_export_dialog();
                    }
                });
            });
            ui.add_space(4.0);
        });

        // Export dialog (modal-ish floating window). Closes via the
        // Save/Cancel buttons, or by pressing Escape.
        self.show_export_dialog(ctx);

        // Main plot area.
        let mut show_busy_note = false;
        egui::CentralPanel::default().show(ctx, |ui| {
            // Track logical (not physical) size for the resize signal.
            // We send "device units" which are pixels in our viewBox.
            // Use logical pixels so R works in the same coordinate
            // system regardless of HiDPI scaling. This is the plot
            // area size (window minus toolbar), which is what we want
            // R to render at. `area` is the same region as a Rect, used
            // for zoom/pan drawing and input below.
            let area = ui.available_rect_before_wrap();
            let available = area.size();
            let signal_w = available.x.max(1.0) as u32;
            let signal_h = available.y.max(1.0) as u32;
            // Stash for the export dialog's "Use window size" pre-fill.
            self.last_plot_area_logical = (signal_w, signal_h);
            self.maybe_signal_resize((signal_w, signal_h));

            let Some(current_path) = self.current_path().cloned() else {
                // No plot to show. Release the texture so an empty
                // gallery (e.g. after Clear all) does not retain the
                // last rendered buffer.
                self.texture = None;
                self.displayed = None;
                self.requested = None;
                self.last_failed = None;
                ui.centered_and_justified(|ui| {
                    ui.label("no plot yet");
                });
                return;
            };

            let pixels_per_point = ctx.pixels_per_point();
            let target = (
                (available.x * pixels_per_point * SUPERSAMPLE).max(1.0) as u32,
                (available.y * pixels_per_point * SUPERSAMPLE).max(1.0) as u32,
            );

            // Track how long the plot area has kept its size. While it is
            // changing, the last image is stretched to fit on the GPU and
            // nothing is redrawn. A redraw is asked for once it holds still.
            let now = Instant::now();
            if target != self.seen_target {
                self.seen_target = target;
                self.target_changed_at = now;
            }
            if self.settled_target.is_none()
                || now.duration_since(self.target_changed_at) >= RESIZE_SETTLE
            {
                self.settled_target = Some(target);
            }
            let settled = self.settled_target.unwrap_or(target);

            // The file's mtime shows when R has rewritten it in place (for
            // example a resize replay), and its length says whether it is
            // heavy enough for a quick first pass.
            let meta = std::fs::metadata(&current_path).ok();
            let current_mtime = meta.as_ref().and_then(|m| m.modified().ok());
            let file_len = meta.as_ref().map_or(0, |m| m.len());

            if let Some(mtime) = current_mtime {
                let key = RenderKey {
                    path: current_path.clone(),
                    mtime,
                    target: settled,
                };
                if self.requested.as_ref() != Some(&key) {
                    let content_changed = self
                        .requested
                        .as_ref()
                        .map_or(true, |r| r.path != key.path || r.mtime != key.mtime);
                    let heavy_from = if is_rgd(&current_path) {
                        PREVIEW_MIN_BYTES_RGD
                    } else {
                        PREVIEW_MIN_BYTES
                    };
                    let heavy = file_len >= heavy_from;
                    let wait_for_replay = !content_changed
                        && heavy
                        && now.duration_since(self.target_changed_at) < HEAVY_RESIZE_SETTLE;
                    if !wait_for_replay {
                        // A new or rewritten heavy file gets a quick pass at
                        // one pixel per point first. A pure resize does not,
                        // since the stretched sharp image already looks better.
                        let preview = if content_changed && heavy {
                            let scale = pixels_per_point * SUPERSAMPLE;
                            Some((
                                (settled.0 as f32 / scale).max(1.0) as u32,
                                (settled.1 as f32 / scale).max(1.0) as u32,
                            ))
                        } else {
                            None
                        };
                        self.submit_job(key, preview);
                    }
                }
            }

            let up_to_date = match (&self.displayed, &self.requested) {
                (Some(d), Some(r)) => {
                    d.full_quality
                        && d.path == r.path
                        && d.mtime == r.mtime
                        && d.target == r.target
                }
                _ => false,
            };
            let failed = self.requested.is_some() && self.last_failed == self.requested;
            let waited = self.requested_at.elapsed() >= BUSY_NOTE_DELAY;

            // Pull the values we need for drawing out of the borrowed
            // image state and texture as Copy values, so the zoom/pan
            // handling below can take a mutable borrow of self.
            let draw_data = self
                .displayed
                .as_ref()
                .zip(self.texture.as_ref())
                .map(|(d, tex)| (d.rendered_size, tex.id()));

            if let Some((rendered_size, tex_id)) = draw_data {
                show_busy_note = !up_to_date && !failed && waited;

                // Fit the image inside the plot area at its own aspect
                // ratio. egui's Linear texture filter does the scaling on
                // the GPU, which is also what stretches a stale image
                // smoothly while a resize settles.
                let aspect = rendered_size.x.max(1.0) / rendered_size.y.max(1.0);
                let fit_size = if area.width() > area.height() * aspect {
                    egui::vec2(area.height() * aspect, area.height())
                } else {
                    egui::vec2(area.width(), area.width() / aspect)
                };

                // Off zoom mode means always fit: force zoom/pan back
                // so toggling off snaps to the clean view.
                if !self.zoom_mode {
                    self.zoom = 1.0;
                    self.pan = egui::Vec2::ZERO;
                }

                // An interactive region over the whole plot area. Only
                // senses clicks/drags while zooming, so it stays inert
                // otherwise.
                let sense = if self.zoom_mode {
                    egui::Sense::click_and_drag()
                } else {
                    egui::Sense::hover()
                };
                let response = ui.allocate_rect(area, sense);

                if self.zoom_mode {
                    // Combine pinch (zoom_delta) and scroll into one
                    // multiplicative factor, then zoom toward the
                    // cursor so the point under it stays fixed. Scroll
                    // and pinch are global input, so only act on them
                    // when the pointer is actually over the plot area;
                    // otherwise scrolling the toolbar would zoom.
                    if response.hovered() {
                        let pinch = ui.input(|i| i.zoom_delta());
                        let scroll_y = ui.input(|i| i.smooth_scroll_delta.y);
                        let scroll_factor = (scroll_y * 0.002).exp();
                        let old_zoom = self.zoom;
                        let new_zoom = (old_zoom * pinch * scroll_factor).clamp(1.0, 8.0);
                        if (new_zoom - old_zoom).abs() > f32::EPSILON {
                            let pointer = response.hover_pos().unwrap_or(area.center());
                            let q = pointer - area.center();
                            self.pan = q - (q - self.pan) * (new_zoom / old_zoom);
                            self.zoom = new_zoom;
                        }
                    }

                    if response.dragged() {
                        self.pan += response.drag_delta();
                    }
                    if response.double_clicked() {
                        self.zoom = 1.0;
                        self.pan = egui::Vec2::ZERO;
                    }
                }

                let disp_size = fit_size * self.zoom;

                // Clamp pan so the zoomed image cannot be dragged off
                // far enough to reveal empty gaps. In an axis where the
                // image is no larger than the area (e.g. the letterbox
                // axis at fit), the allowed pan is zero, keeping it
                // centered.
                let max_pan_x = ((disp_size.x - area.width()) * 0.5).max(0.0);
                let max_pan_y = ((disp_size.y - area.height()) * 0.5).max(0.0);
                self.pan.x = self.pan.x.clamp(-max_pan_x, max_pan_x);
                self.pan.y = self.pan.y.clamp(-max_pan_y, max_pan_y);

                let center = area.center() + self.pan;
                let image_rect = egui::Rect::from_center_size(center, disp_size);
                let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                // painter_at clips to the plot area, so a zoomed-in
                // image is cropped to the viewport rather than drawn
                // over the toolbar or neighbouring UI.
                let painter = ui.painter_at(area);
                painter.image(tex_id, image_rect, uv, egui::Color32::WHITE);

                if self.zoom_mode {
                    ctx.set_cursor_icon(if response.dragged() {
                        egui::CursorIcon::Grabbing
                    } else {
                        egui::CursorIcon::Grab
                    });
                }
            } else if waited {
                let note = if failed {
                    "failed to render plot"
                } else {
                    "rendering\u{2026}"
                };
                ui.centered_and_justified(|ui| {
                    ui.label(note);
                });
            }
        });

        // A small note in the corner while a slow render is in progress.
        if show_busy_note {
            egui::Area::new(egui::Id::new("rustgd_busy_note"))
                .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-12.0, -12.0))
                .order(egui::Order::Foreground)
                .interactable(false)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.label("Rendering\u{2026}");
                    });
                });
        }
    }
}
