#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod icon;
mod icon_colors;
mod icon_render;
mod library_import_ui;
#[cfg(target_os = "linux")]
mod linux_desktop_integration;
mod theme;
mod ui;
mod window_chrome;

use ui::MainApp;

/// Internal build-tool escape hatch, not a user-facing feature: writes
/// the macOS `.iconset` PNGs (see `icon::write_iconset`) and exits
/// before touching the GUI at all. CI's release packaging runs the
/// built binary with this flag to produce the `.app` bundle's icon —
/// there's no separate helper binary to keep in sync with `icon.rs`.
fn emit_iconset_and_exit_if_requested() {
    let mut args = std::env::args_os().skip(1);
    let Some(flag) = args.next() else { return };
    if flag != "--emit-iconset" {
        return;
    }
    let dir = args
        .next()
        .expect("--emit-iconset requires a directory argument");
    icon::write_iconset(std::path::Path::new(&dir)).expect("failed to write iconset");
    std::process::exit(0);
}

fn main() -> eframe::Result<()> {
    emit_iconset_and_exit_if_requested();

    #[cfg(target_os = "linux")]
    linux_desktop_integration::spawn_registration();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([920.0, 780.0])
            .with_min_inner_size([640.0, 480.0])
            .with_decorations(false)
            .with_icon(icon::app_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "kicad-auto-importer",
        options,
        Box::new(|cc| {
            theme::install(&cc.egui_ctx);
            Ok(Box::new(MainApp::default()))
        }),
    )
}
