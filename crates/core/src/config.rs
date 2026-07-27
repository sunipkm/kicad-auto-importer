//! Config persistence — a drop-in-compatible reader/writer for the
//! existing Python plugin's `ultralib_importer.json`, so a project
//! already configured there needs zero reconfiguration to work with
//! this tool.
//!
//! Deliberate deviation from the Python version: this tool is always
//! single-project-scoped (per design decision), so there is no global
//! fallback config location — a project directory is always required.
//! Python's fallback path was hardcoded Linux-only anyway
//! (`~/.config/kicad/ultralib_importer/...`, ignoring `%APPDATA%` /
//! `~/Library/...` on other platforms), so nothing is lost by dropping it.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const CONFIG_FILENAME: &str = "ultralib_importer.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImporterConfig {
    #[serde(default)]
    pub watch_folder: String,
    #[serde(default)]
    pub symbol_lib: String,
    #[serde(default)]
    pub footprint_lib: String,
    #[serde(default = "default_model_subdir")]
    pub model_subdir: String,
    #[serde(default)]
    pub move_zip: bool,
    #[serde(default = "default_true")]
    pub backup_zip: bool,
    #[serde(default)]
    pub overwrite: bool,
}

fn default_model_subdir() -> String {
    "3dmodels".to_string()
}

fn default_true() -> bool {
    true
}

impl Default for ImporterConfig {
    fn default() -> Self {
        ImporterConfig {
            watch_folder: String::new(),
            symbol_lib: String::new(),
            footprint_lib: String::new(),
            model_subdir: default_model_subdir(),
            move_zip: false,
            backup_zip: true,
            overwrite: false,
        }
    }
}

impl ImporterConfig {
    pub fn config_path(project_dir: &Path) -> PathBuf {
        project_dir.join(CONFIG_FILENAME)
    }

    /// Never fails — mirrors the Python version's `except Exception:
    /// pass` followed by returning `{}` (here: all field defaults).
    pub fn load(project_dir: &Path) -> Self {
        let path = Self::config_path(project_dir);
        fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Full-snapshot overwrite, pretty-printed (matches
    /// `json.dump(..., indent=2)` in shape, not necessarily byte-for-byte).
    pub fn save(&self, project_dir: &Path) -> std::io::Result<()> {
        let path = Self::config_path(project_dir);
        let text = serde_json::to_string_pretty(self).expect("ImporterConfig always serializes");
        fs::write(path, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempdir().unwrap();
        let cfg = ImporterConfig::load(dir.path());
        assert_eq!(cfg, ImporterConfig::default());
        assert_eq!(cfg.model_subdir, "3dmodels");
        assert!(cfg.backup_zip);
        assert!(!cfg.move_zip);
        assert!(!cfg.overwrite);
    }

    #[test]
    fn corrupt_file_yields_defaults_without_panicking() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(CONFIG_FILENAME), "{ not valid json").unwrap();
        let cfg = ImporterConfig::load(dir.path());
        assert_eq!(cfg, ImporterConfig::default());
    }

    #[test]
    fn round_trips_through_save_and_load() {
        let dir = tempdir().unwrap();
        let cfg = ImporterConfig {
            watch_folder: "/home/user/Downloads".to_string(),
            symbol_lib: "${KIPRJMOD}/Parts.kicad_sym".to_string(),
            footprint_lib: "${KIPRJMOD}/Parts.pretty".to_string(),
            model_subdir: "3dmodels".to_string(),
            move_zip: true,
            backup_zip: false,
            overwrite: true,
        };
        cfg.save(dir.path()).unwrap();
        let loaded = ImporterConfig::load(dir.path());
        assert_eq!(loaded, cfg);
    }

    #[test]
    fn reads_a_config_file_shaped_like_the_python_plugins() {
        // Exact schema/values as produced by plugins/config.py +
        // main_dialog.py in the sibling Python repo — this is the real
        // interop test: a project configured by the Python plugin must
        // load correctly here with zero reconfiguration.
        let dir = tempdir().unwrap();
        let json = r#"{
  "watch_folder": "/home/sunip/Downloads",
  "symbol_lib": "${KIPRJMOD}/chickadee-stamp-v3.kicad_sym",
  "footprint_lib": "${KIPRJMOD}/chickadee-stamp-v3.pretty",
  "model_subdir": "3dmodels",
  "move_zip": false,
  "backup_zip": false,
  "overwrite": false
}"#;
        fs::write(dir.path().join(CONFIG_FILENAME), json).unwrap();
        let cfg = ImporterConfig::load(dir.path());
        assert_eq!(cfg.watch_folder, "/home/sunip/Downloads");
        assert_eq!(cfg.symbol_lib, "${KIPRJMOD}/chickadee-stamp-v3.kicad_sym");
        assert_eq!(cfg.footprint_lib, "${KIPRJMOD}/chickadee-stamp-v3.pretty");
        assert!(!cfg.backup_zip);
    }
}
