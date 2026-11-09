//! Draws a .rgd page straight from its display list with tiny-skia, the
//! library resvg draws with. Each shape is built the way usvg builds it
//! from the SVG the device writes for the same page, and clipping works
//! the way resvg clips, so a page looks the same through either path.
//!
//! Text is laid out by usvg, one label at a time at the origin, and the
//! label is then placed with a transform. Layouts are cached across pages
//! and redraws, so a label is shaped once however often it is drawn.
//!
//! One difference is deliberate. A raster drawn with interpolate = FALSE
//! keeps its hard pixel edges here. usvg 0.43 does not understand the
//! "pixelated" keyword the SVG uses for that, so the SVG path smooths it.

use std::collections::HashMap;

use tiny_skia::{
    BlendMode, Color, ColorU8, FillRule, FilterQuality, LineCap, LineJoin, Mask, MaskType, Paint,
    Path, PathBuilder, Pattern, Pixmap, PixmapPaint, Rect, SpreadMode, Stroke, StrokeDash,
    Transform,
};

use crate::display_list::{self, DecodeError, Points, Raster, Record, Style, Text};

/// Most label layouts kept at once. The cache is emptied when it reaches
/// this, which only a very text-heavy session does.
const TEXT_CACHE_LIMIT: usize = 20_000;

/// The SVG writes numbers with three decimals, so a size or width below
/// half a thousandth reads as zero there and usvg drops the element.
/// The renderer drops the same elements.
const SVG_ZERO: f32 = 0.0005;

// R's line end and line join codes.
const GE_BUTT_CAP: u8 = 2;
const GE_SQUARE_CAP: u8 = 3;
const GE_MITRE_JOIN: u8 = 2;
const GE_BEVEL_JOIN: u8 = 3;

/// The miter limit of a fresh R graphics context, also declared on the
/// SVG root element.
const DEFAULT_MITER: f32 = 10.0;

/// A page read from a .rgd file, ready to draw at any size.
pub struct Page {
    bytes: Vec<u8>,
    pub width: f32,
    pub height: f32,
    background: i32,
}

impl Page {
    /// Check the header and every record once, so a damaged file fails
    /// here and never halfway through drawing.
    pub fn new(bytes: Vec<u8>) -> Result<Page, DecodeError> {
        let header = display_list::read_header(&bytes)?;
        for item in display_list::records(&bytes[display_list::HEADER_LEN..]) {
            item?;
        }
        Ok(Page {
            width: header.width,
            height: header.height,
            background: header.background,
            bytes,
        })
    }

    fn records(&self) -> display_list::Records<'_> {
        display_list::records(&self.bytes[display_list::HEADER_LEN..])
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct TextKey {
    text: String,
    family: u8,
    face: u8,
    size: u32,
    anchor: u8,
    col: i32,
}

/// usvg layouts of single labels drawn at the origin. A layout carries no
/// position or rotation, so one serves every place the label appears.
pub struct TextCache {
    trees: HashMap<TextKey, Option<usvg::Tree>>,
}

impl TextCache {
    pub fn new() -> Self {
        Self {
            trees: HashMap::new(),
        }
    }

