//! System tray icon and menu, Telegram-Desktop-style: the window can be
//! closed to the tray and recovered from it (see `ui::MainApp::update`'s
//! close-request interception and `wake_rx`/`single_instance` handling),
//! and watching can be toggled without the window even being open.
//!
//! Menu clicks and tray icon clicks are *not* funneled through a channel
//! we own — `tray-icon` exposes global static receivers
//! (`TrayIconEvent::receiver()`, `menu::MenuEvent::receiver()`) that
//! `MainApp::update` polls directly every frame, the same `try_recv`
//! pattern already used there for the watcher's log channel. Menu items
//! are matched purely by the string ids below rather than by holding on
//! to the `MenuItem` objects themselves: `muda`'s item types wrap
//! `Rc<RefCell<..>>` internally (see its `MenuItem` definition), so
//! they're not `Send` and can't be handed from the Linux tray thread
//! (below) back to the eframe thread anyway.
//!
//! Platform split, straight from `tray-icon`'s own official `eframe`
//! example (`tauri-apps/tray-icon/examples/egui.rs`): egui/eframe uses
//! winit, which on Linux drives an X11/Wayland event loop, not GTK's
//! GLib main loop that `tray-icon` needs there — so the tray icon has to
//! live on its own dedicated thread that runs `gtk::main()`. Windows and
//! macOS don't have that split (their native tray implementations pump
//! through the same event loop winit already runs), so there the icon
//! is built inside the `eframe` app-creator closure, once winit's event
//! loop has actually started (per `tray-icon`'s own docs, creating it
//! any earlier is unreliable).

use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
#[cfg(not(target_os = "linux"))]
use tray_icon::TrayIcon;
use tray_icon::{Icon, TrayIconBuilder};

use crate::icon_render::render_icon_rgba;

pub const MENU_SHOW: &str = "show";
pub const MENU_START: &str = "start_watch";
pub const MENU_STOP: &str = "stop_watch";
pub const MENU_QUIT: &str = "quit";

const TRAY_ICON_SIZE: u32 = 32;

fn build_icon() -> Icon {
    let rgba = render_icon_rgba(TRAY_ICON_SIZE);
    Icon::from_rgba(rgba, TRAY_ICON_SIZE, TRAY_ICON_SIZE)
        .expect("render_icon_rgba always returns exactly TRAY_ICON_SIZE*TRAY_ICON_SIZE*4 bytes")
}

fn build_menu() -> Menu {
    let menu = Menu::new();
    let _ = menu.append_items(&[
        &MenuItem::with_id(MENU_SHOW, "Show Window", true, None),
        &PredefinedMenuItem::separator(),
        &MenuItem::with_id(MENU_START, "Start Watching", true, None),
        &MenuItem::with_id(MENU_STOP, "Stop Watching", true, None),
        &PredefinedMenuItem::separator(),
        &MenuItem::with_id(MENU_QUIT, "Quit", true, None),
    ]);
    menu
}

fn builder() -> TrayIconBuilder {
    TrayIconBuilder::new()
        .with_menu(Box::new(build_menu()))
        .with_icon(build_icon())
        .with_tooltip("KiCad Auto Importer")
        // Frees up left-click to reach `TrayIconEvent::Click` (handled in
        // `ui::MainApp::update` as "show the window") instead of it being
        // consumed to open the menu, which right-click still does (its
        // default is left as-is). Unsupported on Linux — clicking the
        // indicator there always opens the menu, so "Show Window" is the
        // only way to restore the window on that platform.
        .with_menu_on_left_click(false)
}

/// Linux only: spawns the dedicated GTK thread described above. Detached
/// deliberately — it runs for the rest of the process's life and is torn
/// down by process exit, same as the official example.
#[cfg(target_os = "linux")]
pub fn spawn() {
    std::thread::Builder::new()
        .name("tray-gtk".into())
        .spawn(|| {
            if let Err(err) = gtk::init() {
                eprintln!("tray icon: gtk::init failed, no tray icon this run: {err}");
                return;
            }
            match builder().build() {
                Ok(tray_icon) => {
                    // Never read again, but must outlive `gtk::main()` or
                    // the tray icon would be torn down immediately.
                    let _tray_icon = tray_icon;
                    gtk::main();
                }
                Err(err) => eprintln!("tray icon: failed to build, no tray icon this run: {err}"),
            }
        })
        .expect("failed to spawn the tray-gtk thread");
}

/// Windows/macOS only: must be called from the `eframe` app-creator
/// closure (main thread, after winit's event loop has started). The
/// caller keeps the returned `TrayIcon` alive for as long as the tray
/// icon should stay visible — `MainApp` stores it in a field for exactly
/// that reason.
#[cfg(not(target_os = "linux"))]
pub fn build() -> Option<TrayIcon> {
    match builder().build() {
        Ok(tray_icon) => Some(tray_icon),
        Err(err) => {
            eprintln!("tray icon: failed to build, no tray icon this run: {err}");
            None
        }
    }
}
