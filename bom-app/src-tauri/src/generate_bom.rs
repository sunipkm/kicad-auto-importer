//! "Generate BOM" orchestration — group placed schematic symbols into
//! unique purchasable parts, price them against Mouser/DigiKey for a
//! given board quantity, and produce a priced PDF/XLSX report. See
//! `populate_bom` for the sibling "Populate BOM" orchestration this
//! shares its 24h `Last Checked` cache with.
//!
//! UI-agnostic for the same reason `populate_bom::run_lookup_batch` is
//! — the egui desktop app and the Tauri `bom-app` frontend both drive
//! `run_bom_batch`, differing only in how `BomEvent`s reach the screen.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::bom_pricing::{self, ChosenOffer, PartGroup, PricedRow};
use crate::bom_report;
use crate::parts_lookup::{self, PartsCredentials};
use crate::populate_bom::{last_checked_age, LAST_CHECKED_PROPERTY, RECHECK_THRESHOLD};
use kicad_parse::kicad_process;
use kicad_parse::schematic::SchematicFile;
use kicad_parse::symbol_importer::set_symbol_property;

pub enum BomEvent {
    Log(String),
    /// Fires once a group starts being priced — drives the "currently
    /// looking up…" label next to the progress bar.
    CurrentItem(String),
    RowResult {
        index: usize,
        needed_qty: u32,
        outcome: Result<ChosenOffer, String>,
    },
    Done {
        grand_total: f64,
    },
}

/// Bundles `run_bom_batch`'s one-shot batch parameters — keeps the
/// function under clippy's too-many-arguments threshold instead of
/// taking nine loose parameters (same rationale as `bom_report`'s
/// `TextStyle`/`TableContext`).
pub struct BomBatchRequest {
    pub groups: Vec<PartGroup>,
    pub board_qty: u32,
    pub passive_margin_percent: u32,
    pub force_recheck: bool,
    pub project_name: String,
    pub pdf_path: Option<PathBuf>,
    pub xlsx_path: Option<PathBuf>,
    pub credentials: PartsCredentials,
}

