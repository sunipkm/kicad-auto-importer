//! Copies `.kicad_mod` files into the project `.pretty` library
//! directory and patches the `(model ...)` path inside each footprint so
//! it points at the newly-imported 3-D model files.
//!
//! Ported from `plugins/importer/footprint_importer.py`, including the
//! design lesson learned there: footprints are copied byte-for-byte
//! otherwise, deliberately NOT run through the generic sexp
//! parser/renderer. A footprint's `(property ...)` nodes mean different
//! things depending on context (a named value pair, a bareword
//! pad-property flag like `pad_prop_castellated`, a bareword-keyed
//! filter like `ki_fp_filters`), and re-serializing the whole tree
//! risks getting one of those forms wrong. Since the only thing ever
//! changed is the 3-D model path, that's done with a targeted text
//! substitution instead, leaving everything else untouched.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;

use kicad_parse::kicad_paths::kiprjmod_relative_uri;

pub(crate) fn model_path_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(\(model\s+)"((?:[^"\\]|\\.)*)""#).unwrap())
}

pub struct FootprintImporter<'a> {
    lib_path: PathBuf,
    overwrite: bool,
    project_path: Option<PathBuf>,
    log: Box<dyn FnMut(&str) + 'a>,
}

impl<'a> FootprintImporter<'a> {
    pub fn new(
        lib_path: impl Into<PathBuf>,
        overwrite: bool,
        project_path: Option<PathBuf>,
        log: impl FnMut(&str) + 'a,
    ) -> std::io::Result<Self> {
        let lib_path = lib_path.into();
        fs::create_dir_all(&lib_path)?;
        Ok(FootprintImporter {
            lib_path,
            overwrite,
            project_path,
            log: Box::new(log),
        })
    }

    /// Returns a mapping of original footprint name -> imported footprint name.
    pub fn import_all(
        &mut self,
        fp_files: &[PathBuf],
        model_name_map: &HashMap<String, PathBuf>,
        model_dir: Option<&Path>,
    ) -> HashMap<String, String> {
        // Some distributor exports (observed from Mouser's KiCad
        // downloads) ship a footprint and its STEP file as unrelated
        // sibling files with no `(model ...)` reference inside the
        // footprint at all — there's nothing for `patch_model_paths` to
        // rewrite, so the model would otherwise get copied to the
        // project's model directory and never actually get attached to
        // anything. When this batch is unambiguous (exactly one
        // footprint, exactly one distinct model copied alongside it),
        // that pairing is safe to assume; anything less certain is left
        // alone rather than guessing wrong.
        let sole_model_fallback = (fp_files.len() == 1)
            .then(|| {
                let mut distinct = model_name_map.values().collect::<std::collections::HashSet<_>>();
                (distinct.len() == 1).then(|| distinct.drain().next().cloned())
            })
            .flatten()
            .flatten();

        let mut name_map = HashMap::new();
        for fp_path in fp_files {
            let orig_name = fp_path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if let Some(imported) =
                self.import_one(fp_path, model_name_map, model_dir, sole_model_fallback.as_deref())
            {
                name_map.insert(orig_name, imported);
            }
        }
        name_map
    }

    fn import_one(
        &mut self,
        src_path: &Path,
        model_name_map: &HashMap<String, PathBuf>,
        model_dir: Option<&Path>,
        fallback_model: Option<&Path>,
    ) -> Option<String> {
        let name = src_path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let dest_path = self.lib_path.join(src_path.file_name()?);

        if dest_path.exists() && !self.overwrite {
            (self.log)(&format!("  Footprint '{name}' skipped (exists)."));
            return Some(name);
        }

        let content = fs::read_to_string(src_path).ok()?;
        let had_model_ref = model_path_re().is_match(&content);
        let patched = patch_model_paths(
            &content,
            model_name_map,
            model_dir,
            self.project_path.as_deref(),
        );
        let patched = if !had_model_ref {
            if let Some(fallback) = fallback_model {
                (self.log)(&format!(
                    "  \u{2139} Footprint '{name}' had no 3-D model reference \u{2014} attaching the sole model imported alongside it."
                ));
                append_model_reference(&patched, fallback, self.project_path.as_deref())
            } else {
                patched
            }
        } else {
            patched
        };
        fs::write(&dest_path, patched).ok()?;

        (self.log)(&format!(
            "  Footprint '{name}' imported → {}",
            self.lib_path.display()
        ));
        Some(name)
    }
}

