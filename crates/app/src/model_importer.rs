//! 3-D model (.stp/.step/.wrl) import. Ported from
//! `plugins/importer/model_importer.py`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub struct ModelImporter<'a> {
    model_dir: PathBuf,
    overwrite: bool,
    log: Box<dyn FnMut(&str) + 'a>,
}

impl<'a> ModelImporter<'a> {
    pub fn new(
        model_dir: impl Into<PathBuf>,
        overwrite: bool,
        log: impl FnMut(&str) + 'a,
    ) -> std::io::Result<Self> {
        let model_dir = model_dir.into();
        fs::create_dir_all(&model_dir)?;
        Ok(ModelImporter {
            model_dir,
            overwrite,
            log: Box::new(log),
        })
    }

    /// Imports every model in `model_files`, returning a double-keyed
    /// map (both the original basename and the stem without extension
    /// map to the same destination absolute path) so callers can match
    /// by either form.
    pub fn import_all(&mut self, model_files: &[PathBuf]) -> HashMap<String, PathBuf> {
        let mut name_map = HashMap::new();
        for src in model_files {
            if let Some(dest) = self.import_one(src) {
                if let Some(basename) = src.file_name().and_then(|n| n.to_str()) {
                    name_map.insert(basename.to_string(), dest.clone());
                }
                if let Some(stem) = src.file_stem().and_then(|n| n.to_str()) {
                    name_map.insert(stem.to_string(), dest);
                }
            }
        }
        name_map
    }

    fn import_one(&mut self, src: &Path) -> Option<PathBuf> {
        let basename = src.file_name()?;
        let dest = self.model_dir.join(basename);

        if dest.exists() && !self.overwrite {
            (self.log)(&format!(
                "  Model '{}' skipped (exists).",
                basename.to_string_lossy()
            ));
            return Some(dest); // still return existing path so footprints can reference it
        }

        match fs::copy(src, &dest) {
            Ok(_) => {
                (self.log)(&format!(
                    "  Model '{}' copied → {}",
                    basename.to_string_lossy(),
                    self.model_dir.display()
                ));
                Some(dest)
            }
            Err(exc) => {
                (self.log)(&format!(
                    "  \u{2718} Could not copy model '{}': {exc}",
                    basename.to_string_lossy()
                ));
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn copies_model_and_maps_both_basename_and_stem() {
        let src_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let src = src_dir.path().join("Resistor.step");
        fs::write(&src, b"fake step content").unwrap();

        let mut logs = Vec::new();
        let mut importer =
            ModelImporter::new(dest_dir.path(), false, |m| logs.push(m.to_string())).unwrap();
        let map = importer.import_all(std::slice::from_ref(&src));

        let expected = dest_dir.path().join("Resistor.step");
        assert_eq!(map.get("Resistor.step"), Some(&expected));
        assert_eq!(map.get("Resistor"), Some(&expected));
        assert!(expected.exists());
    }

    #[test]
    fn skip_without_overwrite_still_returns_existing_path() {
        let src_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let src = src_dir.path().join("Resistor.step");
        fs::write(&src, b"new content").unwrap();
        let dest = dest_dir.path().join("Resistor.step");
        fs::write(&dest, b"old content").unwrap();

        let mut importer = ModelImporter::new(dest_dir.path(), false, |_| {}).unwrap();
        let map = importer.import_all(&[src]);

        assert_eq!(map.get("Resistor.step"), Some(&dest));
        assert_eq!(fs::read_to_string(&dest).unwrap(), "old content"); // not overwritten
    }

    #[test]
    fn overwrite_true_replaces_existing_file() {
        let src_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let src = src_dir.path().join("Resistor.step");
        fs::write(&src, b"new content").unwrap();
        let dest = dest_dir.path().join("Resistor.step");
        fs::write(&dest, b"old content").unwrap();

        let mut importer = ModelImporter::new(dest_dir.path(), true, |_| {}).unwrap();
        importer.import_all(&[src]);

        assert_eq!(fs::read_to_string(&dest).unwrap(), "new content");
    }
}
