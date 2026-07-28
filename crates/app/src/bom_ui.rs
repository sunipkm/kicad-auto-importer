//! "Generate BOM" — a priced, grouped bill of materials for the whole
//! project: every placed schematic symbol (`kicad_auto_importer_core::schematic`)
//! is grouped into unique purchasable parts
//! (`kicad_auto_importer_core::bom_pricing::group_placed_symbols`),
//! multiplied by a user-entered board quantity, padded with extra
//! margin for resistors/capacitors/inductors (hand assembly eats
//! spares — cheap parts, so always round up), and priced against
//! Mouser/DigiKey using the cheapest available quantity-break tier
//! (`bom_pricing::choose_cheapest_offer`) — not necessarily the exact
//! quantity needed, since crossing into a lower per-unit tier can cost
//! less overall.
//!
//! Distinct from "Populate BOM" (`part_lookup_ui.rs`): that one
//! annotates every placed *reference* with vendor data and reports
//! stock/lifecycle status per reference. This one groups identical
//! parts first and answers "what would it cost to actually order
//! this," which needs different input (board count, a margin
//! percentage) and produces different output (grouped + priced, as
//! both a PDF and an `.xlsx` workbook — not a CSV: a reference list
//! like "R1, R5, R20" is exactly the kind of value a CSV's own column
//! delimiter can get confused for, corrupting every column after it in
//! some spreadsheet locales, which a real spreadsheet format sidesteps
//! entirely (see `bom_report::generate_priced_bom_xlsx`'s docs).
//!
//! Shares its lookup cache with "Populate BOM": before querying
//! Mouser/DigiKey for a group, this checks whichever of the group's
//! placed instances has the freshest `Last Checked` property (written
//! by either feature — see `part_lookup_ui::LAST_CHECKED_PROPERTY`) and,
//! if it's within `part_lookup_ui::RECHECK_THRESHOLD`, reuses that
//! cached lookup (`parts_lookup::read_cached_part_info`) instead of
//! spending another API call. A cache hit costs nothing in precision:
//! `parts_lookup::apply_part_info` now writes a full-fidelity raw price
//! breaks property alongside the capped/formatted display string, so
//! reading it back doesn't lose the precision `cheapest_purchase`'s
//! bracket-optimization math needs. Fresh (non-cached) lookups get
//! written back onto every instance in the group — not just the one
//! checked — so Populate BOM's own per-reference cache benefits too,
//! and vice versa. "Force re-check" bypasses the cache, same as
//! Populate BOM's own checkbox.
//!
//! Structurally still a near-copy of `part_lookup_ui.rs`: a genuine
//! second OS window (`show_viewport_immediate`), the same
//! progress-bar/collapsible-log/background-thread shape, and the same
//! "KiCad has this project open" warning/skip-save handling since this
//! now writes to the schematic too. No per-row selection here, though
//! (unlike Populate BOM) — grouping already happens automatically, and
//! the point of this window is "price the whole BOM," not "recheck a
//! few specific parts."

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

use egui::{Color32, RichText};
use egui_extras::{Column, TableBuilder};
use egui_phosphor::regular as icon;

use kicad_auto_importer_core::bom_pricing::{self, ChosenOffer, PartGroup, PricedRow};
use kicad_auto_importer_core::bom_report;
use kicad_auto_importer_core::kicad_process;
use kicad_auto_importer_core::parts_lookup::{self, PartsCredentials};
use kicad_auto_importer_core::schematic::{self, SchematicFile};
use kicad_auto_importer_core::symbol_importer::set_symbol_property;

use crate::part_lookup_ui::{last_checked_age, LAST_CHECKED_PROPERTY, RECHECK_THRESHOLD};
use crate::theme::{self, ACCENT, DANGER, OK};

enum BomEvent {
    Log(String),
    /// Fires once a group starts being priced — drives the "currently
    /// looking up…" label next to the progress bar.
    CurrentItem(String),
    RowResult {
        index: usize,
        needed_qty: u32,
        outcome: Result<ChosenOffer, String>,
    },
    Done {
        grand_total: f64,
    },
}

