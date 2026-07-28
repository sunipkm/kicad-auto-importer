//! Tauri backend for the standalone "BOM" app (Populate BOM + Generate
//! BOM, split out of the egui `kicad-auto-importer` desktop app — see
//! the repository root `README.md`'s "Workspace layout" section for
//! why).
//!
//! Commands here are thin wrappers around `kicad_auto_importer_core` —
//! the whole point of choosing Tauri over Electron is that the actual
//! logic (schematic parsing, Mouser/DigiKey clients, grouping/pricing,
//! PDF/XLSX generation) lives in that crate unchanged, reused as-is
//! rather than reimplemented in JS/TS.

#[cfg(target_os = "linux")]
mod linux_desktop_integration;

use tauri::{AppHandle, Emitter};

use kicad_auto_importer_core::bom_pricing::{self, ChosenOffer};
use kicad_auto_importer_core::digikey::{self, DigikeyCredentials};
use kicad_auto_importer_core::generate_bom::{self, BomBatchRequest, BomEvent};
use kicad_auto_importer_core::global_settings::GlobalSettings;
use kicad_auto_importer_core::kicad_process;
use kicad_auto_importer_core::mouser;
use kicad_auto_importer_core::parts_cache::PartsCache;
use kicad_auto_importer_core::parts_lookup::{
    self, PartsCredentials, ScoredCandidate, VendorCandidate,
};
use kicad_auto_importer_core::populate_bom::{
    self, LookupEvent, SelectedRow, LAST_CHECKED_PROPERTY, RECHECK_THRESHOLD,
};
use kicad_auto_importer_core::schematic::{self, SchematicFile};
use kicad_auto_importer_core::symbol_importer::set_symbol_property;

/// Reads `~/.config/kicad-auto-importer/settings.json` (per-OS
/// equivalent — see `GlobalSettings`'s own docs) — the *same* file the
/// egui `kicad-auto-importer` app reads/writes, by construction: the
/// location is derived from `dirs::config_dir()` alone, not from which
/// binary is running, so Mouser/DigiKey credentials entered in either
/// app are immediately visible in the other without any bridging code
/// here beyond calling the same `core` function.
#[tauri::command]
fn load_global_settings() -> GlobalSettings {
    GlobalSettings::load()
}

#[tauri::command]
fn save_global_settings(settings: GlobalSettings) -> Result<(), String> {
    settings.save().map_err(|e| e.to_string())
}