/// Rewrites every `(model "...")` path to a `${KIPRJMOD}`-relative URI
/// pointing at the newly-imported model file, via a plain text
/// substitution — everything else in `content` is passed through
/// unchanged.
pub fn patch_model_paths(
    content: &str,
    model_name_map: &HashMap<String, PathBuf>,
    model_dir: Option<&Path>,
    project_path: Option<&Path>,
) -> String {
    model_path_re()
        .replace_all(content, |caps: &regex::Captures| -> String {
            let prefix = &caps[1];
            let orig_path = caps[2].replace("\\\"", "\"").replace("\\\\", "\\");

            let orig_basename = orig_path
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(&orig_path)
                .to_string();
            let orig_stem = orig_basename
                .rsplit_once('.')
                .map(|(stem, _)| stem.to_string())
                .unwrap_or_else(|| orig_basename.clone());

            let dest_abs = model_name_map
                .get(&orig_basename)
                .or_else(|| model_name_map.get(&orig_stem))
                .cloned()
                .or_else(|| {
                    model_dir.and_then(|dir| {
                        ["stp", "step", "wrl", "STP", "STEP"]
                            .iter()
                            .map(|ext| dir.join(format!("{orig_stem}.{ext}")))
                            .find(|candidate| candidate.exists())
                    })
                });

            let Some(dest_abs) = dest_abs else {
                return caps[0].to_string(); // leave original path untouched if unresolved
            };

            let new_path = match project_path {
                Some(project_path) => kiprjmod_relative_uri(&dest_abs, project_path),
                None => dest_abs.to_string_lossy().replace('\\', "/"),
            };

            let escaped = new_path.replace('\\', "\\\\").replace('"', "\\\"");
            format!("{prefix}\"{escaped}\"")
        })
        .to_string()
}

