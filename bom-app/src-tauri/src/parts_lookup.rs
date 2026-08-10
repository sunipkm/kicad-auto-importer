//! Merges the Mouser (`crate::mouser`), DigiKey (`crate::digikey`), and
//! Arrow (`crate::arrow`) vendor clients into the single API `crates/app`'s
//! "Populate BOM" flow actually talks to, and writes the merged result onto
//! a KiCad symbol as new properties (`Mfr`, `Mfr #`, `<Vendor>`,
//! `<Vendor> #`, `<Vendor> Qty/Price`).
//!
//! Replaces the earlier Octopart/Nexar integration (a single paid
//! aggregator API) with two free-to-register direct vendor APIs.
//! Unlike Octopart, which answered one query with data from up to three
//! distributors at once, each vendor here is queried independently, so
//! a configured-but-failing vendor (rate-limited, no match, bad
//! credentials) shouldn't take down a lookup that another configured
//! vendor *did* succeed at — see [`combine_results`] for exactly how a
//! partial success is represented.

use crate::arrow::{self, ArrowCredentials, ArrowError, ArrowPart};
use crate::digikey::{self, DigikeyCredentials, DigikeyError, DigikeyPart};
use crate::mouser::{self, MouserCredentials, MouserError, MouserPart};
use crate::parts_cache::PartsCache;
use kicad_parse::sexp::SexpNode;
use kicad_parse::symbol_importer::{get_symbol_property, set_symbol_property};

/// A handful of the cheapest-to-priciest quantity breaks, packed into
/// one string — see [`format_price_breaks`]. KiCad symbol properties
/// are single-line strings, so this is as much of a price curve as one
/// field can hold.
const MAX_PRICE_BREAKS: usize = 4;

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PartsCredentials {
    pub mouser_api_key: String,
    pub digikey_client_id: String,
    pub digikey_client_secret: String,
    #[serde(default)]
    pub arrow_api_key: String,
}

/// Whether a vendor reported a part as orderable right now — deliberately
/// binary (no separate "unknown" state): a vendor that answered the
/// lookup at all always states a quantity/availability one way or the
/// other, and a vendor that *didn't* answer isn't represented as an
/// offer in the first place (see [`VendorOffer`]), so "no offer" already
/// means "not confirmed in stock" without needing a third state here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StockStatus {
    InStock,
    OutOfStock,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VendorOffer {
    /// Display label: "Mouser", "DigiKey", or "Arrow".
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
    /// offers one (most relevant alongside an `"Obsolete"`/`"NRND"`
    /// lifecycle status) — empty when the vendor doesn't provide one.
    pub suggested_replacement: String,
    /// The vendor's raw `(quantity, unit price)` break pairs, unsorted
    /// and uncapped — unlike `price_summary` (a human-readable string,
    /// capped at [`MAX_PRICE_BREAKS`] entries), this keeps full
    /// precision for [`cheapest_purchase`]'s bracket-optimization math.
    pub price_breaks: Vec<(f64, f64)>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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
/// Ties (including both empty) keep `a`. Shared by call sites including
/// DigiKey's own `DetailedDescription` vs. `ProductDescription`
/// (`digikey::parse_search_response`), and the final pick between
/// descriptions from any configured vendor (`combine_results` below) — the
/// rule treats all vendors equally.
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
        "no API credentials are set — add credentials for at least one vendor (Mouser, DigiKey, or Arrow) first"
    )]
    MissingCredentials,
    #[error("{0}")]
    AllVendorsFailed(String),
}

/// The one function callers (the `part_lookup_ui` background thread)
/// need: queries every vendor with credentials configured, merging the
/// results — see [`combine_results`].
///
/// All configured vendors' HTTP calls run concurrently on plain background
/// threads (`std::thread::scope`, this codebase's established pattern
/// for background HTTP work — see `crates/app/src/library_import_ui.rs`'s
/// docs) rather than one after the other: latency is roughly the slowest
/// single vendor's round trip, not the sum of all of them.
#[allow(dead_code)]
pub fn lookup_part_info(creds: &PartsCredentials, mpn: &str) -> Result<PartInfo, PartsLookupError> {
    let want_mouser = !creds.mouser_api_key.trim().is_empty();
    let want_digikey = !creds.digikey_client_id.trim().is_empty()
        && !creds.digikey_client_secret.trim().is_empty();
    let want_arrow = !creds.arrow_api_key.trim().is_empty();

    let (mouser_result, digikey_result, arrow_result) = std::thread::scope(|scope| {
        let mouser_handle = want_mouser.then(|| {
            scope.spawn(|| {
                mouser::lookup_part(
                    &MouserCredentials {
                        api_key: creds.mouser_api_key.clone(),
                    },
                    mpn,
                )
            })
        });
        let digikey_handle = want_digikey.then(|| {
            scope.spawn(|| {
                digikey::lookup_part(
                    &DigikeyCredentials {
                        client_id: creds.digikey_client_id.clone(),
                        client_secret: creds.digikey_client_secret.clone(),
                    },
                    mpn,
                )
            })
        });
        let arrow_handle = want_arrow.then(|| {
            scope.spawn(|| {
                arrow::lookup_part(
                    &ArrowCredentials {
                        api_key: creds.arrow_api_key.clone(),
                    },
                    mpn,
                )
            })
        });
        (
            mouser_handle.map(|h| h.join().expect("mouser lookup thread panicked")),
            digikey_handle.map(|h| h.join().expect("digikey lookup thread panicked")),
            arrow_handle.map(|h| h.join().expect("arrow lookup thread panicked")),
        )
    });

    combine_results(mpn, mouser_result, digikey_result, arrow_result)
}

