//! "Import From Another Project" — cherry-pick symbols (with their
//! linked footprint + 3-D models tagging along) from another KiCad
//! project's own project-local libraries into the currently-open one.
//! Mirrors the sibling Python plugin's separate `ImportLibraryDialog`.
//!
//! Drawn as a genuine separate native OS window via
//! `Context::show_viewport_immediate`, not a floating `egui::Window`
//! embedded in the main one — a floating window is confined to the main
//! window's own OS-level bounds, which made the symbol table too cramped
//! (and effectively unresizable/unscrollable in practice) to be usable.
//! A real second window gets its own OS-managed resize handles and
//! plenty of room to scroll in.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

use egui::{Color32, RichText};
use egui_extras::{Column, TableBuilder};
use egui_phosphor::regular as icon;

use kicad_auto_importer_core::library_import::{
    import_symbols, load_project_symbols, load_symbols_from_file, project_dir_from_pro_file,
    CrossImportSettings, SourceSymbol,
};

use crate::theme::{self, ACCENT, DANGER};

enum ImportEvent {
    Log(String),
    Progress {
        done: usize,
        total: usize,
        current: String,
    },
    Done(String),
}

#[derive(Default)]
pub struct LibraryImportState {
    pub open: bool,
    source_pro_display: String,
    source_project_dir: Option<PathBuf>,
    rows: Vec<SourceSymbol>,
    checked: HashSet<usize>,
    /// The row a plain/ctrl click last landed on — the anchor a
    /// subsequent shift-click extends a contiguous range from.
    last_clicked: Option<usize>,
    log_lines: Vec<String>,
    status: String,
    rx: Option<mpsc::Receiver<ImportEvent>>,
    in_progress: bool,
    progress_done: usize,
    progress_total: usize,
    /// The symbol currently being imported — shown alongside the
    /// progress bar so a long batch shows *what's* happening, not just
    /// how much of it is left.
    current_item: String,
}

impl LibraryImportState {
    fn log(&mut self, msg: impl Into<String>) {
        self.log_lines.push(msg.into());
    }

    fn load_from_project(&mut self, dir: PathBuf) {
        let mut lines = Vec::new();
        self.rows = load_project_symbols(&dir, |m| lines.push(m.to_string()));
        for line in lines {
            self.log(line);
        }
        self.checked.clear();
        self.last_clicked = None;
        self.source_project_dir = Some(dir);
        self.log(format!("Found {} symbol(s).", self.rows.len()));
    }

    /// Plain and ctrl clicks both just toggle the clicked row, leaving
    /// every other row's checked state untouched (there's no "select
    /// only this one and clear the rest" behavior here, matching this
    /// being a checkbox list rather than a single-selection list — bulk
    /// changes go through Select All/None). A shift click instead checks
    /// every row between the last plain/ctrl-clicked row and this one,
    /// inclusive, without moving that anchor, so repeated shift-clicks
    /// keep extending/redefining the range from the same starting point.
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

    fn browse_source_project(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("KiCad project", &["kicad_pro"])
            .pick_file()
        {
            self.source_pro_display = path.to_string_lossy().to_string();
            let dir = project_dir_from_pro_file(&path);
            self.load_from_project(dir);
        }
    }

    fn reload(&mut self) {
        if let Some(dir) = self.source_project_dir.clone() {
            self.load_from_project(dir);
        } else {
            self.status = "Choose a source project first.".to_string();
        }
    }

    fn add_symbols_from_file(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("KiCad symbol library", &["kicad_sym"])
            .pick_file()
        else {
            return;
        };
        let label = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let added = load_symbols_from_file(&path, Some(&label));
        self.log(format!(
            "Added {} symbol(s) from {}.",
            added.len(),
            path.display()
        ));
        self.rows.extend(added);
        self.rows.sort_by(|a, b| {
            (a.library.to_lowercase(), a.name.to_lowercase())
                .cmp(&(b.library.to_lowercase(), b.name.to_lowercase()))
        });
    }

