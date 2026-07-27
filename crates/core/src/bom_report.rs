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

use crate::parts_lookup::PartInfo;

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
}

const PAGE_WIDTH: f32 = 297.0;
const PAGE_HEIGHT: f32 = 210.0;
const MARGIN: f32 = 15.0;
const ROW_HEIGHT: f32 = 7.0;
const ROW_FONT_SIZE: f32 = 9.0;
const HEADER_FONT_SIZE: f32 = 9.0;
/// Columns as (label, width in mm) — widths sum to exactly
/// `PAGE_WIDTH - 2 * MARGIN` (267mm) so the table's right edge lands on
/// the right margin.
const COLUMNS: &[(&str, f32)] = &[
    ("Reference", 16.0),
    ("Symbol", 30.0),
    ("Manufacturer", 42.0),
    ("MPN", 36.0),
    ("Stock", 55.0),
    ("Lifecycle", 40.0),
    ("Status", 48.0),
];

fn column_x(index: usize) -> f32 {
    MARGIN + COLUMNS[..index].iter().map(|(_, w)| w).sum::<f32>()
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

/// Truncates to at most `max_chars`, appending `"..."` when it does —
/// plain ASCII ellipsis rather than the Unicode `…` glyph, to stay
/// safely inside the built-in fonts' WinAnsi-only coverage.
fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let keep = max_chars.saturating_sub(3);
    let mut out: String = s.chars().take(keep).collect();
    out.push_str("...");
    out
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

fn draw_text(
    layer: &PdfLayerReference,
    fonts: &Fonts,
    text: &str,
    x: f32,
    y: f32,
    style: TextStyle,
) {
    layer.set_fill_color(text_color(style.color));
    let font = if style.bold {
        &fonts.bold
    } else {
        &fonts.regular
    };
    layer.use_text(text, style.size, Mm(x), Mm(y), font);
}

/// Draws the table's column header row at `y`, returning the y for the
/// first data row below it.
fn draw_column_header(layer: &PdfLayerReference, fonts: &Fonts, y: f32) -> f32 {
    fill_rect(
        layer,
        MARGIN,
        y - ROW_HEIGHT + 2.0,
        PAGE_WIDTH - 2.0 * MARGIN,
        ROW_HEIGHT,
        HEADER_BG,
    );
    let style = TextStyle::new(HEADER_FONT_SIZE, true, WHITE);
    for (i, (label, _)) in COLUMNS.iter().enumerate() {
        draw_text(
            layer,
            fonts,
            label,
            column_x(i) + 1.5,
            y - ROW_HEIGHT + 4.5,
            style,
        );
    }
    y - ROW_HEIGHT
}

/// Draws one data row at `y` (the row's *top*), returning the y for the
/// next row below it.
fn draw_row(layer: &PdfLayerReference, fonts: &Fonts, y: f32, index: usize, row: &ReportRow) {
    let in_stock = row.in_stock();
    let bg = if row.needs_attention() {
        FLAGGED_BG
    } else if index % 2 == 1 {
        ZEBRA_BG
    } else {
        WHITE
    };
    fill_rect(
        layer,
        MARGIN,
        y - ROW_HEIGHT + 2.0,
        PAGE_WIDTH - 2.0 * MARGIN,
        ROW_HEIGHT,
        bg,
    );

    let text_y = y - ROW_HEIGHT + 4.5;
    let plain = TextStyle::new(ROW_FONT_SIZE, false, BLACK);
    draw_text(
        layer,
        fonts,
        &row.reference,
        column_x(0) + 1.5,
        text_y,
        plain,
    );
    draw_text(
        layer,
        fonts,
        &truncate(&row.symbol, 24),
        column_x(1) + 1.5,
        text_y,
        plain,
    );

    match &row.outcome {
        Ok(info) => {
            draw_text(
                layer,
                fonts,
                &truncate(&info.manufacturer, 30),
                column_x(2) + 1.5,
                text_y,
                plain,
            );
            draw_text(
                layer,
                fonts,
                &truncate(&info.mpn, 26),
                column_x(3) + 1.5,
                text_y,
                plain,
            );
            let stock_text = info
                .offers
                .iter()
                .map(|o| format!("{}: {}", o.seller, o.stock_summary))
                .collect::<Vec<_>>()
                .join(" | ");
            draw_text(
                layer,
                fonts,
                &truncate(&stock_text, 32),
                column_x(4) + 1.5,
                text_y,
                plain,
            );
            let lifecycle_concern = info.lifecycle_concern();
            let lifecycle_text = info
                .offers
                .iter()
                .map(|o| o.lifecycle_summary.as_str())
                .collect::<Vec<_>>()
                .join(" | ");
            draw_text(
                layer,
                fonts,
                &truncate(&lifecycle_text, 24),
                column_x(5) + 1.5,
                text_y,
                TextStyle::new(
                    ROW_FONT_SIZE,
                    lifecycle_concern,
                    if lifecycle_concern { RED } else { BLACK },
                ),
            );
            let (status, color) = if !in_stock {
                ("NOT IN STOCK", RED)
            } else if lifecycle_concern {
                ("OBSOLETE/EOL", RED)
            } else {
                ("OK", GREEN)
            };
            draw_text(
                layer,
                fonts,
                status,
                column_x(6) + 1.5,
                text_y,
                TextStyle::new(ROW_FONT_SIZE, true, color),
            );
        }
        Err(msg) => {
            draw_text(layer, fonts, "-", column_x(2) + 1.5, text_y, plain);
            draw_text(
                layer,
                fonts,
                &truncate(msg, 20),
                column_x(3) + 1.5,
                text_y,
                TextStyle::new(ROW_FONT_SIZE, false, RED),
            );
            draw_text(layer, fonts, "", column_x(4) + 1.5, text_y, plain);
            draw_text(layer, fonts, "", column_x(5) + 1.5, text_y, plain);
            draw_text(
                layer,
                fonts,
                "LOOKUP FAILED",
                column_x(6) + 1.5,
                text_y,
                TextStyle::new(ROW_FONT_SIZE, true, RED),
            );
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

    draw_text(
        &layer,
        &fonts,
        "BOM Stock Report",
        MARGIN,
        PAGE_HEIGHT - MARGIN,
        TextStyle::new(18.0, true, BLACK),
    );
    draw_text(
        &layer,
        &fonts,
        &format!("{project_name}  \u{2014}  generated {generated_at}"),
        MARGIN,
        PAGE_HEIGHT - MARGIN - 7.0,
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

    let mut y = draw_column_header(&layer, &fonts, PAGE_HEIGHT - MARGIN - 22.0);

    for (i, row) in rows.iter().enumerate() {
        if y - ROW_HEIGHT < MARGIN {
            let (page_idx, layer_idx) = doc.add_page(Mm(PAGE_WIDTH), Mm(PAGE_HEIGHT), "Layer 1");
            layer = doc.get_page(page_idx).get_layer(layer_idx);
            y = draw_column_header(&layer, &fonts, PAGE_HEIGHT - MARGIN);
        }
        draw_row(&layer, &fonts, y, i, row);
        y -= ROW_HEIGHT;
    }

    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    doc.save(&mut BufWriter::new(File::create(out_path)?))?;
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
            offers: vec![VendorOffer {
                seller: "Mouser".to_string(),
                url: String::new(),
                sku: String::new(),
                price_summary: String::new(),
                stock_status: StockStatus::InStock,
                stock_summary: "1,000 In Stock".to_string(),
                lifecycle_summary: "Active".to_string(),
                lifecycle_concern: false,
                suggested_replacement: String::new(),
            }],
            warnings: Vec::new(),
        }
    }

    fn out_of_stock_info() -> PartInfo {
        PartInfo {
            manufacturer: "YAGEO".to_string(),
            mpn: "AC0402FR-072K7L".to_string(),
            offers: vec![VendorOffer {
                seller: "DigiKey".to_string(),
                url: String::new(),
                sku: String::new(),
                price_summary: String::new(),
                stock_status: StockStatus::OutOfStock,
                stock_summary: "0 in stock".to_string(),
                lifecycle_summary: "Unknown".to_string(),
                lifecycle_concern: false,
                suggested_replacement: String::new(),
            }],
            warnings: Vec::new(),
        }
    }

    fn obsolete_info() -> PartInfo {
        PartInfo {
            manufacturer: "Texas Instruments".to_string(),
            mpn: "LM117HVH".to_string(),
            offers: vec![VendorOffer {
                seller: "Mouser".to_string(),
                url: String::new(),
                sku: String::new(),
                price_summary: String::new(),
                stock_status: StockStatus::InStock,
                stock_summary: "12 In Stock".to_string(),
                lifecycle_summary: "Obsolete".to_string(),
                lifecycle_concern: true,
                suggested_replacement: String::new(),
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
        let total: f32 = COLUMNS.iter().map(|(_, w)| w).sum();
        assert!((total - (PAGE_WIDTH - 2.0 * MARGIN)).abs() < 0.01);
    }

    #[test]
    fn truncate_leaves_short_strings_untouched() {
        assert_eq!(truncate("R1", 10), "R1");
    }

    #[test]
    fn truncate_shortens_and_marks_long_strings() {
        let out = truncate("a very long manufacturer name indeed", 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.ends_with("..."));
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
}
