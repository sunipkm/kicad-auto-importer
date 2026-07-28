//! Core, GUI-free import pipeline for kicad-auto-importer — ported
//! from the sibling Python KiCad plugin's `plugins/importer/*`,
//! `plugins/watcher.py`, and `plugins/config.py`. No dependency on any
//! GUI toolkit or on KiCad itself; the `app` crate is the only thing
//! that knows about windows.

pub mod bom_pricing;
pub mod bom_report;
pub mod config;
pub mod digikey;
pub mod footprint_importer;
pub mod generate_bom;
pub mod global_settings;
pub mod http_agent;
pub mod kicad_paths;
pub mod kicad_process;
pub mod library_import;
pub mod model_importer;
pub mod mouser;
pub mod parts_cache;
pub mod parts_lookup;
pub mod populate_bom;
pub mod schematic;
pub mod sexp;
pub mod symbol_importer;
pub mod watcher;
pub mod zip_importer;
