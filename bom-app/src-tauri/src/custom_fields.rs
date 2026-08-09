//! Custom user-defined fields on part groups — store which fields exist,
//! read/write their values from schematic symbols, integrate with BOM export.
#![allow(dead_code)]
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

const CONFIG_FILENAME: &str = "custom_fields.json";

/// The set of custom fields the user has defined and wishes to track.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomFieldsConfig {
    /// Ordered list of custom field names. Order matters for Excel export.
    pub fields: Vec<String>,
}

impl CustomFieldsConfig {
    /// Load from ~/.Library/Application Support/kicad-auto-importer/custom_fields.json
    pub fn load() -> Self {
        if let Ok(path) = config_path() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(config) = serde_json::from_str(&content) {
                    return config;
                }
            }
        }
        Self { fields: Vec::new() }
    }

    /// Save to config directory.
    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = config_path()?;
        let content = serde_json::to_string_pretty(self)?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Add a new field if it doesn't already exist.
    pub fn add_field(&mut self, field_name: String) {
        if !self.fields.contains(&field_name) {
            self.fields.push(field_name);
        }
    }

    /// Remove a field by name.
    pub fn remove_field(&mut self, field_name: &str) {
        self.fields.retain(|f| f != field_name);
    }

    /// Get the index of a field, if it exists.
    pub fn field_index(&self, field_name: &str) -> Option<usize> {
        self.fields.iter().position(|f| f == field_name)
    }
}

/// Values of custom fields for a single part group — one entry per field
/// in `CustomFieldsConfig::fields`, in the same order.
pub type CustomFieldValues = HashMap<String, String>;

/// Read custom field values from a symbol in a schematic file.
pub fn read_custom_fields(
    sch_path: &std::path::Path,
    uuid: &str,
    field_names: &[String],
) -> Result<CustomFieldValues, Box<dyn std::error::Error>> {
    use kicad_parse::schematic::SchematicFile;

    let sch = SchematicFile::open(sch_path)?;
    let node = sch
        .get_symbol_node(uuid)
        .ok_or("Symbol not found in schematic")?;

    let mut values = CustomFieldValues::new();
    for field_name in field_names {
        if let Some(value) = kicad_parse::symbol_importer::get_symbol_property(&node, field_name)
        {
            values.insert(field_name.clone(), value);
        }
    }
    Ok(values)
}

/// Write custom field values back to a symbol in a schematic file.
pub fn write_custom_fields(
    sch_path: &std::path::Path,
    uuid: &str,
    values: &CustomFieldValues,
) -> Result<(), Box<dyn std::error::Error>> {
    use kicad_parse::schematic::SchematicFile;
    use kicad_parse::symbol_importer::set_symbol_property;

    let mut sch = SchematicFile::open(sch_path)?;
    let mut node = sch
        .get_symbol_node(uuid)
        .ok_or_else(|| format!("Symbol with UUID {} not found in {}", uuid, sch_path.display()))?;

    for (field_name, value) in values {
        set_symbol_property(&mut node, field_name, value);
    }
    sch.patch_symbol(uuid, &node);
    sch.save()?;
    Ok(())
}

fn config_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let config_dir = if cfg!(target_os = "macos") {
        std::path::PathBuf::from(
            std::env::var("HOME")? + "/.Library/Application Support/kicad-auto-importer",
        )
    } else if cfg!(target_os = "windows") {
        std::path::PathBuf::from(
            std::env::var("APPDATA")? + "\\kicad-auto-importer",
        )
    } else {
        std::path::PathBuf::from(
            std::env::var("HOME")? + "/.config/kicad-auto-importer",
        )
    };
    Ok(config_dir.join(CONFIG_FILENAME))
}