/// Merges up to three independent vendor lookups into one [`PartInfo`].
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
    arrow: Option<Result<ArrowPart, ArrowError>>,
) -> Result<PartInfo, PartsLookupError> {
    if mouser.is_none() && digikey.is_none() && arrow.is_none() {
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
                    manufacturer = part.manufacturer.clone();
                }
                if !part.mpn.is_empty() {
                    resolved_mpn = part.mpn.clone();
                }
                description = richer_description(&description, &part.description).to_string();
                offers.push(mouser_part_to_offer(part));
            }
            Err(exc) => warnings.push(format!("Mouser: {exc}")),
        }
    }

    if let Some(result) = digikey {
        match result {
            Ok(part) => {
                if manufacturer.is_empty() {
                    manufacturer = part.manufacturer.clone();
                }
                if !part.mpn.is_empty() {
                    resolved_mpn = part.mpn.clone();
                }
                description = richer_description(&description, &part.description).to_string();
                offers.push(digikey_part_to_offer(part));
            }
            Err(exc) => warnings.push(format!("DigiKey: {exc}")),
        }
    }

    if let Some(result) = arrow {
        match result {
            Ok(part) => {
                if manufacturer.is_empty() {
                    manufacturer = part.manufacturer.clone();
                }
                if !part.mpn.is_empty() {
                    resolved_mpn = part.mpn.clone();
                }
                description = richer_description(&description, &part.description).to_string();
                offers.push(arrow_part_to_offer(part));
            }
            Err(exc) => warnings.push(format!("Arrow: {exc}")),
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

fn mouser_part_to_offer(part: MouserPart) -> VendorOffer {
    VendorOffer {
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
    }
}

fn digikey_part_to_offer(part: DigikeyPart) -> VendorOffer {
    VendorOffer {
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
    }
}

fn arrow_part_to_offer(part: ArrowPart) -> VendorOffer {
    VendorOffer {
        seller: "Arrow".to_string(),
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
    }
}

/// One vendor's specific candidate match for a queried MPN — the raw
/// material a "which of these is actually my part" picker chooses from.
/// Distinct from [`VendorOffer`]: a candidate carries the vendor's own
/// reported `manufacturer`/`mpn`/`description` for *this specific
/// match* (different candidates from one keyword search can report
/// different MPNs entirely — a broader or near-duplicate match), where
/// `VendorOffer` is scoped to whichever single candidate a caller has
/// already committed to.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VendorCandidate {
    pub manufacturer: String,
    pub mpn: String,
    pub description: String,
    pub offer: VendorOffer,
}

fn mouser_part_to_candidate(part: MouserPart) -> VendorCandidate {
    VendorCandidate {
        manufacturer: part.manufacturer.clone(),
        mpn: part.mpn.clone(),
        description: part.description.clone(),
        offer: mouser_part_to_offer(part),
    }
}

fn digikey_part_to_candidate(part: DigikeyPart) -> VendorCandidate {
    VendorCandidate {
        manufacturer: part.manufacturer.clone(),
        mpn: part.mpn.clone(),
        description: part.description.clone(),
        offer: digikey_part_to_offer(part),
    }
}

fn arrow_part_to_candidate(part: ArrowPart) -> VendorCandidate {
    VendorCandidate {
        manufacturer: part.manufacturer.clone(),
        mpn: part.mpn.clone(),
        description: part.description.clone(),
        offer: arrow_part_to_offer(part),
    }
}

/// Every plausible match for `mpn`, across every vendor with
/// credentials configured — unlike [`lookup_part_info`] (which commits
/// to one winner per vendor via [`combine_results`]), this surfaces the
/// whole candidate list so a caller can show a user a picker, or apply
/// its own selection policy. `warnings` carries one entry per
/// *configured* vendor whose search itself failed (still not fatal on
/// its own, same as `PartInfo::warnings`).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CandidateSet {
    pub candidates: Vec<VendorCandidate>,
    pub warnings: Vec<String>,
}

/// A handful of results per vendor is enough for a human to pick from
/// without the request/response itself becoming unwieldy.
const MAX_CANDIDATE_RESULTS: u32 = 10;

