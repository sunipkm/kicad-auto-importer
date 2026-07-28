//! "Populate BOM" orchestration — look up manufacturer/vendor info for
//! a caller-selected set of placed schematic symbols and write it back
//! onto each one, with a 24h "already checked" cache (see
//! `RECHECK_THRESHOLD`) shared with `generate_bom`.
//!
//! This is UI-agnostic on purpose: both the egui desktop app and the
//! Tauri `bom-app` frontend drive the exact same `run_lookup_batch`,
//! the only difference being how they turn `LookupEvent`s into visible
//! progress (an `mpsc::Sender` polled every frame for egui, an
//! `app_handle.emit(...)` push for Tauri) — see the phased plan this
//! was moved from `crates/app` under.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::bom_report::{self, ReportRow};
use crate::parts_lookup::{self, PartsCredentials};
use crate::schematic::SchematicFile;
use crate::sexp::SexpNode;
use crate::symbol_importer::{get_symbol_property, set_symbol_property};

/// Below this age, a part's `Last Checked` property is considered fresh
/// enough to skip re-querying Mouser/DigiKey for — see `run_lookup_batch`.
/// Force-re-check bypasses this from the UI. Shared with `generate_bom`'s
/// "Generate BOM" batch, which reads/writes the same property so the two
/// features' caches benefit each other (a part Populate BOM checked this
/// morning doesn't get re-queried by Generate BOM this afternoon, and
/// vice versa).
pub const RECHECK_THRESHOLD: chrono::Duration = chrono::Duration::hours(24);
pub const LAST_CHECKED_PROPERTY: &str = "Last Checked";

pub enum LookupEvent {
    Log(String),
    /// Fires once a row starts being processed (before the staleness
    /// check, so it fires even for a row that ends up skipped) — drives
    /// the "currently looking up…" label next to the progress bar.
    CurrentItem(String),
    RowResult {
        index: usize,
        ok: bool,
        needs_attention: bool,
        skipped: bool,
        summary: String,
    },
    Done,
}

/// A checked row, snapshotted at the moment "Populate BOM" is clicked —
/// owns its own `sch_path`/`uuid` (from `PlacedSymbol`) since rows can
/// come from any schematic file in the hierarchy, not just the root one.
pub struct SelectedRow {
    pub index: usize,
    pub reference: String,
    pub lib_id: String,
    pub sch_path: PathBuf,
    pub uuid: String,
}

