//! Merges the Mouser (`crate::mouser`) and DigiKey (`crate::digikey`)
//! vendor clients into the single API `crates/app`'s "Populate BOM"
//! flow actually talks to, and writes the merged result onto a KiCad
//! symbol as new properties (`Mfr`, `Mfr #`, `<Vendor>`, `<Vendor> #`,
//! `<Vendor> Qty/Price`).
//!
//! Replaces the earlier Octopart/Nexar integration (a single paid
//! aggregator API) with two free-to-register direct vendor APIs.
//! Unlike Octopart, which answered one query with data from up to three
//! distributors at once, each vendor here is queried independently, so
//! a configured-but-failing vendor (rate-limited, no match, bad
//! credentials) shouldn't take down a lookup that another configured
//! vendor *did* succeed at — see [`combine_results`] for exactly how a
//! partial success is represented.

use crate::digikey::{self, DigikeyCredentials, DigikeyError, DigikeyPart};
use crate::mouser::{self, MouserCredentials, MouserError, MouserPart};
use crate::sexp::{Child, SexpNode};
use crate::symbol_importer::{get_symbol_property, set_symbol_property};

/// A handful of the cheapest-to-priciest quantity breaks, packed into
/// one string — see [`format_price_breaks`]. KiCad symbol properties
/// are single-line strings, so this is as much of a price curve as one
/// field can hold.
const MAX_PRICE_BREAKS: usize = 4;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PartsCredentials {
    pub mouser_api_key: String,
    pub digikey_client_id: String,
    pub digikey_client_secret: String,
}

