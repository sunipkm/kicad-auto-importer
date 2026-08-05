//! Enumerates every symbol actually *placed* somewhere in a project's
//! schematic sheets — the root sheet plus every hierarchical sub-sheet
//! it transitively references — as opposed to
//! `library_import::load_project_symbols`, which only lists symbols
//! *defined* in the project's own registered libraries. A generic stock
//! symbol like `Device:R` never shows up there (it lives in KiCad's
//! global library, never a project-local one) even though a dozen of
//! them are placed on the schematic; this module is what makes
//! `part_lookup_ui`'s "Populate BOM" cover the whole design instead of
//! only project-local library parts.
//!
//! Looked-up vendor/manufacturer info is written back onto the specific
//! placed *instance* (via `SchematicFile::patch_symbol`, keyed by the
//! instance's own uuid), never into the shared library symbol it was
//! placed from: many differently-valued resistors all share the one
//! `Device:R` library entry, so writing MPN data there would clobber it
//! for every resistor in the design — and for a global/stock library
//! that isn't even project-local, it would corrupt a file KiCad shares
//! across every project on the machine. Real KiCad schematics already
//! carry exactly this shape of per-instance data (a manually-annotated
//! `Device:R` instance has its own `Mfr`/`Mouser`/... properties
//! sitting right next to its `Reference`/`Value`), so this mirrors an
//! established convention rather than inventing a new one.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::sexp::{Child, SexpNode};
use crate::symbol_importer;

/// One symbol placed on some sheet, deduplicated by reference — a
/// multi-unit symbol (e.g. a quad op-amp) places one `(symbol ...)`
/// block per unit, all sharing the same `Reference`; only the first is
/// kept.
#[derive(Debug, Clone, PartialEq)]
pub struct PlacedSymbol {
    pub reference: String,
    pub lib_id: String,
    pub description: String,
    pub datasheet: String,
    /// This instance's own `Value` property (e.g. `"10k"`, `"100nF"`) —
    /// used by `bom_pricing::group_placed_symbols` to tell apart
    /// differently-valued placements of one generic symbol (`Device:R`
    /// etc.) when no MPN has been set.
    pub value: String,
    /// This instance's own `Footprint` property, e.g.
    /// `"Resistor_SMD:R_0603_1608Metric"` — used both for the same
    /// grouping fallback as `value` and to detect passives (`R_`/`C_`/
    /// `L_` footprint name prefix) for `bom_pricing`'s extra-margin rule.
    pub footprint: String,
    /// `symbol_importer::resolve_mpn` applied to this instance right now —
    /// an explicit MPN-like property if one is already set (by hand, or
    /// by a prior "Populate BOM" run), otherwise `symbol_name()` itself.
    /// Computed once here since the instance's own sexp node is already
    /// in hand during the same schematic walk that reads `value`/
    /// `footprint`, rather than re-opening/re-parsing it later.
    pub resolved_mpn: String,
    /// The `.kicad_sch` file this instance actually lives in (root or a
    /// sub-sheet) — write-back reopens exactly this file.
    pub sch_path: PathBuf,
    /// Unique per placed instance; used instead of a name/index to find
    /// the exact byte span to patch, since schematic symbol instances
    /// (unlike `kicad_sym` symbols) have no unique leading name atom.
    pub uuid: String,
    /// Whether this symbol is marked as "Do Not Populate" (DNP).
    pub dnp: bool,
}

impl PlacedSymbol {
    /// The `lib_id`'s library nickname (before the `:`).
    pub fn library(&self) -> &str {
        self.lib_id
            .split_once(':')
            .map_or(self.lib_id.as_str(), |(lib, _)| lib)
    }

    /// The `lib_id`'s bare symbol name (after the `:`) — what
    /// `symbol_importer::resolve_mpn` falls back to searching for when the
    /// instance carries no MPN-like property of its own.
    pub fn symbol_name(&self) -> &str {
        self.lib_id
            .split_once(':')
            .map_or(self.lib_id.as_str(), |(_, name)| name)
    }
}

