//! Octopart/Nexar part-info lookup — manufacturer and Mouser/DigiKey/
//! Arrow distributor pricing for a manufacturer part number (MPN),
//! written onto a KiCad symbol as new properties (see
//! [`apply_part_info`]).
//!
//! "Octopart" is the product; "Nexar" is the company/API it's part of
//! today. Auth is OAuth2 client-credentials (a Client ID + Client
//! Secret, not a single API key) against `identity.nexar.com`, and the
//! actual part search is a GraphQL query against `api.nexar.com`.
//! Endpoints, the auth flow, and the query shape below are verified
//! against Nexar's own official example
//! (`NexarDeveloper/nexar-examples-py/examplePrograms/mpn_pricing_to_csv.py`),
//! not guessed.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::sexp::{Child, SexpNode};
use crate::symbol_importer::set_symbol_property;

const TOKEN_URL: &str = "https://identity.nexar.com/connect/token";
const GRAPHQL_URL: &str = "https://api.nexar.com/graphql";

/// Refresh a cached token this far ahead of its real expiry, rather
/// than waiting for a 401 mid-batch — see [`get_token`].
const TOKEN_EXPIRY_SAFETY_MARGIN: Duration = Duration::from_secs(60);

/// A handful of the cheapest-to-priciest quantity breaks, packed into
/// one string — see [`format_price_breaks`]. KiCad symbol properties
/// are single-line strings, so this is as much of a price curve as one
/// field can hold.
const MAX_PRICE_BREAKS: usize = 4;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OctopartCredentials {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VendorOffer {
    /// Display label: "Mouser", "DigiKey", or "Arrow".
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
}

#[derive(Debug, thiserror::Error)]
pub enum OctopartError {
    #[error(
        "Octopart/Nexar API credentials are not set — add a Client ID and Client Secret first"
    )]
    MissingCredentials,
    #[error("Octopart/Nexar rejected the API credentials (401) — check the Client ID/Secret")]
    AuthFailed,
    #[error("Octopart/Nexar rate-limited this request (429) — try again shortly")]
    RateLimited,
    #[error("Octopart/Nexar returned HTTP {0}")]
    Http(u16),
    #[error("network error talking to Octopart/Nexar: {0}")]
    Network(String),
    #[error("no match found for '{0}'")]
    NotFound(String),
    #[error("could not parse Octopart/Nexar's response: {0}")]
    MalformedResponse(String),
}

impl From<ureq::Error> for OctopartError {
    fn from(err: ureq::Error) -> Self {
        match err {
            ureq::Error::StatusCode(401) => OctopartError::AuthFailed,
            ureq::Error::StatusCode(429) => OctopartError::RateLimited,
            ureq::Error::StatusCode(code) => OctopartError::Http(code),
            other => OctopartError::Network(other.to_string()),
        }
    }
}

