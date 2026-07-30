# KiCad BOM Tool (`bom-app`)

A standalone companion to
[`kicad-auto-importer`](https://github.com/sunipkm/kicad-auto-importer) for
priced, multi-vendor bills of materials: **Populate BOM** (look up
manufacturer/distributor data for every symbol placed on a project's
schematic and write it back onto each one) and **Generate BOM** (group
identical parts, price them out for a given board quantity against the
cheapest available Mouser/DigiKey quantity break, and export a priced
PDF/XLSX report).

It is a [Tauri](https://tauri.app/) app: a Rust backend
(`src-tauri/`) plus a React/TypeScript frontend (`src/`). Schematic
parsing and symbol-library patching reuse the sibling
[`crates/core`](../crates/core) crate (shared with the main
`kicad-auto-importer` desktop app, [`crates/app`](../crates/app/README.md)),
while the Mouser/DigiKey API clients, grouping/pricing math, and
PDF/XLSX generation are this app's own exclusive logic. See the root
[`README.md`](../README.md#workspace-layout) for how the three pieces
(`crates/core`, `crates/app`, `bom-app`) fit together.

`bom-app` shares its Mouser/DigiKey API credentials with `crates/app`:
both read/write the same `settings.json` in the platform's standard
config directory (`~/.config/kicad-auto-importer` on Linux,
`~/Library/Application Support/kicad-auto-importer` on macOS,
`%APPDATA%\kicad-auto-importer` on Windows) — credentials entered in
one show up in the other immediately, no import/export step needed.

The same directory also holds `parts_cache.json` — a local, global
cache of every raw Mouser/DigiKey search result Populate/Generate BOM
have fetched (keyed by manufacturer part number, not by project), used
to pick a vendor+part automatically (cheapest offer that can actually
cover the needed quantity — see `parts_lookup::score_candidates`) and
to skip a live API call entirely when a cached entry is still fresh
(24h). It's plain JSON and safe to delete any time — everything in it
gets refetched on demand.

Each vendor's field in the "API Settings" popover (the gear icon, top
right) has its own "Test" button that verifies the entered key/
credentials against the live API without saving anything — the same
check `crates/app`'s own "API Settings" popup offers.

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

Building or running `bom-app` needs, in addition to the Rust toolchain
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
cd bom-app
npm install
npm run tauri dev
```

This starts the Vite dev server and opens the app in a native webview
with hot reload. Rust changes under `src-tauri/` trigger a rebuild;
frontend changes under `src/` hot-reload without one.

## Building an installer

```sh
cd bom-app
npm install
npm run tauri build
```

Produces a platform-native installer/bundle under
`src-tauri/target/release/bundle/` — an NSIS `.exe` on Windows, a
`.dmg`/`.app` on macOS, and a `.deb`/AppImage on Linux — per
`src-tauri/tauri.conf.json`'s `bundle` config. Handy for a local build,
but **not** what CI's release packaging actually runs — see below.

### How CI packages it

`.github/workflows/test.yml` builds `bom-app` as a bare `cargo build
--release -p bom-app` binary (after `npm run build` produces
`dist/`, which `tauri::generate_context!()` embeds at compile time) and
packages it by hand, the same way it packages `crates/app`'s own
`kicad-auto-importer` binary, rather than invoking `tauri build`'s own
bundler:

- **Linux** — a bare binary in a `.tar.gz`, not a `.deb`/AppImage.
  `bom-app` self-registers its own `.desktop` launcher entry and hicolor
  icons in the user's XDG data directories on first run
  (`src/linux_desktop_integration.rs`) — the exact same mechanism
  `crates/app` uses (`crates/app/src/linux_desktop_integration.rs`) and
  for the same reason: shipping a bare binary means there's no
  packaging step to install a desktop file for us.
- **macOS** — a hand-built `KiCad BOM Tool.app` (ad-hoc signed, same as
  `crates/app`'s own `.app`) wrapped in its own `.dmg` via `create-dmg`.
- **Windows** — `bom-app.exe` and `kicad-auto-importer.exe` are
  installed together by **one** NSIS installer
  ([`packaging/windows/installer.nsi`](../packaging/windows/installer.nsi)),
  not two separate ones — see that script's own header comment for why.

The two apps still ship as independent binaries everywhere (this is
about how they're *packaged/installed*, not a merge of the two apps).

## Layout

- `src-tauri/src/lib.rs` — every `#[tauri::command]` the frontend
  calls: project/schematic listing, the Populate BOM batch
  (`populate_bom`), the vendor result picker
  (`get_scored_candidates`/`apply_vendor_choice`), the Generate BOM
  batch (`generate_bom`), the credentials bridge
  (`load_vendor_credentials`/`save_vendor_credentials`), and the credential
  test buttons (`test_mouser_credentials`/`test_digikey_credentials`).
  Schematic/symbol-library parsing is backed by `kicad_parse`;
  vendor/pricing logic is bom-app's own.
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
  `~/.config/bom-app/xlsx_columns.json`).

## Third-party assets

The following files in `src-tauri/assets/interactive_bom/` are
reproduced verbatim from
[openscopeproject/InteractiveHtmlBom](https://github.com/openscopeproject/InteractiveHtmlBom)
(MIT licence, © openscopeproject contributors):
`ibom.html`, `ibom.css`, `ibom.js`, `util.js`, `render.js`,
`table-util.js`, `split.js`, `pep.js`, `lz-string.js`.
