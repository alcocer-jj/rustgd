//! rustgd-frames: the data frame viewer window (stage 4, full toolbar).
//!
//! This binary watches a frames directory and shows every data frame written
//! to it. Each `view(df)` call in R drops an Arrow IPC file plus a descriptor;
//! the window holds the list of frames, renders the active one under a toolbar
//! (prev/next plus a "Frame n / N" counter), and jumps to the newest frame as
//! it arrives. Every viewed frame stays loaded, so paging back to one restores
//! its sort, selection, and scroll position exactly where it was left, the way
//! RStudio and Positron keep each viewer alive.
//!
//! The per-frame rendering is rustdf's table lifted verbatim: frozen index
//! column, virtualized rows, column sort via the per-column menu,
//! cell/row/column selection, and column resize. The whole frame is held in
//! memory (no paging, no preview cap), which is the intended behavior.
//!
//! One deliberate cosmetic gap, slated as a small follow-up: the bold header
//! font (Ubuntu-Bold) is not bundled yet, so header names use the regular
//! proportional font, keeping the build free of a font file.
//!
//! Clearing a frame or all frames deletes that frame's files from the
//! directory; the window stays open and a later view() repopulates it.
//! Closing the window clears the frames too and writes a viewer_closed
//! marker, and the viewer records its pid in viewer.pid, so the R side can
//! tell a live window from a force-killed one and relaunch a fresh window
//! rather than reopening onto the previous session's frames.
//! Export saves the rows currently shown (filtered and in view order): the
//! save dialog's extension picks Arrow or CSV, and an unfiltered, unsorted
//! frame is exported as a lossless Arrow file copy. The Summary toggle opens
//! a side panel showing the selected column's stats (missing, unique, and a
//! type-appropriate summary), recomputed over the visible rows so it tracks
//! the filter. Each column's three-dot menu carries a filter: a level
//! checklist for factor and logical columns, a "contains" text box plus an
//! optional min/max range for numeric columns, and a "contains" box for the
//! rest, ANDed across columns, with the toolbar showing the surviving count.
//!
//! Build:  cargo build --release --bin rustgd-frames --features frames
//! Run:    rustgd-frames /tmp/rustgd-frames-<pid>
#![windows_subsystem = "windows"]

use arrow::array::Array;
use arrow::csv::WriterBuilder;
use arrow::datatypes::DataType;
use arrow::ipc::reader::FileReader;
use arrow::util::display::{ArrayFormatter, FormatOptions};
use eframe::egui;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq)]
enum SortDir {
    Asc,
    Desc,
}

#[derive(Clone, Copy, PartialEq)]
enum Selection {
    None,
    Column(usize),
    Row(usize),
    Cell(usize, usize),
}

#[derive(Clone, Copy, PartialEq)]
enum MenuAction {
    SortAsc,
    SortDesc,
    ClearSort,
    Copy,
}

/// A factor or logical column with at most this many distinct values gets a
/// level checklist in its filter; above it, that column falls back to the text
/// "contains" filter like the numeric and character columns.
const FILTER_LEVEL_CAP: usize = 100;

/// Per-column filter. `None` means no filter. `Text` is a case-insensitive
/// substring match on the displayed value, used for character, date, and
/// high-cardinality factor columns. `Levels` is the set of selected display
/// values, used for factor and logical columns with a manageable level count.
/// `Numeric` combines a substring match with an optional inclusive min/max
/// range (kept as the raw typed strings so editing decimals is not disturbed);
/// a row whose value is missing or non-numeric drops out once a bound is set.
#[derive(Clone)]
enum FilterState {
    None,
    Text(String),
    Levels(HashSet<String>),
    Numeric {
        text: String,
        min: String,
        max: String,
    },
}

/// Per-column summary, computed once at load. `missing` counts Arrow nulls
/// (R's NA; a floating NaN is a real value, not counted here). `unique` counts
/// distinct non-missing values. `detail` is the type-appropriate summary.
struct ColStats {
    missing: usize,
    unique: usize,
    detail: StatDetail,
}

enum StatDetail {
    Numeric {
        min: f64,
        median: f64,
        mean: f64,
        max: f64,
    },
    Logical {
        n_true: usize,
        n_false: usize,
    },
    Categorical {
        top: Vec<(String, usize)>,
        other: usize,
    },
    Range {
        min: String,
        max: String,
    },
    Empty,
}

/// Short R-style type label for a column. Works on the Arrow schema directly.
fn type_label(dt: &DataType) -> String {
    match dt {
        DataType::Int8
        | DataType::Int16
        | DataType::Int32
        | DataType::Int64
        | DataType::UInt8
        | DataType::UInt16
        | DataType::UInt32
        | DataType::UInt64 => "int",
        DataType::Float16 | DataType::Float32 | DataType::Float64 => "dbl",
        DataType::Utf8 | DataType::LargeUtf8 => "chr",
        DataType::Boolean => "lgl",
        DataType::Date32 | DataType::Date64 => "date",
        DataType::Timestamp(_, _) => "datetime",
        DataType::Dictionary(_, _) => "fct",
        _ => "?",
    }
    .to_string()
}

struct RustdfApp {
    id: u32,
    headers: Vec<String>,
    types: Vec<String>,
    rows: Vec<Vec<String>>,
    numeric: Vec<bool>,
    col_widths: Vec<f32>,
    row_order: Vec<usize>,
    sort_col: Option<usize>,
    sort_dir: SortDir,
    selection: Selection,
    total_rows: usize,
    full_rows: usize,
    v_offset: f32,
    stats: Vec<ColStats>,
    // Per-column filter state and, for checklist columns (factor/logical with a
    // manageable level count), the sorted list of selectable values. A `None`
    // level list means that column uses the text filter. `view_order` is
    // `row_order` after the active filters are applied, and is what the table
    // draws; it equals `row_order` when no filter is active.
    filters: Vec<FilterState>,
    levels: Vec<Option<Vec<String>>>,
    view_order: Vec<usize>,
    // A counter bumped whenever `view_order` changes, plus a one-column cache of
    // the summary stats computed over the visible rows, so the panel reflects
    // the active filter without recomputing every frame.
    view_gen: u64,
    summary_cache: Option<(usize, u64, ColStats)>,
}