/// Whether a vendor reported a part as orderable right now — deliberately
/// binary (no separate "unknown" state): a vendor that answered the
/// lookup at all always states a quantity/availability one way or the
/// other, and a vendor that *didn't* answer isn't represented as an
/// offer in the first place (see [`VendorOffer`]), so "no offer" already
/// means "not confirmed in stock" without needing a third state here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StockStatus {
    InStock,
    OutOfStock,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VendorOffer {
    /// Display label: "Mouser" or "DigiKey".
    pub seller: String,
    pub url: String,
    pub sku: String,
    /// e.g. `"1:$1.23 | 10:$1.05 | 100:$0.89"`.
    pub price_summary: String,
    pub stock_status: StockStatus,
    /// The vendor's own availability text, e.g. `"1,934 In Stock"` or
    /// `"0 In Stock"` — kept verbatim (rather than reduced to just a
    /// number) since it's the most legible thing to put in front of a
    /// human on the BOM report.
    pub stock_summary: String,
    /// The vendor's on-hand quantity as a plain number (0 if unknown or
    /// out of stock) — `stock_status` alone only says "orderable right
    /// now or not," which isn't enough to tell a vendor with 3 in stock
    /// apart from one with 30,000 when deciding whether it can actually
    /// fulfill a specific purchase quantity (see
    /// `bom_pricing::choose_cheapest_offer`).
    pub stock_quantity: u64,
    /// The vendor's own lifecycle status text, e.g. `"Active"`,
    /// `"Obsolete"`, `"Not Recommended for New Designs"` — `"Unknown"`
    /// if the vendor didn't report one for this part (common; both
    /// vendors leave it blank/null far more often than they set it,
    /// even for parts confirmed in stock). Kept verbatim rather than
    /// collapsed to a boolean, same reasoning as `stock_summary`.
    pub lifecycle_summary: String,
    /// True only when `lifecycle_summary` matches a known
    /// obsolete/EOL/NRND-type keyword (see [`is_lifecycle_concern`]) —
    /// unlike stock, an *unknown* lifecycle is not itself a red flag
    /// (most catalog entries simply don't carry this field), so this is
    /// deliberately not derived from "did the vendor report anything."
    pub lifecycle_concern: bool,
    /// A vendor-suggested replacement part number, when the vendor
    /// offers one (Mouser's `SuggestedReplacement` field, most relevant
    /// alongside an `"Obsolete"`/`"NRND"` lifecycle status) — empty
    /// when none was given. DigiKey has no equivalent field, so this is
    /// always empty for a DigiKey offer.
    pub suggested_replacement: String,
    /// The vendor's raw `(quantity, unit price)` break pairs, unsorted
    /// and uncapped — unlike `price_summary` (a human-readable string,
    /// capped at [`MAX_PRICE_BREAKS`] entries), this keeps full
    /// precision for [`cheapest_purchase`]'s bracket-optimization math.
    pub price_breaks: Vec<(f64, f64)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PartInfo {
    pub manufacturer: String,
    pub mpn: String,
    /// The richer (see [`richer_description`]) of whichever matched
    /// vendors reported a description — empty if neither did.
    pub description: String,
    pub offers: Vec<VendorOffer>,
    /// A message per *configured* vendor that failed (e.g. `"DigiKey:
    /// no match found for 'LM358'"`), even though the overall lookup
    /// still succeeded because another vendor came back with data.
    /// Empty when every configured vendor succeeded (or only one vendor
    /// was configured and it succeeded).
    pub warnings: Vec<String>,
}

impl PartInfo {
    /// True if *any* matched vendor reports the part as currently in
    /// stock — what the "Populate BOM" table/log and the PDF stock
    /// report both use to decide whether a part needs flagging.
    pub fn in_stock(&self) -> bool {
        self.offers
            .iter()
            .any(|o| o.stock_status == StockStatus::InStock)
    }

    /// True if *any* matched vendor flags the part as
    /// obsolete/EOL/NRND/discontinued/last-time-buy.
    pub fn lifecycle_concern(&self) -> bool {
        self.offers.iter().any(|o| o.lifecycle_concern)
    }
}

/// Case-insensitive keyword match against a vendor's own lifecycle
/// status text — shared by `mouser`/`digikey` rather than hard-coding
/// each vendor's exact enum of status strings (neither vendor documents
/// a closed set, e.g. DigiKey alone has been seen using "Obsolete",
/// "Discontinued at Digi-Key", and "Not Recommended for New Designs"
/// for what's functionally the same warning).
const LIFECYCLE_CONCERN_KEYWORDS: &[&str] = &[
    "obsolete",
    "discontinued",
    "end of life",
    "eol",
    "nrnd",
    "not recommended",
    "last time buy",
    "ltb",
];

pub(crate) fn is_lifecycle_concern(status_text: &str) -> bool {
    let lower = status_text.to_lowercase();
    LIFECYCLE_CONCERN_KEYWORDS
        .iter()
        .any(|keyword| lower.contains(keyword))
}

/// Picks whichever of two description strings carries more actual
/// content, measured by non-whitespace character count rather than raw
/// length — so a padded-with-spaces string can't out-rank a denser one.
/// Ties (including both empty) keep `a`. Shared by two call sites:
/// DigiKey's own `DetailedDescription` vs. `ProductDescription`
/// (`digikey::parse_search_response`), and the final pick between
/// Mouser's and DigiKey's descriptions (`combine_results` below) — same
/// rule both times, so neither vendor structurally has the edge.
pub(crate) fn richer_description<'a>(a: &'a str, b: &'a str) -> &'a str {
    let non_whitespace_count = |s: &str| s.chars().filter(|c| !c.is_whitespace()).count();
    if non_whitespace_count(b) > non_whitespace_count(a) {
        b
    } else {
        a
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PartsLookupError {
    #[error(
        "no Mouser API key or DigiKey Client ID/Secret is set — add credentials for at least one vendor first"
    )]
    MissingCredentials,
    #[error("{0}")]
    AllVendorsFailed(String),
}

/// The one function callers (the `part_lookup_ui` background thread)
/// need: queries every vendor with credentials configured, merging the
/// results — see [`combine_results`].
pub fn lookup_part_info(creds: &PartsCredentials, mpn: &str) -> Result<PartInfo, PartsLookupError> {
    let mouser_result = if creds.mouser_api_key.trim().is_empty() {
        None
    } else {
        Some(mouser::lookup_part(
            &MouserCredentials {
                api_key: creds.mouser_api_key.clone(),
            },
            mpn,
        ))
    };
    let digikey_result = if creds.digikey_client_id.trim().is_empty()
        || creds.digikey_client_secret.trim().is_empty()
    {
        None
    } else {
        Some(digikey::lookup_part(
            &DigikeyCredentials {
                client_id: creds.digikey_client_id.clone(),
                client_secret: creds.digikey_client_secret.clone(),
            },
            mpn,
        ))
    };
    combine_results(mpn, mouser_result, digikey_result)
}

/// Merges up to two independent vendor lookups into one [`PartInfo`].
/// `None` means that vendor wasn't configured at all (skipped
/// silently, not a failure); `Some(Err(_))` means it was configured but
/// the lookup itself failed (recorded as a warning, not fatal on its
/// own). Fails outright only if no vendor was configured
/// ([`PartsLookupError::MissingCredentials`]) or every *attempted*
/// vendor failed ([`PartsLookupError::AllVendorsFailed`]).
///
/// Pulled out as a pure function (no network calls) specifically so
/// this merge logic — the actual new behavior in this module — is
/// unit-testable without mocking HTTP.
fn combine_results(
    mpn: &str,
    mouser: Option<Result<MouserPart, MouserError>>,
    digikey: Option<Result<DigikeyPart, DigikeyError>>,
) -> Result<PartInfo, PartsLookupError> {
    if mouser.is_none() && digikey.is_none() {
        return Err(PartsLookupError::MissingCredentials);
    }

    let mut manufacturer = String::new();
    let mut resolved_mpn = mpn.to_string();
    let mut description = String::new();
    let mut offers = Vec::new();
    let mut warnings = Vec::new();

    if let Some(result) = mouser {
        match result {
            Ok(part) => {
                if manufacturer.is_empty() {
                    manufacturer = part.manufacturer;
                }
                if !part.mpn.is_empty() {
                    resolved_mpn = part.mpn;
                }
                description = richer_description(&description, &part.description).to_string();
                offers.push(VendorOffer {
                    seller: "Mouser".to_string(),
                    url: part.url,
                    sku: part.sku,
                    price_summary: part.price_summary,
                    stock_status: part.stock_status,
                    stock_summary: part.stock_summary,
                    stock_quantity: part.stock_quantity,
                    lifecycle_summary: part.lifecycle_summary,
                    lifecycle_concern: part.lifecycle_concern,
                    suggested_replacement: part.suggested_replacement,
                    price_breaks: part.price_breaks,
                });
            }
            Err(exc) => warnings.push(format!("Mouser: {exc}")),
        }
    }

    if let Some(result) = digikey {
        match result {
            Ok(part) => {
                if manufacturer.is_empty() {
                    manufacturer = part.manufacturer;
                }
                if !part.mpn.is_empty() {
                    resolved_mpn = part.mpn;
                }
                description = richer_description(&description, &part.description).to_string();
                offers.push(VendorOffer {
                    seller: "DigiKey".to_string(),
                    url: part.url,
                    sku: part.sku,
                    price_summary: part.price_summary,
                    stock_status: part.stock_status,
                    stock_summary: part.stock_summary,
                    stock_quantity: part.stock_quantity,
                    lifecycle_summary: part.lifecycle_summary,
                    lifecycle_concern: part.lifecycle_concern,
                    suggested_replacement: String::new(),
                    price_breaks: part.price_breaks,
                });
            }
            Err(exc) => warnings.push(format!("DigiKey: {exc}")),
        }
    }

    if offers.is_empty() {
        return Err(PartsLookupError::AllVendorsFailed(warnings.join("; ")));
    }

    Ok(PartInfo {
        manufacturer,
        mpn: resolved_mpn,
        description,
        offers,
        warnings,
    })
}

/// Writes `info` onto `sym_node` as `Mfr`/`Mfr #`/`Vendor Description`
/// plus, per matched vendor, `<Vendor>` (URL) / `<Vendor> #` (SKU) /
/// `<Vendor> Qty/Price` / `<Vendor> Stock` / `<Vendor> Lifecycle` (and
/// `<Vendor> Replacement` when the vendor suggested one). Re-running a
/// lookup overwrites these in place — see `set_symbol_property`'s docs
/// for why that's the right default here.
///
/// Also writes two properties not meant for human eyes —
/// `<Vendor> Price Breaks (Raw)` and `<Vendor> Stock Qty` — full-
/// precision, uncapped versions of `price_summary`/`stock_summary`. See
/// [`read_cached_part_info`], which reads them back: this is what lets
/// a later "Generate BOM" run reuse a still-fresh lookup (by whichever
/// tool did it — Populate BOM or Generate BOM) for its own bracket-
/// optimization math without losing precision to the display string's
/// rounding/`MAX_PRICE_BREAKS` cap.
///
/// `Vendor Description` is deliberately a new property, not a rewrite of
/// the symbol's own pre-existing `Description` — that one is whatever
/// the KiCad library symbol already carries (and is what the Populate
/// BOM table shows), and clobbering it with vendor catalog text would
/// silently change something no other part of this write-back touches.
pub fn apply_part_info(sym_node: &mut SexpNode, info: &PartInfo) {
    set_symbol_property(sym_node, "Mfr", &info.manufacturer);
    set_symbol_property(sym_node, "Mfr #", &info.mpn);
    if !info.description.is_empty() {
        set_symbol_property(sym_node, "Vendor Description", &info.description);
    }
    for offer in &info.offers {
        set_symbol_property(sym_node, &offer.seller, &offer.url);
        set_symbol_property(sym_node, &format!("{} #", offer.seller), &offer.sku);
        set_symbol_property(
            sym_node,
            &format!("{} Qty/Price", offer.seller),
            &offer.price_summary,
        );
        set_symbol_property(
            sym_node,
            &raw_price_breaks_property(&offer.seller),
            &format_raw_price_breaks(&offer.price_breaks),
        );
        set_symbol_property(
            sym_node,
            &format!("{} Stock", offer.seller),
            &offer.stock_summary,
        );
        set_symbol_property(
            sym_node,
            &stock_quantity_property(&offer.seller),
            &offer.stock_quantity.to_string(),
        );
        set_symbol_property(
            sym_node,
            &format!("{} Lifecycle", offer.seller),
            &offer.lifecycle_summary,
        );
        if !offer.suggested_replacement.is_empty() {
            set_symbol_property(
                sym_node,
                &format!("{} Replacement", offer.seller),
                &offer.suggested_replacement,
            );
        }
    }
}

/// Reconstructs a [`PartInfo`] from whatever [`apply_part_info`] last
/// wrote onto `sym_node` — the read side of the same cache, used to
/// skip a fresh Mouser/DigiKey lookup when the instance's own
/// `Last Checked` property (`crates/app/src/part_lookup_ui.rs`) is
/// still within the recheck window. `None` if `sym_node` was never
/// looked up (no `Mfr #`), has no vendor offer at all, or every offer it
/// does have carries no price-break data — the last case covers a
/// `Last Checked` property written before the `<Vendor> Price Breaks
/// (Raw)` property existed (an older "Populate BOM" run from before this
/// cache was added): trusting that stale entry would make "Generate
/// BOM" report every such part as unpriceable for a full 24h instead of
/// just doing the fresh lookup this cache miss should trigger.
pub fn read_cached_part_info(sym_node: &SexpNode) -> Option<PartInfo> {
    let mpn = get_symbol_property(sym_node, "Mfr #").filter(|s| !s.is_empty())?;
    let manufacturer = get_symbol_property(sym_node, "Mfr").unwrap_or_default();
    let description = get_symbol_property(sym_node, "Vendor Description").unwrap_or_default();

    let mut offers = Vec::new();
    for seller in ["Mouser", "DigiKey"] {
        let Some(sku) =
            get_symbol_property(sym_node, &format!("{seller} #")).filter(|s| !s.is_empty())
        else {
            continue;
        };
        let stock_quantity = get_symbol_property(sym_node, &stock_quantity_property(seller))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let lifecycle_summary = get_symbol_property(sym_node, &format!("{seller} Lifecycle"))
            .unwrap_or_else(|| "Unknown".to_string());
        let price_breaks = get_symbol_property(sym_node, &raw_price_breaks_property(seller))
            .map(|raw| parse_raw_price_breaks(&raw))
            .unwrap_or_default();
        offers.push(VendorOffer {
            seller: seller.to_string(),
            url: get_symbol_property(sym_node, seller).unwrap_or_default(),
            sku,
            price_summary: get_symbol_property(sym_node, &format!("{seller} Qty/Price"))
                .unwrap_or_default(),
            stock_status: if stock_quantity > 0 {
                StockStatus::InStock
            } else {
                StockStatus::OutOfStock
            },
            stock_summary: get_symbol_property(sym_node, &format!("{seller} Stock"))
                .unwrap_or_default(),
            stock_quantity,
            lifecycle_concern: is_lifecycle_concern(&lifecycle_summary),
            lifecycle_summary,
            suggested_replacement: get_symbol_property(sym_node, &format!("{seller} Replacement"))
                .unwrap_or_default(),
            price_breaks,
        });
    }
    if offers.is_empty() || offers.iter().all(|o| o.price_breaks.is_empty()) {
        return None;
    }

    Some(PartInfo {
        manufacturer,
        mpn,
        description,
        offers,
        warnings: Vec::new(),
    })
}

fn raw_price_breaks_property(seller: &str) -> String {
    format!("{seller} Price Breaks (Raw)")
}

fn stock_quantity_property(seller: &str) -> String {
    format!("{seller} Stock Qty")
}

/// `"1:0.55|10:0.41|100:0.32"` — unlike [`format_price_breaks`], not
/// meant for a human: full float precision, unsorted, uncapped. See
/// [`read_cached_part_info`].
fn format_raw_price_breaks(breaks: &[(f64, f64)]) -> String {
    breaks
        .iter()
        .map(|(qty, price)| format!("{qty}:{price}"))
        .collect::<Vec<_>>()
        .join("|")
}

fn parse_raw_price_breaks(raw: &str) -> Vec<(f64, f64)> {
    raw.split('|')
        .filter_map(|entry| {
            let (qty, price) = entry.split_once(':')?;
            Some((qty.trim().parse().ok()?, price.trim().parse().ok()?))
        })
        .collect()
}

/// Candidate property names (matched case-insensitively) that a symbol
/// might already carry a manufacturer part number under, checked in
/// priority order before falling back to the symbol's own name — see
/// [`resolve_mpn`].
const MPN_PROPERTY_CANDIDATES: &[&str] = &[
    "MPN",
    "Manufacturer Part Number",
    "Mfr#",
    "Mfr #",
    "Part Number",
];

/// What to search Mouser/DigiKey for: an existing MPN-like property on
/// `sym_node` if it has one (vendor-exported symbols, e.g. from
/// UltraLibrarian, sometimes already carry one under a different name
/// than this app writes to `Mfr #`), otherwise `symbol_name` itself —
/// vendor-exported symbols are typically already named after the MPN.
pub fn resolve_mpn(sym_node: &SexpNode, symbol_name: &str) -> String {
    for prop in sym_node.find_all("property") {
        let Some(Child::Atom(key)) = prop.children.first() else {
            continue;
        };
        let is_mpn_candidate = MPN_PROPERTY_CANDIDATES
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(key.text()));
        if !is_mpn_candidate {
            continue;
        }
        if let Some(Child::Atom(value)) = prop.children.get(1) {
            let value = value.text().trim();
            if !value.is_empty() {
                return value.to_string();
            }
        }
    }
    symbol_name.to_string()
}

