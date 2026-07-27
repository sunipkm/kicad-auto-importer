#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ui;

use ui::MainApp;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([720.0, 640.0]),
        ..Default::default()
    };
    eframe::run_native(
        "kicad-auto-importer",
        options,
        Box::new(|_cc| Ok(Box::new(MainApp::default()))),
    )
}