    /// Runs the actual import on a background thread — importing several
    /// symbols (each with footprint + 3-D model copies) can take a
    /// visible moment, and blocking the GUI thread for it would freeze
    /// the window and make a progress bar pointless.
    fn import_selected(&mut self, dest: &CrossImportSettings) {
        let Some(source_project_dir) = self.source_project_dir.clone() else {
            self.status = "Choose a source project first.".to_string();
            return;
        };
        if self.checked.is_empty() {
            self.status = "Select at least one symbol first.".to_string();
            return;
        }
        self.status.clear();

        let selected: Vec<SourceSymbol> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(i, _)| self.checked.contains(i))
            .map(|(_, row)| row.clone())
            .collect();

        self.progress_done = 0;
        self.progress_total = selected.len();
        self.current_item.clear();

        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.in_progress = true;

        let dest = dest.clone();
        thread::Builder::new()
            .name("library-import".into())
            .spawn(move || {
                let selected_refs: Vec<&SourceSymbol> = selected.iter().collect();
                let tx_log = tx.clone();
                let tx_progress = tx.clone();
                let summary = import_symbols(
                    &selected_refs,
                    &source_project_dir,
                    &dest,
                    move |m| {
                        let _ = tx_log.send(ImportEvent::Log(m.to_string()));
                    },
                    move |done, total, current| {
                        let _ = tx_progress.send(ImportEvent::Progress {
                            done,
                            total,
                            current: current.to_string(),
                        });
                    },
                );
                let _ = tx.send(ImportEvent::Done(summary));
            })
            .expect("failed to spawn the library-import thread");
    }

    fn drain_channel(&mut self) {
        let mut lines = Vec::new();
        let mut done_summary = None;
        if let Some(rx) = &self.rx {
            while let Ok(event) = rx.try_recv() {
                match event {
                    ImportEvent::Log(msg) => lines.push(msg),
                    ImportEvent::Progress {
                        done,
                        total,
                        current,
                    } => {
                        self.progress_done = done;
                        self.progress_total = total;
                        self.current_item = current;
                    }
                    ImportEvent::Done(summary) => done_summary = Some(summary),
                }
            }
        }
        for line in lines {
            self.log(line);
        }
        if let Some(summary) = done_summary {
            self.log(format!("\u{2714} Done: {summary}"));
            self.progress_done = self.progress_total;
            self.current_item.clear();
            self.in_progress = false;
            self.rx = None;
        }
    }
}

/// Sizes a table row tall enough to show a wrapped multi-line
/// description instead of clipping it to one line. Capped so a single
/// absurdly long description can't make one row take over the whole
/// table.
///
/// Measures the real wrapped galley rather than guessing from a
/// characters-per-line constant: a proportional font's characters don't
/// all have the same width, so a fixed "~40 chars ≈ one line" heuristic
/// (the previous approach here) systematically under-counts lines for
/// text with a lot of wide characters, silently clipping the last line
/// or two. `WRAP_WIDTH` has to track the Description column's actual
/// width below (`Column::initial(300.0)`, minus a little slack for cell
/// padding) — there's no getting around that by hand, since row heights
/// must be known *before* `TableBuilder` lays out and resolves real
/// column widths for this frame.
fn description_row_height(ctx: &egui::Context, description: &str) -> f32 {
    const MAX_LINES: usize = 4;
    const WRAP_WIDTH: f32 = 300.0 - 12.0;

    let font_id = egui::TextStyle::Body.resolve(&ctx.style());
    let galley = ctx.fonts(|f| f.layout_delayed_color(description.to_owned(), font_id, WRAP_WIDTH));
    let line_count = galley.rows.len().max(1);
    let line_height = galley.rect.height() / line_count as f32;
    line_height * line_count.min(MAX_LINES) as f32 + 8.0
}

