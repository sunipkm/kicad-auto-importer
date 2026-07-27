# kicad-auto-importer

A standalone watch-folder importer for KiCad part libraries. It watches
a folder (e.g. your Downloads folder) for part-provider ZIP downloads
(UltraLibrarian / Mouser / DigiKey), extracts them, and imports the
symbol / footprint / 3-D model files into one KiCad project's
libraries — registering the destination symbol and footprint libraries
in that project's `sym-lib-table` / `fp-lib-table` so KiCad picks them
up automatically.

This is a companion to the
[part-provider-importer](https://github.com/sunipkm/part-provider-importer)
KiCad pcbnew plugin (Python), which that repo's own README describes in
more detail. That plugin does two things: watches a folder and imports
downloads (what this program replaces), and lets you cherry-pick
symbols/footprints from another KiCad project into the one you have
open (which stays a Python pcbnew ActionPlugin, since it benefits from
running inside KiCad).

Unlike the Python plugin, this program has **no dependency on KiCad,
Python, or wxPython at all** — it's a single native binary you run
alongside KiCad (or leave running in the background).

## Config compatibility

This tool reads and writes the exact same `ultralib_importer.json`
schema as the Python plugin — if a project is already configured by
the Python plugin's watch-folder feature, this tool picks it up with
zero reconfiguration (and vice versa).

One deliberate difference: this tool always requires a known KiCad
project directory (there's no global fallback config location) — it's
explicitly scoped to one project per running instance.

## Building

```
cargo build --release
```

Produces a single binary at `target/release/kicad_auto_importer_app`
(`.exe` on Windows). No installation step, no external runtime
dependencies.

## Workspace layout

- `crates/core` — the import pipeline: sexp (S-expression) parsing and
  writing, `sym-lib-table`/`fp-lib-table` handling, the symbol /
  footprint / 3-D model import pipeline, the folder-watcher's
  settle/debounce logic, and config persistence. No GUI dependency —
  fully unit tested on its own.
- `crates/app` — the small window (fields for watch folder / symbol
  library / footprint library / options, a start/stop toggle, and a
  log pane) that drives `core`.

## A note on KiCad's file format

KiCad's own quoting rules for its S-expression files are stricter and
more inconsistent than they look: some numeric-looking values are
always quoted (`(property "Height" "1.04" ...)`, `(number "7" ...)`),
while some keyword-like values are always bare
(`(justify left top)`, `(type default)`) — and a couple of fields even
mix bareword names with quoted values in the same node
(`(property ki_fp_filters "...")`). `crates/core/src/sexp.rs` documents
this in detail; the short version is that quoting is decided by
**what the source file already did**, never guessed from an atom's
content. Getting this wrong silently corrupts a project's library files
in a way KiCad's own parser then hard-rejects.
