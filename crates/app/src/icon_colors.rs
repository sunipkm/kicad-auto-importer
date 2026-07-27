//! Raw RGB triples for the icon glyph and its background tile — kept
//! separate from `theme.rs`'s `egui::Color32` constants specifically so
//! `build.rs` can render the Windows `.ico` it embeds into the `.exe`
//! without depending on `egui` at build-script time. Must be kept in
//! sync with `theme::ACCENT` / `theme::TITLE_BAR_BG` by hand — there are
//! only the two colors, so that's a small price for not pulling `egui`
//! into the build-dependency graph.

pub const ACCENT_RGB: [u8; 3] = [0x4f, 0x9d, 0xff];
pub const TITLE_BAR_BG_RGB: [u8; 3] = [0x1b, 0x1e, 0x24];
