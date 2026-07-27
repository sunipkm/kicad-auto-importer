//! The main application window — mirrors the layout of the sibling
//! Python plugin's `plugins/ui/main_dialog.py`: path fields with
//! Browse… buttons, an options group, a start/stop watch toggle, manual
//! one-shot import buttons, and a log pane.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use egui::{Color32, RichText, Stroke};
use egui_phosphor::regular as icon;
use kicad_auto_importer_core::config::ImporterConfig;
use kicad_auto_importer_core::kicad_paths::{expand_kicad_vars, kiprjmod_relative_uri};
use kicad_auto_importer_core::library_import::CrossImportSettings;
use kicad_auto_importer_core::watcher::{FolderWatcher, WatchEvent};
use kicad_auto_importer_core::zip_importer::{
    import_folder, import_zip, validate_model_subdir, ImportSettings,
};

use crate::library_import_ui::{self, LibraryImportState};
use crate::theme::{self, ACCENT, DANGER, TITLE_BAR_BG};
use crate::window_chrome;

pub struct MainApp {
    project_path: String, // absolute path to the project directory, or "" if none chosen yet
    watch_folder: String,
    symbol_lib: String,
    footprint_lib: String,
    model_subdir: String,
    move_zip: bool,
    backup_zip: bool,
    overwrite: bool,

    watcher: Option<FolderWatcher>,
    log_rx: Option<mpsc::Receiver<WatchEvent>>,
    log_lines: Vec<String>,
    status: String,

    library_import: LibraryImportState,
}

impl Default for MainApp {
    fn default() -> Self {
        MainApp {
            project_path: String::new(),
            watch_folder: String::new(),
            symbol_lib: String::new(),
            footprint_lib: String::new(),
            model_subdir: "3dmodels".to_string(),
            move_zip: false,
            backup_zip: true,
            overwrite: false,
            watcher: None,
            log_rx: None,
            log_lines: Vec::new(),
            status: String::new(),
            library_import: LibraryImportState::default(),
        }
    }
}

impl MainApp {
    fn log(&mut self, msg: impl Into<String>) {
        self.log_lines.push(msg.into());
    }

    fn project_dir(&self) -> Option<PathBuf> {
        if self.project_path.is_empty() {
            None
        } else {
            Some(PathBuf::from(&self.project_path))
        }
    }

    fn to_display(&self, abs: &Path) -> String {
        match self.project_dir() {
            Some(dir) if !dir.as_os_str().is_empty() => kiprjmod_relative_uri(abs, &dir),
            _ => abs.to_string_lossy().replace('\\', "/"),
        }
    }

    fn to_absolute(&self, display: &str) -> PathBuf {
        if display.is_empty() {
            return PathBuf::new();
        }
        let project_dir = self.project_path.clone();
        let expanded = expand_kicad_vars(display, Some(&project_dir));
        PathBuf::from(expanded)
    }

    fn load_config_for_current_project(&mut self) {
        let Some(project_dir) = self.project_dir() else {
            return;
        };
        let cfg = ImporterConfig::load(&project_dir);
        if !cfg.watch_folder.is_empty() {
            self.watch_folder = cfg.watch_folder;
        }
        if !cfg.symbol_lib.is_empty() {
            self.symbol_lib = cfg.symbol_lib;
        }
        if !cfg.footprint_lib.is_empty() {
            self.footprint_lib = cfg.footprint_lib;
        }
        self.model_subdir = cfg.model_subdir;
        self.move_zip = cfg.move_zip;
        self.backup_zip = cfg.backup_zip;
        self.overwrite = cfg.overwrite;
        self.log(format!(
            "Loaded settings for project '{}'.",
            project_dir.display()
        ));
    }

    fn save_config(&mut self) {
        let Some(project_dir) = self.project_dir() else {
            return;
        };
        let cfg = ImporterConfig {
            watch_folder: self.watch_folder.clone(),
            symbol_lib: self.symbol_lib.clone(),
            footprint_lib: self.footprint_lib.clone(),
            model_subdir: self.model_subdir.clone(),
            move_zip: self.move_zip,
            backup_zip: self.backup_zip,
            overwrite: self.overwrite,
        };
        if let Err(exc) = cfg.save(&project_dir) {
            self.log(format!("\u{2718} Could not save config: {exc}"));
        }
    }

