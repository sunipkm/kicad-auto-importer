//! Mouser Search API v1 client — manufacturer and pricing info for a
//! manufacturer part number (MPN), used as one of two vendor sources
//! merged by `crate::parts_lookup`.
//!
//! Auth is a single static API key passed as a query parameter (no
//! OAuth) — the simplest of the two vendor integrations. Endpoint and
//! auth shape are well-corroborated across independent community
//! sources (Mouser's own `/api-search/` page, `sparkmicro/mouser-api`,
//! `PatrickWalther/go-mouser`); the exact response field names are
//! reconstructed the same way, not verified against a live authenticated
//! call (Mouser's interactive Swagger docs require a registered
//! account) — see `docs/plans/parts-lookup.md` for the full caveat.

use serde::{Deserialize, Serialize};

use crate::parts_lookup::{format_price_breaks, is_lifecycle_concern, StockStatus};

const BASE_URL: &str = "https://api.mouser.com/api/v1";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MouserCredentials {
    pub api_key: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MouserPart {
    pub manufacturer: String,
    pub mpn: String,
    pub url: String,
    pub sku: String,
    pub price_summary: String,
    pub stock_status: StockStatus,
    /// Mouser's own `Availability` text verbatim, e.g. `"1,934 In
    /// Stock"` — see [`parse_availability`].
    pub stock_summary: String,
    /// Mouser's own `LifecycleStatus` text, e.g. `"Obsolete"` —
    /// `"Unknown"` if Mouser didn't set it (the common case: seen
    /// `null` even for parts confirmed in stock).
    pub lifecycle_summary: String,
    pub lifecycle_concern: bool,
    /// Mouser's `SuggestedReplacement`, if given; empty otherwise.
    pub suggested_replacement: String,
}

#[derive(Debug, thiserror::Error)]
pub enum MouserError {
    #[error("no Mouser API key is set")]
    MissingApiKey,
    #[error("Mouser rejected the API key (401) — check the key in settings")]
    AuthFailed,
    #[error("Mouser rate-limited this request (429) — try again shortly")]
    RateLimited,
    #[error("Mouser returned HTTP {0}")]
    Http(u16),
    #[error("network error talking to Mouser: {0}")]
    Network(String),
    #[error("no match found for '{0}'")]
    NotFound(String),
    #[error("could not parse Mouser's response: {0}")]
    MalformedResponse(String),
}

impl From<ureq::Error> for MouserError {
    fn from(err: ureq::Error) -> Self {
        match err {
            ureq::Error::StatusCode(401) => MouserError::AuthFailed,
            ureq::Error::StatusCode(429) => MouserError::RateLimited,
            ureq::Error::StatusCode(code) => MouserError::Http(code),
            other => MouserError::Network(other.to_string()),
        }
    }
}

pub fn lookup_part(creds: &MouserCredentials, mpn: &str) -> Result<MouserPart, MouserError> {
    if creds.api_key.trim().is_empty() {
        return Err(MouserError::MissingApiKey);
    }
    search_part(&creds.api_key, mpn)
}

/// Confirms `api_key` is accepted by Mouser, for the app's "API
/// Settings" connection-test button — Mouser has no dedicated
/// credential-check endpoint, so this runs a throwaway keyword search
/// and only cares whether the key itself was rejected. `NotFound` still
/// counts as success: it means Mouser authenticated the request and
/// simply found no match for the probe keyword, which is exactly what a
/// real, working key looks like for an arbitrary search term.
pub fn test_credentials(api_key: &str) -> Result<(), MouserError> {
    if api_key.trim().is_empty() {
        return Err(MouserError::MissingApiKey);
    }
    match search_part(api_key, "test") {
        Ok(_) | Err(MouserError::NotFound(_)) => Ok(()),
        Err(other) => Err(other),
    }
}

#[derive(Serialize)]
struct SearchRequest<'a> {
    #[serde(rename = "SearchByKeywordRequest")]
    search_by_keyword_request: SearchByKeywordRequest<'a>,
}

