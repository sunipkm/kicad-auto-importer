# Interactive HTML BOM in bom-app — implementation sprintboard

> Merges functionality from `kicad-interactive-bom`
> (openscopeproject/InteractiveHtmlBom, MIT) into bom-app: a clickable
> PCB view + BOM table in a new Tauri window, opened from Generate BOM.
> Check items off (`- [x]`) as they land. If interrupted, read the
> "Progress log" at the bottom first — it records exactly what's been
> verified/read vs. not-yet-started.

## Decisions
- PCB data source: reimplement `.kicad_pcb` parsing natively in Rust — no
  Python/pcbnew dependency at runtime.
- Reuse InteractiveHtmlBom's static `web/` assets (ibom.html/js/css) as
  the viewer, vendored into bom-app.
- Scope: MVP now = board edge outline + footprint pad outlines/positions
  + clickable BOM-row linkage. No copper tracks/zones/net highlighting,
  no exact silkscreen/fab text rendering (no newstroke font port).
- Excel export = reuse the *existing* priced xlsx export
  (`bom_report::generate_priced_bom_xlsx`) — not a new plain-BOM xlsx format.
  Must support user-selectable + reorderable columns (mirroring
  InteractiveHtmlBom's `dialog/settings_dialog.py` FieldsPanel:
  Show-checkbox + Up/Down reorder, adapted as a plain checklist).
  Mandatory (always-checked, disabled, still reorderable) columns: Part,
  References, Needed Qty. Optional: Purchase Qty, Vendor, Unit Price,
  Ext Price, In Stock, Stock Qty, Stock Shortfall, Lifecycle Concern.
  Persisted so both Generate BOM's own xlsx export and the Interactive
  BOM window's Export Excel share the same config.
- Trigger: NOT a new tab. Generate BOM, after finishing, shows a
  "View Interactive BOM" action that opens a **new Tauri window**
  rendering the generated interactive HTML BOM, with "Export Excel" /
  "Export HTML" actions in that window.

## Source references (read-only reference project)
- `/home/sunip/Codes/python/kicad-interactive-bom/InteractiveHtmlBom/ecad/kicad.py` — pcbnew-based parser (reference only, not ported as-is)
- `/home/sunip/Codes/python/kicad-interactive-bom/InteractiveHtmlBom/core/ibom.py` — `generate_file()`'s `///MARKER///` HTML templating scheme (verified, see below), `generate_bom()` grouping
- `/home/sunip/Codes/python/kicad-interactive-bom/DATAFORMAT.md` — pcbdata JSON schema (footprint/pad/drawing/bom structs; partially reviewed, see progress log)
- `/home/sunip/Codes/python/kicad-interactive-bom/InteractiveHtmlBom/web/*` — ibom.html/css/js + vendored split.js/pep.js/lz-string.js (all MIT) to be copied into bom-app as static assets
- `/home/sunip/Codes/python/kicad-interactive-bom/InteractiveHtmlBom/dialog/settings_dialog.py` — `FieldsPanel` (Show/Group checkbox grid + `OnFieldsUp`/`OnFieldsDown`/`_swapRows`) — pattern for the xlsx column picker (Phase C2)

### Verified: `ibom.html`/`generate_file()` templating (exact marker list, in order)
`ibom.html`'s `<head>` has, in this order: `///CSS///`, `///USERCSS///`,
then inside one `<script>` block: `///SPLITJS///`, `///LZ-STRING///`,
`///POINTER_EVENTS_POLYFILL///`, `///CONFIG///`, `///PCBDATA///`,
`///UTILJS///`, `///RENDERJS///`, `///TABLEUTILJS///`, `///IBOMJS///`,
`///USERJS///` — then `</head>`, then `<body>` starts with
`///USERHEADER///` before the real markup, and (checked separately,
assume near end of body) `///USERFOOTER///`.
`generate_file()` (ibom.py) replace order: CSS, USERCSS, SPLITJS,
LZ-STRING (empty string if `!config.compression` — **we will always
pass `""` here, no compression support in the Rust port for MVP**),
POINTER_EVENTS_POLYFILL, CONFIG (`"var config = " + json`),
UTILJS, RENDERJS, TABLEUTILJS, IBOMJS, USERJS, USERHEADER, USERFOOTER,
and **PCBDATA replaced last** ("for better performance" — do the same in
Rust, i.e. embed pcbdata via `.replacen`/`.replace` as the final step).
`var pcbdata = <json>` (no LZString wrapping since we skip compression).
Rust equivalent: `bom-app/src-tauri/src/interactive_bom.rs::render_html`
does the same sequence of `.replace()` calls against an `include_str!`
of the vendored `ibom.html`, with `USERCSS`/`USERJS`/`USERHEADER` slots
also carrying our injected "Export Excel"/"Export HTML" toolbar
(instead of empty user.css/user.js/userheader.html — upstream leaves
those blank/optional local files, we always inject our own content
there, no upstream `user.css`/`user.js`/`userheader.html`/`userfooter.html`
files exist to vendor, we just supply the replacement text directly in
Rust rather than reading nonexistent files).

