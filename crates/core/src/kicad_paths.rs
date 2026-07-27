//! KiCad path-variable expansion and sym-lib-table / fp-lib-table
//! parsing/writing. Ported from the sibling Python plugin's
//! `plugins/importer/kicad_paths.py`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;

use crate::sexp::{self, Atom, Child, SexpNode};

// ── KiCad config directory discovery ────────────────────────────────────

/// Plausible KiCad per-user config directories, newest version first,
/// across the platforms KiCad supports.
pub fn candidate_kicad_config_dirs() -> Vec<PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    let mut bases: Vec<PathBuf> = Vec::new();

    if cfg!(target_os = "windows") {
        let appdata = std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join("AppData").join("Roaming"));
        bases.push(appdata.join("kicad"));
    } else if cfg!(target_os = "macos") {
        bases.push(home.join("Library").join("Preferences").join("kicad"));
        bases.push(
            home.join("Library")
                .join("Application Support")
                .join("kicad"),
        );
    } else {
        bases.push(home.join(".config").join("kicad"));
    }

    let mut dirs_out = Vec::new();
    for base in bases {
        if !base.is_dir() {
            continue;
        }
        let mut versions: Vec<String> = fs::read_dir(&base)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.path().is_dir())
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default();
        versions.sort_by(|a, b| b.cmp(a)); // newest-looking version string first
        for v in versions {
            dirs_out.push(base.join(v));
        }
        dirs_out.push(base); // fallback: some setups store files directly here
    }
    dirs_out
}

/// Best-effort read of user-defined path variables (Preferences →
/// Configure Paths) from `kicad_common.json`. Empty if not found.
/// Cached for the lifetime of the process, mirroring the Python
/// `lru_cache(maxsize=1)`.
fn load_user_env_vars() -> &'static std::collections::HashMap<String, String> {
    static CACHE: OnceLock<std::collections::HashMap<String, String>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut result = std::collections::HashMap::new();
        for cfg_dir in candidate_kicad_config_dirs() {
            let path = cfg_dir.join("kicad_common.json");
            if !path.is_file() {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            if let Some(vars) = value
                .get("environment")
                .and_then(|e| e.get("vars"))
                .and_then(|v| v.as_object())
            {
                for (k, v) in vars {
                    // First file found wins for each key; don't overwrite.
                    result
                        .entry(k.clone())
                        .or_insert_with(|| json_value_to_string(v));
                }
            }
        }
        result
    })
}

fn json_value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Search standard KiCad config locations for a global sym-lib-table or
/// fp-lib-table. `table_filename` should be `sym-lib-table` or
/// `fp-lib-table`. Returns the first match found, or `None`.
pub fn find_global_lib_table(table_filename: &str) -> Option<PathBuf> {
    candidate_kicad_config_dirs()
        .into_iter()
        .map(|dir| dir.join(table_filename))
        .find(|p| p.is_file())
}

// ── path-variable expansion ─────────────────────────────────────────────

fn var_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\$\{(\w+)\}").unwrap())
}

/// Expand `${VAR}` tokens the way KiCad resolves them:
///   1. `${KIPRJMOD}` -> `kiprjmod` (the relevant project directory)
///   2. environment variables
///   3. user-defined vars from `kicad_common.json`
///
/// Unresolved variables are left as literal `${VAR}` text.
pub fn expand_kicad_vars(raw_path: &str, kiprjmod: Option<&str>) -> String {
    let path = raw_path.replace('\\', "/");
    var_re()
        .replace_all(&path, |caps: &regex::Captures| -> String {
            let var = &caps[1];
            if var == "KIPRJMOD" {
                if let Some(kp) = kiprjmod {
                    if !kp.is_empty() {
                        return kp.replace('\\', "/");
                    }
                }
            }
            if let Ok(val) = std::env::var(var) {
                return val;
            }
            if let Some(val) = load_user_env_vars().get(var) {
                return val.replace('\\', "/");
            }
            caps[0].to_string()
        })
        .to_string()
}

