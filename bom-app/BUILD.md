# Building `kicad-bom`

See [README.md](README.md) for what the tool does and how it works.
This document covers installing, building, and developing it.

## Installation

Prebuilt binaries are published alongside `crates/app`'s own on the
[Releases page](https://github.com/sunipkm/kicad-auto-importer/releases/latest):
a bare binary in a `.tar.gz` on Linux, a `.dmg` on macOS, and — combined
with `kicad-auto-importer.exe` into a single installer — an NSIS `.exe`
on Windows. See [How CI packages it](#how-ci-packages-it) below for the
exact shape of each. Building from source needs the
[Prerequisites](#prerequisites) below plus a Rust toolchain (see the
root [`README.md`](../README.md)).

## Prerequisites

Building or running `kicad-bom` needs, in addition to the Rust toolchain
`crates/core`/`crates/app` already require:

- **Node.js** (18+) and **npm** — install dependencies once with
  `npm install` from this directory before the first `dev`/`build`.
- The same platform system libraries Tauri's own docs list for its
  [prerequisites](https://v2.tauri.app/start/prerequisites/). On
  Linux (Debian/Ubuntu):
  ```sh
  sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file \
    libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev
  ```

## Development

```sh
cd kicad-bom
npm install
npm run tauri dev
```

This starts the Vite dev server and opens the app in a native webview
with hot reload. Rust changes under `src-tauri/` trigger a rebuild;
frontend changes under `src/` hot-reload without one.

## Building an installer

```sh
cd kicad-bom
npm install
npm run tauri build
```

Produces a platform-native installer/bundle under
`src-tauri/target/release/bundle/` — an NSIS `.exe` on Windows, a
`.dmg`/`.app` on macOS, and a `.deb`/AppImage on Linux — per
`src-tauri/tauri.conf.json`'s `bundle` config. Handy for a local build,
but **not** what CI's release packaging actually runs — see below.

### How CI packages it

`.github/workflows/test.yml` builds `kicad-bom` as a bare `cargo build
--release -p kicad-bom` binary (after `npm run build` produces
`dist/`, which `tauri::generate_context!()` embeds at compile time) and
packages it by hand, the same way it packages `crates/app`'s own
`kicad-auto-importer` binary, rather than invoking `tauri build`'s own
bundler:

- **Linux** — a bare binary in a `.tar.gz`, not a `.deb`/AppImage.
  `kicad-bom` self-registers its own `.desktop` launcher entry and hicolor
  icons in the user's XDG data directories on first run
  (`src/linux_desktop_integration.rs`) — the exact same mechanism
  `crates/app` uses (`crates/app/src/linux_desktop_integration.rs`) and
  for the same reason: shipping a bare binary means there's no
  packaging step to install a desktop file for us.
- **macOS** — a hand-built `KiCad BOM Tool.app` (ad-hoc signed, same as
  `crates/app`'s own `.app`) wrapped in its own `.dmg` via `create-dmg`.
- **Windows** — `kicad-bom.exe` and `kicad-auto-importer.exe` are
  installed together by **one** NSIS installer
  ([`packaging/windows/installer.nsi`](../packaging/windows/installer.nsi)),
  not two separate ones — see that script's own header comment for why.

The two apps still ship as independent binaries everywhere (this is
about how they're *packaged/installed*, not a merge of the two apps).

## Source layout

- `src-tauri/src/lib.rs` — every `#[tauri::command]` the frontend
  calls: project/schematic listing, the Populate BOM batch
  (`populate_bom`), the vendor result picker
  (`get_scored_candidates`/`apply_vendor_choice`), the Generate BOM
  batch (`generate_bom`), the credentials bridge
  (`load_vendor_credentials`/`save_vendor_credentials`), and the credential
  test buttons (`test_mouser_credentials`/`test_digikey_credentials`).
  Schematic/symbol-library parsing is backed by `kicad_parse`;
  vendor/pricing logic is kicad-bom's own.
- `src-tauri/src/populate_bom.rs` / `generate_bom.rs` — the two batch
  orchestrations, UI-agnostic (shared with the egui desktop app for
  `populate_bom`'s lookup logic) and driven from `lib.rs` on their own
  OS thread, emitting progress events back to the frontend.
- `src-tauri/src/bom_pricing.rs` — groups placed symbols into unique
  purchasable parts and prices them for a board quantity.
- `src-tauri/src/bom_report.rs` — PDF/XLSX report generation.
- `src-tauri/src/parts_lookup.rs`, `mouser.rs`, `digikey.rs`,
  `parts_cache.rs` — vendor API clients, candidate scoring, and the
  local raw-result cache.
- `src/App.tsx` — project picker + top-level layout.
- `src/SettingsPanel.tsx` — Mouser/DigiKey credential fields and their
  "Test" buttons.
- `src/PopulateBom.tsx` — the placed-symbol table, selection, and
  batch lookup UI.
- `src/VendorDropdown.tsx` — the "which of these matches my part"
  dropdown, anchored under each row's own trigger button in the
  Populate BOM table, ranking every candidate best-first
  (`get_scored_candidates`) so the automatic pick is usually obvious at
  a glance, with a manual override.
- `src/GenerateBom.tsx` — board quantity/margin inputs, the grouped
  parts table, and the priced batch run (PDF + XLSX export).
- `src/XlsxColumnsPanel.tsx` — checkbox + reorder UI for configuring
  which columns appear in the XLSX export (persisted to
  `~/.config/kicad-bom/xlsx_columns.json`).
