//! "Populate BOM" — annotate every symbol actually *placed* on the
//! current project's schematic (root sheet plus every hierarchical
//! sub-sheet, deduplicated by reference designator) with manufacturer
//! and Mouser/DigiKey distributor info (see
//! `kicad_auto_importer_core::parts_lookup`).
//!
//! Row discovery is `kicad_auto_importer_core::schematic`'s job — see
//! that module's docs for why this covers the *whole* schematic (not
//! just symbols defined in the project's own local libraries the way an
//! earlier version of this feature did) and, just as importantly, why
//! looked-up vendor info is written back onto each placed *instance*
//! (keyed by its schematic uuid) rather than into the shared library
//! symbol it came from: a generic `Device:R` is reused by every
//! resistor in the design regardless of value, so patching the library
//! symbol would clobber it for all of them (and for a global/stock
//! library, would corrupt a file KiCad shares across every project on
//! the machine).
//!
//! Structurally still a near-copy of `library_import_ui.rs`: a genuine
//! second OS window (`show_viewport_immediate`, not a floating
//! `egui::Window` — see that file's docs for why), the same
//! table/checkbox/log-pane layout, including its wrapped multi-line
//! table rows. Since rows can come from more than one schematic file
//! (root plus sub-sheets), `run_lookup_batch` groups selected rows by
//! their own `sch_path` and opens/patches/saves each schematic file
//! once, rather than assuming everything lives in one file.
//!
//! The lookups themselves run on a plain background `std::thread` (this
//! app's established pattern for anything that shouldn't block the GUI
//! thread — see `single_instance.rs`, `tray.rs`), not async: a handful
//! of on-demand HTTP requests triggered by one button click don't
//! benefit from an async runtime the way the folder watcher's many
//! concurrent, long-lived settle-waits did.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

use egui::{Color32, RichText};
use egui_extras::{Column, TableBuilder};
use egui_phosphor::regular as icon;

use kicad_auto_importer_core::bom_report::{self, ReportRow};
use kicad_auto_importer_core::parts_lookup::{self, PartsCredentials};
use kicad_auto_importer_core::schematic::{self, PlacedSymbol, SchematicFile};
use kicad_auto_importer_core::sexp::SexpNode;
use kicad_auto_importer_core::symbol_importer::{get_symbol_property, set_symbol_property};

/// Below this age, a part's `Last Checked` property is considered fresh
/// enough to skip re-querying Mouser/DigiKey for — see `run_lookup_batch`.
/// Force-re-check bypasses this from the UI.
const RECHECK_THRESHOLD: chrono::Duration = chrono::Duration::hours(24);
const LAST_CHECKED_PROPERTY: &str = "Last Checked";

use crate::theme::{self, ACCENT, DANGER, OK};

enum LookupEvent {
    Log(String),
    /// Fires once a row starts being processed (before the staleness
    /// check, so it fires even for a row that ends up skipped) — drives
    /// the "currently looking up…" label next to the progress bar.
    CurrentItem(String),
    RowResult {
        index: usize,
        ok: bool,
        needs_attention: bool,
        skipped: bool,
        summary: String,
    },
    Done,
}

/// What a single row's last lookup found — shown in the table's Result
/// column and kept until the next reload or batch. `needs_attention` is
/// only meaningful when `ok` is true (a found part that's either not
/// confirmed in stock or flagged obsolete/EOL/NRND by some vendor); a
/// failed lookup is already flagged by `ok` alone. `skipped` means the
/// row was left untouched because it was already checked within
/// `RECHECK_THRESHOLD` and Force wasn't on — `ok`/`needs_attention` are
/// meaningless in that case (always `true`/`false`).
struct RowResult {
    ok: bool,
    needs_attention: bool,
    skipped: bool,
    summary: String,
}

/// A checked row, snapshotted at the moment "Populate BOM" is clicked —
/// owns its own `sch_path`/`uuid` (from `PlacedSymbol`) since rows can
/// come from any schematic file in the hierarchy, not just the root one.
struct SelectedRow {
    index: usize,
    reference: String,
    lib_id: String,
    sch_path: PathBuf,
    uuid: String,
}