/// At most one `parts_lookup::lookup_part_info` call per *group*, not
/// per reference — the entire efficiency point of grouping identical
/// parts first — and even that's skipped when a fresh-enough cached
/// lookup is already sitting on the schematic (see module docs).
pub fn run_bom_batch(
    request: BomBatchRequest,
    mut on_event: impl FnMut(BomEvent),
) -> Vec<PricedRow> {
    let BomBatchRequest {
        groups,
        board_qty,
        passive_margin_percent,
        force_recheck,
        project_name,
        pdf_path,
        xlsx_path,
        credentials,
    } = request;

    // Every schematic file any group's instances live in, opened once
    // (not per-group/per-reference) — mirrors `populate_bom::run_lookup_batch`'s
    // own `by_file` batching, just keyed across the whole run up front
    // since cache reads need it before the per-group loop even starts.
    let mut sch_files: HashMap<PathBuf, SchematicFile> = HashMap::new();
    for group in &groups {
        for (path, _) in &group.instances {
            if let std::collections::hash_map::Entry::Vacant(entry) = sch_files.entry(path.clone())
            {
                match SchematicFile::open(path) {
                    Ok(sch) => {
                        entry.insert(sch);
                    }
                    Err(exc) => {
                        on_event(BomEvent::Log(format!(
                            "\u{2718} Could not open '{}': {exc}",
                            path.display()
                        )));
                    }
                }
            }
        }
    }

    let now = chrono::Utc::now();
    // Same global raw-candidate cache `populate_bom::run_lookup_batch`
    // uses, loaded/saved once per batch here too — a group whose MPN
    // some other project (or Populate BOM, earlier in this same run's
    // history) already looked up recently costs no network call at all.
    let mut cache = crate::parts_cache::PartsCache::load();
    let mut priced_rows: Vec<PricedRow> = Vec::with_capacity(groups.len());
    let mut grand_total = 0.0f64;

    for (index, group) in groups.into_iter().enumerate() {
        on_event(BomEvent::CurrentItem(group.display_name.clone()));
        let raw_needed = group.per_board_qty * board_qty;
        let needed_qty = bom_pricing::margin_adjusted_quantity(
            raw_needed,
            group.is_passive,
            passive_margin_percent,
        );

        // Reuse whichever instance in the group has the freshest cached
        // lookup, if any is still within the recheck window.
        let cached_info = if force_recheck {
            None
        } else {
            group.instances.iter().find_map(|(path, uuid)| {
                let sch = sch_files.get(path)?;
                let node = sch.get_symbol_node(uuid)?;
                let age = last_checked_age(&node, now)?;
                if age < RECHECK_THRESHOLD {
                    parts_lookup::read_cached_part_info(&node)
                } else {
                    None
                }
            })
        };

        let (lookup_result, from_cache): (Result<parts_lookup::PartInfo, String>, bool) =
            match cached_info {
                Some(info) => {
                    on_event(BomEvent::Log(format!(
                        "\u{23f8} '{}': reusing a lookup checked within the last 24h.",
                        group.display_name
                    )));
                    (Ok(info), true)
                }
                None => {
                    on_event(BomEvent::Log(format!(
                        "Looking up '{}' ({} ref(s), need {needed_qty})\u{2026}",
                        group.display_name,
                        group.references.len()
                    )));
                    (
                        parts_lookup::lookup_best_match(
                            &mut cache,
                            &credentials,
                            &group.search_mpn,
                            needed_qty,
                            force_recheck,
                            now,
                            RECHECK_THRESHOLD,
                        )
                        .map_err(|e| e.to_string()),
                        false,
                    )
                }
            };

        // A fresh (non-cached) lookup gets written back onto every
        // instance in the group — success or failure, same reasoning as
        // Populate BOM's own `run_lookup_batch`: a failed lookup still
        // counts as "checked," so a genuinely-not-found part isn't
        // re-queried every single run. This only patches the in-memory
        // node; whether it actually reaches disk is decided per-file,
        // fresh, right before the save below.
        if !from_cache {
            for (path, uuid) in &group.instances {
                let Some(sch) = sch_files.get_mut(path) else {
                    continue;
                };
                let Some(mut node) = sch.get_symbol_node(uuid) else {
                    continue;
                };
                if let Ok(info) = &lookup_result {
                    parts_lookup::apply_part_info(&mut node, info);
                }
                set_symbol_property(&mut node, LAST_CHECKED_PROPERTY, &now.to_rfc3339());
                sch.patch_symbol(uuid, &node);
            }
        }

        let outcome = match lookup_result {
            Ok(info) => {
                for warning in &info.warnings {
                    on_event(BomEvent::Log(format!(
                        "  \u{26a0} '{}': {warning}",
                        group.display_name
                    )));
                }
                match bom_pricing::choose_cheapest_offer(&info, needed_qty) {
                    Some(chosen) => {
                        grand_total += chosen.total_price;
                        let shortfall = chosen.stock_quantity < u64::from(chosen.purchase_qty);
                        let flag = if shortfall { " (NOT ENOUGH STOCK)" } else { "" };
                        on_event(BomEvent::Log(format!(
                            "\u{2714} '{}': buy {} from {} @ ${:.2} = ${:.2}{flag}",
                            group.display_name,
                            chosen.purchase_qty,
                            chosen.seller,
                            chosen.unit_price,
                            chosen.total_price
                        )));
                        Ok(chosen)
                    }
                    None => {
                        let msg = "no priced offers available".to_string();
                        on_event(BomEvent::Log(format!(
                            "\u{2718} '{}': {msg}",
                            group.display_name
                        )));
                        Err(msg)
                    }
                }
            }
            Err(exc) => {
                on_event(BomEvent::Log(format!(
                    "\u{2718} '{}': {exc}",
                    group.display_name
                )));
                Err(exc)
            }
        };

        on_event(BomEvent::RowResult {
            index,
            needed_qty,
            outcome: outcome.clone(),
        });
        priced_rows.push(PricedRow {
            group,
            needed_qty,
            outcome,
        });
    }

    // Rechecked fresh here, per file, right before actually writing —
    // not once upfront for the whole (potentially long) pricing batch,
    // and not for the project as a whole — see `populate_bom::
    // run_lookup_batch`'s identical per-write, per-file check.
    for (path, sch) in &sch_files {
        if !sch.has_pending_changes() {
            continue;
        }
        if kicad_process::file_is_locked(path) {
            on_event(BomEvent::Log(format!(
                "\u{23f8} Skipped saving '{}' \u{2014} KiCad has this file open.",
                path.display()
            )));
        } else if let Err(exc) = sch.save() {
            on_event(BomEvent::Log(format!(
                "\u{2718} Could not save '{}': {exc}",
                path.display()
            )));
        }
    }

    if let Err(exc) = cache.save() {
        on_event(BomEvent::Log(format!(
            "\u{26a0} Could not save the local parts cache: {exc}"
        )));
    }

    let unix_secs = chrono::Utc::now().timestamp().max(0) as u64;
    let generated_at = bom_report::format_utc_timestamp(unix_secs);

    if let Some(path) = &pdf_path {
        match bom_report::generate_priced_bom(
            &priced_rows,
            &project_name,
            board_qty,
            &generated_at,
            path,
        ) {
            Ok(()) => on_event(BomEvent::Log(format!(
                "Priced BOM PDF saved to '{}'.",
                path.display()
            ))),
            Err(exc) => on_event(BomEvent::Log(format!(
                "\u{2718} Could not generate PDF: {exc}"
            ))),
        }
    }
    if let Some(path) = &xlsx_path {
        let xlsx_cols = crate::xlsx_columns::XlsxColumnsConfig::load().visible_columns();
        match bom_report::generate_priced_bom_xlsx(&priced_rows, board_qty, &xlsx_cols, path) {
            Ok(()) => on_event(BomEvent::Log(format!(
                "Priced BOM spreadsheet saved to '{}'.",
                path.display()
            ))),
            Err(exc) => on_event(BomEvent::Log(format!(
                "\u{2718} Could not generate spreadsheet: {exc}"
            ))),
        }
    }

    on_event(BomEvent::Log(format!(
        "Done: estimated total ${grand_total:.2}."
    )));
    on_event(BomEvent::Done { grand_total });
    priced_rows
}
