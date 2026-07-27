//! The main application window — mirrors the layout of the sibling
//! Python plugin's `plugins/ui/main_dialog.py`: path fields with
//! Browse… buttons, an options group, a start/stop watch toggle, manual
//! one-shot import buttons, and a log pane.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use kicad_auto_importer_core::config::ImporterConfig;
use kicad_auto_importer_core::kicad_paths::{expand_kicad_vars, kiprjmod_relative_uri};
use kicad_auto_importer_core::watcher::{FolderWatcher, WatchEvent};
use kicad_auto_importer_core::zip_importer::{
    import_folder, import_zip, validate_model_subdir, ImportSettings,
};

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
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("KiCad symbol library", &["kicad_sym"])
            .save_file()
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
fn path_row(ui: &mut egui::Ui, label: &str, value: &mut String, with_browse: bool) -> bool {
    let mut clicked = false;
    ui.horizontal(|ui| {
        ui.add_sized([210.0, 22.0], egui::Label::new(label));
        let reserved_for_button = if with_browse { 100.0 } else { 0.0 };
        let remaining = (ui.available_width() - reserved_for_button).max(200.0);
        ui.add_sized([remaining, 22.0], egui::TextEdit::singleline(value));
        if with_browse && ui.button("Browse\u{2026}").clicked() {
            clicked = true;
        }
    });
    clicked
}

impl eframe::App for MainApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_log_channel();
        if self.watcher.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("kicad-auto-importer");
            ui.add_space(6.0);

            if path_row(
                ui,
                "KiCad Project (.kicad_pro):",
                &mut self.project_path,
                true,
            ) {
                self.browse_project();
            }
            if path_row(ui, "Watch folder:", &mut self.watch_folder, true) {
                self.browse_watch_folder();
            }
            if path_row(
                ui,
                "Symbol library (.kicad_sym):",
                &mut self.symbol_lib,
                true,
            ) {
                self.browse_symbol_lib();
            }
            if path_row(
                ui,
                "Footprint library (.pretty):",
                &mut self.footprint_lib,
                true,
            ) {
                self.browse_footprint_lib();
            }
            path_row(ui, "3-D model subfolder:", &mut self.model_subdir, false);
            ui.label(
                egui::RichText::new("Relative to the project directory (${KIPRJMOD}).")
                    .small()
                    .weak(),
            );

            ui.add_space(8.0);
            ui.group(|ui| {
                ui.label("Options");
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

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let watching = self.watcher.is_some();
                let toggle_label = if watching {
                    "\u{23f9} Stop Watching"
                } else {
                    "\u{25b6} Start Watching"
                };
                if ui.button(toggle_label).clicked() {
                    if watching {
                        self.stop_watching();
                    } else {
                        self.start_watching();
                    }
                }
                if ui.button("\u{2b07} Import ZIP\u{2026}").clicked() {
                    self.import_zip_dialog();
                }
                if ui.button("\u{1f4c1} Import Folder\u{2026}").clicked() {
                    self.import_folder_dialog();
                }
            });

            if !self.status.is_empty() {
                ui.colored_label(egui::Color32::from_rgb(200, 60, 60), &self.status);
            }

            ui.add_space(8.0);
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("Activity Log");
                if ui.button("Clear log").clicked() {
                    self.log_lines.clear();
                }
            });
            // Fill whatever vertical space is left in the window, rather
            // than a fixed height — everything above this point has
            // already claimed its space for this frame, so
            // `available_height()` here is exactly the remainder.
            let remaining_height = ui.available_height().max(100.0);
            egui::ScrollArea::vertical()
                .max_height(remaining_height)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut self.log_lines.join("\n"))
                            .desired_width(f32::INFINITY)
                            .desired_rows(20)
                            .font(egui::TextStyle::Monospace)
                            .interactive(false),
                    );
                });
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop_watching();
    }
}