/// Backs each vendor's "Test" button — confirms a key/credential pair is
/// actually accepted by the vendor (`mouser::test_credentials` runs a
/// throwaway search; a rejected key is the only failure that matters)
/// without writing anything back. `async fn` + `spawn_blocking`, not a
/// plain `#[tauri::command]`: a non-async command's body runs inline on
/// the same thread that dispatches IPC (see `populate_bom`/`generate_bom`
/// below for the full explanation), which would freeze the whole UI for
/// the network round trip this makes.
#[tauri::command]
async fn test_mouser_credentials(api_key: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        mouser::test_credentials(&api_key).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn test_digikey_credentials(client_id: String, client_secret: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let creds = DigikeyCredentials {
            client_id,
            client_secret,
        };
        digikey::test_credentials(&creds).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Round-trip proof-of-life for the Rust↔JS boundary: validates
/// `path` as a project directory, finds its root schematic (see
/// `schematic::find_root_schematic`), and counts every placed symbol
/// via the same `load_schematic_symbols` walk Populate/Generate BOM
/// will eventually drive — real core logic, not a toy "greet" command.
#[derive(serde::Serialize)]
struct ProjectInfo {
    project_dir: String,
    root_schematic: Option<String>,
    placed_symbol_count: usize,
}

#[tauri::command]
fn open_project(path: String) -> Result<ProjectInfo, String> {
    let project_dir = std::path::PathBuf::from(&path);
    if !project_dir.is_dir() {
        return Err(format!("'{path}' is not a directory"));
    }

    let root_schematic = schematic::find_root_schematic(&project_dir);
    let mut log_lines = Vec::new();
    let symbols =
        schematic::load_schematic_symbols(&project_dir, |m| log_lines.push(m.to_string()));

    Ok(ProjectInfo {
        project_dir: path,
        root_schematic: root_schematic.map(|p| p.display().to_string()),
        placed_symbol_count: symbols.len(),
    })
}

/// A result reconstructed from whatever a *past* lookup already wrote
/// onto the instance — no network call, just re-reading
/// `parts_lookup::apply_part_info`'s own write-back via
/// `read_cached_part_info`/`summarize_offers`. Distinct from a `RowResult`
/// (`PopulateBomEvent`), which only exists once this session's own
/// "Populate BOM" run has actually processed the row.
#[derive(serde::Serialize)]
struct CachedResult {
    /// `false` when the instance has no `Mfr #` at all — i.e. this tool
    /// has never looked it up — in which case `summary` is always
    /// `"Unavailable"`.
    available: bool,
    /// `true` once the cached `Last Checked` is past
    /// `RECHECK_THRESHOLD` — the next "Populate BOM"/"Generate BOM" run
    /// would re-query this part rather than reuse it. Reflected in
    /// `summary` too (a `" (Stale)"` suffix) so the frontend doesn't
    /// need to duplicate that formatting.
    stale: bool,
    needs_attention: bool,
    summary: String,
}

fn cached_result_for(
    sch: Option<&SchematicFile>,
    uuid: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> CachedResult {
    let found = sch
        .and_then(|sch| sch.get_symbol_node(uuid))
        .and_then(|node| {
            let info = parts_lookup::read_cached_part_info(&node)?;
            let (summary, needs_attention) = parts_lookup::summarize_offers(&info.offers, 1)?;
            let stale = populate_bom::last_checked_age(&node, now)
                .is_none_or(|age| age >= RECHECK_THRESHOLD);
            Some(CachedResult {
                available: true,
                stale,
                needs_attention,
                summary: if stale {
                    format!("{summary} (Stale)")
                } else {
                    summary
                },
            })
        });
    found.unwrap_or(CachedResult {
        available: false,
        stale: false,
        needs_attention: false,
        summary: "Unavailable".to_string(),
    })
}

/// One row of `list_placed_symbols` — the frontend's own copy of
/// `kicad_auto_importer_core::schematic::PlacedSymbol`, trimmed to what
/// the Populate BOM table actually shows plus `index` (this call's
/// position, matched back up by `populate_bom` below — see its own
/// docs for why re-listing between the two calls would break that
/// pairing) and `mpn`/`sch_path`/`uuid`, which the vendor-picker flow
/// (`get_scored_candidates`/`apply_vendor_choice` below) needs to look
/// up and pin a choice for this exact instance.
#[derive(serde::Serialize)]
struct PlacedSymbolRow {
    index: usize,
    reference: String,
    value: String,
    description: String,
    mpn: String,
    sch_path: String,
    uuid: String,
    cached: CachedResult,
}

#[tauri::command]
fn list_placed_symbols(project_dir: String) -> Vec<PlacedSymbolRow> {
    let project_dir = std::path::PathBuf::from(project_dir);
    let symbols = schematic::load_schematic_symbols(&project_dir, |_| {});
    let now = chrono::Utc::now();

    // Opens each schematic file at most once (same batching
    // `run_lookup_batch` uses) rather than per row — reading the cached
    // result for every placed symbol on load otherwise means reopening
    // and reparsing the same file for every one of its rows.
    let mut open_files: std::collections::HashMap<std::path::PathBuf, Option<SchematicFile>> =
        std::collections::HashMap::new();

    symbols
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            let sch = open_files
                .entry(row.sch_path.clone())
                .or_insert_with(|| SchematicFile::open(&row.sch_path).ok())
                .as_ref();
            let cached = cached_result_for(sch, &row.uuid, now);
            PlacedSymbolRow {
                index,
                reference: row.reference.clone(),
                value: row.value.clone(),
                description: row.description.clone(),
                mpn: row.resolved_mpn.clone(),
                sch_path: row.sch_path.display().to_string(),
                uuid: row.uuid.clone(),
                cached,
            }
        })
        .collect()
}

/// Every plausible vendor match for `mpn`, ranked best-first by
/// `parts_lookup::score_candidates` at `needed_qty` — the data the
/// vendor-choice dropdown shows. Checks the same global parts cache the
/// batch commands use before ever calling
/// `parts_lookup::lookup_part_candidates` live, so opening the dropdown
/// for a part Populate/Generate BOM already looked up recently is
/// instant, not a fresh round trip. `force_refresh` bypasses the cache
/// read (same "Force re-check" meaning as everywhere else), but a fresh
/// result is still written back to it either way. Doesn't touch the
/// schematic at all — safe to call freely while the user is still
/// deciding.
#[tauri::command]
fn get_scored_candidates(
    mpn: String,
    needed_qty: u32,
    force_refresh: bool,
    credentials: PartsCredentials,
) -> Result<Vec<ScoredCandidate>, String> {
    let mut cache = PartsCache::load();
    let now = chrono::Utc::now();

    let candidate_set = if force_refresh {
        None
    } else {
        cache.get_fresh(&mpn, now, RECHECK_THRESHOLD).cloned()
    };
    let candidate_set = match candidate_set {
        Some(set) => set,
        None => {
            let fetched = parts_lookup::lookup_part_candidates(&credentials, &mpn)
                .map_err(|e| e.to_string())?;
            cache.put(&mpn, now, fetched.clone());
            fetched
        }
    };
    let _ = cache.save();

    Ok(parts_lookup::score_candidates(
        &candidate_set.candidates,
        needed_qty,
    ))
}

/// Pins the user's manually-picked candidate(s) (at most one per
/// vendor) onto the placed instance at `sch_path`/`uuid` — the same
/// `Mfr #`/`<Vendor> #`/… properties a normal `populate_bom` lookup
/// would write, via the same `parts_lookup::apply_part_info`, plus a
/// fresh `Last Checked` so a subsequent Populate BOM run within the 24h
/// window doesn't immediately re-query and potentially overwrite this
/// choice with an auto-picked one. `chosen` empty is a no-op success
/// (nothing to pin).
#[tauri::command]
fn apply_vendor_choice(
    sch_path: String,
    uuid: String,
    mpn: String,
    chosen: Vec<VendorCandidate>,
) -> Result<(), String> {
    let Some(info) = parts_lookup::build_part_info_from_candidates(&mpn, chosen) else {
        return Ok(());
    };

    let mut sch = SchematicFile::open(sch_path).map_err(|e| e.to_string())?;
    let mut node = sch
        .get_symbol_node(&uuid)
        .ok_or_else(|| "symbol no longer on the schematic".to_string())?;
    parts_lookup::apply_part_info(&mut node, &info);
    set_symbol_property(
        &mut node,
        LAST_CHECKED_PROPERTY,
        &chrono::Utc::now().to_rfc3339(),
    );
    sch.patch_symbol(&uuid, &node);
    sch.save().map_err(|e| e.to_string())
}

/// See `kicad_process`'s own docs — KiCad locks a project as a whole,
/// not per individual `.kicad_sch`. The frontend calls this before
/// `populate_bom` to warn the user, the same way the egui app's
/// `look_up_selected` does with an `rfd::MessageDialog`.
#[tauri::command]
fn check_kicad_open(project_dir: String) -> bool {
    kicad_process::project_open_in_kicad(std::path::Path::new(&project_dir))
}

/// Mirrors `kicad_auto_importer_core::populate_bom::LookupEvent` as a
/// JSON-serializable payload emitted to the frontend under the
/// `populate-bom-event` event name — the Tauri equivalent of the egui
/// app's `mpsc::Receiver<LookupEvent>` polling.
#[derive(serde::Serialize, Clone)]
#[serde(tag = "kind")]
enum PopulateBomEvent {
    Log {
        message: String,
    },
    CurrentItem {
        reference: String,
    },
    RowResult {
        index: usize,
        ok: bool,
        needs_attention: bool,
        skipped: bool,
        summary: String,
    },
    Done,
}

impl From<LookupEvent> for PopulateBomEvent {
    fn from(event: LookupEvent) -> Self {
        match event {
            LookupEvent::Log(message) => PopulateBomEvent::Log { message },
            LookupEvent::CurrentItem(reference) => PopulateBomEvent::CurrentItem { reference },
            LookupEvent::RowResult {
                index,
                ok,
                needs_attention,
                skipped,
                summary,
            } => PopulateBomEvent::RowResult {
                index,
                ok,
                needs_attention,
                skipped,
                summary,
            },
            LookupEvent::Done => PopulateBomEvent::Done,
        }
    }
}

/// Runs `populate_bom::run_lookup_batch` for the rows at
/// `selected_indices` (positions from the *most recent*
/// `list_placed_symbols` call — the schematic is re-walked here to
/// resolve them back to `SelectedRow`s, so this assumes nothing on the
/// schematic reordered placed symbols between the two calls, true for
/// the normal "list, select, click Populate BOM" flow the frontend
/// drives). Spawns the actual batch onto its own OS thread and returns
/// immediately: a plain (non-`async`) `#[tauri::command]` runs its body
/// inline on the same thread that dispatches IPC (and, on Linux, pumps
/// the webview's event loop), so running the batch — network calls and
/// all — directly here would freeze the UI and queue up every
/// `app.emit` until the whole batch finished, instead of delivering
/// progress as it happens. The frontend already tracks completion via
/// the `Done` event, not this call's return, so firing the thread and
/// returning here is enough — same reasoning `crates/app`'s
/// `look_up_selected` uses its own `std::thread::spawn` for.
#[tauri::command]
fn populate_bom(
    app: AppHandle,
    project_dir: String,
    selected_indices: Vec<usize>,
    force_recheck: bool,
    report_path: String,
    kicad_open: bool,
    credentials: PartsCredentials,
) {
    std::thread::spawn(move || {
        let project_dir = std::path::PathBuf::from(&project_dir);
        let project_name = project_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "project".to_string());

        let indices: std::collections::HashSet<usize> = selected_indices.into_iter().collect();
        let selected: Vec<SelectedRow> = schematic::load_schematic_symbols(&project_dir, |_| {})
            .into_iter()
            .enumerate()
            .filter(|(index, _)| indices.contains(index))
            .map(|(index, row)| SelectedRow {
                index,
                reference: row.reference,
                lib_id: row.lib_id,
                sch_path: row.sch_path,
                uuid: row.uuid,
            })
            .collect();

        populate_bom::run_lookup_batch(
            selected,
            project_name,
            std::path::PathBuf::from(report_path),
            force_recheck,
            kicad_open,
            credentials,
            move |event| {
                let _ = app.emit("populate-bom-event", PopulateBomEvent::from(event));
            },
        );
    });
}

