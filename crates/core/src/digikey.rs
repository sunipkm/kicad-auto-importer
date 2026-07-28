//! DigiKey Product Information API v4 client — manufacturer and pricing
//! info for a manufacturer part number (MPN), used as one of two vendor
//! sources merged by `crate::parts_lookup`.
//!
//! Auth is OAuth2 client-credentials (2-legged): a Client ID + Client
//! Secret exchanged for a short-lived (~10 minute) bearer token against
//! `api.digikey.com/v1/oauth2/token`. This endpoint/flow is
//! well-corroborated (DigiKey's own developer portal, a working example
//! script); the exact `KeywordSearch` response field names are
//! reconstructed from community sources, not verified against a live
//! authenticated call — see `docs/plans/parts-lookup.md` for the full
//! caveat.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::parts_lookup::{
    format_price_breaks, is_lifecycle_concern, richer_description, StockStatus,
};

const TOKEN_URL: &str = "https://api.digikey.com/v1/oauth2/token";
const SEARCH_URL: &str = "https://api.digikey.com/products/v4/search/keyword";

/// Refresh a cached token this far ahead of its real expiry, rather
/// than waiting for a 401 mid-batch — see `get_token`. Proportionally
/// similar to Nexar's 60s/3600s margin, scaled down for DigiKey's much
/// shorter ~600s token lifetime.
const TOKEN_EXPIRY_SAFETY_MARGIN: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DigikeyCredentials {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DigikeyPart {
    pub manufacturer: String,
    pub mpn: String,
    /// The richer (see [`richer_description`]) of DigiKey's own
    /// `Description.DetailedDescription` and
    /// `Description.ProductDescription` — empty if DigiKey sent neither.
    pub description: String,
    pub url: String,
    pub sku: String,
    pub price_summary: String,
    pub stock_status: StockStatus,
    /// e.g. `"2,500 in stock"`, derived from DigiKey's own
    /// `QuantityAvailable` integer (unlike Mouser's free-text
    /// `Availability`, this one is always a plain count).
    pub stock_summary: String,
    /// DigiKey's own `ProductStatus.Status` text, e.g. `"Obsolete"`,
    /// `"Not Recommended for New Designs"` — `"Unknown"` if DigiKey
    /// didn't set one.
    pub lifecycle_summary: String,
    pub lifecycle_concern: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum DigikeyError {
    #[error("no DigiKey Client ID/Secret is set")]
    MissingCredentials,
    #[error("DigiKey rejected the API credentials (401) — check the Client ID/Secret")]
    AuthFailed,
    #[error("DigiKey rate-limited this request (429) — try again shortly")]
    RateLimited,
    #[error("DigiKey returned HTTP {0}")]
    Http(u16),
    #[error("network error talking to DigiKey: {0}")]
    Network(String),
    #[error("no match found for '{0}'")]
    NotFound(String),
    #[error("could not parse DigiKey's response: {0}")]
    MalformedResponse(String),
}

impl From<ureq::Error> for DigikeyError {
    fn from(err: ureq::Error) -> Self {
        match err {
            ureq::Error::StatusCode(401) => DigikeyError::AuthFailed,
            ureq::Error::StatusCode(429) => DigikeyError::RateLimited,
            ureq::Error::StatusCode(code) => DigikeyError::Http(code),
            other => DigikeyError::Network(other.to_string()),
        }
    }
}

pub fn lookup_part(creds: &DigikeyCredentials, mpn: &str) -> Result<DigikeyPart, DigikeyError> {
    if creds.client_id.trim().is_empty() || creds.client_secret.trim().is_empty() {
        return Err(DigikeyError::MissingCredentials);
    }
    let token = get_token(creds)?;
    search_part(&creds.client_id, &token, mpn)
}

/// Confirms `creds` are accepted by DigiKey's OAuth token endpoint, for
/// the app's "API Settings" connection-test button. Cheaper than a full
/// part search — the token exchange itself is the actual credential
/// check, so there's no need to hit the search API at all — and it warms
/// the same process-lifetime token cache `lookup_part` uses.
pub fn test_credentials(creds: &DigikeyCredentials) -> Result<(), DigikeyError> {
    if creds.client_id.trim().is_empty() || creds.client_secret.trim().is_empty() {
        return Err(DigikeyError::MissingCredentials);
    }
    get_token(creds).map(|_| ())
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
/// a pure function (no global state, an injectable `now`), the same
/// pattern used by the original Octopart/Nexar client, so the cache's
/// decision logic is unit-testable without a real wait or touching the
/// process-global `TOKEN_CACHE`.
fn cached_token_is_valid(cached: &CachedToken, creds: &DigikeyCredentials, now: Instant) -> bool {
    let same_creds =
        cached.client_id == creds.client_id && cached.client_secret == creds.client_secret;
    let fresh_enough =
        cached.expires_at.saturating_duration_since(now) > TOKEN_EXPIRY_SAFETY_MARGIN;
    same_creds && fresh_enough
}

/// Process-lifetime, in-memory token cache — never persisted to disk.
fn get_token(creds: &DigikeyCredentials) -> Result<String, DigikeyError> {
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

fn fetch_token(creds: &DigikeyCredentials) -> Result<(String, u64), DigikeyError> {
    let response = ureq::post(TOKEN_URL)
        .send_form([
            ("client_id", creds.client_id.as_str()),
            ("client_secret", creds.client_secret.as_str()),
            ("grant_type", "client_credentials"),
        ])
        .map_err(DigikeyError::from)?;
    let text = read_body(response)?;
    let parsed: TokenResponse =
        serde_json::from_str(&text).map_err(|e| DigikeyError::MalformedResponse(e.to_string()))?;
    Ok((parsed.access_token, parsed.expires_in))
}

fn read_body(response: ureq::http::Response<ureq::Body>) -> Result<String, DigikeyError> {
    response
        .into_body()
        .read_to_string()
        .map_err(|e| DigikeyError::Network(e.to_string()))
}

// ── Keyword search ───────────────────────────────────────────────────

#[derive(Serialize)]
struct SearchRequest<'a> {
    #[serde(rename = "Keywords")]
    keywords: &'a str,
    #[serde(rename = "Limit")]
    limit: u32,
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(rename = "Products", default)]
    products: Vec<RawProduct>,
}

#[derive(Deserialize)]
struct RawProduct {
    #[serde(rename = "ManufacturerProductNumber", default)]
    manufacturer_product_number: String,
    #[serde(rename = "Manufacturer", default)]
    manufacturer: Option<RawManufacturer>,
    #[serde(rename = "ProductUrl", default)]
    product_url: String,
    #[serde(rename = "ProductVariations", default)]
    product_variations: Vec<RawVariation>,
    /// DigiKey's own total on-hand quantity across variations.
    #[serde(rename = "QuantityAvailable", default)]
    quantity_available: u64,
    #[serde(rename = "ProductStatus", default)]
    product_status: Option<RawProductStatus>,
    #[serde(rename = "Description", default)]
    description: Option<RawDescription>,
}

#[derive(Deserialize)]
struct RawProductStatus {
    #[serde(rename = "Status", default)]
    status: String,
}

/// DigiKey splits its description into two independent strings rather
/// than one — `ProductDescription` is a terse catalog blurb (e.g. "IC
/// OPAMP GP 2 CIRCUIT 8DIP"), `DetailedDescription` is usually longer
/// and more human-readable (e.g. "Standard (General Purpose) Amplifier
/// 2 Circuit 8-PDIP"). [`richer_description`] picks between them the
/// same way it later picks between vendors.
#[derive(Deserialize)]
struct RawDescription {
    #[serde(rename = "ProductDescription", default)]
    product_description: String,
    #[serde(rename = "DetailedDescription", default)]
    detailed_description: String,
}

#[derive(Deserialize)]
struct RawManufacturer {
    #[serde(rename = "Name", default)]
    name: String,
}

#[derive(Deserialize)]
struct RawVariation {
    #[serde(rename = "DigiKeyProductNumber", default)]
    digikey_product_number: String,
    #[serde(rename = "StandardPricing", default)]
    standard_pricing: Vec<RawPriceBreak>,
}

#[derive(Deserialize)]
struct RawPriceBreak {
    #[serde(rename = "BreakQuantity", default)]
    break_quantity: f64,
    #[serde(rename = "UnitPrice", default)]
    unit_price: f64,
}

fn search_part(client_id: &str, token: &str, mpn: &str) -> Result<DigikeyPart, DigikeyError> {
    let text = raw_search(client_id, token, mpn)?;
    parse_search_response(&text, mpn)
}

fn raw_search(client_id: &str, token: &str, mpn: &str) -> Result<String, DigikeyError> {
    let request = SearchRequest {
        keywords: mpn,
        limit: 1,
    };
    let body = serde_json::to_string(&request).expect("request body always serializes");

    let response = ureq::post(SEARCH_URL)
        .header("X-DIGIKEY-Client-Id", client_id)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .send(body.as_str())
        .map_err(DigikeyError::from)?;

    read_body(response)
}

/// Fetches the raw (unparsed) JSON DigiKey returns for `mpn` — useful
/// for checking `parse_search_response`'s field-name assumptions
/// against a real account (see `examples/digikey_lookup.rs`); goes
/// through the same cache-aware [`get_token`] as [`lookup_part`], so it
/// won't burn a fresh token on every call either.
pub fn fetch_raw(creds: &DigikeyCredentials, mpn: &str) -> Result<String, DigikeyError> {
    if creds.client_id.trim().is_empty() || creds.client_secret.trim().is_empty() {
        return Err(DigikeyError::MissingCredentials);
    }
    let token = get_token(creds)?;
    raw_search(&creds.client_id, &token, mpn)
}

fn parse_search_response(text: &str, mpn: &str) -> Result<DigikeyPart, DigikeyError> {
    let parsed: SearchResponse =
        serde_json::from_str(text).map_err(|e| DigikeyError::MalformedResponse(e.to_string()))?;

    let product = parsed
        .products
        .into_iter()
        .next()
        .ok_or_else(|| DigikeyError::NotFound(mpn.to_string()))?;

    let variation = product
        .product_variations
        .into_iter()
        .find(|v| !v.standard_pricing.is_empty());
    let (sku, breaks): (String, Vec<(f64, f64)>) = match variation {
        Some(v) => {
            let breaks = v
                .standard_pricing
                .iter()
                .map(|b| (b.break_quantity, b.unit_price))
                .collect();
            (v.digikey_product_number, breaks)
        }
        None => (String::new(), Vec::new()),
    };

    let stock_status = if product.quantity_available > 0 {
        StockStatus::InStock
    } else {
        StockStatus::OutOfStock
    };
    let lifecycle_summary = product
        .product_status
        .map(|s| s.status)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Unknown".to_string());
    let lifecycle_concern = is_lifecycle_concern(&lifecycle_summary);
    let description = product
        .description
        .map(|d| richer_description(&d.detailed_description, &d.product_description).to_string())
        .unwrap_or_default();

    Ok(DigikeyPart {
        manufacturer: product.manufacturer.map(|m| m.name).unwrap_or_default(),
        mpn: if product.manufacturer_product_number.is_empty() {
            mpn.to_string()
        } else {
            product.manufacturer_product_number
        },
        description,
        url: product.product_url,
        sku,
        price_summary: format_price_breaks(&breaks),
        stock_status,
        stock_summary: format!("{} in stock", product.quantity_available),
        lifecycle_summary,
        lifecycle_concern,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Best-corroborated shape (see module docs) — hand-written, not a
    /// live capture.
    const FIXTURE: &str = r#"{
        "Products": [{
            "ManufacturerProductNumber": "LM358P",
            "Manufacturer": { "Name": "Texas Instruments" },
            "Description": {
                "ProductDescription": "IC OPAMP GP 2 CIRCUIT 8DIP",
                "DetailedDescription": "Standard (General Purpose) Amplifier 2 Circuit 8-PDIP"
            },
            "ProductUrl": "https://www.digikey.com/lm358p",
            "ProductVariations": [{
                "DigiKeyProductNumber": "296-1395-5-ND",
                "StandardPricing": [
                    { "BreakQuantity": 1, "UnitPrice": 0.60 },
                    { "BreakQuantity": 100, "UnitPrice": 0.35 },
                    { "BreakQuantity": 10, "UnitPrice": 0.45 }
                ]
            }],
            "QuantityAvailable": 2500,
            "ProductStatus": { "Id": 0, "Status": "Active" }
        }],
        "ProductsCount": 1
    }"#;

    #[test]
    fn parses_manufacturer_and_fields() {
        let part = parse_search_response(FIXTURE, "LM358P").unwrap();
        assert_eq!(part.manufacturer, "Texas Instruments");
        assert_eq!(part.mpn, "LM358P");
        assert_eq!(part.sku, "296-1395-5-ND");
        assert_eq!(part.url, "https://www.digikey.com/lm358p");
    }

    #[test]
    fn formats_price_breaks_sorted() {
        let part = parse_search_response(FIXTURE, "LM358P").unwrap();
        assert_eq!(part.price_summary, "1:$0.60 | 10:$0.45 | 100:$0.35");
    }

    #[test]
    fn zero_products_is_not_found() {
        let empty = r#"{"Products": [], "ProductsCount": 0}"#;
        let err = parse_search_response(empty, "NOSUCHPART").unwrap_err();
        assert!(matches!(err, DigikeyError::NotFound(mpn) if mpn == "NOSUCHPART"));
    }

    #[test]
    fn parses_in_stock_quantity() {
        let part = parse_search_response(FIXTURE, "LM358P").unwrap();
        assert_eq!(part.stock_status, StockStatus::InStock);
        assert_eq!(part.stock_summary, "2500 in stock");
    }

    #[test]
    fn zero_quantity_available_is_out_of_stock() {
        let text = r#"{"Products": [{
            "ManufacturerProductNumber": "X", "ProductUrl": "",
            "ProductVariations": [], "QuantityAvailable": 0
        }]}"#;
        let part = parse_search_response(text, "X").unwrap();
        assert_eq!(part.stock_status, StockStatus::OutOfStock);
        assert_eq!(part.stock_summary, "0 in stock");
    }

    #[test]
    fn missing_credentials_is_rejected_before_any_request() {
        let err = lookup_part(&DigikeyCredentials::default(), "X").unwrap_err();
        assert!(matches!(err, DigikeyError::MissingCredentials));
    }

    // ── lifecycle status ─────────────────────────────────────────────

    #[test]
    fn parses_active_lifecycle_status() {
        let part = parse_search_response(FIXTURE, "LM358P").unwrap();
        assert_eq!(part.lifecycle_summary, "Active");
        assert!(!part.lifecycle_concern);
    }

    #[test]
    fn obsolete_product_status_is_flagged() {
        let text = r#"{"Products": [{
            "ManufacturerProductNumber": "X", "ProductUrl": "",
            "ProductVariations": [], "QuantityAvailable": 0,
            "ProductStatus": { "Id": 1, "Status": "Obsolete" }
        }]}"#;
        let part = parse_search_response(text, "X").unwrap();
        assert_eq!(part.lifecycle_summary, "Obsolete");
        assert!(part.lifecycle_concern);
    }

    #[test]
    fn missing_product_status_defaults_to_unknown() {
        let text = r#"{"Products": [{
            "ManufacturerProductNumber": "X", "ProductUrl": "",
            "ProductVariations": [], "QuantityAvailable": 0
        }]}"#;
        let part = parse_search_response(text, "X").unwrap();
        assert_eq!(part.lifecycle_summary, "Unknown");
        assert!(!part.lifecycle_concern);
    }

    // ── description ───────────────────────────────────────────────────

    #[test]
    fn picks_the_richer_of_detailed_and_product_description() {
        let part = parse_search_response(FIXTURE, "LM358P").unwrap();
        assert_eq!(
            part.description,
            "Standard (General Purpose) Amplifier 2 Circuit 8-PDIP"
        );
    }

    #[test]
    fn falls_back_to_product_description_when_detailed_is_shorter() {
        let text = r#"{"Products": [{
            "ManufacturerProductNumber": "X", "ProductUrl": "",
            "ProductVariations": [], "QuantityAvailable": 0,
            "Description": {
                "ProductDescription": "IC OPAMP GP 2 CIRCUIT 8DIP",
                "DetailedDescription": ""
            }
        }]}"#;
        let part = parse_search_response(text, "X").unwrap();
        assert_eq!(part.description, "IC OPAMP GP 2 CIRCUIT 8DIP");
    }

    #[test]
    fn missing_description_is_empty_not_an_error() {
        let text = r#"{"Products": [{
            "ManufacturerProductNumber": "X", "ProductUrl": "",
            "ProductVariations": [], "QuantityAvailable": 0
        }]}"#;
        let part = parse_search_response(text, "X").unwrap();
        assert_eq!(part.description, "");
    }

    // ── token cache ──────────────────────────────────────────────────

    fn creds(id: &str, secret: &str) -> DigikeyCredentials {
        DigikeyCredentials {
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
        let cached = token("id", "secret", Duration::from_secs(600), now);
        assert!(cached_token_is_valid(&cached, &creds("id", "secret"), now));
    }

    #[test]
    fn cached_token_invalid_for_different_credentials() {
        let now = Instant::now();
        let cached = token("id", "secret", Duration::from_secs(600), now);
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
        // Only 15s left — under the 30s safety margin.
        let cached = token("id", "secret", Duration::from_secs(15), now);
        assert!(!cached_token_is_valid(&cached, &creds("id", "secret"), now));
    }

    #[test]
    fn cached_token_invalid_once_actually_expired() {
        let now = Instant::now();
        let cached = token("id", "secret", Duration::from_secs(600), now);
        let later = now + Duration::from_secs(1200);
        assert!(!cached_token_is_valid(
            &cached,
            &creds("id", "secret"),
            later
        ));
    }
}
