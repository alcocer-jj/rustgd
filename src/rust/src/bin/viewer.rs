//! rustgd-viewer: the standalone window the device's R-side launches.
//! Watches a directory passed as argv[1], displays SVG plots.
//! Loads a bundled fonts directory from argv[2] — Liberation Sans is
//! registered as egui's primary font so the toolbar's arrows render
//! correctly and the UI typography matches the SVG plot typography.
//!
//! Detects window resize and writes resize.txt into the watch
//! directory so the R-side task callback can apply the new
//! dimensions and trigger a display list replay of every plot in
//! the session history.
//!
//! On exit (user closes the window), writes a `viewer_closed`
//! signal file into the watch directory so the R-side poller
//! can call `dev.off()` and clean up the session.
//!
//! Gallery view: all SVG files in the watch directory are sorted
//! ascending and navigable with prev/next buttons in the top
//! toolbar, or with the Left/Right/Home/End keys. When a new plot
//! arrives, the view auto-jumps to it. Cache invalidation tracks
//! file modification time so in-place overwrites (resize replays)
//! are picked up reliably.
//!
//! The UI follows the OS appearance setting (Light / Dark on macOS),
//! including runtime changes. The plot itself keeps R's native
//! background — typically white — regardless of UI theme.
#![windows_subsystem = "windows"]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eframe::egui;
use tiny_skia::{Pixmap, Transform};

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

    // Keep copies of the watch and fonts directories for the closure
    // and post-run signal.
    let watch_dir_for_close = watch_dir.clone();
    let fonts_dir_for_egui = fonts_dir.clone();

    let app = ViewerApp::new(watch_dir, fonts_dir);
    let _ = eframe::run_native(
        "rustgd",
        options,
        Box::new(move |cc| {
            setup_egui_fonts(&cc.egui_ctx, fonts_dir_for_egui.as_deref());
            // Follow the OS appearance setting (Light / Dark). egui
            // re-evaluates the system theme on relevant events, so a
            // mid-session toggle of macOS Appearance updates the UI
            // automatically. The default in egui is Dark, which is
            // why the viewer started out dark before this opt-in.
            cc.egui_ctx.options_mut(|opts| {
                opts.theme_preference = egui::ThemePreference::System;
            });
            Ok(Box::new(app))
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
    /// All SVG files in the watch directory, sorted ascending by path.
    /// The last entry is the most recent plot.
    svg_paths: Vec<PathBuf>,
    /// Index into `svg_paths` of the plot currently displayed. Valid
    /// only when `svg_paths` is non-empty.
    current_index: usize,
    cached: Option<CachedRender>,
    /// The single GPU texture the plot is drawn into. Reused across
    /// renders via `TextureHandle::set` rather than allocating a new
    /// texture each frame, so a resize never holds the old and new
    /// textures alive at the same time. None until the first render,
    /// and released when there is no plot to show.
    texture: Option<egui::TextureHandle>,
    last_poll: Instant,
    poll_interval: Duration,
    /// Last available size we wrote a resize signal for. Used to
    /// detect when the window has changed.
    last_signaled_size: Option<(u32, u32)>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExportFormat {
    Png,
    Svg,
}

struct CachedRender {
    svg_path: PathBuf,
    target_size: (u32, u32),
    rendered_size: egui::Vec2,
    /// Modification time of the SVG file at the moment it was
    /// rasterized. Used to detect that R has overwritten the file
    /// in place (e.g. after a resize replay) so we re-read it.
    mtime: std::time::SystemTime,
}

impl ViewerApp {
    fn new(watch_dir: PathBuf, fonts_dir: Option<PathBuf>) -> Self {
        Self {
            watch_dir,
            fonts_dir,
            svg_paths: Vec::new(),
            current_index: 0,
            cached: None,
            texture: None,
            last_poll: Instant::now(),
            poll_interval: Duration::from_millis(10),
            last_signaled_size: None,
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

        let mut new_paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "svg").unwrap_or(false))
            .collect();
        new_paths.sort();

        if new_paths != self.svg_paths {
            // Auto-jump to the latest plot whenever the file list
            // changes — matches the R console and httpgd behavior.
            self.svg_paths = new_paths;
            self.current_index = self.svg_paths.len().saturating_sub(1);
            self.cached = None;
            self.reset_zoom();
        }
    }

    fn prev(&mut self) {
        if self.current_index > 0 {
            self.current_index -= 1;
            self.cached = None;
            self.reset_zoom();
        }
    }

    fn next(&mut self) {
        if self.current_index + 1 < self.svg_paths.len() {
            self.current_index += 1;
            self.cached = None;
            self.reset_zoom();
        }
    }

    fn goto_first(&mut self) {
        if !self.svg_paths.is_empty() && self.current_index != 0 {
            self.current_index = 0;
            self.cached = None;
            self.reset_zoom();
        }
    }

    fn goto_latest(&mut self) {
        let last = self.svg_paths.len().saturating_sub(1);
        if !self.svg_paths.is_empty() && self.current_index != last {
            self.current_index = last;
            self.cached = None;
            self.reset_zoom();
        }
    }

    fn current_path(&self) -> Option<&PathBuf> {
        self.svg_paths.get(self.current_index)
    }

    /// Return zoom and pan to the fit view. Called whenever the
    /// displayed plot changes so a new plot is never shown through a
    /// stale zoom or pan offset.
    fn reset_zoom(&mut self) {
        self.zoom = 1.0;
        self.pan = egui::Vec2::ZERO;
    }

    /// Write a clear-all marker file. The R-side poller picks it up,
    /// empties plot_history, deletes every plot-*.svg, and resets the
    /// page counter. The viewer's directory poll then sees the empty
    /// dir and shows "no plots".
    fn signal_clear_all(&self) {
        let signal_path = self.watch_dir.join("clear_all.txt");
        let _ = std::fs::write(&signal_path, b"");
    }

    /// Write a clear-active marker file containing the 1-based index
    /// of the currently-displayed plot. The R-side poller removes that
    /// entry from plot_history, deletes the corresponding SVG file,
    /// and renumbers higher-indexed files down by one.
    fn signal_clear_active(&self) {
        if self.svg_paths.is_empty() {
            return;
        }
        let signal_path = self.watch_dir.join("clear_plot.txt");
        let contents = format!("{}", self.current_index + 1);
        let _ = std::fs::write(&signal_path, contents);
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

        if let Some(c) = &self.cached {
            if c.rendered_size.x > 0.0 && c.rendered_size.y > 0.0 {
                let svg_aspect = c.rendered_size.x / c.rendered_size.y;
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
        let Some(source_svg) = self.current_path().cloned() else {
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
            ExportFormat::Svg => {
                if self.export_transparent {
                    // Strip the device background rect and write the
                    // modified SVG. Fall back to a plain copy if the
                    // file cannot be read as text for any reason.
                    match std::fs::read_to_string(&source_svg) {
                        Ok(s) => {
                            let _ = std::fs::write(&dest, strip_background_rect(&s));
                        }
                        Err(_) => {
                            let _ = std::fs::copy(&source_svg, &dest);
                        }
                    }
                } else {
                    let _ = std::fs::copy(&source_svg, &dest);
                }
            }
            ExportFormat::Png => {
                let _ = self.export_as_png(&source_svg, &dest);
            }
        }
    }

    /// Rasterize the given SVG file at `export_width_in × export_height_in`
    /// inches at `export_dpi` and write a PNG. Aspect ratio is preserved
    /// (the rendered image fits within the chosen bounding box). The
    /// PNG file is tagged with the chosen DPI in its pHYs metadata
    /// chunk so print software handles it at the intended resolution.
    fn export_as_png(&self, source_svg: &Path, dest: &Path) -> Result<(), String> {
        let svg_string = std::fs::read_to_string(source_svg).map_err(|e| e.to_string())?;
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

    /// Write the current plot-area dimensions to resize.txt so the
    /// R-side task callback can trigger a display list replay.
    /// Debounced by 5ms — we don't spam writes during active dragging.
    fn maybe_signal_resize(&mut self, current_size: (u32, u32)) {
        const DEBOUNCE: Duration = Duration::from_millis(5);

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
            let signal_path = self.watch_dir.join("resize.txt");
            let contents = format!("{},{}", pending_size.0, pending_size.1);
            let _ = std::fs::write(&signal_path, contents);
            self.last_signaled_size = Some(pending_size);
            self.size_change_pending = None;
        }
    }

    /// Rasterize `path` at the given supersampled target size and
    /// return the CPU-side image plus its pixel dimensions. The
    /// tiny-skia pixmap is scoped to this function, so it is freed
    /// before the caller uploads the image to the GPU; that keeps the
    /// pixmap and egui's upload staging copy from being resident at
    /// the same time. Texture creation and reuse are handled by the
    /// caller so a single GPU texture can be updated in place.
    fn render_svg(
        &self,
        path: &Path,
        target_w: u32,
        target_h: u32,
    ) -> Option<(egui::ColorImage, egui::Vec2)> {
        let svg_bytes = std::fs::read(path).ok()?;

        let mut opt = usvg::Options::default();
        if let Some(dir) = &self.fonts_dir {
            opt.fontdb_mut().load_fonts_dir(dir);
        }
        opt.fontdb_mut().load_system_fonts();
        opt.font_family = "Liberation Sans".to_string();

        let tree = usvg::Tree::from_data(&svg_bytes, &opt).ok()?;

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

        // Scope the pixmap so it is dropped at the end of this block,
        // before the ColorImage is handed back and uploaded. Only the
        // pixels Vec survives past here.
        let color_image = {
            let mut pixmap = Pixmap::new(pixmap_w, pixmap_h)?;
            let transform = Transform::from_scale(scale, scale);
            resvg::render(&tree, transform, &mut pixmap.as_mut());

            let pixels: Vec<egui::Color32> = pixmap
                .pixels()
                .iter()
                .map(|p| {
                    egui::Color32::from_rgba_premultiplied(p.red(), p.green(), p.blue(), p.alpha())
                })
                .collect();

            egui::ColorImage {
                size: [pixmap_w as usize, pixmap_h as usize],
                pixels,
            }
        };

        let rendered = egui::Vec2::new(pixmap_w as f32, pixmap_h as f32);
        Some((color_image, rendered))
    }
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

        // Keyboard navigation. Sample the input flags before mutating
        // self so we don't hold a borrow on ctx.input across mutations.
        let go_prev = ctx.input(|i| i.key_pressed(egui::Key::ArrowLeft));
        let go_next = ctx.input(|i| i.key_pressed(egui::Key::ArrowRight));
        let go_first = ctx.input(|i| i.key_pressed(egui::Key::Home));
        let go_last = ctx.input(|i| i.key_pressed(egui::Key::End));
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

        // Top toolbar: prev / next / counter on the left, action
        // buttons (Export, Clear plot, Clear all) on the right.
        egui::TopBottomPanel::top("rustgd_toolbar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                let at_first = self.current_index == 0 || self.svg_paths.is_empty();
                let at_last = self.current_index + 1 >= self.svg_paths.len();
                let has_plots = !self.svg_paths.is_empty();

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

                let counter_text = if self.svg_paths.is_empty() {
                    "no plots".to_string()
                } else {
                    format!("Plot {} / {}", self.current_index + 1, self.svg_paths.len())
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
                self.cached = None;
                ui.centered_and_justified(|ui| {
                    ui.label("no plot yet");
                });
                return;
            };

            let pixels_per_point = ctx.pixels_per_point();
            let target_w = (available.x * pixels_per_point * SUPERSAMPLE).max(1.0) as u32;
            let target_h = (available.y * pixels_per_point * SUPERSAMPLE).max(1.0) as u32;

            // Read the file's mtime so we can detect in-place
            // overwrites (e.g. R re-rendering the same plot at a
            // new size during a resize).
            let current_mtime = std::fs::metadata(&current_path)
                .and_then(|m| m.modified())
                .ok();

            let needs_render = match &self.cached {
                None => true,
                Some(c) => {
                    c.svg_path != current_path
                        || c.target_size != (target_w, target_h)
                        || current_mtime.map_or(false, |t| t > c.mtime)
                }
            };

            if needs_render {
                if let Some((image, rendered)) = self.render_svg(&current_path, target_w, target_h)
                {
                    // Update the existing texture in place if we have
                    // one, otherwise allocate it once. Either way only
                    // a single GPU texture is ever live. The is_some
                    // check keeps the borrow of self.texture scoped so
                    // the else branch can reassign it cleanly.
                    if self.texture.is_some() {
                        self.texture
                            .as_mut()
                            .unwrap()
                            .set(image, egui::TextureOptions::LINEAR);
                    } else {
                        self.texture = Some(ctx.load_texture(
                            "rustgd-svg",
                            image,
                            egui::TextureOptions::LINEAR,
                        ));
                    }
                    self.cached = Some(CachedRender {
                        svg_path: current_path,
                        target_size: (target_w, target_h),
                        rendered_size: rendered,
                        mtime: current_mtime.unwrap_or(std::time::UNIX_EPOCH),
                    });
                }
            }

            // Pull the values we need for drawing out of the borrowed
            // cache/texture as Copy values, so the zoom/pan handling
            // below can take a mutable borrow of self without conflict.
            let draw_data = self
                .cached
                .as_ref()
                .zip(self.texture.as_ref())
                .map(|(c, tex)| (c.rendered_size, tex.id()));

            if let Some((rendered_size, tex_id)) = draw_data {
                let pixels_per_point = ctx.pixels_per_point();
                // rendered_size is in supersampled physical pixels.
                // Dividing by both pixels_per_point and SUPERSAMPLE
                // gives back the logical fit size. egui's Linear
                // texture filter does the downsample (and any zoom
                // upscale) on the GPU.
                let fit_size = rendered_size / (pixels_per_point * SUPERSAMPLE);

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
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("failed to render SVG");
                });
            }
        });
    }
}