impl RustdfApp {
    /// Read an Arrow IPC file fully into display strings (non-paged).
    fn from_arrow_ipc(
        path: &Path,
        id: u32,
        full_rows_hint: usize,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let file = File::open(path)?;
        let reader = FileReader::try_new(file, None)?;
        let schema = reader.schema();

        let headers: Vec<String> = schema.fields().iter().map(|f| f.name().clone()).collect();
        let types: Vec<String> = schema
            .fields()
            .iter()
            .map(|f| type_label(f.data_type()))
            .collect();
        let ncols = headers.len();
        let numeric: Vec<bool> = types
            .iter()
            .map(|t| t.as_str() == "int" || t.as_str() == "dbl")
            .collect();

        let options = FormatOptions::default().with_null("NA");
        let mut rows: Vec<Vec<String>> = Vec::new();

        // Per-column stat accumulators, filled in the same pass that formats
        // cells. Numeric columns collect parsed values (for min/median/mean/max)
        // and a set of distinct strings; others count value frequencies, which
        // gives both the unique count and the most-frequent values.
        let mut missing = vec![0usize; ncols];
        let mut num_values: Vec<Vec<f64>> = (0..ncols).map(|_| Vec::new()).collect();
        let mut num_unique: Vec<HashSet<String>> = (0..ncols).map(|_| HashSet::new()).collect();
        let mut cat_counts: Vec<HashMap<String, usize>> =
            (0..ncols).map(|_| HashMap::new()).collect();

        for batch in reader {
            let batch = batch?;
            let nrows = batch.num_rows();
            let formatters: Vec<ArrayFormatter> = (0..batch.num_columns())
                .map(|c| ArrayFormatter::try_new(batch.column(c).as_ref(), &options))
                .collect::<Result<Vec<_>, _>>()?;
            for r in 0..nrows {
                let mut row = Vec::with_capacity(ncols);
                for c in 0..batch.num_columns() {
                    let val = formatters[c].value(r).to_string();
                    if batch.column(c).is_null(r) {
                        missing[c] += 1;
                    } else if numeric[c] {
                        if let Ok(f) = val.parse::<f64>() {
                            num_values[c].push(f);
                        }
                        num_unique[c].insert(val.clone());
                    } else {
                        *cat_counts[c].entry(val.clone()).or_insert(0) += 1;
                    }
                    row.push(val);
                }
                rows.push(row);
            }
        }

        let total_rows = rows.len();
        let full_rows = full_rows_hint.max(total_rows);

        // Finalize per-column stats. Alongside, capture the level list for
        // factor and logical columns small enough to filter with a checklist.
        let mut stats: Vec<ColStats> = Vec::with_capacity(ncols);
        let mut levels: Vec<Option<Vec<String>>> = Vec::with_capacity(ncols);
        for c in 0..ncols {
            let (unique, detail, level_list) = if numeric[c] {
                let mut vals = std::mem::take(&mut num_values[c]);
                let unique = num_unique[c].len();
                let detail = if vals.is_empty() {
                    StatDetail::Empty
                } else {
                    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                    let nn = vals.len();
                    let min = vals[0];
                    let max = vals[nn - 1];
                    let mean = vals.iter().sum::<f64>() / nn as f64;
                    let median = if nn % 2 == 1 {
                        vals[nn / 2]
                    } else {
                        (vals[nn / 2 - 1] + vals[nn / 2]) / 2.0
                    };
                    StatDetail::Numeric {
                        min,
                        median,
                        mean,
                        max,
                    }
                };
                (unique, detail, None)
            } else {
                let counts = std::mem::take(&mut cat_counts[c]);
                let unique = counts.len();
                // Checklist levels for factor and logical columns, captured
                // before `counts` is consumed below.
                let level_list: Option<Vec<String>> =
                    if (types[c] == "fct" || types[c] == "lgl") && unique <= FILTER_LEVEL_CAP {
                        let mut keys: Vec<String> = counts.keys().cloned().collect();
                        keys.sort();
                        Some(keys)
                    } else {
                        None
                    };
                let detail = match types[c].as_str() {
                    "lgl" => {
                        let n_true: usize = counts
                            .iter()
                            .filter(|(k, _)| k.eq_ignore_ascii_case("true"))
                            .map(|(_, v)| *v)
                            .sum();
                        let n_false: usize = counts
                            .iter()
                            .filter(|(k, _)| k.eq_ignore_ascii_case("false"))
                            .map(|(_, v)| *v)
                            .sum();
                        StatDetail::Logical { n_true, n_false }
                    }
                    "date" | "datetime" => {
                        let mut keys: Vec<&String> = counts.keys().collect();
                        keys.sort();
                        match (keys.first(), keys.last()) {
                            (Some(first), Some(last)) => StatDetail::Range {
                                min: (*first).clone(),
                                max: (*last).clone(),
                            },
                            _ => StatDetail::Empty,
                        }
                    }
                    _ => {
                        if counts.is_empty() {
                            StatDetail::Empty
                        } else {
                            let mut pairs: Vec<(String, usize)> = counts.into_iter().collect();
                            pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                            let other: usize =
                                pairs.iter().skip(CAT_LIST_CAP).map(|(_, c)| *c).sum();
                            pairs.truncate(CAT_LIST_CAP);
                            StatDetail::Categorical { top: pairs, other }
                        }
                    }
                };
                (unique, detail, level_list)
            };
            stats.push(ColStats {
                missing: missing[c],
                unique,
                detail,
            });
            levels.push(level_list);
        }

        // Estimate column widths from a sample of the loaded rows.
        let sample = total_rows.min(200);
        let mut col_widths = Vec::with_capacity(ncols);
        for c in 0..ncols {
            let mut max_chars = headers[c].chars().count();
            for r in 0..sample {
                let len = rows[r][c].chars().count();
                if len > max_chars {
                    max_chars = len;
                }
            }
            let w = (max_chars as f32 * 8.0 + 16.0).clamp(180.0, 600.0);
            col_widths.push(w);
        }

        let row_order: Vec<usize> = (0..total_rows).collect();
        let view_order = row_order.clone();
        let filters: Vec<FilterState> = (0..ncols).map(|_| FilterState::None).collect();

        Ok(Self {
            id,
            headers,
            types,
            rows,
            numeric,
            col_widths,
            row_order,
            sort_col: None,
            sort_dir: SortDir::Asc,
            selection: Selection::None,
            total_rows,
            full_rows,
            v_offset: 0.0,
            stats,
            filters,
            levels,
            view_order,
            view_gen: 0,
            summary_cache: None,
        })
    }

    fn cell_for_sort(&self, row: usize, col: usize) -> &str {
        self.rows
            .get(row)
            .and_then(|r| r.get(col))
            .map(|s| s.as_str())
            .unwrap_or("")
    }

    fn column_as_text(&self, col: usize) -> String {
        self.rows
            .iter()
            .map(|row| row.get(col).map(|s| s.as_str()).unwrap_or(""))
            .collect::<Vec<&str>>()
            .join("\n")
    }

    fn recompute_order(&mut self) {
        let n = self.total_rows;
        let mut order: Vec<usize> = (0..n).collect();
        if let Some(c) = self.sort_col {
            let desc = self.sort_dir == SortDir::Desc;
            if self.numeric[c] {
                let keys: Vec<Option<f64>> = (0..n)
                    .map(|r| self.cell_for_sort(r, c).parse::<f64>().ok())
                    .collect();
                order.sort_by(|&a, &b| {
                    use std::cmp::Ordering;
                    match (keys[a], keys[b]) {
                        (None, None) => Ordering::Equal,
                        (None, _) => Ordering::Greater,
                        (_, None) => Ordering::Less,
                        (Some(x), Some(y)) => {
                            let ord = x.partial_cmp(&y).unwrap_or(Ordering::Equal);
                            if desc {
                                ord.reverse()
                            } else {
                                ord
                            }
                        }
                    }
                });
            } else {
                order.sort_by(|&a, &b| {
                    let sa = self.cell_for_sort(a, c);
                    let sb = self.cell_for_sort(b, c);
                    let ord = sa.cmp(sb);
                    if desc {
                        ord.reverse()
                    } else {
                        ord
                    }
                });
            }
        }
        self.row_order = order;
        self.recompute_view();
    }

    /// Whether any column has an active filter.
    fn any_filter_active(&self) -> bool {
        self.filters.iter().any(|f| !matches!(f, FilterState::None))
    }

    /// Whether a single row passes every active column filter.
    fn passes_filters(&self, row: usize) -> bool {
        for (c, filter) in self.filters.iter().enumerate() {
            match filter {
                FilterState::None => {}
                FilterState::Text(needle) => {
                    if needle.is_empty() {
                        continue;
                    }
                    let cell = self.cell_for_sort(row, c);
                    if !cell.to_lowercase().contains(&needle.to_lowercase()) {
                        return false;
                    }
                }
                FilterState::Levels(selected) => {
                    let cell = self.cell_for_sort(row, c);
                    if !selected.contains(cell) {
                        return false;
                    }
                }
                FilterState::Numeric { text, min, max } => {
                    let cell = self.cell_for_sort(row, c);
                    if !text.is_empty() && !cell.to_lowercase().contains(&text.to_lowercase()) {
                        return false;
                    }
                    let lo = min.trim().parse::<f64>().ok();
                    let hi = max.trim().parse::<f64>().ok();
                    if lo.is_some() || hi.is_some() {
                        match cell.parse::<f64>() {
                            Ok(v) => {
                                if let Some(lo) = lo {
                                    if v < lo {
                                        return false;
                                    }
                                }
                                if let Some(hi) = hi {
                                    if v > hi {
                                        return false;
                                    }
                                }
                            }
                            Err(_) => return false,
                        }
                    }
                }
            }
        }
        true
    }

    /// Rebuild `view_order` from `row_order` by applying the active filters.
    /// Cheap to skip when nothing is filtered.
    fn recompute_view(&mut self) {
        if !self.any_filter_active() {
            self.view_order = self.row_order.clone();
        } else {
            self.view_order = self
                .row_order
                .iter()
                .copied()
                .filter(|&r| self.passes_filters(r))
                .collect();
        }
        self.view_gen = self.view_gen.wrapping_add(1);
    }

    /// Drop every column's filter and show all rows again.
    fn clear_filters(&mut self) {
        for f in &mut self.filters {
            *f = FilterState::None;
        }
        self.recompute_view();
    }

