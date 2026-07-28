# kicad-auto-importer

<!--[![test](https://github.com/sunipkm/kicad-auto-importer/actions/workflows/test.yml/badge.svg)](https://github.com/sunipkm/kicad-auto-importer/actions/workflows/test.yml)-->
[![release](https://github.com/sunipkm/kicad-auto-importer/actions/workflows/release.yml/badge.svg)](https://github.com/sunipkm/kicad-auto-importer/actions/workflows/release.yml)
[![Latest release](https://img.shields.io/github/v/release/sunipkm/kicad-auto-importer)](https://github.com/sunipkm/kicad-auto-importer/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A standalone companion application for KiCad part libraries and bills of
materials. It watches a folder (e.g. a Downloads folder) for
part-provider ZIP downloads (UltraLibrarian / Mouser / DigiKey),
extracts them, and imports the symbol / footprint / 3-D model files into
a KiCad project's libraries — registering the destination symbol and
footprint libraries in that project's `sym-lib-table` / `fp-lib-table`,
so KiCad picks them up automatically.

It also supports loading the project-specific symbols from another
KiCad project and copying them over (with footprints and 3-D models)
into the working project's own libraries.

Separately, it can populate a schematic's bill of materials with
manufacturer and distributor information from the Mouser and DigiKey
APIs — including live stock availability and lifecycle status — and
generate a PDF stock report summarizing the results, with parts that
are out of stock or flagged obsolete/EOL/NRND clearly marked.

This tool does not depend on KiCad — it is a single native binary with
its own GUI, meant to run alongside KiCad or remain running in the
background to import new parts as they arrive in the configured watch
folder.

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
- **Populate BOM**: scans a project's schematic, including every
  hierarchical sub-sheet, for placed symbols, then looks each one up
  against the Mouser and/or DigiKey APIs and writes manufacturer, part
  number, distributor SKU/pricing, stock availability, and lifecycle
  status onto the matching schematic symbol instance.
- Generates a paginated PDF stock report from a Populate BOM run, with
  parts not confirmed in stock or flagged obsolete/EOL/NRND highlighted.
- Skips re-querying a part that was checked within the last 24 hours
  (with an option to force a re-check), and warns before writing to a
  schematic that appears to be open in KiCad at the same time.
- Single self-contained binary — does not require KiCad or Python (Linux
  needs GTK3 + libayatana-appindicator at runtime for the tray icon; see
  below).
- Closes to a system tray icon instead of quitting — the window can be
  restored from there, or watching can be started/stopped without
  opening it at all. Only one instance ever runs; launching it again
  simply brings the existing window to the front.

## Populate BOM

The Populate BOM window is opened from the main window once a project
and at least one vendor's API credentials (see below) are configured.
It lists every symbol placed on the project's schematic — the root
sheet plus every hierarchical sub-sheet, deduplicated by reference
designator — rather than only symbols defined in the project's own
local libraries, so parts living entirely inside a sub-sheet are still
covered.

For each row selected, a manufacturer part number is resolved from the
symbol's existing properties (or its symbol name as a fallback) and
looked up against whichever of Mouser and DigiKey are configured. The
result is written back onto that specific placed *instance* of the
symbol — identified by its schematic UUID — rather than the shared
library symbol it comes from, since a generic symbol such as `Device:R`
is reused by many differently-valued placements and patching the
library symbol itself would incorrectly apply one part's data to all of
them.

Once a batch finishes, a paginated PDF stock report is generated at a
location chosen via a save dialog (defaulting to the project's root
directory), listing every part that was looked up along with its
manufacturer, distributor, stock status, and lifecycle status; parts
that are not confirmed in stock or that a vendor flags as
obsolete/end-of-life/not-recommended-for-new-designs are highlighted.

A part that was already checked within the last 24 hours is skipped on
subsequent runs to avoid unnecessary API traffic, unless the "Force
re-check" option is enabled. Before a batch starts, the application also
checks whether the target project appears to be open in a live KiCad
process (project manager, schematic editor, or PCB editor); if so, a
warning is shown, since a direct write to the schematic file on disk
would be invisible to — and could later be overwritten by — an
already-open KiCad session. The lookups and PDF report still proceed in
that case; only the write-back to the schematic file itself is held
back until the project is closed in KiCad.

## API Settings — Mouser and DigiKey registration

Manufacturer and distributor lookups are performed through the Mouser
Search API and the DigiKey Product Information API. Credentials for
either or both vendors can be entered via the "API Settings" popup in
the main window; a lookup uses whichever vendor(s) are configured.

- **Mouser** requires an API key, obtained by registering for API
  access at [mouser.com/api-search](https://www.mouser.com/api-search/).
- **DigiKey** requires a Client ID and Client Secret from an OAuth2
  client-credentials application, obtained at
  [developer.digikey.com](https://developer.digikey.com/).

Each vendor's fields include a "Test" button that verifies the entered
credentials against the live API and reports whether they were
accepted. These credentials are account-level, not project-level, and
are stored separately from any single project's own settings — see
[Configuration](#configuration) below.

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

On Windows, running the installer contained in the zip completes setup.
On macOS, opening the `.dmg` and dragging the application to
Applications installs it. Linux has no installer — the executable can be
run directly after extracting the archive, and the system tray icon
requires GTK3 and libayatana-appindicator (or libappindicator), which is
standard on most desktop distributions; e.g. on Debian/Ubuntu:
`sudo apt install libgtk-3-0 libayatana-appindicator3-1`.

### Build from source

On Linux, the tray icon's build dependencies must be installed first:
`sudo apt install libgtk-3-dev libayatana-appindicator3-dev` (Debian/Ubuntu;
`gtk3 libappindicator-gtk3` or `libayatana-appindicator` on Arch/Manjaro).

```sh
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

## Workspace layout

- `crates/core` — the import pipeline: sexp (S-expression) parsing and
  writing, `sym-lib-table`/`fp-lib-table` handling, the symbol /
  footprint / 3-D model import pipeline, the folder-watcher's
  settle/debounce logic, Mouser/DigiKey part lookups and PDF
  stock-report generation, and config persistence. No GUI dependency —
  fully unit tested on its own.
- `crates/app` — the egui-based GUI: the main window (watch folder,
  symbol/footprint library paths, options, a start/stop toggle, and an
  activity log), the Populate BOM and Import-From-Another-Project
  sub-windows, the API Settings popup, and system tray integration —
  all driving `core`.

## License

MIT — see [LICENSE](LICENSE).
