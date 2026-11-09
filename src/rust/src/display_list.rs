//! The display list. The device records each page as a list of drawing
//! records with 32-bit coordinates, and every writer works from that one
//! list. A page goes to disk as a binary .rgd file, as SVG text generated
//! from the same records, or both.
//!
//! The file layout is little-endian. A 24-byte header holds the magic
//! bytes RGD1, the format version, a reserved field, the page width and
//! height in device units, the background color, and the record count.
//! Each record then starts with a one-byte kind and a four-byte payload
//! length, so a reader can skip a kind it does not know.
//!
//! Coordinates are device units with y running top-down, the same space
//! the SVG uses. Colors keep R's packed format, 8 bits per channel with
//! alpha. Line types stay in R's packed form too, so every reader turns
//! them into dashes the same way.
//!
//! Everything for a page lives in one growing byte buffer. Nothing per
//! record gets its own heap allocation that outlives the drawing call,
//! the same rule the device has followed since the heap corruption it
//! once hit on macOS.

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use std::collections::HashMap;
use std::fmt::Write as _;

/// Magic bytes at the start of every .rgd file.
pub const MAGIC: [u8; 4] = *b"RGD1";
/// Format version. A reader refuses any other version.
pub const VERSION: u16 = 1;
/// Bytes in the file header.
pub const HEADER_LEN: usize = 24;
/// Opaque white in R's packed color format, the page background.
pub const WHITE: i32 = -1;

const KIND_STYLE: u8 = 1;
const KIND_CLIP: u8 = 2;
const KIND_LINE: u8 = 3;
const KIND_POLYLINE: u8 = 4;
const KIND_POLYGON: u8 = 5;
const KIND_PATH: u8 = 6;
const KIND_RECT: u8 = 7;
const KIND_CIRCLE: u8 = 8;
const KIND_TEXT: u8 = 9;
const KIND_RASTER: u8 = 10;

// Payload sizes of the fixed-size records and the fixed parts of the rest.
const STYLE_LEN: usize = 22;
const TEXT_FIXED_LEN: usize = 30;
const RASTER_FIXED_LEN: usize = 29;

// Line end and line join codes from R_ext/GraphicsEngine.h (R_GE_lineend
// and R_GE_linejoin). Round is 1 in both enums and is R's default, so it is
// handled by inheritance from the root <svg> element instead of a constant.
const GE_BUTT_CAP: u8 = 2;
const GE_SQUARE_CAP: u8 = 3;
const GE_MITRE_JOIN: u8 = 2;
const GE_BEVEL_JOIN: u8 = 3;

/// R's default miter limit for a fresh graphics context. The root <svg>
/// element declares the same value so per-element output can skip it.
const R_DEFAULT_LMITRE: f32 = 10.0;

/// SVG font family names, indexed like the device's bundled font families.
pub const FAMILY_SVG_NAMES: [&str; 3] = ["Liberation Sans", "Liberation Serif", "Liberation Mono"];

// ----------------------------------------------------------------------
// Records
// ----------------------------------------------------------------------

/// Stroke and fill settings for the shapes that follow a Style record.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Style {
    pub col: i32,
    pub fill: i32,
    pub lwd: f32,
    pub lty: i32,
    pub lend: u8,
    pub ljoin: u8,
    pub lmitre: f32,
}

impl Style {
    /// The style before any Style record, never used for drawing because
    /// the device writes a Style record ahead of its first shape.
    const INITIAL: Style = Style {
        col: 0,
        fill: 0,
        lwd: 1.0,
        lty: 0,
        lend: 1,
        ljoin: 1,
        lmitre: R_DEFAULT_LMITRE,
    };

    /// SVG attributes for everything about a stroke except its color and
    /// width. Round caps, round joins, and the default miter limit match
    /// the root element, so they are left out.
    fn svg_attrs(&self) -> String {
        let mut out = self.dasharray();
        match self.lend {
            GE_BUTT_CAP => out.push_str(" stroke-linecap=\"butt\""),
            GE_SQUARE_CAP => out.push_str(" stroke-linecap=\"square\""),
            _ => {}
        }
        match self.ljoin {
            GE_MITRE_JOIN => {
                out.push_str(" stroke-linejoin=\"miter\"");
                if self.lmitre != R_DEFAULT_LMITRE {
                    let _ = write!(out, " stroke-miterlimit=\"{:.3}\"", self.lmitre.max(1.0));
                }
            }
            GE_BEVEL_JOIN => out.push_str(" stroke-linejoin=\"bevel\""),
            _ => {}
        }
        out
    }

