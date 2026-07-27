//! Embeds the app icon into the Windows `.exe`'s own PE resources.
//!
//! `eframe`'s runtime `with_icon(...)` (see `src/icon.rs`) only sets the
//! icon Windows shows for the *running window* — it does nothing for
//! the icon Explorer, the taskbar (before launch), shortcuts, or
//! Alt+Tab-before-the-window-exists show for the `.exe` *file* itself.
//! That has to be a resource baked into the binary at link time, which
//! is what this build script does via `winresource`.
//!
//! The `.ico` it embeds is rendered from scratch here rather than
//! loaded from a checked-in asset, reusing `src/icon_render.rs` (pulled
//! in via `#[path]`, not a normal `mod` declaration, since a build
//! script is its own separate compilation with its own dependency
//! graph — see `[target.'cfg(windows)'.build-dependencies]` in
//! `Cargo.toml`). Everything here is gated on `target_os = "windows"`
//! because those build-dependencies (and `winresource` itself) are only
//! ever pulled in for that target; referencing them unconditionally
//! would fail to compile this script at all on macOS/Linux.

#[cfg(target_os = "windows")]
#[path = "src/icon_colors.rs"]
mod icon_colors;
#[cfg(target_os = "windows")]
#[path = "src/icon_render.rs"]
mod icon_render;

#[cfg(target_os = "windows")]
fn main() {
    const SIZE: u32 = 256;

    let rgba = icon_render::render_icon_rgba(SIZE);
    let image = image::RgbaImage::from_raw(SIZE, SIZE, rgba)
        .expect("render_icon_rgba always returns exactly SIZE*SIZE*4 bytes");

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR is always set for build scripts");
    let ico_path = std::path::Path::new(&out_dir).join("app_icon.ico");
    image
        .save_with_format(&ico_path, image::ImageFormat::Ico)
        .expect("failed to encode the app icon as .ico");

    let mut res = winresource::WindowsResource::new();
    res.set_icon(
        ico_path
            .to_str()
            .expect("OUT_DIR is always valid UTF-8 in practice"),
    );
    res.compile()
        .expect("failed to embed the Windows icon resource");
}

#[cfg(not(target_os = "windows"))]
fn main() {}
