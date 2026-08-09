//! Arrow Electronics API client — manufacturer and pricing info for a
//! manufacturer part number (MPN), used as one of three vendor sources
//! merged by `crate::parts_lookup`.
//!
//! Authentication uses Arrow's API key + part search/detail endpoints
//! documented in their public developer portal. Similar to Mouser (simple
//! API key auth), but different response structure.

use serde::Deserialize;

use crate::parts_lookup::{format_price_breaks, is_lifecycle_concern, StockStatus};

const BASE_URL: &str = "https://api.arrow.com/v1";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ArrowCredentials {
    pub api_key: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ArrowPart {
    pub manufacturer: String,
    pub mpn: String,
    pub description: String,
    pub url: String,
    pub sku: String,
    pub price_summary: String,
    pub stock_status: StockStatus,
    pub stock_summary: String,
    pub stock_quantity: u64,
    pub lifecycle_summary: String,
    pub lifecycle_concern: bool,
    pub suggested_replacement: String,
    pub price_breaks: Vec<(f64, f64)>,
}

#[derive(Debug, thiserror::Error)]
pub enum ArrowError {
    #[error("no Arrow API key is set")]
    MissingApiKey,
    #[error("Arrow rejected the API key (401) — check the key in settings")]
    AuthFailed,
    #[error("Arrow rate-limited this request (429) — try again shortly")]
    RateLimited,
    #[error("Arrow returned HTTP {0}")]
    Http(u16),
    #[error("network error talking to Arrow: {0}")]
    Network(String),
    #[error("no match found for '{0}'")]
    NotFound(String),
    #[error("could not parse Arrow's response: {0}")]
    MalformedResponse(String),
}

impl From<ureq::Error> for ArrowError {
    fn from(err: ureq::Error) -> Self {
        match err {
            ureq::Error::StatusCode(401) => ArrowError::AuthFailed,
            ureq::Error::StatusCode(429) => ArrowError::RateLimited,
            ureq::Error::StatusCode(code) => ArrowError::Http(code),
            other => ArrowError::Network(other.to_string()),
        }
    }
}

pub fn lookup_part(creds: &ArrowCredentials, mpn: &str) -> Result<ArrowPart, ArrowError> {
    if creds.api_key.trim().is_empty() {
        return Err(ArrowError::MissingApiKey);
    }
    search_parts_impl(&creds.api_key, mpn)?
        .into_iter()
        .next()
        .ok_or_else(|| ArrowError::NotFound(mpn.to_string()))
}

pub fn search_parts(
    api_key: &str,
    mpn: &str,
    max_results: u32,
) -> Result<Vec<ArrowPart>, ArrowError> {
    if api_key.trim().is_empty() {
        return Err(ArrowError::MissingApiKey);
    }
    search_parts_impl(api_key, mpn).map(|mut parts| {
        parts.truncate(max_results as usize);
        parts
    })
}

pub fn test_credentials(api_key: &str) -> Result<(), ArrowError> {
    if api_key.trim().is_empty() {
        return Err(ArrowError::MissingApiKey);
    }
    match search_parts_impl(api_key, "test") {
        Ok(_) | Err(ArrowError::NotFound(_)) => Ok(()),
        Err(other) => Err(other),
    }
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    results: Vec<SearchResult>,
    #[serde(default)]
    errors: Vec<ApiError>,
}

#[derive(Deserialize)]
struct ApiError {
    message: Option<String>,
}

#[derive(Deserialize)]
struct SearchResult {
    #[serde(default)]
    manufacturer_part_number: String,
    #[serde(default)]
    manufacturer: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    web_link: String,
    #[serde(default)]
    arrow_part_number: String,
    #[serde(default)]
    availability: PriceAvailability,
    #[serde(default)]
    lifecycle_status: String,
}

#[derive(Deserialize, Default)]
struct PriceAvailability {
    #[serde(default)]
    quantity_on_hand: u64,
    #[serde(default)]
    price_breaks: Vec<PriceBreak>,
    #[serde(default)]
    qualified_in_stock: bool,
}

#[derive(Deserialize)]
struct PriceBreak {
    #[serde(default)]
    quantity: f64,
    #[serde(default)]
    unit_price: f64,
}

fn search_parts_impl(api_key: &str, mpn: &str) -> Result<Vec<ArrowPart>, ArrowError> {
    let url = format!(
        "{}/search?search_term={}&apiKey={}",
        BASE_URL,
        mpn.trim().replace(" ", "+"),
        api_key.trim()
    );

    let response_body = crate::http_agent::agent()
        .get(&url)
        .call()
        .map_err(ArrowError::from)?
        .into_body()
        .read_to_string()
        .map_err(|e| ArrowError::Network(e.to_string()))?;

    let response: SearchResponse = serde_json::from_str(&response_body)
        .map_err(|e| ArrowError::MalformedResponse(e.to_string()))?;

    if !response.errors.is_empty() {
        let msg = response
            .errors
            .first()
            .and_then(|e| e.message.as_ref())
            .map(|s| s.as_str())
            .unwrap_or("unknown error");
        return Err(ArrowError::MalformedResponse(msg.to_string()));
    }

    if response.results.is_empty() {
        return Err(ArrowError::NotFound(mpn.to_string()));
    }

    let parts = response
        .results
        .into_iter()
        .map(|result| {
            let stock_status = if result.availability.qualified_in_stock {
                StockStatus::InStock
            } else {
                StockStatus::OutOfStock
            };

            let stock_summary = format!("{} In Stock", result.availability.quantity_on_hand);
            let lifecycle_concern = is_lifecycle_concern(&result.lifecycle_status);

            let price_breaks: Vec<(f64, f64)> = result
                .availability
                .price_breaks
                .into_iter()
                .map(|pb| (pb.quantity, pb.unit_price))
                .collect();

            let price_summary = format_price_breaks(&price_breaks);

            ArrowPart {
                manufacturer: result.manufacturer,
                mpn: result.manufacturer_part_number,
                description: result.description,
                url: result.web_link,
                sku: result.arrow_part_number,
                price_summary,
                stock_status,
                stock_summary,
                stock_quantity: result.availability.quantity_on_hand,
                lifecycle_summary: result.lifecycle_status,
                lifecycle_concern,
                suggested_replacement: String::new(),
                price_breaks,
            }
        })
        .collect();

    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_api_key_returns_error() {
        let creds = ArrowCredentials {
            api_key: String::new(),
        };
        assert!(lookup_part(&creds, "LM358P").is_err());
    }

    #[test]
    fn test_credentials_requires_api_key() {
        assert!(test_credentials("").is_err());
    }
}