// ── lib table parsing ────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LibEntry {
    pub name: String,
    pub lib_type: String,
    pub uri: String,
    pub descr: String,
}

/// Parse a sym-lib-table or fp-lib-table file. Returns `[]` if the file
/// doesn't exist or fails to parse.
pub fn parse_lib_table(table_path: &Path, kiprjmod: Option<&str>) -> Vec<LibEntry> {
    let Ok(text) = fs::read_to_string(table_path) else {
        return Vec::new();
    };
    let Ok(root) = sexp::parse(&text) else {
        return Vec::new();
    };

    let mut entries = Vec::new();
    for lib in root.find_all("lib") {
        let mut entry = LibEntry::default();
        for child in &lib.children {
            if let Child::Node(sub) = child {
                let val = sub
                    .first_atom()
                    .map(|a| a.text().to_string())
                    .unwrap_or_default();
                match sub.name.as_str() {
                    "name" => entry.name = val,
                    "type" => entry.lib_type = val,
                    "uri" => entry.uri = val,
                    "descr" => entry.descr = val,
                    _ => {}
                }
            }
        }
        if !entry.uri.is_empty() {
            entry.uri = expand_kicad_vars(&entry.uri, kiprjmod);
        }
        if !entry.name.is_empty() {
            entries.push(entry);
        }
    }
    entries
}

/// Parse only the project-local table (sym-lib-table or fp-lib-table).
pub fn load_project_local_table(project_dir: &Path, table_filename: &str) -> Vec<LibEntry> {
    parse_lib_table(
        &project_dir.join(table_filename),
        Some(&project_dir.to_string_lossy()),
    )
}

// ── lib table writing / registration ────────────────────────────────────

/// Return a `${KIPRJMOD}/...`-style URI for `path` if it lies inside
/// `project_dir` (KiCad's recommended, portable form for
/// project-specific libraries); otherwise return the absolute path
/// unchanged (with forward slashes), since `${KIPRJMOD}` can't
/// represent a location outside the project.
pub fn kiprjmod_relative_uri(path: &Path, project_dir: &Path) -> String {
    let path_norm = normalize_lexically(&to_absolute(path));

    if !project_dir.as_os_str().is_empty() {
        let project_norm = normalize_lexically(&to_absolute(project_dir));
        if let Ok(rel) = path_norm.strip_prefix(&project_norm) {
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            return format!("${{KIPRJMOD}}/{}", rel_str);
        }
    }
    path_norm.to_string_lossy().replace('\\', "/")
}

fn to_absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    }
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn fresh_table_root(root_name: &str) -> SexpNode {
    let mut root = SexpNode::new(root_name);
    // Real KiCad writes this bare, e.g. `(version 7)`, not `(version "7")`.
    root.push_node(SexpNode::leaf("version", Atom::bare("7")));
    root
}

