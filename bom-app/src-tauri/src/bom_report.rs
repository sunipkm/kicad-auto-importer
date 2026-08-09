//! Renders a "Populate BOM" batch's results as a paginated PDF table,
//! landscape A4, clearly flagging every part that isn't confirmed in
//! stock anywhere (see [`ReportRow::in_stock`]) or that any vendor
//! flags as obsolete/EOL/NRND (see [`ReportRow::lifecycle_concern`]) —
//! a failed lookup counts as both, since "we don't know" is exactly the
//! case a human reviewing the BOM needs pointed out, not silently
//! dropped.
//!
//! Deliberately built on `printpdf`'s built-in Base14 fonts
//! (`BuiltinFont::Helvetica`/`HelveticaBold`) rather than an embedded
//! TTF: every PDF viewer already ships substitutes for the standard 14
//! fonts, so this needs no font asset bundled with the app (relevant
//! since it ships cross-platform — see `kicad_paths.rs`'s per-OS
//! handling) at the cost of `WinAnsiEncoding`-only text (fine for
//! references/manufacturers/part numbers, which are all
//! ASCII/Latin-1 in practice).
//!
//! Table rows are drawn by hand (position each cell's text explicitly,
//! track a running `y` cursor, start a new page and re-draw the column
//! header when the next row wouldn't fit) rather than via a layout
//! crate — the alternative (`genpdf`) needs its own bundled TTF for
//! accurate text metrics, reintroducing the exact font-asset problem
//! this module avoids by using Base14 fonts directly.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use printpdf::path::PaintMode;
use printpdf::{BuiltinFont, Color, Mm, PdfLayerReference, Rect, Rgb};
use rust_xlsxwriter::{Color as XlsxColor, Format as XlsxFormat, Workbook};
use serde::{Deserialize, Serialize};

use crate::bom_pricing;
use crate::parts_lookup::PartInfo;

/// A column that can appear in the priced XLSX export.
///
/// Mandatory columns (Part, References, NeededQty) are always included;
/// optional columns can be toggled and reordered via [`crate::xlsx_columns::XlsxColumnsConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum XlsxColumn {
    Part,
    References,
    NeededQty,
    PurchaseQty,
    Vendor,
    UnitPrice,
    TotalPrice,
    InStock,
    StockQty,
    StockShortfall,
    LifecycleConcern,
}

impl XlsxColumn {
    /// Default column set in default display order — matches the original fixed layout.
    pub const ALL: &'static [Self] = &[
        Self::Part,
        Self::References,
        Self::NeededQty,
        Self::PurchaseQty,
        Self::Vendor,
        Self::UnitPrice,
        Self::TotalPrice,
        Self::InStock,
        Self::StockQty,
        Self::StockShortfall,
        Self::LifecycleConcern,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Part => "Part",
            Self::References => "References",
            Self::NeededQty => "Need",
            Self::PurchaseQty => "Buy",
            Self::Vendor => "Vendor",
            Self::UnitPrice => "Unit Price",
            Self::TotalPrice => "Ext Price",
            Self::InStock => "In Stock",
            Self::StockQty => "Stock Qty",
            Self::StockShortfall => "Stock Shortfall",
            Self::LifecycleConcern => "Lifecycle Concern",
        }
    }

    /// Mandatory columns are always visible and cannot be hidden.
    pub fn is_mandatory(self) -> bool {
        matches!(self, Self::Part | Self::References | Self::NeededQty)
    }
}

/// One row of the report: a placed symbol plus whatever the lookup
/// found (or the error it failed with).
pub struct ReportRow {
    pub reference: String,
    pub symbol: String,
    /// `Err` holds the lookup failure's display message.
    pub outcome: Result<PartInfo, String>,
}

impl ReportRow {
    /// Mirrors `PartInfo::in_stock` but also covers the failed-lookup
    /// case (no vendor data at all is never "confirmed in stock").
    fn in_stock(&self) -> bool {
        self.outcome
            .as_ref()
            .map(|info| info.in_stock())
            .unwrap_or(false)
    }

    /// Mirrors `PartInfo::lifecycle_concern`; a failed lookup counts as
    /// a concern too (same reasoning as `in_stock` above).
    fn lifecycle_concern(&self) -> bool {
        self.outcome
            .as_ref()
            .map(|info| info.lifecycle_concern())
            .unwrap_or(true)
    }