/// Inserts a brand-new `(model "...")` block into a footprint that has
/// none at all, right before the footprint's closing paren, with a
/// neutral zero-offset/unit-scale/zero-rotation transform. Used when a
/// source footprint never referenced a 3-D model in the first place
/// (see `FootprintImporter::import_all`'s `sole_model_fallback`), so
/// there is no existing `(model ...)` text for `patch_model_paths` to
/// rewrite.
fn append_model_reference(content: &str, model_path: &Path, project_path: Option<&Path>) -> String {
    let uri = match project_path {
        Some(project_path) => kiprjmod_relative_uri(model_path, project_path),
        None => model_path.to_string_lossy().replace('\\', "/"),
    };
    let escaped = uri.replace('\\', "\\\\").replace('"', "\\\"");

    let trimmed = content.trim_end();
    let Some(close_idx) = trimmed.rfind(')') else {
        return content.to_string();
    };
    format!(
        "{}\t(model \"{escaped}\"\n\t\t(offset (xyz 0 0 0))\n\t\t(scale (xyz 1 1 1))\n\t\t(rotate (xyz 0 0 0))\n\t)\n{}",
        &trimmed[..close_idx],
        &trimmed[close_idx..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn patches_only_the_model_path_leaves_everything_else_untouched() {
        let dir = tempdir().unwrap();
        let model_path = dir.path().join("assets").join("models").join("Part.step");

        let mut map = HashMap::new();
        map.insert("Part.step".to_string(), model_path.clone());

        let patched = patch_model_paths(
            r#"(footprint (model "source/Part.step"))"#,
            &map,
            None,
            Some(dir.path()),
        );

        assert_eq!(
            patched,
            r#"(footprint (model "${KIPRJMOD}/assets/models/Part.step"))"#
        );
    }

    #[test]
    fn leaves_unresolved_model_paths_untouched() {
        let map = HashMap::new();
        let content = r#"(footprint (model "source/Unknown.step"))"#;
        let patched = patch_model_paths(content, &map, None, None);
        assert_eq!(patched, content);
    }

    #[test]
    fn never_touches_bareword_pad_property_flags() {
        // The core regression test for this module's whole design: real
        // footprint content containing `(property pad_prop_castellated)`
        // and `(property ki_fp_filters "...")` must survive byte-for-byte
        // since we never run it through the generic sexp parser.
        let content = "(footprint \"CASTCONN\"\n\t(pad \"1\" thru_hole circle (at 0 0) (size 1 1) (drill 1)\n\t\t(property pad_prop_castellated)\n\t)\n\t(property ki_fp_filters \"Connector*:*_2x??_*\")\n\t(model \"orig/Part.step\")\n)";
        let mut map = HashMap::new();
        map.insert(
            "Part.step".to_string(),
            PathBuf::from("/proj/3dmodels/Part.step"),
        );

        let patched = patch_model_paths(content, &map, None, Some(Path::new("/proj")));

        assert!(patched.contains("(property pad_prop_castellated)"));
        assert!(patched.contains(r#"(property ki_fp_filters "Connector*:*_2x??_*")"#));
        assert!(patched.contains(r#"(model "${KIPRJMOD}/3dmodels/Part.step")"#));
    }

    #[test]
    fn append_model_reference_inserts_before_the_closing_paren() {
        let content = "(footprint \"Conn\"\n\t(pad \"1\" thru_hole circle (at 0 0) (size 1 1) (drill 1))\n)";
        let patched =
            append_model_reference(content, Path::new("/proj/3dmodels/Part.stp"), Some(Path::new("/proj")));
        assert!(patched.contains(r#"(model "${KIPRJMOD}/3dmodels/Part.stp""#));
        assert!(patched.contains("(pad \"1\" thru_hole circle (at 0 0) (size 1 1) (drill 1))"));
        assert!(patched.trim_end().ends_with(')'));
    }

    #[test]
    fn a_footprint_shipped_with_no_model_reference_gets_the_sole_imported_model_attached() {
        // Regression test for a real-world Mouser/UltraLibrarian export:
        // the footprint and its STEP file are unrelated sibling files
        // with no `(model ...)` link between them at all.
        let dir = tempdir().unwrap();
        let lib_dir = dir.path().join("Combined.pretty");
        let src_fp = dir.path().join("CONN_48404-0003_MOL.kicad_mod");
        fs::write(
            &src_fp,
            "(footprint \"CONN_48404-0003_MOL\"\n\t(pad \"1\" thru_hole circle (at 0 0) (size 1 1) (drill 1))\n)",
        )
        .unwrap();

        let model_dest = dir.path().join("3dmodels").join("484040003.stp");
        fs::create_dir_all(model_dest.parent().unwrap()).unwrap();
        fs::write(&model_dest, b"fake step data").unwrap();
        let mut model_name_map = HashMap::new();
        model_name_map.insert("484040003.stp".to_string(), model_dest.clone());
        model_name_map.insert("484040003".to_string(), model_dest.clone());

        let mut fi =
            FootprintImporter::new(&lib_dir, false, Some(dir.path().to_path_buf()), |_| {}).unwrap();
        let name_map = fi.import_all(std::slice::from_ref(&src_fp), &model_name_map, None);
        assert_eq!(
            name_map.get("CONN_48404-0003_MOL"),
            Some(&"CONN_48404-0003_MOL".to_string())
        );

        let imported = fs::read_to_string(lib_dir.join("CONN_48404-0003_MOL.kicad_mod")).unwrap();
        assert!(imported.contains(r#"(model "${KIPRJMOD}/3dmodels/484040003.stp""#));
    }
}