fn unique_nickname(existing: &HashSet<String>, desired: &str) -> String {
    if !existing.contains(desired) {
        return desired.to_string();
    }
    let mut i = 2;
    loop {
        let candidate = format!("{desired}_{i}");
        if !existing.contains(&candidate) {
            return candidate;
        }
        i += 1;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LibTableError {
    #[error("io error writing lib table: {0}")]
    Io(#[from] std::io::Error),
}

/// Ensure a `(lib ...)` entry pointing at `uri` exists in the table at
/// `table_path`, creating the table file if it doesn't exist yet.
///
/// Returns `(changed, actual_nickname)`:
///   - `changed = false` if an entry with this exact uri is already
///     present (`actual_nickname` is whatever it's currently registered
///     under — which may differ from the requested `nickname` if it was
///     renamed by the user in KiCad).
///   - `changed = true` if a new entry was appended (`actual_nickname`
///     is the requested `nickname`, or a suffixed variant if that name
///     was already taken by a different library).
pub fn add_or_update_lib_entry(
    table_path: &Path,
    nickname: &str,
    uri: &str,
    lib_type: &str,
    descr: &str,
    mut log: impl FnMut(&str),
) -> Result<(bool, String), LibTableError> {
    let root_name = table_path
        .file_name()
        .map(|n| n.to_string_lossy().replace('-', "_"))
        .unwrap_or_else(|| "lib_table".to_string());

    let mut root = if table_path.is_file() {
        let parsed = fs::read_to_string(table_path)
            .ok()
            .and_then(|s| sexp::parse(&s).ok());
        match parsed {
            Some(r) => r,
            None => {
                log(&format!(
                    "  \u{26a0} Existing {} could not be parsed; a fresh one will be created — back it up if it had custom entries.",
                    table_path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
                ));
                fresh_table_root(&root_name)
            }
        }
    } else {
        fresh_table_root(&root_name)
    };

    let mut existing_names: HashSet<String> = HashSet::new();
    for lib in root.find_all("lib") {
        let mut entry_name = String::new();
        let mut entry_uri = String::new();
        for child in &lib.children {
            if let Child::Node(sub) = child {
                let val = sub
                    .first_atom()
                    .map(|a| a.text().to_string())
                    .unwrap_or_default();
                match sub.name.as_str() {
                    "name" => entry_name = val,
                    "uri" => entry_uri = val,
                    _ => {}
                }
            }
        }
        if !entry_name.is_empty() {
            existing_names.insert(entry_name.clone());
        }
        if entry_uri == uri {
            return Ok((false, entry_name)); // already registered under this uri
        }
    }

    let final_nickname = unique_nickname(&existing_names, nickname);

    let mut lib_node = SexpNode::new("lib");
    lib_node.push_node(SexpNode::leaf("name", Atom::quoted(final_nickname.clone())));
    lib_node.push_node(SexpNode::leaf("type", Atom::quoted(lib_type)));
    lib_node.push_node(SexpNode::leaf("uri", Atom::quoted(uri)));
    lib_node.push_node(SexpNode::leaf("options", Atom::quoted("")));
    lib_node.push_node(SexpNode::leaf("descr", Atom::quoted(descr)));
    root.push_node(lib_node);

    if let Some(parent) = table_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(table_path, sexp::render(&root))?;

    Ok((true, final_nickname))
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// Ensure `lib_path` (a `.kicad_sym` file or `.pretty` directory) is
/// registered in the project-local sym-lib-table / fp-lib-table, using a
/// `${KIPRJMOD}`-relative URI whenever the library lives inside the
/// project directory. Safe to call every time a library is written to —
/// no-ops if it's already registered, but always logs the outcome
/// (registered / already-registered / failed) so registration is never
/// silent.
///
/// Returns the nickname the library is actually registered under, which
/// callers should use when building "Nickname:FootprintName" references.
pub fn register_project_library(
    project_dir: Option<&Path>,
    lib_path: &Path,
    table_filename: &str,
    mut log: impl FnMut(&str),
) -> String {
    let requested_nickname = lib_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    let Some(project_dir) = project_dir else {
        log(&format!(
            "  \u{26a0} No project directory known — skipping {table_filename} registration for '{requested_nickname}'. \
             Open a KiCad project (save the board/schematic) and re-import, or add the library manually in KiCad."
        ));
        return requested_nickname;
    };

    let uri = kiprjmod_relative_uri(lib_path, project_dir);
    let table_path = project_dir.join(table_filename);
    let kind = if table_filename.contains("sym") {
        "symbol"
    } else {
        "footprint"
    };

    match add_or_update_lib_entry(
        &table_path,
        &requested_nickname,
        &uri,
        "KiCad",
        "Added automatically by kicad-auto-importer",
        &mut log,
    ) {
        Ok((changed, actual_nickname)) => {
            if changed {
                if actual_nickname != requested_nickname {
                    log(&format!(
                        "  Registered {kind} library as '{actual_nickname}' in {table_filename} \
                         ('{requested_nickname}' was already taken by another library) → {uri}"
                    ));
                } else {
                    log(&format!(
                        "  Registered {kind} library '{actual_nickname}' in {table_filename} → {uri}"
                    ));
                }
            } else {
                log(&format!(
                    "  {} library '{actual_nickname}' already registered in {table_filename} → {uri}",
                    capitalize(kind)
                ));
            }
            actual_nickname
        }
        Err(exc) => {
            log(&format!(
                "  \u{2718} Failed to register {kind} library '{requested_nickname}' in {table_filename} \
                 ({}): {exc}",
                table_path.display()
            ));
            requested_nickname
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn kiprjmod_relative_uri_inside_project() {
        let dir = tempdir().unwrap();
        let project_dir = dir.path();
        let lib_path = project_dir
            .join("libraries")
            .join("symbols")
            .join("Parts.kicad_sym");
        assert_eq!(
            kiprjmod_relative_uri(&lib_path, project_dir),
            "${KIPRJMOD}/libraries/symbols/Parts.kicad_sym"
        );
    }

    #[test]
    fn register_new_library_creates_table() {
        let dir = tempdir().unwrap();
        let project_dir = dir.path();
        let lib_path = project_dir.join("Parts.kicad_sym");

        let mut logs = Vec::new();
        let nickname =
            register_project_library(Some(project_dir), &lib_path, "sym-lib-table", |m| {
                logs.push(m.to_string())
            });
        assert_eq!(nickname, "Parts");

        let table_path = project_dir.join("sym-lib-table");
        assert!(table_path.is_file());
        let entries = parse_lib_table(&table_path, Some(&project_dir.to_string_lossy()));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Parts");
        // Registered URIs are always forward-slash-normalized (KiCad's own
        // file format convention, even on Windows) — `to_string_lossy()`
        // alone would keep Windows' native backslashes and mismatch here.
        assert_eq!(
            entries[0].uri,
            project_dir
                .join("Parts.kicad_sym")
                .to_string_lossy()
                .replace('\\', "/")
        );
    }

    #[test]
    fn re_registering_same_uri_is_a_noop_and_reuses_nickname() {
        let dir = tempdir().unwrap();
        let project_dir = dir.path();
        let lib_path = project_dir.join("Parts.pretty");

        register_project_library(Some(project_dir), &lib_path, "fp-lib-table", |_| {});
        // Simulate the library having been renamed to something else by
        // the user in KiCad, then re-registering the *same* uri again.
        let table_path = project_dir.join("fp-lib-table");
        let text = fs::read_to_string(&table_path).unwrap();
        let renamed = text.replace("\"Parts\"", "\"MyRenamedLib\"");
        fs::write(&table_path, renamed).unwrap();

        let mut changed_flag = None;
        let nickname =
            register_project_library(Some(project_dir), &lib_path, "fp-lib-table", |msg| {
                if msg.contains("already registered") {
                    changed_flag = Some(false);
                }
            });
        assert_eq!(nickname, "MyRenamedLib");
        assert_eq!(changed_flag, Some(false));

        let entries = parse_lib_table(&table_path, Some(&project_dir.to_string_lossy()));
        assert_eq!(entries.len(), 1); // no duplicate entry added
    }

    #[test]
    fn nickname_collision_with_different_uri_gets_suffixed() {
        let dir = tempdir().unwrap();
        let project_dir = dir.path();

        register_project_library(
            Some(project_dir),
            &project_dir.join("Parts.kicad_sym"),
            "sym-lib-table",
            |_| {},
        );
        let nickname = register_project_library(
            Some(project_dir),
            &project_dir.join("other_dir").join("Parts.kicad_sym"),
            "sym-lib-table",
            |_| {},
        );
        assert_eq!(nickname, "Parts_2");
    }

    #[test]
    fn missing_project_dir_skips_registration_without_panicking() {
        let mut logs = Vec::new();
        let nickname = register_project_library(
            None,
            Path::new("/tmp/whatever/Parts.kicad_sym"),
            "sym-lib-table",
            |m| logs.push(m.to_string()),
        );
        assert_eq!(nickname, "Parts");
        assert!(logs[0].contains("No project directory known"));
    }
}
