//! The application's OS-level window/taskbar/dock icon — rendered from
//! the exact same circuitry glyph and colors as the custom title bar
//! (see `ui::title_bar`), so the icon is consistent everywhere instead
//! of falling back to whatever generic placeholder the OS picks when no
//! icon is set (e.g. a bare letter on macOS).
//!
//! There's no bundled image asset to load: the glyph comes from the
//! same Phosphor font `egui_phosphor` already ships, rasterized directly
//! via `ab_glyph` (see `icon_render`) onto a solid tile matching the
//! title bar's background. That renderer also feeds:
//! - [`write_iconset`], the PNGs macOS's `iconutil` needs to build an
//!   `.icns` for the release `.app` bundle, via the `--emit-iconset`
//!   flag `main.rs` checks for before starting the GUI (CI invokes it
//!   as a build step; end users never see it), and
//! - `build.rs`, which renders the same glyph again (independently, via
//!   `#[path]`, not by calling anything in this file — see there) to
//!   embed a `.ico` into the Windows `.exe` itself.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use crate::icon_render::{render_dmg_background_rgba, render_icon_rgba};

/// Size used for the window/taskbar/dock icon set via `with_icon(...)`.
const WINDOW_ICON_SIZE: u32 = 256;

/// Rasterized once (font loading + glyph outlining isn't free) and
/// cached for the process's lifetime; every caller gets a cheap clone of
/// the same `Arc`.
pub fn app_icon() -> Arc<egui::IconData> {
    static ICON: OnceLock<Arc<egui::IconData>> = OnceLock::new();
    ICON.get_or_init(|| {
        Arc::new(egui::IconData {
            rgba: render_icon_rgba(WINDOW_ICON_SIZE),
            width: WINDOW_ICON_SIZE,
            height: WINDOW_ICON_SIZE,
        })
    })
    .clone()
}

/// Every size + filename `iconutil -c icns` expects inside a `.iconset`
/// directory (the "@2x" entries are the Retina variants of the size
/// named just before them).
const ICONSET_ENTRIES: &[(&str, u32)] = &[
    ("icon_16x16.png", 16),
    ("icon_16x16@2x.png", 32),
    ("icon_32x32.png", 32),
    ("icon_32x32@2x.png", 64),
    ("icon_128x128.png", 128),
    ("icon_128x128@2x.png", 256),
    ("icon_256x256.png", 256),
    ("icon_256x256@2x.png", 512),
    ("icon_512x512.png", 512),
    ("icon_512x512@2x.png", 1024),
];

/// Writes every PNG a macOS `.iconset` needs into `dir` (created if
/// missing), ready for `iconutil -c icns <dir> -o AppIcon.icns`.
pub fn write_iconset(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(dir)?;
    for (name, size) in ICONSET_ENTRIES {
        let rgba = render_icon_rgba(*size);
        let image = image::RgbaImage::from_raw(*size, *size, rgba)
            .expect("render_icon_rgba always returns exactly size*size*4 bytes");
        image.save_with_format(dir.join(name), image::ImageFormat::Png)?;
    }
    Ok(())
}

/// Size used for a single standalone `.ico` (see [`write_ico`]) — large
/// enough to look sharp as the NSIS installer/uninstaller's icon.
const STANDALONE_ICO_SIZE: u32 = 256;

/// Writes a single `.ico` at `path`. Used for the Windows NSIS
/// installer/uninstaller icon (see `packaging/windows/installer.nsi`):
/// unlike `build.rs`'s embedded-in-the-`.exe` icon, `makensis` needs an
/// actual `.ico` file on disk to point `MUI_ICON`/`MUI_UNICON` at.
pub fn write_ico(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let rgba = render_icon_rgba(STANDALONE_ICO_SIZE);
    let image = image::RgbaImage::from_raw(STANDALONE_ICO_SIZE, STANDALONE_ICO_SIZE, rgba)
        .expect("render_icon_rgba always returns exactly size*size*4 bytes");
    image.save_with_format(path, image::ImageFormat::Ico)?;
    Ok(())
}

/// The macOS installer DMG's Finder window size — the CI packaging step
/// (`.github/workflows/test.yml`) passes the same numbers to
/// `create-dmg --window-size` and positions the `.app`/`Applications`
/// icons within it, so this and that have to stay in sync by hand.
pub const DMG_WINDOW_SIZE: (u32, u32) = (660, 400);

/// Writes the DMG Finder-window background (see
/// [`render_dmg_background_rgba`]) to `path` as a PNG.
pub fn write_dmg_background(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let (width, height) = DMG_WINDOW_SIZE;
    let rgba = render_dmg_background_rgba(width, height);
    let image = image::RgbaImage::from_raw(width, height, rgba)
        .expect("render_dmg_background_rgba always returns exactly width*height*4 bytes");
    image.save_with_format(path, image::ImageFormat::Png)?;
    Ok(())
}