    /// Whether this row's background should be highlighted — either
    /// kind of flag is worth a human's attention, so either is enough.
    fn needs_attention(&self) -> bool {
        !self.in_stock() || self.lifecycle_concern()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("pdf error: {0}")]
    Pdf(#[from] printpdf::Error),
    #[error("xlsx error: {0}")]
    Xlsx(#[from] rust_xlsxwriter::XlsxError),
}

const PAGE_WIDTH: f32 = 297.0;
const PAGE_HEIGHT: f32 = 210.0;
const MARGIN: f32 = 15.0;
/// A single-line row's height — also doubles as the header row's fixed
/// height, since column labels never wrap.
const ROW_HEIGHT: f32 = 7.0;
/// Extra height a data row grows by per wrapped line beyond its first —
/// see [`row_height`].
const LINE_STEP: f32 = 4.0;
const ROW_FONT_SIZE: f32 = 9.0;
const HEADER_FONT_SIZE: f32 = 9.0;
/// Columns as (label, width in mm) — widths sum to exactly
/// `PAGE_WIDTH - 2 * MARGIN` (267mm) so the table's right edge lands on
/// the right margin. Used by the "Populate BOM" stock report
/// (`generate`); the priced BOM report (`generate_priced_bom`) has its
/// own [`PRICED_COLUMNS`] — both share the same drawing machinery below
/// via an explicit `columns` parameter rather than a single hardcoded
/// const.
const STOCK_COLUMNS: &[(&str, f32)] = &[
    ("Reference", 16.0),
    ("Symbol", 30.0),
    ("Manufacturer", 42.0),
    ("MPN", 36.0),
    ("Stock", 55.0),
    ("Lifecycle", 40.0),
    ("Status", 48.0),
];

/// Columns for the priced/grouped BOM report — see [`STOCK_COLUMNS`].
/// Widths also sum to exactly `PAGE_WIDTH - 2 * MARGIN` (267mm).
const PRICED_COLUMNS: &[(&str, f32)] = &[
    ("Part", 50.0),
    ("References", 45.0),
    ("Need", 16.0),
    ("Buy", 16.0),
    ("Vendor", 26.0),
    ("Unit $", 22.0),
    ("Ext $", 24.0),
    ("Flags", 68.0),
];

fn column_x(columns: &[(&str, f32)], index: usize) -> f32 {
    MARGIN + columns[..index].iter().map(|(_, w)| w).sum::<f32>()
}

fn text_color(color: (f32, f32, f32)) -> Color {
    Color::Rgb(Rgb::new(color.0, color.1, color.2, None))
}

const BLACK: (f32, f32, f32) = (0.0, 0.0, 0.0);
const WHITE: (f32, f32, f32) = (1.0, 1.0, 1.0);
const RED: (f32, f32, f32) = (0.72, 0.08, 0.08);
const GREEN: (f32, f32, f32) = (0.0, 0.5, 0.15);
const HEADER_BG: (f32, f32, f32) = (0.20, 0.22, 0.26);
const ZEBRA_BG: (f32, f32, f32) = (0.95, 0.95, 0.96);
const FLAGGED_BG: (f32, f32, f32) = (0.98, 0.85, 0.85);

// ── text measurement & word-wrap ────────────────────────────────────────
//
// No ellipsis truncation: every cell wraps onto as many lines as it
// needs so the full value is always visible (a row just grows taller).
// That requires knowing each character's actual rendered width, which
// `printpdf`'s built-in Base14 fonts don't expose — the crate only
// computes glyph widths for *embedded* TTF fonts (see `font.rs`'s
// `font_metrics()`), since built-in fonts are never parsed, just
// referenced by name and rendered by whatever the PDF viewer ships.
//
// So the two tables below hardcode the standard Adobe Core 14 metrics
// for Helvetica/Helvetica-Bold — public, unchanging, part of the PDF
// spec itself (the same numbers ship in every PDF-capable toolchain,
// e.g. Ghostscript's and matplotlib's bundled `Helvetica.afm`) — for
// the WinAnsi/ASCII printable range (32..=126), which is all this
// report ever renders. `WX` (width in 1/1000 em) per character code,
// index 0 == code 32 (space).
const HELVETICA_WIDTHS: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 222, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556,
    556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556, 1015, 667, 667, 722, 722, 667,
    611, 778, 722, 278, 500, 667, 556, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667,
    667, 611, 278, 278, 278, 469, 556, 222, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500,
    222, 833, 556, 556, 556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584,
];
const HELVETICA_BOLD_WIDTHS: [u16; 95] = [
    278, 333, 474, 556, 556, 889, 722, 278, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556,
    556, 556, 556, 556, 556, 556, 556, 333, 333, 584, 584, 584, 611, 975, 722, 722, 722, 722, 667,
    611, 778, 722, 278, 556, 722, 611, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667,
    667, 611, 333, 278, 333, 584, 556, 278, 556, 611, 556, 611, 556, 333, 611, 611, 278, 278, 556,
    278, 889, 611, 611, 611, 611, 389, 556, 333, 611, 556, 778, 556, 556, 500, 389, 280, 389, 584,
];
/// 1000 units/em, and 1pt == 1/72in == 0.352778mm.
const PT_TO_MM: f32 = 0.352778;

/// A non-ASCII/control character has no entry in the tables above (this
/// report's content is names/part numbers/URLs — effectively always
/// ASCII in practice); `556` is Helvetica's digit/most-common-glyph
/// width, a reasonable stand-in so one stray character doesn't throw
/// off wrapping.
fn char_width_mm(c: char, size_pt: f32, bold: bool) -> f32 {
    let code = c as u32;
    let units = if (32..=126).contains(&code) {
        let table = if bold {
            &HELVETICA_BOLD_WIDTHS
        } else {
            &HELVETICA_WIDTHS
        };
        table[(code - 32) as usize]
    } else {
        556
    };
    (units as f32 / 1000.0) * size_pt * PT_TO_MM
}

fn text_width_mm(text: &str, size_pt: f32, bold: bool) -> f32 {
    text.chars().map(|c| char_width_mm(c, size_pt, bold)).sum()
}

/// Breaks a single token with no wrap points of its own (a part number,
/// typically) across as many lines as it takes to fit `max_width_mm` —
/// purely character-greedy, since a bare digit/letter run has nothing
/// more natural to break on.
fn hard_break(word: &str, max_width_mm: f32, size_pt: f32, bold: bool) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut width = 0.0;
    for ch in word.chars() {
        let w = char_width_mm(ch, size_pt, bold);
        if !current.is_empty() && width + w > max_width_mm {
            lines.push(std::mem::take(&mut current));
            width = 0.0;
        }
        current.push(ch);
        width += w;
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

/// Greedy word-wrap of `text` to `max_width_mm`, falling back to
/// [`hard_break`] for any single word that alone exceeds the width
/// (e.g. an MPN with no spaces) — real word-wrap where there's
/// whitespace to break on, character-wrap only where there isn't.
/// Never drops or truncates anything; always returns at least one
/// (possibly empty) line.
fn wrap_text(text: &str, max_width_mm: f32, size_pt: f32, bold: bool) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let space_width = char_width_mm(' ', size_pt, bold);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0.0;

    for word in text.split(' ') {
        let word_width = text_width_mm(word, size_pt, bold);
        let with_sep = if current.is_empty() {
            word_width
        } else {
            current_width + space_width + word_width
        };
        if with_sep <= max_width_mm {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
            current_width = with_sep;
            continue;
        }

        if !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
        if word_width <= max_width_mm {
            current = word.to_string();
            current_width = word_width;
        } else {
            let mut broken = hard_break(word, max_width_mm, size_pt, bold);
            let last = broken.pop().unwrap_or_default();
            lines.extend(broken);
            current_width = text_width_mm(&last, size_pt, bold);
            current = last;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

struct Fonts {
    regular: printpdf::IndirectFontRef,
    bold: printpdf::IndirectFontRef,
}

fn fill_rect(
    layer: &PdfLayerReference,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    color: (f32, f32, f32),
) {
    layer.set_fill_color(text_color(color));
    layer.add_rect(
        Rect::new(Mm(x), Mm(y), Mm(x + width), Mm(y + height)).with_mode(PaintMode::Fill),
    );
}

/// Bundles a `draw_text` call's non-positional parameters — font size,
/// weight, and color — so the function itself stays under clippy's
/// too-many-arguments threshold instead of taking eight loose scalars.
#[derive(Clone, Copy)]
struct TextStyle {
    size: f32,
    bold: bool,
    color: (f32, f32, f32),
}

impl TextStyle {
    const fn new(size: f32, bold: bool, color: (f32, f32, f32)) -> Self {
        TextStyle { size, bold, color }
    }
}

/// Draws `text` at `(x, y)`, hard-clipped to `clip_width` mm — belt and
/// suspenders alongside `truncate`'s character-count trimming below:
/// Helvetica is proportional, so no fixed character budget is ever a
/// *guaranteed* fit (a digit-heavy string like an MPN can render
/// noticeably wider than a same-length word), and an underestimate used
/// to mean this cell's text visibly ran into the next column's. Clipping
/// makes that geometrically impossible — worst case, long text is cut
/// off flush at the column boundary instead of overlapping.
fn draw_text(
    layer: &PdfLayerReference,
    fonts: &Fonts,
    text: &str,
    x: f32,
    y: f32,
    clip_width: f32,
    style: TextStyle,
) {
    layer.save_graphics_state();
    layer.add_rect(
        Rect::new(Mm(x), Mm(y - 2.5), Mm(x + clip_width), Mm(y + 5.0)).with_mode(PaintMode::Clip),
    );
    layer.set_fill_color(text_color(style.color));
    let font = if style.bold {
        &fonts.bold
    } else {
        &fonts.regular
    };
    layer.use_text(text, style.size, Mm(x), Mm(y), font);
    layer.restore_graphics_state();
}

/// A column's usable width for [`draw_text`]'s clip, i.e. its full
/// width minus the `1.5mm` left inset every cell is drawn at.
fn column_width(columns: &[(&str, f32)], index: usize) -> f32 {
    columns[index].1 - 1.5
}

/// Draws the table's column header row at `y`, returning the y for the
/// first data row below it.
fn draw_column_header(
    layer: &PdfLayerReference,
    fonts: &Fonts,
    columns: &[(&str, f32)],
    y: f32,
) -> f32 {
    fill_rect(
        layer,
        MARGIN,
        y - ROW_HEIGHT + 2.0,
        PAGE_WIDTH - 2.0 * MARGIN,
        ROW_HEIGHT,
        HEADER_BG,
    );
    let style = TextStyle::new(HEADER_FONT_SIZE, true, WHITE);
    for (i, (label, _)) in columns.iter().enumerate() {
        draw_text(
            layer,
            fonts,
            label,
            column_x(columns, i) + 1.5,
            y - ROW_HEIGHT + 4.5,
            column_width(columns, i),
            style,
        );
    }
    y - ROW_HEIGHT
}

/// A row's total height for `n_lines` (the most any of its cells wraps
/// to) — `ROW_HEIGHT` covers the first line (plus top/bottom padding),
/// each additional line grows the row by exactly `LINE_STEP`.
fn row_height(n_lines: usize) -> f32 {
    ROW_HEIGHT + n_lines.saturating_sub(1) as f32 * LINE_STEP
}

/// One column's pre-wrapped lines plus the style to draw them in.
struct WrappedCell {
    lines: Vec<String>,
    style: TextStyle,
}

/// Word-wraps every cell of `row` against its own column's width and
/// works out the resulting row height — pulled apart from actually
/// drawing (`draw_wrapped_row`) so the pagination loop in `generate` can
/// ask "how tall will this row be" *before* committing to drawing it on
/// the current page.
fn wrap_row(row: &ReportRow) -> (Vec<WrappedCell>, f32) {
    let plain = TextStyle::new(ROW_FONT_SIZE, false, BLACK);
    let cell = |text: &str, col: usize, style: TextStyle| WrappedCell {
        lines: wrap_text(
            text,
            column_width(STOCK_COLUMNS, col),
            style.size,
            style.bold,
        ),
        style,
    };

    let mut cols = Vec::with_capacity(STOCK_COLUMNS.len());
    cols.push(cell(&row.reference, 0, plain));
    cols.push(cell(&row.symbol, 1, plain));

    match &row.outcome {
        Ok(info) => {
            cols.push(cell(&info.manufacturer, 2, plain));
            cols.push(cell(&info.mpn, 3, plain));

            let stock_text = info
                .offers
                .iter()
                .map(|o| format!("{}: {}", o.seller, o.stock_summary))
                .collect::<Vec<_>>()
                .join(" | ");
            cols.push(cell(&stock_text, 4, plain));

            let lifecycle_concern = info.lifecycle_concern();
            let lifecycle_text = info
                .offers
                .iter()
                .map(|o| o.lifecycle_summary.as_str())
                .collect::<Vec<_>>()
                .join(" | ");
            let lifecycle_style = TextStyle::new(
                ROW_FONT_SIZE,
                lifecycle_concern,
                if lifecycle_concern { RED } else { BLACK },
            );
            cols.push(cell(&lifecycle_text, 5, lifecycle_style));

            let (status, color) = if !info.in_stock() {
                ("NOT IN STOCK", RED)
            } else if lifecycle_concern {
                ("OBSOLETE/EOL", RED)
            } else {
                ("OK", GREEN)
            };
            cols.push(cell(status, 6, TextStyle::new(ROW_FONT_SIZE, true, color)));
        }
        Err(msg) => {
            cols.push(cell("-", 2, plain));
            cols.push(cell(msg, 3, TextStyle::new(ROW_FONT_SIZE, false, RED)));
            cols.push(cell("", 4, plain));
            cols.push(cell("", 5, plain));
            cols.push(cell(
                "LOOKUP FAILED",
                6,
                TextStyle::new(ROW_FONT_SIZE, true, RED),
            ));
        }
    }

    let n_lines = cols.iter().map(|c| c.lines.len()).max().unwrap_or(1).max(1);
    (cols, row_height(n_lines))
}

/// Bundles a table-drawing call's per-page/per-table constants (which
/// PDF layer, which fonts, which column layout) — same rationale as
/// `TextStyle` above: keeps `draw_wrapped_row` under clippy's
/// too-many-arguments threshold instead of taking three more loose
/// parameters on top of the ones that actually vary per row.
struct TableContext<'a> {
    layer: &'a PdfLayerReference,
    fonts: &'a Fonts,
    columns: &'a [(&'a str, f32)],
}

/// Draws one already-wrapped data row at `y` (the row's *top*, height
/// `height` as returned by `wrap_row`), returning the y for the next
/// row below it.
fn draw_wrapped_row(
    ctx: &TableContext,
    y: f32,
    height: f32,
    index: usize,
    needs_attention: bool,
    cols: &[WrappedCell],
) {
    let bg = if needs_attention {
        FLAGGED_BG
    } else if index % 2 == 1 {
        ZEBRA_BG
    } else {
        WHITE
    };
    fill_rect(
        ctx.layer,
        MARGIN,
        y - height + 2.0,
        PAGE_WIDTH - 2.0 * MARGIN,
        height,
        bg,
    );

    for (i, col) in cols.iter().enumerate() {
        let x = column_x(ctx.columns, i) + 1.5;
        let width = column_width(ctx.columns, i);
        for (line_idx, line) in col.lines.iter().enumerate() {
            let line_y = y - ROW_HEIGHT + 4.5 - line_idx as f32 * LINE_STEP;
            draw_text(ctx.layer, ctx.fonts, line, x, line_y, width, col.style);
        }
    }
}

/// Formats a Unix timestamp as `"YYYY-MM-DD HH:MM:SS UTC"` — a
/// dependency-free civil-calendar conversion (Howard Hinnant's
/// public-domain `civil_from_days` algorithm) rather than pulling in a
/// date/time crate for one display string on the report's title block.
/// Callers pass their own `SystemTime`-derived value; this function has
/// no wall-clock access of its own, which is what keeps it a pure,
/// easily-tested function.
pub fn format_utc_timestamp(unix_secs: u64) -> String {
    let unix_secs = unix_secs as i64;
    let days = unix_secs.div_euclid(86400);
    let secs_of_day = unix_secs.rem_euclid(86400);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day / 60) % 60;
    let second = secs_of_day % 60;

    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02} {hour:02}:{minute:02}:{second:02} UTC")
}