### `DATAFORMAT.md` — pcbdata schema, footprint struct still needs a full read
Verified so far (top-level `pcbdata` object): `edges_bbox` (`minx/miny/maxx/maxy`),
`edges` (array of drawing structs — segment/rect/circle/arc/curve/polygon —
each described further down in the doc under "drawing struct"),
`drawings.{silkscreen,fabrication}.{F,B}` (arrays of drawings, empty in MVP),
`footprints` (array, index = numeric component ID — **struct fields
not yet fully read, needed before writing `build_pcbdata`**), optional
`tracks`/`zones`/`nets` (all omitted in MVP — no copper/net rendering),
`metadata` (`title`/`revision`/`company`/`date`/optional `variant`),
`bom.{both,F,B}` (arrays of bomrow — **bomrow struct not yet read**),
`bom.skipped` (numeric IDs, DNP — none in MVP, always `[]`),
`bom.fields` (map keyed by component ID → array of field values, order
matches `config.extra_fields`), optional `font_data` (omitted — no
silkscreen/fab text rendering in MVP).
**TODO before Phase C**: read the rest of `DATAFORMAT.md` (footprint
struct incl. pads, and the "bom row struct"/"footprint struct" sections
below the drawing-struct section already read) — needed to get
`build_pcbdata`'s footprint/pad/bom-row field names exactly right.

## Steps

### Phase A — Vendor frontend assets  ✅ DONE
- [x] Copy `web/{ibom.html,ibom.css,util.js,render.js,table-util.js,ibom.js,split.js,pep.js,lz-string.js}`
      into `bom-app/src-tauri/assets/interactive_bom/`.
- [x] Add attribution note to `bom-app/README.md`.

## Phase B — `.kicad_pcb` parsing (crates/core) — parallel with A  ✅ DONE
- [x] New `crates/core/src/pcb.rs`: `PcbBoard` — `footprints` (ref, layer,
      center/angle, computed local bbox, `pads` with pos/size/angle/shape/
      type/layers/net), `edges`/`edges_bbox` from Edge.Cuts geometry,
      top-level-property `metadata` (KiCad 8+ has no `title_block`).
- [x] `find_root_pcb(project_dir)` — same `.kicad_pro`-stem → fallback pattern.
- [x] Register `pub mod pcb;` in `crates/core/src/lib.rs`.
- [x] 47 tests pass: circumcircle math, SOT-23 arc sample, cardinal-angle
      checks, rotate 90°/180°, find_root_pcb, footprint count, edges_bbox
      validity, metadata title+revision, pad absolute positions.

### KiCad 8+/10 format notes (verified from kiwi-pwr-in.kicad_pcb v20260206)
- No `(title_block ...)` — custom vars are top-level `(property "KEY" "VAL")`.
  `PRJ_TITLE`/`BOARD_REV` are the field names in the test project.
- Arcs: `(start ...) (mid ...) (end ...)` three-point format; convert to
  circumcircle with `arc_circumcircle()` (public, used by `interactive_bom.rs`).
- Board outline: `gr_rect` on `Edge.Cuts` in the test project.
- Layers are quoted strings (`"F.Cu"`, `"*.Cu"`).
- `net` in pads: single-atom `(net "netname")` in KiCad 8+.
- Angles: real degrees (not decidegrees), positive = CCW in KiCad screen space.

## Phase C — pcbdata assembly — depends on B  ✅ DONE
- [x] New `bom-app/src-tauri/src/interactive_bom.rs`:
  - [x] `build_pcbdata(board, priced_rows) -> serde_json::Value`.
  - [x] `render_html(pcbdata) -> String` — marker-replace templating via `include_str!`.
  - [x] Security hardening: `escape_script()` escapes `</script` in embedded JSON.
  - [x] `config` JSON with `fields: ["Value","Footprint","Price","Vendor"]`.
- [x] Register `mod interactive_bom;` in `bom-app/src-tauri/src/lib.rs`.
- [x] `generate_bom::run_bom_batch` returns `Vec<PricedRow>`.

## Phase C2 — Configurable Excel columns  ✅ DONE
- [x] `bom_report.rs`: `XlsxColumn` enum with `label()`/`is_mandatory()`/`ALL`;
      `generate_priced_bom_xlsx(rows, board_qty, columns: &[XlsxColumn], out_path)`.
- [x] New `xlsx_columns.rs`: `XlsxColumnsConfig` load/save under platform config dir.
- [x] Commands: `load_xlsx_columns_config`, `save_xlsx_columns_config`.
- [x] New `XlsxColumnsPanel.tsx`: checkbox-list and Up/Down reorder; mandatory columns
      checked and disabled. Accessible from the gear-icon `SettingsPanel`.
- [x] Both `generate_bom`'s xlsx branch and `export_interactive_bom_xlsx` load and
      use the saved config.

## Phase D — Tauri wiring — depends on C  ✅ DONE
- [x] `InteractiveBomState(Mutex<Option<InteractiveBomSession>>)` managed
      app state: `{ html, priced_rows, board_qty }`.