/// Queries every configured vendor for every match of `mpn`, up to
/// [`MAX_CANDIDATE_RESULTS`] each — see [`CandidateSet`]. Fails outright
/// only if no vendor was configured ([`PartsLookupError::MissingCredentials`])
/// or every attempted vendor's search itself failed
/// ([`PartsLookupError::AllVendorsFailed`]).
///
/// Runs both vendors' searches concurrently — same reasoning as
/// [`lookup_part_info`]'s identical `std::thread::scope` use, and
/// doubly relevant here: this is what the frontend's vendor-result
/// picker calls synchronously while a modal is open, so halving the
/// wait directly shortens how long that modal spends on "Looking up
/// candidates…".
pub fn lookup_part_candidates(
    creds: &PartsCredentials,
    mpn: &str,
) -> Result<CandidateSet, PartsLookupError> {
    let want_mouser = !creds.mouser_api_key.trim().is_empty();
    let want_digikey = !creds.digikey_client_id.trim().is_empty()
        && !creds.digikey_client_secret.trim().is_empty();
    let want_arrow = !creds.arrow_api_key.trim().is_empty();

    let (mouser_result, digikey_result, arrow_result) = std::thread::scope(|scope| {
        let mouser_handle = want_mouser.then(|| {
            scope.spawn(|| mouser::search_parts(&creds.mouser_api_key, mpn, MAX_CANDIDATE_RESULTS))
        });
        let digikey_handle = want_digikey.then(|| {
            scope.spawn(|| {
                digikey::search_parts(
                    &DigikeyCredentials {
                        client_id: creds.digikey_client_id.clone(),
                        client_secret: creds.digikey_client_secret.clone(),
                    },
                    mpn,
                    MAX_CANDIDATE_RESULTS,
                )
            })
        });
        let arrow_handle = want_arrow.then(|| {
            scope.spawn(|| arrow::search_parts(&creds.arrow_api_key, mpn, MAX_CANDIDATE_RESULTS))
        });
        (
            mouser_handle.map(|h| h.join().expect("mouser search thread panicked")),
            digikey_handle.map(|h| h.join().expect("digikey search thread panicked")),
            arrow_handle.map(|h| h.join().expect("arrow search thread panicked")),
        )
    });

    combine_candidate_results(mpn, mouser_result, digikey_result, arrow_result)
}

fn combine_candidate_results(
    mpn: &str,
    mouser: Option<Result<Vec<MouserPart>, MouserError>>,
    digikey: Option<Result<Vec<DigikeyPart>, DigikeyError>>,
    arrow: Option<Result<Vec<ArrowPart>, ArrowError>>,
) -> Result<CandidateSet, PartsLookupError> {
    if mouser.is_none() && digikey.is_none() && arrow.is_none() {
        return Err(PartsLookupError::MissingCredentials);
    }

    let mut candidates = Vec::new();
    let mut warnings = Vec::new();

    if let Some(result) = mouser {
        match result {
            Ok(parts) => candidates.extend(parts.into_iter().map(mouser_part_to_candidate)),
            Err(exc) => warnings.push(format!("Mouser: {exc}")),
        }
    }
    if let Some(result) = digikey {
        match result {
            Ok(parts) => candidates.extend(parts.into_iter().map(digikey_part_to_candidate)),
            Err(exc) => warnings.push(format!("DigiKey: {exc}")),
        }
    }
    if let Some(result) = arrow {
        match result {
            Ok(parts) => candidates.extend(parts.into_iter().map(arrow_part_to_candidate)),
            Err(exc) => warnings.push(format!("Arrow: {exc}")),
        }
    }

    if candidates.is_empty() {
        let msg = if warnings.is_empty() {
            format!("no match found for '{mpn}'")
        } else {
            warnings.join("; ")
        };
        return Err(PartsLookupError::AllVendorsFailed(msg));
    }

    Ok(CandidateSet {
        candidates,
        warnings,
    })
}