/// Renders `rows` as a PDF table to `out_path`. `project_name` and
/// `generated_at` (already formatted — this module has no opinion on
/// date/time formatting) are shown in the title block on the first page.
pub fn generate(
    rows: &[ReportRow],
    project_name: &str,
    generated_at: &str,
    out_path: &Path,
) -> Result<(), ReportError> {
    let (doc, page1, layer1) = printpdf::PdfDocument::new(
        "BOM Stock Report",
        Mm(PAGE_WIDTH),
        Mm(PAGE_HEIGHT),
        "Layer 1",
    );
    let fonts = Fonts {
        regular: doc.add_builtin_font(BuiltinFont::Helvetica)?,
        bold: doc.add_builtin_font(BuiltinFont::HelveticaBold)?,
    };

    let mut layer = doc.get_page(page1).get_layer(layer1);

    let not_in_stock = rows.iter().filter(|r| !r.in_stock()).count();
    let lifecycle_flagged = rows.iter().filter(|r| r.lifecycle_concern()).count();

    let full_width = PAGE_WIDTH - 2.0 * MARGIN;
    draw_text(
        &layer,
        &fonts,
        "BOM Stock Report",
        MARGIN,
        PAGE_HEIGHT - MARGIN,
        full_width,
        TextStyle::new(18.0, true, BLACK),
    );
    draw_text(
        &layer,
        &fonts,
        &format!("{project_name}  \u{2014}  generated {generated_at}"),
        MARGIN,
        PAGE_HEIGHT - MARGIN - 7.0,
        full_width,
        TextStyle::new(10.0, false, BLACK),
    );
    let summary = format!(
        "{not_in_stock} of {} part(s) NOT confirmed in stock  \u{2014}  {lifecycle_flagged} of {} flagged obsolete/EOL/NRND",
        rows.len(),
        rows.len()
    );
    draw_text(
        &layer,
        &fonts,
        &summary,
        MARGIN,
        PAGE_HEIGHT - MARGIN - 14.0,
        full_width,
        TextStyle::new(
            11.0,
            true,
            if not_in_stock > 0 || lifecycle_flagged > 0 {
                RED
            } else {
                GREEN
            },
        ),
    );

    let mut y = draw_column_header(&layer, &fonts, STOCK_COLUMNS, PAGE_HEIGHT - MARGIN - 22.0);

    for (i, row) in rows.iter().enumerate() {
        let (cols, height) = wrap_row(row);
        if y - height < MARGIN {
            let (page_idx, layer_idx) = doc.add_page(Mm(PAGE_WIDTH), Mm(PAGE_HEIGHT), "Layer 1");
            layer = doc.get_page(page_idx).get_layer(layer_idx);
            y = draw_column_header(&layer, &fonts, STOCK_COLUMNS, PAGE_HEIGHT - MARGIN);
        }
        let ctx = TableContext {
            layer: &layer,
            fonts: &fonts,
            columns: STOCK_COLUMNS,
        };
        draw_wrapped_row(&ctx, y, height, i, row.needs_attention(), &cols);
        y -= height;
    }

    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    doc.save(&mut BufWriter::new(File::create(out_path)?))?;
    Ok(())
}

