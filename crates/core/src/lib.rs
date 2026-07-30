//! Core KiCad source-file primitives shared by (or reusable across)
//! both `kicad-auto-importer` and `bom-app`: S-expression parsing,
//! symbol-library (`.kicad_sym`) and schematic (`.kicad_sch`) parsing/
//! patching, and project/library-table (`fp-lib-table`/`sym-lib-table`)
//! resolution — the parts of the original Python plugin's
//! `plugins/importer/*` genuinely about the *shape* of KiCad's own file
//! formats, as opposed to either app's own import/pricing/watch
//! features built on top of them.

pub mod kicad_paths;
#[cfg(feature = "kicad-process")]
pub mod kicad_process;
pub mod schematic;
pub mod sexp;
pub mod symbol_importer;