    fn layout(&mut self, t: &Text, opt: &usvg::Options) -> Option<&usvg::Tree> {
        let key = TextKey {
            text: t.text.to_string(),
            family: t.family,
            face: t.face,
            size: t.size.to_bits(),
            anchor: display_list::text_anchor_code(t.hadj),
            col: t.col,
        };
        if !self.trees.contains_key(&key) {
            if self.trees.len() >= TEXT_CACHE_LIMIT {
                self.trees.clear();
            }
            let at_origin = Text {
                x: 0.0,
                y: 0.0,
                rot: 0.0,
                ..*t
            };
            let svg = format!(
                "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"1\" height=\"1\">{}</svg>",
                display_list::text_element(&at_origin)
            );
            let tree = usvg::Tree::from_str(&svg, opt).ok();
            self.trees.insert(key.clone(), tree);
        }
        self.trees.get(&key).and_then(|tree| tree.as_ref())
    }
}

/// Draw a page to fit inside target_w by target_h pixels, keeping its
/// aspect ratio. The size works out the same as for the page's SVG.
pub fn render_fit(
    page: &Page,
    target_w: u32,
    target_h: u32,
    opt: &usvg::Options,
    texts: &mut TextCache,
) -> Option<Pixmap> {
    if page.width <= 0.0 || page.height <= 0.0 {
        return None;
    }
    let scale = (target_w as f32 / page.width).min(target_h as f32 / page.height);
    let w = (page.width * scale).max(1.0) as u32;
    let h = (page.height * scale).max(1.0) as u32;
    let mut pixmap = Pixmap::new(w, h)?;
    draw(page, &mut pixmap, scale, false, opt, texts);
    Some(pixmap)
}

/// Draw a page onto a pixmap at the given scale. A transparent drawing
/// leaves out the page background, as an SVG export without its
/// background rect would.
pub fn draw(
    page: &Page,
    canvas: &mut Pixmap,
    scale: f32,
    transparent: bool,
    opt: &usvg::Options,
    texts: &mut TextCache,
) {
    if !transparent {
        if page.background == display_list::WHITE {
            canvas.fill(Color::WHITE);
        } else {
            let (r, g, b, a) = rgba(page.background);
            canvas.fill(Color::from_rgba8(r, g, b, a));
        }
    }
    let mut painter = Painter {
        canvas,
        base: Transform::from_scale(scale, scale),
        style: Style {
            col: 0,
            fill: 0,
            lwd: 1.0,
            lty: 0,
            lend: 1,
            ljoin: 1,
            lmitre: DEFAULT_MITER,
        },
        clip: None,
    };
    for item in page.records() {
        let Ok((record, _)) = item else { break };
        match record {
            Record::Style(style) => painter.style = style,
            Record::Clip(corners) => painter.set_clip(corners),
            Record::Line([x1, y1, x2, y2]) => {
                let mut pb = PathBuilder::with_capacity(2, 2);
                pb.move_to(x1, y1);
                pb.line_to(x2, y2);
                if let Some(path) = pb.finish() {
                    painter.shape(&path, FillRule::Winding, false);
                }
            }
            Record::Polyline(points) => {
                if let Some(path) = point_path(points, false) {
                    painter.shape(&path, FillRule::Winding, false);
                }
            }
            Record::Polygon(points) => {
                if let Some(path) = point_path(points, true) {
                    painter.shape(&path, FillRule::Winding, true);
                }
            }
            Record::Path {
                winding,
                counts,
                points,
            } => {
                let mut pb = PathBuilder::with_capacity(points.len() + 8, points.len() + 8);
                let mut pts = points.iter();
                for count in counts.iter() {
                    if count == 0 {
                        continue;
                    }
                    for i in 0..count {
                        let Some((x, y)) = pts.next() else { break };
                        if i == 0 {
                            pb.move_to(x, y);
                        } else {
                            pb.line_to(x, y);
                        }
                    }
                    pb.close();
                }
                let rule = if winding {
                    FillRule::Winding
                } else {
                    FillRule::EvenOdd
                };
                if let Some(path) = pb.finish() {
                    painter.shape(&path, rule, true);
                }
            }
            Record::Rect([x, y, w, h]) => {
                // usvg drops a rect with no width or height, stroke included.
                if w < SVG_ZERO || h < SVG_ZERO {
                    continue;
                }
                if let Some(rect) = Rect::from_xywh(x, y, w, h) {
                    let path = PathBuilder::from_rect(rect);
                    painter.shape(&path, FillRule::Winding, true);
                }
            }
            Record::Circle([cx, cy, r]) => {
                if r < SVG_ZERO {
                    continue;
                }
                if let Some(path) = circle_path(cx, cy, r) {
                    painter.shape(&path, FillRule::Winding, true);
                }
            }
            Record::Text(t) => {
                let Some(tree) = texts.layout(&t, opt) else {
                    continue;
                };
                let Some((target, ts)) = painter.target() else {
                    continue;
                };
                // The SVG rotates the label about its anchor point, so the
                // layout is moved to the anchor and turned there.
                let mut placed = ts.pre_translate(t.x, t.y);
                if t.rot.abs() > 0.001 {
                    placed = placed.pre_rotate(-t.rot);
                }
                resvg::render(tree, placed, &mut target.as_mut());
            }
            Record::Raster(r) => painter.raster(&r),
        }
    }
    painter.finish_clip();
}

/// The clip rectangle in force and, once something is drawn under it, the
/// layer collecting those drawings. resvg draws every clip group into its
/// own layer, masks the layer with the clip, and then composites it.
struct Clip {
    key: [u32; 4],
    /// None when the rectangle has no area, which hides everything in it.
    rect: Option<Rect>,
    /// A clip covering the whole canvas would mask nothing, so drawing
    /// goes straight onto the canvas.
    covers_canvas: bool,
    layer: Option<(Pixmap, i32, i32)>,
}

struct Painter<'p> {
    canvas: &'p mut Pixmap,
    base: Transform,
    style: Style,
    clip: Option<Clip>,
}

