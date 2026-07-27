# kicad-auto-importer

<!--[![test](https://github.com/sunipkm/kicad-auto-importer/actions/workflows/test.yml/badge.svg)](https://github.com/sunipkm/kicad-auto-importer/actions/workflows/test.yml)-->
[![release](https://github.com/sunipkm/kicad-auto-importer/actions/workflows/release.yml/badge.svg)](https://github.com/sunipkm/kicad-auto-importer/actions/workflows/release.yml)
[![Latest release](https://img.shields.io/github/v/release/sunipkm/kicad-auto-importer)](https://github.com/sunipkm/kicad-auto-importer/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A standalone watch-folder importer for KiCad part libraries. It watches
a folder (e.g. your Downloads folder) for part-provider ZIP downloads
(UltraLibrarian / Mouser / DigiKey), extracts them, and imports the
symbol / footprint / 3-D model files into one KiCad project's
libraries — registering the destination symbol and footprint libraries
in that project's `sym-lib-table` / `fp-lib-table` so KiCad picks them
up automatically.

Additionally, it supports loading the project-specific symbols from
another KiCad project, and copy them over (with footprints and 3D
models) to the project-specific libraries of the working project.

This tool does not depend on KiCad — it's a single
native binary with its own GUI that you run alongside KiCad, or leave
running in the background to import new parts as they land in your
downloads folder.

## Features

- Watches a folder and imports part-provider ZIPs as they arrive, or
  imports a single ZIP / folder on demand.
- Registers imported symbol and footprint libraries in the target
  project's `sym-lib-table` / `fp-lib-table` automatically.
- Copies referenced 3-D models into the project and rewrites model
  paths to a `${KIPRJMOD}`-relative URI.
- Optional move-after-import and timestamped backups of the source ZIP.
- Single self-contained binary — does not require KiCad or Python (Linux
  needs GTK3 + libayatana-appindicator at runtime for the tray icon; see
  below).
- Import symbols and footprints from project-specific libraries of other
  KiCad projects.
- Closes to a system tray icon instead of quitting — restore the window
  from there, or start/stop watching without opening it at all. Only one
  instance ever runs; launching it again just brings the existing window
  to the front.

## Installation

### Download a release

Prebuilt binaries for Linux, macOS (Intel and Apple Silicon), and
Windows are published on the
[Releases page](https://github.com/sunipkm/kicad-auto-importer/releases/latest):

| Platform             | Artifact                                         | Contains |
| -------------------- | ------------------------------------------------- | -------- |
| Windows (x86_64)     | `kicad-auto-importer-x86_64-pc-windows-msvc.zip`   | An NSIS installer (`KiCadAutoImporter-Setup-*.exe`) — per-user install, no admin/UAC prompt, Start Menu shortcut, and a proper uninstaller. |
| Linux (x86_64)       | `kicad-auto-importer-x86_64-unknown-linux-gnu.tar.gz` | The bare binary. |
| macOS (Intel)        | `kicad-auto-importer-x86_64-apple-darwin.dmg`   | A `.dmg` — the usual drag-`KiCad Auto Importer.app`-onto-Applications installer. |
| macOS (Apple Silicon) | `kicad-auto-importer-aarch64-apple-darwin.dmg` | Same as above. |

Windows: run the installer inside the zip. macOS: open the `.dmg` and
drag the app to Applications. Linux: untar and run the executable
directly — there is no installer, and the system tray icon needs GTK3
and libayatana-appindicator (or libappindicator) installed, which is
standard on most desktop distros; e.g. on Debian/Ubuntu:
`sudo apt install libgtk-3-0 libayatana-appindicator3-1`.

### Build from source

On Linux, install the tray icon's build dependencies first:
`sudo apt install libgtk-3-dev libayatana-appindicator3-dev` (Debian/Ubuntu;
`gtk3 libappindicator-gtk3` or `libayatana-appindicator` on Arch/Manjaro).

```sh
cargo build --release -p kicad-auto-importer
```

Produces a single binary at `target/release/kicad-auto-importer`
(`.exe` on Windows).

## Config file

Each project's settings (watch folder, symbol/footprint library paths,
options) are stored in `.kicad-autoimport-cfg.json` inside that project's
directory. A project is always scoped to one running instance — there
is no global fallback config location.

## Workspace layout

- `crates/core` — the import pipeline: sexp (S-expression) parsing and
  writing, `sym-lib-table`/`fp-lib-table` handling, the symbol /
  footprint / 3-D model import pipeline, the folder-watcher's
  settle/debounce logic, and config persistence. No GUI dependency —
  fully unit tested on its own.
- `crates/app` — the small window (fields for watch folder / symbol
  library / footprint library / options, a start/stop toggle, and a
  log pane) that drives `core`.

## License

MIT — see [LICENSE](LICENSE).
