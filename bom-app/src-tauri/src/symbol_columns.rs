//! Persisted configuration for which symbol properties appear in the
//! Populate BOM table and in what order — similar to `xlsx_columns`
//! but for the schematic symbol display, stored as `symbol_columns.json`
//! in the platform config directory.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const CONFIG_FILENAME: &str = "symbol_columns.json";

/// Standard symbol properties that can be displayed as columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "PascalCase")]
pub enum SymbolColumn {
    Reference,
    Value,
    Description,
    Footprint,
    Mpn,
}

impl SymbolColumn {
    pub const ALL: &'static [Self] = &[
        Self::Reference,
        Self::Value,
        Self::Description,
        Self::Footprint,
        Self::Mpn,
    ];

    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            Self::Reference => "Reference",
            Self::Value => "Value",
            Self::Description => "Description",
            Self::Footprint => "Footprint",
            Self::Mpn => "MPN",
        }
    }

    /// Reference is always shown; others can be hidden.
    pub fn is_mandatory(self) -> bool {
        matches!(self, Self::Reference)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SymbolColumnEntry {
    pub column: SymbolColumn,
    pub visible: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SymbolColumnsConfig {
    pub entries: Vec<SymbolColumnEntry>,
}

impl Default for SymbolColumnsConfig {
    fn default() -> Self {
        Self {
            entries: SymbolColumn::ALL
                .iter()
                .map(|&col| SymbolColumnEntry {
                    column: col,
                    visible: true,
                })
                .collect(),
        }
    }
}

impl SymbolColumnsConfig {
    fn config_path() -> Option<PathBuf> {
        Some(dirs::config_dir()?.join("bom-app").join(CONFIG_FILENAME))
    }

    /// Never fails — missing or corrupt config → default. Also defensively
    /// re-inserts any column not yet in the saved config (forward-compatibility).
    pub fn load() -> Self {
        let saved: Option<Self> = Self::config_path()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str(&t).ok());

        let mut config = saved.unwrap_or_default();

        // Add any column not yet in the saved config (forward-compatibility).
        for &col in SymbolColumn::ALL {
            if !config.entries.iter().any(|e| e.column == col) {
                config.entries.push(SymbolColumnEntry {
                    column: col,
                    visible: true,
                });
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
            serde_json::to_string_pretty(self).expect("SymbolColumnsConfig always serializes");
        fs::write(path, text)
    }

    /// The columns that should actually appear in a display, in entry order.
    #[allow(dead_code)]
    pub fn visible_columns(&self) -> Vec<SymbolColumn> {
        self.entries
            .iter()
            .filter(|e| e.visible)
            .map(|e| e.column)
            .collect()
    }
}