    /// Compute one column's summary over the currently visible rows. Missing is
    /// taken from the displayed NA sentinel here (the load-time stats use the
    /// exact Arrow nulls); the two agree except for literal "NA" string values.
    fn compute_column_stats(&self, col: usize) -> ColStats {
        let is_num = self.numeric[col];
        let mut missing = 0usize;
        let mut num_values: Vec<f64> = Vec::new();
        let mut num_unique: HashSet<String> = HashSet::new();
        let mut cat_counts: HashMap<String, usize> = HashMap::new();
        for &r in &self.view_order {
            let cell = self
                .rows
                .get(r)
                .and_then(|row| row.get(col))
                .map(|s| s.as_str())
                .unwrap_or("");
            if cell == "NA" {
                missing += 1;
                continue;
            }
            if is_num {
                if let Ok(f) = cell.parse::<f64>() {
                    num_values.push(f);
                }
                num_unique.insert(cell.to_string());
            } else {
                *cat_counts.entry(cell.to_string()).or_insert(0) += 1;
            }
        }
        if is_num {
            let unique = num_unique.len();
            let detail = if num_values.is_empty() {
                StatDetail::Empty
            } else {
                let mut vals = num_values;
                vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let nn = vals.len();
                let min = vals[0];
                let max = vals[nn - 1];
                let mean = vals.iter().sum::<f64>() / nn as f64;
                let median = if nn % 2 == 1 {
                    vals[nn / 2]
                } else {
                    (vals[nn / 2 - 1] + vals[nn / 2]) / 2.0
                };
                StatDetail::Numeric {
                    min,
                    median,
                    mean,
                    max,
                }
            };
            ColStats {
                missing,
                unique,
                detail,
            }
        } else {
            let unique = cat_counts.len();
            let detail = match self.types[col].as_str() {
                "lgl" => {
                    let n_true: usize = cat_counts
                        .iter()
                        .filter(|(k, _)| k.eq_ignore_ascii_case("true"))
                        .map(|(_, v)| *v)
                        .sum();
                    let n_false: usize = cat_counts
                        .iter()
                        .filter(|(k, _)| k.eq_ignore_ascii_case("false"))
                        .map(|(_, v)| *v)
                        .sum();
                    StatDetail::Logical { n_true, n_false }
                }
                "date" | "datetime" => {
                    let mut keys: Vec<&String> = cat_counts.keys().collect();
                    keys.sort();
                    match (keys.first(), keys.last()) {
                        (Some(f), Some(l)) => StatDetail::Range {
                            min: (*f).clone(),
                            max: (*l).clone(),
                        },
                        _ => StatDetail::Empty,
                    }
                }
                _ => {
                    if cat_counts.is_empty() {
                        StatDetail::Empty
                    } else {
                        let mut pairs: Vec<(String, usize)> = cat_counts.into_iter().collect();
                        pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                        let other: usize = pairs.iter().skip(CAT_LIST_CAP).map(|(_, c)| *c).sum();
                        pairs.truncate(CAT_LIST_CAP);
                        StatDetail::Categorical { top: pairs, other }
                    }
                }
            };
            ColStats {
                missing,
                unique,
                detail,
            }
        }
    }

    /// Make sure the summary cache holds the stats for the selected column at
    /// the current view generation, recomputing only when stale.
    fn refresh_summary(&mut self) {
        let col = match self.selection {
            Selection::Column(c) => c,
            Selection::Cell(_, c) => c,
            _ => return,
        };
        if col >= self.headers.len() {
            return;
        }
        let fresh = matches!(
            &self.summary_cache,
            Some((cc, g, _)) if *cc == col && *g == self.view_gen
        );
        if fresh {
            return;
        }
        let stats = self.compute_column_stats(col);
        self.summary_cache = Some((col, self.view_gen, stats));
    }
}

const INDEX_W: f32 = 56.0;
const ROW_H: f32 = 22.0;
const HEADER_H: f32 = 42.0;
const MIN_COL_W: f32 = 40.0;

fn draw_text(
    ui: &egui::Ui,
    rect: egui::Rect,
    text: &str,
    color: egui::Color32,
    size: f32,
    align: egui::Align2,
    mono: bool,
) {
    let painter = ui.painter().with_clip_rect(rect);
    let pos = if align == egui::Align2::CENTER_CENTER {
        rect.center()
    } else if align == egui::Align2::RIGHT_CENTER {
        egui::pos2(rect.right() - 6.0, rect.center().y)
    } else {
        egui::pos2(rect.left() + 6.0, rect.center().y)
    };
    let font = if mono {
        egui::FontId::monospace(size)
    } else {
        egui::FontId::proportional(size)
    };
    painter.text(pos, align, text, font, color);
}

/// Header labels. The bold family is not bundled yet, so this uses the regular
/// proportional font for now; bundling Ubuntu-Bold is a small follow-up.
fn draw_text_bold(
    ui: &egui::Ui,
    rect: egui::Rect,
    text: &str,
    color: egui::Color32,
    size: f32,
    align: egui::Align2,
) {
    let painter = ui.painter().with_clip_rect(rect);
    let pos = if align == egui::Align2::CENTER_CENTER {
        rect.center()
    } else if align == egui::Align2::RIGHT_CENTER {
        egui::pos2(rect.right() - 6.0, rect.center().y)
    } else {
        egui::pos2(rect.left() + 6.0, rect.center().y)
    };
    let font = egui::FontId::proportional(size);
    painter.text(pos, align, text, font, color);
}

fn draw_sort_arrow(ui: &egui::Ui, center: egui::Pos2, ascending: bool, color: egui::Color32) {
    let s = 5.0;
    let pts = if ascending {
        vec![
            egui::pos2(center.x, center.y - s),
            egui::pos2(center.x - s, center.y + s),
            egui::pos2(center.x + s, center.y + s),
        ]
    } else {
        vec![
            egui::pos2(center.x, center.y + s),
            egui::pos2(center.x - s, center.y - s),
            egui::pos2(center.x + s, center.y - s),
        ]
    };
    ui.painter()
        .add(egui::Shape::convex_polygon(pts, color, egui::Stroke::NONE));
}

struct Descriptor {
    entry: String,
    title: String,
    full_rows: usize,
}