pub struct BomState {
    pub open: bool,
    groups: Vec<PartGroup>,
    /// The project dir `groups` was last loaded from — reload
    /// automatically if the window is (re)opened pointing somewhere
    /// else, e.g. after switching projects.
    loaded_from: Option<PathBuf>,
    board_qty: u32,
    /// Percentage extra ordered for resistor/capacitor/inductor
    /// footprints on top of what's strictly needed — see
    /// `bom_pricing::margin_adjusted_quantity`.
    passive_margin_percent: u32,
    /// Bypasses the 24h "checked recently, reuse the cache" gate — see
    /// `part_lookup_ui::RECHECK_THRESHOLD`. Opt-in (defaults off), same
    /// reasoning as Populate BOM's identical checkbox.
    force_recheck: bool,
    log_lines: Vec<String>,
    status: String,
    rx: Option<mpsc::Receiver<BomEvent>>,
    in_progress: bool,
    /// Per-group outcome of the most recent run, keyed by index into
    /// `groups`. Cleared on reload (indices become meaningless against
    /// a freshly loaded group list) and at the start of each new run.
    results: HashMap<usize, (u32, Result<ChosenOffer, String>)>,
    progress_done: usize,
    progress_total: usize,
    current_item: String,
    /// Set once a run completes — `None` beforehand (including while a
    /// run is in progress), so the UI can tell "never run" apart from
    /// "ran and totaled $0.00".
    grand_total: Option<f64>,
}

impl Default for BomState {
    fn default() -> Self {
        BomState {
            open: false,
            groups: Vec::new(),
            loaded_from: None,
            board_qty: 1,
            passive_margin_percent: 20,
            force_recheck: false,
            log_lines: Vec::new(),
            status: String::new(),
            rx: None,
            in_progress: false,
            results: HashMap::new(),
            progress_done: 0,
            progress_total: 0,
            current_item: String::new(),
            grand_total: None,
        }
    }
}

impl BomState {
    fn log(&mut self, msg: impl Into<String>) {
        self.log_lines.push(msg.into());
    }

    fn load_from(&mut self, project_dir: &Path) {
        let mut lines = Vec::new();
        let symbols = schematic::load_schematic_symbols(project_dir, |m| lines.push(m.to_string()));
        self.groups = bom_pricing::group_placed_symbols(&symbols);
        for line in lines {
            self.log(line);
        }
        self.results.clear();
        self.progress_done = 0;
        self.progress_total = 0;
        self.current_item.clear();
        self.grand_total = None;
        self.loaded_from = Some(project_dir.to_path_buf());
        self.log(format!(
            "Found {} unique part(s) across {} placed symbol(s).",
            self.groups.len(),
            symbols.len()
        ));
    }

    fn drain_channel(&mut self) {
        let mut lines = Vec::new();
        let mut results = Vec::new();
        let mut done = None;
        if let Some(rx) = &self.rx {
            while let Ok(event) = rx.try_recv() {
                match event {
                    BomEvent::Log(msg) => lines.push(msg),
                    BomEvent::CurrentItem(item) => self.current_item = item,
                    BomEvent::RowResult {
                        index,
                        needed_qty,
                        outcome,
                    } => results.push((index, needed_qty, outcome)),
                    BomEvent::Done { grand_total } => done = Some(grand_total),
                }
            }
        }
        for line in lines {
            self.log(line);
        }
        for (index, needed_qty, outcome) in results {
            self.progress_done += 1;
            self.results.insert(index, (needed_qty, outcome));
        }
        if let Some(grand_total) = done {
            self.in_progress = false;
            self.current_item.clear();
            self.grand_total = Some(grand_total);
            self.rx = None;
        }
    }

