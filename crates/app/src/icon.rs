//! The application's OS-level window/taskbar/dock icon — rendered from
//! the exact same circuitry glyph and colors as the custom title bar
//! (see `ui::title_bar`), so the icon is consistent everywhere instead
//! of falling back to whatever generic placeholder the OS picks when no
//! icon is set (e.g. a bare letter on macOS).
//!
//! There's no bundled image asset to load: the glyph comes from the
//! same Phosphor font `egui_phosphor` already ships, rasterized directly
//! via `ab_glyph` onto a solid tile matching the title bar's background.

use std::sync::{Arc, OnceLock};

use ab_glyph::{point, Font, FontRef};
use egui_phosphor::regular as icon_glyphs;

use crate::theme::{ACCENT, TITLE_BAR_BG};

const SIZE: u32 = 256;

/// Rasterized once (font loading + glyph outlining isn't free) and
/// cached for the process's lifetime; every caller gets a cheap clone of
/// the same `Arc`.
pub fn app_icon() -> Arc<egui::IconData> {
    static ICON: OnceLock<Arc<egui::IconData>> = OnceLock::new();
    ICON.get_or_init(|| Arc::new(render_icon())).clone()
}

fn render_icon() -> egui::IconData {
    let bg = TITLE_BAR_BG.to_srgba_unmultiplied();
    let fg = ACCENT.to_srgba_unmultiplied();

    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.copy_from_slice(&bg);
    }

    draw_glyph_centered(&mut rgba, bg, fg);

    egui::IconData {
        rgba,
        width: SIZE,
        height: SIZE,
    }
}

fn draw_glyph_centered(rgba: &mut [u8], bg: [u8; 4], fg: [u8; 4]) {
    let Ok(font) = FontRef::try_from_slice(egui_phosphor::Variant::Regular.font_bytes()) else {
        return;
    };
    let ch = icon_glyphs::CIRCUITRY
        .chars()
        .next()
        .expect("icon glyph constants are always exactly one char");
    let glyph_id = font.glyph_id(ch);
    let scale = SIZE as f32 * 0.62;

    // `outline_glyph` positions the outline relative to wherever the
    // glyph's origin was placed; probe once at the origin to learn its
    // natural size, then re-outline at the position that centers it.
    let Some(probe) = font.outline_glyph(glyph_id.with_scale_and_position(scale, point(0.0, 0.0)))
    else {
        return;
    };
    let natural = probe.px_bounds();
    let pos_x = (SIZE as f32 - natural.width()) / 2.0 - natural.min.x;
    let pos_y = (SIZE as f32 - natural.height()) / 2.0 - natural.min.y;

    let Some(outline) =
        font.outline_glyph(glyph_id.with_scale_and_position(scale, point(pos_x, pos_y)))
    else {
        return;
    };
    let bounds = outline.px_bounds();

    outline.draw(|x, y, coverage| {
        let px = bounds.min.x as i32 + x as i32;
        let py = bounds.min.y as i32 + y as i32;
        if px < 0 || py < 0 || px as u32 >= SIZE || py as u32 >= SIZE {
            return;
        }
        let idx = ((py as u32 * SIZE + px as u32) * 4) as usize;
        for c in 0..3 {
            let blended = bg[c] as f32 + (fg[c] as f32 - bg[c] as f32) * coverage;
            rgba[idx + c] = blended.clamp(0.0, 255.0) as u8;
        }
        rgba[idx + 3] = 255;
    });
}
