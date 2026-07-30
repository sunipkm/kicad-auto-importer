//! Cherry-pick symbols (with their linked footprint + 3-D models) from
//! another KiCad project's own project-local libraries into the current
//! project. Ported from the sibling Python plugin's
//! `plugins/importer/library_import.py` + `plugins/ui/import_library_dialog.py`.
//!
//! The unit of selection is the **symbol** — footprints and 3-D models
//! are pulled in automatically for whichever symbols are selected, never
//! browsed on their own. Symbol discovery only ever looks at the other
//! project's *project-local* `sym-lib-table` (never the global one, to
//! keep the picker scoped to "this project's own parts"); footprint file
//! resolution is more permissive, preferring the source project's local
//! `fp-lib-table` but falling back to the user's global one and then to
//! a same-directory `.pretty` guess, since otherwise a symbol using a
//! stock KiCad footprint would never resolve.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::footprint_importer::{model_path_re, FootprintImporter};
use crate::model_importer::ModelImporter;
use crate::zip_importer::resolve_model_dir;
use kicad_auto_importer_core::kicad_paths::{
    self, expand_kicad_vars, load_project_local_table, register_project_library, LibEntry,
};
use kicad_auto_importer_core::sexp::{Child, SexpNode};
use kicad_auto_importer_core::symbol_importer::{
    extract_footprint_ref, is_top_level_symbol_name, patch_symbol_footprint, SymbolLibrary,
};

/// One row in the cherry-pick table: a symbol found in another project,
/// plus enough metadata to display it and (later) import it.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceSymbol {
    pub library: String,
    pub sym_lib_path: PathBuf,
    pub name: String,
    pub footprint_ref: Option<String>,
    pub type_label: String,
    pub units: usize,
    pub pins: usize,
    pub description: String,
    pub keywords: String,
    pub datasheet: String,
}

/// Destination settings for a cross-project import — same shape as
/// `zip_importer::ImportSettings`'s destination fields, minus the
/// watch-folder/ZIP-specific ones this flow has no use for.
///
/// `Clone` so the app can move an owned copy onto the background thread
/// `import_symbols` now runs on (see `library_import_ui::import_selected`)
/// without holding a borrow across the whole batch.
#[derive(Clone)]
pub struct CrossImportSettings {
    pub symbol_lib: PathBuf,
    pub footprint_lib: PathBuf,
    pub project_path: PathBuf,
    pub model_subdir: String,
    pub overwrite: bool,
}

/// Just the containing directory of a `.kicad_pro` file — its content is
/// never parsed; only its location matters, since `sym-lib-table` /
/// `fp-lib-table` always live directly beside it.
pub fn project_dir_from_pro_file(pro_file: &Path) -> PathBuf {
    pro_file
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| pro_file.to_path_buf())
}

/// Enumerate every selectable symbol reachable from `source_project_dir`'s
/// own project-local `sym-lib-table` — never the global table.
pub fn load_project_symbols(
    source_project_dir: &Path,
    mut log: impl FnMut(&str),
) -> Vec<SourceSymbol> {
    let mut rows = Vec::new();
    for entry in load_project_local_table(source_project_dir, "sym-lib-table") {
        let sym_lib_path = PathBuf::from(&entry.uri);
        if !sym_lib_path.is_file() {
            log(&format!(
                "  \u{26a0} Library '{}' not found at {} \u{2014} skipped.",
                entry.name, entry.uri
            ));
            continue;
        }
        rows.extend(load_symbols_from_file(&sym_lib_path, Some(&entry.name)));
    }
    sort_rows(&mut rows);
    rows
}

/// Describe every top-level symbol in a single `.kicad_sym` file directly
/// (no sym-lib-table/project context needed) — used both by
/// `load_project_symbols` per-library and by the dialog's "add symbols
/// from a specific library file" escape hatch for unregistered libraries.
pub fn load_symbols_from_file(
    sym_lib_path: &Path,
    library_label: Option<&str>,
) -> Vec<SourceSymbol> {
    let Ok(lib) = SymbolLibrary::open(sym_lib_path) else {
        return Vec::new();
    };
    let label = library_label.map(str::to_string).unwrap_or_else(|| {
        sym_lib_path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    });

    let mut rows: Vec<SourceSymbol> = lib
        .symbol_names()
        .filter(|name| is_top_level_symbol_name(name))
        .filter_map(|name| {
            lib.get_symbol_node(name)
                .map(|node| describe_symbol(name, &node, &label, sym_lib_path))
        })
        .collect();
    sort_rows(&mut rows);
    rows
}