    /// R packs a line type into up to eight 4-bit segment lengths, read
    /// from the low bits up. A segment is in units of the line width when
    /// the line is wider than one, as in R's own devices.
    fn dasharray(&self) -> String {
        if self.lty == 0 || self.lty == -1 {
            return String::new();
        }
        let unit = if self.lwd > 1.0 { self.lwd } else { 1.0 };
        let mut bits = self.lty as u32;
        let mut parts: Vec<String> = Vec::with_capacity(8);
        for _ in 0..8 {
            let seg = bits & 0xF;
            if seg == 0 {
                break;
            }
            parts.push(format!("{:.3}", seg as f32 * unit));
            bits >>= 4;
        }
        if parts.is_empty() {
            return String::new();
        }
        format!(" stroke-dasharray=\"{}\"", parts.join(","))
    }
}

/// A text label. y is the baseline. hadj is R's horizontal adjustment,
/// 0 for left, 0.5 for centered, and 1 for right.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Text<'a> {
    pub x: f32,
    pub y: f32,
    pub rot: f32,
    pub hadj: f32,
    pub size: f32,
    pub col: i32,
    pub family: u8,
    pub face: u8,
    pub text: &'a str,
}

/// A raster image. y is the bottom edge, where R anchors and rotates the
/// image. pixels holds pw by ph RGBA pixels, row by row from the top.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Raster<'a> {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub angle: f32,
    pub interpolate: bool,
    pub pw: u32,
    pub ph: u32,
    pub pixels: &'a [u8],
}

/// x and y pairs, stored as consecutive 32-bit floats.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Points<'a>(&'a [u8]);

impl<'a> Points<'a> {
    pub fn len(&self) -> usize {
        self.0.len() / 8
    }

    pub fn iter(&self) -> impl Iterator<Item = (f32, f32)> + 'a {
        let bytes = self.0;
        (0..bytes.len() / 8).map(move |i| (f32_at(bytes, i * 8), f32_at(bytes, i * 8 + 4)))
    }
}

/// Point counts of the subpaths in a path.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Counts<'a>(&'a [u8]);

impl<'a> Counts<'a> {
    pub fn iter(&self) -> impl Iterator<Item = u32> + 'a {
        let bytes = self.0;
        (0..bytes.len() / 4).map(move |i| u32_at(bytes, i * 4))
    }
}

