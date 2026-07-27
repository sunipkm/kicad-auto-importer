//! Orchestrates import of a part-provider download — either a ZIP
//! archive or an already-extracted folder (some OSes, e.g. macOS
//! Safari / Archive Utility, auto-extract downloaded ZIPs into a
//! sibling folder). Ported from `plugins/importer/zip_importer.py`.
//!
//! Expected contents (UltraLibrarian / Mouser / DigiKey style), at any
//! depth:
//!   `*.kicad_sym`       – KiCad symbol
//!   `*.kicad_mod`       – KiCad footprint(s)
//!   `*.stp` / `*.step`  – STEP 3-D model(s)   (optional)
//!   `*.wrl`             – VRML 3-D model(s)   (optional)
//!
//! Deliberate deviation from the Python version, following from this
//! tool's "always single-project-scoped" design: `project_path` is a
//! required field here, not optional — every path in `ImportSettings`
//! already presupposes a known project directory.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::footprint_importer::FootprintImporter;
use crate::kicad_paths::register_project_library;
use crate::model_importer::ModelImporter;
use crate::sexp::{self, Child};
use crate::symbol_importer::{is_top_level_symbol_name, patch_symbol_footprint, SymbolLibrary};

/// Directories to ignore when scanning (macOS zip-extraction artefacts,
/// our own backup folder, hidden/system folders).
pub const IGNORE_DIR_NAMES: &[&str] = &["_imported", "__MACOSX"];

pub struct ImportSettings {
    pub symbol_lib: PathBuf,
    pub footprint_lib: PathBuf,
    pub project_path: PathBuf,
    pub model_subdir: String,
    pub overwrite: bool,
    /// Only used by `import_zip`/`import_folder`'s post-processing step.
    pub watch_folder: Option<PathBuf>,
    pub move_zip: bool,
    pub backup_zip: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("no .kicad_sym or .kicad_mod files found")]
    NothingToImport,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("symbol library error: {0}")]
    SymbolLibrary(#[from] crate::symbol_importer::SymbolLibraryError),
    #[error("could not parse source symbol file '{path}': {source}")]
    SourceSexp {
        path: PathBuf,
        source: sexp::SexpError,
    },
    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
}

fn should_descend(entry: &walkdir::DirEntry) -> bool {
    if !entry.file_type().is_dir() || entry.depth() == 0 {
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    !IGNORE_DIR_NAMES.contains(&name.as_ref()) && !name.starts_with('.')
}

fn matches_ext(path: &Path, exts: &[&str]) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    let ext_lower = ext.to_lowercase();
    exts.iter().any(|e| *e == ext_lower)
}

/// Recursively walk `root` (diving into every subfolder), bucketing
/// every file by type. Ignores known noise directories.
pub fn scan(root: &Path) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>) {
    let mut sym_files = Vec::new();
    let mut fp_files = Vec::new();
    let mut model_files = Vec::new();

    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(should_descend)
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path().to_path_buf();
        if matches_ext(&path, &["kicad_sym"]) {
            sym_files.push(path);
        } else if matches_ext(&path, &["kicad_mod"]) {
            fp_files.push(path);
        } else if matches_ext(&path, &["stp", "step", "wrl"]) {
            model_files.push(path);
        }
    }
    (sym_files, fp_files, model_files)
}

/// Quick recursive check used by the watcher to decide whether a newly
/// created folder is a part download worth importing.
pub fn has_importable_files(root: &Path) -> bool {
    WalkDir::new(root)
        .into_iter()
        .filter_entry(should_descend)
        .filter_map(|e| e.ok())
        .any(|e| {
            e.file_type().is_file()
                && matches_ext(e.path(), &["kicad_sym", "kicad_mod", "stp", "step", "wrl"])
        })
}

/// Returns the absolute path to the project's 3-D model directory,
/// creating it if it doesn't exist yet.
pub fn resolve_model_dir(
    project_path: &Path,
    model_subdir: &str,
    mut log: impl FnMut(&str),
) -> std::io::Result<PathBuf> {
    let full = project_path.join(model_subdir);
    if full.is_dir() {
        log(&format!("  Using 3-D model directory: {}", full.display()));
    } else {
        fs::create_dir_all(&full)?;
        log(&format!(
            "  Created 3-D model directory: {}",
            full.display()
        ));
    }
    Ok(full)
}