fn parse_descriptor(path: &Path) -> Option<Descriptor> {
    let text = fs::read_to_string(path).ok()?;
    let mut entry: Option<String> = None;
    let mut title = String::new();
    let mut full_rows = 0usize;
    for line in text.lines() {
        let line = line.trim();
        let (key, value) = match line.split_once('=') {
            Some(pair) => pair,
            None => continue,
        };
        match key.trim() {
            "entry" => entry = Some(value.trim().to_string()),
            "title" => title = value.trim().to_string(),
            "full_rows" => full_rows = value.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    Some(Descriptor {
        entry: entry?,
        title,
        full_rows,
    })
}

/// Wait briefly for at least one frame descriptor (with its Arrow file) to
/// appear, in case the binary starts a moment before R finishes the first
/// write. Returns false on timeout; the poll loop then picks the frame up.
fn wait_for_any_frame(dir: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(rd) = fs::read_dir(dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                let name = match path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n,
                    None => continue,
                };
                if name.starts_with("frame-") && name.ends_with(".txt") {
                    if let Some(desc) = parse_descriptor(&path) {
                        if dir.join(&desc.entry).exists() {
                            return true;
                        }
                    }
                }
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

impl RustdfApp {
    /// Draw this frame's table into the given ui. The gallery owns the window,
    /// the toolbar, and the close marker; this renders one frame's content and
    /// keeps its own sort, selection, resize, and scroll state.
    fn render(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        // Palette derived from egui's current visuals, so the window follows
        // the system theme and matches the plot viewer and web viewer in both
        // light and dark, rather than rustdf's fixed dark colors. Header and
        // index get the inactive widget fill (a tint that stays distinct from
        // the panel in either theme); grid lines use the noninteractive
        // separator stroke; selection uses egui's own selection colors.
        let style = ctx.style();
        let v = &style.visuals;
        let header_bg = v.widgets.inactive.weak_bg_fill;
        let header_hover = v.widgets.hovered.weak_bg_fill;
        let index_bg = v.widgets.inactive.weak_bg_fill;
        let text_data = v.text_color();
        let text_na = v.weak_text_color();
        let text_header = v.strong_text_color();
        let text_white = v.text_color();
        let accent = v.text_color();
        let dots_hover_color = v.strong_text_color();
        let grid_stroke = egui::Stroke::new(1.0, v.widgets.noninteractive.bg_stroke.color);
        // Selection keeps rustdf's Positron-style navy rather than egui's stock
        // selection blue. The fill is translucent so the row underneath shows
        // through; the border is solid. Navy reads on both light and dark rows.
        let sel_fill = egui::Color32::from_rgba_unmultiplied(0x2C, 0x4B, 0x77, 64);
        let sel_stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(0x2C, 0x4B, 0x77));

        let id = self.id;
        let headers = &self.headers;
        let types = &self.types;
        let rows = &self.rows;
        let widths = &self.col_widths;
        let order = &self.view_order;
        let numeric = &self.numeric;
        let levels = &self.levels;
        let filters = &self.filters;
        let sort_col = self.sort_col;
        let sort_dir = self.sort_dir;
        let selection = self.selection;
        let nrows = self.total_rows;
        let visible = self.view_order.len();
        let filter_active = self.any_filter_active();
        let full_rows = self.full_rows;
        let preview = full_rows > nrows;
        let ncols = headers.len();
        let mirror = self.v_offset;
        let mut deltas = vec![0.0_f32; ncols];
        let mut clicked_col: Option<usize> = None;
        let mut clicked_index: Option<usize> = None;
        let mut clicked_cell: Option<(usize, usize)> = None;
        let mut clicked_dots: Option<usize> = None;
        let mut menu_action: Option<(usize, MenuAction)> = None;
        let mut filter_change: Option<(usize, FilterState)> = None;

        let new_offset = {
            if preview {
                ui.label(format!(
                    "showing {} of {} rows × {} columns  ·  preview",
                    nrows, full_rows, ncols
                ));
            } else if filter_active {
                ui.label(format!(
                    "showing {} of {} rows × {} columns  ·  filtered",
                    visible, nrows, ncols
                ));
            } else {
                ui.label(format!("{} rows × {} columns", nrows, ncols));
            }
            ui.separator();

            let full = ui.available_rect_before_wrap();
            let index_rect = egui::Rect::from_min_max(
                full.min,
                egui::pos2(full.left() + INDEX_W, full.bottom()),
            );
            let data_rect =
                egui::Rect::from_min_max(egui::pos2(full.left() + INDEX_W, full.top()), full.max);

            // ---- LEFT: frozen index column ----
            let mut index_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(index_rect)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            {
                let ui = &mut index_ui;
                ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
                let (corner, _) =
                    ui.allocate_exact_size(egui::vec2(INDEX_W, HEADER_H), egui::Sense::hover());
                ui.painter().rect_filled(corner, 0.0, header_bg);
                ui.painter().rect_stroke(corner, 0.0, grid_stroke);
                if matches!(selection, Selection::Column(0)) {
                    ui.painter().line_segment(
                        [
                            egui::pos2(corner.right(), corner.top()),
                            egui::pos2(corner.right(), corner.bottom()),
                        ],
                        sel_stroke,
                    );
                }

                egui::ScrollArea::vertical()
                    .id_salt(("idx_v", id))
                    .auto_shrink([true, false])
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                    .vertical_scroll_offset(mirror)
                    .show_rows(ui, ROW_H, visible, |ui, row_range| {
                        ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
                        let mut idx_sel_rects: Vec<egui::Rect> = Vec::new();
                        let mut idx_right_lines: Vec<egui::Rect> = Vec::new();
                        for r in row_range {
                            let orig = if order.is_empty() { r } else { order[r] };
                            let (rect, resp) = ui.allocate_exact_size(
                                egui::vec2(INDEX_W, ROW_H),
                                egui::Sense::click(),
                            );
                            let row_selected =
                                matches!(selection, Selection::Row(sr) if sr == orig);
                            let right_selected = matches!(selection, Selection::Column(0))
                                || matches!(selection, Selection::Cell(scr, 0) if scr == orig);
                            ui.painter().rect_filled(rect, 0.0, index_bg);
                            if row_selected {
                                ui.painter().rect_filled(rect, 0.0, sel_fill);
                                idx_sel_rects.push(rect);
                            } else if right_selected {
                                idx_right_lines.push(rect);
                            }
                            ui.painter().rect_stroke(rect, 0.0, grid_stroke);
                            draw_text(
                                ui,
                                rect,
                                &(orig + 1).to_string(),
                                text_white,
                                13.0,
                                egui::Align2::CENTER_CENTER,
                                true,
                            );
                            if resp.clicked() {
                                clicked_index = Some(orig);
                            }
                        }
                        for r in &idx_sel_rects {
                            ui.painter().rect_stroke(*r, 0.0, sel_stroke);
                        }
                        for r in &idx_right_lines {
                            ui.painter().line_segment(
                                [
                                    egui::pos2(r.right(), r.top()),
                                    egui::pos2(r.right(), r.bottom()),
                                ],
                                sel_stroke,
                            );
                        }
                    });
            }

            // ---- RIGHT: data ----
            let mut data_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(data_rect)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            data_ui.set_clip_rect(data_rect);
            let new_offset = {
                let ui = &mut data_ui;
                egui::ScrollArea::horizontal()
                    .id_salt(("data_h", id))
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing = egui::Vec2::ZERO;

                            // header row
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
                                let mut hdr_sel_rects: Vec<egui::Rect> = Vec::new();
                                for c in 0..ncols {
                                    let w = widths[c];
                                    let (rect, resp) = ui.allocate_exact_size(
                                        egui::vec2(w, HEADER_H),
                                        egui::Sense::click(),
                                    );
                                    let col_selected =
                                        matches!(selection, Selection::Column(sc) if sc == c);
                                    let bg = if resp.hovered() {
                                        header_hover
                                    } else {
                                        header_bg
                                    };
                                    ui.painter().rect_filled(rect, 0.0, bg);
                                    if col_selected {
                                        ui.painter().rect_filled(rect, 0.0, sel_fill);
                                        hdr_sel_rects.push(rect);
                                    }
                                    ui.painter().rect_stroke(rect, 0.0, grid_stroke);

                                    let label_w = (w - 40.0).max(0.0);
                                    let name_rect = egui::Rect::from_min_size(
                                        rect.min,
                                        egui::vec2(label_w, HEADER_H * 0.6),
                                    );
                                    let type_rect = egui::Rect::from_min_size(
                                        egui::pos2(rect.left(), rect.top() + HEADER_H * 0.6),
                                        egui::vec2(label_w, HEADER_H * 0.4),
                                    );
                                    draw_text_bold(
                                        ui,
                                        name_rect,
                                        &headers[c],
                                        text_header,
                                        14.0,
                                        egui::Align2::LEFT_CENTER,
                                    );
                                    draw_text(
                                        ui,
                                        type_rect,
                                        &types[c],
                                        text_white,
                                        11.0,
                                        egui::Align2::LEFT_CENTER,
                                        false,
                                    );

                                    if sort_col == Some(c) {
                                        let cx = rect.right() - 36.0;
                                        let cy = rect.top() + HEADER_H * 0.50;
                                        draw_sort_arrow(
                                            ui,
                                            egui::pos2(cx, cy),
                                            sort_dir == SortDir::Asc,
                                            accent,
                                        );
                                    }

                                    let dots_rect = egui::Rect::from_min_max(
                                        egui::pos2(rect.right() - 28.0, rect.top()),
                                        egui::pos2(rect.right() - 4.0, rect.bottom()),
                                    );
                                    let dots_resp = ui.interact(
                                        dots_rect,
                                        egui::Id::new(("rustdf_dots", id, c)),
                                        egui::Sense::click(),
                                    );
                                    let dots_color = if dots_resp.hovered() {
                                        dots_hover_color
                                    } else {
                                        accent
                                    };
                                    let dcx = dots_rect.center().x;
                                    let dcy = dots_rect.center().y;
                                    let dot_r = 1.5;
                                    let dot_s = 4.0;
                                    ui.painter().circle_filled(
                                        egui::pos2(dcx, dcy - dot_s),
                                        dot_r,
                                        dots_color,
                                    );
                                    ui.painter().circle_filled(
                                        egui::pos2(dcx, dcy),
                                        dot_r,
                                        dots_color,
                                    );
                                    ui.painter().circle_filled(
                                        egui::pos2(dcx, dcy + dot_s),
                                        dot_r,
                                        dots_color,
                                    );

                                    if dots_resp.clicked() {
                                        clicked_dots = Some(c);
                                    } else if resp.clicked() {
                                        clicked_col = Some(c);
                                    }

                                    let popup_id = egui::Id::new(("rustdf_menu", id, c));
                                    let is_sorted = sort_col == Some(c);
                                    egui::popup::popup_below_widget(
                                        ui,
                                        popup_id,
                                        &dots_resp,
                                        egui::PopupCloseBehavior::CloseOnClickOutside,
                                        |ui| {
                                            ui.set_min_width(180.0);
                                            if ui
                                                .add_enabled(true, egui::Button::new("Copy Column"))
                                                .clicked()
                                            {
                                                menu_action = Some((c, MenuAction::Copy));
                                                ui.memory_mut(|m| m.close_popup());
                                            }
                                            ui.separator();
                                            if ui
                                                .add_enabled(
                                                    true,
                                                    egui::Button::new("Sort Ascending"),
                                                )
                                                .clicked()
                                            {
                                                menu_action = Some((c, MenuAction::SortAsc));
                                                ui.memory_mut(|m| m.close_popup());
                                            }
                                            if ui
                                                .add_enabled(
                                                    true,
                                                    egui::Button::new("Sort Descending"),
                                                )
                                                .clicked()
                                            {
                                                menu_action = Some((c, MenuAction::SortDesc));
                                                ui.memory_mut(|m| m.close_popup());
                                            }
                                            if ui
                                                .add_enabled(
                                                    is_sorted,
                                                    egui::Button::new("Clear Sorting"),
                                                )
                                                .clicked()
                                            {
                                                menu_action = Some((c, MenuAction::ClearSort));
                                                ui.memory_mut(|m| m.close_popup());
                                            }

                                            ui.separator();
                                            ui.label(egui::RichText::new("Filter").strong());
                                            let current = &filters[c];
                                            match &levels[c] {
                                                Some(level_list) => {
                                                    // Checklist for factor and
                                                    // logical columns.
                                                    let mut selected: HashSet<String> =
                                                        match current {
                                                            FilterState::Levels(s) => s.clone(),
                                                            _ => {
                                                                level_list.iter().cloned().collect()
                                                            }
                                                        };
                                                    let mut changed = false;
                                                    ui.horizontal(|ui| {
                                                        if ui.button("All").clicked() {
                                                            selected = level_list
                                                                .iter()
                                                                .cloned()
                                                                .collect();
                                                            changed = true;
                                                        }
                                                        if ui.button("None").clicked() {
                                                            selected.clear();
                                                            changed = true;
                                                        }
                                                    });
                                                    egui::ScrollArea::vertical()
                                                        .max_height(200.0)
                                                        .id_salt(("filter_levels", id, c))
                                                        .show(ui, |ui| {
                                                            for lvl in level_list {
                                                                let mut on = selected.contains(lvl);
                                                                if ui
                                                                    .checkbox(&mut on, lvl.as_str())
                                                                    .changed()
                                                                {
                                                                    if on {
                                                                        selected
                                                                            .insert(lvl.clone());
                                                                    } else {
                                                                        selected.remove(lvl);
                                                                    }
                                                                    changed = true;
                                                                }
                                                            }
                                                        });
                                                    if changed {
                                                        // All levels selected means
                                                        // no effective filter.
                                                        filter_change = Some((
                                                            c,
                                                            if selected.len() == level_list.len() {
                                                                FilterState::None
                                                            } else {
                                                                FilterState::Levels(selected)
                                                            },
                                                        ));
                                                    }
                                                }
                                                None => {
                                                    if numeric[c] {
                                                        // Numeric: contains plus
                                                        // optional min/max range.
                                                        let (ct, cmin, cmax) = match current {
                                                            FilterState::Numeric {
                                                                text,
                                                                min,
                                                                max,
                                                            } => (
                                                                text.clone(),
                                                                min.clone(),
                                                                max.clone(),
                                                            ),
                                                            FilterState::Text(t) => (
                                                                t.clone(),
                                                                String::new(),
                                                                String::new(),
                                                            ),
                                                            _ => (
                                                                String::new(),
                                                                String::new(),
                                                                String::new(),
                                                            ),
                                                        };
                                                        let mut text = ct;
                                                        let mut min_str = cmin;
                                                        let mut max_str = cmax;
                                                        let mut changed = false;
                                                        if ui
                                                            .add(
                                                                egui::TextEdit::singleline(
                                                                    &mut text,
                                                                )
                                                                .hint_text("contains...")
                                                                .desired_width(160.0),
                                                            )
                                                            .changed()
                                                        {
                                                            changed = true;
                                                        }
                                                        ui.horizontal(|ui| {
                                                            ui.label("min");
                                                            if ui
                                                                .add(
                                                                    egui::TextEdit::singleline(
                                                                        &mut min_str,
                                                                    )
                                                                    .desired_width(64.0),
                                                                )
                                                                .changed()
                                                            {
                                                                changed = true;
                                                            }
                                                            ui.label("max");
                                                            if ui
                                                                .add(
                                                                    egui::TextEdit::singleline(
                                                                        &mut max_str,
                                                                    )
                                                                    .desired_width(64.0),
                                                                )
                                                                .changed()
                                                            {
                                                                changed = true;
                                                            }
                                                        });
                                                        if changed {
                                                            let empty = text.is_empty()
                                                                && min_str.trim().is_empty()
                                                                && max_str.trim().is_empty();
                                                            filter_change = Some((
                                                                c,
                                                                if empty {
                                                                    FilterState::None
                                                                } else {
                                                                    FilterState::Numeric {
                                                                        text,
                                                                        min: min_str,
                                                                        max: max_str,
                                                                    }
                                                                },
                                                            ));
                                                        }
                                                    } else {
                                                        // Text "contains" for
                                                        // character and date columns.
                                                        let current_text = match current {
                                                            FilterState::Text(t) => t.clone(),
                                                            _ => String::new(),
                                                        };
                                                        let mut text = current_text.clone();
                                                        let resp = ui.add(
                                                            egui::TextEdit::singleline(&mut text)
                                                                .hint_text("contains...")
                                                                .desired_width(160.0),
                                                        );
                                                        if resp.changed() {
                                                            filter_change = Some((
                                                                c,
                                                                if text.is_empty() {
                                                                    FilterState::None
                                                                } else {
                                                                    FilterState::Text(text)
                                                                },
                                                            ));
                                                        }
                                                    }
                                                }
                                            }
                                            if !matches!(current, FilterState::None)
                                                && ui.button("Clear filter").clicked()
                                            {
                                                filter_change = Some((c, FilterState::None));
                                                ui.memory_mut(|m| m.close_popup());
                                            }
                                        },
                                    );

                                    let handle = egui::Rect::from_min_max(
                                        egui::pos2(rect.right() - 4.0, rect.top()),
                                        egui::pos2(rect.right() + 4.0, rect.bottom()),
                                    );
                                    let rresp = ui.interact(
                                        handle,
                                        egui::Id::new(("rustdf_resize", id, c)),
                                        egui::Sense::drag(),
                                    );
                                    if rresp.hovered() || rresp.dragged() {
                                        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeColumn);
                                        let line = egui::Rect::from_min_max(
                                            egui::pos2(rect.right() - 1.0, rect.top()),
                                            egui::pos2(rect.right() + 1.0, rect.bottom()),
                                        );
                                        ui.painter().rect_filled(line, 0.0, accent);
                                    }
                                    if rresp.dragged() {
                                        deltas[c] += rresp.drag_delta().x;
                                    }
                                }
                                for r in &hdr_sel_rects {
                                    ui.painter().rect_stroke(*r, 0.0, sel_stroke);
                                }
                            });

                            // data body
                            let out = egui::ScrollArea::vertical()
                                .id_salt(("data_v", id))
                                .auto_shrink([true, false])
                                .scroll_bar_visibility(
                                    egui::scroll_area::ScrollBarVisibility::AlwaysHidden,
                                )
                                .show_rows(ui, ROW_H, visible, |ui, row_range| {
                                    ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
                                    let mut data_sel_rects: Vec<egui::Rect> = Vec::new();
                                    for r in row_range {
                                        let orig = if order.is_empty() { r } else { order[r] };
                                        ui.horizontal(|ui| {
                                            ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
                                            for c in 0..ncols {
                                                let w = widths[c];
                                                let (rect, resp) = ui.allocate_exact_size(
                                                    egui::vec2(w, ROW_H),
                                                    egui::Sense::click(),
                                                );
                                                let (this_fill, this_border) = match selection {
                                                    Selection::Column(sc) if sc == c => {
                                                        (true, true)
                                                    }
                                                    Selection::Row(sr) if sr == orig => {
                                                        (true, true)
                                                    }
                                                    Selection::Cell(scr, scc)
                                                        if scr == orig && scc == c =>
                                                    {
                                                        (false, true)
                                                    }
                                                    _ => (false, false),
                                                };
                                                if this_fill {
                                                    ui.painter().rect_filled(rect, 0.0, sel_fill);
                                                }
                                                ui.painter().rect_stroke(rect, 0.0, grid_stroke);
                                                if this_border {
                                                    data_sel_rects.push(rect);
                                                }
                                                let val = rows
                                                    .get(orig)
                                                    .and_then(|row| row.get(c))
                                                    .map(|s| s.as_str())
                                                    .unwrap_or("");
                                                let align = if numeric[c] {
                                                    egui::Align2::RIGHT_CENTER
                                                } else {
                                                    egui::Align2::LEFT_CENTER
                                                };
                                                let color =
                                                    if val == "NA" { text_na } else { text_data };
                                                draw_text(ui, rect, val, color, 13.0, align, true);
                                                if resp.clicked() {
                                                    clicked_cell = Some((orig, c));
                                                }
                                            }
                                        });
                                    }
                                    for r in &data_sel_rects {
                                        ui.painter().rect_stroke(*r, 0.0, sel_stroke);
                                    }
                                });
                            out.state.offset.y
                        })
                        .inner
                    })
                    .inner
            };
            ui.painter()
                .hline(full.left()..=full.right(), full.top() + 0.5, grid_stroke);
            new_offset
        };

        let mut changed = false;
        for c in 0..ncols {
            if deltas[c] != 0.0 {
                self.col_widths[c] = (self.col_widths[c] + deltas[c]).clamp(MIN_COL_W, 1000.0);
                changed = true;
            }
        }
        if let Some(c) = clicked_col {
            self.selection = Selection::Column(c);
            changed = true;
        }
        if let Some(c) = clicked_dots {
            self.selection = Selection::Column(c);
            let menu_id = egui::Id::new(("rustdf_menu", id, c));
            ctx.memory_mut(|m| m.open_popup(menu_id));
            changed = true;
        }
        if let Some((c, action)) = menu_action {
            match action {
                MenuAction::SortAsc => {
                    self.sort_col = Some(c);
                    self.sort_dir = SortDir::Asc;
                    self.recompute_order();
                }
                MenuAction::SortDesc => {
                    self.sort_col = Some(c);
                    self.sort_dir = SortDir::Desc;
                    self.recompute_order();
                }
                MenuAction::ClearSort => {
                    self.sort_col = None;
                    self.recompute_order();
                }
                MenuAction::Copy => {
                    let text = self.column_as_text(c);
                    ctx.output_mut(|o| o.copied_text = text);
                }
            }
            self.selection = Selection::None;
            changed = true;
        }
        if let Some((c, new_filter)) = filter_change.as_ref() {
            self.filters[*c] = new_filter.clone();
            self.recompute_view();
            changed = true;
        }
        if let Some(orig) = clicked_index {
            self.selection = Selection::Row(orig);
            changed = true;
        }
        if let Some((orig, c)) = clicked_cell {
            self.selection = Selection::Cell(orig, c);
            changed = true;
        }
        let any_interaction = clicked_col.is_some()
            || clicked_index.is_some()
            || clicked_cell.is_some()
            || clicked_dots.is_some()
            || menu_action.is_some()
            || filter_change.is_some();
        let outside_click = !any_interaction && ctx.input(|i| i.pointer.any_click());
        if outside_click && self.selection != Selection::None {
            self.selection = Selection::None;
            changed = true;
        }

        if changed || (new_offset - mirror).abs() > 0.01 {
            ctx.request_repaint();
        }
        self.v_offset = new_offset;
    }
}