#[derive(Serialize)]
struct SearchByKeywordRequest<'a> {
    keyword: &'a str,
    records: u32,
    #[serde(rename = "startingRecord")]
    starting_record: u32,
    #[serde(rename = "searchOptions")]
    search_options: &'a str,
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(rename = "Errors", default)]
    errors: Vec<MouserApiError>,
    #[serde(rename = "SearchResults", default)]
    search_results: Option<SearchResults>,
}

#[derive(Deserialize)]
struct MouserApiError {
    #[serde(rename = "Message", default)]
    message: String,
}

#[derive(Deserialize)]
struct SearchResults {
    #[serde(rename = "Parts", default)]
    parts: Vec<RawPart>,
}

#[derive(Deserialize)]
struct RawPart {
    #[serde(rename = "MouserPartNumber", default)]
    mouser_part_number: String,
    #[serde(rename = "ManufacturerPartNumber", default)]
    manufacturer_part_number: String,
    #[serde(rename = "Manufacturer", default)]
    manufacturer: String,
    #[serde(rename = "ProductDetailUrl", default)]
    product_detail_url: String,
    #[serde(rename = "PriceBreaks", default)]
    price_breaks: Vec<RawPriceBreak>,
    /// Free-text, e.g. `"1,934 In Stock"`, `"0 In Stock"`,
    /// `"Non-Stocked"` — see [`parse_availability`]. `Option` because
    /// Mouser sends `null` here for some parts, not just an empty
    /// string or an absent key.
    #[serde(rename = "Availability", default)]
    availability: Option<String>,
    /// e.g. `"Obsolete"` — `null` far more often than set, even for
    /// well-stocked parts (confirmed against a live account).
    #[serde(rename = "LifecycleStatus", default)]
    lifecycle_status: Option<String>,
    #[serde(rename = "SuggestedReplacement", default)]
    suggested_replacement: Option<String>,
}

#[derive(Deserialize)]
struct RawPriceBreak {
    #[serde(rename = "Quantity", default)]
    quantity: f64,
    /// Mouser's real API is consistently documented (across every
    /// independent community source found) as returning this as a
    /// formatted string like `"$1.2300"`, not a bare number — but a
    /// number is accepted too so parsing doesn't break if that's wrong.
    #[serde(rename = "Price", default)]
    price: PriceValue,
}

#[derive(Deserialize, Default)]
#[serde(untagged)]
enum PriceValue {
    #[default]
    Missing,
    Text(String),
    Number(f64),
}

impl PriceValue {
    fn as_f64(&self) -> f64 {
        match self {
            PriceValue::Missing => 0.0,
            PriceValue::Number(n) => *n,
            PriceValue::Text(s) => s
                .chars()
                .filter(|c| c.is_ascii_digit() || *c == '.')
                .collect::<String>()
                .parse()
                .unwrap_or(0.0),
        }
    }
}

/// Mouser's `Availability` field is free text, not a plain integer
/// (`"1,934 In Stock"`, `"0 In Stock"`, `"Non-Stocked"`, `"Call"`, `null`,
/// or an empty string) — read only the leading quantity, if any: present
/// and nonzero means in stock, anything else (no leading number, or a
/// leading zero) means it isn't orderable right now. The raw text is
/// kept verbatim as the summary since it's more legible to a human than
/// a number reduced back down from it.
fn parse_availability(raw: Option<&str>) -> (StockStatus, String) {
    let trimmed = raw.unwrap_or("").trim();
    if trimmed.is_empty() {
        return (StockStatus::OutOfStock, "Availability unknown".to_string());
    }
    let leading_digits: String = trimmed
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ',')
        .filter(|c| c.is_ascii_digit())
        .collect();
    let quantity: u64 = leading_digits.parse().unwrap_or(0);
    let status = if quantity > 0 {
        StockStatus::InStock
    } else {
        StockStatus::OutOfStock
    };
    (status, trimmed.to_string())
}

fn search_part(api_key: &str, mpn: &str) -> Result<MouserPart, MouserError> {
    let text = fetch_raw(api_key, mpn)?;
    parse_search_response(&text, mpn)
}