fn sort_rows(rows: &mut [SourceSymbol]) {
    rows.sort_by(|a, b| {
        (a.library.to_lowercase(), a.name.to_lowercase())
            .cmp(&(b.library.to_lowercase(), b.name.to_lowercase()))
    });
}

fn describe_symbol(
    name: &str,
    node: &SexpNode,
    library: &str,
    sym_lib_path: &Path,
) -> SourceSymbol {
    SourceSymbol {
        library: library.to_string(),
        sym_lib_path: sym_lib_path.to_path_buf(),
        name: name.to_string(),
        footprint_ref: extract_footprint_ref(node),
        type_label: symbol_type_label(node),
        units: count_units(name, node),
        pins: count_pins(node),
        description: property_value(node, "Description"),
        keywords: property_value(node, "ki_keywords"),
        datasheet: property_value(node, "Datasheet"),
    }
}

fn symbol_type_label(node: &SexpNode) -> String {
    if let Some(extends) = node.find_all("extends").into_iter().next() {
        let target = extends
            .first_atom()
            .map(|a| a.text().to_string())
            .unwrap_or_default();
        return format!("Alias of {target}");
    }
    if !node.find_all("power").is_empty() {
        return "Power".to_string();
    }
    "Normal".to_string()
}

fn property_value(node: &SexpNode, key: &str) -> String {
    for prop in node.find_all("property") {
        if let (Some(Child::Atom(k)), Some(Child::Atom(v))) =
            (prop.children.first(), prop.children.get(1))
        {
            if k.text() == key {
                return v.text().to_string();
            }
        }
    }
    String::new()
}

fn count_pins(node: &SexpNode) -> usize {
    let mut count = usize::from(node.name == "pin");
    for child in &node.children {
        if let Child::Node(n) = child {
            count += count_pins(n);
        }
    }
    count
}

/// Distinct electrical units, inferred from the KiCad sub-symbol naming
/// convention `<symbol name>_<unit>_<body style>`; falls back to a plain
/// sub-symbol count if names don't follow that convention.
fn count_units(sym_name: &str, node: &SexpNode) -> usize {
    let sub_symbols = node.find_all("symbol");
    let mut units = std::collections::HashSet::new();
    let mut matched_any = false;
    for sub in &sub_symbols {
        let Some(sub_name) = sub.first_atom().map(|a| a.text()) else {
            continue;
        };
        if let Some(rest) = sub_name
            .strip_prefix(sym_name)
            .map(|r| r.trim_start_matches('_'))
        {
            if let Some(unit) = rest.split('_').next().filter(|u| !u.is_empty()) {
                units.insert(unit.to_string());
                matched_any = true;
            }
        }
    }
    if matched_any {
        units.len().max(1)
    } else {
        sub_symbols.len().max(1)
    }
}

/// Global `fp-lib-table` entries, overlaid by `source_project_dir`'s own
/// project-local ones (project-local wins on nickname collision) —
/// deliberately more permissive than symbol discovery, since a symbol
/// using a stock KiCad footprint has to resolve through the *global*
/// table.
pub fn load_combined_fp_table(source_project_dir: &Path) -> HashMap<String, LibEntry> {
    let mut table = HashMap::new();
    if let Some(global_path) = kicad_paths::find_global_lib_table("fp-lib-table") {
        for entry in kicad_paths::parse_lib_table(&global_path, None) {
            table.insert(entry.name.clone(), entry);
        }
    }
    for entry in load_project_local_table(source_project_dir, "fp-lib-table") {
        table.insert(entry.name.clone(), entry);
    }
    table
}