/// The one function callers (the `part_lookup_ui` background thread)
/// need: fetches (or reuses a cached) bearer token, then searches for
/// `mpn`.
pub fn lookup_part_info(creds: &OctopartCredentials, mpn: &str) -> Result<PartInfo, OctopartError> {
    if creds.client_id.trim().is_empty() || creds.client_secret.trim().is_empty() {
        return Err(OctopartError::MissingCredentials);
    }
    let token = get_token(creds)?;
    search_part(&token, mpn)
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

/// What to search Octopart/Nexar for: an existing MPN-like property on
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

// ── Token fetch + cache ──────────────────────────────────────────────

struct CachedToken {
    client_id: String,
    client_secret: String,
    access_token: String,
    expires_at: Instant,
}

static TOKEN_CACHE: OnceLock<Mutex<Option<CachedToken>>> = OnceLock::new();

/// Whether `cached` can be reused right now for `creds` — pulled out as
/// a pure function (no global state, an injectable `now`) specifically
/// so the cache's actual decision logic (same credentials, not about to
/// expire) is unit-testable without a real `Instant::now() + 1hr` wait
/// or touching the process-global `TOKEN_CACHE`.
fn cached_token_is_valid(cached: &CachedToken, creds: &OctopartCredentials, now: Instant) -> bool {
    let same_creds =
        cached.client_id == creds.client_id && cached.client_secret == creds.client_secret;
    let fresh_enough =
        cached.expires_at.saturating_duration_since(now) > TOKEN_EXPIRY_SAFETY_MARGIN;
    same_creds && fresh_enough
}

/// Process-lifetime, in-memory token cache: a token fetched for one
/// "Look Up Selected" batch is reused by later batches in the same app
/// session instead of re-authenticating every click. Never persisted to
/// disk (cleared on restart) — cheap to re-fetch, not worth the added
/// complexity of storing it anywhere durable.
fn get_token(creds: &OctopartCredentials) -> Result<String, OctopartError> {
    let cache = TOKEN_CACHE.get_or_init(|| Mutex::new(None));

    if let Some(cached) = cache.lock().unwrap().as_ref() {
        if cached_token_is_valid(cached, creds, Instant::now()) {
            return Ok(cached.access_token.clone());
        }
    }

    let (access_token, expires_in) = fetch_token(creds)?;
    *cache.lock().unwrap() = Some(CachedToken {
        client_id: creds.client_id.clone(),
        client_secret: creds.client_secret.clone(),
        access_token: access_token.clone(),
        expires_at: Instant::now() + Duration::from_secs(expires_in),
    });
    Ok(access_token)
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

/// The actual network call — always hits the network; callers wanting
/// the cache should go through [`get_token`] instead.
fn fetch_token(creds: &OctopartCredentials) -> Result<(String, u64), OctopartError> {
    let response = ureq::post(TOKEN_URL)
        .send_form([
            ("grant_type", "client_credentials"),
            ("client_id", creds.client_id.as_str()),
            ("client_secret", creds.client_secret.as_str()),
            ("scope", "supply.domain"),
        ])
        .map_err(OctopartError::from)?;
    let text = read_body(response)?;
    let parsed: TokenResponse =
        serde_json::from_str(&text).map_err(|e| OctopartError::MalformedResponse(e.to_string()))?;
    Ok((parsed.access_token, parsed.expires_in))
}

fn read_body(response: ureq::http::Response<ureq::Body>) -> Result<String, OctopartError> {
    response
        .into_body()
        .read_to_string()
        .map_err(|e| OctopartError::Network(e.to_string()))
}

// ── GraphQL search ───────────────────────────────────────────────────

/// Verbatim from Nexar's own official example (see module docs), minus
/// fields this app never uses (`inventoryLevel`, `moq`, `packaging`,
/// `updated`, `country`, `homepageUrl`), plus `manufacturer { name }`
/// (confirmed valid via a separate official `supSearchMpn` example).
const SEARCH_QUERY: &str = r#"
query ($queries: [SupPartMatchQuery!]!) {
  supMultiMatch (queries: $queries) {
    hits
    parts {
      mpn
      manufacturer { name }
      sellers {
        company { name }
        offers {
          clickUrl
          sku
          prices { currency price quantity }
        }
      }
    }
  }
}
"#;

#[derive(Serialize)]
struct GraphQlRequest<'a> {
    query: &'a str,
    variables: GraphQlVariables<'a>,
}

#[derive(Serialize)]
struct GraphQlVariables<'a> {
    queries: Vec<MpnQuery<'a>>,
}

#[derive(Serialize)]
struct MpnQuery<'a> {
    mpn: &'a str,
    limit: u32,
    start: u32,
}

#[derive(Deserialize)]
struct GraphQlResponse {
    #[serde(default)]
    data: Option<GraphQlData>,
    #[serde(default)]
    errors: Option<Vec<GraphQlErrorMessage>>,
}

#[derive(Deserialize)]
struct GraphQlErrorMessage {
    message: String,
}

#[derive(Deserialize)]
struct GraphQlData {
    #[serde(rename = "supMultiMatch")]
    sup_multi_match: Vec<SupMatchResult>,
}

#[derive(Deserialize)]
struct SupMatchResult {
    parts: Vec<RawPart>,
}

#[derive(Deserialize)]
struct RawPart {
    mpn: String,
    manufacturer: Option<RawManufacturer>,
    sellers: Vec<RawSeller>,
}

#[derive(Deserialize)]
struct RawManufacturer {
    name: String,
}

#[derive(Deserialize)]
struct RawSeller {
    company: RawCompany,
    offers: Vec<RawOffer>,
}

#[derive(Deserialize)]
struct RawCompany {
    name: String,
}

#[derive(Deserialize)]
struct RawOffer {
    #[serde(rename = "clickUrl", default)]
    click_url: Option<String>,
    #[serde(default)]
    sku: Option<String>,
    #[serde(default)]
    prices: Vec<RawPrice>,
}

#[derive(Deserialize)]
struct RawPrice {
    quantity: f64,
    price: f64,
}

fn search_part(token: &str, mpn: &str) -> Result<PartInfo, OctopartError> {
    let request = GraphQlRequest {
        query: SEARCH_QUERY,
        variables: GraphQlVariables {
            queries: vec![MpnQuery {
                mpn,
                limit: 1,
                start: 0,
            }],
        },
    };
    let body = serde_json::to_string(&request).expect("request body always serializes");

    let response = ureq::post(GRAPHQL_URL)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .send(body.as_str())
        .map_err(OctopartError::from)?;

    let text = read_body(response)?;
    parse_search_response(&text, mpn)
}

