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
use crate::symbol_importer::set_symbol_property;

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

#[derive(Debug, Clone, PartialEq)]
pub struct VendorOffer {
    /// Display label: "Mouser" or "DigiKey".
    pub seller: String,
    pub url: String,
    pub sku: String,
    /// e.g. `"1:$1.23 | 10:$1.05 | 100:$0.89"`.
    pub price_summary: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PartInfo {
    pub manufacturer: String,
    pub mpn: String,
    pub offers: Vec<VendorOffer>,
    /// A message per *configured* vendor that failed (e.g. `"DigiKey:
    /// no match found for 'LM358'"`), even though the overall lookup
    /// still succeeded because another vendor came back with data.
    /// Empty when every configured vendor succeeded (or only one vendor
    /// was configured and it succeeded).
    pub warnings: Vec<String>,
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
                offers.push(VendorOffer {
                    seller: "Mouser".to_string(),
                    url: part.url,
                    sku: part.sku,
                    price_summary: part.price_summary,
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
                offers.push(VendorOffer {
                    seller: "DigiKey".to_string(),
                    url: part.url,
                    sku: part.sku,
                    price_summary: part.price_summary,
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
        offers,
        warnings,
    })
}

/// Writes `info` onto `sym_node` as `Mfr`/`Mfr #` plus, per matched
/// vendor, `<Vendor>` (URL) / `<Vendor> #` (SKU) / `<Vendor> Qty/Price`.
/// Re-running a lookup overwrites these in place — see
/// `set_symbol_property`'s docs for why that's the right default here.
pub fn apply_part_info(sym_node: &mut SexpNode, info: &PartInfo) {
    set_symbol_property(sym_node, "Mfr", &info.manufacturer);
    set_symbol_property(sym_node, "Mfr #", &info.mpn);
    for offer in &info.offers {
        set_symbol_property(sym_node, &offer.seller, &offer.url);
        set_symbol_property(sym_node, &format!("{} #", offer.seller), &offer.sku);
        set_symbol_property(
            sym_node,
            &format!("{} Qty/Price", offer.seller),
            &offer.price_summary,
        );
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mouser_part(seller_suffix: &str) -> MouserPart {
        MouserPart {
            manufacturer: "Texas Instruments".to_string(),
            mpn: "LM358P".to_string(),
            url: format!("https://mouser.com/{seller_suffix}"),
            sku: "595-LM358P".to_string(),
            price_summary: "1:$0.55".to_string(),
        }
    }

    fn digikey_part() -> DigikeyPart {
        DigikeyPart {
            manufacturer: "Texas Instruments".to_string(),
            mpn: "LM358P".to_string(),
            url: "https://digikey.com/lm358p".to_string(),
            sku: "296-1395-5-ND".to_string(),
            price_summary: "1:$0.60".to_string(),
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
}
