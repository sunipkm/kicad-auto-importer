//! Placeholder binary — proves `app` links against `core` correctly.
//!
//! The real egui/eframe front-end (Phase 5 of the project plan) is not
//! yet implemented; this crate currently exists only so
//! `cargo build --workspace` / `cargo test --workspace` exercise both
//! crates while `core` (the sexp/import-pipeline/watcher logic) is
//! being built out and verified first.

fn main() {
    println!("kicad-auto-importer core is wired up; GUI front-end not implemented yet.");
}