#[derive(Default)]
pub struct PartLookupState {
    pub open: bool,
    rows: Vec<PlacedSymbol>,
    /// The project dir `rows` was last loaded from — reload
    /// automatically if the window is (re)opened pointing somewhere
    /// else, e.g. after switching projects.
    loaded_from: Option<PathBuf>,
    checked: HashSet<usize>,
    /// See `library_import_ui::LibraryImportState::last_clicked` — same
    /// shift-click range-select behavior.
    last_clicked: Option<usize>,
    log_lines: Vec<String>,
    status: String,
    rx: Option<mpsc::Receiver<LookupEvent>>,
    in_progress: bool,
    /// Per-row outcome of the most recent batch that covered it, keyed
    /// by index into `rows`. Cleared on reload (indices become
    /// meaningless against a freshly loaded row list) and at the start
    /// of each new batch.
    results: HashMap<usize, RowResult>,
    progress_done: usize,
    progress_total: usize,
    /// The row currently being looked up — see `LookupEvent::CurrentItem`.
    current_item: String,
    /// Bypasses the 24h "checked recently, skip it" gate — see
    /// `run_lookup_batch`. Opt-in (defaults off) since the whole point
    /// of the gate is to avoid hammering Mouser/DigiKey on every run.
    force_recheck: bool,
}

impl PartLookupState {
    fn log(&mut self, msg: impl Into<String>) {
        self.log_lines.push(msg.into());
    }

    fn load_from(&mut self, project_dir: &Path) {
        let mut lines = Vec::new();
        self.rows = schematic::load_schematic_symbols(project_dir, |m| lines.push(m.to_string()));
        for line in lines {
            self.log(line);
        }
        self.checked.clear();
        self.last_clicked = None;
        self.results.clear();
        self.progress_done = 0;
        self.progress_total = 0;
        self.current_item.clear();
        self.loaded_from = Some(project_dir.to_path_buf());
        self.log(format!(
            "Found {} symbol(s) placed on the schematic.",
            self.rows.len()
        ));
    }

    fn handle_row_click(&mut self, i: usize, shift: bool) {
        if shift {
            let anchor = self.last_clicked.unwrap_or(i);
            let (lo, hi) = if anchor <= i {
                (anchor, i)
            } else {
                (i, anchor)
            };
            for idx in lo..=hi {
                self.checked.insert(idx);
            }
        } else {
            if self.checked.contains(&i) {
                self.checked.remove(&i);
            } else {
                self.checked.insert(i);
            }
            self.last_clicked = Some(i);
        }
    }

    fn drain_channel(&mut self) {
        let mut lines = Vec::new();
        let mut results = Vec::new();
        let mut done = false;
        if let Some(rx) = &self.rx {
            while let Ok(event) = rx.try_recv() {
                match event {
                    LookupEvent::Log(msg) => lines.push(msg),
                    LookupEvent::CurrentItem(item) => self.current_item = item,
                    LookupEvent::RowResult {
                        index,
                        ok,
                        needs_attention,
                        skipped,
                        summary,
                    } => {
                        results.push((index, ok, needs_attention, skipped, summary));
                    }
                    LookupEvent::Done => done = true,
                }
            }
        }
        for line in lines {
            self.log(line);
        }
        for (index, ok, needs_attention, skipped, summary) in results {
            self.progress_done += 1;
            self.results.insert(
                index,
                RowResult {
                    ok,
                    needs_attention,
                    skipped,
                    summary,
                },
            );
        }
        if done {
            self.in_progress = false;
            self.current_item.clear();
            self.rx = None;
        }
    }

    fn look_up_selected(&mut self, project_dir: &Path, credentials: PartsCredentials) {
        if self.checked.is_empty() {
            self.status = "Select at least one symbol first.".to_string();
            return;
        }
        let digikey_configured = !credentials.digikey_client_id.trim().is_empty()
            && !credentials.digikey_client_secret.trim().is_empty();
        if credentials.mouser_api_key.trim().is_empty() && !digikey_configured {
            self.status = "Set a Mouser API key or a DigiKey Client ID/Secret first.".to_string();
            return;
        }
        self.status.clear();
        self.results.clear();

        let selected: Vec<SelectedRow> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(i, _)| self.checked.contains(i))
            .map(|(index, row)| SelectedRow {
                index,
                reference: row.reference.clone(),
                lib_id: row.lib_id.clone(),
                sch_path: row.sch_path.clone(),
                uuid: row.uuid.clone(),
            })
            .collect();