/// Find the project's root schematic: the `.kicad_sch` matching the
/// stem of whichever `.kicad_pro` lives directly in `project_dir`
/// (falls back to the sole `.kicad_sch` present if there's no matching
/// `.kicad_pro`, or none at all).
pub fn find_root_schematic(project_dir: &Path) -> Option<PathBuf> {
    let entries: Vec<PathBuf> = fs::read_dir(project_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();

    let pro_stem = entries
        .iter()
        .find(|p| p.extension().is_some_and(|ext| ext == "kicad_pro"))
        .and_then(|p| p.file_stem())
        .map(|s| s.to_string_lossy().to_string());

    if let Some(stem) = pro_stem {
        let candidate = project_dir.join(format!("{stem}.kicad_sch"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    let mut sch_files: Vec<&PathBuf> = entries
        .iter()
        .filter(|p| p.extension().is_some_and(|ext| ext == "kicad_sch"))
        .collect();
    if sch_files.len() == 1 {
        return sch_files.pop().cloned();
    }
    None
}

/// Walk `project_dir`'s root schematic and every hierarchical sub-sheet
/// it (transitively) references, collecting every placed symbol
/// instance: symbols with `(in_bom no)` and KiCad's own auto-generated
/// non-BOM references (`#PWR...`, `#FLG...`, ...) are skipped, but symbols
/// marked `(dnp yes)` ("Do Not Populate") are included with `dnp=true`.
pub fn load_schematic_symbols(project_dir: &Path, mut log: impl FnMut(&str)) -> Vec<PlacedSymbol> {
    let Some(root) = find_root_schematic(project_dir) else {
        log("  \u{26a0} No root schematic (.kicad_sch matching the project) found.");
        return Vec::new();
    };

    let mut visited_files = HashSet::new();
    let mut seen_refs = HashSet::new();
    let mut out = Vec::new();
    collect_from_file(
        &root,
        &mut visited_files,
        &mut seen_refs,
        &mut out,
        &mut log,
    );
    sort_by_reference(&mut out);
    out
}

fn collect_from_file(
    path: &Path,
    visited_files: &mut HashSet<PathBuf>,
    seen_refs: &mut HashSet<String>,
    out: &mut Vec<PlacedSymbol>,
    log: &mut impl FnMut(&str),
) {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited_files.insert(canon) {
        return; // already visited — cycle guard, and dedupes a sheet reused more than once
    }

    let Ok(text) = fs::read_to_string(path) else {
        log(&format!(
            "  \u{26a0} Could not read '{}' \u{2014} skipped.",
            path.display()
        ));
        return;
    };
    let Ok(root) = SexpNode::parse(&text) else {
        log(&format!(
            "  \u{26a0} Could not parse '{}' \u{2014} skipped.",
            path.display()
        ));
        return;
    };

    // `root.find_all("symbol")` only ever matches *direct* children of
    // `(kicad_sch ...)` — placed instances — never the cached library
    // definitions nested one level deeper inside `(lib_symbols ...)`.
    for sym in root.find_all("symbol") {
        let Some(lib_id) = sym
            .find(&["lib_id"])
            .and_then(|n| n.first_atom())
            .map(|a| a.text().to_string())
        else {
            continue;
        };
        let in_bom = sym
            .find(&["in_bom"])
            .and_then(|n| n.first_atom())
            .map(|a| a.text() != "no")
            .unwrap_or(true);
        if !in_bom {
            continue;
        }
        let dnp = sym
            .find(&["dnp"])
            .and_then(|n| n.first_atom())
            .map(|a| a.text() == "yes")
            .unwrap_or(false);
        let reference = property_value(sym, "Reference");
        if reference.is_empty() || reference.starts_with('#') {
            continue;
        }
        if !seen_refs.insert(reference.clone()) {
            continue; // another unit of the same multi-unit symbol
        }
        let Some(uuid) = sym
            .find(&["uuid"])
            .and_then(|n| n.first_atom())
            .map(|a| a.text().to_string())
        else {
            continue;
        };
        let symbol_name = lib_id
            .split_once(':')
            .map_or(lib_id.as_str(), |(_, name)| name);
        let resolved_mpn = symbol_importer::resolve_mpn(sym, symbol_name);
        out.push(PlacedSymbol {
            reference,
            lib_id,
            description: property_value(sym, "Description"),
            datasheet: property_value(sym, "Datasheet"),
            value: property_value(sym, "Value"),
            footprint: property_value(sym, "Footprint"),
            resolved_mpn,
            sch_path: path.to_path_buf(),
            uuid,
            dnp,
        });
    }

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    for sheet in root.find_all("sheet") {
        let sheetfile = property_value(sheet, "Sheetfile");
        if sheetfile.is_empty() {
            continue;
        }
        let sub_path = dir.join(&sheetfile);
        if sub_path.is_file() {
            collect_from_file(&sub_path, visited_files, seen_refs, out, log);
        } else {
            log(&format!(
                "  \u{26a0} Sub-sheet '{sheetfile}' referenced from '{}' not found \u{2014} skipped.",
                path.display()
            ));
        }
    }
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

/// Sorts references the way a human expects (R1, R2, ..., R10), not
/// plain lexicographic order (which would put R10 right after R1).
fn sort_by_reference(rows: &mut [PlacedSymbol]) {
    rows.sort_by_key(|r| natural_ref_key(&r.reference));
}

fn natural_ref_key(reference: &str) -> (String, u64, String) {
    let prefix_end = reference
        .find(|c: char| c.is_ascii_digit())
        .unwrap_or(reference.len());
    let (prefix, rest) = reference.split_at(prefix_end);
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let num: u64 = digits.parse().unwrap_or(0);
    (prefix.to_string(), num, reference.to_string())
}

// ── per-instance write-back ─────────────────────────────────────────────

struct SchSymbolSpan {
    uuid: String,
    start: usize,
    end: usize,
}

/// A single `.kicad_sch` file, opened for surgical per-instance
/// patching — mirrors `symbol_importer::SymbolLibrary`'s byte-preserving
/// approach (only spans actually patched are ever re-rendered; every
/// other line, including the file's own tab indentation and every other
/// placed symbol/wire/label, is copied out untouched) rather than a
/// full parse-and-rerender of the whole schematic, which risks a
/// misleading diff and a much larger blast radius from any sexp-grammar
/// gap.
pub struct SchematicFile {
    path: PathBuf,
    source: String,
    symbols: Vec<SchSymbolSpan>,
    replaced: std::collections::HashMap<usize, String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SchematicFileError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl SchematicFile {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SchematicFileError> {
        let path = path.into();
        let source = fs::read_to_string(&path)?;
        let symbols = SchematicFile::scan_spans(&source);
        Ok(SchematicFile {
            path,
            source,
            symbols,
            replaced: std::collections::HashMap::new(),
        })
    }

    /// Parses and returns just the named instance's subtree, for callers
    /// that need to inspect/patch it — mirrors
    /// `SymbolLibrary::get_symbol_node`.
    pub fn get_symbol_node(&self, uuid: &str) -> Option<SexpNode> {
        let span = self.symbols.iter().find(|s| s.uuid == uuid)?;
        SexpNode::parse(&self.source[span.start..span.end]).ok()
    }

    /// Queues `node` (already patched, e.g. via
    /// `crate::parts_lookup::apply_part_info`) to replace the instance with
    /// this uuid the next time `save` is called. Returns `false` if no
    /// instance with that uuid exists in this file.
    pub fn patch_symbol(&mut self, uuid: &str, node: &SexpNode) -> bool {
        let Some(idx) = self.symbols.iter().position(|s| s.uuid == uuid) else {
            return false;
        };
        self.replaced.insert(idx, node.render_at_indent(1));
        true
    }

    pub fn has_pending_changes(&self) -> bool {
        !self.replaced.is_empty()
    }

    /// Splices queued replacements into the original file text and
    /// writes the result — never re-renders any untouched span.
    pub fn save(&self) -> std::io::Result<()> {
        let mut out = String::with_capacity(self.source.len() + 4096);
        let mut cursor = 0usize;
        for (idx, span) in self.symbols.iter().enumerate() {
            out.push_str(&self.source[cursor..span.start]);
            match self.replaced.get(&idx) {
                Some(replacement) => out.push_str(replacement),
                None => out.push_str(&self.source[span.start..span.end]),
            }
            cursor = span.end;
        }
        out.push_str(&self.source[cursor..]);
        fs::write(&self.path, out)
    }
}

/// Shallow top-level scan for every direct `(symbol ...)` child of the
/// schematic root (i.e. every placed instance, at the same nesting depth
/// as `(sheet ...)`/`(wire ...)`/etc.) — deliberately does not descend
/// into `(lib_symbols ...)`, whose own nested `(symbol ...)` children are
/// cached library *definitions* one level deeper, not placed instances.
/// Mirrors `symbol_importer::scan_top_level`'s depth-tracking/
/// string-skipping approach, adapted to key each span by its `uuid`
/// instead of a leading name atom (schematic symbol instances have none
/// to dedupe on the way `kicad_sym` symbols do).
impl SchematicFile {
    fn scan_spans(source: &str) -> Vec<SchSymbolSpan> {
        let mut depth: i32 = 0;
        let mut in_string = false;
        let mut escape = false;
        let mut current_start: Option<usize> = None;
        let mut spans = Vec::new();

        for (idx, ch) in source.char_indices() {
            if in_string {
                if escape {
                    escape = false;
                } else if ch == '\\' {
                    escape = true;
                } else if ch == '"' {
                    in_string = false;
                }
                continue;
            }
            match ch {
                '"' => in_string = true,
                '(' => {
                    depth += 1;
                    if depth == 2 {
                        current_start = Some(idx);
                    }
                }
                ')' => {
                    if depth == 2 {
                        if let Some(start) = current_start.take() {
                            let end = idx + ch.len_utf8();
                            let span_text = &source[start..end];
                            if child_name(span_text) == "symbol" {
                                if let Ok(node) = SexpNode::parse(span_text) {
                                    if let Some(uuid) =
                                        node.find(&["uuid"]).and_then(|n| n.first_atom())
                                    {
                                        spans.push(SchSymbolSpan {
                                            uuid: uuid.text().to_string(),
                                            start,
                                            end,
                                        });
                                    }
                                }
                            }
                        }
                    }
                    depth -= 1;
                    if depth < 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
        spans
    }
}

/// Given `"(name ...)"` (including the outer parens), extract just the
/// node name.
fn child_name(text: &str) -> &str {
    let inner = text[1..].trim_start();
    let end = inner
        .find(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .unwrap_or(inner.len());
    &inner[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_flat_project(dir: &Path) {
        fs::write(dir.join("demo.kicad_pro"), "{}").unwrap();
        fs::write(
            dir.join("demo.kicad_sch"),
            r##"(kicad_sch (version 20231120)
	(lib_symbols
		(symbol "Device:R"
			(property "Reference" "R" (at 0 0 0))
		)
	)
	(symbol
		(lib_id "Device:R")
		(at 100 100 0)
		(unit 1)
		(in_bom yes)
		(on_board yes)
		(dnp no)
		(uuid "11111111-1111-1111-1111-111111111111")
		(property "Reference" "R1" (at 0 0 0))
		(property "Value" "10k" (at 0 0 0))
		(property "Footprint" "" (at 0 0 0))
		(property "Datasheet" "" (at 0 0 0))
		(property "Description" "Resistor" (at 0 0 0))
	)
	(symbol
		(lib_id "Device:R")
		(at 200 100 0)
		(unit 2)
		(in_bom yes)
		(on_board yes)
		(dnp no)
		(uuid "22222222-2222-2222-2222-222222222222")
		(property "Reference" "R1" (at 0 0 0))
		(property "Value" "10k" (at 0 0 0))
	)
	(symbol
		(lib_id "power:GND")
		(at 300 100 0)
		(unit 1)
		(in_bom yes)
		(on_board yes)
		(dnp no)
		(uuid "33333333-3333-3333-3333-333333333333")
		(property "Reference" "#PWR01" (at 0 0 0))
	)
	(symbol
		(lib_id "Jumper:SolderJumper_2_Open")
		(at 400 100 0)
		(unit 1)
		(in_bom no)
		(on_board yes)
		(dnp no)
		(uuid "44444444-4444-4444-4444-444444444444")
		(property "Reference" "JP1" (at 0 0 0))
	)
)"##,
        )
        .unwrap();
    }

    #[test]
    fn reads_value_and_footprint_and_falls_back_to_symbol_name_for_mpn() {
        let dir = tempdir().unwrap();
        write_flat_project(dir.path());
        let rows = load_schematic_symbols(dir.path(), |_| {});
        assert_eq!(rows[0].value, "10k");
        assert_eq!(rows[0].footprint, "");
        // No MPN-like property set on this instance, so it falls back
        // to the bare symbol name — same rule as `symbol_importer::resolve_mpn`.
        assert_eq!(rows[0].resolved_mpn, "R");
    }

    #[test]
    fn resolved_mpn_prefers_an_explicit_mpn_property() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("demo.kicad_pro"), "{}").unwrap();
        fs::write(
            dir.path().join("demo.kicad_sch"),
            r#"(kicad_sch (version 20231120)
	(symbol
		(lib_id "Resistor_Precision:PTS0603")
		(at 100 100 0)
		(unit 1)
		(in_bom yes)
		(uuid "66666666-6666-6666-6666-666666666666")
		(property "Reference" "R1" (at 0 0 0))
		(property "Value" "10k" (at 0 0 0))
		(property "Footprint" "Resistor_SMD:R_0603_1608Metric" (at 0 0 0))
		(property "MPN" "RC0603FR-0710KL" (at 0 0 0))
	)
)"#,
        )
        .unwrap();

        let rows = load_schematic_symbols(dir.path(), |_| {});
        assert_eq!(rows[0].resolved_mpn, "RC0603FR-0710KL");
        assert_eq!(rows[0].footprint, "Resistor_SMD:R_0603_1608Metric");
    }

    #[test]
    fn finds_root_schematic_matching_project_stem() {
        let dir = tempdir().unwrap();
        write_flat_project(dir.path());
        assert_eq!(
            find_root_schematic(dir.path()),
            Some(dir.path().join("demo.kicad_sch"))
        );
    }

    #[test]
    fn dedupes_multi_unit_and_skips_power_and_excluded_symbols() {
        let dir = tempdir().unwrap();
        write_flat_project(dir.path());
        let rows = load_schematic_symbols(dir.path(), |_| {});
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].reference, "R1");
        assert_eq!(rows[0].lib_id, "Device:R");
        assert_eq!(rows[0].symbol_name(), "R");
        assert_eq!(rows[0].library(), "Device");
    }

    #[test]
    fn includes_symbols_marked_do_not_populate_with_dnp_flag() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("demo.kicad_pro"), "{}").unwrap();
        fs::write(
            dir.path().join("demo.kicad_sch"),
            r#"(kicad_sch (version 20231120)
	(symbol
		(lib_id "Device:R")
		(at 100 100 0)
		(unit 1)
		(in_bom yes)
		(on_board yes)
		(dnp yes)
		(uuid "77777777-7777-7777-7777-777777777777")
		(property "Reference" "R1" (at 0 0 0))
	)
	(symbol
		(lib_id "Device:R")
		(at 200 100 0)
		(unit 1)
		(in_bom yes)
		(on_board yes)
		(dnp no)
		(uuid "88888888-8888-8888-8888-888888888888")
		(property "Reference" "R2" (at 0 0 0))
	)
)"#,
        )
        .unwrap();

        let rows = load_schematic_symbols(dir.path(), |_| {});
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].reference, "R1");
        assert!(rows[0].dnp);
        assert_eq!(rows[1].reference, "R2");
        assert!(!rows[1].dnp);
    }

    #[test]
    fn follows_hierarchical_sub_sheets() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("top.kicad_pro"), "{}").unwrap();
        fs::write(
            dir.path().join("top.kicad_sch"),
            r#"(kicad_sch (version 20231120)
	(sheet
		(at 50 50)
		(property "Sheetname" "Sub" (at 0 0 0))
		(property "Sheetfile" "sub.kicad_sch" (at 0 0 0))
	)
)"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("sub.kicad_sch"),
            r#"(kicad_sch (version 20231120)
	(symbol
		(lib_id "Device:C")
		(at 10 10 0)
		(unit 1)
		(in_bom yes)
		(uuid "55555555-5555-5555-5555-555555555555")
		(property "Reference" "C1" (at 0 0 0))
	)
)"#,
        )
        .unwrap();

        let rows = load_schematic_symbols(dir.path(), |_| {});
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].reference, "C1");
        assert_eq!(rows[0].sch_path, dir.path().join("sub.kicad_sch"));
    }

    #[test]
    fn follows_two_levels_of_nested_sub_sheets() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("top.kicad_pro"), "{}").unwrap();
        fs::write(
            dir.path().join("top.kicad_sch"),
            r#"(kicad_sch (version 20231120)
	(sheet
		(at 50 50)
		(property "Sheetname" "Mid" (at 0 0 0))
		(property "Sheetfile" "mid.kicad_sch" (at 0 0 0))
	)
)"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("mid.kicad_sch"),
            r#"(kicad_sch (version 20231120)
	(symbol
		(lib_id "Device:R")
		(at 10 10 0)
		(unit 1)
		(in_bom yes)
		(uuid "99999999-9999-9999-9999-999999999999")
		(property "Reference" "R1" (at 0 0 0))
	)
	(sheet
		(at 100 50)
		(property "Sheetname" "Leaf" (at 0 0 0))
		(property "Sheetfile" "leaf.kicad_sch" (at 0 0 0))
	)
)"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("leaf.kicad_sch"),
            r#"(kicad_sch (version 20231120)
	(symbol
		(lib_id "Device:C")
		(at 10 10 0)
		(unit 1)
		(in_bom yes)
		(uuid "55555555-5555-5555-5555-555555555555")
		(property "Reference" "C1" (at 0 0 0))
	)
)"#,
        )
        .unwrap();

        let rows = load_schematic_symbols(dir.path(), |_| {});
        let refs: Vec<&str> = rows.iter().map(|r| r.reference.as_str()).collect();
        assert_eq!(refs, vec!["C1", "R1"]);
        assert_eq!(rows[0].sch_path, dir.path().join("leaf.kicad_sch"));
        assert_eq!(rows[1].sch_path, dir.path().join("mid.kicad_sch"));
    }

    #[test]
    fn collects_symbols_from_multiple_sibling_sub_sheets() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("top.kicad_pro"), "{}").unwrap();
        fs::write(
            dir.path().join("top.kicad_sch"),
            r#"(kicad_sch (version 20231120)
	(sheet
		(at 50 50)
		(property "Sheetname" "A"
			(at 0 0 0))
		(property "Sheetfile" "a.kicad_sch" (at 0 0 0))
	)
	(sheet
		(at 150 50)
		(property "Sheetname" "B" (at 0 0 0))
		(property "Sheetfile" "b.kicad_sch" (at 0 0 0))
	)
)"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("a.kicad_sch"),
            r#"(kicad_sch (version 20231120)
	(symbol
		(lib_id "Device:R")
		(at 10 10 0)
		(unit 1)
		(in_bom yes)
		(uuid "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa")
		(property "Reference" "R1" (at 0 0 0))
	)
)"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("b.kicad_sch"),
            r#"(kicad_sch (version 20231120)
	(symbol
		(lib_id "Device:C")
		(at 10 10 0)
		(unit 1)
		(in_bom yes)
		(uuid "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb")
		(property "Reference" "C1" (at 0 0 0))
	)
)"#,
        )
        .unwrap();

        let rows = load_schematic_symbols(dir.path(), |_| {});
        let refs: Vec<&str> = rows.iter().map(|r| r.reference.as_str()).collect();
        assert_eq!(refs, vec!["C1", "R1"]);
        assert_eq!(rows[0].sch_path, dir.path().join("b.kicad_sch"));
        assert_eq!(rows[1].sch_path, dir.path().join("a.kicad_sch"));
    }

    #[test]
    fn references_sort_naturally() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("demo.kicad_pro"), "{}").unwrap();
        let mut body = String::from("(kicad_sch (version 20231120)\n");
        for (i, r) in ["R10", "R2", "R1"].iter().enumerate() {
            body.push_str(&format!(
                r#"	(symbol (lib_id "Device:R") (at 0 0 0) (unit 1) (in_bom yes)
		(uuid "0000000{i}-0000-0000-0000-000000000000")
		(property "Reference" "{r}" (at 0 0 0))
	)
"#
            ));
        }
        body.push(')');
        fs::write(dir.path().join("demo.kicad_sch"), body).unwrap();

        let rows = load_schematic_symbols(dir.path(), |_| {});
        let refs: Vec<&str> = rows.iter().map(|r| r.reference.as_str()).collect();
        assert_eq!(refs, vec!["R1", "R2", "R10"]);
    }

    #[test]
    fn patch_symbol_rewrites_only_the_targeted_instance() {
        let dir = tempdir().unwrap();
        write_flat_project(dir.path());
        let sch_path = dir.path().join("demo.kicad_sch");

        let mut sch = SchematicFile::open(&sch_path).unwrap();
        let mut node = sch
            .get_symbol_node("11111111-1111-1111-1111-111111111111")
            .unwrap();
        crate::symbol_importer::set_symbol_property(&mut node, "Mfr", "KOA Speer");
        assert!(sch.patch_symbol("11111111-1111-1111-1111-111111111111", &node));
        sch.save().unwrap();

        let after = fs::read_to_string(&sch_path).unwrap();
        assert!(after.contains("KOA Speer"));
        // The second unit of the same multi-unit symbol, and every
        // other symbol, must be untouched.
        assert!(after.contains("22222222-2222-2222-2222-222222222222"));
        let reopened = SchematicFile::open(&sch_path).unwrap();
        assert!(reopened
            .get_symbol_node("22222222-2222-2222-2222-222222222222")
            .unwrap()
            .find_all("property")
            .iter()
            .all(|p| !matches!(p.children.first(), Some(Child::Atom(a)) if a.text() == "Mfr")));
    }
}
