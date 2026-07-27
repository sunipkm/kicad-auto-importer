#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ui;

use ui::MainApp;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([900.0, 760.0])
            .with_min_inner_size([640.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "kicad-auto-importer",
        options,
        Box::new(|_cc| Ok(Box::new(MainApp::default()))),
    )
}
