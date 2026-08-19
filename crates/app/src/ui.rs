//! The main application window — mirrors the layout of the sibling
//! Python plugin's `plugins/ui/main_dialog.py`: path fields with
//! Browse… buttons, an options group, a start/stop watch toggle, manual
//! one-shot import buttons, and a log pane.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use egui::{Color32, RichText, Stroke};
use egui_phosphor::regular as icon;
use kicad_parse::kicad_paths::{expand_kicad_vars, kiprjmod_relative_uri};

use crate::config::ImporterConfig;
use crate::global_settings::GlobalSettings;
use crate::library_import::CrossImportSettings;
use crate::library_import_ui::{self, LibraryImportState};
use crate::theme::{self, ACCENT, DANGER, TITLE_BAR_BG};
use crate::tray;
use crate::watcher::{FolderWatcher, WatchEvent};
use crate::window_chrome;
use crate::zip_importer::{import_folder, import_zip, validate_model_subdir, ImportSettings};

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

    /// Fires whenever a second copy of the app is launched (see
    /// `single_instance`) — the window should show/focus itself.
    wake_rx: mpsc::Receiver<()>,
    /// Set only by the tray menu's Quit item; lets the close-request
    /// interception in `update()` tell "really quit" apart from the
    /// window's own X button / Alt+F4 / etc., which should hide to tray
    /// instead of exiting.
    force_quit: bool,
    /// Kept alive for as long as the tray icon should stay visible.
    /// Always `None` on Linux, where the tray icon instead lives for the
    /// process's lifetime on its own dedicated GTK thread (see `tray`).
    _tray_icon: Option<tray_icon::TrayIcon>,
    /// The tray menu's single start/stop toggle item, held onto so its
    /// label/enabled state can be updated directly as watching state
    /// changes (see `sync_tray_toggle`). Same Linux/non-Linux split as
    /// `_tray_icon`: always `None` on Linux, where the real item lives
    /// on the GTK thread instead and is synced via `tray::set_state`.
    tray_toggle_item: Option<tray_icon::menu::MenuItem>,
}

