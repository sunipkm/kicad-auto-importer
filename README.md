# kicad-auto-importer

<!--[![test](https://github.com/sunipkm/kicad-auto-importer/actions/workflows/test.yml/badge.svg)](https://github.com/sunipkm/kicad-auto-importer/actions/workflows/test.yml)-->
[![release](https://github.com/sunipkm/kicad-auto-importer/actions/workflows/release.yml/badge.svg)](https://github.com/sunipkm/kicad-auto-importer/actions/workflows/release.yml)
[![Latest release](https://img.shields.io/github/v/release/sunipkm/kicad-auto-importer)](https://github.com/sunipkm/kicad-auto-importer/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A Rust workspace with two companion desktop applications for KiCad,
built on a shared, GUI-agnostic core library — each has its own README
with full details:

- **[`kicad-auto-importer`](crates/app/README.md)** — watches a folder
  for part-provider ZIP downloads (UltraLibrarian / Mouser / DigiKey)
  and imports the symbol / footprint / 3-D model files straight into a
  KiCad project's libraries. This is the original application the
  repository is named after.
- **[`bom-app`](bom-app/README.md)** — a standalone Tauri app for
  populating a schematic's bill of materials with manufacturer/
  distributor data and generating a priced, multi-vendor PDF/XLSX BOM
  report.

The two share Mouser/DigiKey API credentials via the same
`settings.json` and reuse the same import/lookup/pricing logic from
`crates/core` — see [Workspace layout](#workspace-layout) below.

## Workspace layout

- `crates/core` — the import pipeline: sexp (S-expression) parsing and
  writing, `sym-lib-table`/`fp-lib-table` handling, the symbol /
  footprint / 3-D model import pipeline, the folder-watcher's
  settle/debounce logic, Mouser/DigiKey part lookups, BOM
  grouping/pricing, and PDF/XLSX report generation, and config
  persistence. No GUI dependency — fully unit tested on its own, and
  shared unchanged by both apps below.
- `crates/app` — the egui-based GUI: the main window (watch folder,
  symbol/footprint library paths, options, a start/stop toggle, and an
  activity log), the Import-From-Another-Project sub-window, the API
  Settings popup, and system tray integration — all driving `core`. See
  [`crates/app/README.md`](crates/app/README.md).
- `bom-app` — a standalone Tauri (Rust backend + React/TypeScript
  frontend) app for Populate BOM and Generate BOM — see
  [`bom-app/README.md`](bom-app/README.md). Split out of `crates/app`
  because a richer, multi-vendor result picker needed more UI
  flexibility than egui's table widgets comfortably give; its backend
  is a thin wrapper around the same `core` orchestration
  (`populate_bom`/`generate_bom` modules), not a reimplementation.

## License

MIT — see [LICENSE](LICENSE).