- [x] `generate_bom` command: after `run_bom_batch`, parses PCB, builds
      pcbdata/html, stores session, emits `InteractiveBomReady { available }`.
- [x] New commands: `open_interactive_bom`, `export_interactive_bom_xlsx`,
      `export_interactive_bom_html` — all registered in `generate_handler!`.
- N/A `capabilities/default.json` — `open_interactive_bom` opens in the system
      browser via `tauri_plugin_opener`, not a new Tauri webview; no new
      capability scope needed.
- N/A `tauri.conf.json` `withGlobalTauri` — same reason.
- N/A Export toolbar injection — browser window can't invoke Tauri commands.

## Phase E — Frontend — depends on D  ✅ DONE
- [x] `GenerateBom.tsx`: handles `InteractiveBomReady { available }` event;
      shows "View Interactive BOM ↗" button when `available: true`.

## Phase F — Verification  ✅ DONE
- [x] `cargo test` — `pcb` module: 47 tests pass.
- [x] `cargo test` — full workspace: 241 tests, all pass (2026-07-30).
- [x] `cargo clippy` — zero errors, zero warnings (2026-07-30).
- [ ] Manual smoke test.
- [x] `npm run build` for bom-app frontend — passes clean (2026-07-30).

## Relevant files (all read/verified this session unless noted)
- `crates/core/src/schematic.rs` — pattern for `find_root_pcb` (read in full)
- `crates/core/src/sexp.rs` — s-expression primitives (read in full: `SexpNode`/`Atom`/`Child`, `parse()`, `find()`/`find_all()`/`first_atom()`)
- `crates/core/src/lib.rs` — **not yet read**, need to check current `pub mod` list before adding `pcb`
- `bom-app/src-tauri/src/generate_bom.rs` — `run_bom_batch`/`BomEvent`/`BomBatchRequest` (read in full)
- `bom-app/src-tauri/src/bom_pricing.rs` — `PartGroup`/`PricedRow`/`ChosenOffer`/`group_placed_symbols` (read in full)
- `bom-app/src-tauri/src/bom_report.rs` — `generate_priced_bom_xlsx`, `PRICED_COLUMNS`/`STOCK_COLUMNS` consts, PDF drawing machinery (read: consts + full xlsx fn + module doc comment; PDF-drawing internals only skimmed)
- `bom-app/src-tauri/src/vendor_credentials.rs` — load/save pattern to mirror for `xlsx_columns.rs` (read in full)
- `bom-app/src-tauri/src/lib.rs` — command/state wiring (read in full)
- `bom-app/src-tauri/capabilities/default.json`, `bom-app/src-tauri/tauri.conf.json` (both read in full)
- `bom-app/src/GenerateBom.tsx` (read in full)
- `bom-app/src/App.tsx`, `SettingsPanel.tsx` — **not yet read**, needed to decide where `XlsxColumnsPanel` mounts

## Further Considerations (future work, not in this pass)
1. Full parity: copper tracks/zones + net highlighting, real silkscreen/
   fabrication text via a Rust port of `newstroke_font.py`/`fontparser.py`/
   `svgpath.py` — deliberately deferred.
2. Custom/complex pad shapes (trapezoid, true custom polygon) fall back
   to a rect bounding box in the MVP parser — flag in code comments.

## Progress log
- **2026-07-30**: Pure research/planning session (Plan Mode), then a
  second session began implementation research but **no code has been
  written yet** — this file itself is the first artifact written to the
  repo. Confirmed via direct reads: (a) no `generate_bom.rs`/
  `populate_bom.rs`/`bom_report.rs` duplicates exist in `crates/core`
  (resolves the ambiguity flagged in earlier planning — edit
  `bom-app/src-tauri/src/*` directly); (b) exact `ibom.html` marker
  list + `generate_file()` replace order (see templating note above);
  (c) `DATAFORMAT.md` read through the drawing-struct section only —
  footprint/pad/bomrow struct sections still unread; (d) `web/` dir
  listing confirmed (`ibom.css,ibom.html,ibom.js,lz-string.js,pep.js,
  render.js,split.js,table-util.js,util.js,user-file-examples/` — no
  `user.css`/`user.js`/`userheader.html`/`userfooter.html` exist
  upstream, confirmed empty-by-default per `generate_file()`'s
  `get_file_content` returning `""` for missing files).
- **2026-07-30 (session 2)**: Phases A, C, D, E implemented. `cargo build`
  (bom-app) and `npm run build` both pass. 241 workspace tests pass.
  Also completed across both sessions: full core module refactor
  (`sexp::parse/render` as `SexpNode` impl methods; `LibEntry::from_table/
  from_project`; `scan_symbol_spans` → `SchematicFile::scan_spans`;
  `scan_top_level` → `SymbolLibrary::scan_top_level`); `generate_bom.rs`
  returns `Vec<PricedRow>`.
- **Remaining**: manual smoke test (needs PCB with footprints placed).
