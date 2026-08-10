//! Groups placed schematic symbols (`kicad_parse::schematic::PlacedSymbol`)
//! into unique purchasable parts and works out what an actual order
//! would cost — the "Generate BOM" feature, distinct from "Populate
//! BOM" (`part_lookup_ui`/`bom_report`'s per-reference stock/lifecycle
//! report): this groups identical parts, multiplies by a board count,
//! pads passive (resistor/capacitor/inductor) quantities with spares,
//! and picks the cheapest way to actually buy each one.
//!
//! Grouping and margin math here are pure/network-free and fully unit
//! tested; the actual vendor lookups (one per group, not one per
//! reference — the entire efficiency point of grouping first) are the
//! caller's job (`crates/app/src/bom_ui.rs`), via
//! `crate::parts_lookup::lookup_part_info`.
#![allow(dead_code)]
use std::collections::HashMap;
use std::path::PathBuf;

use crate::parts_lookup::{cheapest_purchase, PartInfo, StockStatus};
use kicad_parse::schematic::PlacedSymbol;

/// One unique purchasable part, with every reference designator that
/// needs one.
#[derive(Debug, Clone, PartialEq)]
pub struct PartGroup {
    pub group_key: String,
    /// Natural-sorted (relies on the caller — `schematic::load_schematic_symbols`
    /// already returns its rows in that order, and `group_placed_symbols`
    /// preserves input order within a group, so no extra sort is done
    /// here).
    pub references: Vec<String>,
    /// What to search Mouser/DigiKey with.
    pub search_mpn: String,
    /// The MPN, or `"<symbol> <value>"` (e.g. `"R 10k"`) for a
    /// no-explicit-MPN group — see [`group_placed_symbols`].
    pub display_name: String,
    pub is_passive: bool,
    pub per_board_qty: u32,
    /// `(sch_path, uuid)` for every placed instance in this group, same
    /// order as `references` — lets a caller (`crates/app/src/bom_ui.rs`)
    /// read a cached lookup off any instance's own schematic properties
    /// (via `parts_lookup::read_cached_part_info`) and check its
    /// `Last Checked` age, and write a fresh lookup back onto every
    /// instance so Populate BOM's own per-reference cache benefits too.
    pub instances: Vec<(PathBuf, String)>,
    /// Custom field values: field_name → value (populated from first instance)
    pub custom_fields: std::collections::HashMap<String, String>,
}

/// Groups `symbols` (typically `schematic::load_schematic_symbols`'s
/// output) into unique purchasable parts.
///
/// Grouping key: if a placed instance already resolves to an explicit
/// MPN-like property (`resolved_mpn != symbol_name()` — see
/// `parts_lookup::resolve_mpn`'s docs), group by that MPN. Otherwise
/// (the common case for generic `Device:R`/`Device:C`/`Device:L`
/// placements with no MPN set) group by `lib_id|value|footprint`
/// instead: many differently-valued passives share one generic symbol,
/// so grouping by symbol name alone would incorrectly merge, say, a 10k
/// and a 100k resistor into one line.
pub fn group_placed_symbols(symbols: &[PlacedSymbol]) -> Vec<PartGroup> {
    let mut index_by_key: HashMap<String, usize> = HashMap::new();
    let mut groups: Vec<PartGroup> = Vec::new();

    for sym in symbols {
        let has_explicit_mpn = sym.resolved_mpn != sym.symbol_name();
        let key = if has_explicit_mpn {
            sym.resolved_mpn.clone()
        } else {
            format!("{}|{}|{}", sym.lib_id, sym.value, sym.footprint)
        };

        if let Some(&idx) = index_by_key.get(&key) {
            groups[idx].references.push(sym.reference.clone());
            groups[idx].per_board_qty += 1;
            groups[idx]
                .instances
                .push((sym.sch_path.clone(), sym.uuid.clone()));
        } else {
            let display_name = if has_explicit_mpn {
                sym.resolved_mpn.clone()
            } else if sym.value.is_empty() {
                sym.symbol_name().to_string()
            } else {
                format!("{} {}", sym.symbol_name(), sym.value)
            };
            index_by_key.insert(key.clone(), groups.len());
            groups.push(PartGroup {
                group_key: key,
                references: vec![sym.reference.clone()],
                search_mpn: sym.resolved_mpn.clone(),
                display_name,
                is_passive: is_passive_footprint(&sym.footprint),
                per_board_qty: 1,
                instances: vec![(sym.sch_path.clone(), sym.uuid.clone())],
                custom_fields: Default::default(),
            });
        }
    }

    groups
}