/// One decoded record. Point lists, text, and pixels borrow from the
/// buffer they were read from.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Record<'a> {
    Style(Style),
    /// Corners of the clipping rectangle as x0, y0, x1, y1, in any order.
    Clip([f32; 4]),
    Line([f32; 4]),
    Polyline(Points<'a>),
    Polygon(Points<'a>),
    Path {
        winding: bool,
        counts: Counts<'a>,
        points: Points<'a>,
    },
    /// Left edge, top edge, width, and height.
    Rect([f32; 4]),
    /// Center and radius.
    Circle([f32; 3]),
    Text(Text<'a>),
    Raster(Raster<'a>),
}

// ----------------------------------------------------------------------
// Writing
// ----------------------------------------------------------------------

/// The records for one page, appended into one growing buffer. A Style
/// record is only written when the style differs from the last one.
pub struct RecordWriter {
    bytes: Vec<u8>,
    count: u32,
    style: Option<Style>,
}

impl RecordWriter {
    pub fn new() -> Self {
        Self {
            bytes: Vec::new(),
            count: 0,
            style: None,
        }
    }

    /// Empty the buffer for a new page. The allocation is kept.
    pub fn clear(&mut self) {
        self.bytes.clear();
        self.count = 0;
        self.style = None;
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn count(&self) -> u32 {
        self.count
    }

    fn begin(&mut self, kind: u8, len: usize) {
        self.bytes.push(kind);
        self.bytes.extend_from_slice(&(len as u32).to_le_bytes());
        self.count += 1;
    }

    fn put_f32(&mut self, v: f32) {
        self.bytes.extend_from_slice(&v.to_le_bytes());
    }

    fn put_u32(&mut self, v: u32) {
        self.bytes.extend_from_slice(&v.to_le_bytes());
    }

    fn put_i32(&mut self, v: i32) {
        self.bytes.extend_from_slice(&v.to_le_bytes());
    }

    pub fn style(&mut self, style: Style) {
        if self.style == Some(style) {
            return;
        }
        self.begin(KIND_STYLE, STYLE_LEN);
        self.put_i32(style.col);
        self.put_i32(style.fill);
        self.put_f32(style.lwd);
        self.put_i32(style.lty);
        self.bytes.push(style.lend);
        self.bytes.push(style.ljoin);
        self.put_f32(style.lmitre);
        self.style = Some(style);
    }

    pub fn clip(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        self.four(KIND_CLIP, [x0, y0, x1, y1]);
    }

    pub fn line(&mut self, x1: f32, y1: f32, x2: f32, y2: f32) {
        self.four(KIND_LINE, [x1, y1, x2, y2]);
    }

    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        self.four(KIND_RECT, [x, y, w, h]);
    }

    fn four(&mut self, kind: u8, values: [f32; 4]) {
        self.begin(kind, 16);
        for v in values {
            self.put_f32(v);
        }
    }

    pub fn circle(&mut self, cx: f32, cy: f32, r: f32) {
        self.begin(KIND_CIRCLE, 12);
        self.put_f32(cx);
        self.put_f32(cy);
        self.put_f32(r);
    }

    pub fn polyline<I: ExactSizeIterator<Item = (f32, f32)>>(&mut self, points: I) {
        self.point_list(KIND_POLYLINE, points);
    }

    pub fn polygon<I: ExactSizeIterator<Item = (f32, f32)>>(&mut self, points: I) {
        self.point_list(KIND_POLYGON, points);
    }

    fn point_list<I: ExactSizeIterator<Item = (f32, f32)>>(&mut self, kind: u8, points: I) {
        let n = points.len();
        self.begin(kind, 4 + 8 * n);
        self.put_u32(n as u32);
        for (x, y) in points {
            self.put_f32(x);
            self.put_f32(y);
        }
    }

    /// A path of several closed subpaths. counts gives the points in each
    /// subpath, and points must yield exactly their sum.
    pub fn path<I: Iterator<Item = (f32, f32)>>(
        &mut self,
        winding: bool,
        counts: &[u32],
        points: I,
    ) {
        let total: usize = counts.iter().map(|&c| c as usize).sum();
        self.begin(KIND_PATH, 1 + 4 + 4 * counts.len() + 8 * total);
        self.bytes.push(winding as u8);
        self.put_u32(counts.len() as u32);
        for &c in counts {
            self.put_u32(c);
        }
        for (x, y) in points.take(total) {
            self.put_f32(x);
            self.put_f32(y);
        }
    }

    pub fn text(&mut self, t: &Text) {
        let bytes = t.text.as_bytes();
        self.begin(KIND_TEXT, TEXT_FIXED_LEN + bytes.len());
        for v in [t.x, t.y, t.rot, t.hadj, t.size] {
            self.put_f32(v);
        }
        self.put_i32(t.col);
        self.bytes.push(t.family);
        self.bytes.push(t.face);
        self.put_u32(bytes.len() as u32);
        self.bytes.extend_from_slice(bytes);
    }

    pub fn raster(&mut self, r: &Raster) {
        self.begin(KIND_RASTER, RASTER_FIXED_LEN + r.pixels.len());
        for v in [r.x, r.y, r.w, r.h, r.angle] {
            self.put_f32(v);
        }
        self.bytes.push(r.interpolate as u8);
        self.put_u32(r.pw);
        self.put_u32(r.ph);
        self.bytes.extend_from_slice(r.pixels);
    }
}

/// Append the file header for a page. The records follow it unchanged.
pub fn write_header(out: &mut Vec<u8>, width: f32, height: f32, background: i32, count: u32) {
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&background.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
}

// ----------------------------------------------------------------------
// Reading
// ----------------------------------------------------------------------

/// Why a buffer could not be read.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum DecodeError {
    /// Fewer bytes than a header.
    TooShort,
    /// The file does not start with the magic bytes.
    NotDisplayList,
    /// A format version this build does not read.
    Version(u16),
    /// A record runs past the end of the data.
    Truncated,
    /// A text record that is not valid UTF-8.
    BadText,
}

/// The fields of a file header.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Header {
    pub width: f32,
    pub height: f32,
    pub background: i32,
    pub count: u32,
}

pub fn read_header(bytes: &[u8]) -> Result<Header, DecodeError> {
    if bytes.len() < HEADER_LEN {
        return Err(DecodeError::TooShort);
    }
    if bytes[0..4] != MAGIC {
        return Err(DecodeError::NotDisplayList);
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    if version != VERSION {
        return Err(DecodeError::Version(version));
    }
    Ok(Header {
        width: f32_at(bytes, 8),
        height: f32_at(bytes, 12),
        background: i32_at(bytes, 16),
        count: u32_at(bytes, 20),
    })
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn i32_at(b: &[u8], at: usize) -> i32 {
    i32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn f32_at(b: &[u8], at: usize) -> f32 {
    f32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Walks the records in a buffer, skipping kinds this build does not know.
/// Each record comes with the offset just past it, so a caller can resume
/// from there later.
pub struct Records<'a> {
    bytes: &'a [u8],
    pos: usize,
}

/// The records of a buffer holding records only, without a file header.
// The viewer reads pages with this once it draws .rgd files itself.
#[allow(dead_code)]
pub fn records(bytes: &[u8]) -> Records<'_> {
    Records { bytes, pos: 0 }
}

impl<'a> Records<'a> {
    fn fail(&mut self) -> Option<Result<(Record<'a>, usize), DecodeError>> {
        self.pos = self.bytes.len();
        Some(Err(DecodeError::Truncated))
    }
}

impl<'a> Iterator for Records<'a> {
    type Item = Result<(Record<'a>, usize), DecodeError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.pos >= self.bytes.len() {
                return None;
            }
            if self.bytes.len() - self.pos < 5 {
                return self.fail();
            }
            let kind = self.bytes[self.pos];
            let len = u32_at(self.bytes, self.pos + 1) as usize;
            let start = self.pos + 5;
            let end = match start.checked_add(len) {
                Some(end) if end <= self.bytes.len() => end,
                _ => return self.fail(),
            };
            self.pos = end;
            let p = &self.bytes[start..end];

            let record = match kind {
                KIND_STYLE => {
                    if p.len() < STYLE_LEN {
                        return self.fail();
                    }
                    Record::Style(Style {
                        col: i32_at(p, 0),
                        fill: i32_at(p, 4),
                        lwd: f32_at(p, 8),
                        lty: i32_at(p, 12),
                        lend: p[16],
                        ljoin: p[17],
                        lmitre: f32_at(p, 18),
                    })
                }
                KIND_CLIP | KIND_LINE | KIND_RECT => {
                    if p.len() < 16 {
                        return self.fail();
                    }
                    let v = [f32_at(p, 0), f32_at(p, 4), f32_at(p, 8), f32_at(p, 12)];
                    match kind {
                        KIND_CLIP => Record::Clip(v),
                        KIND_LINE => Record::Line(v),
                        _ => Record::Rect(v),
                    }
                }
                KIND_CIRCLE => {
                    if p.len() < 12 {
                        return self.fail();
                    }
                    Record::Circle([f32_at(p, 0), f32_at(p, 4), f32_at(p, 8)])
                }
                KIND_POLYLINE | KIND_POLYGON => {
                    if p.len() < 4 {
                        return self.fail();
                    }
                    let n = u32_at(p, 0) as usize;
                    let need = n.checked_mul(8).and_then(|b| b.checked_add(4));
                    match need {
                        Some(need) if need <= p.len() => {}
                        _ => return self.fail(),
                    }
                    let points = Points(&p[4..4 + 8 * n]);
                    if kind == KIND_POLYLINE {
                        Record::Polyline(points)
                    } else {
                        Record::Polygon(points)
                    }
                }
                KIND_PATH => {
                    if p.len() < 5 {
                        return self.fail();
                    }
                    let winding = p[0] != 0;
                    let nsub = u32_at(p, 1) as usize;
                    let counts_end = match nsub.checked_mul(4).and_then(|b| b.checked_add(5)) {
                        Some(e) if e <= p.len() => e,
                        _ => return self.fail(),
                    };
                    let counts = Counts(&p[5..counts_end]);
                    let total = counts
                        .iter()
                        .try_fold(0usize, |acc, c| acc.checked_add(c as usize));
                    let points_end = match total
                        .and_then(|t| t.checked_mul(8))
                        .and_then(|b| b.checked_add(counts_end))
                    {
                        Some(e) if e <= p.len() => e,
                        _ => return self.fail(),
                    };
                    Record::Path {
                        winding,
                        counts,
                        points: Points(&p[counts_end..points_end]),
                    }
                }
                KIND_TEXT => {
                    if p.len() < TEXT_FIXED_LEN {
                        return self.fail();
                    }
                    let n = u32_at(p, 26) as usize;
                    match TEXT_FIXED_LEN.checked_add(n) {
                        Some(e) if e <= p.len() => {}
                        _ => return self.fail(),
                    }
                    let text = match std::str::from_utf8(&p[TEXT_FIXED_LEN..TEXT_FIXED_LEN + n]) {
                        Ok(t) => t,
                        Err(_) => {
                            self.pos = self.bytes.len();
                            return Some(Err(DecodeError::BadText));
                        }
                    };
                    Record::Text(Text {
                        x: f32_at(p, 0),
                        y: f32_at(p, 4),
                        rot: f32_at(p, 8),
                        hadj: f32_at(p, 12),
                        size: f32_at(p, 16),
                        col: i32_at(p, 20),
                        family: p[24],
                        face: p[25],
                        text,
                    })
                }
                KIND_RASTER => {
                    if p.len() < RASTER_FIXED_LEN {
                        return self.fail();
                    }
                    let pw = u32_at(p, 21);
                    let ph = u32_at(p, 25);
                    let npix = (pw as usize)
                        .checked_mul(ph as usize)
                        .and_then(|n| n.checked_mul(4));
                    match npix.and_then(|n| n.checked_add(RASTER_FIXED_LEN)) {
                        Some(e) if e <= p.len() => {}
                        _ => return self.fail(),
                    }
                    let npix = npix.unwrap_or(0);
                    Record::Raster(Raster {
                        x: f32_at(p, 0),
                        y: f32_at(p, 4),
                        w: f32_at(p, 8),
                        h: f32_at(p, 12),
                        angle: f32_at(p, 16),
                        interpolate: p[20] != 0,
                        pw,
                        ph,
                        pixels: &p[RASTER_FIXED_LEN..RASTER_FIXED_LEN + npix],
                    })
                }
                // A kind from a newer build. Its length lets us step over it.
                _ => continue,
            };
            return Some(Ok((record, end)));
        }
    }
}

// ----------------------------------------------------------------------
// SVG
// ----------------------------------------------------------------------

/// Convert R's packed rcolor (0xAABBGGRR) into an SVG paint value.
///
/// Every bit pattern is a real color. In particular 0x80000000 is black at
/// alpha 128, which is exactly adjustcolor("black", alpha.f = 0.5). An
/// earlier version treated that value as NA_INTEGER and drew nothing. The
/// graphics engine never hands a device NA here because it converts NA
/// colors to transparent white first, so alpha 0 is the only "no paint"
/// case.
pub fn r_color_to_svg(color: i32) -> String {
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

/// Builds the SVG for a page from its records. It keeps what it has
/// already formatted, so writing a growing page again only formats the
/// records added since. That keeps repeated writes as cheap as they were
/// when the device built SVG strings directly.
///
/// Every primitive is tagged with the clip rectangle active when it was
/// drawn. render() emits a clipPath definition for each distinct
/// rectangle and wraps consecutive primitives sharing a clip in one
/// group.
pub struct SvgWriter {
    /// Formatted elements, back to back in one growing buffer.
    raw: String,
    /// The clip id, start, and end of each element in raw.
    meta: Vec<(Option<usize>, usize, usize)>,
    /// Clip rectangle, by the bits of its corners, to its id.
    clip_ids: HashMap<[u32; 4], usize>,
    /// Distinct clip rectangles, indexed by id.
    clip_rects: Vec<[f32; 4]>,
    current_clip: Option<usize>,
    style: Style,
    /// Bytes of the record buffer already formatted.
    consumed: usize,
}

impl SvgWriter {
    pub fn new() -> Self {
        Self {
            raw: String::new(),
            meta: Vec::new(),
            clip_ids: HashMap::new(),
            clip_rects: Vec::new(),
            current_clip: None,
            style: Style::INITIAL,
            consumed: 0,
        }
    }

    /// Start a new page. Allocations are kept.
    pub fn clear(&mut self) {
        self.raw.clear();
        self.meta.clear();
        self.clip_ids.clear();
        self.clip_rects.clear();
        self.current_clip = None;
        self.style = Style::INITIAL;
        self.consumed = 0;
    }

    /// Format the records that came after the ones already handled.
    pub fn catch_up(&mut self, records: &[u8]) -> Result<(), DecodeError> {
        if records.len() < self.consumed {
            // The buffer was cleared under us. Start the page over.
            self.clear();
        }
        let mut iter = Records {
            bytes: records,
            pos: self.consumed,
        };
        while let Some(item) = iter.next() {
            let (record, end) = item?;
            self.add(record);
            self.consumed = end;
        }
        Ok(())
    }

    fn push(&mut self, element: &str) {
        let start = self.raw.len();
        self.raw.push_str(element);
        self.meta.push((self.current_clip, start, self.raw.len()));
    }

    fn add(&mut self, record: Record) {
        let s = self.style;
        match record {
            Record::Style(style) => self.style = style,
            Record::Clip(corners) => {
                let key = corners.map(f32::to_bits);
                let id = match self.clip_ids.get(&key) {
                    Some(&id) => id,
                    None => {
                        let id = self.clip_rects.len();
                        self.clip_rects.push(corners);
                        self.clip_ids.insert(key, id);
                        id
                    }
                };
                self.current_clip = Some(id);
            }
            Record::Line([x1, y1, x2, y2]) => {
                let el = format!(
                    "<line x1=\"{:.3}\" y1=\"{:.3}\" x2=\"{:.3}\" y2=\"{:.3}\" \
                     stroke=\"{}\" stroke-width=\"{:.3}\"{} fill=\"none\"/>",
                    x1,
                    y1,
                    x2,
                    y2,
                    r_color_to_svg(s.col),
                    s.lwd,
                    s.svg_attrs()
                );
                self.push(&el);
            }
            Record::Polyline(points) => {
                let mut el = String::with_capacity(32 + points.len() * 18);
                el.push_str("<polyline points=\"");
                write_points(&mut el, points);
                let _ = write!(
                    el,
                    "\" stroke=\"{}\" stroke-width=\"{:.3}\"{} fill=\"none\"/>",
                    r_color_to_svg(s.col),
                    s.lwd,
                    s.svg_attrs()
                );
                self.push(&el);
            }
            Record::Polygon(points) => {
                let mut el = String::with_capacity(32 + points.len() * 18);
                el.push_str("<polygon points=\"");
                write_points(&mut el, points);
                let _ = write!(
                    el,
                    "\" stroke=\"{}\" fill=\"{}\" stroke-width=\"{:.3}\"{}/>",
                    r_color_to_svg(s.col),
                    r_color_to_svg(s.fill),
                    s.lwd,
                    s.svg_attrs()
                );
                self.push(&el);
            }
            Record::Path {
                winding,
                counts,
                points,
            } => {
                let mut el = String::with_capacity(64 + points.len() * 18);
                el.push_str("<path d=\"");
                let mut pts = points.iter();
                let mut first_token = true;
                for count in counts.iter() {
                    if count == 0 {
                        continue;
                    }
                    for i in 0..count {
                        let Some((x, y)) = pts.next() else { break };
                        if !first_token {
                            el.push(' ');
                        }
                        first_token = false;
                        let cmd = if i == 0 { 'M' } else { 'L' };
                        let _ = write!(el, "{}{:.3},{:.3}", cmd, x, y);
                    }
                    el.push_str(" Z");
                }
                let fill_rule = if winding { "nonzero" } else { "evenodd" };
                let _ = write!(
                    el,
                    "\" stroke=\"{}\" fill=\"{}\" stroke-width=\"{:.3}\"{} fill-rule=\"{}\"/>",
                    r_color_to_svg(s.col),
                    r_color_to_svg(s.fill),
                    s.lwd,
                    s.svg_attrs(),
                    fill_rule
                );
                self.push(&el);
            }
            Record::Rect([x, y, w, h]) => {
                let el = format!(
                    "<rect x=\"{:.3}\" y=\"{:.3}\" width=\"{:.3}\" height=\"{:.3}\" \
                     stroke=\"{}\" fill=\"{}\" stroke-width=\"{:.3}\"{}/>",
                    x,
                    y,
                    w,
                    h,
                    r_color_to_svg(s.col),
                    r_color_to_svg(s.fill),
                    s.lwd,
                    s.svg_attrs()
                );
                self.push(&el);
            }
            Record::Circle([cx, cy, r]) => {
                let el = format!(
                    "<circle cx=\"{:.3}\" cy=\"{:.3}\" r=\"{:.3}\" \
                     stroke=\"{}\" fill=\"{}\" stroke-width=\"{:.3}\"{}/>",
                    cx,
                    cy,
                    r,
                    r_color_to_svg(s.col),
                    r_color_to_svg(s.fill),
                    s.lwd,
                    s.svg_attrs()
                );
                self.push(&el);
            }
            Record::Text(t) => {
                let el = text_element(&t);
                self.push(&el);
            }
            Record::Raster(r) => {
                if let Some(el) = raster_element(&r) {
                    self.push(&el);
                }
            }
        }
    }

    /// The whole page as SVG text.
    pub fn render(&self, width: f32, height: f32, background: i32) -> String {
        let w = width;
        let h = height;
        let mut out = String::with_capacity(self.raw.len() + 512 + 64 * self.meta.len());
        let _ = write!(
            out,
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <svg xmlns=\"http://www.w3.org/2000/svg\" \
             viewBox=\"0 0 {w} {h}\" width=\"{w}\" height=\"{h}\" \
             stroke-linecap=\"round\" stroke-linejoin=\"round\" \
             stroke-miterlimit=\"10\">\n"
        );
        if !self.clip_rects.is_empty() {
            out.push_str("<defs>\n");
            for (i, [x0, y0, x1, y1]) in self.clip_rects.iter().copied().enumerate() {
                let x_left = x0.min(x1);
                let y_top = y0.min(y1);
                let cw = (x0.max(x1) - x_left).max(0.0);
                let ch = (y0.max(y1) - y_top).max(0.0);
                let _ = write!(
                    out,
                    "<clipPath id=\"cp{}\"><rect x=\"{:.3}\" y=\"{:.3}\" \
                     width=\"{:.3}\" height=\"{:.3}\"/></clipPath>\n",
                    i, x_left, y_top, cw, ch
                );
            }
            out.push_str("</defs>\n");
        }
        // The background always covers the full page and is never clipped.
        // Its stable id lets the viewer strip it for a transparent export
        // without touching any background the plot itself draws.
        let bg = if background == WHITE {
            "#ffffff".to_string()
        } else {
            r_color_to_svg(background)
        };
        let _ = writeln!(
            out,
            "<rect id=\"rustgd-bg\" width=\"{w}\" height=\"{h}\" fill=\"{bg}\"/>"
        );
        // Open a clip group whenever the clip changes and close it before
        // the next one. Elements drawn before any clip stand bare.
        let mut open: Option<usize> = None;
        for &(clip, start, end) in &self.meta {
            if clip != open {
                if open.is_some() {
                    out.push_str("</g>\n");
                }
                if let Some(id) = clip {
                    let _ = writeln!(out, "<g clip-path=\"url(#cp{})\">", id);
                }
                open = clip;
            }
            out.push_str(&self.raw[start..end]);
            out.push('\n');
        }
        if open.is_some() {
            out.push_str("</g>\n");
        }
        out.push_str("</svg>\n");
        out
    }
}

/// Points as "x,y" pairs separated by spaces.
fn write_points(out: &mut String, points: Points) {
    for (i, (x, y)) in points.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let _ = write!(out, "{:.3},{:.3}", x, y);
    }
}

fn text_element(t: &Text) -> String {
    let anchor = if t.hadj < 0.25 {
        "start"
    } else if t.hadj > 0.75 {
        "end"
    } else {
        "middle"
    };
    // R rotates text anticlockwise and SVG rotates clockwise, so the angle
    // is negated. The rotation center is the text's anchor point.
    let transform = if t.rot.abs() > 0.001 {
        format!(" transform=\"rotate({:.3} {:.3} {:.3})\"", -t.rot, t.x, t.y)
    } else {
        String::new()
    };
    let weight_attr = if t.face == 1 || t.face == 3 {
        " font-weight=\"bold\""
    } else {
        ""
    };
    let style_attr = if t.face == 2 || t.face == 3 {
        " font-style=\"italic\""
    } else {
        ""
    };
    let family = FAMILY_SVG_NAMES
        .get(t.family as usize)
        .copied()
        .unwrap_or(FAMILY_SVG_NAMES[0]);
    format!(
        "<text x=\"{:.3}\" y=\"{:.3}\" font-family=\"{}\" \
         font-size=\"{:.3}\"{}{} fill=\"{}\" text-anchor=\"{}\"{}>{}</text>",
        t.x,
        t.y,
        family,
        t.size,
        weight_attr,
        style_attr,
        r_color_to_svg(t.col),
        anchor,
        transform,
        escape_xml(t.text)
    )
}

/// The image as a PNG data URI. A raster that fails to encode is dropped
/// rather than taking the page down, the same policy svglite uses.
fn raster_element(r: &Raster) -> Option<String> {
    if r.pw == 0 || r.ph == 0 || r.pixels.is_empty() {
        return None;
    }
    let mut png_buf: Vec<u8> = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_buf, r.pw, r.ph);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(r.pixels).ok()?;
    }
    let b64 = BASE64_STANDARD.encode(&png_buf);
    let interp_attr = if r.interpolate {
        ""
    } else {
        " image-rendering=\"pixelated\""
    };
    // R rotates anticlockwise around the bottom-left corner. SVG rotate()
    // turns clockwise in y-down space, so the angle is negated and the
    // center is the bottom-left corner. This matches the text rotation.
    let transform_attr = if r.angle.abs() > 0.001 {
        format!(
            " transform=\"rotate({:.3}, {:.3}, {:.3})\"",
            -r.angle, r.x, r.y
        )
    } else {
        String::new()
    };
    Some(format!(
        "<image x=\"{:.3}\" y=\"{:.3}\" width=\"{:.3}\" height=\"{:.3}\" \
         preserveAspectRatio=\"none\"{}{} \
         href=\"data:image/png;base64,{}\"/>",
        r.x,
        r.y - r.h,
        r.w,
        r.h,
        interp_attr,
        transform_attr,
        b64
    ))
}

