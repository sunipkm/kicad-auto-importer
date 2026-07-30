//! Persisted configuration for which columns appear in the priced XLSX export
//! and in what order — mirroring InteractiveHtmlBom's `FieldsPanel` (Show/Up/Down
//! reorder) but stored as a simple JSON array under
//! `bom-app/xlsx_columns.json` in the platform config directory.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::bom_report::XlsxColumn;

const CONFIG_FILENAME: &str = "xlsx_columns.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct XlsxColumnEntry {
    pub column: XlsxColumn,
    pub visible: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct XlsxColumnsConfig {
    pub entries: Vec<XlsxColumnEntry>,
}

impl Default for XlsxColumnsConfig {
    fn default() -> Self {
        Self {
            entries: XlsxColumn::ALL
                .iter()
                .map(|&col| XlsxColumnEntry { column: col, visible: true })
                .collect(),
        }
    }
}

impl XlsxColumnsConfig {
    fn config_path() -> Option<PathBuf> {
        Some(dirs::config_dir()?.join("bom-app").join(CONFIG_FILENAME))
    }

    /// Never fails — missing or corrupt config → default. Also defensively
    /// re-inserts any column from XlsxColumn::ALL that isn't in the saved config.
    pub fn load() -> Self {
        let saved: Option<Self> = Self::config_path()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str(&t).ok());

        let mut config = saved.unwrap_or_default();

        // Add any column not yet in the saved config (forward-compatibility).
        for &col in XlsxColumn::ALL {
            if !config.entries.iter().any(|e| e.column == col) {
                config.entries.push(XlsxColumnEntry { column: col, visible: true });
            }
        }

        // Mandatory columns can never be hidden.
        for entry in &mut config.entries {
            if entry.column.is_mandatory() {
                entry.visible = true;
            }
        }

        config
    }

    /// Full-snapshot overwrite, pretty-printed.
    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::config_path().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "could not determine the platform's config directory",
            )
        })?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text =
            serde_json::to_string_pretty(self).expect("XlsxColumnsConfig always serializes");
        fs::write(path, text)
    }

    /// The columns that should actually appear in an export, in entry order.
    pub fn visible_columns(&self) -> Vec<XlsxColumn> {
        self.entries.iter().filter(|e| e.visible).map(|e| e.column).collect()
    }
}