const POLL: Duration = Duration::from_millis(250);

struct FrameMeta {
    index: u32,
    descriptor: String,
    entry: String,
    title: String,
    full_rows: usize,
}

/// The window: a gallery over every frame in the directory. It owns the list
/// of descriptors and the loaded table for each frame visited, renders the
/// active one under the toolbar, and watches the directory for new frames.
struct GalleryApp {
    frames_dir: PathBuf,
    metas: Vec<FrameMeta>,
    pos: usize,
    loaded: HashMap<u32, RustdfApp>,
    last_poll: Instant,
    marker_written: bool,
    show_summary: bool,
}

impl GalleryApp {
    fn new(frames_dir: PathBuf) -> Self {
        let mut app = Self {
            frames_dir,
            metas: Vec::new(),
            pos: 0,
            loaded: HashMap::new(),
            last_poll: Instant::now(),
            marker_written: false,
            show_summary: false,
        };
        app.refresh();
        if !app.metas.is_empty() {
            app.pos = app.metas.len() - 1;
        }
        app
    }

    /// Rescan the directory, rebuild the sorted descriptor list, drop loaded
    /// frames whose descriptor disappeared, and jump to the newest frame when
    /// a new one has been added.
    fn refresh(&mut self) {
        let mut metas: Vec<FrameMeta> = Vec::new();
        if let Ok(rd) = fs::read_dir(&self.frames_dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                let name = match path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n,
                    None => continue,
                };
                if let Some(num) = name
                    .strip_prefix("frame-")
                    .and_then(|r| r.strip_suffix(".txt"))
                    .and_then(|d| d.parse::<u32>().ok())
                {
                    if let Some(desc) = parse_descriptor(&path) {
                        metas.push(FrameMeta {
                            index: num,
                            descriptor: name.to_string(),
                            entry: desc.entry,
                            title: desc.title,
                            full_rows: desc.full_rows,
                        });
                    }
                }
            }
        }
        metas.sort_by_key(|m| m.index);

        let new_max = metas.last().map(|m| m.index);
        let old_max = self.metas.last().map(|m| m.index);
        let grew = match (new_max, old_max) {
            (Some(a), Some(b)) => a > b,
            (Some(_), None) => true,
            _ => false,
        };

        // Forget tables whose frame is gone (relevant once delete/clear exist).
        let live: HashSet<u32> = metas.iter().map(|m| m.index).collect();
        self.loaded.retain(|k, _| live.contains(k));

        self.metas = metas;
        if self.metas.is_empty() {
            self.pos = 0;
        } else if grew {
            self.pos = self.metas.len() - 1;
        } else if self.pos >= self.metas.len() {
            self.pos = self.metas.len() - 1;
        }
    }

    /// Delete one frame's data file and descriptor from the directory. The
    /// next refresh drops it from the gallery.
    fn remove_frame_files(&self, descriptor: &str, entry: &str) {
        let _ = fs::remove_file(self.frames_dir.join(entry));
        let _ = fs::remove_file(self.frames_dir.join(descriptor));
    }

    /// Delete every frame's data file and descriptor, leaving the directory
    /// and any close marker in place so the window stays open and empty.
    fn clear_all_files(&self) {
        if let Ok(rd) = fs::read_dir(&self.frames_dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                let name = match path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n,
                    None => continue,
                };
                if name.starts_with("frame-")
                    && (name.ends_with(".txt") || name.ends_with(".arrow"))
                {
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }
}