/// Fetches the raw (unparsed) JSON Mouser returns for `mpn` — useful
/// for checking `parse_search_response`'s field-name assumptions
/// against a real account (see `examples/mouser_lookup.rs`); not used
/// by [`lookup_part`] itself, which goes through [`search_part`]
/// instead so parse errors surface at the normal call site too.
pub fn fetch_raw(api_key: &str, mpn: &str) -> Result<String, MouserError> {
    let request = SearchRequest {
        search_by_keyword_request: SearchByKeywordRequest {
            keyword: mpn,
            records: 1,
            starting_record: 0,
            search_options: "",
        },
    };
    let body = serde_json::to_string(&request).expect("request body always serializes");

    let url = format!("{BASE_URL}/search/keyword?apiKey={api_key}");
    let response = ureq::post(&url)
        .header("Content-Type", "application/json")
        .send(body.as_str())
        .map_err(MouserError::from)?;

    response
        .into_body()
        .read_to_string()
        .map_err(|e| MouserError::Network(e.to_string()))
}

fn parse_search_response(text: &str, mpn: &str) -> Result<MouserPart, MouserError> {
    let parsed: SearchResponse =
        serde_json::from_str(text).map_err(|e| MouserError::MalformedResponse(e.to_string()))?;

    if let Some(err) = parsed.errors.into_iter().find(|e| !e.message.is_empty()) {
        return Err(MouserError::MalformedResponse(err.message));
    }

    let part = parsed
        .search_results
        .and_then(|r| r.parts.into_iter().next())
        .ok_or_else(|| MouserError::NotFound(mpn.to_string()))?;

    let breaks: Vec<(f64, f64)> = part
        .price_breaks
        .iter()
        .map(|b| (b.quantity, b.price.as_f64()))
        .collect();
    let (stock_status, stock_summary) = parse_availability(part.availability.as_deref());
    let lifecycle_summary = part
        .lifecycle_status
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Unknown")
        .to_string();
    let lifecycle_concern = is_lifecycle_concern(&lifecycle_summary);

    Ok(MouserPart {
        manufacturer: part.manufacturer,
        mpn: if part.manufacturer_part_number.is_empty() {
            mpn.to_string()
        } else {
            part.manufacturer_part_number
        },
        url: part.product_detail_url,
        sku: part.mouser_part_number,
        price_summary: format_price_breaks(&breaks),
        stock_status,
        stock_summary,
        lifecycle_summary,
        lifecycle_concern,
        suggested_replacement: part
            .suggested_replacement
            .unwrap_or_default()
            .trim()
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Best-corroborated shape (see module docs) — hand-written, not a
    /// live capture.
    const FIXTURE: &str = r#"{
        "Errors": [],
        "SearchResults": {
            "NumberOfResult": 1,
            "Parts": [{
                "MouserPartNumber": "595-LM358P",
                "ManufacturerPartNumber": "LM358P",
                "Manufacturer": "Texas Instruments",
                "ProductDetailUrl": "https://www.mouser.com/lm358p",
                "PriceBreaks": [
                    { "Quantity": 1, "Price": "$0.5500", "Currency": "USD" },
                    { "Quantity": 100, "Price": "$0.3200", "Currency": "USD" },
                    { "Quantity": 10, "Price": "$0.4100", "Currency": "USD" }
                ],
                "Availability": "1,934 In Stock"
            }]
        }
    }"#;

    #[test]
    fn parses_manufacturer_and_fields() {
        let part = parse_search_response(FIXTURE, "LM358P").unwrap();
        assert_eq!(part.manufacturer, "Texas Instruments");
        assert_eq!(part.mpn, "LM358P");
        assert_eq!(part.sku, "595-LM358P");
        assert_eq!(part.url, "https://www.mouser.com/lm358p");
    }

    #[test]
    fn formats_price_breaks_sorted_and_parses_dollar_strings() {
        let part = parse_search_response(FIXTURE, "LM358P").unwrap();
        assert_eq!(part.price_summary, "1:$0.55 | 10:$0.41 | 100:$0.32");
    }

    #[test]
    fn numeric_price_is_also_accepted() {
        let text = r#"{
            "SearchResults": { "Parts": [{
                "MouserPartNumber": "1", "ManufacturerPartNumber": "X",
                "Manufacturer": "Acme", "ProductDetailUrl": "",
                "PriceBreaks": [{ "Quantity": 1, "Price": 1.23 }]
            }]}
        }"#;
        let part = parse_search_response(text, "X").unwrap();
        assert_eq!(part.price_summary, "1:$1.23");
    }

    #[test]
    fn zero_results_is_not_found() {
        let empty = r#"{"SearchResults": {"NumberOfResult": 0, "Parts": []}}"#;
        let err = parse_search_response(empty, "NOSUCHPART").unwrap_err();
        assert!(matches!(err, MouserError::NotFound(mpn) if mpn == "NOSUCHPART"));
    }

    #[test]
    fn api_errors_are_surfaced() {
        let errored = r#"{"Errors": [{"Id": "1", "Code": "X", "Message": "invalid apiKey"}]}"#;
        let err = parse_search_response(errored, "X").unwrap_err();
        assert!(matches!(err, MouserError::MalformedResponse(msg) if msg == "invalid apiKey"));
    }

    #[test]
    fn missing_credentials_is_rejected_before_any_request() {
        let err = lookup_part(&MouserCredentials::default(), "X").unwrap_err();
        assert!(matches!(err, MouserError::MissingApiKey));
    }

    // ── stock availability ───────────────────────────────────────────

    #[test]
    fn parses_in_stock_availability() {
        let part = parse_search_response(FIXTURE, "LM358P").unwrap();
        assert_eq!(part.stock_status, StockStatus::InStock);
        assert_eq!(part.stock_summary, "1,934 In Stock");
    }

    #[test]
    fn zero_in_stock_text_is_out_of_stock() {
        assert_eq!(
            parse_availability(Some("0 In Stock")),
            (StockStatus::OutOfStock, "0 In Stock".to_string())
        );
    }

    #[test]
    fn non_numeric_availability_is_out_of_stock() {
        assert_eq!(
            parse_availability(Some("Non-Stocked")),
            (StockStatus::OutOfStock, "Non-Stocked".to_string())
        );
    }

    #[test]
    fn empty_availability_is_out_of_stock_and_unknown() {
        assert_eq!(
            parse_availability(Some("")),
            (StockStatus::OutOfStock, "Availability unknown".to_string())
        );
    }

    #[test]
    fn null_availability_is_out_of_stock_and_unknown() {
        // Mouser sends `"Availability": null`, not just an empty string
        // or a missing key, for some parts — must not error parsing.
        assert_eq!(
            parse_availability(None),
            (StockStatus::OutOfStock, "Availability unknown".to_string())
        );
    }

    #[test]
    fn comma_thousands_separator_is_parsed() {
        assert_eq!(
            parse_availability(Some("12,345 In Stock")).0,
            StockStatus::InStock
        );
    }

    // ── lifecycle status ─────────────────────────────────────────────

    #[test]
    fn null_lifecycle_status_defaults_to_unknown_and_no_concern() {
        // The common case in practice: `"LifecycleStatus": null` even
        // for well-stocked, active parts.
        let part = parse_search_response(FIXTURE, "LM358P").unwrap();
        assert_eq!(part.lifecycle_summary, "Unknown");
        assert!(!part.lifecycle_concern);
    }

    #[test]
    fn obsolete_lifecycle_status_is_flagged() {
        let text = r#"{
            "SearchResults": { "Parts": [{
                "MouserPartNumber": "1", "ManufacturerPartNumber": "X",
                "Manufacturer": "Acme", "ProductDetailUrl": "",
                "LifecycleStatus": "Obsolete",
                "SuggestedReplacement": "X-NEW"
            }]}
        }"#;
        let part = parse_search_response(text, "X").unwrap();
        assert_eq!(part.lifecycle_summary, "Obsolete");
        assert!(part.lifecycle_concern);
        assert_eq!(part.suggested_replacement, "X-NEW");
    }
}