/// Sorted-by-quantity, capped-at-[`MAX_PRICE_BREAKS`], compact price
/// string shared by the `mouser`/`digikey` clients — each reduces its
/// own vendor-specific price-break shape down to `(quantity, price)`
/// pairs before calling this.
pub(crate) fn format_price_breaks(breaks: &[(f64, f64)]) -> String {
    let mut sorted: Vec<&(f64, f64)> = breaks.iter().collect();
    sorted.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    sorted
        .into_iter()
        .take(MAX_PRICE_BREAKS)
        .map(|(qty, price)| format!("{}:${:.2}", *qty as i64, price))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// One candidate way to buy at least some required quantity — see
/// [`cheapest_purchase`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PurchaseOption {
    pub quantity: u32,
    pub unit_price: f64,
    pub total_price: f64,
}

/// The cheapest way to buy *at least* `needed` units, given a vendor's
/// raw `(break quantity, unit price)` pairs (any order, e.g.
/// `VendorOffer::price_breaks`).
///
/// Considers not just buying exactly `needed`, but also buying up to
/// any larger break quantity — crossing into a lower per-unit tier can
/// cost less overall than buying only what's strictly required (needing
/// 6 when breaks are `1:$0.10`/`10:$0.05`: buying 10 for $0.50 beats
/// buying 6 for $0.60). Within a tier, cost only grows with quantity, so
/// the cheapest point in any tier at or above `needed` is always either
/// `needed` itself or that tier's own starting break quantity — no
/// other candidate quantity can do better, so those are the only ones
/// checked.
///
/// Returns `None` if `breaks` is empty or `needed` is 0.
pub fn cheapest_purchase(breaks: &[(f64, f64)], needed: u32) -> Option<PurchaseOption> {
    if breaks.is_empty() || needed == 0 {
        return None;
    }

    let mut sorted: Vec<(u32, f64)> = breaks
        .iter()
        .map(|&(qty, price)| (qty.max(0.0) as u32, price))
        .collect();
    sorted.sort_by_key(|&(qty, _)| qty);

    // The unit price that applies when buying exactly `qty`: the price
    // of the largest break quantity not exceeding `qty` ("buy at least
    // this many, get this price" is how distributor breaks work).
    let price_for = |qty: u32| -> Option<f64> {
        sorted
            .iter()
            .rev()
            .find(|&&(break_qty, _)| break_qty <= qty)
            .map(|&(_, price)| price)
    };

    let mut candidates = vec![needed];
    candidates.extend(
        sorted
            .iter()
            .map(|&(qty, _)| qty)
            .filter(|&qty| qty > needed),
    );

    candidates
        .into_iter()
        .filter_map(|qty| {
            price_for(qty).map(|unit_price| PurchaseOption {
                quantity: qty,
                unit_price,
                total_price: unit_price * qty as f64,
            })
        })
        .min_by(|a, b| {
            a.total_price
                .partial_cmp(&b.total_price)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mouser_part(seller_suffix: &str) -> MouserPart {
        MouserPart {
            manufacturer: "Texas Instruments".to_string(),
            mpn: "LM358P".to_string(),
            description: "Op Amps Dual Op Amp".to_string(),
            url: format!("https://mouser.com/{seller_suffix}"),
            sku: "595-LM358P".to_string(),
            price_summary: "1:$0.55".to_string(),
            stock_status: StockStatus::InStock,
            stock_summary: "1,934 In Stock".to_string(),
            stock_quantity: 1934,
            lifecycle_summary: "Active".to_string(),
            lifecycle_concern: false,
            suggested_replacement: String::new(),
            price_breaks: vec![(1.0, 0.55)],
        }
    }

    fn digikey_part() -> DigikeyPart {
        DigikeyPart {
            manufacturer: "Texas Instruments".to_string(),
            mpn: "LM358P".to_string(),
            description: "Standard (General Purpose) Amplifier 2 Circuit 8-PDIP".to_string(),
            url: "https://digikey.com/lm358p".to_string(),
            sku: "296-1395-5-ND".to_string(),
            price_summary: "1:$0.60".to_string(),
            stock_status: StockStatus::InStock,
            stock_summary: "2,500 in stock".to_string(),
            stock_quantity: 2500,
            lifecycle_summary: "Active".to_string(),
            lifecycle_concern: false,
            price_breaks: vec![(1.0, 0.60)],
        }
    }

    #[test]
    fn both_vendors_succeeding_merges_both_offers_with_no_warnings() {
        let info = combine_results(
            "LM358P",
            Some(Ok(mouser_part("a"))),
            Some(Ok(digikey_part())),
        )
        .unwrap();
        assert_eq!(info.manufacturer, "Texas Instruments");
        assert_eq!(info.offers.len(), 2);
        assert!(info.warnings.is_empty());
        assert_eq!(info.offers[0].seller, "Mouser");
        assert_eq!(info.offers[1].seller, "DigiKey");
    }

    #[test]
    fn one_vendor_failing_still_succeeds_with_a_warning() {
        let info = combine_results(
            "LM358P",
            Some(Ok(mouser_part("a"))),
            Some(Err(DigikeyError::NotFound("LM358P".to_string()))),
        )
        .unwrap();
        assert_eq!(info.offers.len(), 1);
        assert_eq!(info.offers[0].seller, "Mouser");
        assert_eq!(info.warnings.len(), 1);
        assert!(info.warnings[0].starts_with("DigiKey:"));
    }

    #[test]
    fn neither_vendor_configured_is_missing_credentials() {
        let err = combine_results("LM358P", None, None).unwrap_err();
        assert!(matches!(err, PartsLookupError::MissingCredentials));
    }

    #[test]
    fn both_configured_vendors_failing_is_all_vendors_failed() {
        let err = combine_results(
            "LM358P",
            Some(Err(MouserError::NotFound("LM358P".to_string()))),
            Some(Err(DigikeyError::NotFound("LM358P".to_string()))),
        )
        .unwrap_err();
        assert!(
            matches!(err, PartsLookupError::AllVendorsFailed(msg) if msg.contains("Mouser") && msg.contains("DigiKey"))
        );
    }

    #[test]
    fn only_one_vendor_configured_and_it_fails_is_all_vendors_failed() {
        let err =
            combine_results("LM358P", Some(Err(MouserError::MissingApiKey)), None).unwrap_err();
        assert!(matches!(err, PartsLookupError::AllVendorsFailed(_)));
    }

    #[test]
    fn falls_back_to_the_queried_mpn_when_a_vendor_returns_none() {
        let mut part = mouser_part("a");
        part.mpn = String::new();
        let info = combine_results("QUERY-MPN", Some(Ok(part)), None).unwrap();
        assert_eq!(info.mpn, "QUERY-MPN");
    }

    // ── stock status ─────────────────────────────────────────────────

    #[test]
    fn in_stock_true_when_any_offer_is_in_stock() {
        let mut out_of_stock = digikey_part();
        out_of_stock.stock_status = StockStatus::OutOfStock;
        out_of_stock.stock_summary = "0 in stock".to_string();
        let info = combine_results(
            "LM358P",
            Some(Ok(mouser_part("a"))), // in stock
            Some(Ok(out_of_stock)),
        )
        .unwrap();
        assert!(info.in_stock());
    }

    #[test]
    fn in_stock_false_when_every_offer_is_out_of_stock() {
        let mut mouser = mouser_part("a");
        mouser.stock_status = StockStatus::OutOfStock;
        mouser.stock_summary = "0 In Stock".to_string();
        let info = combine_results("LM358P", Some(Ok(mouser)), None).unwrap();
        assert!(!info.in_stock());
    }

    #[test]
    fn apply_part_info_writes_a_stock_property_per_vendor() {
        let info = combine_results(
            "LM358P",
            Some(Ok(mouser_part("a"))),
            Some(Ok(digikey_part())),
        )
        .unwrap();
        let mut node = crate::sexp::parse(r#"(symbol "U1" (property "Reference" "U"))"#).unwrap();
        apply_part_info(&mut node, &info);

        let prop_value = |key: &str| -> Option<String> {
            node.find_all("property").into_iter().find_map(|p| {
                let Some(Child::Atom(k)) = p.children.first() else {
                    return None;
                };
                (k.text() == key).then(|| match p.children.get(1) {
                    Some(Child::Atom(v)) => v.text().to_string(),
                    _ => String::new(),
                })
            })
        };
        assert_eq!(
            prop_value("Mouser Stock").as_deref(),
            Some("1,934 In Stock")
        );
        assert_eq!(
            prop_value("DigiKey Stock").as_deref(),
            Some("2,500 in stock")
        );
        assert_eq!(prop_value("Mouser Lifecycle").as_deref(), Some("Active"));
        assert_eq!(prop_value("DigiKey Lifecycle").as_deref(), Some("Active"));
        // No suggested replacement given here, so no property at all —
        // not an empty one.
        assert_eq!(prop_value("Mouser Replacement"), None);
    }

    #[test]
    fn apply_part_info_writes_a_suggested_replacement_when_given() {
        let mut mouser = mouser_part("a");
        mouser.suggested_replacement = "LM358PWR".to_string();
        let info = combine_results("LM358P", Some(Ok(mouser)), None).unwrap();
        let mut node = crate::sexp::parse(r#"(symbol "U1" (property "Reference" "U"))"#).unwrap();
        apply_part_info(&mut node, &info);

        let has_replacement = node.find_all("property").into_iter().any(|p| {
            matches!(p.children.first(), Some(Child::Atom(a)) if a.text() == "Mouser Replacement")
                && matches!(p.children.get(1), Some(Child::Atom(v)) if v.text() == "LM358PWR")
        });
        assert!(has_replacement);
    }

    // ── read_cached_part_info (apply_part_info's read side) ──────────

    #[test]
    fn read_cached_part_info_round_trips_apply_part_info() {
        let mut mouser = mouser_part("a");
        mouser.price_breaks = vec![(1.0, 0.55), (10.0, 0.41), (100.0, 0.32)];
        let info = combine_results("LM358P", Some(Ok(mouser)), Some(Ok(digikey_part()))).unwrap();
        let mut node = crate::sexp::parse(r#"(symbol "U1" (property "Reference" "U"))"#).unwrap();
        apply_part_info(&mut node, &info);

        let cached = read_cached_part_info(&node).unwrap();
        assert_eq!(cached.manufacturer, info.manufacturer);
        assert_eq!(cached.mpn, info.mpn);
        assert_eq!(cached.offers.len(), 2);
        let mouser_offer = cached.offers.iter().find(|o| o.seller == "Mouser").unwrap();
        let mut breaks = mouser_offer.price_breaks.clone();
        breaks.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        assert_eq!(breaks, vec![(1.0, 0.55), (10.0, 0.41), (100.0, 0.32)]);
        assert_eq!(mouser_offer.stock_quantity, 1934);
        assert_eq!(mouser_offer.stock_status, StockStatus::InStock);
    }

    #[test]
    fn read_cached_part_info_is_none_when_never_looked_up() {
        let node = crate::sexp::parse(r#"(symbol "U1" (property "Reference" "U"))"#).unwrap();
        assert!(read_cached_part_info(&node).is_none());
    }

    #[test]
    fn read_cached_part_info_is_none_when_cached_offers_carry_no_price_breaks() {
        // Simulates a `Last Checked`/`Mfr #`/vendor property set written
        // by a "Populate BOM" run from before the raw price-breaks
        // property existed — Generate BOM must treat this as a cache
        // miss (and do a fresh lookup) rather than a priceable-but-empty
        // hit, or it'd report "no priced offers available" for a full
        // 24h even though a real lookup would find prices.
        let mut node = crate::sexp::parse(r#"(symbol "U1" (property "Reference" "U"))"#).unwrap();
        set_symbol_property(&mut node, "Mfr", "Texas Instruments");
        set_symbol_property(&mut node, "Mfr #", "LM358P");
        set_symbol_property(&mut node, "Mouser", "https://mouser.com/lm358p");
        set_symbol_property(&mut node, "Mouser #", "595-LM358P");
        set_symbol_property(&mut node, "Mouser Qty/Price", "1:$0.55");
        set_symbol_property(&mut node, "Mouser Stock", "1,934 In Stock");
        // Deliberately no "Mouser Price Breaks (Raw)" / "Mouser Stock Qty".
        assert!(read_cached_part_info(&node).is_none());
    }

    #[test]
    fn raw_price_breaks_round_trip_exactly() {
        let breaks = vec![(1.0, 0.123456), (10.0, 0.0987), (100.0, 0.05)];
        let formatted = format_raw_price_breaks(&breaks);
        assert_eq!(parse_raw_price_breaks(&formatted), breaks);
    }

    // ── lifecycle status ─────────────────────────────────────────────

    #[test]
    fn lifecycle_concern_true_when_any_offer_is_flagged() {
        let mut mouser = mouser_part("a");
        mouser.lifecycle_summary = "Obsolete".to_string();
        mouser.lifecycle_concern = true;
        let info = combine_results("LM358P", Some(Ok(mouser)), Some(Ok(digikey_part()))).unwrap();
        assert!(info.lifecycle_concern());
    }

    #[test]
    fn lifecycle_concern_false_when_no_offer_is_flagged() {
        let info = combine_results("LM358P", Some(Ok(mouser_part("a"))), None).unwrap();
        assert!(!info.lifecycle_concern());
    }

    #[test]
    fn is_lifecycle_concern_matches_known_keywords_case_insensitively() {
        for text in [
            "Obsolete",
            "obsolete",
            "Discontinued at Digi-Key",
            "Not Recommended for New Designs",
            "NRND",
            "Last Time Buy",
            "End of Life",
        ] {
            assert!(
                is_lifecycle_concern(text),
                "expected '{text}' to be flagged"
            );
        }
    }

    #[test]
    fn is_lifecycle_concern_does_not_flag_active_or_unknown() {
        assert!(!is_lifecycle_concern("Active"));
        assert!(!is_lifecycle_concern("New Product"));
        assert!(!is_lifecycle_concern("Unknown"));
        assert!(!is_lifecycle_concern(""));
    }

    // ── description ──────────────────────────────────────────────────

    #[test]
    fn richer_description_picks_more_non_whitespace_characters() {
        assert_eq!(
            richer_description("short", "a much longer description"),
            "a much longer description"
        );
        assert_eq!(
            richer_description("a much longer description", "short"),
            "a much longer description"
        );
    }

    #[test]
    fn richer_description_is_not_fooled_by_padding_whitespace() {
        // "short" padded with spaces is still shorter in actual content
        // than "denser", even though its raw `.len()` is now bigger.
        assert_eq!(richer_description("short          ", "denser"), "denser");
    }

    #[test]
    fn richer_description_keeps_a_on_ties_including_both_empty() {
        assert_eq!(richer_description("same", "same"), "same");
        assert_eq!(richer_description("", ""), "");
    }

    #[test]
    fn combine_results_picks_the_richer_vendor_description() {
        // `digikey_part()`'s description is longer than `mouser_part()`'s
        // — see the fixtures above.
        let info = combine_results(
            "LM358P",
            Some(Ok(mouser_part("a"))),
            Some(Ok(digikey_part())),
        )
        .unwrap();
        assert_eq!(
            info.description,
            "Standard (General Purpose) Amplifier 2 Circuit 8-PDIP"
        );
    }

    #[test]
    fn combine_results_description_is_empty_when_neither_vendor_has_one() {
        let mut mouser = mouser_part("a");
        mouser.description = String::new();
        let info = combine_results("LM358P", Some(Ok(mouser)), None).unwrap();
        assert_eq!(info.description, "");
    }

    #[test]
    fn apply_part_info_writes_a_vendor_description_property() {
        let info = combine_results("LM358P", Some(Ok(mouser_part("a"))), None).unwrap();
        let mut node = crate::sexp::parse(r#"(symbol "U1" (property "Reference" "U"))"#).unwrap();
        apply_part_info(&mut node, &info);

        let has_description = node.find_all("property").into_iter().any(|p| {
            matches!(p.children.first(), Some(Child::Atom(a)) if a.text() == "Vendor Description")
                && matches!(p.children.get(1), Some(Child::Atom(v)) if v.text() == "Op Amps Dual Op Amp")
        });
        assert!(has_description);
    }

    #[test]
    fn apply_part_info_omits_vendor_description_when_empty() {
        let mut mouser = mouser_part("a");
        mouser.description = String::new();
        let info = combine_results("LM358P", Some(Ok(mouser)), None).unwrap();
        let mut node = crate::sexp::parse(r#"(symbol "U1" (property "Reference" "U"))"#).unwrap();
        apply_part_info(&mut node, &info);

        let has_description = node
            .find_all("property")
            .into_iter()
            .any(|p| matches!(p.children.first(), Some(Child::Atom(a)) if a.text() == "Vendor Description"));
        assert!(!has_description);
    }

    // ── price formatting ─────────────────────────────────────────────

    #[test]
    fn format_price_breaks_sorts_and_caps() {
        let breaks = vec![
            (100.0, 0.32),
            (1.0, 0.55),
            (10.0, 0.41),
            (1000.0, 0.20),
            (5000.0, 0.10),
        ];
        assert_eq!(
            format_price_breaks(&breaks),
            "1:$0.55 | 10:$0.41 | 100:$0.32 | 1000:$0.20"
        );
    }

    // ── MPN resolution ───────────────────────────────────────────────

    #[test]
    fn resolve_mpn_falls_back_to_symbol_name_when_no_candidate_property() {
        let node = crate::sexp::parse(r#"(symbol "LM358" (property "Reference" "U"))"#).unwrap();
        assert_eq!(resolve_mpn(&node, "LM358"), "LM358");
    }

    #[test]
    fn resolve_mpn_prefers_an_existing_mpn_property() {
        let node = crate::sexp::parse(
            r#"(symbol "U1" (property "Reference" "U") (property "MPN" "LM358DR"))"#,
        )
        .unwrap();
        assert_eq!(resolve_mpn(&node, "U1"), "LM358DR");
    }

    #[test]
    fn resolve_mpn_candidate_matching_is_case_insensitive() {
        let node =
            crate::sexp::parse(r#"(symbol "U1" (property "manufacturer part number" "LM358DR"))"#)
                .unwrap();
        assert_eq!(resolve_mpn(&node, "U1"), "LM358DR");
    }

    #[test]
    fn resolve_mpn_ignores_a_blank_candidate_property() {
        let node = crate::sexp::parse(r#"(symbol "LM358" (property "MPN" ""))"#).unwrap();
        assert_eq!(resolve_mpn(&node, "LM358"), "LM358");
    }

    // ── cheapest_purchase ────────────────────────────────────────────

    #[test]
    fn crossing_into_a_cheaper_tier_beats_buying_the_exact_amount() {
        // The worked example from the plan: need 6, breaks are
        // 1:$0.10 and 10:$0.05 — buying 10 for $0.50 beats buying
        // exactly 6 for $0.60.
        let breaks = [(1.0, 0.10), (10.0, 0.05)];
        let best = cheapest_purchase(&breaks, 6).unwrap();
        assert_eq!(best.quantity, 10);
        assert!((best.unit_price - 0.05).abs() < 1e-9);
        assert!((best.total_price - 0.50).abs() < 1e-9);
    }

    #[test]
    fn buying_exactly_needed_wins_when_no_higher_tier_is_cheaper() {
        // Need 6, only break is 1:$0.10 — no cheaper tier to cross into.
        let breaks = [(1.0, 0.10)];
        let best = cheapest_purchase(&breaks, 6).unwrap();
        assert_eq!(best.quantity, 6);
        assert!((best.total_price - 0.60).abs() < 1e-9);
    }

    #[test]
    fn needed_quantity_exactly_on_a_break_uses_that_breaks_price() {
        let breaks = [(1.0, 0.10), (10.0, 0.05), (100.0, 0.02)];
        let best = cheapest_purchase(&breaks, 10).unwrap();
        assert_eq!(best.quantity, 10);
        assert!((best.unit_price - 0.05).abs() < 1e-9);
    }

    #[test]
    fn needed_above_every_break_uses_the_highest_breaks_price() {
        let breaks = [(1.0, 0.10), (10.0, 0.05)];
        let best = cheapest_purchase(&breaks, 500).unwrap();
        assert_eq!(best.quantity, 500);
        assert!((best.unit_price - 0.05).abs() < 1e-9);
    }

    #[test]
    fn picks_the_cheapest_of_several_higher_tiers_not_just_the_next_one() {
        // Need 6; 10 costs $0.50 total, but 100 costs only $0.02*100 =
        // $2.00 — wait, that's *more* than 10's $0.50, so the cheapest
        // really is the 10-break here. Use numbers where a *further*
        // tier is the actual winner: 1:$1.00, 10:$0.50 (=$5 for 10),
        // 100:$0.02 (=$2 for 100) — 100 wins outright.
        let breaks = [(1.0, 1.00), (10.0, 0.50), (100.0, 0.02)];
        let best = cheapest_purchase(&breaks, 6).unwrap();
        assert_eq!(best.quantity, 100);
        assert!((best.total_price - 2.00).abs() < 1e-9);
    }

    #[test]
    fn empty_breaks_is_none() {
        assert!(cheapest_purchase(&[], 6).is_none());
    }

    #[test]
    fn zero_needed_is_none() {
        assert!(cheapest_purchase(&[(1.0, 0.10)], 0).is_none());
    }
}