    fn generate(&mut self, project_dir: &Path, credentials: PartsCredentials) {
        if self.groups.is_empty() {
            self.status = "No parts found on the schematic to price.".to_string();
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
        self.progress_done = 0;
        self.current_item.clear();
        self.grand_total = None;

        let project_name = project_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "project".to_string());

        // Checked here, before spending any Mouser/DigiKey calls, same
        // reasoning and wording as `part_lookup_ui::look_up_selected` —
        // this now writes cache data back onto the schematic too (see
        // module docs), so the same "KiCad has it open" hazard applies.
        let kicad_open = kicad_process::project_open_in_kicad(project_dir);
        if kicad_open {
            let choice = rfd::MessageDialog::new()
                .set_title("KiCad Has This Project Open")
                .set_description(format!(
                    "'{project_name}' appears to be open in KiCad.\n\n\
                     Generate BOM can still look up pricing and produce the report, but \
                     schematic changes (the cached lookup data used to skip repeat lookups) \
                     will NOT be written back until you close it in KiCad.\n\nContinue anyway?"
                ))
                .set_level(rfd::MessageLevel::Warning)
                .set_buttons(rfd::MessageButtons::OkCancel)
                .show();
            if choice != rfd::MessageDialogResult::Ok {
                self.status = "Generate BOM cancelled \u{2014} close the project in KiCad first, \
                     or confirm the warning next time to proceed anyway."
                    .to_string();
                return;
            }
        }

        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");

        // Asked every run rather than remembered — see
        // `part_lookup_ui`'s identical ask-every-time save-dialog
        // pattern. Cancelling either dialog just skips that one output
        // format; the lookups/pricing still happen and the other format
        // (if chosen) still gets written.
        let pdf_path = rfd::FileDialog::new()
            .set_directory(project_dir)
            .set_file_name(format!(
                "{}_bom_{timestamp}.pdf",
                project_name.replace(' ', "_")
            ))
            .add_filter("PDF", &["pdf"])
            .save_file();
        let xlsx_path = rfd::FileDialog::new()
            .set_directory(project_dir)
            .set_file_name(format!(
                "{}_bom_{timestamp}.xlsx",
                project_name.replace(' ', "_")
            ))
            .add_filter("Excel Workbook", &["xlsx"])
            .save_file();

        self.progress_total = self.groups.len();

        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.in_progress = true;

        let request = BomBatchRequest {
            groups: self.groups.clone(),
            board_qty: self.board_qty.max(1),
            passive_margin_percent: self.passive_margin_percent,
            force_recheck: self.force_recheck,
            kicad_open,
            project_name,
            pdf_path,
            xlsx_path,
            credentials,
        };
        thread::Builder::new()
            .name("bom-generate".into())
            .spawn(move || run_bom_batch(request, tx))
            .expect("failed to spawn the bom-generate thread");
    }
}

/// Bundles `BomState::generate`'s one-shot batch parameters — keeps
/// `run_bom_batch` under clippy's too-many-arguments threshold instead
/// of taking nine loose parameters (same rationale as `bom_report`'s
/// `TextStyle`/`TableContext`).
struct BomBatchRequest {
    groups: Vec<PartGroup>,
    board_qty: u32,
    passive_margin_percent: u32,
    force_recheck: bool,
    /// See `part_lookup_ui::run_lookup_batch`'s identical flag — skips
    /// saving schematic changes (but not the lookups/report) when set.
    kicad_open: bool,
    project_name: String,
    pdf_path: Option<PathBuf>,
    xlsx_path: Option<PathBuf>,
    credentials: PartsCredentials,
}