        let project_name = project_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "project".to_string());
        let default_report_name = format!(
            "{}_stock_report_{}.pdf",
            project_name.replace(' ', "_"),
            chrono::Utc::now().format("%Y%m%d_%H%M%S")
        );
        // Asked every run rather than remembered, so the report never
        // silently lands somewhere the user didn't just choose — see
        // `browse_project`/`browse_watch_folder` for the same
        // ask-every-time pattern with `rfd::FileDialog`. Cancelling
        // still runs the batch (the schematic write-back matters
        // regardless); it just falls back to the project root.
        let report_path = rfd::FileDialog::new()
            .set_directory(project_dir)
            .set_file_name(&default_report_name)
            .add_filter("PDF", &["pdf"])
            .save_file()
            .unwrap_or_else(|| project_dir.join(&default_report_name));

        self.progress_done = 0;
        self.progress_total = selected.len();
        self.current_item.clear();

        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.in_progress = true;

        let force = self.force_recheck;
        thread::Builder::new()
            .name("part-lookup".into())
            .spawn(move || {
                run_lookup_batch(selected, project_name, report_path, force, credentials, tx)
            })
            .expect("failed to spawn the part-lookup thread");
    }
}

/// Runs on the background thread spawned by `look_up_selected`. Groups
/// the selected rows by which schematic file they actually live in and
/// opens each file once (not per-symbol), saving once per file at the
/// end — same batching `library_import::import_symbols` uses for its
/// own destination library, and for the same reason: cheap, and avoids
/// re-reading/re-writing a whole file for every single row in it.
///
/// Before actually querying a vendor, checks that row's own
/// `Last Checked` property (written after every *attempted* lookup,
/// success or failure — see below) and skips it if younger than
/// `RECHECK_THRESHOLD`, unless `force` is set. This is a per-part gate,
/// not a whole-batch one: "Select All" + Populate BOM on day 2 only
/// actually re-queries the parts that have gone stale since day 1,
/// which is the whole point — Mouser/DigiKey rate limits and plain
/// courtesy both argue against re-fetching a part's stock status every
/// time the button is clicked.
fn run_lookup_batch(
    selected: Vec<SelectedRow>,
    project_name: String,
    report_path: PathBuf,
    force: bool,
    credentials: PartsCredentials,
    tx: mpsc::Sender<LookupEvent>,
) {
    let send_log = |msg: String| {
        let _ = tx.send(LookupEvent::Log(msg));
    };
    let send_result =
        |index: usize, ok: bool, needs_attention: bool, skipped: bool, summary: String| {
            let _ = tx.send(LookupEvent::RowResult {
                index,
                ok,
                needs_attention,
                skipped,
                summary,
            });
        };
    let send_current = |reference: String| {
        let _ = tx.send(LookupEvent::CurrentItem(reference));
    };

    // One shared timestamp for the whole batch — a run takes seconds to
    // low minutes, so there's no meaningful staleness difference between
    // rows checked at the start vs. the end of it.
    let now = chrono::Utc::now();

    // Keyed by the row's original index (not push order) so the report
    // below can be emitted in the same natural-reference order the
    // table itself uses, regardless of which schematic-file group a row
    // happened to land in.
    let mut report_rows: HashMap<usize, ReportRow> = HashMap::new();

    let mut by_file: HashMap<PathBuf, Vec<SelectedRow>> = HashMap::new();
    for row in selected {
        by_file.entry(row.sch_path.clone()).or_default().push(row);
    }

    let mut ok_count = 0usize;
    let mut err_count = 0usize;
    let mut skipped_count = 0usize;

    for (path, rows) in by_file {
        let mut sch = match SchematicFile::open(&path) {
            Ok(sch) => sch,
            Err(exc) => {
                send_log(format!(
                    "\u{2718} Could not open '{}': {exc}",
                    path.display()
                ));
                for row in &rows {
                    let msg = format!("could not open schematic: {exc}");
                    send_result(row.index, false, false, false, msg.clone());
                    report_rows.insert(row.index, report_row(row, Err(msg)));
                    err_count += 1;
                }
                continue;
            }
        };

        for row in &rows {
            send_current(row.reference.clone());
            let Some(mut node) = sch.get_symbol_node(&row.uuid) else {
                send_log(format!(
                    "\u{2718} '{}': no longer on the schematic, skipped.",
                    row.reference
                ));
                let msg = "no longer on schematic".to_string();
                send_result(row.index, false, false, false, msg.clone());
                report_rows.insert(row.index, report_row(row, Err(msg)));
                err_count += 1;
                continue;
            };

            if !force {
                if let Some(age) = last_checked_age(&node, now) {
                    if age < RECHECK_THRESHOLD {
                        let summary = format!("Skipped \u{2014} checked {} ago", format_age(age));
                        send_log(format!(
                            "\u{23f8} '{}': {summary} (Force to re-check).",
                            row.reference
                        ));
                        send_result(row.index, true, false, true, summary);
                        skipped_count += 1;
                        continue;
                    }
                }
            }

            let symbol_name = row
                .lib_id
                .split_once(':')
                .map_or(row.lib_id.as_str(), |(_, name)| name);
            let mpn = parts_lookup::resolve_mpn(&node, symbol_name);
            send_log(format!(
                "Looking up '{}' ({}, as '{mpn}')\u{2026}",
                row.reference, row.lib_id
            ));

            match parts_lookup::lookup_part_info(&credentials, &mpn) {
                Ok(info) => {
                    let vendors: Vec<&str> =
                        info.offers.iter().map(|o| o.seller.as_str()).collect();
                    for warning in &info.warnings {
                        send_log(format!("  \u{26a0} '{}': {warning}", row.reference));
                    }
                    parts_lookup::apply_part_info(&mut node, &info);
                    set_symbol_property(&mut node, LAST_CHECKED_PROPERTY, &now.to_rfc3339());
                    sch.patch_symbol(&row.uuid, &node);
                    let in_stock = info.in_stock();
                    let lifecycle_concern = info.lifecycle_concern();
                    let mut flags = String::new();
                    if !in_stock {
                        flags.push_str(" (NOT IN STOCK)");
                    }
                    if lifecycle_concern {
                        flags.push_str(" (OBSOLETE/EOL)");
                    }
                    let summary = format!(
                        "{} \u{2014} {}{flags}",
                        info.manufacturer,
                        if vendors.is_empty() {
                            "no Mouser/DigiKey offers found".to_string()
                        } else {
                            vendors.join(", ")
                        },
                    );
                    send_log(format!("\u{2714} '{}': {summary}", row.reference));
                    send_result(
                        row.index,
                        true,
                        !in_stock || lifecycle_concern,
                        false,
                        summary,
                    );
                    report_rows.insert(row.index, report_row(row, Ok(info)));
                    ok_count += 1;
                }
                Err(exc) => {
                    // A failed lookup (e.g. no match found) still counts
                    // as "checked" — without this, a genuinely-not-found
                    // part would get re-queried on every single run
                    // forever, which is exactly the hammering the 24h
                    // gate exists to prevent.
                    set_symbol_property(&mut node, LAST_CHECKED_PROPERTY, &now.to_rfc3339());
                    sch.patch_symbol(&row.uuid, &node);
                    send_log(format!("\u{2718} '{}': {exc}", row.reference));
                    send_result(row.index, false, false, false, exc.to_string());
                    report_rows.insert(row.index, report_row(row, Err(exc.to_string())));
                    err_count += 1;
                }
            }
        }

        if sch.has_pending_changes() {
            if let Err(exc) = sch.save() {
                send_log(format!(
                    "\u{2718} Could not save '{}': {exc}",
                    path.display()
                ));
            }
        }
    }

    send_log(format!(
        "Done: {ok_count} updated, {err_count} error(s), {skipped_count} skipped (checked recently)."
    ));
    save_stock_report(&report_path, &project_name, report_rows, &send_log);
    let _ = tx.send(LookupEvent::Done);
}

