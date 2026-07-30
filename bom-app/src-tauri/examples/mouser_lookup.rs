//! Standalone CLI to test `bom_app_lib::mouser` against a real Mouser
//! account. Prints the raw JSON response alongside the parsed
//! `MouserPart`, so a field-name mismatch between what's hard-coded in
//! `mouser.rs` and what Mouser's API actually returns is immediately
//! visible — see `docs/plans/parts-lookup.md`'s confidence caveat.
//!
//! Usage:
//!   MOUSER_API_KEY=<key> cargo run -p bom-app --example mouser_lookup -- <MPN>
//!
//! `MOUSER_API_KEY` may be omitted if `~/.kicadautoimporterrc` has a
//! `[mouser]`/`api_key` entry instead — see `examples/common/mod.rs`.

#[path = "common/mod.rs"]
mod common;

use bom_app_lib::mouser::{self, MouserCredentials};

fn main() {
    let Some(mpn) = std::env::args().nth(1) else {
        eprintln!(
            "Usage: MOUSER_API_KEY=<key> cargo run -p bom-app --example mouser_lookup -- <MPN>"
        );
        std::process::exit(1);
    };
    let api_key = common::credential("MOUSER_API_KEY", "mouser", "api_key");
    if api_key.trim().is_empty() {
        eprintln!("Set MOUSER_API_KEY, or add [mouser]/api_key to ~/.kicadautoimporterrc, first.");
        std::process::exit(1);
    }

    println!("Looking up '{mpn}' via Mouser...\n");

    println!("--- raw response ---");
    match mouser::fetch_raw(&api_key, &mpn) {
        Ok(raw) => print_pretty_json(&raw),
        Err(exc) => {
            eprintln!("Request failed: {exc}");
            std::process::exit(1);
        }
    }

    println!("\n--- parsed result ---");
    let creds = MouserCredentials { api_key };
    match mouser::lookup_part(&creds, &mpn) {
        Ok(part) => println!("{part:#?}"),
        Err(exc) => eprintln!("Parsing failed: {exc}"),
    }
}

fn print_pretty_json(raw: &str) {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(value) => println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| raw.to_string())
        ),
        Err(_) => println!("{raw}"),
    }
}
