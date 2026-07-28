//! Self-registers a `.desktop` launcher entry and a hicolor icon set in
//! the user's XDG data directories on startup — `bom-app` ships as a
//! bare Linux binary (see `.github/workflows/bom-app.yml`), not a
//! `.deb`/AppImage, so there's no packaging step to install a desktop
//! file for us. Without this, the app has no icon in the taskbar/app
//! launcher and no entry a user can pin or search for.
//!
//! Mirrors `kicad-auto-importer`'s own
//! `crates/app/src/linux_desktop_integration.rs` (same mechanism, this
//! app's own identity) rather than sharing code with it: the one real
//! difference is the icon source — `crates/app` renders its icon from
//! vector data at every size on demand, while this app just re-writes
//! the same static PNGs Tauri's own bundler config already points at
//! (`icons/`), so pulling that behind a shared abstraction wouldn't
//! actually remove much duplication.
//!
//! Runs off the GUI thread so a slow or read-only home directory never
//! delays startup, and is a cheap no-op on every launch after the first:
//! it compares the recorded `Exec=` line against the current executable
//! path before writing anything.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const APP_ID: &str = "kicad-bom-tool";

/// `(hicolor size, embedded PNG bytes)` — sized to exactly match files
/// already shipped in `icons/` for Tauri's own bundler, so writing them
/// out here needs no image-decode dependency, just the raw bytes.
const ICONS: &[(u32, &[u8])] = &[
    (32, include_bytes!("../icons/32x32.png")),
    (128, include_bytes!("../icons/128x128.png")),
    (256, include_bytes!("../icons/128x128@2x.png")),
    (512, include_bytes!("../icons/icon.png")),
];

/// Fire-and-forget: spawns the registration off-thread and never reports
/// success/failure back to the caller. This is a best-effort cosmetic
/// integration, not something the app's own startup should ever block on
/// or fail over.
pub fn spawn_registration() {
    let _ = std::thread::Builder::new()
        .name("desktop-integration".into())
        .spawn(|| {
            if let Err(err) = register() {
                eprintln!("desktop integration: {err}");
            }
        });
}

fn data_home() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return Some(dir);
        }
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
}

fn register() -> Result<(), Box<dyn std::error::Error>> {
    let data_home = data_home().ok_or("neither XDG_DATA_HOME nor HOME is set")?;
    register_to(&data_home)
}

/// Split out from [`register`] so tests can point it at a throwaway
/// directory instead of mutating the process-wide `$HOME`/`$XDG_DATA_HOME`
/// env vars, which would race against any other test running in parallel
/// in this same test binary.
fn register_to(data_home: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let exe = std::env::current_exe()?.canonicalize()?;

    let desktop_path = data_home
        .join("applications")
        .join(format!("{APP_ID}.desktop"));
    let exec_line = format!("Exec={}", quote(&exe));

    if let Ok(existing) = fs::read_to_string(&desktop_path) {
        if existing.lines().any(|line| line == exec_line) {
            return Ok(()); // already registered for this exact binary path
        }
    }

    for &(size, bytes) in ICONS {
        write_icon(data_home, size, bytes)?;
    }
    write_desktop_file(&desktop_path, &exec_line)?;

    // Best-effort cache refresh: neither xdg-utils nor these caches are
    // guaranteed to exist, and a stale cache is purely cosmetic (fixed by
    // the next login or a manual refresh either way), so failures here
    // are silently ignored rather than surfaced.
    let _ = std::process::Command::new("update-desktop-database")
        .arg(data_home.join("applications"))
        .status();
    let _ = std::process::Command::new("gtk-update-icon-cache")
        .args(["-f", "-t"])
        .arg(data_home.join("icons/hicolor"))
        .status();

    Ok(())
}

fn write_icon(data_home: &Path, size: u32, bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let dir = data_home
        .join("icons/hicolor")
        .join(format!("{size}x{size}"))
        .join("apps");
    fs::create_dir_all(&dir)?;
    fs::write(dir.join(format!("{APP_ID}.png")), bytes)?;
    Ok(())
}

fn write_desktop_file(path: &Path, exec_line: &str) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(path.parent().expect("desktop_path always has a parent"))?;
    let mut file = fs::File::create(path)?;
    write!(
        file,
        "[Desktop Entry]\n\
         Type=Application\n\
         Version=1.0\n\
         Name=KiCad BOM Tool\n\
         Comment=Populate a KiCad schematic's bill of materials with Mouser/DigiKey data and generate a priced BOM report\n\
         {exec_line}\n\
         Icon={APP_ID}\n\
         Terminal=false\n\
         Categories=Development;Electronics;\n\
         StartupWMClass={APP_ID}\n"
    )?;
    Ok(())
}

/// `Exec=` values are parsed by desktop-file consumers with shell-like
/// quoting rules (freedesktop.org Desktop Entry Spec §Exec), so a path
/// containing a space would otherwise be split into multiple arguments.
fn quote(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s.chars().any(|c| c.is_whitespace() || c == '"') {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn writes_desktop_file_and_every_icon_size() {
        let dir = tempdir().unwrap();
        register_to(dir.path()).unwrap();

        let desktop = fs::read_to_string(
            dir.path()
                .join("applications")
                .join(format!("{APP_ID}.desktop")),
        )
        .unwrap();
        assert!(desktop.starts_with("[Desktop Entry]\n"));
        assert!(desktop.contains(&format!("Icon={APP_ID}\n")));
        assert!(desktop.contains(&format!("StartupWMClass={APP_ID}\n")));

        let exe = std::env::current_exe().unwrap().canonicalize().unwrap();
        assert!(desktop.contains(&format!("Exec={}\n", quote(&exe))));

        for &(size, _) in ICONS {
            let icon = dir
                .path()
                .join("icons/hicolor")
                .join(format!("{size}x{size}"))
                .join("apps")
                .join(format!("{APP_ID}.png"));
            assert!(icon.is_file(), "missing {size}x{size} icon");
        }
    }

    #[test]
    fn second_call_with_unchanged_exe_path_is_idempotent() {
        let dir = tempdir().unwrap();
        register_to(dir.path()).unwrap();

        let desktop_path = dir
            .path()
            .join("applications")
            .join(format!("{APP_ID}.desktop"));
        let before = fs::read_to_string(&desktop_path).unwrap();

        register_to(dir.path()).unwrap();

        assert_eq!(before, fs::read_to_string(&desktop_path).unwrap());
    }
}