/// Whether `footprint` (a `PlacedSymbol::footprint`-shaped string, e.g.
/// `"Resistor_SMD:R_0603_1608Metric"`) names a resistor/capacitor/
/// inductor by KiCad's own footprint-library naming convention — the
/// footprint *name* (after the library nickname's `:`) starting with
/// `R_`/`C_`/`L_`. Checked against the name, not the whole string, so a
/// library nicknamed e.g. `"Resistor_SMD"` doesn't itself trip a
/// false-positive match.
pub fn is_passive_footprint(footprint: &str) -> bool {
    let name = footprint
        .split_once(':')
        .map_or(footprint, |(_, name)| name);
    name.starts_with("R_") || name.starts_with("C_") || name.starts_with("L_")
}

/// Pads `needed` with extra margin for passives — non-passives pass
/// through unchanged. Passives get `needed + max(ceil(needed *
/// extra_percent / 100), extra_minimum)`: a percentage bump for larger
/// quantities, with a flat floor so even a 1-off need still gets spares.
/// When `extra_percent` is 0, no minimum is applied.
pub fn margin_adjusted_quantity(
    needed: u32,
    is_passive: bool,
    extra_percent: u32,
    extra_minimum: u32,
) -> u32 {
    if !is_passive || needed == 0 {
        return needed;
    }
    if extra_percent == 0 {
        return needed;
    }
    let percent_extra = (needed * extra_percent).div_ceil(100);
    needed + percent_extra.max(extra_minimum)
}

/// The winning vendor/quantity for one [`PricedRow`] — see
/// [`choose_cheapest_offer`].
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ChosenOffer {
    pub seller: String,
    pub manufacturer: String,
    pub mpn: String,
    pub sku: String,
    /// The actual quantity to buy — `cheapest_purchase`'s answer for
    /// this specific (winning) offer's own price breaks, which can
    /// differ from another offer's, hence living here rather than as a
    /// single generic field on [`PricedRow`].
    pub purchase_qty: u32,
    pub unit_price: f64,
    pub total_price: f64,
    pub in_stock: bool,
    /// The winning vendor's own on-hand quantity — compare against
    /// `purchase_qty` to tell "in stock" apart from "in stock, but not
    /// enough of it to cover this line" (see [`choose_cheapest_offer`]).
    pub stock_quantity: u64,
    pub lifecycle_concern: bool,
    /// Price breaks for this offer: (min_qty, unit_price) pairs.
    /// Used for Excel export to create dynamic pricing formulas.
    pub price_breaks: Vec<(f64, f64)>,
}

/// One priced BOM line: a [`PartGroup`] plus what looking it up found
/// (or the error it failed with) — what both `bom_report::generate_priced_bom`
/// and `generate_priced_bom_csv` consume.
#[derive(Debug, Clone, PartialEq)]
pub struct PricedRow {
    pub group: PartGroup,
    /// The margin-adjusted quantity that was actually searched for
    /// (`margin_adjusted_quantity(group.per_board_qty * board_qty, ...)`)
    /// — the *minimum* to buy, before `cheapest_purchase` possibly
    /// rounds up further into a cheaper tier.
    pub needed_qty: u32,
    /// `Err` holds the lookup failure's display message.
    pub outcome: Result<ChosenOffer, String>,
}

