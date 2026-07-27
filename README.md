# kicad-auto-importer

[![test](https://github.com/sunipkm/kicad-auto-importer/actions/workflows/test.yml/badge.svg)](https://github.com/sunipkm/kicad-auto-importer/actions/workflows/test.yml)
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
- Single self-contained binary — no KiCad, Python, or runtime
  dependencies to install.
- Import symbols and footprints from project-specific libraries of other
  KiCad projects.

## Installation

### Download a release

Prebuilt binaries for Linux, macOS (Intel and Apple Silicon), and
Windows are published on the
[Releases page](https://github.com/sunipkm/kicad-auto-importer/releases/latest):

| Platform             | Artifact                                         |
| -------------------- | ------------------------------------------------- |
| Windows (x86_64)     | `kicad-auto-importer-x86_64-pc-windows-msvc.zip`   |
| Linux (x86_64)       | `kicad-auto-importer-x86_64-unknown-linux-gnu.tar.gz` |
| macOS (Intel)        | `kicad-auto-importer-x86_64-apple-darwin.tar.gz`   |
| macOS (Apple Silicon) | `kicad-auto-importer-aarch64-apple-darwin.tar.gz` |

Unzip or untar the archive and run the executable — there is no
installer and no external runtime to set up.

### Build from source

```sh
cargo build --release -p kicad_auto_importer_app
```

Produces a single binary at `target/release/kicad_auto_importer_app`
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