/// One row of `list_part_groups` — the frontend's own copy of
/// `kicad_auto_importer_core::bom_pricing::PartGroup`, trimmed to what
/// the Generate BOM preview table shows. `index` matches this call's
/// position, same pairing convention as `PlacedSymbolRow` — see its own
/// docs for why re-listing between calls would break it (here, the
/// pairing only matters for `generate_bom` below, which re-groups the
/// schematic itself rather than trusting a stale list from the
/// frontend, so it's tolerant of this in a way `populate_bom` is not;
/// the index is still exposed so `generate-bom-event`'s `RowResult`
/// can be matched back to a table row without restating every field).
#[derive(serde::Serialize)]
struct PartGroupRow {
    index: usize,
    display_name: String,
    references: Vec<String>,
    per_board_qty: u32,
    is_passive: bool,
}

#[tauri::command]
fn list_part_groups(project_dir: String) -> Vec<PartGroupRow> {
    let project_dir = std::path::PathBuf::from(project_dir);
    let symbols = schematic::load_schematic_symbols(&project_dir, |_| {});
    bom_pricing::group_placed_symbols(&symbols)
        .into_iter()
        .enumerate()
        .map(|(index, group)| PartGroupRow {
            index,
            display_name: group.display_name,
            references: group.references,
            per_board_qty: group.per_board_qty,
            is_passive: group.is_passive,
        })
        .collect()
}

