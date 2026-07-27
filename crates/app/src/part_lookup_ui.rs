//! "Populate BOM" — annotate every symbol registered anywhere in the
//! current project (not just the one destination library this app
//! manages) with manufacturer and Mouser/DigiKey distributor info (see
//! `kicad_auto_importer_core::parts_lookup`).
//!
//! "Every symbol in the project" means every symbol in every library
//! listed in the project's own *local* `sym-lib-table` — via
//! `library_import::load_project_symbols`, the exact same project-wide
//! scan `library_import_ui`'s cherry-pick dialog already uses for a
//! *source* project, just pointed at the current one instead. This is
//! deliberately not a schematic/BOM-instance parse (i.e. not "every
//! symbol actually placed on a sheet, deduplicated by reference
//! designator") — no `.kicad_sch` parsing exists in this codebase, and
//! sym-lib-table scanning already gives the intended safety property for
//! free: it only ever touches libraries the project itself registered
//! locally, never KiCad's global/system libraries (see
//! `load_project_local_table`'s own docs).
//!
//! Structurally a near-copy of `library_import_ui.rs`: a genuine second
//! OS window (`show_viewport_immediate`, not a floating `egui::Window` —
//! see that file's docs for why), the same table/checkbox/log-pane
//! layout. Since rows can now come from more than one library file,
//! `run_lookup_batch` groups selected rows by their own `sym_lib_path`
//! and opens/patches/saves each library once, rather than assuming a
//! single destination file the way the first version of this feature
//! did.
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

use kicad_auto_importer_core::library_import::{load_project_symbols, SourceSymbol};
use kicad_auto_importer_core::parts_lookup::{self, PartsCredentials};
use kicad_auto_importer_core::symbol_importer::SymbolLibrary;

use crate::theme::{self, ACCENT, DANGER, OK};

enum LookupEvent {
    Log(String),
    RowResult {
        index: usize,
        ok: bool,
        summary: String,
    },
    Done,
}

/// What a single row's last lookup found — shown in the table's Result
/// column and kept until the next reload or batch.
struct RowResult {
    ok: bool,
    summary: String,
}

/// A checked row, snapshotted at the moment "Populate BOM" is clicked —
/// owns its own `sym_lib_path` (from `SourceSymbol`) since rows can now
/// come from any of the project's registered libraries, not just one.
struct SelectedRow {
    index: usize,
    name: String,
    sym_lib_path: PathBuf,
}

#[derive(Default)]
pub struct PartLookupState {
    pub open: bool,
    rows: Vec<SourceSymbol>,
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
}

impl PartLookupState {
    fn log(&mut self, msg: impl Into<String>) {
        self.log_lines.push(msg.into());
    }

    fn load_from(&mut self, project_dir: &Path) {
        let mut lines = Vec::new();
        self.rows = load_project_symbols(project_dir, |m| lines.push(m.to_string()));
        for line in lines {
            self.log(line);
        }
        self.checked.clear();
        self.last_clicked = None;
        self.results.clear();
        self.progress_done = 0;
        self.progress_total = 0;
        self.loaded_from = Some(project_dir.to_path_buf());
        self.log(format!(
            "Found {} symbol(s) across the project's registered libraries.",
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
                    LookupEvent::RowResult { index, ok, summary } => {
                        results.push((index, ok, summary));
                    }
                    LookupEvent::Done => done = true,
                }
            }
        }
        for line in lines {
            self.log(line);
        }
        for (index, ok, summary) in results {
            self.progress_done += 1;
            self.results.insert(index, RowResult { ok, summary });
        }
        if done {
            self.in_progress = false;
            self.rx = None;
        }
    }

    fn look_up_selected(&mut self, credentials: PartsCredentials) {
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
                name: row.name.clone(),
                sym_lib_path: row.sym_lib_path.clone(),
            })
            .collect();

        self.progress_done = 0;
        self.progress_total = selected.len();

        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.in_progress = true;

        thread::Builder::new()
            .name("part-lookup".into())
            .spawn(move || run_lookup_batch(selected, credentials, tx))
            .expect("failed to spawn the part-lookup thread");
    }
}