fn parse_search_response(text: &str, mpn: &str) -> Result<PartInfo, OctopartError> {
    let parsed: GraphQlResponse =
        serde_json::from_str(text).map_err(|e| OctopartError::MalformedResponse(e.to_string()))?;

    if let Some(errors) = parsed.errors.filter(|e| !e.is_empty()) {
        let joined = errors
            .into_iter()
            .map(|e| e.message)
            .collect::<Vec<_>>()
            .join("; ");
        return Err(OctopartError::MalformedResponse(joined));
    }

    let data = parsed
        .data
        .ok_or_else(|| OctopartError::MalformedResponse("response had no 'data'".to_string()))?;
    let part = data
        .sup_multi_match
        .into_iter()
        .next()
        .and_then(|m| m.parts.into_iter().next())
        .ok_or_else(|| OctopartError::NotFound(mpn.to_string()))?;

    Ok(part_info_from_raw(part))
}

/// Case/punctuation-insensitive: Nexar's returned company names vary
/// ("Digi-Key Electronics", "Digikey", ...) in ways a plain `==` would
/// miss.
fn match_vendor(company_name: &str) -> Option<&'static str> {
    let normalized: String = company_name
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
        .to_lowercase();
    const VENDORS: &[(&str, &str)] = &[
        ("mouser", "Mouser"),
        ("digikey", "DigiKey"),
        ("arrow", "Arrow"),
    ];
    VENDORS
        .iter()
        .find(|(needle, _)| normalized.contains(needle))
        .map(|(_, label)| *label)
}

fn part_info_from_raw(part: RawPart) -> PartInfo {
    let manufacturer = part.manufacturer.map(|m| m.name).unwrap_or_default();
    let mut offers: Vec<VendorOffer> = Vec::new();

    for seller in part.sellers {
        let Some(label) = match_vendor(&seller.company.name) else {
            continue;
        };
        // First match per vendor wins (mirrors Nexar's own example,
        // which only ever annotates a seller's *first* offer) — skip if
        // this vendor's already represented, and skip offers with no
        // pricing at all (nothing useful to show).
        if offers.iter().any(|o| o.seller == label) {
            continue;
        }
        let Some(offer) = seller.offers.into_iter().find(|o| !o.prices.is_empty()) else {
            continue;
        };
        offers.push(VendorOffer {
            seller: label.to_string(),
            url: offer.click_url.unwrap_or_default(),
            sku: offer.sku.unwrap_or_default(),
            price_summary: format_price_breaks(&offer.prices),
        });
    }

    PartInfo {
        manufacturer,
        mpn: part.mpn,
        offers,
    }
}