/// Mirrors `kicad_auto_importer_core::generate_bom::BomEvent` as a
/// JSON-serializable payload emitted under the `generate-bom-event`
/// event name — the Generate BOM equivalent of `PopulateBomEvent`.
#[derive(serde::Serialize, Clone)]
#[serde(tag = "kind")]
enum GenerateBomEvent {
    Log {
        message: String,
    },
    CurrentItem {
        display_name: String,
    },
    RowResult {
        index: usize,
        needed_qty: u32,
        outcome: Result<ChosenOffer, String>,
    },
    Done {
        grand_total: f64,
    },
}

impl From<BomEvent> for GenerateBomEvent {
    fn from(event: BomEvent) -> Self {
        match event {
            BomEvent::Log(message) => GenerateBomEvent::Log { message },
            BomEvent::CurrentItem(display_name) => GenerateBomEvent::CurrentItem { display_name },
            BomEvent::RowResult {
                index,
                needed_qty,
                outcome,
            } => GenerateBomEvent::RowResult {
                index,
                needed_qty,
                outcome,
            },
            BomEvent::Done { grand_total } => GenerateBomEvent::Done { grand_total },
        }
    }
}

/// Runs `generate_bom::run_bom_batch` for the whole project — unlike
/// `populate_bom`, there's no row selection to resolve back against a
/// prior `list_part_groups` call: every group found on the schematic
/// right now is priced. Spawns its own OS thread and returns
/// immediately, same reasoning (and same "Done"-event-driven frontend
/// contract) as `populate_bom`.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn generate_bom(
    app: AppHandle,
    project_dir: String,
    board_qty: u32,
    passive_margin_percent: u32,
    force_recheck: bool,
    kicad_open: bool,
    pdf_path: Option<String>,
    xlsx_path: Option<String>,
    credentials: PartsCredentials,
) {
    std::thread::spawn(move || {
        let project_dir = std::path::PathBuf::from(&project_dir);
        let project_name = project_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "project".to_string());

        let symbols = schematic::load_schematic_symbols(&project_dir, |_| {});
        let groups = bom_pricing::group_placed_symbols(&symbols);

        let request = BomBatchRequest {
            groups,
            board_qty: board_qty.max(1),
            passive_margin_percent,
            force_recheck,
            kicad_open,
            project_name,
            pdf_path: pdf_path.map(std::path::PathBuf::from),
            xlsx_path: xlsx_path.map(std::path::PathBuf::from),
            credentials,
        };

        generate_bom::run_bom_batch(request, move |event| {
            let _ = app.emit("generate-bom-event", GenerateBomEvent::from(event));
        });
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "linux")]
    linux_desktop_integration::spawn_registration();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            open_project,
            load_global_settings,
            save_global_settings,
            test_mouser_credentials,
            test_digikey_credentials,
            list_placed_symbols,
            check_kicad_open,
            populate_bom,
            get_scored_candidates,
            apply_vendor_choice,
            list_part_groups,
            generate_bom
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