/// Groups the selected rows by which schematic file they actually live
/// in and opens each file once (not per-symbol), saving once per file
/// at the end — same batching `library_import::import_symbols` uses for
/// its own destination library, and for the same reason: cheap, and
/// avoids re-reading/re-writing a whole file for every single row in it.
///
/// Before actually querying a vendor, checks that row's own
/// `Last Checked` property (written after every *attempted* lookup,
/// success or failure — see below) and skips it if younger than
/// `RECHECK_THRESHOLD`, unless `force` is set. This is a per-part gate,
/// not a whole-batch one: "Select All" + Populate BOM on day 2 only
/// actually re-queries the parts that have gone stale since day 1,
/// which is the whole point — Mouser/DigiKey rate limits and plain
/// courtesy both argue against re-fetching a part's stock status every
/// time the button is clicked.
///
/// If `kicad_open` is set (see `kicad_process::project_open_in_kicad`,
/// checked by the caller before spawning this batch), every schematic
/// file's final `.save()` is skipped — lookups, the in-memory property
/// patch, and the PDF report all still happen normally, since the
/// vendor API calls are already spent and the report is still useful;
/// only the on-disk write KiCad's own already-open copy would otherwise
/// silently clobber is held back. Because nothing hits disk in that
/// case, the `Last Checked` gate isn't persisted either, so the next
/// run (once KiCad is closed) retries those same parts cleanly instead
/// of treating a blocked write as "checked".
pub fn run_lookup_batch(
    selected: Vec<SelectedRow>,
    project_name: String,
    report_path: PathBuf,
    force: bool,
    kicad_open: bool,
    credentials: PartsCredentials,
    mut on_event: impl FnMut(LookupEvent),
) {
    if kicad_open {
        on_event(LookupEvent::Log(format!(
            "\u{26a0} '{project_name}' appears to be open in KiCad \u{2014} looking up and \
             reporting stock/lifecycle info, but schematic changes will NOT be written back \
             until you close it."
        )));
    }

    // One shared timestamp for the whole batch — a run takes seconds to
    // low minutes, so there's no meaningful staleness difference between
    // rows checked at the start vs. the end of it.
    let now = chrono::Utc::now();

    // Global (cross-project) raw-candidate cache, keyed by search
    // string — see `parts_cache`'s module docs. Loaded once and saved
    // once at the end of the batch (mirrors the schematic-file-per-path
    // batching just below); every row's lookup goes through
    // `parts_lookup::lookup_best_match`, which only touches the network
    // for a search string this cache doesn't already have fresh.
    let mut cache = crate::parts_cache::PartsCache::load();

    // Keyed by the row's original index (not push order) so the report
    // below can be emitted in the same natural-reference order the
    // table itself uses, regardless of which schematic-file group a row
    // happened to land in.
    let mut report_rows: HashMap<usize, ReportRow> = HashMap::new();

    let mut by_file: HashMap<PathBuf, Vec<SelectedRow>> = HashMap::new();
    for row in selected {
        by_file.entry(row.sch_path.clone()).or_default().push(row);
    }

    let mut ok_count = 0usize;
    let mut err_count = 0usize;
    let mut skipped_count = 0usize;

    for (path, rows) in by_file {
        let mut sch = match SchematicFile::open(&path) {
            Ok(sch) => sch,
            Err(exc) => {
                on_event(LookupEvent::Log(format!(
                    "\u{2718} Could not open '{}': {exc}",
                    path.display()
                )));
                for row in &rows {
                    let msg = format!("could not open schematic: {exc}");
                    on_event(LookupEvent::RowResult {
                        index: row.index,
                        ok: false,
                        needs_attention: false,
                        skipped: false,
                        summary: msg.clone(),
                    });
                    report_rows.insert(row.index, report_row(row, Err(msg)));
                    err_count += 1;
                }
                continue;
            }
        };

        for row in &rows {
            on_event(LookupEvent::CurrentItem(row.reference.clone()));
            let Some(mut node) = sch.get_symbol_node(&row.uuid) else {
                on_event(LookupEvent::Log(format!(
                    "\u{2718} '{}': no longer on the schematic, skipped.",
                    row.reference
                )));
                let msg = "no longer on schematic".to_string();
                on_event(LookupEvent::RowResult {
                    index: row.index,
                    ok: false,
                    needs_attention: false,
                    skipped: false,
                    summary: msg.clone(),
                });
                report_rows.insert(row.index, report_row(row, Err(msg)));
                err_count += 1;
                continue;
            };

            if !force {
                if let Some(age) = last_checked_age(&node, now) {
                    if age < RECHECK_THRESHOLD {
                        // Still fresh — no live vendor call needed, but
                        // "Skipped — checked X ago" told the user nothing
                        // about the part itself. Show the same
                        // vendor+price a fresh lookup would have reported
                        // instead, reconstructed from whatever the last
                        // lookup already wrote onto this instance.
                        let cached_summary = parts_lookup::read_cached_part_info(&node)
                            .and_then(|info| parts_lookup::summarize_offers(&info.offers, 1));
                        let (summary, needs_attention) = cached_summary.unwrap_or_else(|| {
                            ("no match found on the last lookup".to_string(), false)
                        });
                        on_event(LookupEvent::Log(format!(
                            "\u{23f8} '{}': {summary} (checked {} ago \u{2014} Force to re-check).",
                            row.reference,
                            format_age(age)
                        )));
                        on_event(LookupEvent::RowResult {
                            index: row.index,
                            ok: true,
                            needs_attention,
                            skipped: true,
                            summary,
                        });
                        skipped_count += 1;
                        continue;
                    }
                }
            }

            let symbol_name = row
                .lib_id
                .split_once(':')
                .map_or(row.lib_id.as_str(), |(_, name)| name);
            let mpn = parts_lookup::resolve_mpn(&node, symbol_name);
            on_event(LookupEvent::Log(format!(
                "Looking up '{}' ({}, as '{mpn}')\u{2026}",
                row.reference, row.lib_id
            )));

            // Populate BOM annotates one reference at a time and has no
            // board-quantity context of its own (that's Generate BOM's
            // job) — scored at a nominal quantity of 1, which is enough
            // to pick between candidates on stock-gated-cheapest terms
            // without pretending to know the real order size.
            match parts_lookup::lookup_best_match(
                &mut cache,
                &credentials,
                &mpn,
                1,
                force,
                now,
                RECHECK_THRESHOLD,
            ) {
                Ok(info) => {
                    let vendors: Vec<&str> =
                        info.offers.iter().map(|o| o.seller.as_str()).collect();
                    for warning in &info.warnings {
                        on_event(LookupEvent::Log(format!(
                            "  \u{26a0} '{}': {warning}",
                            row.reference
                        )));
                    }
                    parts_lookup::apply_part_info(&mut node, &info);
                    set_symbol_property(&mut node, LAST_CHECKED_PROPERTY, &now.to_rfc3339());
                    sch.patch_symbol(&row.uuid, &node);
                    let in_stock = info.in_stock();
                    let lifecycle_concern = info.lifecycle_concern();
                    let mut flags = String::new();
                    if !in_stock {
                        flags.push_str(" (NOT IN STOCK)");
                    }
                    if lifecycle_concern {
                        flags.push_str(" (OBSOLETE/EOL)");
                    }
                    let summary = format!(
                        "{} \u{2014} {}{flags}",
                        info.manufacturer,
                        if vendors.is_empty() {
                            "no Mouser/DigiKey offers found".to_string()
                        } else {
                            vendors.join(", ")
                        },
                    );
                    on_event(LookupEvent::Log(format!(
                        "\u{2714} '{}': {summary}",
                        row.reference
                    )));
                    on_event(LookupEvent::RowResult {
                        index: row.index,
                        ok: true,
                        needs_attention: !in_stock || lifecycle_concern,
                        skipped: false,
                        summary,
                    });
                    report_rows.insert(row.index, report_row(row, Ok(info)));
                    ok_count += 1;
                }
                Err(exc) => {
                    // A failed lookup (e.g. no match found) still counts
                    // as "checked" — without this, a genuinely-not-found
                    // part would get re-queried on every single run
                    // forever, which is exactly the hammering the 24h
                    // gate exists to prevent.
                    set_symbol_property(&mut node, LAST_CHECKED_PROPERTY, &now.to_rfc3339());
                    sch.patch_symbol(&row.uuid, &node);
                    on_event(LookupEvent::Log(format!(
                        "\u{2718} '{}': {exc}",
                        row.reference
                    )));
                    on_event(LookupEvent::RowResult {
                        index: row.index,
                        ok: false,
                        needs_attention: false,
                        skipped: false,
                        summary: exc.to_string(),
                    });
                    report_rows.insert(row.index, report_row(row, Err(exc.to_string())));
                    err_count += 1;
                }
            }
        }

        if sch.has_pending_changes() {
            if kicad_open {
                on_event(LookupEvent::Log(format!(
                    "\u{23f8} Skipped saving '{}' \u{2014} KiCad has this project open.",
                    path.display()
                )));
            } else if let Err(exc) = sch.save() {
                on_event(LookupEvent::Log(format!(
                    "\u{2718} Could not save '{}': {exc}",
                    path.display()
                )));
            }
        }
    }

    if let Err(exc) = cache.save() {
        on_event(LookupEvent::Log(format!(
            "\u{26a0} Could not save the local parts cache: {exc}"
        )));
    }

    on_event(LookupEvent::Log(format!(
        "Done: {ok_count} updated, {err_count} error(s), {skipped_count} skipped (checked recently)."
    )));
    save_stock_report(&report_path, &project_name, report_rows, &mut |msg| {
        on_event(LookupEvent::Log(msg));
    });
    on_event(LookupEvent::Done);
}

