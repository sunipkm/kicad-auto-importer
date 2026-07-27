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

use crate::parts_lookup::format_price_breaks;

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
                ]
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
}