/// Look for `<lib_uri>/<fp_bare>.kicad_mod` directly, then fall back to a
/// recursive search in case the library stores footprints in
/// subdirectories.
pub fn find_source_footprint_file(fp_bare: &str, lib_uri: &str) -> Option<PathBuf> {
    let dir = PathBuf::from(lib_uri);
    let direct = dir.join(format!("{fp_bare}.kicad_mod"));
    if direct.is_file() {
        return Some(direct);
    }
    if !dir.is_dir() {
        return None;
    }
    WalkDir::new(&dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .find(|e| {
            e.file_type().is_file()
                && e.path().extension().is_some_and(|ext| ext == "kicad_mod")
                && e.path().file_stem().is_some_and(|stem| stem == fp_bare)
        })
        .map(|e| e.path().to_path_buf())
}

/// If a source symbol's footprint nickname isn't registered in any table
/// at all, look for exactly one sibling `*.pretty` directory next to the
/// `.kicad_sym` file it came from — and only use it if there is exactly
/// one candidate (ambiguous → give up rather than guess wrong).
pub fn guess_footprint_lib_dir(sym_lib_path: &Path) -> Option<PathBuf> {
    let dir = sym_lib_path.parent()?;
    let mut candidates = fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.extension().is_some_and(|ext| ext == "pretty"));
    let first = candidates.next()?;
    if candidates.next().is_some() {
        return None;
    }
    Some(first)
}

fn extract_model_paths(fp_path: &Path) -> Vec<String> {
    let Ok(content) = fs::read_to_string(fp_path) else {
        return Vec::new();
    };
    model_path_re()
        .captures_iter(&content)
        .map(|caps| caps[2].replace("\\\"", "\"").replace("\\\\", "\\"))
        .collect()
}

/// Resolve one `(model ...)` path found in a *source* footprint, against
/// the *source* project (never the destination — that distinction
/// matters, since `${KIPRJMOD}` means something different on each side).
pub fn resolve_source_model_path(
    raw: &str,
    footprint_dir: &Path,
    source_project_dir: &Path,
) -> Option<PathBuf> {
    let expanded = expand_kicad_vars(raw, Some(&source_project_dir.to_string_lossy()));
    let candidate = PathBuf::from(&expanded);
    if candidate.is_absolute() && candidate.is_file() {
        return Some(candidate);
    }
    let by_footprint_dir = footprint_dir.join(&expanded);
    if by_footprint_dir.is_file() {
        return Some(by_footprint_dir);
    }
    let by_project = source_project_dir.join(&expanded);
    if by_project.is_file() {
        return Some(by_project);
    }
    None
}