/// Rejects an absolute path or any `..` path segment — a model subdir
/// must stay inside the project directory. Mirrors
/// `main_dialog.py::_validate_model_subdir`, ported into `core` so both
/// the GUI and any future entry point enforce it consistently.
pub fn validate_model_subdir(raw: &str) -> Result<String, String> {
    let normalized = raw.trim().replace('\\', "/");
    if normalized.is_empty() {
        return Ok("3dmodels".to_string());
    }
    if Path::new(&normalized).is_absolute() || normalized.starts_with('/') {
        return Err(
            "Model subfolder must be relative to the project, not an absolute path.".into(),
        );
    }
    if normalized.split('/').any(|seg| seg == "..") {
        return Err("Model subfolder must not contain '..' segments.".into());
    }
    Ok(normalized)
}

/// Scan `root` (recursively) for KiCad files and import whatever is
/// found. Shared by both `import_zip` and `import_folder`.
pub fn import_from_directory(
    root: &Path,
    settings: &ImportSettings,
    mut log: impl FnMut(&str),
) -> Result<String, ImportError> {
    let (sym_files, fp_files, model_files) = scan(root);
    log(&format!(
        "  Found: {} symbol(s), {} footprint(s), {} 3-D model(s)",
        sym_files.len(),
        fp_files.len(),
        model_files.len()
    ));

    if sym_files.is_empty() && fp_files.is_empty() {
        return Err(ImportError::NothingToImport);
    }

    // Ensure the destination libraries are registered in the project's
    // sym-lib-table / fp-lib-table, so KiCad actually sees them without
    // any manual "Add Library" step. Only attempted for a table when
    // this batch actually contains that kind of file.
    let mut fp_lib_name = settings
        .footprint_lib
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if !fp_files.is_empty() {
        fp_lib_name = register_project_library(
            Some(&settings.project_path),
            &settings.footprint_lib,
            "fp-lib-table",
            &mut log,
        );
    }
    if !sym_files.is_empty() {
        register_project_library(
            Some(&settings.project_path),
            &settings.symbol_lib,
            "sym-lib-table",
            &mut log,
        );
    }

    // Resolve model directory & import models.
    let mut model_name_map = HashMap::new();
    let mut model_dir: Option<PathBuf> = None;
    if !model_files.is_empty() {
        let dir = resolve_model_dir(&settings.project_path, &settings.model_subdir, &mut log)?;
        let mut mi = ModelImporter::new(&dir, settings.overwrite, &mut log)?;
        model_name_map = mi.import_all(&model_files);
        model_dir = Some(dir);
    }

    // Import footprints.
    let mut fp_name_map = HashMap::new();
    if !fp_files.is_empty() {
        let mut fi = FootprintImporter::new(
            &settings.footprint_lib,
            settings.overwrite,
            Some(settings.project_path.clone()),
            &mut log,
        )?;
        fp_name_map = fi.import_all(&fp_files, &model_name_map, model_dir.as_deref());
    }

    // Import symbols (link footprint + models).
    if !sym_files.is_empty() {
        let mut lib = SymbolLibrary::open_or_create(&settings.symbol_lib)?;
        for sym_path in &sym_files {
            let text = fs::read_to_string(sym_path)?;
            let root_node = sexp::parse(&text).map_err(|source| ImportError::SourceSexp {
                path: sym_path.clone(),
                source,
            })?;

            for child in &root_node.children {
                let Child::Node(sym) = child else { continue };
                if sym.name != "symbol" {
                    continue;
                }
                let Some(Child::Atom(name_atom)) = sym.children.first() else {
                    continue;
                };
                let name = name_atom.text().to_string();
                if !is_top_level_symbol_name(&name) {
                    continue;
                }
                if lib.contains(&name) && !settings.overwrite {
                    log(&format!(
                        "  \u{2013} Symbol '{name}' skipped (already exists in destination library)."
                    ));
                    continue;
                }

                let mut node_copy = sym.clone();
                patch_symbol_footprint(&mut node_copy, &fp_lib_name, &fp_name_map);
                if lib.add_symbol(&name, &node_copy, settings.overwrite) {
                    log(&format!("  \u{2714} Symbol '{name}' imported."));
                }
            }
        }
        lib.save()?;
    }

    Ok(format!(
        "{} sym, {} fp, {} model(s) imported",
        sym_files.len(),
        fp_files.len(),
        model_files.len()
    ))
}