/// How long ago `node`'s `Last Checked` property says it was last
/// looked up, or `None` if it has none (or an unparseable one — treated
/// the same as "never checked", i.e. not stale-skipped).
fn last_checked_age(
    node: &SexpNode,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<chrono::Duration> {
    let text = get_symbol_property(node, LAST_CHECKED_PROPERTY)?;
    let last_checked = chrono::DateTime::parse_from_rfc3339(&text).ok()?;
    Some(now.signed_duration_since(last_checked))
}

/// e.g. `"45m"`, `"3h"`, `"2d"` — coarse on purpose, this is just for a
/// log line and a Result-column note, not a precise readout.
fn format_age(age: chrono::Duration) -> String {
    if age.num_hours() < 1 {
        format!("{}m", age.num_minutes().max(0))
    } else if age.num_hours() < 48 {
        format!("{}h", age.num_hours())
    } else {
        format!("{}d", age.num_days())
    }
}

fn report_row(row: &SelectedRow, outcome: Result<parts_lookup::PartInfo, String>) -> ReportRow {
    let symbol_name = row
        .lib_id
        .split_once(':')
        .map_or(row.lib_id.as_str(), |(_, name)| name);
    ReportRow {
        reference: row.reference.clone(),
        symbol: symbol_name.to_string(),
        outcome,
    }
}

/// Renders the batch's PDF stock report to `report_path` (the location
/// the user picked via the save dialog in `look_up_selected`), in the
/// same natural-reference order the table itself shows. Rows skipped by
/// the 24h staleness gate were never re-checked this run, so they're
/// simply absent here rather than reported on stale data — the log
/// already explains why (see `run_lookup_batch`); if every selected row
/// was skipped, there's nothing meaningful to report at all.
fn save_stock_report(
    report_path: &Path,
    project_name: &str,
    report_rows: HashMap<usize, ReportRow>,
    send_log: &impl Fn(String),
) {
    if report_rows.is_empty() {
        send_log(
            "No report generated \u{2014} every selected part was checked within the last 24h."
                .to_string(),
        );
        return;
    }

    let mut ordered: Vec<(usize, ReportRow)> = report_rows.into_iter().collect();
    ordered.sort_by_key(|(i, _)| *i);
    let rows: Vec<ReportRow> = ordered.into_iter().map(|(_, r)| r).collect();

    let unix_secs = chrono::Utc::now().timestamp().max(0) as u64;
    let generated_at = bom_report::format_utc_timestamp(unix_secs);

    match bom_report::generate(&rows, project_name, &generated_at, report_path) {
        Ok(()) => send_log(format!(
            "Stock report saved to '{}'.",
            report_path.display()
        )),
        Err(exc) => send_log(format!(
            "\u{2718} Could not generate PDF stock report: {exc}"
        )),
    }
}

/// Sizes a table row tall enough to show wrapped multi-line Description
/// and Result text instead of clipping either to one line — same
/// technique as `library_import_ui::description_row_height` (measures
/// the real wrapped galley rather than guessing from a
/// characters-per-line constant), applied to both columns and capped
/// per-column, with the row taking whichever of the two needs more
/// height.
fn row_height(ctx: &egui::Context, description: &str, result: &str) -> f32 {
    const MAX_LINES: usize = 4;
    const DESCRIPTION_WRAP_WIDTH: f32 = 260.0 - 12.0;
    // A little narrower than the Result column's own width to leave
    // room for the leading status glyph drawn before the text.
    const RESULT_WRAP_WIDTH: f32 = 320.0 - 12.0 - 20.0;

    let font_id = egui::TextStyle::Body.resolve(&ctx.style());
    let measure = |text: &str, wrap_width: f32| -> f32 {
        let galley =
            ctx.fonts(|f| f.layout_delayed_color(text.to_owned(), font_id.clone(), wrap_width));
        let line_count = galley.rows.len().max(1);
        let line_height = galley.rect.height() / line_count as f32;
        line_height * line_count.min(MAX_LINES) as f32 + 8.0
    };

    measure(description, DESCRIPTION_WRAP_WIDTH).max(measure(result, RESULT_WRAP_WIDTH))
}

pub fn show(
    state: &mut PartLookupState,
    ctx: &egui::Context,
    project_dir: &Path,
    credentials: &PartsCredentials,
) {
    if !state.open {
        return;
    }

    state.drain_channel();
    // Polled every frame while a batch is running so results/log lines
    // show up promptly instead of waiting for the next unrelated repaint.
    if state.in_progress {
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
    }

    if state.loaded_from.as_deref() != Some(project_dir) {
        state.load_from(project_dir);
    }

    let viewport_id = egui::ViewportId::from_hash_of("part_lookup_window");
    let builder = egui::ViewportBuilder::default()
        .with_title("Populate BOM")
        .with_inner_size([980.0, 640.0])
        .with_min_inner_size([600.0, 400.0])
        .with_decorations(false)
        .with_icon(crate::icon::app_icon());

    ctx.show_viewport_immediate(viewport_id, builder, |ctx, _class| {
        if ctx.input(|i| i.viewport().close_requested()) {
            state.open = false;
        }

        egui::TopBottomPanel::top("part_lookup_title_bar")
            .exact_height(crate::window_chrome::BAR_HEIGHT)
            .frame(egui::Frame::none().fill(crate::theme::TITLE_BAR_BG))
            .show_separator_line(false)
            .show(ctx, |ui| {
                crate::window_chrome::title_bar(ui, ctx, "Populate BOM")
            });

        egui::TopBottomPanel::top("part_lookup_top").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::FOLDER).color(ACCENT));
                ui.label(
                    RichText::new(project_dir.display().to_string())
                        .weak()
                        .small(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button(format!("{} Reload", icon::ARROW_CLOCKWISE))
                        .clicked()
                    {
                        state.load_from(project_dir);
                    }
                });
            });

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Select All").clicked() {
                    state.checked = (0..state.rows.len()).collect();
                }
                if ui.button("Select None").clicked() {
                    state.checked.clear();
                }
                ui.label(
                    RichText::new(format!(
                        "{} of {} selected",
                        state.checked.len(),
                        state.rows.len()
                    ))
                    .weak(),
                );
                ui.add_space(12.0);
                ui.checkbox(&mut state.force_recheck, "Force re-check")
                    .on_hover_text(
                        "Ignore each part's own 24h \u{201c}last checked\u{201d} cooldown and \
                     re-query Mouser/DigiKey even for parts checked recently.",
                    );
            });
            ui.add_space(6.0);
        });

        egui::TopBottomPanel::bottom("part_lookup_bottom").show(ctx, |ui| {
            if !state.status.is_empty() {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icon::WARNING_CIRCLE).color(DANGER));
                    ui.colored_label(DANGER, &state.status);
                });
            }

            if state.progress_total > 0 {
                ui.add_space(6.0);
                if !state.current_item.is_empty() {
                    ui.label(
                        RichText::new(format!("Looking up '{}'\u{2026}", state.current_item))
                            .small()
                            .weak(),
                    );
                }
                let fraction = state.progress_done as f32 / state.progress_total as f32;
                let resp = ui.add(egui::ProgressBar::new(fraction).fill(ACCENT));
                // `ProgressBar::text` (egui 0.29) always left-aligns its
                // text against the bar's edge rather than centering it —
                // painting the done/total count ourselves, centered over
                // the bar's own response rect, is the only way to get it
                // in the middle.
                ui.painter().text(
                    resp.rect.center(),
                    egui::Align2::CENTER_CENTER,
                    format!("{}/{}", state.progress_done, state.progress_total),
                    egui::FontId::proportional(13.0),
                    Color32::WHITE,
                );
            }

            ui.add_space(6.0);
            // Collapsed by default — see `library_import_ui::show`'s
            // identical treatment of its own detail log.
            egui::CollapsingHeader::new(format!("{}  Detail Log", icon::TERMINAL_WINDOW))
                .id_salt("part_lookup_log_collapse")
                .default_open(false)
                .show(ui, |ui| {
                    egui::Frame::group(ui.style())
                        .fill(Color32::from_rgb(0x0d, 0x0e, 0x11))
                        .inner_margin(egui::Margin::same(8.0))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            egui::ScrollArea::vertical()
                                .max_height(160.0)
                                .stick_to_bottom(true)
                                .show(ui, |ui| {
                                    // Deliberately interactive — see
                                    // `library_import_ui::show`'s
                                    // identical log widget for why this
                                    // is selectable/copyable without
                                    // actually being editable.
                                    ui.add(
                                        egui::TextEdit::multiline(&mut state.log_lines.join("\n"))
                                            .desired_width(f32::INFINITY)
                                            .frame(false)
                                            .font(egui::TextStyle::Monospace),
                                    );
                                });
                        });
                });

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let label = if state.in_progress {
                    format!("{}  Looking Up\u{2026}", icon::SPINNER)
                } else {
                    format!("{}  Populate BOM", icon::MAGNIFYING_GLASS)
                };
                let clicked = ui
                    .add_enabled(!state.in_progress, theme::accent_button(label))
                    .clicked();
                if clicked {
                    state.look_up_selected(project_dir, credentials.clone());
                }
                if ui.button("Close").clicked() {
                    state.open = false;
                }
            });
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                let table_height = ui.available_height().max(100.0);
                egui::ScrollArea::horizontal()
                    .id_salt("part_lookup_table_hscroll")
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        let row_heights: Vec<f32> = state
                            .rows
                            .iter()
                            .enumerate()
                            .map(|(i, r)| {
                                let result = state
                                    .results
                                    .get(&i)
                                    .map(|r| r.summary.as_str())
                                    .unwrap_or("");
                                row_height(ctx, &r.description, result)
                            })
                            .collect();
                        TableBuilder::new(ui)
                            .id_salt("part_lookup_table")
                            .striped(true)
                            .resizable(true)
                            .sense(egui::Sense::click())
                            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                            .column(Column::exact(24.0))
                            .column(Column::initial(140.0).at_least(80.0).clip(true))
                            .column(Column::initial(140.0).at_least(70.0).clip(true))
                            .column(Column::initial(80.0).at_least(50.0).clip(true))
                            .column(Column::initial(260.0).at_least(120.0).clip(true))
                            .column(Column::initial(320.0).at_least(160.0).clip(true))
                            .min_scrolled_height(0.0)
                            .max_scroll_height(table_height)
                            .header(24.0, |mut header| {
                                header.col(|_ui| {});
                                header.col(|ui| {
                                    ui.strong("Library");
                                });
                                header.col(|ui| {
                                    ui.strong("Symbol");
                                });
                                header.col(|ui| {
                                    ui.strong("Reference");
                                });
                                header.col(|ui| {
                                    ui.strong("Description");
                                });
                                header.col(|ui| {
                                    ui.strong("Result");
                                });
                            })
                            .body(|body| {
                                body.heterogeneous_rows(row_heights.into_iter(), |mut row| {
                                    let i = row.index();
                                    let checked = state.checked.contains(&i);
                                    row.set_selected(checked);

                                    row.col(|ui| {
                                        // See library_import_ui.rs's
                                        // identical checkbox-painting
                                        // code for why this is drawn
                                        // directly rather than an
                                        // `egui::Checkbox` widget.
                                        let box_size = 14.0;
                                        let box_rect = egui::Rect::from_center_size(
                                            ui.max_rect().center(),
                                            egui::vec2(box_size, box_size),
                                        );
                                        let stroke_color = if checked {
                                            ACCENT
                                        } else {
                                            Color32::from_gray(120)
                                        };
                                        ui.painter().rect_stroke(
                                            box_rect,
                                            egui::Rounding::same(3.0),
                                            egui::Stroke::new(1.5_f32, stroke_color),
                                        );
                                        if checked {
                                            ui.painter().rect_filled(
                                                box_rect.shrink(2.0),
                                                egui::Rounding::same(2.0),
                                                theme::ACCENT_DIM,
                                            );
                                            ui.painter().text(
                                                box_rect.center(),
                                                egui::Align2::CENTER_CENTER,
                                                icon::CHECK,
                                                egui::FontId::proportional(11.0),
                                                Color32::WHITE,
                                            );
                                        }
                                    });
                                    let r = &state.rows[i];
                                    row.col(|ui| {
                                        ui.label(r.library());
                                    });
                                    row.col(|ui| {
                                        ui.label(r.symbol_name());
                                    });
                                    row.col(|ui| {
                                        ui.label(&r.reference);
                                    });
                                    row.col(|ui| {
                                        ui.add(egui::Label::new(&r.description).wrap());
                                    });
                                    row.col(|ui| {
                                        if let Some(result) = state.results.get(&i) {
                                            let (glyph, color) = if result.skipped {
                                                (icon::CLOCK, Color32::from_gray(160))
                                            } else if !result.ok {
                                                (icon::X_CIRCLE, DANGER)
                                            } else if result.needs_attention {
                                                (icon::WARNING_CIRCLE, DANGER)
                                            } else {
                                                (icon::CHECK_CIRCLE, OK)
                                            };
                                            ui.horizontal(|ui| {
                                                ui.label(RichText::new(glyph).color(color));
                                                ui.add(egui::Label::new(&result.summary).wrap());
                                            });
                                        }
                                    });

                                    let resp = row.response();
                                    if resp.clicked() {
                                        let shift = resp.ctx.input(|inp| inp.modifiers.shift);
                                        state.handle_row_click(i, shift);
                                    }
                                });
                            });
                    });
            });
        });

        crate::window_chrome::resize_grip(ctx, "part_lookup");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_with_last_checked(rfc3339: &str) -> SexpNode {
        let mut node =
            kicad_auto_importer_core::sexp::parse(r#"(symbol (property "Reference" "R1"))"#)
                .unwrap();
        set_symbol_property(&mut node, LAST_CHECKED_PROPERTY, rfc3339);
        node
    }

    #[test]
    fn last_checked_age_is_none_when_property_absent() {
        let node = kicad_auto_importer_core::sexp::parse(r#"(symbol (property "Reference" "R1"))"#)
            .unwrap();
        assert!(last_checked_age(&node, chrono::Utc::now()).is_none());
    }

    #[test]
    fn last_checked_age_is_none_when_property_unparseable() {
        let node = node_with_last_checked("not a timestamp");
        assert!(last_checked_age(&node, chrono::Utc::now()).is_none());
    }

    #[test]
    fn last_checked_age_computes_elapsed_duration() {
        let now = chrono::Utc::now();
        let checked_at = now - chrono::Duration::hours(5);
        let node = node_with_last_checked(&checked_at.to_rfc3339());

        let age = last_checked_age(&node, now).unwrap();
        assert_eq!(age.num_hours(), 5);
    }

    #[test]
    fn fresh_last_checked_is_under_the_recheck_threshold() {
        let now = chrono::Utc::now();
        let checked_at = now - chrono::Duration::hours(1);
        let node = node_with_last_checked(&checked_at.to_rfc3339());

        let age = last_checked_age(&node, now).unwrap();
        assert!(age < RECHECK_THRESHOLD);
    }

    #[test]
    fn stale_last_checked_is_over_the_recheck_threshold() {
        let now = chrono::Utc::now();
        let checked_at = now - chrono::Duration::hours(25);
        let node = node_with_last_checked(&checked_at.to_rfc3339());

        let age = last_checked_age(&node, now).unwrap();
        assert!(age >= RECHECK_THRESHOLD);
    }

    #[test]
    fn format_age_uses_minutes_under_an_hour() {
        assert_eq!(format_age(chrono::Duration::minutes(45)), "45m");
    }

    #[test]
    fn format_age_uses_hours_under_two_days() {
        assert_eq!(format_age(chrono::Duration::hours(5)), "5h");
        assert_eq!(format_age(chrono::Duration::hours(47)), "47h");
    }

    #[test]
    fn format_age_uses_days_at_and_beyond_two_days() {
        assert_eq!(format_age(chrono::Duration::hours(48)), "2d");
        assert_eq!(format_age(chrono::Duration::days(5)), "5d");
    }
}