    fn build_settings(&mut self, with_watch_folder: bool) -> Option<ImportSettings> {
        let Some(project_path) = self.project_dir() else {
            self.status = "Choose a KiCad project first.".to_string();
            return None;
        };
        if self.symbol_lib.trim().is_empty() || self.footprint_lib.trim().is_empty() {
            self.status = "Set both the symbol library and footprint library paths.".to_string();
            return None;
        }
        let model_subdir = match validate_model_subdir(&self.model_subdir) {
            Ok(s) => s,
            Err(msg) => {
                self.status = msg;
                return None;
            }
        };

        let watch_folder = if with_watch_folder {
            if self.watch_folder.trim().is_empty() {
                self.status = "Set a watch folder first.".to_string();
                return None;
            }
            Some(self.to_absolute(&self.watch_folder))
        } else {
            None
        };

        self.status.clear();
        Some(ImportSettings {
            symbol_lib: self.to_absolute(&self.symbol_lib),
            footprint_lib: self.to_absolute(&self.footprint_lib),
            project_path,
            model_subdir,
            overwrite: self.overwrite,
            watch_folder,
            move_zip: self.move_zip,
            backup_zip: self.backup_zip,
        })
    }

    fn start_watching(&mut self) {
        let Some(settings) = self.build_settings(true) else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        match FolderWatcher::start(settings, tx) {
            Ok(watcher) => {
                self.watcher = Some(watcher);
                self.log_rx = Some(rx);
                self.save_config();
            }
            Err(exc) => {
                self.status = format!("Could not start watching: {exc}");
            }
        }
    }

    fn stop_watching(&mut self) {
        if let Some(watcher) = self.watcher.take() {
            watcher.stop();
        }
        self.log_rx = None;
        self.save_config();
    }

    fn drain_log_channel(&mut self) {
        let mut lines = Vec::new();
        if let Some(rx) = &self.log_rx {
            while let Ok(event) = rx.try_recv() {
                let WatchEvent::Log(msg) = event;
                lines.push(msg);
            }
        }
        for line in lines {
            self.log(line);
        }
    }

    fn import_zip_dialog(&mut self) {
        let Some(paths) = rfd::FileDialog::new()
            .add_filter("ZIP archive", &["zip"])
            .pick_files()
        else {
            return;
        };
        let Some(settings) = self.build_settings(false) else {
            return;
        };
        for path in paths {
            self.log(format!("Importing {}\u{2026}", path.display()));
            let mut lines = Vec::new();
            let result = import_zip(&path, &settings, |m| lines.push(m.to_string()));
            for line in lines {
                self.log(line);
            }
            match result {
                Ok(summary) => self.log(format!(
                    "\u{2714} Imported {}: {summary}",
                    path.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()
                )),
                Err(exc) => self.log(format!("\u{2718} Error: {exc}")),
            }
        }
        self.save_config();
    }

    fn import_folder_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        let Some(settings) = self.build_settings(false) else {
            return;
        };
        self.log(format!("Scanning folder {}\u{2026}", path.display()));
        let mut lines = Vec::new();
        let result = import_folder(&path, &settings, |m| lines.push(m.to_string()));
        for line in lines {
            self.log(line);
        }
        match result {
            Ok(summary) => self.log(format!("\u{2714} Imported: {summary}")),
            Err(exc) => self.log(format!("\u{2718} Error: {exc}")),
        }
        self.save_config();
    }

    /// Validates and saves the destination settings first (same pattern
    /// as every other import action here), then opens the cherry-pick
    /// dialog — mirrors the Python plugin's `_import_from_library`, which
    /// does the same before showing its `ImportLibraryDialog`.
    fn open_library_import(&mut self) {
        if self.build_settings(false).is_none() {
            return;
        }
        self.save_config();
        self.library_import.open = true;
    }

    fn browse_project(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("KiCad project", &["kicad_pro"])
            .pick_file()
        {
            let dir = path.parent().map(|p| p.to_path_buf()).unwrap_or(path);
            self.project_path = dir.to_string_lossy().to_string();
            self.load_config_for_current_project();
        }
    }

    fn browse_watch_folder(&mut self) {
        if let Some(path) = rfd::FileDialog::new().pick_folder() {
            self.watch_folder = self.to_display(&path);
        }
    }

    fn browse_symbol_lib(&mut self) {
        // Deliberately `pick_file`, not `save_file`: this dialog is for
        // *selecting* the destination library, which the import pipeline
        // opens with `SymbolLibrary::open_or_create` and only ever
        // appends/patches into — never replaces wholesale. `save_file`
        // would pop the OS's native "this file already exists, overwrite?"
        // confirmation for the ordinary case of pointing at a library you
        // already have, which is misleading (nothing gets overwritten) and
        // scary for no reason. A not-yet-created library name can still be
        // typed directly into the (editable) field next to this button.
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("KiCad symbol library", &["kicad_sym"])
            .pick_file()
        {
            self.symbol_lib = self.to_display(&path);
        }
    }

    fn browse_footprint_lib(&mut self) {
        if let Some(mut path) = rfd::FileDialog::new().pick_folder() {
            if !path.extension().is_some_and(|e| e == "pretty") {
                path.set_extension("pretty");
            }
            self.footprint_lib = self.to_display(&path);
        }
    }
}