fn priced_row_in_stock(row: &bom_pricing::PricedRow) -> bool {
    row.outcome.as_ref().map(|c| c.in_stock).unwrap_or(false)
}

fn priced_row_lifecycle_concern(row: &bom_pricing::PricedRow) -> bool {
    row.outcome
        .as_ref()
        .map(|c| c.lifecycle_concern)
        .unwrap_or(true)
}

/// True when the winning vendor's own on-hand quantity is less than the
/// quantity this row actually buys — distinct from `!priced_row_in_stock`
/// (which only catches *zero* stock): a vendor with 3 on hand still
/// counts as "in stock" but can't fulfill a purchase of 10.
fn priced_row_stock_shortfall(row: &bom_pricing::PricedRow) -> bool {
    row.outcome
        .as_ref()
        .map(|c| c.stock_quantity < u64::from(c.purchase_qty))
        .unwrap_or(false)
}

fn priced_row_needs_attention(row: &bom_pricing::PricedRow) -> bool {
    !priced_row_in_stock(row)
        || priced_row_lifecycle_concern(row)
        || priced_row_stock_shortfall(row)
}

/// The `Part` column's display text: the vendor-confirmed
/// manufacturer/MPN when a lookup succeeded (more precise/normalized
/// than what was merely searched for), falling back to the group's own
/// `display_name` (the MPN that was searched for, or `"<symbol>
/// <value>"`) when there's no chosen offer to draw from.
fn priced_part_label(row: &bom_pricing::PricedRow) -> String {
    match &row.outcome {
        Ok(chosen) if !chosen.mpn.is_empty() && !chosen.manufacturer.is_empty() => {
            format!("{} \u{2014} {}", chosen.manufacturer, chosen.mpn)
        }
        Ok(chosen) if !chosen.mpn.is_empty() => chosen.mpn.clone(),
        _ => row.group.display_name.clone(),
    }
}

