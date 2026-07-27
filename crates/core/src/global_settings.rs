//! Global (not per-project) app settings — currently just the
//! Octopart/Nexar API credentials (see `crate::octopart`).
//!
//! Deliberately separate from `config.rs`'s `ImporterConfig`: that one
//! is explicitly, by design, project-scoped (see its module docs) with
//! no global fallback location, because every one of its fields only
//! makes sense in the context of a specific KiCad project. An API
//! client ID/secret is an account-level credential that has nothing to
//! do with any one project, so it lives in its own file in a genuine
//! global location instead — `dirs::config_dir()` (`~/.config` on
//! Linux, `~/Library/Application Support` on macOS, `%APPDATA%` on
//! Windows), the same crate already used elsewhere in this codebase for
//! platform-appropriate directory discovery (see
//! `kicad_paths::candidate_kicad_config_dirs`).

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const SETTINGS_FILENAME: &str = "settings.json";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GlobalSettings {
    #[serde(default)]
    pub octopart_client_id: String,
    #[serde(default)]
    pub octopart_client_secret: String,
}

impl GlobalSettings {
    fn settings_path() -> Option<PathBuf> {
        Some(
            dirs::config_dir()?
                .join("kicad-auto-importer")
                .join(SETTINGS_FILENAME),
        )
    }

    /// Never fails — same philosophy as `ImporterConfig::load`: no
    /// settings file, no config dir, or corrupt JSON all just mean
    /// "start from defaults" rather than an error the caller has to
    /// handle.
    pub fn load() -> Self {
        Self::settings_path()
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Full-snapshot overwrite, pretty-printed.
    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::settings_path().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "could not determine the platform's config directory",
            )
        })?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self).expect("GlobalSettings always serializes");
        fs::write(path, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `GlobalSettings::load`/`save` go through `dirs::config_dir()`,
    /// which isn't test-overridable — these tests exercise the pure
    /// serde round-trip and never-fails-on-garbage behavior directly,
    /// the same properties `ImporterConfig`'s own tests check, just
    /// without touching the real global config path.
    #[test]
    fn defaults_are_empty_strings() {
        let settings = GlobalSettings::default();
        assert_eq!(settings.octopart_client_id, "");
        assert_eq!(settings.octopart_client_secret, "");
    }

    #[test]
    fn round_trips_through_serde() {
        let settings = GlobalSettings {
            octopart_client_id: "some-client-id".to_string(),
            octopart_client_secret: "some-client-secret".to_string(),
        };
        let text = serde_json::to_string_pretty(&settings).unwrap();
        let loaded: GlobalSettings = serde_json::from_str(&text).unwrap();
        assert_eq!(loaded, settings);
    }

    #[test]
    fn corrupt_json_falls_back_to_defaults() {
        let loaded: GlobalSettings = serde_json::from_str("{ not valid json").unwrap_or_default();
        assert_eq!(loaded, GlobalSettings::default());
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        // Matches ImporterConfig's `#[serde(default)]` behavior: a
        // settings file predating a new field (or a hand-edited partial
        // one) still loads instead of failing outright.
        let loaded: GlobalSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(loaded, GlobalSettings::default());
    }
}