/// How long ago `node`'s `Last Checked` property says it was last
/// looked up, or `None` if it has none (or an unparseable one — treated
/// the same as "never checked", i.e. not stale-skipped).
pub fn last_checked_age(
    node: &SexpNode,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<chrono::Duration> {
    let text = get_symbol_property(node, LAST_CHECKED_PROPERTY)?;
    let last_checked = chrono::DateTime::parse_from_rfc3339(&text).ok()?;
    Some(now.signed_duration_since(last_checked))
}

/// e.g. `"45m"`, `"3h"`, `"2d"` — coarse on purpose, this is just for a
/// log line and a Result-column note, not a precise readout.
pub fn format_age(age: chrono::Duration) -> String {
    if age.num_hours() < 1 {
        format!("{}m", age.num_minutes().max(0))
    } else if age.num_hours() < 48 {
        format!("{}h", age.num_hours())
    } else {
        format!("{}d", age.num_days())
    }
}

fn report_row(row: &SelectedRow, outcome: Result<parts_lookup::PartInfo, String>) -> ReportRow {
    let symbol_name = row
        .lib_id
        .split_once(':')
        .map_or(row.lib_id.as_str(), |(_, name)| name);
    ReportRow {
        reference: row.reference.clone(),
        symbol: symbol_name.to_string(),
        outcome,
    }
}

/// Renders the batch's PDF stock report to `report_path` (the location
/// the caller picked, e.g. via a save dialog), in the same natural-
/// reference order the table itself shows. Rows skipped by the 24h
/// staleness gate were never re-checked this run, so they're simply
/// absent here rather than reported on stale data — the log already
/// explains why (see `run_lookup_batch`); if every selected row was
/// skipped, there's nothing meaningful to report at all.
fn save_stock_report(
    report_path: &Path,
    project_name: &str,
    report_rows: HashMap<usize, ReportRow>,
    send_log: &mut impl FnMut(String),
) {
    if report_rows.is_empty() {
        send_log(
            "No report generated \u{2014} every selected part was checked within the last 24h."
                .to_string(),
        );
        return;
    }

    let mut ordered: Vec<(usize, ReportRow)> = report_rows.into_iter().collect();
    ordered.sort_by_key(|(i, _)| *i);
    let rows: Vec<ReportRow> = ordered.into_iter().map(|(_, r)| r).collect();

    let unix_secs = chrono::Utc::now().timestamp().max(0) as u64;
    let generated_at = bom_report::format_utc_timestamp(unix_secs);

    match bom_report::generate(&rows, project_name, &generated_at, report_path) {
        Ok(()) => send_log(format!(
            "Stock report saved to '{}'.",
            report_path.display()
        )),
        Err(exc) => send_log(format!(
            "\u{2718} Could not generate PDF stock report: {exc}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_with_last_checked(rfc3339: &str) -> SexpNode {
        let mut node = crate::sexp::parse(r#"(symbol (property "Reference" "R1"))"#).unwrap();
        set_symbol_property(&mut node, LAST_CHECKED_PROPERTY, rfc3339);
        node
    }

    #[test]
    fn last_checked_age_is_none_when_property_absent() {
        let node = crate::sexp::parse(r#"(symbol (property "Reference" "R1"))"#).unwrap();
        assert!(last_checked_age(&node, chrono::Utc::now()).is_none());
    }

    #[test]
    fn last_checked_age_is_none_when_property_unparseable() {
        let node = node_with_last_checked("not a timestamp");
        assert!(last_checked_age(&node, chrono::Utc::now()).is_none());
    }

    #[test]
    fn last_checked_age_computes_elapsed_duration() {
        let now = chrono::Utc::now();
        let checked_at = now - chrono::Duration::hours(5);
        let node = node_with_last_checked(&checked_at.to_rfc3339());

        let age = last_checked_age(&node, now).unwrap();
        assert_eq!(age.num_hours(), 5);
    }

    #[test]
    fn fresh_last_checked_is_under_the_recheck_threshold() {
        let now = chrono::Utc::now();
        let checked_at = now - chrono::Duration::hours(1);
        let node = node_with_last_checked(&checked_at.to_rfc3339());

        let age = last_checked_age(&node, now).unwrap();
        assert!(age < RECHECK_THRESHOLD);
    }

    #[test]
    fn stale_last_checked_is_over_the_recheck_threshold() {
        let now = chrono::Utc::now();
        let checked_at = now - chrono::Duration::hours(25);
        let node = node_with_last_checked(&checked_at.to_rfc3339());

        let age = last_checked_age(&node, now).unwrap();
        assert!(age >= RECHECK_THRESHOLD);
    }

    #[test]
    fn format_age_uses_minutes_under_an_hour() {
        assert_eq!(format_age(chrono::Duration::minutes(45)), "45m");
    }

    #[test]
    fn format_age_uses_hours_under_two_days() {
        assert_eq!(format_age(chrono::Duration::hours(5)), "5h");
        assert_eq!(format_age(chrono::Duration::hours(47)), "47h");
    }

    #[test]
    fn format_age_uses_days_at_and_beyond_two_days() {
        assert_eq!(format_age(chrono::Duration::hours(48)), "2d");
        assert_eq!(format_age(chrono::Duration::days(5)), "5d");
    }
}
