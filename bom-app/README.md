# KiCad BOM Tool (`kicad-bom`)

A standalone companion to
[`kicad-auto-importer`](https://github.com/sunipkm/kicad-auto-importer) for
priced, multi-vendor bills of materials: **Populate BOM** (look up
manufacturer/distributor data for every symbol placed on a project's
schematic and write it back onto each one) and **Generate BOM** (group
identical parts, price them out for a given board quantity against the
cheapest available Mouser/DigiKey quantity break, and export a priced
PDF/XLSX report).

For build/install instructions, see [BUILD.md](BUILD.md). This document
covers how the tool itself works.

It is a [Tauri](https://tauri.app/) app: a Rust backend
(`src-tauri/`) plus a React/TypeScript frontend (`src/`). Schematic
parsing and symbol-library patching reuse the sibling
[`crates/core`](../crates/core) crate (shared with the main
`kicad-auto-importer` desktop app, [`crates/app`](../crates/app/README.md)),
while the Mouser/DigiKey API clients, grouping/pricing math, and
PDF/XLSX generation are this app's own exclusive logic. See the root
[`README.md`](../README.md#workspace-layout) for how the three pieces
(`crates/core`, `crates/app`, `kicad-bom`) fit together.

## Opening a project

Point the tool at a KiCad project directory (Browse… or paste a path).
It locates the root `.kicad_sch` matching the project's `.kicad_pro`
stem and walks it — including every hierarchical sub-sheet it
references, transitively, following each `(sheet (Sheetfile ...))` — to
build the full list of placed symbols. Two kinds of symbols are always
excluded from that list, the same way KiCad's own BOM export excludes
them:

- Symbols with `(in_bom no)` set.
- Symbols marked **DNP** ("Do Not Populate") in KiCad — `(dnp yes)`.
- KiCad's own auto-generated non-BOM references (`#PWR...`, `#FLG...`).

A multi-unit symbol (e.g. a quad op-amp) places one `(symbol ...)`
block per unit sharing the same reference; only the first unit is kept,
so it appears once, not once per unit.

Each tab has its own **Reload** button that re-walks the schematic from
disk — useful after editing the schematic in KiCad itself, since the
tool never watches the file for changes automatically.

## Populate BOM

Lists every placed symbol (reference, value, description) with a
checkbox per row. Selecting rows and clicking **Populate BOM**:

1. Looks up each part's manufacturer part number against Mouser and/or
   DigiKey (whichever vendor(s) have credentials configured), scores
   the candidates by cheapest offer that can actually cover the needed
   quantity, and writes the winning vendor's SKU/price/stock/lifecycle
   data back onto that symbol instance as KiCad properties (`Mfr #`,
   `<Vendor> #`, stock/lifecycle notes, plus a `Last Checked`
   timestamp).
2. Skips any part whose `Last Checked` is under 24h old, unless
   **Force re-check** is ticked — this is what makes re-running
   Populate BOM on a large project cheap: only parts that have gone
   stale (or were never checked) actually hit the network.
3. Saves a PDF stock/lifecycle report for whichever parts were actually
   (re-)checked this run.

Each row's automatic pick can be overridden: the vendor dropdown next
to a row shows every scored candidate from every configured vendor,
ranked best-first, and picking one there pins that exact choice onto
the schematic immediately (`apply_vendor_choice`), independent of a
full batch run.

### Lookup field selection

For each symbol instance, the vendor search uses the first non-empty
property it finds whose name matches one of the following
case-insensitively: `MPN`, `Manufacturer Part Number`, `Mfr#`, `Mfr #`,
or `Part Number`. If none is present, it falls back to the bare library
symbol name (the part after `:` in the library ID); for example,
`Amplifier_Operational:LM358` is searched as `LM358`.

`Reference`, `Value`, `Footprint`, `Description`, and `Datasheet` are
not used to form the vendor search. This matters for generic KiCad
symbols: a `Device:R` resistor with value `10k`, or a `Device:C`
capacitor with value `100nF`, falls back to the broad search term `R`
or `C`, not `10k` or `100nF`. Set one of the MPN properties above on
the symbol instance for a reliable lookup (for example,
`MPN = RC0603FR-0710KL`). Generate BOM still keeps generic parts with
different values or footprints in separate groups, but that grouping
does not make their vendor search term more specific.

## Generate BOM

Groups every placed symbol into unique purchasable parts (same MPN, or
same value+footprint for generic passives with no MPN set), prices each
group for a given **board quantity**, and produces a priced PDF and/or
XLSX report. A **passive extra margin** percentage pads the needed
quantity of resistors/capacitors/inductors only (minimum +5 pieces) to
account for the usual assembly/rework loss on cheap passives.

Like Populate BOM, a group whose cached lookup is still fresh (within
24h — the two features share the same cache) is reused instead of
re-queried, unless **Force re-check** is set.

Once a run completes, if the project has a PCB, an **Interactive BOM**
becomes available — a self-contained HTML board viewer (adapted from
[InteractiveHtmlBom](https://github.com/openscopeproject/InteractiveHtmlBom))
highlighting each part's footprint on the board, annotated with this
run's pricing. It opens in its own window, or can be exported to a
standalone HTML file or XLSX.

## KiCad-open awareness

This tool reads and writes the same `.kicad_sch` files KiCad itself
has open, so every actual write is guarded against clobbering KiCad's
own in-memory copy:

- Right before writing to a specific `.kicad_sch`, the tool checks for
  the sibling lock file KiCad itself creates while that exact file is
  open in an editor (`~<name>.kicad_sch.lck`, next to the file) — the
  same mechanism KiCad uses to warn about a file already being open
  elsewhere. If present, that file's write is skipped (lookups, in-
  memory updates, and the report are unaffected) and logged as a
  warning; nothing about the skip is persisted, so the next run
  retries cleanly.
- This check happens per file, at the moment of the actual write — not
  once for a whole batch — so closing a sheet in KiCad partway through
  a run doesn't block writes to sheets it's no longer holding.
- Before starting a batch, the frontend also shows an advisory heads-up
  if any KiCad process appears to have the project open at all (a
  coarser, best-effort process-level check) — purely informational; it
  never blocks the run, since the precise per-file check above is what
  actually decides whether each write proceeds.

## Credentials and local caches

`kicad-bom` shares its Mouser/DigiKey API credentials with `crates/app`:
both read/write the same `settings.json` in the platform's standard
config directory (`~/.config/kicad-auto-importer` on Linux,
`~/Library/Application Support/kicad-auto-importer` on macOS,
`%APPDATA%\kicad-auto-importer` on Windows) — credentials entered in
one show up in the other immediately, no import/export step needed.
Each vendor's field in the "API Settings" popover (the gear icon, top
right) has its own "Test" button that verifies the entered key/
credentials against the live API without saving anything.

The same directory also holds `parts_cache.json` — a local, global
cache of every raw Mouser/DigiKey search result Populate/Generate BOM
have fetched (keyed by manufacturer part number, not by project), used
to pick a vendor+part automatically (cheapest offer that can actually
cover the needed quantity — see `parts_lookup::score_candidates`) and
to skip a live API call entirely when a cached entry is still fresh
(24h). It's plain JSON and safe to delete any time — everything in it
gets refetched on demand.

## Third-party assets

The following files in `src-tauri/assets/interactive_bom/` are
reproduced verbatim from
[openscopeproject/InteractiveHtmlBom](https://github.com/openscopeproject/InteractiveHtmlBom)
(MIT licence, © openscopeproject contributors):
`ibom.html`, `ibom.css`, `ibom.js`, `util.js`, `render.js`,
`table-util.js`, `split.js`, `pep.js`, `lz-string.js`.