/// Runs on the background thread spawned by `look_up_selected`. Groups
/// the selected rows by which library they actually live in and opens
/// each library once (not per-symbol), saving once per library at the
/// end — same batching `library_import::import_symbols` uses for its
/// own destination library, and for the same reason: cheap, and avoids
/// re-reading/re-writing a whole file for every single row in it.
fn run_lookup_batch(
    selected: Vec<SelectedRow>,
    credentials: PartsCredentials,
    tx: mpsc::Sender<LookupEvent>,
) {
    let send_log = |msg: String| {
        let _ = tx.send(LookupEvent::Log(msg));
    };
    let send_result = |index: usize, ok: bool, summary: String| {
        let _ = tx.send(LookupEvent::RowResult { index, ok, summary });
    };

    let mut by_library: HashMap<PathBuf, Vec<SelectedRow>> = HashMap::new();
    for row in selected {
        by_library
            .entry(row.sym_lib_path.clone())
            .or_default()
            .push(row);
    }

    let mut ok_count = 0usize;
    let mut err_count = 0usize;

    for (path, rows) in by_library {
        let mut lib = match SymbolLibrary::open(&path) {
            Ok(lib) => lib,
            Err(exc) => {
                send_log(format!(
                    "\u{2718} Could not open '{}': {exc}",
                    path.display()
                ));
                for row in &rows {
                    send_result(row.index, false, format!("could not open library: {exc}"));
                    err_count += 1;
                }
                continue;
            }
        };

        let mut any_ok = false;
        for row in &rows {
            let Some(mut node) = lib.get_symbol_node(&row.name) else {
                send_log(format!(
                    "\u{2718} '{}': no longer in the library, skipped.",
                    row.name
                ));
                send_result(row.index, false, "no longer in library".to_string());
                err_count += 1;
                continue;
            };
            let mpn = parts_lookup::resolve_mpn(&node, &row.name);
            send_log(format!("Looking up '{}' (as '{mpn}')\u{2026}", row.name));

            match parts_lookup::lookup_part_info(&credentials, &mpn) {
                Ok(info) => {
                    let vendors: Vec<&str> =
                        info.offers.iter().map(|o| o.seller.as_str()).collect();
                    for warning in &info.warnings {
                        send_log(format!("  \u{26a0} '{}': {warning}", row.name));
                    }
                    parts_lookup::apply_part_info(&mut node, &info);
                    lib.add_symbol(&row.name, &node, true);
                    let summary = format!(
                        "{} \u{2014} {}",
                        info.manufacturer,
                        if vendors.is_empty() {
                            "no Mouser/DigiKey offers found".to_string()
                        } else {
                            vendors.join(", ")
                        }
                    );
                    send_log(format!("\u{2714} '{}': {summary}", row.name));
                    send_result(row.index, true, summary);
                    ok_count += 1;
                    any_ok = true;
                }
                Err(exc) => {
                    send_log(format!("\u{2718} '{}': {exc}", row.name));
                    send_result(row.index, false, exc.to_string());
                    err_count += 1;
                }
            }
        }

        if any_ok {
            if let Err(exc) = lib.save() {
                send_log(format!(
                    "\u{2718} Could not save '{}': {exc}",
                    path.display()
                ));
            }
        }
    }

    send_log(format!("Done: {ok_count} updated, {err_count} error(s)."));
    let _ = tx.send(LookupEvent::Done);
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
        .with_inner_size([900.0, 620.0])
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
                let fraction = state.progress_done as f32 / state.progress_total as f32;
                ui.add(
                    egui::ProgressBar::new(fraction)
                        .text(format!("{}/{}", state.progress_done, state.progress_total))
                        .fill(ACCENT),
                );
            }

            ui.add_space(6.0);
            egui::Frame::group(ui.style())
                .fill(Color32::from_rgb(0x0d, 0x0e, 0x11))
                .inner_margin(egui::Margin::same(8.0))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    egui::ScrollArea::vertical()
                        .max_height(120.0)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            ui.add(
                                egui::TextEdit::multiline(&mut state.log_lines.join("\n"))
                                    .desired_width(f32::INFINITY)
                                    .frame(false)
                                    .font(egui::TextStyle::Monospace)
                                    .interactive(false),
                            );
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
                    state.look_up_selected(credentials.clone());
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
                        TableBuilder::new(ui)
                            .id_salt("part_lookup_table")
                            .striped(true)
                            .resizable(true)
                            .sense(egui::Sense::click())
                            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                            .column(Column::exact(24.0))
                            .column(Column::initial(110.0).at_least(60.0).clip(true))
                            .column(Column::initial(160.0).at_least(80.0).clip(true))
                            .column(Column::initial(90.0).at_least(50.0).clip(true))
                            .column(Column::initial(220.0).at_least(100.0).clip(true))
                            .column(Column::remainder().at_least(160.0).clip(true))
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
                                    ui.strong("Type");
                                });
                                header.col(|ui| {
                                    ui.strong("Description");
                                });
                                header.col(|ui| {
                                    ui.strong("Result");
                                });
                            })
                            .body(|body| {
                                body.rows(22.0, state.rows.len(), |mut row| {
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
                                        ui.label(&r.library);
                                    });
                                    row.col(|ui| {
                                        ui.label(&r.name);
                                    });
                                    row.col(|ui| {
                                        ui.label(&r.type_label);
                                    });
                                    row.col(|ui| {
                                        ui.add(egui::Label::new(&r.description).truncate());
                                    });
                                    row.col(|ui| {
                                        if let Some(result) = state.results.get(&i) {
                                            let (glyph, color) = if result.ok {
                                                (icon::CHECK_CIRCLE, OK)
                                            } else {
                                                (icon::X_CIRCLE, DANGER)
                                            };
                                            ui.horizontal(|ui| {
                                                ui.label(RichText::new(glyph).color(color));
                                                ui.add(
                                                    egui::Label::new(&result.summary).truncate(),
                                                );
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