impl eframe::App for GalleryApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // On close, clear this viewer's frames so a later view() starts fresh,
        // then leave the marker so the R side can tell the window went away and
        // relaunch. This mirrors the web viewer's close behavior.
        if ctx.input(|i| i.viewport().close_requested()) && !self.marker_written {
            self.clear_all_files();
            let _ = fs::write(self.frames_dir.join("viewer_closed"), b"");
            self.marker_written = true;
        }

        // If R has exited and removed the directory, close the window.
        if !self.frames_dir.exists() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        // Poll for new or removed frames on a fixed interval, and keep the
        // watch loop ticking even when the window is idle.
        if self.last_poll.elapsed() >= POLL {
            self.refresh();
            self.last_poll = Instant::now();
        }
        ctx.request_repaint_after(POLL);

        let n = self.metas.len();
        let mut nav: i32 = 0;
        let mut do_clear_frame = false;
        let mut do_clear_all = false;
        let mut do_export = false;
        let mut do_clear_filters = false;
        let mut summary_open = self.show_summary;
        // Filtered row count for the active frame, when a filter is active, for
        // the toolbar indicator. None when nothing is filtered or not loaded.
        let row_status: Option<(usize, usize)> = self
            .metas
            .get(self.pos)
            .map(|m| m.index)
            .and_then(|idx| self.loaded.get(&idx))
            .filter(|f| f.any_filter_active())
            .map(|f| (f.view_order.len(), f.total_rows));

        egui::TopBottomPanel::top("frames_toolbar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let prev = ui.add_enabled(
                    n > 0 && self.pos > 0,
                    egui::Button::new(egui::RichText::new("◀ Prev").size(14.0)),
                );
                if prev.clicked() {
                    nav = -1;
                }
                let next = ui.add_enabled(
                    n > 0 && self.pos + 1 < n,
                    egui::Button::new(egui::RichText::new("Next ▶").size(14.0)),
                );
                if next.clicked() {
                    nav = 1;
                }
                ui.separator();
                if n > 0 {
                    ui.label(format!("Frame {} / {}", self.pos + 1, n));
                    let title = &self.metas[self.pos].title;
                    if !title.is_empty() {
                        ui.separator();
                        ui.label(egui::RichText::new(title).strong());
                    }
                    if let Some((vis, tot)) = row_status {
                        ui.separator();
                        ui.label(egui::RichText::new(format!("{} of {} rows", vis, tot)).weak());
                    }
                    ui.separator();
                    ui.toggle_value(&mut summary_open, egui::RichText::new("Summary").size(14.0));
                    let clear_filters = ui.add_enabled(
                        row_status.is_some(),
                        egui::Button::new(egui::RichText::new("Clear filters").size(14.0)),
                    );
                    if clear_filters.clicked() {
                        do_clear_filters = true;
                    }
                } else {
                    ui.label("no data frames");
                }

                // Clear controls, right-aligned to match the plot and web
                // viewers. In a right-to-left layout the first item added sits
                // furthest right, so "Clear all" ends up as the rightmost
                // button and "Clear frame" to its left.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let clear_all = ui.add_enabled(
                        n > 0,
                        egui::Button::new(egui::RichText::new("Clear all").size(14.0)),
                    );
                    if clear_all.clicked() {
                        do_clear_all = true;
                    }
                    let clear_frame = ui.add_enabled(
                        n > 0,
                        egui::Button::new(egui::RichText::new("Clear frame").size(14.0)),
                    );
                    if clear_frame.clicked() {
                        do_clear_frame = true;
                    }
                    let export = ui.add_enabled(
                        n > 0,
                        egui::Button::new(egui::RichText::new("Export...").size(14.0)),
                    );
                    if export.clicked() {
                        do_export = true;
                    }
                });
            });
            ui.add_space(4.0);
        });
        self.show_summary = summary_open;

        // Clear the active frame's filters when requested.
        if do_clear_filters {
            if let Some(idx) = self.metas.get(self.pos).map(|m| m.index) {
                if let Some(frame) = self.loaded.get_mut(&idx) {
                    frame.clear_filters();
                }
            }
        }

        // Apply the toolbar actions after the panel closure, where self can be
        // mutated. Export reads the active frame and does not change the set,
        // so it runs first; a clear then changes the set and the refresh
        // reconciles metas, loaded, and pos; nav moves within the updated set.
        if do_export {
            let info = self
                .metas
                .get(self.pos)
                .map(|m| (m.index, m.entry.clone(), m.title.clone()));
            if let Some((idx, entry, title)) = info {
                let arrow_path = self.frames_dir.join(&entry);
                let base = if title.is_empty() {
                    format!("frame-{}", idx)
                } else {
                    title
                };
                // Export exactly what is shown: the visible rows in view order.
                // When nothing is filtered or sorted, that is the whole frame in
                // original order, so Arrow export can be a plain file copy.
                let (indices, full) = match self.loaded.get(&idx) {
                    Some(frame) => (
                        frame.view_order.clone(),
                        !frame.any_filter_active() && frame.sort_col.is_none(),
                    ),
                    None => (Vec::new(), true),
                };
                export_frame(&arrow_path, &format!("{}.arrow", base), &indices, full);
            }
        }

        if do_clear_all {
            self.clear_all_files();
            self.refresh();
        } else if do_clear_frame {
            if let Some(meta) = self.metas.get(self.pos) {
                let descriptor = meta.descriptor.clone();
                let entry = meta.entry.clone();
                self.remove_frame_files(&descriptor, &entry);
            }
            self.refresh();
        }

        let len = self.metas.len();
        if nav < 0 && self.pos > 0 {
            self.pos -= 1;
        } else if nav > 0 && self.pos + 1 < len {
            self.pos += 1;
        }

        // Ensure the active frame is loaded before drawing, so both the summary
        // panel and the table can read it.
        let active: Option<u32> = if self.metas.is_empty() {
            None
        } else {
            let (idx, entry, full_rows) = {
                let meta = &self.metas[self.pos];
                (meta.index, meta.entry.clone(), meta.full_rows)
            };
            if !self.loaded.contains_key(&idx) {
                let path = self.frames_dir.join(&entry);
                match RustdfApp::from_arrow_ipc(&path, idx, full_rows) {
                    Ok(app) => {
                        self.loaded.insert(idx, app);
                    }
                    Err(e) => {
                        eprintln!("rustgd-frames: failed to load frame {}: {}", idx, e);
                    }
                }
            }
            if self.loaded.contains_key(&idx) {
                Some(idx)
            } else {
                None
            }
        };

        // Summary side panel: the selected column's stats for the active frame.
        // Drawn before the central panel so it claims the right edge.
        if self.show_summary {
            // Recompute the selected column's stats over the visible rows
            // (cached, so only when the filter or selection changed).
            if let Some(idx) = active {
                if let Some(frame) = self.loaded.get_mut(&idx) {
                    frame.refresh_summary();
                }
            }
            egui::SidePanel::right("frame_summary")
                .resizable(true)
                .default_width(260.0)
                .show(ctx, |ui| {
                    match active.and_then(|idx| self.loaded.get(&idx)) {
                        Some(frame) => render_summary(ui, frame),
                        None => {
                            ui.add_space(8.0);
                            ui.label("no data frames yet");
                        }
                    }
                });
        }

        egui::CentralPanel::default().show(ctx, |ui| match active {
            None => {
                ui.centered_and_justified(|ui| {
                    ui.label("no data frames yet");
                });
            }
            Some(idx) => {
                if let Some(frame) = self.loaded.get_mut(&idx) {
                    frame.render(ctx, ui);
                }
            }
        });
    }
}

