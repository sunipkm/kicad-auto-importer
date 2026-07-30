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
        let mut name_map = HashMap::new();
        for fp_path in fp_files {
            let orig_name = fp_path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if let Some(imported) = self.import_one(fp_path, model_name_map, model_dir) {
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
        let patched = patch_model_paths(
            &content,
            model_name_map,
            model_dir,
            self.project_path.as_deref(),
        );
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
}
