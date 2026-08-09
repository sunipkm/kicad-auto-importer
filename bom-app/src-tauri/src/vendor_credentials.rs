//! bom-app's own global (not per-project) settings: just the
//! Mouser/DigiKey API credentials (see `crate::parts_lookup`) — an
//! account-level credential that has nothing to do with any one project or with the
//! separate `kicad-auto-importer` desktop app, which has its own,
//! entirely disjoint global settings in its own app crate — there's
//! nothing left for the two apps to actually share here, so this stays
//! local to bom-app rather than living in the shared core crate.
//!
//! Lives under `~/.config/bom-app/settings.json` (per-OS equivalent of
//! `dirs::config_dir()`).

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const SETTINGS_FILENAME: &str = "settings.json";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VendorCredentials {
    #[serde(default)]
    pub mouser_api_key: String,
    #[serde(default)]
    pub digikey_client_id: String,
    #[serde(default)]
    pub digikey_client_secret: String,
    #[serde(default)]
    pub arrow_api_key: String,
}

impl VendorCredentials {
    fn settings_path() -> Option<PathBuf> {
        Some(dirs::config_dir()?.join("bom-app").join(SETTINGS_FILENAME))
    }

    /// Never fails — no settings file, no config dir, or corrupt JSON
    /// all just mean "start from defaults" rather than an error the
    /// caller has to handle.
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
        let text = serde_json::to_string_pretty(self).expect("VendorCredentials always serializes");
        fs::write(path, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `load`/`save` go through `dirs::config_dir()`, which isn't
    /// test-overridable — these tests exercise the pure serde
    /// round-trip and never-fails-on-garbage behavior directly.
    #[test]
    fn defaults_are_empty_strings() {
        let settings = VendorCredentials::default();
        assert_eq!(settings.mouser_api_key, "");
        assert_eq!(settings.digikey_client_id, "");
        assert_eq!(settings.digikey_client_secret, "");
        assert_eq!(settings.arrow_api_key, "");
    }

    #[test]
    fn round_trips_through_serde() {
        let settings = VendorCredentials {
            mouser_api_key: "some-mouser-key".to_string(),
            digikey_client_id: "some-client-id".to_string(),
            digikey_client_secret: "some-client-secret".to_string(),
            arrow_api_key: "some-arrow-key".to_string(),
        };
        let text = serde_json::to_string_pretty(&settings).unwrap();
        let loaded: VendorCredentials = serde_json::from_str(&text).unwrap();
        assert_eq!(loaded, settings);
    }

    #[test]
    fn corrupt_json_falls_back_to_defaults() {
        let loaded: VendorCredentials =
            serde_json::from_str("{ not valid json").unwrap_or_default();
        assert_eq!(loaded, VendorCredentials::default());
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let loaded: VendorCredentials = serde_json::from_str("{}").unwrap();
        assert_eq!(loaded, VendorCredentials::default());
    }
}