/// A labeled path field that fills all remaining horizontal space in
/// its row (rather than a fixed width), with an optional trailing
/// Browse button. Returns whether that button was clicked — callers
/// handle the actual file/folder dialog afterward, once this function's
/// borrow of `value` has ended, so they can freely call other `&mut
/// self` methods in response.
fn path_row(
    ui: &mut egui::Ui,
    row_icon: &str,
    label: &str,
    value: &mut String,
    with_browse: bool,
) -> bool {
    let mut clicked = false;
    ui.horizontal(|ui| {
        ui.add_sized(
            [26.0, 26.0],
            egui::Label::new(RichText::new(row_icon).size(16.0).color(ACCENT)),
        );
        ui.add_sized([200.0, 26.0], egui::Label::new(label));
        // Lay out right-to-left so the Browse button claims its natural
        // size first and the text field fills exactly what's left —
        // no hand-computed width to drift out of sync with the button.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if with_browse
                && ui
                    .add_sized(
                        [106.0, 26.0],
                        egui::Button::new(format!("{} Browse\u{2026}", icon::FOLDER_OPEN)),
                    )
                    .clicked()
            {
                clicked = true;
            }
            let remaining = ui.available_width().max(80.0);
            ui.add_sized([remaining, 26.0], egui::TextEdit::singleline(value));
        });
    });
    clicked
}