/// Extract `zip_path` to a temp directory, import its contents, then
/// optionally move or back up the original ZIP.
pub fn import_zip(
    zip_path: &Path,
    settings: &ImportSettings,
    mut log: impl FnMut(&str),
) -> Result<String, ImportError> {
    let tmp = tempfile::Builder::new().prefix("ultralib_").tempdir()?;
    log(&format!(
        "  Extracting {}\u{2026}",
        zip_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
    ));
    {
        let file = fs::File::open(zip_path)?;
        let mut archive = zip::ZipArchive::new(file)?;
        archive.extract(tmp.path())?;
    }

    let result = import_from_directory(tmp.path(), settings, &mut log)?;
    post_process_zip(zip_path, settings, &mut log);
    Ok(result)
}

/// Import directly from an already-extracted folder (no ZIP present),
/// recursing into any subfolders it contains. Used both for manual
/// "Import Folder…" and for auto-extracted downloads picked up by the
/// watcher.
pub fn import_folder(
    folder_path: &Path,
    settings: &ImportSettings,
    mut log: impl FnMut(&str),
) -> Result<String, ImportError> {
    log(&format!(
        "  Scanning folder {}\u{2026}",
        folder_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
    ));
    let result = import_from_directory(folder_path, settings, &mut log)?;
    post_process_folder(folder_path, settings, &mut log);
    Ok(result)
}

