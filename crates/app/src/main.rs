#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod icon;
mod icon_colors;
mod icon_render;
mod library_import_ui;
#[cfg(target_os = "linux")]
mod linux_desktop_integration;
mod part_lookup_ui;
mod single_instance;
mod theme;
mod tray;
mod ui;
mod window_chrome;

use ui::MainApp;

/// Internal build-tool escape hatches, not user-facing features: render
/// an image asset CI's release packaging needs (see `icon.rs`) and exit
/// before touching the GUI at all — there's no separate helper binary to
/// keep in sync with `icon.rs`/`icon_render.rs` for any of these.
///
/// - `--emit-iconset <dir>` — macOS `.iconset` PNGs, consumed by
///   `iconutil` to build the release `.app` bundle's `.icns`.
/// - `--emit-ico <path>` — a single `.ico`, used as the Windows NSIS
///   installer/uninstaller's icon (`packaging/windows/installer.nsi`).
/// - `--emit-dmg-background <path>` — the macOS installer DMG's Finder
///   window background image, consumed by `create-dmg --background`.
fn emit_build_asset_and_exit_if_requested() {
    let mut args = std::env::args_os().skip(1);
    let Some(flag) = args.next() else { return };
    match flag.to_str() {
        Some("--emit-iconset") => {
            let dir = args
                .next()
                .expect("--emit-iconset requires a directory argument");
            icon::write_iconset(std::path::Path::new(&dir)).expect("failed to write iconset");
        }
        Some("--emit-ico") => {
            let path = args
                .next()
                .expect("--emit-ico requires a file path argument");
            icon::write_ico(std::path::Path::new(&path)).expect("failed to write .ico");
        }
        Some("--emit-dmg-background") => {
            let path = args
                .next()
                .expect("--emit-dmg-background requires a file path argument");
            icon::write_dmg_background(std::path::Path::new(&path))
                .expect("failed to write the DMG background");
        }
        _ => return,
    }
    std::process::exit(0);
}

fn main() -> eframe::Result<()> {
    emit_build_asset_and_exit_if_requested();

    // Never returns if another instance is already running — it wakes
    // that instance's window instead and exits this process here.
    let wake_rx = single_instance::claim_or_exit();

    #[cfg(target_os = "linux")]
    linux_desktop_integration::spawn_registration();
    #[cfg(target_os = "linux")]
    tray::spawn();

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
            #[cfg(not(target_os = "linux"))]
            let (tray_icon, tray_toggle_item) = match tray::build() {
                Some((icon, item)) => (Some(icon), Some(item)),
                None => (None, None),
            };
            #[cfg(target_os = "linux")]
            let (tray_icon, tray_toggle_item) = (None, None);
            Ok(Box::new(MainApp::new(wake_rx, tray_icon, tray_toggle_item)))
        }),
    )
}
