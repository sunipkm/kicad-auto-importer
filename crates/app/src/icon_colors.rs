//! Raw RGB triples for the icon glyph and its background tile — kept
//! separate from `theme.rs`'s `egui::Color32` constants (a) so
//! `build.rs` can render the Windows `.ico` it embeds into the `.exe`
//! without depending on `egui` at build-script time, and (b) because the
//! icon's glyph color is now *deliberately* different from the app's own
//! UI accent color (`theme::ACCENT`, still blue): it's shifted toward
//! the teal/turquoise most people recognize as "KiCad-ish" — a nod to
//! KiCad's own branding via palette only, not a reproduction of KiCad's
//! actual (trademarked) logo artwork, which this tool has no rights or
//! affiliation to use. `TITLE_BAR_BG_RGB` still matches
//! `theme::TITLE_BAR_BG` and should stay in sync with it by hand.

/// KiCad-esque teal — approximate homage, not a verified brand-color match.
pub const ACCENT_RGB: [u8; 3] = [0x1b, 0xb6, 0xa6];
pub const TITLE_BAR_BG_RGB: [u8; 3] = [0x1b, 0x1e, 0x24];