fn wrap_priced_row(row: &bom_pricing::PricedRow) -> (Vec<WrappedCell>, f32) {
    let plain = TextStyle::new(ROW_FONT_SIZE, false, BLACK);
    let cell = |text: &str, col: usize, style: TextStyle| WrappedCell {
        lines: wrap_text(
            text,
            column_width(PRICED_COLUMNS, col),
            style.size,
            style.bold,
        ),
        style,
    };

    let mut cols = Vec::with_capacity(PRICED_COLUMNS.len());
    let references = row.group.references.join(", ");
    let part = priced_part_label(row);

    cols.push(cell(&part, 0, plain));
    cols.push(cell(&references, 1, plain));
    cols.push(cell(&row.needed_qty.to_string(), 2, plain));

    match &row.outcome {
        Ok(chosen) => {
            cols.push(cell(&chosen.purchase_qty.to_string(), 3, plain));
            cols.push(cell(&chosen.seller, 4, plain));
            cols.push(cell(&format!("${:.2}", chosen.unit_price), 5, plain));
            cols.push(cell(&format!("${:.2}", chosen.total_price), 6, plain));

            let mut flags: Vec<&str> = Vec::new();
            if !chosen.in_stock {
                flags.push("NOT IN STOCK");
            } else if chosen.stock_quantity < u64::from(chosen.purchase_qty) {
                flags.push("NOT ENOUGH STOCK");
            }
            if chosen.lifecycle_concern {
                flags.push("OBSOLETE/EOL");
            }
            let (flag, color) = if flags.is_empty() {
                ("OK".to_string(), GREEN)
            } else {
                (flags.join(" / "), RED)
            };
            cols.push(cell(&flag, 7, TextStyle::new(ROW_FONT_SIZE, true, color)));
        }
        Err(msg) => {
            cols.push(cell("-", 3, plain));
            cols.push(cell("-", 4, plain));
            cols.push(cell("-", 5, plain));
            cols.push(cell("-", 6, plain));
            cols.push(cell(
                &format!("LOOKUP FAILED: {msg}"),
                7,
                TextStyle::new(ROW_FONT_SIZE, true, RED),
            ));
        }
    }

    let n_lines = cols.iter().map(|c| c.lines.len()).max().unwrap_or(1).max(1);
    (cols, row_height(n_lines))
}

/// Renders a "Generate BOM" batch's grouped, priced results as a
/// paginated PDF table — the priced-BOM sibling of [`generate`], same
/// page/pagination/word-wrap machinery, different column layout
/// ([`PRICED_COLUMNS`]) and data source
/// (`bom_pricing::PricedRow`/`crate::bom_pricing`).
pub fn generate_priced_bom(
    rows: &[bom_pricing::PricedRow],
    project_name: &str,
    board_qty: u32,
    generated_at: &str,
    out_path: &Path,
) -> Result<(), ReportError> {
    let (doc, page1, layer1) =
        printpdf::PdfDocument::new("Priced BOM", Mm(PAGE_WIDTH), Mm(PAGE_HEIGHT), "Layer 1");
    let fonts = Fonts {
        regular: doc.add_builtin_font(BuiltinFont::Helvetica)?,
        bold: doc.add_builtin_font(BuiltinFont::HelveticaBold)?,
    };

    let mut layer = doc.get_page(page1).get_layer(layer1);

    let grand_total: f64 = rows
        .iter()
        .filter_map(|r| r.outcome.as_ref().ok())
        .map(|c| c.total_price)
        .sum();
    let failed = rows.iter().filter(|r| r.outcome.is_err()).count();

    let full_width = PAGE_WIDTH - 2.0 * MARGIN;
    draw_text(
        &layer,
        &fonts,
        "Priced BOM",
        MARGIN,
        PAGE_HEIGHT - MARGIN,
        full_width,
        TextStyle::new(18.0, true, BLACK),
    );
    draw_text(
        &layer,
        &fonts,
        &format!(
            "{project_name}  \u{2014}  {board_qty} board(s)  \u{2014}  generated {generated_at}"
        ),
        MARGIN,
        PAGE_HEIGHT - MARGIN - 7.0,
        full_width,
        TextStyle::new(10.0, false, BLACK),
    );
    let summary = if failed > 0 {
        format!(
            "Estimated total: ${grand_total:.2}  \u{2014}  {failed} of {} part(s) could not be priced",
            rows.len()
        )
    } else {
        format!("Estimated total: ${grand_total:.2}")
    };
    draw_text(
        &layer,
        &fonts,
        &summary,
        MARGIN,
        PAGE_HEIGHT - MARGIN - 14.0,
        full_width,
        TextStyle::new(11.0, true, if failed > 0 { RED } else { GREEN }),
    );

    let mut y = draw_column_header(&layer, &fonts, PRICED_COLUMNS, PAGE_HEIGHT - MARGIN - 22.0);

    for (i, row) in rows.iter().enumerate() {
        let (cols, height) = wrap_priced_row(row);
        if y - height < MARGIN {
            let (page_idx, layer_idx) = doc.add_page(Mm(PAGE_WIDTH), Mm(PAGE_HEIGHT), "Layer 1");
            layer = doc.get_page(page_idx).get_layer(layer_idx);
            y = draw_column_header(&layer, &fonts, PRICED_COLUMNS, PAGE_HEIGHT - MARGIN);
        }
        let ctx = TableContext {
            layer: &layer,
            fonts: &fonts,
            columns: PRICED_COLUMNS,
        };
        draw_wrapped_row(&ctx, y, height, i, priced_row_needs_attention(row), &cols);
        y -= height;
    }

    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    doc.save(&mut BufWriter::new(File::create(out_path)?))?;
    Ok(())
}

