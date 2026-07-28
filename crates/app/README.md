# kicad-auto-importer (desktop app)

<!--[![test](https://github.com/sunipkm/kicad-auto-importer/actions/workflows/test.yml/badge.svg)](https://github.com/sunipkm/kicad-auto-importer/actions/workflows/test.yml)-->
[![release](https://github.com/sunipkm/kicad-auto-importer/actions/workflows/release.yml/badge.svg)](https://github.com/sunipkm/kicad-auto-importer/actions/workflows/release.yml)
[![Latest release](https://img.shields.io/github/v/release/sunipkm/kicad-auto-importer)](https://github.com/sunipkm/kicad-auto-importer/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)

A standalone companion application for KiCad part libraries. It watches
a folder (e.g. a Downloads folder) for part-provider ZIP downloads
(UltraLibrarian / Mouser / DigiKey), extracts them, and imports the
symbol / footprint / 3-D model files into a KiCad project's libraries —
registering the destination symbol and footprint libraries in that
project's `sym-lib-table` / `fp-lib-table`, so KiCad picks them up
automatically.

It also supports loading the project-specific symbols from another
KiCad project and copying them over (with footprints and 3-D models)
into the working project's own libraries.

This tool does not depend on KiCad — it is a single native binary with
its own GUI, meant to run alongside KiCad or remain running in the
background to import new parts as they arrive in the configured watch
folder.

Populating a schematic's bill of materials with manufacturer/distributor
data and generating a priced BOM report is a separate application —
see [`bom-app`](../../bom-app/README.md) — sharing this app's
Mouser/DigiKey API credentials via the same `settings.json` (see
[Configuration](#configuration)).

This is one of two applications in the `kicad-auto-importer` repository
— see the [root README](../../README.md) for how the pieces fit
together.

## Features

- Watches a folder and imports part-provider ZIPs as they arrive, or
  imports a single ZIP / folder on demand.
- Registers imported symbol and footprint libraries in the target
  project's `sym-lib-table` / `fp-lib-table` automatically.
- Copies referenced 3-D models into the project and rewrites model
  paths to a `${KIPRJMOD}`-relative URI.
- Optional move-after-import and timestamped backups of the source ZIP.
- Imports symbols and footprints from the project-specific libraries of
  other KiCad projects.
- Single self-contained binary — does not require KiCad or Python (Linux
  needs GTK3 + libayatana-appindicator at runtime for the tray icon; see
  below).
- Closes to a system tray icon instead of quitting — the window can be
  restored from there, or watching can be started/stopped without
  opening it at all. Only one instance ever runs; launching it again
  simply brings the existing window to the front.

## API Settings — Mouser and DigiKey registration

Manufacturer and distributor lookups (used by
[`bom-app`](../../bom-app/README.md)) are performed through the Mouser
Search API and the DigiKey Product Information API. Credentials for
either or both vendors can be entered via the "API Settings" popup in
this app's main window, or from `bom-app` itself — both read/write the
same `settings.json` (see [Configuration](#configuration) below), so
credentials entered in one are immediately visible in the other.

- **Mouser** requires an API key, obtained by registering for API
  access at [mouser.com/api-search](https://www.mouser.com/api-search/).
- **DigiKey** requires a Client ID and Client Secret from an OAuth2
  client-credentials application, obtained at
  [developer.digikey.com](https://developer.digikey.com/).

Each vendor's fields in this app's "API Settings" popup include a
"Test" button that verifies the entered credentials against the live
API and reports whether they were accepted. These credentials are
account-level, not project-level, and are stored separately from any
single project's own settings.

## Installation

### Download a release

Prebuilt binaries for Linux, macOS (Apple Silicon), and Windows are
published on the
[Releases page](https://github.com/sunipkm/kicad-auto-importer/releases/latest):

| Platform             | Artifact                                         | Contains |
| -------------------- | ------------------------------------------------- | -------- |
| Windows (x86_64)     | `kicad-auto-importer-x86_64-pc-windows-msvc.zip`   | An NSIS installer (`KiCadAutoImporter-Setup-*.exe`) — per-user install, no admin/UAC prompt, Start Menu shortcut, and a proper uninstaller. |
| Linux (x86_64)       | `kicad-auto-importer-x86_64-unknown-linux-gnu.tar.gz` | The bare binary. |
| macOS (Apple Silicon) | `kicad-auto-importer-aarch64-apple-darwin.dmg` | A `.dmg` — the usual drag-`KiCad Auto Importer.app`-onto-Applications installer. |

On Windows, running the installer contained in the zip completes setup.
On macOS, opening the `.dmg` and dragging the application to
Applications installs it. Linux has no installer — the executable can be
run directly after extracting the archive, and the system tray icon
requires GTK3 and libayatana-appindicator (or libappindicator), which is
standard on most desktop distributions; e.g. on Debian/Ubuntu:
`sudo apt install libgtk-3-0 libayatana-appindicator3-1`.

### Install with cargo

It is installable with a Rust toolchain already set up. On Linux, the tray
icon's build dependencies must be installed first:
`sudo apt install libgtk-3-dev libayatana-appindicator3-dev` (Debian/Ubuntu;
`gtk3 libappindicator-gtk3` or `libayatana-appindicator` on Arch/Manjaro).

```sh
cargo install --locked kicad-auto-importer
```

This installs the `kicad-auto-importer` binary to cargo's bin
directory (`~/.cargo/bin` by default), which should already be on
`PATH` if cargo itself is.

### Build from source

On Linux, the tray icon's build dependencies must be installed first:
`sudo apt install libgtk-3-dev libayatana-appindicator3-dev` (Debian/Ubuntu;
`gtk3 libappindicator-gtk3` or `libayatana-appindicator` on Arch/Manjaro).

```sh
git clone https://github.com/sunipkm/kicad-auto-importer
cd kicad-auto-importer
cargo build --release -p kicad-auto-importer
```

This produces a single binary at `target/release/kicad-auto-importer`
(`.exe` on Windows).

## Configuration

Two separate configuration files are used:

- **Per-project settings** — the watch folder, symbol/footprint library
  paths, and import options for a given project are stored in
  `.kicad-autoimport-cfg.json` inside that project's own directory. A
  project is always scoped to one running instance; there is no global
  fallback location for these settings.
- **Global settings** — the Mouser/DigiKey API credentials and the
  path of the last-opened project are account-level rather than
  project-level, and are stored in a single `settings.json` in the
  platform's standard configuration directory (`~/.config` on Linux,
  `~/Library/Application Support` on macOS, `%APPDATA%` on Windows).

## License

MIT — see [LICENSE](../../LICENSE).