/// Merges a user's manually-picked candidates (at most one per vendor,
/// from [`lookup_part_candidates`]'s result — the "vendor result
/// picker" this exists for) into one [`PartInfo`], ready for
/// [`apply_part_info`] the same way a [`lookup_part_info`] result would
/// be. Reuses the same "richer of the two descriptions wins" merge
/// [`combine_results`] applies to a fresh two-vendor lookup, just
/// starting from already-fetched candidates instead of a live query —
/// picking is a *selection* over data already in hand, not a new
/// lookup. Returns `None` for an empty `chosen` list (nothing to
/// apply).
pub fn build_part_info_from_candidates(
    mpn: &str,
    chosen: Vec<VendorCandidate>,
) -> Option<PartInfo> {
    if chosen.is_empty() {
        return None;
    }

    let mut manufacturer = String::new();
    let mut resolved_mpn = mpn.to_string();
    let mut description = String::new();
    let mut offers = Vec::with_capacity(chosen.len());

    for candidate in chosen {
        if manufacturer.is_empty() {
            manufacturer = candidate.manufacturer.clone();
        }
        if !candidate.mpn.is_empty() {
            resolved_mpn = candidate.mpn.clone();
        }
        description = richer_description(&description, &candidate.description).to_string();
        offers.push(candidate.offer);
    }

    Some(PartInfo {
        manufacturer,
        mpn: resolved_mpn,
        description,
        offers,
        warnings: Vec::new(),
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
/// skip a fresh vendor lookup when the instance's own
/// `Last Checked` property (`crate::populate_bom`) is still within the
/// recheck window. `None` if `sym_node` was never
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
    for seller in ["Mouser", "DigiKey", "Arrow"] {
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

/// `"<vendor> — $<unit price>"` plus whether it needs a human's
/// attention, for whatever `apply_part_info` last wrote onto a symbol —
/// picks the same offer [`score_candidates`] would rank first at
/// `needed_qty` (feasible-and-cheapest), so this reads as the "chosen
/// vendor" a fresh lookup would also land on. Used to show a result
/// without a network call: both when a project first loads (whatever
/// the schematic already carries from a past run) and when a batch run
/// skips a still-fresh part. `None` if no offer has price-break data to
/// rank by (mirrors [`read_cached_part_info`]'s own "no usable cache"
/// case).
pub fn summarize_offers(offers: &[VendorOffer], needed_qty: u32) -> Option<(String, bool)> {
    let mut priced: Vec<(&VendorOffer, PurchaseOption, bool)> = offers
        .iter()
        .filter_map(|offer| {
            let option = cheapest_purchase(&offer.price_breaks, needed_qty)?;
            let feasible = offer.stock_quantity >= u64::from(option.quantity);
            Some((offer, option, feasible))
        })
        .collect();

    priced.sort_by(|(_, a_opt, a_feasible), (_, b_opt, b_feasible)| {
        (!a_feasible)
            .cmp(&!b_feasible)
            .then_with(|| a_opt.total_price.partial_cmp(&b_opt.total_price).unwrap())
    });

    let (offer, option, feasible) = priced.into_iter().next()?;
    let needs_attention = !feasible || offer.lifecycle_concern;
    Some((
        format!("{} — ${:.2}", offer.seller, option.unit_price),
        needs_attention,
    ))
}

/// One [`VendorCandidate`] priced out for a specific needed quantity —
/// what [`score_candidates`] ranks. `feasible` is the actual purchasing
/// decision ("can this vendor's on-hand stock cover the purchase
/// quantity") — `score` is a 0–100 *display* number derived from it,
/// not the other way around: [`score_candidates`]' sort order is always
/// authoritative, `score` just needs to agree with that order for a
/// human skimming a ranked list, not the reverse.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ScoredCandidate {
    pub candidate: VendorCandidate,
    pub purchase_qty: u32,
    pub unit_price: f64,
    pub total_price: f64,
    /// Whether this offer's own on-hand stock covers `purchase_qty`.
    pub feasible: bool,
    pub score: f64,
}

/// Ranks every candidate that can be priced at all (has price-break
/// data for `needed_qty`) best-first, for automatic vendor+part
/// selection — this *is* the "choose the correct vendor and part"
/// decision, made once across every raw candidate from every vendor
/// rather than as two separate steps (pick a vendor, then pick a
/// price).
///
/// Ranking is exactly two factors, in this priority order:
/// 1. **Feasibility** — a candidate whose on-hand stock covers
///    `needed_qty` always outranks one that can't, regardless of price.
///    A cheaper offer that can't actually fill the order isn't
///    cheaper — it's a different, worse purchase (backorder, split
///    order, or no order at all).
/// 2. **Total cost** — among candidates tied on feasibility, cheapest
///    wins.
///
/// This is intentionally *not* a single blended score used for sorting
/// — a weighted-average approach can let a large-enough price gap
/// outvote a hard "can't fulfill it" fact, which is exactly the
/// mistake `bom_pricing::choose_cheapest_offer` was written to avoid
/// for a single vendor's own price breaks; this generalizes that same
/// rule across every raw candidate from every vendor at once. `score`
/// is computed *after* sorting, purely so a UI can show a number next
/// to each ranked choice — it has no influence on the order itself.
///
/// Candidates with no usable price-break data for `needed_qty` (e.g. a
/// vendor listed the part with no pricing at all) are dropped, not
/// scored at zero — there's nothing to rank them against.
pub fn score_candidates(candidates: &[VendorCandidate], needed_qty: u32) -> Vec<ScoredCandidate> {
    let mut priced: Vec<(VendorCandidate, PurchaseOption, bool)> = candidates
        .iter()
        .filter_map(|c| {
            let option = cheapest_purchase(&c.offer.price_breaks, needed_qty)?;
            let feasible = c.offer.stock_quantity >= u64::from(option.quantity);
            Some((c.clone(), option, feasible))
        })
        .collect();

    priced.sort_by(|(_, a_opt, a_feasible), (_, b_opt, b_feasible)| {
        // Ranking priority (in order):
        // 1. Feasibility: candidates with enough stock rank first
        // 2. Overbuy ratio: prefer options where quantity ≈ needed qty
        //    (don't force buying 700 when you need 5)
        // 3. Total price: among equal overbuy ratios, prefer cheaper
        let a_overbuy = a_opt.quantity as f64 / needed_qty as f64;
        let b_overbuy = b_opt.quantity as f64 / needed_qty as f64;
        (!a_feasible)
            .cmp(&!b_feasible)
            .then_with(|| a_overbuy.partial_cmp(&b_overbuy).unwrap())
            .then_with(|| a_opt.total_price.partial_cmp(&b_opt.total_price).unwrap())
    });

    // Best overbuy ratio and price among feasible options if any exist,
    // else best overall — reference points for scoring below.
    let best_overbuy = priced
        .iter()
        .find(|(_, _, feasible)| *feasible)
        .map(|(_, opt, _)| opt.quantity as f64 / needed_qty as f64)
        .unwrap_or_else(|| {
            priced
                .first()
                .map(|(_, opt, _)| opt.quantity as f64 / needed_qty as f64)
                .unwrap_or(1.0)
        });
    let best_price = priced
        .first()
        .map(|(_, opt, _)| opt.total_price)
        .unwrap_or(0.0);

    priced
        .into_iter()
        .map(|(candidate, option, feasible)| {
            let overbuy_ratio = option.quantity as f64 / needed_qty as f64;
            // Overbuy penalty: buying 2x what's needed costs slightly more
            // (storage, waste), buying 140x is catastrophic. Use (ratio^0.5)
            // as a soft penalty curve: overbuy_ratio=1→1.0, 4→2.0, 100→10.0.
            let overbuy_penalty = overbuy_ratio.sqrt() / best_overbuy.sqrt();

            // Price score: ratio of best to this candidate's total price.
            let price_ratio = if option.total_price > 0.0 {
                (best_price / option.total_price).min(1.0)
            } else {
                1.0
            };

            // Combined score: overbuy penalty and price both matter equally.
            let mut score = (price_ratio / overbuy_penalty) * 100.0;
            if !feasible {
                score *= 0.5;
            }
            if candidate.offer.lifecycle_concern {
                score *= 0.85;
            }
            ScoredCandidate {
                candidate,
                purchase_qty: option.quantity,
                unit_price: option.unit_price,
                total_price: option.total_price,
                feasible,
                score: (score * 10.0).round() / 10.0,
            }
        })
        .collect()
}

/// The single, automatic "choose the correct vendor and part" call
/// both `populate_bom::run_lookup_batch` and `generate_bom::run_bom_batch`
/// use in place of the old "just take the first API result"
/// `lookup_part_info`: reuses `cache`'s entry for `search_string` if
/// it's fresher than `max_age` (no network call at all), otherwise
/// calls [`lookup_part_candidates`] live and records the result in
/// `cache` for next time — then [`score_candidates`] against
/// `needed_qty` and commits to whichever single candidate ranks best.
///
/// `force_refresh` bypasses the cache read (but a fresh live result is
/// still written back to it) — the caller's own "Force re-check"
/// option, same meaning as `populate_bom`/`generate_bom`'s existing
/// per-instance 24h gate.
///
/// Falls back to the first raw candidate (whichever vendor happened to
/// list it first) only if *none* of them carry enough price-break data
/// to be scored at all — still surfaces the match (manufacturer, stock,
/// lifecycle) rather than treating "found it, but no pricing" the same
/// as "found nothing".
#[allow(clippy::too_many_arguments)]
pub fn lookup_best_match(
    cache: &mut PartsCache,
    creds: &PartsCredentials,
    search_string: &str,
    needed_qty: u32,
    force_refresh: bool,
    now: chrono::DateTime<chrono::Utc>,
    max_age: chrono::Duration,
) -> Result<PartInfo, PartsLookupError> {
    let cached = if force_refresh {
        None
    } else {
        cache.get_fresh(search_string, now, max_age).cloned()
    };

    let candidate_set = match cached {
        Some(set) => set,
        None => {
            let fetched = lookup_part_candidates(creds, search_string)?;
            cache.put(search_string, now, fetched.clone());
            fetched
        }
    };

    let winner = score_candidates(&candidate_set.candidates, needed_qty)
        .into_iter()
        .next()
        .map(|scored| scored.candidate)
        .or_else(|| candidate_set.candidates.first().cloned());

    let Some(winner) = winner else {
        return Err(PartsLookupError::AllVendorsFailed(format!(
            "no match found for '{search_string}'"
        )));
    };

    let mut info = build_part_info_from_candidates(search_string, vec![winner])
        .expect("a single non-empty candidate always produces a PartInfo");
    info.warnings = candidate_set.warnings;
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kicad_parse::sexp::Child;

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
            None,
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
            None,
        )
        .unwrap();
        assert_eq!(info.offers.len(), 1);
        assert_eq!(info.offers[0].seller, "Mouser");
        assert_eq!(info.warnings.len(), 1);
        assert!(info.warnings[0].starts_with("DigiKey:"));
    }

    #[test]
    fn neither_vendor_configured_is_missing_credentials() {
        let err = combine_results("LM358P", None, None, None).unwrap_err();
        assert!(matches!(err, PartsLookupError::MissingCredentials));
    }

    #[test]
    fn both_configured_vendors_failing_is_all_vendors_failed() {
        let err = combine_results(
            "LM358P",
            Some(Err(MouserError::NotFound("LM358P".to_string()))),
            Some(Err(DigikeyError::NotFound("LM358P".to_string()))),
            None,
        )
        .unwrap_err();
        assert!(
            matches!(err, PartsLookupError::AllVendorsFailed(msg) if msg.contains("Mouser") && msg.contains("DigiKey"))
        );
    }

    #[test]
    fn only_one_vendor_configured_and_it_fails_is_all_vendors_failed() {
        let err = combine_results("LM358P", Some(Err(MouserError::MissingApiKey)), None, None)
            .unwrap_err();
        assert!(matches!(err, PartsLookupError::AllVendorsFailed(_)));
    }

    // ── combine_candidate_results / lookup_part_candidates ────────────

    #[test]
    fn candidates_neither_vendor_configured_is_missing_credentials() {
        let err = combine_candidate_results("LM358P", None, None, None).unwrap_err();
        assert!(matches!(err, PartsLookupError::MissingCredentials));
    }

    #[test]
    fn candidates_both_vendors_failing_is_all_vendors_failed() {
        let err = combine_candidate_results(
            "LM358P",
            Some(Err(MouserError::NotFound("LM358P".to_string()))),
            Some(Err(DigikeyError::NotFound("LM358P".to_string()))),
            None,
        )
        .unwrap_err();
        assert!(
            matches!(err, PartsLookupError::AllVendorsFailed(msg) if msg.contains("Mouser") && msg.contains("DigiKey"))
        );
    }

    #[test]
    fn candidates_flattens_every_result_from_both_vendors() {
        let mut second_mouser = mouser_part("b");
        second_mouser.mpn = "LM358PWR".to_string();
        let set = combine_candidate_results(
            "LM358P",
            Some(Ok(vec![mouser_part("a"), second_mouser])),
            Some(Ok(vec![digikey_part()])),
            None,
        )
        .unwrap();
        assert_eq!(set.candidates.len(), 3);
        assert_eq!(
            set.candidates
                .iter()
                .filter(|c| c.offer.seller == "Mouser")
                .count(),
            2
        );
        assert_eq!(
            set.candidates
                .iter()
                .filter(|c| c.offer.seller == "DigiKey")
                .count(),
            1
        );
        assert!(set.warnings.is_empty());
    }

    #[test]
    fn candidates_one_vendor_failing_still_returns_the_others_candidates() {
        let set = combine_candidate_results(
            "LM358P",
            Some(Ok(vec![mouser_part("a")])),
            Some(Err(DigikeyError::NotFound("LM358P".to_string()))),
            None,
        )
        .unwrap();
        assert_eq!(set.candidates.len(), 1);
        assert_eq!(set.warnings.len(), 1);
        assert!(set.warnings[0].contains("DigiKey"));
    }

    #[test]
    fn build_part_info_from_candidates_is_none_for_an_empty_choice() {
        assert!(build_part_info_from_candidates("LM358P", Vec::new()).is_none());
    }

    #[test]
    fn build_part_info_from_candidates_merges_one_candidate_per_vendor() {
        let chosen = vec![
            mouser_part_to_candidate(mouser_part("a")),
            digikey_part_to_candidate(digikey_part()),
        ];
        let info = build_part_info_from_candidates("LM358P", chosen).unwrap();
        assert_eq!(info.offers.len(), 2);
        assert_eq!(info.manufacturer, "Texas Instruments");
        assert_eq!(info.mpn, "LM358P");
        // The DigiKey fixture's description is the richer one — same
        // `richer_description` merge `combine_results` uses.
        assert!(info.description.contains("Standard (General Purpose)"));
    }

    #[test]
    fn build_part_info_from_candidates_keeps_a_single_vendor_choice() {
        let chosen = vec![mouser_part_to_candidate(mouser_part("a"))];
        let info = build_part_info_from_candidates("LM358P", chosen).unwrap();
        assert_eq!(info.offers.len(), 1);
        assert_eq!(info.offers[0].seller, "Mouser");
    }

    #[test]
    fn falls_back_to_the_queried_mpn_when_a_vendor_returns_none() {
        let mut part = mouser_part("a");
        part.mpn = String::new();
        let info = combine_results("QUERY-MPN", Some(Ok(part)), None, None).unwrap();
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
            None,
        )
        .unwrap();
        assert!(info.in_stock());
    }

    #[test]
    fn in_stock_false_when_every_offer_is_out_of_stock() {
        let mut mouser = mouser_part("a");
        mouser.stock_status = StockStatus::OutOfStock;
        mouser.stock_summary = "0 In Stock".to_string();
        let info = combine_results("LM358P", Some(Ok(mouser)), None, None).unwrap();
        assert!(!info.in_stock());
    }

    #[test]
    fn apply_part_info_writes_a_stock_property_per_vendor() {
        let info = combine_results(
            "LM358P",
            Some(Ok(mouser_part("a"))),
            Some(Ok(digikey_part())),
            None,
        )
        .unwrap();
        let mut node = SexpNode::parse(r#"(symbol "U1" (property "Reference" "U"))"#).unwrap();
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
        let info = combine_results("LM358P", Some(Ok(mouser)), None, None).unwrap();
        let mut node = SexpNode::parse(r#"(symbol "U1" (property "Reference" "U"))"#).unwrap();
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
        let info =
            combine_results("LM358P", Some(Ok(mouser)), Some(Ok(digikey_part())), None).unwrap();
        let mut node = SexpNode::parse(r#"(symbol "U1" (property "Reference" "U"))"#).unwrap();
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
        let node = SexpNode::parse(r#"(symbol "U1" (property "Reference" "U"))"#).unwrap();
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
        let mut node = SexpNode::parse(r#"(symbol "U1" (property "Reference" "U"))"#).unwrap();
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
        let info =
            combine_results("LM358P", Some(Ok(mouser)), Some(Ok(digikey_part())), None).unwrap();
        assert!(info.lifecycle_concern());
    }

    #[test]
    fn lifecycle_concern_false_when_no_offer_is_flagged() {
        let info = combine_results("LM358P", Some(Ok(mouser_part("a"))), None, None).unwrap();
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
            None,
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
        let info = combine_results("LM358P", Some(Ok(mouser)), None, None).unwrap();
        assert_eq!(info.description, "");
    }

    #[test]
    fn apply_part_info_writes_a_vendor_description_property() {
        let info = combine_results("LM358P", Some(Ok(mouser_part("a"))), None, None).unwrap();
        let mut node = SexpNode::parse(r#"(symbol "U1" (property "Reference" "U"))"#).unwrap();
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
        let info = combine_results("LM358P", Some(Ok(mouser)), None, None).unwrap();
        let mut node = SexpNode::parse(r#"(symbol "U1" (property "Reference" "U"))"#).unwrap();
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

    // ── score_candidates ─────────────────────────────────────────────

    #[test]
    fn feasible_candidate_beats_a_cheaper_infeasible_one() {
        let mut cheap_but_out_of_stock = mouser_part_to_candidate(mouser_part("a"));
        cheap_but_out_of_stock.offer.price_breaks = vec![(1.0, 0.006)];
        cheap_but_out_of_stock.offer.stock_quantity = 200;

        let mut pricier_but_in_stock = digikey_part_to_candidate(digikey_part());
        pricier_but_in_stock.offer.price_breaks = vec![(1.0, 0.009)];
        pricier_but_in_stock.offer.stock_quantity = 50_000;

        let ranked = score_candidates(
            &[cheap_but_out_of_stock.clone(), pricier_but_in_stock.clone()],
            1000,
        );

        assert_eq!(ranked[0].candidate.offer.seller, "DigiKey");
        assert!(ranked[0].feasible);
        assert!(!ranked[1].feasible);
        assert!(ranked[0].score > ranked[1].score);
    }

    #[test]
    fn cheapest_wins_among_candidates_tied_on_feasibility() {
        let mut pricier = mouser_part_to_candidate(mouser_part("a"));
        pricier.offer.price_breaks = vec![(1.0, 0.009)];
        pricier.offer.stock_quantity = 50_000;

        let mut cheaper = digikey_part_to_candidate(digikey_part());
        cheaper.offer.price_breaks = vec![(1.0, 0.006)];
        cheaper.offer.stock_quantity = 50_000;

        let ranked = score_candidates(&[pricier, cheaper], 1000);

        assert_eq!(ranked[0].candidate.offer.seller, "DigiKey");
        assert!((ranked[0].total_price - 6.0).abs() < 1e-9);
        assert_eq!(ranked[0].score, 100.0);
    }

    #[test]
    fn when_nothing_is_feasible_still_ranks_by_price() {
        let mut a = mouser_part_to_candidate(mouser_part("a"));
        a.offer.price_breaks = vec![(1.0, 0.009)];
        a.offer.stock_quantity = 10;

        let mut b = digikey_part_to_candidate(digikey_part());
        b.offer.price_breaks = vec![(1.0, 0.006)];
        b.offer.stock_quantity = 10;

        let ranked = score_candidates(&[a, b], 1000);

        assert!(!ranked[0].feasible);
        assert_eq!(ranked[0].candidate.offer.seller, "DigiKey");
    }

    #[test]
    fn candidates_with_no_price_breaks_are_dropped_not_scored() {
        let mut no_price = mouser_part_to_candidate(mouser_part("a"));
        no_price.offer.price_breaks = Vec::new();

        let ranked = score_candidates(&[no_price], 10);
        assert!(ranked.is_empty());
    }

    #[test]
    fn score_candidates_of_an_empty_list_is_empty() {
        assert!(score_candidates(&[], 10).is_empty());
    }

    // ── summarize_offers ──────────────────────────────────────────────

    fn offer(seller: &str, unit_price: f64, stock_quantity: u64) -> VendorOffer {
        VendorOffer {
            seller: seller.to_string(),
            url: String::new(),
            sku: "SKU".to_string(),
            price_summary: format!("1:${unit_price:.2}"),
            stock_status: if stock_quantity > 0 {
                StockStatus::InStock
            } else {
                StockStatus::OutOfStock
            },
            stock_summary: format!("{stock_quantity} In Stock"),
            stock_quantity,
            lifecycle_summary: "Active".to_string(),
            lifecycle_concern: false,
            suggested_replacement: String::new(),
            price_breaks: vec![(1.0, unit_price)],
        }
    }

    #[test]
    fn summarize_offers_picks_the_cheapest_feasible_offer() {
        let offers = vec![offer("Mouser", 1.00, 100), offer("DigiKey", 0.80, 100)];
        let (summary, needs_attention) = summarize_offers(&offers, 1).unwrap();
        assert_eq!(summary, "DigiKey — $0.80");
        assert!(!needs_attention);
    }

    #[test]
    fn summarize_offers_flags_out_of_stock_as_needing_attention() {
        let offers = vec![offer("Mouser", 0.50, 0)];
        let (summary, needs_attention) = summarize_offers(&offers, 1).unwrap();
        assert_eq!(summary, "Mouser — $0.50");
        assert!(needs_attention);
    }

    #[test]
    fn summarize_offers_flags_lifecycle_concern_as_needing_attention() {
        let mut concerning = offer("Mouser", 0.50, 100);
        concerning.lifecycle_concern = true;
        let (_, needs_attention) = summarize_offers(&[concerning], 1).unwrap();
        assert!(needs_attention);
    }

    #[test]
    fn summarize_offers_is_none_without_price_breaks() {
        let mut no_price = offer("Mouser", 0.50, 100);
        no_price.price_breaks.clear();
        assert!(summarize_offers(&[no_price], 1).is_none());
    }

    #[test]
    fn summarize_offers_of_an_empty_list_is_none() {
        assert!(summarize_offers(&[], 1).is_none());
    }

    #[test]
    fn prefers_reasonable_moq_over_high_moq_even_if_cheaper() {
        // The bug from the BOM: D12/D13 choosing 7500 units @ $0.14
        // when 10 units @ $0.15 were available. We need 10 units.
        let mut high_moq_cheaper = digikey_part_to_candidate(digikey_part());
        high_moq_cheaper.mpn = "D12_DIGIKEY_SKU_A".to_string();
        high_moq_cheaper.offer.sku = "SKU-700-qty".to_string();
        high_moq_cheaper.offer.price_breaks = vec![(700.0, 0.14)];
        high_moq_cheaper.offer.stock_quantity = 10_000;

        let mut reasonable_moq_slighter_pricier = mouser_part_to_candidate(mouser_part("a"));
        reasonable_moq_slighter_pricier.mpn = "D12_MOUSER_SKU_B".to_string();
        reasonable_moq_slighter_pricier.offer.sku = "SKU-10-qty".to_string();
        reasonable_moq_slighter_pricier.offer.price_breaks = vec![(10.0, 0.15)];
        reasonable_moq_slighter_pricier.offer.stock_quantity = 100_000;

        let ranked = score_candidates(
            &[
                high_moq_cheaper.clone(),
                reasonable_moq_slighter_pricier.clone(),
            ],
            10,
        );

        // Reasonable MOQ (buy 10 when you need 10) should rank first,
        // despite slightly higher unit price, because overbuy_ratio=1.0
        // beats overbuy_ratio=70.0.
        assert_eq!(ranked[0].candidate.offer.sku, "SKU-10-qty");
        assert!(ranked[0].score > ranked[1].score);
        assert_eq!(ranked[0].purchase_qty, 10);
        assert_eq!(ranked[1].purchase_qty, 700);
    }
}