fn timestamp() -> String {
    // No wall-clock access inside pure `core` tests/build tooling is
    // required here; callers needing a real timestamp use
    // `std::time::SystemTime` at the call site in the `app` crate if a
    // deterministic value matters for tests. For the backup-folder
    // naming convention we only need *a* monotonically-distinct string
    // per run, so this uses SystemTime directly (fine for production
    // use, just not for hermetic core unit tests around exact names).
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

fn post_process_zip(zip_path: &Path, settings: &ImportSettings, log: &mut impl FnMut(&str)) {
    let Some(watch_folder) = &settings.watch_folder else {
        return;
    };

    if settings.backup_zip && watch_folder.is_dir() {
        let bak_dir = watch_folder.join("_imported");
        if fs::create_dir_all(&bak_dir).is_ok() {
            let stem = zip_path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let ext = zip_path
                .extension()
                .map(|e| e.to_string_lossy().to_string())
                .unwrap_or_default();
            let dest = bak_dir.join(format!("{stem}_{}.{ext}", timestamp()));
            match fs::copy(zip_path, &dest) {
                Ok(_) => log(&format!("  Backup saved \u{2192} {}", dest.display())),
                Err(exc) => log(&format!("  Warning: could not back up ZIP: {exc}")),
            }
        }
    }

    if settings.move_zip {
        match fs::remove_file(zip_path) {
            Ok(_) => log("  Original ZIP removed."),
            Err(exc) => log(&format!("  Warning: could not remove original ZIP: {exc}")),
        }
    }
}

fn post_process_folder(folder_path: &Path, settings: &ImportSettings, log: &mut impl FnMut(&str)) {
    let Some(watch_folder) = &settings.watch_folder else {
        return;
    };
    if !folder_path.is_dir() {
        return;
    }

    if settings.move_zip {
        let bak_dir = watch_folder.join("_imported");
        if let Err(exc) = fs::create_dir_all(&bak_dir) {
            log(&format!("  Warning: could not move folder: {exc}"));
            return;
        }
        let name = folder_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let dest = bak_dir.join(format!("{name}_{}", timestamp()));
        match fs::rename(folder_path, &dest) {
            Ok(_) => log(&format!("  Folder moved \u{2192} {}", dest.display())),
            Err(exc) => log(&format!("  Warning: could not move folder: {exc}")),
        }
    } else if settings.backup_zip {
        // "Backup" for a folder just means leaving it where it is —
        // there's no separate original file to duplicate.
        log(&format!(
            "  Original folder left in place: {}",
            folder_path.display()
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn scan_finds_files_at_any_depth_and_ignores_noise_dirs() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("KiCad").join("Symbol");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            nested.join("Part.kicad_sym"),
            "(kicad_symbol_lib (version 20231120))",
        )
        .unwrap();
        fs::write(dir.path().join("Part.kicad_mod"), "(footprint \"x\")").unwrap();
        fs::write(dir.path().join("Part.step"), b"fake").unwrap();

        let ignored = dir.path().join("__MACOSX");
        fs::create_dir_all(&ignored).unwrap();
        fs::write(ignored.join("junk.kicad_sym"), "junk").unwrap();

        let (sym, fp, models) = scan(dir.path());
        assert_eq!(sym.len(), 1);
        assert_eq!(fp.len(), 1);
        assert_eq!(models.len(), 1);
    }

    #[test]
    fn has_importable_files_false_for_unrelated_folder() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("readme.txt"), b"hello").unwrap();
        assert!(!has_importable_files(dir.path()));
    }

    #[test]
    fn validate_model_subdir_rejects_absolute_and_dotdot() {
        assert!(validate_model_subdir("3dmodels").is_ok());
        assert_eq!(validate_model_subdir("").unwrap(), "3dmodels");
        assert!(validate_model_subdir("/etc/passwd").is_err());
        assert!(validate_model_subdir("../outside").is_err());
        assert!(validate_model_subdir("nested/../../escape").is_err());
    }

    #[test]
    fn end_to_end_import_from_directory_registers_libraries() {
        let project_dir = tempdir().unwrap();
        let download_dir = tempdir().unwrap();

        fs::write(
            download_dir.path().join("Widget.kicad_sym"),
            r#"(kicad_symbol_lib (version 20231120) (generator test)
  (symbol "Widget"
    (property "Reference" "U")
    (property "Footprint" "OrigLib:OrigFP" (at 0 0 0))))"#,
        )
        .unwrap();
        fs::write(
            download_dir.path().join("OrigFP.kicad_mod"),
            r#"(footprint "OrigFP" (model "3D/OrigFP.step"))"#,
        )
        .unwrap();
        fs::write(download_dir.path().join("OrigFP.step"), b"fake step data").unwrap();

        let settings = ImportSettings {
            symbol_lib: project_dir.path().join("Combined.kicad_sym"),
            footprint_lib: project_dir.path().join("Combined.pretty"),
            project_path: project_dir.path().to_path_buf(),
            model_subdir: "3dmodels".to_string(),
            overwrite: false,
            watch_folder: None,
            move_zip: false,
            backup_zip: false,
        };

        let mut logs = Vec::new();
        let summary =
            import_from_directory(download_dir.path(), &settings, |m| logs.push(m.to_string()))
                .unwrap();
        assert!(summary.contains("1 sym"));
        assert!(summary.contains("1 fp"));

        let sym_lib = SymbolLibrary::open(&settings.symbol_lib).unwrap();
        assert!(sym_lib.contains("Widget"));

        let sym_lib_table = fs::read_to_string(project_dir.path().join("sym-lib-table")).unwrap();
        assert!(sym_lib_table.contains("Combined"));
        let fp_lib_table = fs::read_to_string(project_dir.path().join("fp-lib-table")).unwrap();
        assert!(fp_lib_table.contains("Combined"));

        let footprint_text =
            fs::read_to_string(settings.footprint_lib.join("OrigFP.kicad_mod")).unwrap();
        assert!(footprint_text.contains("${KIPRJMOD}/3dmodels/OrigFP.step"));

        assert!(fs::metadata(project_dir.path().join("3dmodels").join("OrigFP.step")).is_ok());
    }
}