/// How many distinct values the summary panel keeps for a non-numeric column.
/// At or below this many distinct values the panel lists all of them; above it,
/// the panel shows the most frequent ones and a "more" line.
const CAT_LIST_CAP: usize = 20;

/// Render the summary side panel for one frame: the stats of whichever column
/// is selected, or a hint when nothing is.
fn render_summary(ui: &mut egui::Ui, frame: &RustdfApp) {
    let col = match frame.selection {
        Selection::Column(c) => Some(c),
        Selection::Cell(_, c) => Some(c),
        _ => None,
    };
    let col = match col {
        Some(c) if c < frame.headers.len() && c < frame.stats.len() => c,
        _ => {
            ui.add_space(8.0);
            ui.label("Click a column to see its summary.");
            return;
        }
    };

    egui::ScrollArea::vertical()
        .id_salt(("summary_scroll", frame.id))
        .show(ui, |ui| {
            ui.add_space(8.0);
            ui.heading(&frame.headers[col]);
            ui.label(egui::RichText::new(format!("type: {}", frame.types[col])).weak());
            ui.separator();

            // Prefer the cache, which holds stats over the visible rows; fall back to
            // the load-time full-frame stats if it is somehow absent.
            let st: &ColStats = match &frame.summary_cache {
                Some((cc, _, s)) if *cc == col => s,
                _ => &frame.stats[col],
            };
            let denom = frame.view_order.len();
            let pct = if denom > 0 {
                st.missing as f64 / denom as f64 * 100.0
            } else {
                0.0
            };
            egui::Grid::new(("summary_head", frame.id, col))
                .num_columns(2)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    ui.label("missing");
                    ui.label(format!("{} ({:.1}%)", st.missing, pct));
                    ui.end_row();
                    ui.label("unique");
                    ui.label(format!("{}", st.unique));
                    ui.end_row();
                });
            ui.separator();

            match &st.detail {
                StatDetail::Numeric {
                    min,
                    median,
                    mean,
                    max,
                } => {
                    egui::Grid::new(("summary_num", frame.id, col))
                        .num_columns(2)
                        .spacing([12.0, 4.0])
                        .show(ui, |ui| {
                            ui.label("min");
                            ui.label(fmt_num(*min));
                            ui.end_row();
                            ui.label("median");
                            ui.label(fmt_num(*median));
                            ui.end_row();
                            ui.label("mean");
                            ui.label(fmt_num(*mean));
                            ui.end_row();
                            ui.label("max");
                            ui.label(fmt_num(*max));
                            ui.end_row();
                        });
                }
                StatDetail::Logical { n_true, n_false } => {
                    egui::Grid::new(("summary_lgl", frame.id, col))
                        .num_columns(2)
                        .spacing([12.0, 4.0])
                        .show(ui, |ui| {
                            ui.label("TRUE");
                            ui.label(format!("{}", n_true));
                            ui.end_row();
                            ui.label("FALSE");
                            ui.label(format!("{}", n_false));
                            ui.end_row();
                        });
                }
                StatDetail::Range { min, max } => {
                    egui::Grid::new(("summary_range", frame.id, col))
                        .num_columns(2)
                        .spacing([12.0, 4.0])
                        .show(ui, |ui| {
                            ui.label("min");
                            ui.label(min);
                            ui.end_row();
                            ui.label("max");
                            ui.label(max);
                            ui.end_row();
                        });
                }
                StatDetail::Categorical { top, other } => {
                    let truncated = *other > 0;
                    let label = if truncated {
                        "most frequent"
                    } else if frame.types[col] == "fct" {
                        "levels"
                    } else {
                        "values"
                    };
                    ui.label(egui::RichText::new(label).weak());
                    ui.add_space(2.0);
                    egui::Grid::new(("summary_cat", frame.id, col))
                        .num_columns(2)
                        .spacing([12.0, 4.0])
                        .show(ui, |ui| {
                            for (val, cnt) in top {
                                ui.label(val);
                                ui.label(format!("{}", cnt));
                                ui.end_row();
                            }
                            if truncated {
                                let rem = st.unique.saturating_sub(top.len());
                                ui.label(
                                    egui::RichText::new(format!("other ({} levels)", rem)).weak(),
                                );
                                ui.label(egui::RichText::new(format!("{}", other)).weak());
                                ui.end_row();
                            }
                        });
                }
                StatDetail::Empty => {
                    ui.label("(no non-missing values)");
                }
            }
        });
}

