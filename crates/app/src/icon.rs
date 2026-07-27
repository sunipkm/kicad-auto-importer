//! The application's OS-level window/taskbar/dock icon — rendered from
//! the exact same circuitry glyph and colors as the custom title bar
//! (see `ui::title_bar`), so the icon is consistent everywhere instead
//! of falling back to whatever generic placeholder the OS picks when no
//! icon is set (e.g. a bare letter on macOS).
//!
//! There's no bundled image asset to load: the glyph comes from the
//! same Phosphor font `egui_phosphor` already ships, rasterized directly
//! via `ab_glyph` onto a solid tile matching the title bar's background.
//! [`render_icon_rgba`] takes a size so the same renderer also feeds
//! [`write_iconset`], which emits the PNGs macOS's `iconutil` needs to
//! build an `.icns` for the release `.app` bundle (triggered via the
//! `--emit-iconset` flag `main.rs` checks for before starting the GUI —
//! CI invokes it as a build step; end users never see it).

use std::path::Path;
use std::sync::{Arc, OnceLock};

use ab_glyph::{point, Font, FontRef};
use egui_phosphor::regular as icon_glyphs;

use crate::theme::{ACCENT, TITLE_BAR_BG};

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

/// Renders the app icon at an arbitrary square `size`, as unmultiplied
/// RGBA bytes (row-major, `size * size * 4` long).
pub fn render_icon_rgba(size: u32) -> Vec<u8> {
    let bg = TITLE_BAR_BG.to_srgba_unmultiplied();
    let fg = ACCENT.to_srgba_unmultiplied();

    let mut rgba = vec![0u8; (size * size * 4) as usize];
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.copy_from_slice(&bg);
    }

    draw_glyph_centered(&mut rgba, size, bg, fg);
    rgba
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

fn draw_glyph_centered(rgba: &mut [u8], size: u32, bg: [u8; 4], fg: [u8; 4]) {
    let Ok(font) = FontRef::try_from_slice(egui_phosphor::Variant::Regular.font_bytes()) else {
        return;
    };
    let ch = icon_glyphs::CIRCUITRY
        .chars()
        .next()
        .expect("icon glyph constants are always exactly one char");
    let glyph_id = font.glyph_id(ch);
    let scale = size as f32 * 0.62;

    // `outline_glyph` positions the outline relative to wherever the
    // glyph's origin was placed; probe once at the origin to learn its
    // natural size, then re-outline at the position that centers it.
    let Some(probe) = font.outline_glyph(glyph_id.with_scale_and_position(scale, point(0.0, 0.0)))
    else {
        return;
    };
    let natural = probe.px_bounds();
    let pos_x = (size as f32 - natural.width()) / 2.0 - natural.min.x;
    let pos_y = (size as f32 - natural.height()) / 2.0 - natural.min.y;

    let Some(outline) =
        font.outline_glyph(glyph_id.with_scale_and_position(scale, point(pos_x, pos_y)))
    else {
        return;
    };
    let bounds = outline.px_bounds();

    outline.draw(|x, y, coverage| {
        let px = bounds.min.x as i32 + x as i32;
        let py = bounds.min.y as i32 + y as i32;
        if px < 0 || py < 0 || px as u32 >= size || py as u32 >= size {
            return;
        }
        let idx = ((py as u32 * size + px as u32) * 4) as usize;
        for c in 0..3 {
            let blended = bg[c] as f32 + (fg[c] as f32 - bg[c] as f32) * coverage;
            rgba[idx + c] = blended.clamp(0.0, 255.0) as u8;
        }
        rgba[idx + 3] = 255;
    });
}