/// Import every symbol in `selected` (with its linked footprint and 3-D
/// models) into the destination libraries described by `settings`.
/// Per-symbol failures are logged and counted but never abort the batch.
/// Returns a `"{n} symbol(s), {m} footprint(s), {k} model(s) imported[,
/// {e} error(s)]"` summary.
///
/// `on_progress(index, total, name)` fires once at the *start* of each
/// selected symbol (before its footprint/model work), so a caller
/// driving a progress bar can show what's currently being imported
/// rather than only what's already finished.
pub fn import_symbols(
    selected: &[&SourceSymbol],
    source_project_dir: &Path,
    settings: &CrossImportSettings,
    mut log: impl FnMut(&str),
    mut on_progress: impl FnMut(usize, usize, &str),
) -> String {
    let fp_table = load_combined_fp_table(source_project_dir);

    let dest_fp_lib_name = register_project_library(
        Some(&settings.project_path),
        &settings.footprint_lib,
        "fp-lib-table",
        &mut log,
    );
    register_project_library(
        Some(&settings.project_path),
        &settings.symbol_lib,
        "sym-lib-table",
        &mut log,
    );

    let mut dest_lib = match SymbolLibrary::open_or_create(&settings.symbol_lib) {
        Ok(lib) => lib,
        Err(exc) => {
            log(&format!(
                "  \u{2718} Could not open destination symbol library: {exc}"
            ));
            return "0 symbol(s), 0 footprint(s), 0 model(s) imported, 1 error(s)".to_string();
        }
    };

    let mut source_cache: HashMap<PathBuf, SymbolLibrary> = HashMap::new();
    let (mut imported_syms, mut imported_fps, mut imported_models, mut errors) =
        (0usize, 0usize, 0usize, 0usize);

    let total = selected.len();
    for (index, row) in selected.iter().enumerate() {
        on_progress(index, total, &row.name);

        let source_lib = match source_cache.entry(row.sym_lib_path.clone()) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                match SymbolLibrary::open(&row.sym_lib_path) {
                    Ok(lib) => e.insert(lib),
                    Err(exc) => {
                        log(&format!(
                            "  \u{2718} '{}': could not open source library: {exc}",
                            row.name
                        ));
                        errors += 1;
                        continue;
                    }
                }
            }
        };

        let Some(mut node_copy) = source_lib.get_symbol_node(&row.name) else {
            log(&format!(
                "  \u{2718} '{}': not found in source library.",
                row.name
            ));
            errors += 1;
            continue;
        };

        let mut fp_name_map = HashMap::new();
        let mut footprint_resolved = false;
        if let Some(footprint_ref) = &row.footprint_ref {
            let (nickname, fp_bare) = match footprint_ref.split_once(':') {
                Some((lib, bare)) => (Some(lib.to_string()), bare.to_string()),
                None => (None, footprint_ref.clone()),
            };

            let src_fp_path = nickname
                .as_deref()
                .and_then(|nick| fp_table.get(nick))
                .and_then(|entry| find_source_footprint_file(&fp_bare, &entry.uri))
                .or_else(|| {
                    guess_footprint_lib_dir(&row.sym_lib_path).and_then(|dir| {
                        find_source_footprint_file(&fp_bare, &dir.to_string_lossy())
                    })
                });

            match src_fp_path {
                Some(src_fp_path) => {
                    let footprint_dir = src_fp_path.parent().unwrap_or(source_project_dir);
                    let raw_models = extract_model_paths(&src_fp_path);
                    let resolved_models: Vec<PathBuf> = raw_models
                        .iter()
                        .filter_map(|raw| {
                            resolve_source_model_path(raw, footprint_dir, source_project_dir)
                        })
                        .collect();

                    let mut model_name_map = HashMap::new();
                    let mut model_dir_for_patch: Option<PathBuf> = None;
                    // Deferred-message pattern: a constructor's `Result`
                    // keeps borrowing `log` mutably (it boxes `&mut log`
                    // internally) for as long as that `Result` temporary is
                    // considered live, which spans the *whole* enclosing
                    // `match` — so the `Err` arm can't call `log` again
                    // itself. Stash the message and log it only once we're
                    // past the match entirely.
                    let mut deferred_warning: Option<String> = None;
                    if !resolved_models.is_empty() {
                        let dir_result = resolve_model_dir(
                            &settings.project_path,
                            &settings.model_subdir,
                            &mut log,
                        );
                        match dir_result {
                            Ok(dest_model_dir) => {
                                let mi_result = ModelImporter::new(
                                    &dest_model_dir,
                                    settings.overwrite,
                                    &mut log,
                                );
                                match mi_result {
                                    Ok(mut mi) => {
                                        model_name_map = mi.import_all(&resolved_models);
                                        imported_models += resolved_models.len();
                                        model_dir_for_patch = Some(dest_model_dir);
                                    }
                                    Err(exc) => {
                                        deferred_warning = Some(format!(
                                            "Could not prepare model directory: {exc}"
                                        ));
                                    }
                                }
                            }
                            Err(exc) => {
                                deferred_warning =
                                    Some(format!("Could not prepare model directory: {exc}"));
                            }
                        }
                    }
                    if let Some(msg) = deferred_warning {
                        log(&format!("  \u{26a0} {msg}"));
                    }

                    // Scoped in its own block so `fi_result` (which embeds
                    // `&mut log`) is fully dropped before the deferred log
                    // call below reborrows `log` — see the comment above.
                    let mut deferred_warning: Option<String> = None;
                    {
                        let fi_result = FootprintImporter::new(
                            &settings.footprint_lib,
                            settings.overwrite,
                            Some(settings.project_path.clone()),
                            &mut log,
                        );
                        match fi_result {
                            Ok(mut fi) => {
                                fp_name_map = fi.import_all(
                                    std::slice::from_ref(&src_fp_path),
                                    &model_name_map,
                                    model_dir_for_patch.as_deref(),
                                );
                                imported_fps += 1;
                                footprint_resolved = true;
                            }
                            Err(exc) => {
                                deferred_warning =
                                    Some(format!("Could not prepare footprint library: {exc}"));
                            }
                        }
                    }
                    if let Some(msg) = deferred_warning {
                        log(&format!("  \u{26a0} {msg}"));
                    }
                }
                None => {
                    log(&format!(
                        "  \u{26a0} '{}': footprint '{}' could not be resolved \u{2014} left unchanged.",
                        row.name, footprint_ref
                    ));
                }
            }

            // Only rewrite the Footprint property when a source footprint
            // file was actually found and copied — otherwise the dangling
            // reference stays exactly as the source symbol had it, per the
            // reference plugin's behavior (a warning was already logged
            // above), rather than being rewritten to point at a
            // destination library nickname with nothing behind it.
            if footprint_resolved {
                patch_symbol_footprint(&mut node_copy, &dest_fp_lib_name, &fp_name_map);
            }
        }

        if dest_lib.add_symbol(&row.name, &node_copy, settings.overwrite) {
            log(&format!("  \u{2714} Symbol '{}' imported.", row.name));
            imported_syms += 1;
        } else {
            log(&format!(
                "  \u{2013} Symbol '{}' skipped (already exists in destination library).",
                row.name
            ));
        }
    }

    if let Err(exc) = dest_lib.save() {
        log(&format!(
            "  \u{2718} Could not save destination symbol library: {exc}"
        ));
        errors += 1;
    }

    let mut summary =
        format!("{imported_syms} symbol(s), {imported_fps} footprint(s), {imported_models} model(s) imported");
    if errors > 0 {
        summary.push_str(&format!(", {errors} error(s)"));
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_source_project(dir: &Path) {
        fs::write(
            dir.join("sym-lib-table"),
            r#"(sym_lib_table
  (version 7)
  (lib (name "Src")(type "KiCad")(uri "${KIPRJMOD}/Src.kicad_sym")(options "")(descr ""))
)"#,
        )
        .unwrap();
        fs::write(
            dir.join("fp-lib-table"),
            r#"(fp_lib_table
  (version 7)
  (lib (name "SrcFP")(type "KiCad")(uri "${KIPRJMOD}/SrcFP.pretty")(options "")(descr ""))
)"#,
        )
        .unwrap();
        fs::write(
            dir.join("Src.kicad_sym"),
            r#"(kicad_symbol_lib (version 20231120) (generator test)
  (symbol "Widget"
    (property "Reference" "R" (at 0 0 0))
    (property "Footprint" "SrcFP:Widget" (at 0 0 0))
    (property "Description" "A test widget" (at 0 0 0))
  )
)"#,
        )
        .unwrap();
        fs::create_dir_all(dir.join("SrcFP.pretty")).unwrap();
        fs::write(
            dir.join("SrcFP.pretty").join("Widget.kicad_mod"),
            r#"(footprint "Widget" (model "3D/Widget.step"))"#,
        )
        .unwrap();
        fs::create_dir_all(dir.join("3D")).unwrap();
        fs::write(dir.join("3D").join("Widget.step"), b"fake step data").unwrap();
    }

    #[test]
    fn load_project_symbols_skips_sub_symbol_names() {
        let dir = tempdir().unwrap();
        write_source_project(dir.path());
        fs::write(
            dir.path().join("Src.kicad_sym"),
            r#"(kicad_symbol_lib (version 20231120) (generator test)
  (symbol "Widget"
    (property "Footprint" "SrcFP:Widget" (at 0 0 0))
    (symbol "Widget_0_1" (pin input line (at 0 0 0) (length 1) (name "A" (effects (font (size 1 1)))) (number "1" (effects (font (size 1 1))))))
  )
  (symbol "Widget_1_1:sub" (property "Reference" "X" (at 0 0 0)))
)"#,
        )
        .unwrap();

        let rows = load_project_symbols(dir.path(), |_| {});
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "Widget");
        assert_eq!(rows[0].library, "Src");
        assert_eq!(rows[0].pins, 1);
    }

    #[test]
    fn end_to_end_import_registers_libraries_and_patches_paths() {
        let source_dir = tempdir().unwrap();
        write_source_project(source_dir.path());
        let dest_dir = tempdir().unwrap();

        let rows = load_project_symbols(source_dir.path(), |_| {});
        assert_eq!(rows.len(), 1);
        let selected: Vec<&SourceSymbol> = rows.iter().collect();

        let settings = CrossImportSettings {
            symbol_lib: dest_dir.path().join("Combined.kicad_sym"),
            footprint_lib: dest_dir.path().join("Combined.pretty"),
            project_path: dest_dir.path().to_path_buf(),
            model_subdir: "3dmodels".to_string(),
            overwrite: false,
        };

        let mut logs = Vec::new();
        let summary = import_symbols(
            &selected,
            source_dir.path(),
            &settings,
            |m| logs.push(m.to_string()),
            |_, _, _| {},
        );
        assert!(summary.contains("1 symbol(s)"));
        assert!(summary.contains("1 footprint(s)"));
        assert!(summary.contains("1 model(s)"));
        assert!(!summary.contains("error"));

        let dest_lib = SymbolLibrary::open(&settings.symbol_lib).unwrap();
        assert!(dest_lib.contains("Widget"));

        let footprint_text =
            fs::read_to_string(settings.footprint_lib.join("Widget.kicad_mod")).unwrap();
        assert!(footprint_text.contains("${KIPRJMOD}/3dmodels/Widget.step"));

        assert!(fs::metadata(dest_dir.path().join("3dmodels").join("Widget.step")).is_ok());

        let sym_lib_table = fs::read_to_string(dest_dir.path().join("sym-lib-table")).unwrap();
        assert!(sym_lib_table.contains("Combined"));
        let fp_lib_table = fs::read_to_string(dest_dir.path().join("fp-lib-table")).unwrap();
        assert!(fp_lib_table.contains("Combined"));

        // The symbol's Footprint property must now point at the
        // *destination* footprint library nickname, not the source one.
        let sym_source = fs::read_to_string(&settings.symbol_lib).unwrap();
        assert!(sym_source.contains("Combined:Widget"));
    }

    #[test]
    fn duplicate_symbol_without_overwrite_is_skipped_not_an_error() {
        let source_dir = tempdir().unwrap();
        write_source_project(source_dir.path());
        let dest_dir = tempdir().unwrap();

        let rows = load_project_symbols(source_dir.path(), |_| {});
        let selected: Vec<&SourceSymbol> = rows.iter().collect();
        let settings = CrossImportSettings {
            symbol_lib: dest_dir.path().join("Combined.kicad_sym"),
            footprint_lib: dest_dir.path().join("Combined.pretty"),
            project_path: dest_dir.path().to_path_buf(),
            model_subdir: "3dmodels".to_string(),
            overwrite: false,
        };

        import_symbols(
            &selected,
            source_dir.path(),
            &settings,
            |_| {},
            |_, _, _| {},
        );
        let summary = import_symbols(
            &selected,
            source_dir.path(),
            &settings,
            |_| {},
            |_, _, _| {},
        );

        assert!(summary.contains("0 symbol(s)"));
        assert!(!summary.contains("error"));
    }

    #[test]
    fn missing_footprint_file_leaves_symbol_footprint_ref_untouched_and_is_not_an_error() {
        let source_dir = tempdir().unwrap();
        fs::write(
            source_dir.path().join("sym-lib-table"),
            r#"(sym_lib_table (version 7)
  (lib (name "Src")(type "KiCad")(uri "${KIPRJMOD}/Src.kicad_sym")(options "")(descr ""))
)"#,
        )
        .unwrap();
        fs::write(
            source_dir.path().join("Src.kicad_sym"),
            r#"(kicad_symbol_lib (version 20231120) (generator test)
  (symbol "Orphan" (property "Footprint" "NoSuchLib:NoSuchFP" (at 0 0 0)))
)"#,
        )
        .unwrap();
        let dest_dir = tempdir().unwrap();

        let rows = load_project_symbols(source_dir.path(), |_| {});
        let selected: Vec<&SourceSymbol> = rows.iter().collect();
        let settings = CrossImportSettings {
            symbol_lib: dest_dir.path().join("Combined.kicad_sym"),
            footprint_lib: dest_dir.path().join("Combined.pretty"),
            project_path: dest_dir.path().to_path_buf(),
            model_subdir: "3dmodels".to_string(),
            overwrite: false,
        };

        let mut logs = Vec::new();
        let summary = import_symbols(
            &selected,
            source_dir.path(),
            &settings,
            |m| logs.push(m.to_string()),
            |_, _, _| {},
        );
        assert!(summary.contains("1 symbol(s)"));
        assert!(!summary.contains("error"));

        let sym_source = fs::read_to_string(&settings.symbol_lib).unwrap();
        // Dangling reference preserved verbatim, not rewritten to garbage.
        assert!(sym_source.contains("NoSuchLib:NoSuchFP"));
    }
}