impl eframe::App for MainApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_log_channel();
        if self.watcher.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }

        egui::TopBottomPanel::top("title_bar")
            .exact_height(window_chrome::BAR_HEIGHT)
            .frame(egui::Frame::none().fill(TITLE_BAR_BG))
            .show_separator_line(false)
            .show(ctx, |ui| {
                window_chrome::title_bar(ui, ctx, "KiCad Auto Importer")
            });

        let panel_frame = egui::Frame::central_panel(&ctx.style())
            .inner_margin(egui::Margin::symmetric(16.0, 14.0));
        egui::CentralPanel::default()
            .frame(panel_frame)
            .show(ctx, |ui| {
                // The whole form lives inside a vertical scroll area rather
                // than relying on the window always being tall enough for
                // every row: extra rows (like the status message below)
                // then just push the scroll extent down instead of
                // overflowing past the window's bottom edge.
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let watching = self.watcher.is_some();
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(if watching {
                                    icon::CIRCLE_HALF
                                } else {
                                    icon::CIRCLE
                                })
                                .color(if watching {
                                    theme::OK
                                } else {
                                    Color32::from_gray(120)
                                }),
                            );
                            ui.label(
                                RichText::new(if watching {
                                    "Watching for new packages\u{2026}"
                                } else {
                                    "Idle"
                                })
                                .weak(),
                            );
                        });
                        ui.add_space(4.0);

                        if path_row(
                            ui,
                            icon::FOLDER,
                            "KiCad Project (.kicad_pro):",
                            &mut self.project_path,
                            true,
                        ) {
                            self.browse_project();
                        }
                        if path_row(ui, icon::EYE, "Watch folder:", &mut self.watch_folder, true) {
                            self.browse_watch_folder();
                        }
                        if path_row(
                            ui,
                            icon::FILE_CODE,
                            "Symbol library (.kicad_sym):",
                            &mut self.symbol_lib,
                            true,
                        ) {
                            self.browse_symbol_lib();
                        }
                        if path_row(
                            ui,
                            icon::CUBE,
                            "Footprint library (.pretty):",
                            &mut self.footprint_lib,
                            true,
                        ) {
                            self.browse_footprint_lib();
                        }
                        path_row(
                            ui,
                            icon::CUBE_TRANSPARENT,
                            "3-D model subfolder:",
                            &mut self.model_subdir,
                            false,
                        );
                        ui.label(
                            RichText::new("Relative to the project directory (${KIPRJMOD}).")
                                .small()
                                .weak(),
                        );

                        ui.add_space(10.0);
                        ui.group(|ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(icon::SLIDERS).color(ACCENT));
                                ui.label(RichText::new("Options").strong());
                            });
                            ui.add_space(2.0);
                            ui.checkbox(
                                &mut self.move_zip,
                                "Move (not copy) source ZIP/folder after successful import",
                            );
                            ui.checkbox(
                                &mut self.backup_zip,
                                "Keep a timestamped backup of imported ZIPs in the watch folder",
                            );
                            ui.checkbox(
                                &mut self.overwrite,
                                "Overwrite existing symbols / footprints with the same name",
                            );
                        });

                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            let toggle_size = egui::vec2(190.0, 44.0);
                            let action_size = egui::vec2(180.0, 44.0);

                            let toggle_resp = if watching {
                                ui.add_sized(
                                    toggle_size,
                                    egui::Button::new(format!("{}  Stop Watching", icon::STOP))
                                        .fill(Color32::from_rgb(0x5a, 0x24, 0x24))
                                        .stroke(Stroke::new(1.0_f32, DANGER)),
                                )
                            } else {
                                ui.add_sized(
                                    toggle_size,
                                    theme::accent_button(format!("{}  Start Watching", icon::PLAY)),
                                )
                            };
                            if toggle_resp.clicked() {
                                if watching {
                                    self.stop_watching();
                                } else {
                                    self.start_watching();
                                }
                            }

                            if ui
                                .add_sized(
                                    action_size,
                                    egui::Button::new(format!(
                                        "{}  Import ZIP\u{2026}",
                                        icon::FILE_ZIP
                                    )),
                                )
                                .clicked()
                            {
                                self.import_zip_dialog();
                            }
                            if ui
                                .add_sized(
                                    action_size,
                                    egui::Button::new(format!(
                                        "{}  Import Folder\u{2026}",
                                        icon::FOLDER_OPEN
                                    )),
                                )
                                .clicked()
                            {
                                self.import_folder_dialog();
                            }
                            if ui
                                .add_sized(
                                    action_size,
                                    egui::Button::new(format!(
                                        "{}  Another Project\u{2026}",
                                        icon::LINK_SIMPLE
                                    )),
                                )
                                .clicked()
                            {
                                self.open_library_import();
                            }
                        });

                        if !self.status.is_empty() {
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(icon::WARNING_CIRCLE).color(DANGER));
                                ui.colored_label(DANGER, &self.status);
                            });
                        }

                        ui.add_space(10.0);
                        // A fixed height for the log's inner scroll area — deliberately
                        // *not* "whatever's left in the window", since the whole form
                        // now lives inside its own outer scroll area (see above) where
                        // extra rows (like the status message) push the scroll extent
                        // instead of shrinking anything. That's what actually stops
                        // this panel from being squeezed or pushed past the window.
                        egui::Frame::group(ui.style())
                            .fill(Color32::from_rgb(0x0d, 0x0e, 0x11))
                            .inner_margin(egui::Margin::same(10.0))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(icon::TERMINAL_WINDOW).color(ACCENT));
                                    ui.label(RichText::new("Activity Log").strong());
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui
                                                .add_sized(
                                                    [104.0, 28.0],
                                                    egui::Button::new(format!(
                                                        "{}  Clear",
                                                        icon::TRASH
                                                    )),
                                                )
                                                .clicked()
                                            {
                                                self.log_lines.clear();
                                            }
                                        },
                                    );
                                });
                                ui.add_space(6.0);
                                egui::ScrollArea::vertical()
                                    .max_height(240.0)
                                    .stick_to_bottom(true)
                                    .show(ui, |ui| {
                                        ui.add(
                                            egui::TextEdit::multiline(
                                                &mut self.log_lines.join("\n"),
                                            )
                                            .desired_width(f32::INFINITY)
                                            .desired_rows(20)
                                            .frame(false)
                                            .font(egui::TextStyle::Monospace)
                                            .interactive(false),
                                        );
                                    });
                            });
                    });
            });

        if self.library_import.open {
            let dest_settings = CrossImportSettings {
                symbol_lib: self.to_absolute(&self.symbol_lib),
                footprint_lib: self.to_absolute(&self.footprint_lib),
                project_path: self.project_dir().unwrap_or_default(),
                model_subdir: self.model_subdir.clone(),
                overwrite: self.overwrite,
            };
            library_import_ui::show(&mut self.library_import, ctx, &dest_settings);
        }

        window_chrome::resize_grip(ctx, "main");
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop_watching();
    }
}
