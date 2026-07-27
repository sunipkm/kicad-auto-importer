#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod theme;
mod ui;

use ui::MainApp;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([920.0, 780.0])
            .with_min_inner_size([640.0, 480.0])
            .with_decorations(false),
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
