//! Shared by the `mouser_lookup`/`digikey_lookup` example CLIs only —
//! not part of the public library, and not related to this app's own
//! `GlobalSettings` (`~/.config/kicad-auto-importer/settings.json`,
//! used by the GUI). Lives at `examples/common/mod.rs` specifically so
//! Cargo's example auto-discovery (`examples/*.rs` and
//! `examples/*/main.rs`) doesn't treat it as a third example binary —
//! each example pulls it in with `#[path = "common/mod.rs"] mod common;`.
//!
//! Reads `~/.kicadautoimporterrc`, a dotfile format specific to these
//! test CLIs for convenience (so credentials don't need retyping as env
//! vars on every run): `[section]` headers, indentation-tolerant
//! `key = value` lines, blank lines and `#`/`;` full-line comments
//! ignored.
//!
//! ```ini
//! [digikey]
//!     client_id = ...
//!     client_secret = ...
//!
//! [mouser]
//!     api_key = ...
//! ```

use std::path::PathBuf;

fn rc_path() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".kicadautoimporterrc"))
}

fn read_rc_value(section: &str, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(rc_path()?).ok()?;
    let mut current_section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            current_section = name.trim().to_lowercase();
            continue;
        }
        if !current_section.eq_ignore_ascii_case(section) {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim().eq_ignore_ascii_case(key) {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// The env var wins if set to a non-blank value; otherwise falls back
/// to `~/.kicadautoimporterrc`'s `[rc_section]`/`rc_key`.
pub fn credential(env_var: &str, rc_section: &str, rc_key: &str) -> String {
    if let Ok(value) = std::env::var(env_var) {
        if !value.trim().is_empty() {
            return value;
        }
    }
    read_rc_value(rc_section, rc_key).unwrap_or_default()
}
