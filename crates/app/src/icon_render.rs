//! The pure "render the app icon glyph to an RGBA buffer" logic — no
//! `egui` dependency, deliberately, so this same file can be pulled into
//! `build.rs` via `#[path]` (see there) to render the icon `.ico`
//! embedded into the Windows `.exe`, without needing `egui`/`eframe` as
//! build-dependencies just for that.

use ab_glyph::{point, Font, FontRef};
use egui_phosphor::regular as icon_glyphs;

use crate::icon_colors::{ACCENT_RGB, TITLE_BAR_BG_RGB};

/// Renders the app icon at an arbitrary square `size`, as unmultiplied
/// RGBA bytes (row-major, `size * size * 4` long).
pub fn render_icon_rgba(size: u32) -> Vec<u8> {
    let bg = [
        TITLE_BAR_BG_RGB[0],
        TITLE_BAR_BG_RGB[1],
        TITLE_BAR_BG_RGB[2],
        255,
    ];
    let fg = [ACCENT_RGB[0], ACCENT_RGB[1], ACCENT_RGB[2], 255];

    let mut rgba = vec![0u8; (size * size * 4) as usize];
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.copy_from_slice(&bg);
    }

    draw_glyph_centered(&mut rgba, size, bg, fg);
    rgba
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