impl<'p> Painter<'p> {
    fn set_clip(&mut self, corners: [f32; 4]) {
        let key = corners.map(f32::to_bits);
        if self.clip.as_ref().map(|c| c.key) == Some(key) {
            return;
        }
        self.finish_clip();
        let [x0, y0, x1, y1] = corners;
        let x = x0.min(x1);
        let y = y0.min(y1);
        let w = x0.max(x1) - x;
        let h = y0.max(y1) - y;
        let rect = if w < SVG_ZERO || h < SVG_ZERO {
            None
        } else {
            Rect::from_xywh(x, y, w, h)
        };
        let covers_canvas = rect.map_or(false, |r| {
            let b = self.base;
            r.left() * b.sx + b.tx <= 0.0
                && r.top() * b.sy + b.ty <= 0.0
                && r.right() * b.sx + b.tx >= self.canvas.width() as f32
                && r.bottom() * b.sy + b.ty >= self.canvas.height() as f32
        });
        self.clip = Some(Clip {
            key,
            rect,
            covers_canvas,
            layer: None,
        });
    }

    /// Where the next drawing goes and the transform to draw it with. None
    /// when the clip hides everything.
    fn target(&mut self) -> Option<(&mut Pixmap, Transform)> {
        let base = self.base;
        let canvas_w = self.canvas.width() as i32;
        let canvas_h = self.canvas.height() as i32;
        match self.clip.as_mut() {
            None => Some((&mut *self.canvas, base)),
            Some(clip) if clip.covers_canvas => Some((&mut *self.canvas, base)),
            Some(clip) => {
                let rect = clip.rect?;
                if clip.layer.is_none() {
                    // The clip's pixel bounds plus a 2 pixel margin, like
                    // resvg's layers, so no anti-aliased edge is cut off.
                    let x0 = ((rect.left() * base.sx + base.tx).floor() as i32 - 2).max(0);
                    let y0 = ((rect.top() * base.sy + base.ty).floor() as i32 - 2).max(0);
                    let x1 = ((rect.right() * base.sx + base.tx).ceil() as i32 + 2).min(canvas_w);
                    let y1 = ((rect.bottom() * base.sy + base.ty).ceil() as i32 + 2).min(canvas_h);
                    if x1 <= x0 || y1 <= y0 {
                        return None;
                    }
                    let layer = Pixmap::new((x1 - x0) as u32, (y1 - y0) as u32)?;
                    clip.layer = Some((layer, x0, y0));
                }
                let (layer, x0, y0) = clip.layer.as_mut()?;
                let ts = base.post_translate(-(*x0 as f32), -(*y0 as f32));
                Some((layer, ts))
            }
        }
    }