/// Writes the same rows [`generate_priced_bom`] renders as a PDF out as
/// a genuine `.xlsx` workbook instead of a CSV — for pasting into a
/// distributor's bulk-order tool or reviewing/filtering in a
/// spreadsheet, where a PDF table is inconvenient. An earlier version of
/// this used CSV, but CSV's column delimiter is also valid cell content:
/// a multi-reference group like `"R1, R5, R20"` can get re-split into
/// extra columns the moment a spreadsheet app's locale treats a
/// different character as the delimiter, corrupting every column after
/// it. A real spreadsheet format has no such ambiguity — each value is
/// its own cell regardless of what characters it contains — and, as a
/// bonus, can carry the same red-tinted "needs attention" row highlight
/// the PDF report already uses, which plain CSV has no way to express
/// at all.
///
/// One row per [`bom_pricing::PricedRow`] plus a trailing `Total` row; a
/// failed lookup still gets a row (with blank price cells and a
/// `LOOKUP FAILED` note) rather than silently vanishing from the BOM.
pub fn generate_priced_bom_xlsx(
    rows: &[bom_pricing::PricedRow],
    board_qty: u32,
    columns: &[crate::xlsx_columns::XlsxColumnKey],
    out_path: &Path,
) -> Result<(), ReportError> {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_worksheet();
    sheet.set_name("Priced BOM")?;

    let header_format = XlsxFormat::new()
        .set_bold()
        .set_font_color(XlsxColor::White)
        .set_background_color(XlsxColor::RGB(0x33_38_3F));
    let normal_format = XlsxFormat::new();
    let flagged_format = XlsxFormat::new().set_background_color(XlsxColor::RGB(0xFA_D9_D9));
    let money_format = XlsxFormat::new().set_num_format("$0.00");
    let flagged_money_format = XlsxFormat::new()
        .set_num_format("$0.00")
        .set_background_color(XlsxColor::RGB(0xFA_D9_D9));
    let bold_format = XlsxFormat::new().set_bold();
    let bold_money_format = XlsxFormat::new().set_bold().set_num_format("$0.00");

    for (col, xcol) in columns.iter().enumerate() {
        let label = match xcol {
            crate::xlsx_columns::XlsxColumnKey::Standard(c) => c.label(),
            crate::xlsx_columns::XlsxColumnKey::Custom(name) => name,
        };
        sheet.write_with_format(0, col as u16, label, &header_format)?;
    }

    let mut grand_total = 0.0f64;
    let mut row_idx: u32 = 1;
    for row in rows {
        let text_fmt = if priced_row_needs_attention(row) {
            &flagged_format
        } else {
            &normal_format
        };
        let money_fmt = if priced_row_needs_attention(row) {
            &flagged_money_format
        } else {
            &money_format
        };

        if let Ok(ch) = &row.outcome {
            grand_total += ch.total_price;
        }

        for (col, xcol) in columns.iter().enumerate() {
            let c = col as u16;
            match xcol {
                crate::xlsx_columns::XlsxColumnKey::Standard(col_type) => {
                    match col_type {
                        XlsxColumn::Part => {
                            let v = priced_part_label(row);
                            sheet.write_with_format(row_idx, c, &v, text_fmt)?;
                        }
                        XlsxColumn::References => {
                            let v = row.group.references.join(", ");
                            sheet.write_with_format(row_idx, c, &v, text_fmt)?;
                        }
                        XlsxColumn::NeededQty => {
                            sheet.write_with_format(row_idx, c, row.needed_qty, text_fmt)?;
                        }
                        XlsxColumn::PurchaseQty => {
                            if let Ok(ch) = &row.outcome {
                                sheet.write_with_format(row_idx, c, ch.purchase_qty, text_fmt)?;
                            }
                        }
                        XlsxColumn::Vendor => {
                            if let Ok(ch) = &row.outcome {
                                sheet.write_with_format(row_idx, c, &ch.seller, text_fmt)?;
                            }
                        }
                        XlsxColumn::UnitPrice => {
                            if let Ok(ch) = &row.outcome {
                                sheet.write_with_format(row_idx, c, ch.unit_price, money_fmt)?;
                            }
                        }
                        XlsxColumn::TotalPrice => {
                            if let Ok(ch) = &row.outcome {
                                sheet.write_with_format(row_idx, c, ch.total_price, money_fmt)?;
                            }
                        }
                        XlsxColumn::InStock => {
                            if let Ok(ch) = &row.outcome {
                                sheet.write_with_format(row_idx, c, ch.in_stock, text_fmt)?;
                            }
                        }
                        XlsxColumn::StockQty => {
                            if let Ok(ch) = &row.outcome {
                                sheet.write_with_format(row_idx, c, ch.stock_quantity, text_fmt)?;
                            }
                        }
                        XlsxColumn::StockShortfall => {
                            if let Ok(ch) = &row.outcome {
                                let sf = ch.stock_quantity < u64::from(ch.purchase_qty);
                                sheet.write_with_format(row_idx, c, sf, text_fmt)?;
                            }
                        }
                        XlsxColumn::LifecycleConcern => match &row.outcome {
                            Ok(ch) => {
                                sheet.write_with_format(row_idx, c, ch.lifecycle_concern, text_fmt)?;
                            }
                            Err(msg) => {
                                sheet.write_with_format(
                                    row_idx,
                                    c,
                                    format!("LOOKUP FAILED: {msg}"),
                                    text_fmt,
                                )?;
                            }
                        },
                    }
                }
                crate::xlsx_columns::XlsxColumnKey::Custom(field_name) => {
                    let value = row.group.custom_fields.get(field_name).cloned().unwrap_or_default();
                    sheet.write_with_format(row_idx, c, &value, text_fmt)?;
                }
            }
        }
        row_idx += 1;
    }

    sheet.write_with_format(row_idx, 0, "Total", &bold_format)?;
    if let Some(total_col) = columns.iter().position(|c| {
        matches!(c, crate::xlsx_columns::XlsxColumnKey::Standard(XlsxColumn::TotalPrice))
    }) {
        sheet.write_with_format(row_idx, total_col as u16, grand_total, &bold_money_format)?;
    }
    row_idx += 1;
    sheet.write_with_format(row_idx, 0, "Board quantity", &bold_format)?;
    sheet.write_with_format(row_idx, 1, board_qty, &normal_format)?;

    sheet.autofit();

    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    workbook.save(out_path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parts_lookup::{PartInfo, StockStatus, VendorOffer};
    use tempfile::tempdir;

    fn in_stock_info() -> PartInfo {
        PartInfo {
            manufacturer: "Texas Instruments".to_string(),
            mpn: "LM358P".to_string(),
            description: String::new(),
            offers: vec![VendorOffer {
                seller: "Mouser".to_string(),
                url: String::new(),
                sku: String::new(),
                price_summary: String::new(),
                stock_status: StockStatus::InStock,
                stock_summary: "1,000 In Stock".to_string(),
                stock_quantity: 1000,
                lifecycle_summary: "Active".to_string(),
                lifecycle_concern: false,
                suggested_replacement: String::new(),
                price_breaks: Vec::new(),
            }],
            warnings: Vec::new(),
        }
    }

    fn out_of_stock_info() -> PartInfo {
        PartInfo {
            manufacturer: "YAGEO".to_string(),
            mpn: "AC0402FR-072K7L".to_string(),
            description: String::new(),
            offers: vec![VendorOffer {
                seller: "DigiKey".to_string(),
                url: String::new(),
                sku: String::new(),
                price_summary: String::new(),
                stock_status: StockStatus::OutOfStock,
                stock_summary: "0 in stock".to_string(),
                stock_quantity: 0,
                lifecycle_summary: "Unknown".to_string(),
                lifecycle_concern: false,
                suggested_replacement: String::new(),
                price_breaks: Vec::new(),
            }],
            warnings: Vec::new(),
        }
    }

    fn obsolete_info() -> PartInfo {
        PartInfo {
            manufacturer: "Texas Instruments".to_string(),
            mpn: "LM117HVH".to_string(),
            description: String::new(),
            offers: vec![VendorOffer {
                seller: "Mouser".to_string(),
                url: String::new(),
                sku: String::new(),
                price_summary: String::new(),
                stock_status: StockStatus::InStock,
                stock_summary: "12 In Stock".to_string(),
                stock_quantity: 12,
                lifecycle_summary: "Obsolete".to_string(),
                lifecycle_concern: true,
                suggested_replacement: String::new(),
                price_breaks: Vec::new(),
            }],
            warnings: Vec::new(),
        }
    }

    #[test]
    fn formats_utc_timestamp() {
        assert_eq!(format_utc_timestamp(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(
            format_utc_timestamp(1_700_000_000),
            "2023-11-14 22:13:20 UTC"
        );
        assert_eq!(
            format_utc_timestamp(1_732_665_600),
            "2024-11-27 00:00:00 UTC"
        );
        assert_eq!(
            format_utc_timestamp(1_753_646_700),
            "2025-07-27 20:05:00 UTC"
        );
    }

    #[test]
    fn column_widths_sum_to_table_width() {
        let total: f32 = STOCK_COLUMNS.iter().map(|(_, w)| w).sum();
        assert!((total - (PAGE_WIDTH - 2.0 * MARGIN)).abs() < 0.01);
    }

    #[test]
    fn priced_column_widths_sum_to_table_width() {
        let total: f32 = PRICED_COLUMNS.iter().map(|(_, w)| w).sum();
        assert!((total - (PAGE_WIDTH - 2.0 * MARGIN)).abs() < 0.01);
    }

    // ── text measurement & word-wrap ─────────────────────────────────

    #[test]
    fn width_tables_cover_the_full_ascii_printable_range() {
        assert_eq!(HELVETICA_WIDTHS.len(), 95);
        assert_eq!(HELVETICA_BOLD_WIDTHS.len(), 95);
    }

    #[test]
    fn wider_characters_measure_wider_than_narrower_ones() {
        // 'i' (narrow) vs 'M' (wide) at the same size/weight.
        assert!(char_width_mm('i', 9.0, false) < char_width_mm('M', 9.0, false));
    }

    #[test]
    fn bold_measures_at_least_as_wide_as_regular() {
        for c in ['R', '1', ' '] {
            assert!(char_width_mm(c, 9.0, true) >= char_width_mm(c, 9.0, false));
        }
    }

    #[test]
    fn wrap_text_never_drops_content_short_text_stays_one_line() {
        let lines = wrap_text("R1", 50.0, 9.0, false);
        assert_eq!(lines, vec!["R1".to_string()]);
    }

    #[test]
    fn wrap_text_never_exceeds_the_given_width() {
        let text = "Samsung Electro-Mechanics CL21B224KBFVPNE some more trailing words here";
        let max_width = 30.0;
        let lines = wrap_text(text, max_width, 9.0, false);
        assert!(lines.len() > 1, "expected wrapping across multiple lines");
        for line in &lines {
            assert!(
                text_width_mm(line, 9.0, false) <= max_width + 0.01,
                "line '{line}' exceeds {max_width}mm"
            );
        }
        // Every word survives somewhere, in original order, nothing
        // dropped or ellipsized.
        assert_eq!(lines.join(" "), text);
    }

    #[test]
    fn wrap_text_hard_breaks_a_single_token_wider_than_the_column() {
        // No spaces at all — must still split across lines rather than
        // overflowing or silently truncating.
        let text = "MMASU105SB5104KFNA01";
        let max_width = 15.0;
        let lines = wrap_text(text, max_width, 9.0, false);
        assert!(lines.len() > 1);
        for line in &lines {
            assert!(text_width_mm(line, 9.0, false) <= max_width + 0.01);
        }
        assert_eq!(lines.concat(), text);
    }

    #[test]
    fn wrap_text_of_empty_string_is_one_empty_line() {
        assert_eq!(wrap_text("", 50.0, 9.0, false), vec![String::new()]);
    }

    #[test]
    fn row_height_grows_by_line_step_per_extra_line() {
        assert_eq!(row_height(1), ROW_HEIGHT);
        assert_eq!(row_height(2), ROW_HEIGHT + LINE_STEP);
        assert_eq!(row_height(3), ROW_HEIGHT + 2.0 * LINE_STEP);
    }

    #[test]
    fn generates_a_nonempty_pdf_with_mixed_rows() {
        let dir = tempdir().unwrap();
        let out_path = dir.path().join("report.pdf");
        let rows = vec![
            ReportRow {
                reference: "R1".to_string(),
                symbol: "R".to_string(),
                outcome: Ok(in_stock_info()),
            },
            ReportRow {
                reference: "R2".to_string(),
                symbol: "R".to_string(),
                outcome: Ok(out_of_stock_info()),
            },
            ReportRow {
                reference: "U9".to_string(),
                symbol: "LM117HVH".to_string(),
                outcome: Ok(obsolete_info()),
            },
            ReportRow {
                reference: "U1".to_string(),
                symbol: "LM358".to_string(),
                outcome: Err("no match found for 'LM358'".to_string()),
            },
        ];

        generate(&rows, "demo-project", "2026-07-27 12:00", &out_path).unwrap();

        let bytes = std::fs::read(&out_path).unwrap();
        assert!(bytes.starts_with(b"%PDF"));
        assert!(bytes.len() > 500);
    }

    // ── lifecycle concern ────────────────────────────────────────────

    #[test]
    fn row_needs_attention_when_obsolete_even_though_in_stock() {
        let row = ReportRow {
            reference: "U9".to_string(),
            symbol: "LM117HVH".to_string(),
            outcome: Ok(obsolete_info()),
        };
        assert!(row.in_stock());
        assert!(row.lifecycle_concern());
        assert!(row.needs_attention());
    }

    #[test]
    fn row_does_not_need_attention_when_active_and_in_stock() {
        let row = ReportRow {
            reference: "R1".to_string(),
            symbol: "R".to_string(),
            outcome: Ok(in_stock_info()),
        };
        assert!(!row.needs_attention());
    }

    #[test]
    fn failed_lookup_counts_as_a_lifecycle_concern_too() {
        let row = ReportRow {
            reference: "U1".to_string(),
            symbol: "LM358".to_string(),
            outcome: Err("no match found".to_string()),
        };
        assert!(row.lifecycle_concern());
        assert!(row.needs_attention());
    }

    #[test]
    fn paginates_when_rows_exceed_one_page() {
        let dir = tempdir().unwrap();
        let out_path = dir.path().join("big_report.pdf");
        let rows: Vec<ReportRow> = (0..200)
            .map(|i| ReportRow {
                reference: format!("R{i}"),
                symbol: "R".to_string(),
                outcome: Ok(in_stock_info()),
            })
            .collect();

        generate(&rows, "demo-project", "2026-07-27 12:00", &out_path).unwrap();
        let bytes = std::fs::read(&out_path).unwrap();
        assert!(bytes.starts_with(b"%PDF"));
    }

    #[test]
    fn creates_missing_parent_directories() {
        let dir = tempdir().unwrap();
        let out_path = dir.path().join("nested").join("dir").join("report.pdf");
        generate(&[], "demo-project", "2026-07-27 12:00", &out_path).unwrap();
        assert!(out_path.is_file());
    }

    // ── priced BOM (PDF + CSV) ────────────────────────────────────────

    fn priced_group(display_name: &str, references: &[&str]) -> bom_pricing::PartGroup {
        bom_pricing::PartGroup {
            group_key: display_name.to_string(),
            references: references.iter().map(|r| r.to_string()).collect(),
            search_mpn: display_name.to_string(),
            display_name: display_name.to_string(),
            is_passive: false,
            per_board_qty: references.len() as u32,
            instances: Vec::new(),
            custom_fields: Default::default(),
        }
    }

    fn priced_ok_row(display_name: &str, references: &[&str]) -> bom_pricing::PricedRow {
        bom_pricing::PricedRow {
            group: priced_group(display_name, references),
            needed_qty: references.len() as u32,
            outcome: Ok(bom_pricing::ChosenOffer {
                seller: "Mouser".to_string(),
                manufacturer: "Texas Instruments".to_string(),
                mpn: display_name.to_string(),
                sku: "595-XYZ".to_string(),
                purchase_qty: references.len() as u32,
                unit_price: 0.10,
                total_price: 0.10 * references.len() as f64,
                in_stock: true,
                stock_quantity: 10_000,
                lifecycle_concern: false,
            }),
        }
    }

    fn priced_failed_row(display_name: &str, references: &[&str]) -> bom_pricing::PricedRow {
        bom_pricing::PricedRow {
            group: priced_group(display_name, references),
            needed_qty: references.len() as u32,
            outcome: Err("no match found".to_string()),
        }
    }

    #[test]
    fn generates_a_nonempty_priced_pdf() {
        let dir = tempdir().unwrap();
        let out_path = dir.path().join("priced.pdf");
        let rows = vec![
            priced_ok_row("LM358P", &["U1", "U2"]),
            priced_failed_row("UNKNOWN123", &["U9"]),
        ];
        generate_priced_bom(&rows, "demo-project", 5, "2026-07-27 12:00", &out_path).unwrap();

        let bytes = std::fs::read(&out_path).unwrap();
        assert!(bytes.starts_with(b"%PDF"));
        assert!(bytes.len() > 500);
    }

    #[test]
    fn priced_pdf_creates_missing_parent_directories() {
        let dir = tempdir().unwrap();
        let out_path = dir.path().join("nested").join("dir").join("priced.pdf");
        generate_priced_bom(&[], "demo-project", 1, "2026-07-27 12:00", &out_path).unwrap();
        assert!(out_path.is_file());
    }

    fn open_priced_sheet(path: &Path) -> calamine::Range<calamine::Data> {
        use calamine::Reader;
        let mut workbook: calamine::Xlsx<_> = calamine::open_workbook(path).unwrap();
        workbook.worksheet_range("Priced BOM").unwrap()
    }

    #[test]
    fn generates_an_xlsx_with_a_correct_grand_total() {
        use calamine::DataType;

        let dir = tempdir().unwrap();
        let out_path = dir.path().join("priced.xlsx");
        let rows = vec![
            priced_ok_row("LM358P", &["U1", "U2"]), // 2 * $0.10 = $0.20
            priced_ok_row("RC0603FR", &["R1"]),     // 1 * $0.10 = $0.10
            priced_failed_row("UNKNOWN123", &["U9"]),
        ];
        let cols: Vec<_> = XlsxColumn::ALL
            .iter()
            .map(|&c| crate::xlsx_columns::XlsxColumnKey::Standard(c))
            .collect();
        generate_priced_bom_xlsx(&rows, 5, &cols, &out_path).unwrap();

        let range = open_priced_sheet(&out_path);
        assert_eq!(range.get_value((0, 0)).unwrap().get_string(), Some("Part"));
        assert_eq!(
            range.get_value((0, 9)).unwrap().get_string(),
            Some("Stock Shortfall")
        );
        // Row 4 (0-indexed) is the Total row: header + 3 data rows.
        assert_eq!(range.get_value((4, 0)).unwrap().get_string(), Some("Total"));
        assert!((range.get_value((4, 6)).unwrap().get_float().unwrap() - 0.30).abs() < 1e-9);
        assert_eq!(
            range.get_value((5, 0)).unwrap().get_string(),
            Some("Board quantity")
        );
        assert_eq!(range.get_value((5, 1)).unwrap().get_float(), Some(5.0));
        assert_eq!(
            range.get_value((3, 10)).unwrap().get_string(),
            Some("LOOKUP FAILED: no match found")
        );
    }

    #[test]
    fn xlsx_records_a_stock_shortfall() {
        use calamine::DataType;

        let dir = tempdir().unwrap();
        let out_path = dir.path().join("priced.xlsx");
        let mut row = priced_ok_row("LM358P", &["U1"]);
        if let Ok(chosen) = &mut row.outcome {
            chosen.purchase_qty = 10;
            chosen.stock_quantity = 3;
        }
        let cols: Vec<_> = XlsxColumn::ALL
            .iter()
            .map(|&c| crate::xlsx_columns::XlsxColumnKey::Standard(c))
            .collect();
        generate_priced_bom_xlsx(&[row], 1, &cols, &out_path).unwrap();

        let range = open_priced_sheet(&out_path);
        assert_eq!(range.get_value((1, 8)).unwrap().get_float(), Some(3.0));
        assert_eq!(range.get_value((1, 9)).unwrap().get_bool(), Some(true));
    }

    #[test]
    fn xlsx_creates_missing_parent_directories() {
        let dir = tempdir().unwrap();
        let out_path = dir.path().join("nested").join("dir").join("priced.xlsx");
        let cols: Vec<_> = XlsxColumn::ALL
            .iter()
            .map(|&c| crate::xlsx_columns::XlsxColumnKey::Standard(c))
            .collect();
        generate_priced_bom_xlsx(&[], 1, &cols, &out_path).unwrap();
        assert!(out_path.is_file());
    }
}
