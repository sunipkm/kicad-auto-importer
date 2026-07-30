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

The two apps each keep their own Mouser/DigiKey API credentials and
settings (separate `settings.json` files), but share the KiCad
source-file primitives (S-expression parsing, `.kicad_sym`/`.kicad_sch`
handling, project/library-table resolution) from `crates/core` — see
[Workspace layout](#workspace-layout) below.

## Workspace layout

- `crates/core` — core KiCad source-file primitives shared by both
  apps: sexp (S-expression) parsing and writing, `.kicad_sym` symbol-
  library and `.kicad_sch` schematic parsing/patching, and
  `sym-lib-table`/`fp-lib-table`/project-file resolution. No GUI
  dependency, no app-specific business logic — fully unit tested on its
  own.
- `crates/app` — the egui-based GUI, and this app's own exclusive
  logic: the symbol/footprint/3-D-model import pipeline, the folder-
  watcher's settle/debounce logic, and its project-scoped config
  persistence — plus the main window (watch folder, symbol/footprint
  library paths, options, a start/stop toggle, and an activity log),
  the Import-From-Another-Project sub-window, and system tray
  integration. See [`crates/app/README.md`](crates/app/README.md).
- `bom-app` — a standalone Tauri (Rust backend + React/TypeScript
  frontend) app for Populate BOM and Generate BOM — see
  [`bom-app/README.md`](bom-app/README.md). Split out of `crates/app`
  because a richer, multi-vendor result picker needed more UI
  flexibility than egui's table widgets comfortably give; its backend
  owns its own Mouser/DigiKey lookup, BOM grouping/pricing, and
  PDF/XLSX report generation logic (`parts_lookup`/`bom_pricing`/
  `bom_report`/`populate_bom`/`generate_bom` modules), reusing only
  `crates/core`'s schematic/symbol-library primitives.

## Development

`test-project/` is a fixture KiCad project used to manually smoke-test
both apps (watcher/import, Populate/Generate BOM). Running the apps
against it can leave it locally modified, so its tracked files are kept
under `git update-index --skip-worktree` — Git ignores those local
edits by default. To intentionally update the fixture:

```sh
./scripts/test-project.sh unlock   # allow edits/staging/commits
# ...edit, git add, git commit...
./scripts/test-project.sh lock     # re-protect
```

Run `./scripts/test-project.sh reset` to discard local edits and
restore the committed content (re-locks automatically), or `status` to
see the current lock state.

## License

MIT — see [LICENSE](LICENSE).