    /// Mask the open layer with its clip and composite it, as resvg does
    /// when a clip group ends.
    fn finish_clip(&mut self) {
        let Some(clip) = self.clip.take() else { return };
        let (Some(rect), Some((mut layer, x0, y0))) = (clip.rect, clip.layer) else {
            return;
        };
        let ts = self.base.post_translate(-(x0 as f32), -(y0 as f32));
        // resvg fills a black pixmap, clears the clip shape out of it, and
        // uses the inverted alpha as the mask.
        if let Some(mut clip_pixmap) = Pixmap::new(layer.width(), layer.height()) {
            clip_pixmap.fill(Color::BLACK);
            let mut paint = Paint::default();
            paint.anti_alias = true;
            paint.blend_mode = BlendMode::Clear;
            clip_pixmap.fill_path(
                &PathBuilder::from_rect(rect),
                &paint,
                FillRule::Winding,
                ts,
                None,
            );
            let mut mask = Mask::from_pixmap(clip_pixmap.as_ref(), MaskType::Alpha);
            mask.invert();
            layer.apply_mask(&mask);
        }
        self.canvas.draw_pixmap(
            x0,
            y0,
            layer.as_ref(),
            &PixmapPaint {
                opacity: 1.0,
                blend_mode: BlendMode::SourceOver,
                quality: FilterQuality::Nearest,
            },
            Transform::identity(),
            None,
        );
    }

    /// Fill, if the shape takes a fill, and then stroke, in the current
    /// style. SVG paints in that order too.
    fn shape(&mut self, path: &Path, rule: FillRule, fills: bool) {
        let style = self.style;
        let Some((target, ts)) = self.target() else {
            return;
        };
        if fills {
            fill_path(target, path, style.fill, rule, ts);
        }
        stroke_path(target, path, &style, ts);
    }

    /// Draw a raster the way resvg draws a decoded image. The pixels fill
    /// a rectangle of their own size through a pattern, under a transform
    /// that stretches it onto the page.
    fn raster(&mut self, r: &Raster) {
        if r.w < SVG_ZERO || r.h < SVG_ZERO || r.pw == 0 || r.ph == 0 {
            return;
        }
        let Some(image) = raster_pixmap(r) else {
            return;
        };
        let Some(rect) = Rect::from_xywh(0.0, 0.0, r.pw as f32, r.ph as f32) else {
            return;
        };
        let Some((target, ts)) = self.target() else {
            return;
        };
        let mut placed = ts;
        if r.angle.abs() > 0.001 {
            placed = placed.pre_concat(Transform::from_rotate_at(-r.angle, r.x, r.y));
        }
        placed = placed.pre_concat(Transform::from_row(
            r.w / r.pw as f32,
            0.0,
            0.0,
            r.h / r.ph as f32,
            r.x,
            r.y - r.h,
        ));
        let quality = if r.interpolate {
            FilterQuality::Bicubic
        } else {
            FilterQuality::Nearest
        };
        let mut paint = Paint::default();
        paint.shader = Pattern::new(
            image.as_ref(),
            SpreadMode::Pad,
            quality,
            1.0,
            Transform::identity(),
        );
        target.fill_rect(rect, &paint, placed, None);
    }
}

fn rgba(color: i32) -> (u8, u8, u8, u8) {
    (
        (color & 0xFF) as u8,
        ((color >> 8) & 0xFF) as u8,
        ((color >> 16) & 0xFF) as u8,
        ((color >> 24) & 0xFF) as u8,
    )
}

fn fill_path(target: &mut Pixmap, path: &Path, color: i32, rule: FillRule, ts: Transform) {
    let (r, g, b, a) = rgba(color);
    if a == 0 {
        return;
    }
    // resvg skips filling a shape with no area.
    let bounds = path.bounds();
    if bounds.width() == 0.0 || bounds.height() == 0.0 {
        return;
    }
    let mut paint = Paint::default();
    paint.set_color_rgba8(r, g, b, a);
    paint.anti_alias = true;
    target.fill_path(path, &paint, rule, ts, None);
}

fn stroke_path(target: &mut Pixmap, path: &Path, style: &Style, ts: Transform) {
    let (r, g, b, a) = rgba(style.col);
    if a == 0 || !style.lwd.is_finite() || style.lwd < SVG_ZERO {
        return;
    }
    let mut paint = Paint::default();
    paint.set_color_rgba8(r, g, b, a);
    paint.anti_alias = true;
    target.stroke_path(path, &paint, &svg_stroke(style), ts, None);
}

