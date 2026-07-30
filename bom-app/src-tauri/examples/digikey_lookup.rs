//! Standalone CLI to test `bom_app_lib::digikey` against a real DigiKey
//! account. Prints the raw JSON response alongside the parsed
//! `DigikeyPart`, so a field-name mismatch between what's hard-coded in
//! `digikey.rs` and what DigiKey's API actually returns is immediately
//! visible — see `docs/plans/parts-lookup.md`'s confidence caveat.
//!
//! Usage:
//!   DIGIKEY_CLIENT_ID=<id> DIGIKEY_CLIENT_SECRET=<secret> \
//!     cargo run -p bom-app --example digikey_lookup -- <MPN>
//!
//! Both env vars may be omitted if `~/.kicadautoimporterrc` has
//! `[digikey]`/`client_id`+`client_secret` entries instead — see
//! `examples/common/mod.rs`.

#[path = "common/mod.rs"]
mod common;

use bom_app_lib::digikey::{self, DigikeyCredentials};

fn main() {
    let Some(mpn) = std::env::args().nth(1) else {
        eprintln!(
            "Usage: DIGIKEY_CLIENT_ID=<id> DIGIKEY_CLIENT_SECRET=<secret> cargo run -p bom-app --example digikey_lookup -- <MPN>"
        );
        std::process::exit(1);
    };
    let client_id = common::credential("DIGIKEY_CLIENT_ID", "digikey", "client_id");
    let client_secret = common::credential("DIGIKEY_CLIENT_SECRET", "digikey", "client_secret");
    if client_id.trim().is_empty() || client_secret.trim().is_empty() {
        eprintln!(
            "Set DIGIKEY_CLIENT_ID/DIGIKEY_CLIENT_SECRET, or add [digikey]/client_id+client_secret to ~/.kicadautoimporterrc, first."
        );
        std::process::exit(1);
    }
    let creds = DigikeyCredentials {
        client_id,
        client_secret,
    };

    println!("Looking up '{mpn}' via DigiKey...\n");

    println!("--- raw response ---");
    match digikey::fetch_raw(&creds, &mpn) {
        Ok(raw) => print_pretty_json(&raw),
        Err(exc) => {
            eprintln!("Request failed: {exc}");
            std::process::exit(1);
        }
    }

    println!("\n--- parsed result ---");
    match digikey::lookup_part(&creds, &mpn) {
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