pub fn show(state: &mut LibraryImportState, ctx: &egui::Context, dest: &CrossImportSettings) {
    if !state.open {
        return;
    }

    state.drain_channel();
    // Polled every frame while a batch is running so results/log lines
    // show up promptly instead of waiting for the next unrelated repaint.
    if state.in_progress {
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
    }

    let viewport_id = egui::ViewportId::from_hash_of("library_import_window");
    let builder = egui::ViewportBuilder::default()
        .with_title("Import From Another Project")
        .with_inner_size([980.0, 640.0])
        .with_min_inner_size([620.0, 420.0])
        // Undecorated, with the same custom title bar as the main
        // window (drawn below) rather than the OS's native one — one
        // window using custom chrome and the other native looked
        // inconsistent, and this one also carries the app icon (see
        // `crate::icon`) instead of falling back to a generic one.
        .with_decorations(false)
        .with_icon(crate::icon::app_icon());

    ctx.show_viewport_immediate(viewport_id, builder, |ctx, _class| {
        if ctx.input(|i| i.viewport().close_requested()) {
            state.open = false;
        }

        egui::TopBottomPanel::top("library_import_title_bar")
            .exact_height(crate::window_chrome::BAR_HEIGHT)
            .frame(egui::Frame::none().fill(crate::theme::TITLE_BAR_BG))
            .show_separator_line(false)
            .show(ctx, |ui| {
                crate::window_chrome::title_bar(ui, ctx, "Import From Another Project")
            });

        // Header and footer are real panels, not rows inside a shared
        // scroll area — that's what makes the table's `CentralPanel`,
        // added last, get exactly "whatever's left" from egui's panel
        // layout system (CentralPanel always sizes itself to the true
        // remainder after every other panel), with no fragile manual
        // height subtraction, and without the header/footer scrolling
        // along with the table.
        egui::TopBottomPanel::top("library_import_top").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                // Explicit row height, matching the main window's title
                // bar technique, so the label/field are vertically
                // centered against the row's real height rather than a
                // guessed one. Buttons are deliberately *not* forced to a
                // size below with `add_sized`: a `Button` only shrinks to
                // fit if its text wraps or truncates, and neither is set
                // here, so squeezing it into a size too small for its
                // text just makes it silently render wider/taller than
                // requested instead — which is what made "Browse…"
                // (longer text) visibly mismatched against "Reload"
                // (shorter text) even though both asked for the same
                // size. Left to size themselves naturally, same-ish-
                // length button labels end up the same-ish size anyway.
                ui.set_height(30.0);
                ui.label(RichText::new(icon::FOLDER).color(ACCENT));
                ui.add_sized(
                    [120.0, 30.0],
                    egui::Label::new("Source project (.kicad_pro):"),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button(format!("{} Browse\u{2026}", icon::FOLDER_OPEN))
                        .clicked()
                    {
                        state.browse_source_project();
                    }
                    if ui
                        .button(format!("{} Reload", icon::ARROW_CLOCKWISE))
                        .clicked()
                    {
                        state.reload();
                    }
                    let remaining = ui.available_width().max(80.0);
                    ui.add_sized(
                        [remaining, 30.0],
                        egui::TextEdit::singleline(&mut state.source_pro_display)
                            .interactive(false),
                    );
                });
            });

            ui.add_space(4.0);
            if ui
                .button(format!(
                    "{}  Add symbols from a specific library file\u{2026}",
                    icon::FILE_CODE
                ))
                .clicked()
            {
                state.add_symbols_from_file();
            }

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

        egui::TopBottomPanel::bottom("library_import_bottom").show(ctx, |ui| {
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
                        RichText::new(format!("Importing '{}'\u{2026}", state.current_item))
                            .small()
                            .weak(),
                    );
                }
                let fraction = state.progress_done as f32 / state.progress_total as f32;
                let resp = ui.add(egui::ProgressBar::new(fraction).fill(ACCENT));
                // `ProgressBar::text` (egui 0.29) always left-aligns, so
                // the done/total count is painted centered over the
                // bar's own rect by hand instead.
                ui.painter().text(
                    resp.rect.center(),
                    egui::Align2::CENTER_CENTER,
                    format!("{}/{}", state.progress_done, state.progress_total),
                    egui::FontId::proportional(13.0),
                    Color32::WHITE,
                );
            }

            ui.add_space(6.0);
            // Collapsed by default — the progress bar above already
            // covers "is it working and on what", so the line-by-line
            // detail log only needs to be opened when something needs
            // digging into.
            egui::CollapsingHeader::new(format!("{}  Detail Log", icon::TERMINAL_WINDOW))
                .id_salt("library_import_log_collapse")
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
                                    // Deliberately interactive (not
                                    // `.interactive(false)`): the content
                                    // is rebuilt from `log_lines` every
                                    // frame regardless of what's typed
                                    // into it, so it can't actually be
                                    // edited, but leaving it interactive
                                    // is what lets drag-select and
                                    // Ctrl+C copy the log text out.
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
                // Natural sizing here too — see the comment on the
                // Browse/Reload row above for why forcing an
                // under-sized `add_sized` on a button backfires instead
                // of shrinking it.
                let label = if state.in_progress {
                    format!("{}  Importing\u{2026}", icon::SPINNER)
                } else {
                    format!("{}  Import Selected", icon::DOWNLOAD)
                };
                if ui
                    .add_enabled(!state.in_progress, theme::accent_button(label))
                    .clicked()
                {
                    state.import_selected(dest);
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
                // This is the true remainder after the panels above and
                // below — not a guess, so nothing here can overflow past
                // either of them.
                let table_height = ui.available_height().max(100.0);
                // Horizontal scroll for when the columns (all fixed-width
                // below — no `Column::remainder()`, which doesn't have a
                // sensible meaning once the table can scroll sideways)
                // add up to more than the window is wide. This needs its
                // own `id_salt`, distinct from the table's, or the two
                // scroll areas collide (that was the earlier "second use
                // of widget ID" warning).
                egui::ScrollArea::horizontal()
                    .id_salt("library_import_table_hscroll")
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        let row_heights: Vec<f32> = state
                            .rows
                            .iter()
                            .map(|r| description_row_height(ctx, &r.description))
                            .collect();
                        TableBuilder::new(ui)
                            .id_salt("library_import_table")
                            .striped(true)
                            .resizable(true)
                            // Cells otherwise only sense hover, not clicks —
                            // this is what makes `row.response()` below
                            // report clicks for the whole row, not just
                            // individual widgets.
                            .sense(egui::Sense::click())
                            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                            // `.clip(true)` on every column below is load
                            // bearing: `Column::initial` otherwise defaults
                            // to "always grow wide enough to fit content
                            // unwrapped" (per its own docs), which is what
                            // was ballooning the whole table's width and
                            // meant the Description column never needed to
                            // wrap its text in the first place.
                            .column(Column::exact(24.0))
                            .column(Column::initial(90.0).at_least(50.0).clip(true))
                            .column(Column::initial(140.0).at_least(60.0).clip(true))
                            .column(Column::initial(90.0).at_least(50.0).clip(true))
                            .column(Column::initial(44.0).at_least(30.0).clip(true))
                            .column(Column::initial(40.0).at_least(30.0).clip(true))
                            .column(Column::initial(150.0).at_least(60.0).clip(true))
                            .column(Column::initial(300.0).at_least(120.0).clip(true))
                            // 0.0, not `table_height`: matches
                            // `egui_demo_lib`'s own table demo — forcing
                            // a *minimum* scrolled height equal to the
                            // max only ever adds empty space below a
                            // short row list, since `max_scroll_height`
                            // below already caps how tall the table can
                            // grow.
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
                                    ui.strong("Units");
                                });
                                header.col(|ui| {
                                    ui.strong("Pins");
                                });
                                header.col(|ui| {
                                    ui.strong("Footprint");
                                });
                                header.col(|ui| {
                                    ui.strong("Description");
                                });
                            })
                            .body(|body| {
                                body.heterogeneous_rows(row_heights.into_iter(), |mut row| {
                                    let i = row.index();
                                    let checked = state.checked.contains(&i);
                                    row.set_selected(checked);

                                    row.col(|ui| {
                                        // Painted directly — deliberately
                                        // *not* an `egui::Checkbox` widget,
                                        // even a disabled one. Any widget
                                        // here allocates its own interact
                                        // sense on top of the row's, and
                                        // the two competing for the same
                                        // pixels is exactly what made
                                        // clicks feel misaligned before.
                                        // Toggling happens once, below, via
                                        // the whole row's click response.
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
                                        ui.label(r.units.to_string());
                                    });
                                    row.col(|ui| {
                                        ui.label(r.pins.to_string());
                                    });
                                    row.col(|ui| {
                                        ui.label(r.footprint_ref.as_deref().unwrap_or(""));
                                    });
                                    row.col(|ui| {
                                        // Explicit `.wrap()` rather than
                                        // plain `ui.label(...)`: it forces
                                        // multi-line wrapping regardless of
                                        // the ambient wrap mode, which is
                                        // what the taller (heterogeneous)
                                        // row heights above are actually
                                        // for.
                                        ui.add(egui::Label::new(&r.description).wrap());
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

        crate::window_chrome::resize_grip(ctx, "library_import");
    });
}