/// The stroke the SVG describes for a style. Round caps and joins and a
/// miter limit of 10 come from the SVG root, and the dash pattern is R's
/// line type in units of the line width, as the SVG writer computes it.
fn svg_stroke(s: &Style) -> Stroke {
    let line_cap = match s.lend {
        GE_BUTT_CAP => LineCap::Butt,
        GE_SQUARE_CAP => LineCap::Square,
        _ => LineCap::Round,
    };
    let line_join = match s.ljoin {
        GE_MITRE_JOIN => LineJoin::Miter,
        GE_BEVEL_JOIN => LineJoin::Bevel,
        _ => LineJoin::Round,
    };
    let miter_limit = if s.ljoin == GE_MITRE_JOIN && s.lmitre != DEFAULT_MITER {
        s.lmitre.max(1.0)
    } else {
        DEFAULT_MITER
    };
    Stroke {
        width: s.lwd,
        miter_limit,
        line_cap,
        line_join,
        dash: dash(s),
    }
}

fn dash(s: &Style) -> Option<StrokeDash> {
    if s.lty == 0 || s.lty == -1 {
        return None;
    }
    let unit = if s.lwd > 1.0 { s.lwd } else { 1.0 };
    let mut bits = s.lty as u32;
    let mut list: Vec<f32> = Vec::with_capacity(16);
    for _ in 0..8 {
        let seg = bits & 0xF;
        if seg == 0 {
            break;
        }
        list.push(seg as f32 * unit);
        bits >>= 4;
    }
    if list.is_empty() {
        return None;
    }
    // SVG repeats an odd-length dash list to make it even.
    if list.len() % 2 != 0 {
        let copy = list.clone();
        list.extend_from_slice(&copy);
    }
    StrokeDash::new(list, 0.0)
}

/// A polyline or polygon. usvg drops one with fewer than two points.
fn point_path(points: Points, close: bool) -> Option<Path> {
    if points.len() < 2 {
        return None;
    }
    let mut pb = PathBuilder::with_capacity(points.len() + 1, points.len());
    for (i, (x, y)) in points.iter().enumerate() {
        if i == 0 {
            pb.move_to(x, y);
        } else {
            pb.line_to(x, y);
        }
    }
    if close {
        pb.close();
    }
    pb.finish()
}

/// A circle built from four arcs, the way usvg turns <circle> into a path.
fn circle_path(cx: f32, cy: f32, r: f32) -> Option<Path> {
    let mut pb = PathBuilder::new();
    pb.move_to(cx + r, cy);
    arc_to(&mut pb, r, cx, cy + r);
    arc_to(&mut pb, r, cx - r, cy);
    arc_to(&mut pb, r, cx, cy - r);
    arc_to(&mut pb, r, cx + r, cy);
    pb.close();
    pb.finish()
}

/// usvg's arc_to for a circular arc with the sweep flag set, through kurbo,
/// so the curves match usvg's exactly.
fn arc_to(pb: &mut PathBuilder, r: f32, x: f32, y: f32) {
    let Some(prev) = pb.last_point() else { return };
    let svg_arc = kurbo::SvgArc {
        from: kurbo::Point::new(prev.x as f64, prev.y as f64),
        to: kurbo::Point::new(x as f64, y as f64),
        radii: kurbo::Vec2::new(r as f64, r as f64),
        x_rotation: 0.0,
        large_arc: false,
        sweep: true,
    };
    match kurbo::Arc::from_svg_arc(&svg_arc) {
        Some(arc) => {
            arc.to_cubic_beziers(0.1, |p1, p2, p| {
                pb.cubic_to(
                    p1.x as f32,
                    p1.y as f32,
                    p2.x as f32,
                    p2.y as f32,
                    p.x as f32,
                    p.y as f32,
                );
            });
        }
        None => pb.line_to(x, y),
    }
}

/// The raster's pixels, premultiplied the way tiny-skia premultiplies a
/// decoded PNG.
fn raster_pixmap(r: &Raster) -> Option<Pixmap> {
    let mut pixmap = Pixmap::new(r.pw, r.ph)?;
    for (dst, src) in pixmap.pixels_mut().iter_mut().zip(r.pixels.chunks_exact(4)) {
        *dst = ColorU8::from_rgba(src[0], src[1], src[2], src[3]).premultiply();
    }
    Some(pixmap)
}
