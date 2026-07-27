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
//! themselves, though, *are* held onto directly (see `build_menu`/
//! `set_state`) so the single start/stop toggle item's label and
//! enabled state can be kept in sync with app state — matched by id
//! only applies to *events coming from* the menu, not to updating it.
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
//!
//! That same split is why keeping the toggle item's label in sync needs
//! two different mechanisms (see `set_state`): on Windows/macOS the
//! `MenuItem` lives on the same thread as `MainApp`, so it can just be
//! mutated directly every frame. On Linux the `MenuItem` lives on the
//! GTK thread, and `muda`'s item types wrap `Rc<RefCell<..>>`
//! internally (see its `MenuItem` definition) — not `Send`, so it can't
//! be hopped over to the eframe thread or stashed in a shared static.
//! What *can* cross threads is a couple of plain `AtomicBool`s, which
//! `MainApp` writes every frame and a small periodic GTK-thread timer
//! (started in `spawn`) reads and applies to the real, locally-owned
//! `MenuItem`.

#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, Ordering};

use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
#[cfg(not(target_os = "linux"))]
use tray_icon::TrayIcon;
use tray_icon::{Icon, TrayIconBuilder};

use crate::icon_render::render_icon_rgba;

pub const MENU_SHOW: &str = "show";
pub const MENU_TOGGLE: &str = "toggle_watch";
pub const MENU_QUIT: &str = "quit";

const TRAY_ICON_SIZE: u32 = 32;

/// Cross-thread-safe stand-in for "is the watcher running" / "would
/// clicking Start actually do anything right now" — see the module
/// docs' explanation of why the Linux GTK thread can't just be handed
/// the real state directly. Both default to `false`, matching
/// `MainApp`'s own startup state (`watcher: None`, no project chosen
/// yet), so the tray's very first render (before `MainApp::update` has
/// run even once) shows "Start Watching", disabled, rather than
/// something misleadingly enabled.
#[cfg(target_os = "linux")]
static WATCHING: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "linux")]
static CAN_START: AtomicBool = AtomicBool::new(false);

/// Called from `MainApp::update` every frame with the current watching
/// state and whether `start_watching` would currently succeed. On
/// Linux this only updates the two atomics above; `spawn`'s periodic
/// GTK-thread poll picks up the change and applies it to the real
/// `MenuItem`. On Windows/macOS `MainApp` mutates its own directly-held
/// `MenuItem` instead (same thread, no relay needed), so this is a
/// harmless no-op there.
pub fn set_state(watching: bool, can_start: bool) {
    #[cfg(target_os = "linux")]
    {
        WATCHING.store(watching, Ordering::Relaxed);
        CAN_START.store(can_start, Ordering::Relaxed);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (watching, can_start);
    }
}

/// What the toggle item should say and whether it should be clickable,
/// given the same two booleans `set_state` takes. Shared by both the
/// Linux poll (`spawn`) and non-Linux direct application (`MainApp`)
/// so the two platforms can't drift into showing different labels for
/// the same state.
pub fn toggle_label_and_enabled(watching: bool, can_start: bool) -> (&'static str, bool) {
    if watching {
        ("Stop Watching", true)
    } else {
        ("Start Watching", can_start)
    }
}

/// Applies `toggle_label_and_enabled`'s result to an actual `MenuItem`.
/// Used by `spawn`'s GTK-thread poll on Linux; on Windows/macOS
/// `MainApp` calls this directly instead (see `set_state`'s docs), on
/// the item it holds in its own `tray_toggle_item` field.
pub fn apply_toggle_state(item: &MenuItem, watching: bool, can_start: bool) {
    let (label, enabled) = toggle_label_and_enabled(watching, can_start);
    item.set_text(label);
    item.set_enabled(enabled);
}

fn build_icon() -> Icon {
    let rgba = render_icon_rgba(TRAY_ICON_SIZE);
    Icon::from_rgba(rgba, TRAY_ICON_SIZE, TRAY_ICON_SIZE)
        .expect("render_icon_rgba always returns exactly TRAY_ICON_SIZE*TRAY_ICON_SIZE*4 bytes")
}

/// Builds the menu and returns the toggle item's own handle alongside
/// it — appending a `&MenuItem` into a `Menu` clones its underlying
/// `Rc`, so this handle and the one now live inside `menu` refer to the
/// same shared item; mutating this handle later (`apply_toggle_state`)
/// changes what the menu displays.
fn build_menu() -> (Menu, MenuItem) {
    let toggle_item = MenuItem::with_id(MENU_TOGGLE, "Start Watching", false, None);
    let menu = Menu::new();
    let _ = menu.append_items(&[
        &MenuItem::with_id(MENU_SHOW, "Show Window", true, None),
        &PredefinedMenuItem::separator(),
        &toggle_item,
        &PredefinedMenuItem::separator(),
        &MenuItem::with_id(MENU_QUIT, "Quit", true, None),
    ]);
    (menu, toggle_item)
}

fn builder(menu: Menu) -> TrayIconBuilder {
    TrayIconBuilder::new()
        .with_menu(Box::new(menu))
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
            let (menu, toggle_item) = build_menu();
            match builder(menu).build() {
                Ok(tray_icon) => {
                    // Never read again, but must outlive `gtk::main()` or
                    // the tray icon would be torn down immediately.
                    let _tray_icon = tray_icon;

                    let mut last_watching = WATCHING.load(Ordering::Relaxed);
                    let mut last_can_start = CAN_START.load(Ordering::Relaxed);
                    apply_toggle_state(&toggle_item, last_watching, last_can_start);

                    // 200ms is plenty responsive for a menu label nobody
                    // is staring at continuously, and cheap enough to run
                    // for the process's whole lifetime.
                    gtk::glib::timeout_add_local(
                        std::time::Duration::from_millis(200),
                        move || {
                            let watching = WATCHING.load(Ordering::Relaxed);
                            let can_start = CAN_START.load(Ordering::Relaxed);
                            if watching != last_watching || can_start != last_can_start {
                                apply_toggle_state(&toggle_item, watching, can_start);
                                last_watching = watching;
                                last_can_start = can_start;
                            }
                            gtk::glib::ControlFlow::Continue
                        },
                    );

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
/// icon should stay visible, and keeps the `MenuItem` around to mutate
/// directly as watching state changes — `MainApp` stores both in fields
/// for exactly that reason.
#[cfg(not(target_os = "linux"))]
pub fn build() -> Option<(TrayIcon, MenuItem)> {
    let (menu, toggle_item) = build_menu();
    match builder(menu).build() {
        Ok(tray_icon) => Some((tray_icon, toggle_item)),
        Err(err) => {
            eprintln!("tray icon: failed to build, no tray icon this run: {err}");
            None
        }
    }
}
