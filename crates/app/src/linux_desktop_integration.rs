//! Self-registers a `.desktop` launcher entry and a hicolor icon set in
//! the user's XDG data directories on startup, the same way Telegram
//! Desktop's Linux tarball build does — this project ships bare
//! per-platform binaries (see `.github/workflows/release.yml`), not a
//! `.deb`/AppImage/Flatpak, so there's no packaging step to install a
//! desktop file for us. Without this, the app has no icon in the
//! taskbar/app launcher and no entry a user can pin or search for.
//!
//! Runs off the GUI thread so a slow or read-only home directory never
//! delays startup, and is a cheap no-op on every launch after the first:
//! it compares the recorded `Exec=` line against the current executable
//! path before writing anything.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::icon_render::render_icon_rgba;

pub const APP_ID: &str = "kicad-auto-importer";

/// Standard hicolor theme sizes; also covers what most taskbars/launchers
/// pick for HiDPI without upscaling a single small source image.
const ICON_SIZES: &[u32] = &[16, 32, 48, 64, 128, 256, 512];

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

    for &size in ICON_SIZES {
        write_icon(data_home, size)?;
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

fn write_icon(data_home: &Path, size: u32) -> Result<(), Box<dyn std::error::Error>> {
    let rgba = render_icon_rgba(size);
    let image = image::RgbaImage::from_raw(size, size, rgba)
        .expect("render_icon_rgba always returns exactly size*size*4 bytes");
    let dir = data_home
        .join("icons/hicolor")
        .join(format!("{size}x{size}"))
        .join("apps");
    fs::create_dir_all(&dir)?;
    image.save_with_format(dir.join(format!("{APP_ID}.png")), image::ImageFormat::Png)?;
    Ok(())
}

fn write_desktop_file(path: &Path, exec_line: &str) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(path.parent().expect("desktop_path always has a parent"))?;
    let mut file = fs::File::create(path)?;
    // Without an explicit `Path=`, the working directory a
    // desktop-launched process starts in is up to whichever launcher
    // ran it — some default to `/`, which the app then has no
    // permission to create anything under. `watch_folder`/library path
    // fields are meant to always hold absolute paths anyway, so this is
    // only ever a fallback, but it turns "silently inherits an
    // unwritable cwd" into "silently inherits the user's own home
    // directory" instead, which is always writable.
    let home_line = std::env::var_os("HOME")
        .map(|home| format!("Path={}\n", quote(Path::new(&home))))
        .unwrap_or_default();
    write!(
        file,
        "[Desktop Entry]\n\
         Type=Application\n\
         Version=1.0\n\
         Name=KiCad Auto Importer\n\
         Comment=Watches a folder for KiCad part-provider downloads and imports them into your project's libraries automatically\n\
         {exec_line}\n\
         {home_line}\
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

        for &size in ICON_SIZES {
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