/// Runs on the background thread spawned by `BomState::generate`. At
/// most one `parts_lookup::lookup_part_info` call per *group*, not per
/// reference — the entire efficiency point of grouping identical parts
/// first — and even that's skipped when a fresh-enough cached lookup is
/// already sitting on the schematic (see module docs).
fn run_bom_batch(request: BomBatchRequest, tx: mpsc::Sender<BomEvent>) {
    let BomBatchRequest {
        groups,
        board_qty,
        passive_margin_percent,
        force_recheck,
        kicad_open,
        project_name,
        pdf_path,
        xlsx_path,
        credentials,
    } = request;

    let send_log = |msg: String| {
        let _ = tx.send(BomEvent::Log(msg));
    };
    let send_current = |name: String| {
        let _ = tx.send(BomEvent::CurrentItem(name));
    };
    let send_result = |index: usize, needed_qty: u32, outcome: Result<ChosenOffer, String>| {
        let _ = tx.send(BomEvent::RowResult {
            index,
            needed_qty,
            outcome,
        });
    };

    if kicad_open {
        send_log(format!(
            "\u{26a0} '{project_name}' appears to be open in KiCad \u{2014} pricing and \
             reporting normally, but cached lookup data will NOT be written back until you \
             close it."
        ));
    }

    // Every schematic file any group's instances live in, opened once
    // (not per-group/per-reference) — mirrors `part_lookup_ui::run_lookup_batch`'s
    // own `by_file` batching, just keyed across the whole run up front
    // since cache reads need it before the per-group loop even starts.
    let mut sch_files: HashMap<PathBuf, SchematicFile> = HashMap::new();
    for group in &groups {
        for (path, _) in &group.instances {
            if let std::collections::hash_map::Entry::Vacant(entry) = sch_files.entry(path.clone())
            {
                match SchematicFile::open(path) {
                    Ok(sch) => {
                        entry.insert(sch);
                    }
                    Err(exc) => {
                        send_log(format!(
                            "\u{2718} Could not open '{}': {exc}",
                            path.display()
                        ));
                    }
                }
            }
        }
    }

    let now = chrono::Utc::now();
    let mut priced_rows: Vec<PricedRow> = Vec::with_capacity(groups.len());
    let mut grand_total = 0.0f64;

    for (index, group) in groups.into_iter().enumerate() {
        send_current(group.display_name.clone());
        let raw_needed = group.per_board_qty * board_qty;
        let needed_qty = bom_pricing::margin_adjusted_quantity(
            raw_needed,
            group.is_passive,
            passive_margin_percent,
        );

        // Reuse whichever instance in the group has the freshest cached
        // lookup, if any is still within the recheck window.
        let cached_info = if force_recheck {
            None
        } else {
            group.instances.iter().find_map(|(path, uuid)| {
                let sch = sch_files.get(path)?;
                let node = sch.get_symbol_node(uuid)?;
                let age = last_checked_age(&node, now)?;
                if age < RECHECK_THRESHOLD {
                    parts_lookup::read_cached_part_info(&node)
                } else {
                    None
                }
            })
        };

        let (lookup_result, from_cache): (Result<parts_lookup::PartInfo, String>, bool) =
            match cached_info {
                Some(info) => {
                    send_log(format!(
                        "\u{23f8} '{}': reusing a lookup checked within the last 24h.",
                        group.display_name
                    ));
                    (Ok(info), true)
                }
                None => {
                    send_log(format!(
                        "Looking up '{}' ({} ref(s), need {needed_qty})\u{2026}",
                        group.display_name,
                        group.references.len()
                    ));
                    (
                        parts_lookup::lookup_part_info(&credentials, &group.search_mpn)
                            .map_err(|e| e.to_string()),
                        false,
                    )
                }
            };

        // A fresh (non-cached) lookup gets written back onto every
        // instance in the group — success or failure, same reasoning as
        // Populate BOM's own `run_lookup_batch`: a failed lookup still
        // counts as "checked," so a genuinely-not-found part isn't
        // re-queried every single run. Skipped entirely if KiCad has the
        // project open, same as Populate BOM.
        if !from_cache && !kicad_open {
            for (path, uuid) in &group.instances {
                let Some(sch) = sch_files.get_mut(path) else {
                    continue;
                };
                let Some(mut node) = sch.get_symbol_node(uuid) else {
                    continue;
                };
                if let Ok(info) = &lookup_result {
                    parts_lookup::apply_part_info(&mut node, info);
                }
                set_symbol_property(&mut node, LAST_CHECKED_PROPERTY, &now.to_rfc3339());
                sch.patch_symbol(uuid, &node);
            }
        }

        let outcome = match lookup_result {
            Ok(info) => {
                for warning in &info.warnings {
                    send_log(format!("  \u{26a0} '{}': {warning}", group.display_name));
                }
                match bom_pricing::choose_cheapest_offer(&info, needed_qty) {
                    Some(chosen) => {
                        grand_total += chosen.total_price;
                        let shortfall = chosen.stock_quantity < u64::from(chosen.purchase_qty);
                        let flag = if shortfall { " (NOT ENOUGH STOCK)" } else { "" };
                        send_log(format!(
                            "\u{2714} '{}': buy {} from {} @ ${:.2} = ${:.2}{flag}",
                            group.display_name,
                            chosen.purchase_qty,
                            chosen.seller,
                            chosen.unit_price,
                            chosen.total_price
                        ));
                        Ok(chosen)
                    }
                    None => {
                        let msg = "no priced offers available".to_string();
                        send_log(format!("\u{2718} '{}': {msg}", group.display_name));
                        Err(msg)
                    }
                }
            }
            Err(exc) => {
                send_log(format!("\u{2718} '{}': {exc}", group.display_name));
                Err(exc)
            }
        };

        send_result(index, needed_qty, outcome.clone());
        priced_rows.push(PricedRow {
            group,
            needed_qty,
            outcome,
        });
    }

    if kicad_open {
        send_log(
            "\u{23f8} Skipped saving schematic changes \u{2014} KiCad has this project open."
                .to_string(),
        );
    } else {
        for (path, sch) in &sch_files {
            if sch.has_pending_changes() {
                if let Err(exc) = sch.save() {
                    send_log(format!(
                        "\u{2718} Could not save '{}': {exc}",
                        path.display()
                    ));
                }
            }
        }
    }

    let unix_secs = chrono::Utc::now().timestamp().max(0) as u64;
    let generated_at = bom_report::format_utc_timestamp(unix_secs);

    if let Some(path) = &pdf_path {
        match bom_report::generate_priced_bom(
            &priced_rows,
            &project_name,
            board_qty,
            &generated_at,
            path,
        ) {
            Ok(()) => send_log(format!("Priced BOM PDF saved to '{}'.", path.display())),
            Err(exc) => send_log(format!("\u{2718} Could not generate PDF: {exc}")),
        }
    }
    if let Some(path) = &xlsx_path {
        match bom_report::generate_priced_bom_xlsx(&priced_rows, board_qty, path) {
            Ok(()) => send_log(format!(
                "Priced BOM spreadsheet saved to '{}'.",
                path.display()
            )),
            Err(exc) => send_log(format!("\u{2718} Could not generate spreadsheet: {exc}")),
        }
    }

    send_log(format!("Done: estimated total ${grand_total:.2}."));
    let _ = tx.send(BomEvent::Done { grand_total });
}