/// Picks whichever vendor offer in `info` gives the cheapest way to buy
/// at least `needed` units — the per-line "which vendor wins" decision.
/// `None` if no offer carries any price-break data to compute a price
/// from at all (distinct from a lookup failure, which the caller
/// represents as `PricedRow::outcome`'s `Err` case instead).
///
/// Stock-aware: a vendor whose own on-hand quantity can't actually cover
/// the winning purchase quantity is only picked if *no* offer can — a
/// slightly pricier vendor that has enough on hand beats a cheaper one
/// that doesn't, since the cheaper "price" is fictional if you can't
/// actually buy that many. Among offers that tie on "can fulfill it,"
/// price still decides.
///
/// If `preferred_vendor` is set, attempts to use that vendor if it has
/// a priced offer, otherwise falls back to the cheapest available.
pub fn choose_cheapest_offer(info: &PartInfo, needed: u32) -> Option<ChosenOffer> {
    choose_offer_with_preference(info, needed, None)
}

/// Like `choose_cheapest_offer`, but allows specifying a preferred vendor.
/// If the preferred vendor has a priced offer with sufficient stock, it's
/// selected. If it lacks stock, falls back to the cheapest vendor that
/// can fulfill it. If no vendor can fulfill it, picks the preferred vendor
/// if available, otherwise the cheapest.
pub fn choose_offer_with_preference(
    info: &PartInfo,
    needed: u32,
    preferred_vendor: Option<&str>,
) -> Option<ChosenOffer> {
    let candidates: Vec<(
        &crate::parts_lookup::VendorOffer,
        crate::parts_lookup::PurchaseOption,
    )> = info
        .offers
        .iter()
        .filter_map(|offer| {
            cheapest_purchase(&offer.price_breaks, needed).map(|option| (offer, option))
        })
        .collect();

    if candidates.is_empty() {
        return None;
    }

    // Partition into well-stocked and understocked
    let sufficiently_stocked: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|(offer, option)| offer.stock_quantity >= u64::from(option.quantity))
        .collect();

    // If a preferred vendor was specified, check if it's well-stocked
    if let Some(pref_vendor) = preferred_vendor {
        if let Some(&(offer, option)) = sufficiently_stocked
            .iter()
            .find(|(o, _)| o.seller == pref_vendor)
        {
            // Preferred vendor is well-stocked; use it
            return Some(build_chosen_offer(info, offer, option));
        }

        // Preferred vendor is not well-stocked; try other well-stocked vendors
        if !sufficiently_stocked.is_empty() {
            let (offer, option) =
                sufficiently_stocked
                    .iter()
                    .copied()
                    .min_by(|(_, a), (_, b)| {
                        a.total_price
                            .partial_cmp(&b.total_price)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })?;
            return Some(build_chosen_offer(info, offer, option));
        }

        // No well-stocked vendors; try the preferred vendor even if understocked
        if let Some(&(offer, option)) = candidates.iter().find(|(o, _)| o.seller == pref_vendor) {
            return Some(build_chosen_offer(info, offer, option));
        }
    }

    // No preferred vendor, or it wasn't available; use cheapest from well-stocked, or all candidates
    let pool = if sufficiently_stocked.is_empty() {
        &candidates
    } else {
        &sufficiently_stocked
    };

    let (offer, option) = pool.iter().copied().min_by(|(_, a), (_, b)| {
        a.total_price
            .partial_cmp(&b.total_price)
            .unwrap_or(std::cmp::Ordering::Equal)
    })?;

    Some(build_chosen_offer(info, offer, option))
}