/// Turn a whole .rgd file into SVG text.
pub fn file_to_svg(bytes: &[u8]) -> Result<String, DecodeError> {
    let header = read_header(bytes)?;
    let mut writer = SvgWriter::new();
    writer.catch_up(&bytes[HEADER_LEN..])?;
    Ok(writer.render(header.width, header.height, header.background))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RecordWriter {
        let mut w = RecordWriter::new();
        w.style(Style {
            col: 0xFF000000u32 as i32,
            fill: 0x804682B4u32 as i32,
            lwd: 2.5,
            lty: 0x44,
            lend: 2,
            ljoin: 2,
            lmitre: 3.0,
        });
        w.clip(10.0, 20.0, 300.0, 200.0);
        w.line(1.0, 2.0, 3.0, 4.0);
        w.polyline([(0.5f32, 1.5f32), (2.25, 3.125)].into_iter());
        w.polygon([(1.0f32, 1.0f32), (5.0, 1.0), (3.0, 4.0)].into_iter());
        w.path(
            true,
            &[3, 4],
            [
                (0.0f32, 0.0f32),
                (9.0, 0.0),
                (9.0, 9.0),
                (2.0, 2.0),
                (4.0, 2.0),
                (4.0, 4.0),
                (2.0, 4.0),
            ]
            .into_iter(),
        );
        w.rect(5.0, 6.0, 70.0, 80.0);
        w.circle(50.0, 60.0, 7.5);
        w.text(&Text {
            x: 100.0,
            y: 120.0,
            rot: 90.0,
            hadj: 1.0,
            size: 16.0,
            col: 0xFF0000FFu32 as i32,
            family: 2,
            face: 3,
            text: "caf\u{e9} <&> \"q\" 'a'",
        });
        w.raster(&Raster {
            x: 10.0,
            y: 90.0,
            w: 40.0,
            h: 30.0,
            angle: 30.0,
            interpolate: false,
            pw: 2,
            ph: 1,
            pixels: &[255, 0, 0, 255, 0, 0, 255, 128],
        });
        w
    }

    #[test]
    fn file_round_trip_matches_incremental_svg() {
        let w = sample();
        let mut file = Vec::new();
        write_header(&mut file, 800.0, 600.0, WHITE, w.count());
        file.extend_from_slice(w.bytes());

        let mut incremental = SvgWriter::new();
        incremental.catch_up(w.bytes()).unwrap();
        let direct = incremental.render(800.0, 600.0, WHITE);
        assert_eq!(file_to_svg(&file).unwrap(), direct);

        let header = read_header(&file).unwrap();
        assert_eq!(header.count, w.count());
        let decoded: Vec<Record> = records(&file[HEADER_LEN..]).map(|r| r.unwrap().0).collect();
        assert_eq!(decoded.len() as u32, w.count());
    }

    #[test]
    fn writing_in_pieces_matches_writing_at_once() {
        let w = sample();
        let mut pieces = SvgWriter::new();
        let bytes = w.bytes();
        let mut cut = 0;
        for item in records(bytes) {
            let (_, end) = item.unwrap();
            pieces.catch_up(&bytes[..end]).unwrap();
            cut = end;
        }
        assert_eq!(cut, bytes.len());
        let mut once = SvgWriter::new();
        once.catch_up(bytes).unwrap();
        assert_eq!(
            pieces.render(800.0, 600.0, WHITE),
            once.render(800.0, 600.0, WHITE)
        );
    }

    #[test]
    fn rejects_other_versions_and_truncation() {
        let w = sample();
        let mut file = Vec::new();
        write_header(&mut file, 800.0, 600.0, WHITE, w.count());
        file.extend_from_slice(w.bytes());
        let mut newer = file.clone();
        newer[4] = 2;
        assert_eq!(read_header(&newer), Err(DecodeError::Version(2)));
        let cut = &file[..file.len() - 3];
        assert!(records(&cut[HEADER_LEN..]).any(|r| r.is_err()));
    }

    #[test]
    fn skips_unknown_kinds() {
        let mut bytes = vec![200u8];
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&[1, 2, 3]);
        let mut w = RecordWriter::new();
        w.line(1.0, 2.0, 3.0, 4.0);
        bytes.extend_from_slice(w.bytes());
        let kinds: Vec<Record> = records(&bytes).map(|r| r.unwrap().0).collect();
        assert_eq!(kinds, vec![Record::Line([1.0, 2.0, 3.0, 4.0])]);
    }
}
