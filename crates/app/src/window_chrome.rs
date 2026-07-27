//! Shared "custom window chrome" used by every undecorated window in
//! this app (the main window and the cherry-pick dialog): a title bar
//! with the app icon, a title, and minimize/maximize/close buttons, plus
//! a bottom-right resize grip. Both are needed because every window here
//! is created with `with_decorations(false)`, trading the OS's native
//! title bar for the app's own branded one — consistently, across every
//! window, rather than mixing native and custom chrome.

use egui::{Color32, RichText, Rounding};
use egui_phosphor::regular as icon;

use crate::theme::{ACCENT, DANGER};

pub const BAR_HEIGHT: f32 = 40.0;

/// Draws the title bar into the current `ui` — call this from inside a
/// `TopBottomPanel::top(...).exact_height(BAR_HEIGHT)`. `title` is the
/// text shown next to the app icon.
pub fn title_bar(ui: &mut egui::Ui, ctx: &egui::Context, title: &str) {
    ui.horizontal(|ui| {
        ui.set_height(BAR_HEIGHT);
        // Window-chrome rows are laid out with exact pixel math (drag
        // region width, flush-together buttons), so the automatic
        // inter-widget spacing egui would otherwise insert has to be
        // switched off — it would silently push the close button past
        // the edge of the window and clip it.
        ui.spacing_mut().item_spacing.x = 0.0;

        ui.add_space(10.0);
        ui.label(RichText::new(icon::CIRCUITRY).size(20.0).color(ACCENT));
        ui.add_space(6.0);
        ui.label(RichText::new(title).strong().size(15.0));

        // Window control buttons, drawn right-to-left so we can measure
        // how much space they take before laying out the drag region.
        let button_size = egui::vec2(BAR_HEIGHT, BAR_HEIGHT);
        let controls_width = button_size.x * 3.0;
        let drag_width = (ui.available_width() - controls_width).max(0.0);

        let (drag_rect, drag_resp) =
            ui.allocate_exact_size(egui::vec2(drag_width, BAR_HEIGHT), egui::Sense::click());
        let drag_resp = ui.interact(drag_rect, drag_resp.id, egui::Sense::drag());
        if drag_resp.drag_started() {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        if drag_resp.double_clicked() {
            let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
        }

        if title_bar_button(ui, icon::MINUS, button_size).clicked() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        let maximize_icon = if maximized {
            icon::CORNERS_IN
        } else {
            icon::SQUARE
        };
        if title_bar_button(ui, maximize_icon, button_size).clicked() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
        }
        if title_bar_button_colored(ui, icon::X, button_size, DANGER).clicked() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    });
}

fn title_bar_button(ui: &mut egui::Ui, glyph: &str, size: egui::Vec2) -> egui::Response {
    title_bar_button_colored(ui, glyph, size, Color32::from_gray(220))
}

fn title_bar_button_colored(
    ui: &mut egui::Ui,
    glyph: &str,
    size: egui::Vec2,
    hover_color: Color32,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let visuals = ui.style().interact(&resp);
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, Rounding::ZERO, visuals.bg_fill);
    }
    let color = if resp.hovered() {
        hover_color
    } else {
        Color32::from_gray(190)
    };
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        glyph,
        egui::FontId::proportional(15.0),
        color,
    );
    resp
}

/// A small drag handle in the bottom-right corner that lets the user
/// resize the window — necessary because `with_decorations(false)`
/// removes the OS-provided resize border along with the title bar.
/// `id_salt` must be unique per window this is called for, since
/// otherwise two windows' grips would share the same persisted widget
/// state.
pub fn resize_grip(ctx: &egui::Context, id_salt: &str) {
    egui::Area::new(egui::Id::new(("resize_grip", id_salt)))
        .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-2.0, -2.0))
        .interactable(true)
        .show(ctx, |ui| {
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::drag());
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                icon::DOTS_SIX,
                egui::FontId::proportional(14.0),
                Color32::from_gray(140),
            );
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeNwSe);
            }
            if resp.drag_started() {
                ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(
                    egui::ResizeDirection::SouthEast,
                ));
            }
        });
}
