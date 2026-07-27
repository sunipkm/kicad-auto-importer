//! Visual theme: icon font setup and a slightly larger, rounder egui
//! style than the defaults, so buttons and text are comfortable to hit
//! on a HiDPI display.

use egui::{Color32, FontFamily, FontId, Rounding, Stroke, TextStyle};

pub const ACCENT: Color32 = Color32::from_rgb(0x4f, 0x9d, 0xff);
pub const ACCENT_DIM: Color32 = Color32::from_rgb(0x2b, 0x54, 0x8a);
pub const TITLE_BAR_BG: Color32 = Color32::from_rgb(0x1b, 0x1e, 0x24);
pub const DANGER: Color32 = Color32::from_rgb(0xe0, 0x5a, 0x5a);
pub const OK: Color32 = Color32::from_rgb(0x4c, 0xd1, 0x7a);

pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);

    ctx.set_visuals(visuals());
    ctx.style_mut(|style| {
        style.spacing.button_padding = egui::vec2(14.0, 10.0);
        style.spacing.item_spacing = egui::vec2(10.0, 10.0);
        style.spacing.interact_size.y = 30.0;
        style.spacing.icon_spacing = 8.0;

        style.text_styles.insert(
            TextStyle::Heading,
            FontId::new(21.0, FontFamily::Proportional),
        );
        style
            .text_styles
            .insert(TextStyle::Body, FontId::new(14.5, FontFamily::Proportional));
        style.text_styles.insert(
            TextStyle::Button,
            FontId::new(15.0, FontFamily::Proportional),
        );
        style.text_styles.insert(
            TextStyle::Small,
            FontId::new(12.0, FontFamily::Proportional),
        );
        style.text_styles.insert(
            TextStyle::Monospace,
            FontId::new(13.5, FontFamily::Monospace),
        );
    });
}

fn visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    v.override_text_color = None;
    v.widgets.noninteractive.rounding = Rounding::same(8.0);
    v.widgets.inactive.rounding = Rounding::same(8.0);
    v.widgets.hovered.rounding = Rounding::same(8.0);
    v.widgets.active.rounding = Rounding::same(8.0);
    v.widgets.open.rounding = Rounding::same(8.0);
    v.window_rounding = Rounding::same(10.0);
    v.menu_rounding = Rounding::same(8.0);

    v.selection.bg_fill = ACCENT_DIM;
    v.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    v.hyperlink_color = ACCENT;

    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    v.widgets.active.bg_stroke = Stroke::new(1.0_f32, ACCENT);

    v
}

/// A filled, rounded "primary" button style (accent-colored) for the
/// single most important action in a row.
pub fn accent_button(text: impl Into<egui::WidgetText>) -> egui::Button<'static> {
    egui::Button::new(text.into())
        .fill(ACCENT_DIM)
        .stroke(Stroke::new(1.0_f32, ACCENT))
        .rounding(Rounding::same(8.0))
}