impl MainApp {
    pub fn new(
        wake_rx: mpsc::Receiver<()>,
        tray_icon: Option<tray_icon::TrayIcon>,
        tray_toggle_item: Option<tray_icon::menu::MenuItem>,
    ) -> Self {
        let global_settings = GlobalSettings::load();
        // Restore the last-opened project, if its directory still
        // exists — a since-deleted/moved project would otherwise show a
        // path with nothing behind it. Every other per-project setting
        // (watch folder, libraries, options) then comes back for free
        // via `load_config_for_current_project`, since it already lives
        // in that project's own `ImporterConfig`.
        let project_path = global_settings.last_project_path.clone();
        let restore_project = !project_path.is_empty() && Path::new(&project_path).is_dir();
        let mut app = MainApp {
            project_path: if restore_project {
                project_path
            } else {
                String::new()
            },
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
            wake_rx,
            force_quit: false,
            _tray_icon: tray_icon,
            tray_toggle_item,
        };
        if restore_project {
            app.load_config_for_current_project();
        }
        app
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

    /// If `self.project_path` currently points at a `.kicad_pro` *file*
    /// — typed or pasted directly, matching what the "KiCad Project
    /// (.kicad_pro):" field's own label invites, rather than picked via
    /// `browse_project`'s file dialog — collapses it down to that file's
    /// parent directory in place, the same normalization `browse_project`
    /// already applies to whatever it picks. `project_path` is documented
    /// as always holding the project *directory* (every other read of it,
    /// e.g. `to_absolute`'s `${KIPRJMOD}` expansion, `save_global_settings`'s
    /// `last_project_path`, assumes exactly that) — without this, typing
    /// the `.kicad_pro` path the label suggests silently breaks every
    /// relative symbol/footprint library path and the "restore last
    /// project on launch" check (`Path::is_dir()` on a file is `false`).
    /// Called every frame right after the field's own widget, so it's a
    /// no-op once already normalized (a directory has no `.kicad_pro`
    /// extension to strip).
    fn normalize_project_path(&mut self) {
        if let Some(dir) = kicad_pro_parent_dir(&self.project_path) {
            self.project_path = dir;
            self.load_config_for_current_project();
            self.save_global_settings();
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

    /// Always assigns every field from whatever `ImporterConfig::load`
    /// returns for `project_dir` — including the watch folder/library
    /// fields, which used to be skipped when the loaded value was empty.
    /// That was meant to avoid clobbering something the user had just
    /// typed, but this function is only ever called when *switching to*
    /// a project (app startup restore, or picking a new `.kicad_pro` in
    /// `browse_project`), never while editing — so a project with no
    /// settings file yet must land on a blank slate, not silently
    /// inherit the *previous* project's symbol/footprint library paths.
    /// Leaving those stale was actively dangerous: `${KIPRJMOD}`-relative
    /// paths re-resolve against the new project directory, so clicking
    /// "Start Watching" without noticing would create a library file
    /// named after the old project, inside the new one.
    fn load_config_for_current_project(&mut self) {
        let Some(project_dir) = self.project_dir() else {
            return;
        };
        let has_settings_file = ImporterConfig::config_path(&project_dir).is_file();
        let cfg = ImporterConfig::load(&project_dir);
        self.watch_folder = cfg.watch_folder;
        self.symbol_lib = cfg.symbol_lib;
        self.footprint_lib = cfg.footprint_lib;
        self.model_subdir = cfg.model_subdir;
        self.move_zip = cfg.move_zip;
        self.backup_zip = cfg.backup_zip;
        self.overwrite = cfg.overwrite;
        if has_settings_file {
            self.log(format!(
                "Loaded settings for project '{}'.",
                project_dir.display()
            ));
        } else {
            // No settings saved yet — default the symbol/footprint
            // library names to the project's own name (matching what
            // KiCad itself does for the .kicad_pro/.kicad_sch/.kicad_pcb
            // trio) instead of leaving them blank, so a first-time user
            // isn't required to type a name before anything works.
            let name = project_name(&project_dir);
            self.symbol_lib = format!("${{KIPRJMOD}}/{name}.kicad_sym");
            self.footprint_lib = format!("${{KIPRJMOD}}/{name}.pretty");
            self.log(format!(
                "No settings file yet for project '{}' \u{2014} defaulting libraries to '{name}'.",
                project_dir.display()
            ));
        }
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

    /// Unlike `save_config`, not tied to `project_dir()` — the
    /// last-opened project path is account-/app-level, not per-project
    /// (see `GlobalSettings`'s module docs).
    fn save_global_settings(&mut self) {
        let settings = GlobalSettings {
            last_project_path: self.project_path.clone(),
        };
        if let Err(exc) = settings.save() {
            self.log(format!("\u{2718} Could not save settings: {exc}"));
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

    /// Same validation `build_settings(true)` runs before actually
    /// starting the watcher, minus the `self.status` side effect —
    /// used every frame (see `sync_tray_toggle`) to decide whether the
    /// "Start Watching" controls (both the main window's button and the
    /// tray menu's toggle item) should be enabled at all, without
    /// clobbering a status message with each redundant check.
    fn can_start_watching(&self) -> bool {
        if self.project_dir().is_none() {
            return false;
        }
        if self.watch_folder.trim().is_empty() {
            return false;
        }
        if self.symbol_lib.trim().is_empty() || self.footprint_lib.trim().is_empty() {
            return false;
        }
        validate_model_subdir(&self.model_subdir).is_ok()
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

    /// Keeps the tray menu's single start/stop item in sync with
    /// current state — called once per frame from `update`. See
    /// `tray::set_state`'s docs for why non-Linux applies the change
    /// directly to `tray_toggle_item` here while Linux only updates a
    /// couple of atomics for its GTK-thread poll to pick up.
    fn sync_tray_toggle(&self, watching: bool, can_start: bool) {
        tray::set_state(watching, can_start);
        if let Some(item) = &self.tray_toggle_item {
            tray::apply_toggle_state(item, watching, can_start);
        }
    }

    /// Whether hiding the window to the tray on close is actually safe,
    /// i.e. whether a tray icon exists for the user to bring it back
    /// from. On Windows/macOS the tray is built inline in `main` and
    /// stored directly (`_tray_icon`); on Linux it's built asynchronously
    /// on a dedicated GTK thread and can fail for reasons outside this
    /// app's control (no `StatusNotifierWatcher`, no D-Bus session), so
    /// `tray::is_available` is checked instead (see its docs).
    fn tray_available(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            tray::is_available()
        }
        #[cfg(not(target_os = "linux"))]
        {
            self._tray_icon.is_some()
        }
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

    fn show_and_focus(ctx: &egui::Context) {
        // `Visible(true)` undoes the X11 hide path below; `Minimized(false)`
        // undoes the minimize fallback the same path uses on Wayland,
        // where `Visible` is a documented no-op either direction
        // (winit's `platform_impl::linux::wayland::window::set_visible`:
        // "Not possible on Wayland"). Note winit *also* can't
        // programmatically un-minimize on Wayland ("You can't unminimize
        // the window on Wayland" — the call is accepted but ignored), so
        // this is best-effort there: the compositor's own affordance
        // (e.g. clicking the app in the dock) is what actually restores
        // it, same as it would for any other minimized window.
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    /// Polls everything that can ask the window to reappear or the
    /// watcher to toggle from outside the UI itself: a second instance
    /// being launched (`wake_rx`, see `single_instance`), and tray icon
    /// / tray menu clicks (`tray_icon`'s own global event receivers —
    /// see `tray.rs` for why those aren't funneled through a channel we
    /// own). Called once per frame from `update`, the same polling
    /// pattern `drain_log_channel` already uses for the watcher's log
    /// channel.
    fn drain_tray_events(&mut self, ctx: &egui::Context) {
        if self.wake_rx.try_recv().is_ok() {
            Self::show_and_focus(ctx);
        }

        while let Ok(event) = tray_icon::TrayIconEvent::receiver().try_recv() {
            if let tray_icon::TrayIconEvent::Click {
                button: tray_icon::MouseButton::Left,
                button_state: tray_icon::MouseButtonState::Up,
                ..
            } = event
            {
                Self::show_and_focus(ctx);
            }
        }

        while let Ok(event) = tray_icon::menu::MenuEvent::receiver().try_recv() {
            match event.id.as_ref() {
                tray::MENU_SHOW => Self::show_and_focus(ctx),
                tray::MENU_TOGGLE => {
                    if self.watcher.is_some() {
                        self.stop_watching();
                    } else if self.can_start_watching() {
                        self.start_watching();
                    }
                }
                tray::MENU_QUIT => {
                    self.force_quit = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                _ => {}
            }
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
            self.save_global_settings();
        }
    }

    fn browse_watch_folder(&mut self) {
        if let Some(path) = rfd::FileDialog::new().pick_folder() {
            self.watch_folder = self.to_display(&path);
        }
    }

    fn browse_symbol_lib(&mut self) {
        // `save_file`, not `pick_file`: this dialog is for *choosing* the
        // destination library, which may not exist yet — the import
        // pipeline opens whatever path comes back with
        // `SymbolLibrary::open_or_create` and only ever appends/patches
        // into it, never replaces wholesale. `pick_file` (an Open-style
        // dialog) looks like the more honest choice, but native Open
        // dialogs on every platform either refuse to return a path that
        // doesn't exist yet or grey out the confirm button entirely —
        // there'd be no way to browse to a brand-new library at all,
        // only ever an existing one (a not-yet-created name could still
        // be typed directly into the editable field next to this
        // button, but not picked via Browse). `save_file` does show the
        // OS's native "this file already exists — replace?" prompt when
        // browsing to a library you already have, which is misleading
        // (nothing gets overwritten), but that's the lesser of the two
        // problems — the user just confirms it and nothing is lost.
        let mut dialog = rfd::FileDialog::new().add_filter("KiCad symbol library", &["kicad_sym"]);
        if let Some(project_dir) = self.project_dir() {
            dialog = dialog
                .set_directory(&project_dir)
                .set_file_name(project_name(&project_dir));
        }
        if let Some(mut path) = dialog.save_file() {
            // Not every platform's save dialog appends the filter's
            // extension on its own for a freshly-typed name — same
            // belt-and-suspenders `set_extension` `browse_footprint_lib`
            // does for `.pretty` below.
            if path.extension().is_none_or(|e| e != "kicad_sym") {
                path.set_extension("kicad_sym");
            }
            self.symbol_lib = self.to_display(&path);
        }
    }

    fn browse_footprint_lib(&mut self) {
        // `save_file`, not `pick_folder` — same reasoning as
        // `browse_symbol_lib` above, just for a directory instead of a
        // single file: `rfd` has no "pick or create a folder" dialog
        // mode, and `pick_folder`'s "Open"-style semantics can leave a
        // not-yet-created `.pretty` library unreachable via Browse
        // (most visibly on Linux, where the native folder chooser many
        // desktop environments route this through — the
        // `xdg-desktop-portal` file chooser — often has no "New Folder"
        // affordance at all). The footprint import pipeline already
        // creates the destination directory itself if it doesn't exist
        // (`footprint_importer`'s `fs::create_dir_all`), so a `save_file`
        // dialog's typed-but-nonexistent path is exactly as usable here
        // as a real file path is for `browse_symbol_lib`.
        let mut dialog = rfd::FileDialog::new();
        if let Some(project_dir) = self.project_dir() {
            dialog = dialog
                .set_directory(&project_dir)
                .set_file_name(project_name(&project_dir));
        }
        if let Some(mut path) = dialog.save_file() {
            if path.extension().is_none_or(|e| e != "pretty") {
                path.set_extension("pretty");
            }
            self.footprint_lib = self.to_display(&path);
        }
    }
}

/// If `path` names a `.kicad_pro` file (case-insensitive extension),
/// returns its parent directory as a string — `None` if `path` doesn't
/// end in `.kicad_pro` (already a directory, or empty) or has no parent
/// component. See `MainApp::normalize_project_path`'s own docs for why
/// this matters.
fn kicad_pro_parent_dir(path: &str) -> Option<String> {
    let p = Path::new(path);
    if !p
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("kicad_pro"))
    {
        return None;
    }
    p.parent()
        .map(|parent| parent.to_string_lossy().to_string())
}

/// The project's own name, for defaulting the symbol/footprint library
/// names — the `.kicad_pro` file's stem if `project_dir` contains
/// exactly one (KiCad always names a project directory after its own
/// `.kicad_pro`, but this tolerates a directory with none or several by
/// falling back to the directory's own name instead of guessing wrong).
fn project_name(project_dir: &Path) -> String {
    let mut kicad_pro_stems = fs::read_dir(project_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("kicad_pro"))
        })
        .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()));
    let first = kicad_pro_stems.next();
    if let Some(name) = first {
        if kicad_pro_stems.next().is_none() {
            return name;
        }
    }
    project_dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Project".to_string())
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
        self.drain_tray_events(ctx);
        // Always scheduled, not just while watching: wake_rx/tray events
        // (a second instance launching, a tray click) need to be polled
        // even while idle or hidden to the tray, since nothing else would
        // otherwise nudge the event loop into calling `update` again.
        ctx.request_repaint_after(std::time::Duration::from_millis(250));

        let watching = self.watcher.is_some();
        let can_start = self.can_start_watching();
        self.sync_tray_toggle(watching, can_start);

        if ctx.input(|i| i.viewport().close_requested())
            && !self.force_quit
            && self.tray_available()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            // `Visible(false)` is all this needs on X11, but it's a
            // silent no-op on Wayland (see `show_and_focus`'s docs) —
            // without also minimizing, the window would stay fully
            // visible and clicking the title bar's Close button would
            // look like it did nothing. `Minimized(true)` *is* honored
            // on Wayland, so send both: each platform picks up whichever
            // one it actually supports.
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
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
                        self.normalize_project_path();
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
                        let toggle_size = egui::vec2(175.0, 44.0);
                        let action_size = egui::vec2(155.0, 44.0);

                        ui.horizontal(|ui| {
                            let toggle_resp = if watching {
                                ui.add_sized(
                                    toggle_size,
                                    egui::Button::new(format!("{}  Stop Watching", icon::STOP))
                                        .fill(Color32::from_rgb(0x5a, 0x24, 0x24))
                                        .stroke(Stroke::new(1.0_f32, DANGER)),
                                )
                            } else {
                                // Grayed out (via `add_enabled_ui`, since
                                // `add_sized` has no enabled-aware
                                // counterpart) whenever `start_watching`
                                // would just immediately fail anyway —
                                // no project chosen, a library path
                                // missing, etc. (see `can_start_watching`).
                                ui.add_enabled_ui(can_start, |ui| {
                                    ui.add_sized(
                                        toggle_size,
                                        theme::accent_button(format!(
                                            "{}  Start Watching",
                                            icon::PLAY
                                        )),
                                    )
                                })
                                .inner
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
                                    theme::accent_button(format!(
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
                                        // Deliberately interactive (not
                                        // `.interactive(false)`): the
                                        // content is rebuilt from
                                        // `log_lines` every frame
                                        // regardless of what's typed into
                                        // it, so it can't actually be
                                        // edited, but leaving it
                                        // interactive is what lets
                                        // drag-select and Ctrl+C copy the
                                        // log text out.
                                        ui.add(
                                            egui::TextEdit::multiline(
                                                &mut self.log_lines.join("\n"),
                                            )
                                            .desired_width(f32::INFINITY)
                                            .desired_rows(20)
                                            .frame(false)
                                            .font(egui::TextStyle::Monospace),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kicad_pro_parent_dir_strips_a_typed_project_file_path() {
        assert_eq!(
            kicad_pro_parent_dir("/home/user/MyProject/MyProject.kicad_pro"),
            Some("/home/user/MyProject".to_string())
        );
    }

    #[test]
    fn kicad_pro_parent_dir_matches_the_extension_case_insensitively() {
        assert_eq!(
            kicad_pro_parent_dir("/home/user/MyProject/MyProject.KICAD_PRO"),
            Some("/home/user/MyProject".to_string())
        );
    }

    #[test]
    fn kicad_pro_parent_dir_is_none_for_a_directory() {
        assert_eq!(kicad_pro_parent_dir("/home/user/MyProject"), None);
    }

    #[test]
    fn kicad_pro_parent_dir_is_none_for_empty_input() {
        assert_eq!(kicad_pro_parent_dir(""), None);
    }

    #[test]
    fn kicad_pro_parent_dir_is_none_for_an_unrelated_extension() {
        assert_eq!(
            kicad_pro_parent_dir("/home/user/MyProject/MyProject.kicad_sch"),
            None
        );
    }

    #[test]
    fn project_name_uses_the_kicad_pro_stem() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Widget.kicad_pro"), "{}").unwrap();
        assert_eq!(project_name(dir.path()), "Widget");
    }

    #[test]
    fn project_name_falls_back_to_the_directory_name_without_a_kicad_pro() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("MyProjectDir");
        fs::create_dir(&sub).unwrap();
        assert_eq!(project_name(&sub), "MyProjectDir");
    }

    #[test]
    fn project_name_falls_back_to_the_directory_name_with_ambiguous_kicad_pro_files() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("MyProjectDir");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("A.kicad_pro"), "{}").unwrap();
        fs::write(sub.join("B.kicad_pro"), "{}").unwrap();
        assert_eq!(project_name(&sub), "MyProjectDir");
    }
}