fn format_price_breaks(prices: &[RawPrice]) -> String {
    let mut sorted: Vec<&RawPrice> = prices.iter().collect();
    sorted.sort_by(|a, b| {
        a.quantity
            .partial_cmp(&b.quantity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    sorted
        .into_iter()
        .take(MAX_PRICE_BREAKS)
        .map(|p| format!("{}:${:.2}", p.quantity as i64, p.price))
        .collect::<Vec<_>>()
        .join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── search response parsing / vendor filtering / price formatting ──

    /// Shaped like a real `supMultiMatch` response (see module docs for
    /// the verified query this corresponds to) — hand-written, not a
    /// live capture, since no real credentials are available to record
    /// one from in this environment.
    const FIXTURE: &str = r#"{
        "data": {
            "supMultiMatch": [
                {
                    "hits": 1,
                    "parts": [
                        {
                            "mpn": "LM358",
                            "manufacturer": { "name": "Texas Instruments" },
                            "sellers": [
                                {
                                    "company": { "name": "Digi-Key Electronics" },
                                    "offers": [
                                        {
                                            "clickUrl": "https://www.digikey.com/lm358",
                                            "sku": "LM358-ND",
                                            "prices": [
                                                { "currency": "USD", "price": 1.23, "quantity": 1 },
                                                { "currency": "USD", "price": 0.89, "quantity": 100 },
                                                { "currency": "USD", "price": 1.05, "quantity": 10 }
                                            ]
                                        }
                                    ]
                                },
                                {
                                    "company": { "name": "Newark" },
                                    "offers": [
                                        {
                                            "clickUrl": "https://newark.com/lm358",
                                            "sku": "12AB34",
                                            "prices": [
                                                { "currency": "USD", "price": 1.50, "quantity": 1 }
                                            ]
                                        }
                                    ]
                                },
                                {
                                    "company": { "name": "Mouser" },
                                    "offers": [
                                        {
                                            "clickUrl": "https://www.mouser.com/lm358",
                                            "sku": "926-LM358",
                                            "prices": []
                                        }
                                    ]
                                }
                            ]
                        }
                    ]
                }
            ]
        }
    }"#;

    #[test]
    fn parses_manufacturer_and_mpn() {
        let info = parse_search_response(FIXTURE, "LM358").unwrap();
        assert_eq!(info.manufacturer, "Texas Instruments");
        assert_eq!(info.mpn, "LM358");
    }

    #[test]
    fn keeps_only_recognized_vendors() {
        let info = parse_search_response(FIXTURE, "LM358").unwrap();
        // "Newark" isn't Mouser/DigiKey/Arrow — filtered out. "Mouser"
        // has no priced offer — filtered out too.
        assert_eq!(info.offers.len(), 1);
        assert_eq!(info.offers[0].seller, "DigiKey");
    }

    #[test]
    fn formats_price_breaks_sorted_and_capped() {
        let info = parse_search_response(FIXTURE, "LM358").unwrap();
        let digikey = &info.offers[0];
        assert_eq!(digikey.url, "https://www.digikey.com/lm358");
        assert_eq!(digikey.sku, "LM358-ND");
        // Sorted by quantity ascending, regardless of input order.
        assert_eq!(digikey.price_summary, "1:$1.23 | 10:$1.05 | 100:$0.89");
    }

    #[test]
    fn zero_hits_is_not_found() {
        let empty = r#"{"data": {"supMultiMatch": [{"hits": 0, "parts": []}]}}"#;
        let err = parse_search_response(empty, "NOSUCHPART").unwrap_err();
        assert!(matches!(err, OctopartError::NotFound(mpn) if mpn == "NOSUCHPART"));
    }

    #[test]
    fn graphql_errors_are_surfaced() {
        let errored = r#"{"errors": [{"message": "invalid MPN syntax"}]}"#;
        let err = parse_search_response(errored, "???").unwrap_err();
        assert!(
            matches!(err, OctopartError::MalformedResponse(msg) if msg.contains("invalid MPN syntax"))
        );
    }

    #[test]
    fn vendor_matching_ignores_case_and_punctuation() {
        assert_eq!(match_vendor("Digi-Key Electronics"), Some("DigiKey"));
        assert_eq!(match_vendor("DIGIKEY"), Some("DigiKey"));
        assert_eq!(match_vendor("Mouser Electronics"), Some("Mouser"));
        assert_eq!(match_vendor("Arrow Electronics, Inc."), Some("Arrow"));
        assert_eq!(match_vendor("Newark"), None);
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

    // ── token cache ──────────────────────────────────────────────────

    fn creds(id: &str, secret: &str) -> OctopartCredentials {
        OctopartCredentials {
            client_id: id.to_string(),
            client_secret: secret.to_string(),
        }
    }

    fn token(id: &str, secret: &str, expires_in: Duration, now: Instant) -> CachedToken {
        CachedToken {
            client_id: id.to_string(),
            client_secret: secret.to_string(),
            access_token: "cached-token".to_string(),
            expires_at: now + expires_in,
        }
    }

    #[test]
    fn cached_token_valid_for_matching_creds_and_far_from_expiry() {
        let now = Instant::now();
        let cached = token("id", "secret", Duration::from_secs(3600), now);
        assert!(cached_token_is_valid(&cached, &creds("id", "secret"), now));
    }

    #[test]
    fn cached_token_invalid_for_different_credentials() {
        let now = Instant::now();
        let cached = token("id", "secret", Duration::from_secs(3600), now);
        assert!(!cached_token_is_valid(
            &cached,
            &creds("id", "different-secret"),
            now
        ));
        assert!(!cached_token_is_valid(
            &cached,
            &creds("different-id", "secret"),
            now
        ));
    }

    #[test]
    fn cached_token_invalid_within_expiry_safety_margin() {
        let now = Instant::now();
        // Only 30s left — under the 60s safety margin, so this should
        // count as already expired even though it technically isn't yet.
        let cached = token("id", "secret", Duration::from_secs(30), now);
        assert!(!cached_token_is_valid(&cached, &creds("id", "secret"), now));
    }

    #[test]
    fn cached_token_invalid_once_actually_expired() {
        let now = Instant::now();
        let cached = token("id", "secret", Duration::from_secs(3600), now);
        // Evaluated as if two hours had passed since it was cached.
        let later = now + Duration::from_secs(7200);
        assert!(!cached_token_is_valid(
            &cached,
            &creds("id", "secret"),
            later
        ));
    }
}
