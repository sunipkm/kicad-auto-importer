//! BOM generation settings: passive extra margin, minimum quantities, etc.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const CONFIG_FILENAME: &str = "bom_config.json";

/// BOM generation configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BomConfig {
    /// Minimum extra pieces to add to passive component orders, even for
    /// tiny quantities. Applied only when extra_percent > 0.
    pub passive_extra_minimum: u32,
}

impl Default for BomConfig {
    fn default() -> Self {
        Self {
            passive_extra_minimum: 5,
        }
    }
}

impl BomConfig {
    /// Load from config directory, defaulting to 5 if missing or unreadable.
    pub fn load() -> Self {
        if let Ok(path) = config_path() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(config) = serde_json::from_str(&content) {
                    return config;
                }
            }
        }
        Self::default()
    }

    /// Save to config directory.
    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = config_path()?;
        let content = serde_json::to_string_pretty(self)?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path, content)?;
        Ok(())
    }
}

fn config_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let config_dir = if cfg!(target_os = "macos") {
        std::path::PathBuf::from(
            std::env::var("HOME")? + "/.Library/Application Support/kicad-auto-importer",
        )
    } else if cfg!(target_os = "windows") {
        std::path::PathBuf::from(std::env::var("APPDATA")? + "\\kicad-auto-importer")
    } else {
        std::path::PathBuf::from(std::env::var("HOME")? + "/.config/kicad-auto-importer")
    };
    Ok(config_dir.join(CONFIG_FILENAME))
}