pub fn show(
    state: &mut BomState,
    ctx: &egui::Context,
    project_dir: &Path,
    credentials: &PartsCredentials,
) {
    if !state.open {
        return;
    }

    state.drain_channel();
    if state.in_progress {
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
    }

    if state.loaded_from.as_deref() != Some(project_dir) {
        state.load_from(project_dir);
    }

    let viewport_id = egui::ViewportId::from_hash_of("bom_window");
    let builder = egui::ViewportBuilder::default()
        .with_title("Generate BOM")
        .with_inner_size([900.0, 620.0])
        .with_min_inner_size([600.0, 400.0])
        .with_decorations(false)
        .with_icon(crate::icon::app_icon());

    ctx.show_viewport_immediate(viewport_id, builder, |ctx, _class| {
        if ctx.input(|i| i.viewport().close_requested()) {
            state.open = false;
        }

        egui::TopBottomPanel::top("bom_title_bar")
            .exact_height(crate::window_chrome::BAR_HEIGHT)
            .frame(egui::Frame::none().fill(crate::theme::TITLE_BAR_BG))
            .show_separator_line(false)
            .show(ctx, |ui| {
                crate::window_chrome::title_bar(ui, ctx, "Generate BOM")
            });

        egui::TopBottomPanel::top("bom_top").show(ctx, |ui| {
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
                ui.label("Boards:");
                let mut board_qty = state.board_qty;
                if ui
                    .add(egui::DragValue::new(&mut board_qty).range(1..=100_000))
                    .changed()
                {
                    state.board_qty = board_qty.max(1);
                }
                ui.add_space(16.0);
                ui.label("Passive extra margin:");
                let mut margin = state.passive_margin_percent;
                if ui
                    .add(egui::DragValue::new(&mut margin).range(0..=200).suffix("%"))
                    .changed()
                {
                    state.passive_margin_percent = margin;
                }
                ui.label(
                    RichText::new("(resistors/capacitors/inductors only \u{2014} min. +5 pcs)")
                        .small()
                        .weak(),
                );
                ui.add_space(16.0);
                ui.checkbox(&mut state.force_recheck, "Force re-check")
                    .on_hover_text(
                        "Ignore each part's own 24h \u{201c}last checked\u{201d} cooldown \
                         (shared with Populate BOM) and re-query Mouser/DigiKey even for parts \
                         checked recently.",
                    );
            });
            ui.add_space(6.0);
        });

        egui::TopBottomPanel::bottom("bom_bottom").show(ctx, |ui| {
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
                ui.painter().text(
                    resp.rect.center(),
                    egui::Align2::CENTER_CENTER,
                    format!("{}/{}", state.progress_done, state.progress_total),
                    egui::FontId::proportional(13.0),
                    Color32::WHITE,
                );
            }

            if let Some(grand_total) = state.grand_total {
                ui.add_space(6.0);
                ui.label(
                    RichText::new(format!("Estimated total: ${grand_total:.2}"))
                        .strong()
                        .size(15.0)
                        .color(OK),
                );
            }

            ui.add_space(6.0);
            egui::CollapsingHeader::new(format!("{}  Detail Log", icon::TERMINAL_WINDOW))
                .id_salt("bom_log_collapse")
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
                    format!("{}  Pricing\u{2026}", icon::SPINNER)
                } else {
                    format!("{}  Generate BOM", icon::CURRENCY_DOLLAR)
                };
                let clicked = ui
                    .add_enabled(!state.in_progress, theme::accent_button(label))
                    .clicked();
                if clicked {
                    state.generate(project_dir, credentials.clone());
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
                    .id_salt("bom_table_hscroll")
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        TableBuilder::new(ui)
                            .id_salt("bom_table")
                            .striped(true)
                            .resizable(true)
                            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                            .column(Column::initial(170.0).at_least(100.0).clip(true))
                            .column(Column::initial(220.0).at_least(120.0).clip(true))
                            .column(Column::initial(70.0).at_least(50.0))
                            .column(Column::remainder().at_least(220.0))
                            .min_scrolled_height(0.0)
                            .max_scroll_height(table_height)
                            .header(24.0, |mut header| {
                                header.col(|ui| {
                                    ui.strong("Part");
                                });
                                header.col(|ui| {
                                    ui.strong("References");
                                });
                                header.col(|ui| {
                                    ui.strong("Need");
                                });
                                header.col(|ui| {
                                    ui.strong("Result");
                                });
                            })
                            .body(|body| {
                                body.rows(22.0, state.groups.len(), |mut row| {
                                    let i = row.index();
                                    let group = &state.groups[i];
                                    row.col(|ui| {
                                        ui.add(egui::Label::new(&group.display_name).truncate());
                                    });
                                    row.col(|ui| {
                                        ui.add(
                                            egui::Label::new(group.references.join(", "))
                                                .truncate(),
                                        );
                                    });
                                    let preview_needed = bom_pricing::margin_adjusted_quantity(
                                        group.per_board_qty * state.board_qty.max(1),
                                        group.is_passive,
                                        state.passive_margin_percent,
                                    );
                                    row.col(|ui| {
                                        let needed = state
                                            .results
                                            .get(&i)
                                            .map(|(n, _)| *n)
                                            .unwrap_or(preview_needed);
                                        ui.label(needed.to_string());
                                    });
                                    row.col(|ui| {
                                        if let Some((_, outcome)) = state.results.get(&i) {
                                            match outcome {
                                                Ok(chosen) => {
                                                    let shortfall = chosen.stock_quantity
                                                        < u64::from(chosen.purchase_qty);
                                                    let flagged = !chosen.in_stock
                                                        || shortfall
                                                        || chosen.lifecycle_concern;
                                                    let (glyph, color) = if flagged {
                                                        (icon::WARNING_CIRCLE, DANGER)
                                                    } else {
                                                        (icon::CHECK_CIRCLE, OK)
                                                    };
                                                    let stock_note = if !chosen.in_stock {
                                                        " (not in stock)"
                                                    } else if shortfall {
                                                        " (not enough stock)"
                                                    } else {
                                                        ""
                                                    };
                                                    let text = format!(
                                                        "Buy {} \u{2014} {} @ ${:.2} = ${:.2}{stock_note}",
                                                        chosen.purchase_qty,
                                                        chosen.seller,
                                                        chosen.unit_price,
                                                        chosen.total_price
                                                    );
                                                    ui.horizontal(|ui| {
                                                        ui.label(RichText::new(glyph).color(color));
                                                        ui.add(egui::Label::new(text).truncate());
                                                    });
                                                }
                                                Err(msg) => {
                                                    ui.horizontal(|ui| {
                                                        ui.label(
                                                            RichText::new(icon::X_CIRCLE)
                                                                .color(DANGER),
                                                        );
                                                        ui.add(egui::Label::new(msg).truncate());
                                                    });
                                                }
                                            }
                                        }
                                    });
                                });
                            });
                    });
            });
        });

        crate::window_chrome::resize_grip(ctx, "bom");
    });
}