fn build_chosen_offer(
    info: &PartInfo,
    offer: &crate::parts_lookup::VendorOffer,
    option: crate::parts_lookup::PurchaseOption,
) -> ChosenOffer {
    ChosenOffer {
        seller: offer.seller.clone(),
        manufacturer: info.manufacturer.clone(),
        mpn: info.mpn.clone(),
        sku: offer.sku.clone(),
        purchase_qty: option.quantity,
        unit_price: option.unit_price,
        total_price: option.total_price,
        in_stock: offer.stock_status == StockStatus::InStock,
        stock_quantity: offer.stock_quantity,
        lifecycle_concern: offer.lifecycle_concern,
        price_breaks: offer.price_breaks.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placed(
        reference: &str,
        lib_id: &str,
        value: &str,
        footprint: &str,
        resolved_mpn: &str,
        dnp: bool,
    ) -> PlacedSymbol {
        PlacedSymbol {
            reference: reference.to_string(),
            lib_id: lib_id.to_string(),
            description: String::new(),
            datasheet: String::new(),
            value: value.to_string(),
            footprint: footprint.to_string(),
            resolved_mpn: resolved_mpn.to_string(),
            dnp,
            sch_path: std::path::PathBuf::new(),
            uuid: reference.to_string(),
        }
    }

    // ── grouping ─────────────────────────────────────────────────────

    #[test]
    fn groups_generic_passives_by_value_and_footprint() {
        let symbols = vec![
            placed(
                "R1",
                "Device:R",
                "10k",
                "Resistor_SMD:R_0603_1608Metric",
                "R",
                false,
            ),
            placed(
                "R2",
                "Device:R",
                "10k",
                "Resistor_SMD:R_0603_1608Metric",
                "R",
                false,
            ),
            placed(
                "R3",
                "Device:R",
                "100k",
                "Resistor_SMD:R_0603_1608Metric",
                "R",
                false,
            ),
        ];
        let groups = group_placed_symbols(&symbols);
        assert_eq!(groups.len(), 2, "10k and 100k must not merge");
        assert_eq!(groups[0].references, vec!["R1", "R2"]);
        assert_eq!(groups[0].per_board_qty, 2);
        assert_eq!(groups[0].display_name, "R 10k");
        assert!(groups[0].is_passive);
        assert_eq!(groups[1].references, vec!["R3"]);
        assert_eq!(groups[1].per_board_qty, 1);
    }

    #[test]
    fn tracks_sch_path_and_uuid_per_instance() {
        let symbols = vec![
            placed(
                "R1",
                "Device:R",
                "10k",
                "Resistor_SMD:R_0603_1608Metric",
                "R",
                false,
            ),
            placed(
                "R2",
                "Device:R",
                "10k",
                "Resistor_SMD:R_0603_1608Metric",
                "R",
                false,
            ),
        ];
        let groups = group_placed_symbols(&symbols);
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].instances,
            vec![
                (std::path::PathBuf::new(), "R1".to_string()),
                (std::path::PathBuf::new(), "R2".to_string()),
            ]
        );
    }

    /// The same identical-value/footprint resistor placed on two
    /// different hierarchical sub-sheets (different `.kicad_sch` files)
    /// must still merge into one group — `run_bom_batch` opens every
    /// distinct `sch_path` across a group's instances, so grouping must
    /// not implicitly assume every instance lives in the same file.
    #[test]
    fn groups_the_same_part_placed_across_different_sheet_files() {
        let mut r1 = placed(
            "R1",
            "Device:R",
            "10k",
            "Resistor_SMD:R_0603_1608Metric",
            "R",
            false,
        );
        r1.sch_path = std::path::PathBuf::from("/project/sheet_a.kicad_sch");
        let mut r2 = placed(
            "R2",
            "Device:R",
            "10k",
            "Resistor_SMD:R_0603_1608Metric",
            "R",
            false,
        );
        r2.sch_path = std::path::PathBuf::from("/project/sheet_b.kicad_sch");

        let groups = group_placed_symbols(&[r1, r2]);
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].instances,
            vec![
                (
                    std::path::PathBuf::from("/project/sheet_a.kicad_sch"),
                    "R1".to_string()
                ),
                (
                    std::path::PathBuf::from("/project/sheet_b.kicad_sch"),
                    "R2".to_string()
                ),
            ]
        );
    }

    #[test]
    fn groups_by_explicit_mpn_regardless_of_value() {
        let symbols = vec![
            placed("U1", "MCU:STM32", "", "QFP:LQFP-48", "STM32F103C8T6", false),
            placed("U2", "MCU:STM32", "", "QFP:LQFP-48", "STM32F103C8T6", false),
        ];
        let groups = group_placed_symbols(&symbols);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].references, vec!["U1", "U2"]);
        assert_eq!(groups[0].display_name, "STM32F103C8T6");
        assert_eq!(groups[0].search_mpn, "STM32F103C8T6");
        assert!(!groups[0].is_passive);
    }

    #[test]
    fn preserves_input_order_across_groups() {
        let symbols = vec![
            placed("C1", "Device:C", "1uF", "Capacitor_SMD:C_0603", "C", true),
            placed("R1", "Device:R", "10k", "Resistor_SMD:R_0603", "R", true),
            placed("C2", "Device:C", "1uF", "Capacitor_SMD:C_0603", "C", true),
        ];
        let groups = group_placed_symbols(&symbols);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].display_name, "C 1uF");
        assert_eq!(groups[0].references, vec!["C1", "C2"]);
        assert_eq!(groups[1].display_name, "R 10k");
    }

    // ── is_passive_footprint ────────────────────────────────────────

    #[test]
    fn recognizes_resistor_capacitor_inductor_footprint_prefixes() {
        assert!(is_passive_footprint("Resistor_SMD:R_0603_1608Metric"));
        assert!(is_passive_footprint("Capacitor_SMD:C_0603_1608Metric"));
        assert!(is_passive_footprint("Inductor_SMD:L_0603_1608Metric"));
    }

    #[test]
    fn does_not_match_unrelated_or_similarly_named_footprints() {
        assert!(!is_passive_footprint("Package_SO:SOIC-8_3.9x4.9mm"));
        assert!(!is_passive_footprint(""));
        // "LED_" starts with 'L' but not the "L_" prefix itself.
        assert!(!is_passive_footprint("LED_SMD:LED_0603"));
    }

    #[test]
    fn works_without_a_library_nickname_prefix() {
        assert!(is_passive_footprint("R_0603_1608Metric"));
    }

    // ── margin_adjusted_quantity ─────────────────────────────────────

    #[test]
    fn non_passives_are_never_padded() {
        assert_eq!(margin_adjusted_quantity(6, false, 20, 5), 6);
        assert_eq!(margin_adjusted_quantity(1000, false, 50, 5), 1000);
    }

    #[test]
    fn small_passive_quantities_get_the_flat_minimum() {
        // ceil(6 * 20 / 100) = 2, below the 5-piece floor.
        assert_eq!(margin_adjusted_quantity(6, true, 20, 5), 11);
    }

    #[test]
    fn large_passive_quantities_get_the_percentage() {
        // ceil(100 * 20 / 100) = 20, above the floor.
        assert_eq!(margin_adjusted_quantity(100, true, 20, 5), 120);
    }

    #[test]
    fn zero_needed_stays_zero_even_for_passives() {
        assert_eq!(margin_adjusted_quantity(0, true, 20, 5), 0);
    }

    // ── choose_cheapest_offer ────────────────────────────────────────

    fn info_with_offers(offers: Vec<crate::parts_lookup::VendorOffer>) -> PartInfo {
        PartInfo {
            manufacturer: "Texas Instruments".to_string(),
            mpn: "LM358P".to_string(),
            description: String::new(),
            offers,
            warnings: Vec::new(),
        }
    }

    /// Plenty of stock by default — tests that care about the
    /// stock-shortfall behavior use `offer_with_stock` directly instead.
    fn offer(seller: &str, breaks: Vec<(f64, f64)>) -> crate::parts_lookup::VendorOffer {
        offer_with_stock(seller, breaks, 1_000_000)
    }

    fn offer_with_stock(
        seller: &str,
        breaks: Vec<(f64, f64)>,
        stock_quantity: u64,
    ) -> crate::parts_lookup::VendorOffer {
        crate::parts_lookup::VendorOffer {
            seller: seller.to_string(),
            url: String::new(),
            sku: format!("{seller}-SKU"),
            price_summary: String::new(),
            stock_status: if stock_quantity > 0 {
                StockStatus::InStock
            } else {
                StockStatus::OutOfStock
            },
            stock_summary: String::new(),
            stock_quantity,
            lifecycle_summary: "Active".to_string(),
            lifecycle_concern: false,
            suggested_replacement: String::new(),
            price_breaks: breaks,
        }
    }

    #[test]
    fn picks_the_cheapest_vendor_across_offers() {
        let info = info_with_offers(vec![
            offer("Mouser", vec![(1.0, 0.20)]),
            offer("DigiKey", vec![(1.0, 0.15)]),
        ]);
        let chosen = choose_cheapest_offer(&info, 10).unwrap();
        assert_eq!(chosen.seller, "DigiKey");
        assert_eq!(chosen.purchase_qty, 10);
        assert!((chosen.total_price - 1.50).abs() < 1e-9);
        assert_eq!(chosen.manufacturer, "Texas Instruments");
        assert_eq!(chosen.mpn, "LM358P");
    }

    #[test]
    fn no_priced_offers_is_none() {
        let info = info_with_offers(vec![offer("Mouser", Vec::new())]);
        assert!(choose_cheapest_offer(&info, 10).is_none());
    }

    #[test]
    fn a_cheaper_but_understocked_vendor_loses_to_one_that_can_fulfill_it() {
        let info = info_with_offers(vec![
            // Mouser is cheaper per unit but only has 3 on hand — can't
            // cover a need of 10.
            offer_with_stock("Mouser", vec![(1.0, 0.10)], 3),
            offer_with_stock("DigiKey", vec![(1.0, 0.15)], 500),
        ]);
        let chosen = choose_cheapest_offer(&info, 10).unwrap();
        assert_eq!(chosen.seller, "DigiKey");
        assert_eq!(chosen.stock_quantity, 500);
    }

    #[test]
    fn cheapest_still_wins_among_offers_that_can_all_fulfill_it() {
        let info = info_with_offers(vec![
            offer_with_stock("Mouser", vec![(1.0, 0.10)], 500),
            offer_with_stock("DigiKey", vec![(1.0, 0.15)], 500),
        ]);
        let chosen = choose_cheapest_offer(&info, 10).unwrap();
        assert_eq!(chosen.seller, "Mouser");
    }

    #[test]
    fn falls_back_to_cheapest_when_no_vendor_has_enough_stock() {
        let info = info_with_offers(vec![
            offer_with_stock("Mouser", vec![(1.0, 0.20)], 2),
            offer_with_stock("DigiKey", vec![(1.0, 0.15)], 1),
        ]);
        let chosen = choose_cheapest_offer(&info, 10).unwrap();
        assert_eq!(chosen.seller, "DigiKey");
        assert_eq!(chosen.stock_quantity, 1);
    }

    // ── vendor preference ────────────────────────────────────────────

    #[test]
    fn prefers_specified_vendor_when_well_stocked() {
        let info = info_with_offers(vec![
            offer_with_stock("Mouser", vec![(1.0, 0.10)], 100),
            offer_with_stock("DigiKey", vec![(1.0, 0.15)], 100),
        ]);
        let chosen = choose_offer_with_preference(&info, 10, Some("Mouser")).unwrap();
        assert_eq!(chosen.seller, "Mouser");
    }

    #[test]
    fn falls_back_when_preferred_vendor_lacks_stock() {
        let info = info_with_offers(vec![
            offer_with_stock("Mouser", vec![(1.0, 0.10)], 3),
            offer_with_stock("DigiKey", vec![(1.0, 0.15)], 100),
        ]);
        let chosen = choose_offer_with_preference(&info, 10, Some("Mouser")).unwrap();
        assert_eq!(chosen.seller, "DigiKey");
    }

    #[test]
    fn uses_preferred_vendor_even_without_stock_as_fallback() {
        let info = info_with_offers(vec![
            offer_with_stock("Mouser", vec![(1.0, 0.10)], 3),
            offer_with_stock("DigiKey", vec![(1.0, 0.15)], 2),
        ]);
        let chosen = choose_offer_with_preference(&info, 10, Some("Mouser")).unwrap();
        assert_eq!(chosen.seller, "Mouser");
    }

    #[test]
    fn ignores_preference_when_vendor_unavailable() {
        let info = info_with_offers(vec![offer_with_stock("DigiKey", vec![(1.0, 0.15)], 100)]);
        let chosen = choose_offer_with_preference(&info, 10, Some("Mouser")).unwrap();
        assert_eq!(chosen.seller, "DigiKey");
    }

    #[test]
    fn none_preference_uses_cheapest_like_default() {
        let info = info_with_offers(vec![
            offer_with_stock("Mouser", vec![(1.0, 0.20)], 100),
            offer_with_stock("DigiKey", vec![(1.0, 0.15)], 100),
        ]);
        let chosen = choose_offer_with_preference(&info, 10, None).unwrap();
        assert_eq!(chosen.seller, "DigiKey");
    }
}