/// Format a numeric stat: a plain integer when it has no fractional part, else
/// up to four decimals with trailing zeros trimmed.
fn fmt_num(x: f64) -> String {
    if !x.is_finite() {
        return format!("{}", x);
    }
    if x == x.trunc() && x.abs() < 1e15 {
        format!("{}", x as i64)
    } else {
        let s = format!("{:.4}", x);
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Save a frame out. Opens a native save dialog with the frame's name
/// pre-filled and an Arrow extension; whatever extension you leave on the name
/// decides the format. ".csv" writes CSV read from the Arrow file (values
/// intact, types flattened); anything else copies the Arrow file verbatim,
/// which is lossless and rereads with arrow::read_feather() in R or pyarrow.
fn export_frame(arrow_path: &Path, default_name: &str, indices: &[usize], full: bool) {
    let dest = match rfd::FileDialog::new()
        .set_file_name(default_name)
        .save_file()
    {
        Some(p) => p,
        None => return,
    };
    let is_csv = dest
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("csv"))
        .unwrap_or(false);
    let result = if is_csv {
        write_csv(arrow_path, &dest, if full { None } else { Some(indices) })
    } else if full {
        // Whole frame in original order: a plain lossless file copy.
        fs::copy(arrow_path, &dest)
            .map(|_| ())
            .map_err(|e| e.into())
    } else {
        write_arrow_filtered(arrow_path, &dest, indices)
    };
    if let Err(e) = result {
        eprintln!("rustgd-frames: export failed: {}", e);
    }
}

/// Read every record batch from an Arrow IPC file into one combined batch.
fn read_combined(
    arrow_path: &Path,
) -> Result<
    (
        arrow::datatypes::SchemaRef,
        arrow::record_batch::RecordBatch,
    ),
    Box<dyn std::error::Error>,
> {
    let input = File::open(arrow_path)?;
    let reader = FileReader::try_new(input, None)?;
    let schema = reader.schema();
    let batches: Vec<arrow::record_batch::RecordBatch> = reader.collect::<Result<Vec<_>, _>>()?;
    let combined = arrow::compute::concat_batches(&schema, &batches)?;
    Ok((schema, combined))
}

/// Select rows from a batch by index, preserving column types and the given
/// order (so a filtered, sorted view exports exactly as shown).
fn take_rows(
    batch: &arrow::record_batch::RecordBatch,
    indices: &[usize],
) -> Result<arrow::record_batch::RecordBatch, Box<dyn std::error::Error>> {
    let idx =
        arrow::array::UInt32Array::from(indices.iter().map(|&i| i as u32).collect::<Vec<u32>>());
    let mut cols = Vec::with_capacity(batch.num_columns());
    for c in 0..batch.num_columns() {
        cols.push(arrow::compute::take(batch.column(c).as_ref(), &idx, None)?);
    }
    Ok(arrow::record_batch::RecordBatch::try_new(
        batch.schema(),
        cols,
    )?)
}

/// Read an Arrow IPC file and write CSV with a header. With `indices`, only
/// those rows are written, in that order; otherwise the whole file.
fn write_csv(
    arrow_path: &Path,
    dest: &Path,
    indices: Option<&[usize]>,
) -> Result<(), Box<dyn std::error::Error>> {
    let output = File::create(dest)?;
    let mut writer = WriterBuilder::new().with_header(true).build(output);
    match indices {
        None => {
            let input = File::open(arrow_path)?;
            let reader = FileReader::try_new(input, None)?;
            for batch in reader {
                writer.write(&batch?)?;
            }
        }
        Some(ix) => {
            let (_schema, combined) = read_combined(arrow_path)?;
            let taken = take_rows(&combined, ix)?;
            writer.write(&taken)?;
        }
    }
    Ok(())
}

/// Write only the given rows of an Arrow IPC file to a new Arrow IPC file,
/// preserving column types and the on-screen order.
fn write_arrow_filtered(
    arrow_path: &Path,
    dest: &Path,
    indices: &[usize],
) -> Result<(), Box<dyn std::error::Error>> {
    let (schema, combined) = read_combined(arrow_path)?;
    let taken = take_rows(&combined, indices)?;
    let out = File::create(dest)?;
    let mut writer = arrow::ipc::writer::FileWriter::try_new(out, &schema)?;
    writer.write(&taken)?;
    writer.finish()?;
    Ok(())
}

fn main() {
    let dir = match std::env::args().nth(1) {
        Some(arg) => PathBuf::from(arg),
        None => {
            eprintln!("usage: rustgd-frames <frames-directory>");
            std::process::exit(2);
        }
    };
    if !dir.is_dir() {
        eprintln!(
            "rustgd-frames: frames directory does not exist: {}",
            dir.display()
        );
        std::process::exit(1);
    }

    // Record our process id so the R side can tell a live window from one that
    // was force-killed (a crash leaves no viewer_closed marker, so a pid that
    // no longer exists is how it knows to relaunch fresh).
    let _ = fs::write(dir.join("viewer.pid"), std::process::id().to_string());

    // Wait briefly so the window opens with the first frame already present
    // rather than flashing the empty state; later frames arrive via the poll.
    let _ = wait_for_any_frame(&dir, Duration::from_secs(5));

    let app = GalleryApp::new(dir);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("rustgd frames")
            .with_inner_size([1000.0, 640.0]),
        ..Default::default()
    };
    let _ = eframe::run_native("rustgd frames", options, Box::new(|_cc| Ok(Box::new(app))));
}
