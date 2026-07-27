# Mouser + DigiKey part-info lookup

Implementation plan + progress tracker for the direct Mouser/DigiKey
parts-lookup integration. Kept in-repo so the work can be picked back up
in a later session. Supersedes `octopart-lookup.md` (removed): the
original Octopart/Nexar integration was replaced outright because
Nexar's API pricing was too expensive for this use case.

## Context

Same feature as before ("Populate BOM" — on-demand lookup that writes
`Mfr`, `Mfr #`, and per-distributor `<Vendor>`, `<Vendor> #`,
`<Vendor> Qty/Price` properties onto symbols already in the project),
now backed by direct Mouser and DigiKey developer APIs instead of one
paid aggregator. Arrow is dropped — Octopart aggregated it as a bonus;
there's no direct Arrow API in scope here.

Every other decision already made for this feature carries over
unchanged:
1. Credentials stored as plaintext JSON in the same global,
   non-project config file (`GlobalSettings`, `~/.config/kicad-auto-importer/settings.json`
   on Linux).
2. Manual, on-demand trigger via "Populate BOM".
3. MPN source: existing MPN-like property, else the symbol's own name.
4. Price fields: compact multi-break string, e.g. `"1:$1.23 | 10:$1.05 | 100:$0.89"`.

**Confidence caveat (resolved):** the field names below were originally
reconstructed from public community wrapper libraries and blog posts,
not a live authenticated call. Both were since verified against real
accounts via `examples/mouser_lookup.rs`/`digikey_lookup.rs` (MPN
`LM358P`) — every guessed field name matched on the first try for both
vendors, including `Price` genuinely being a `"$0.32"`-style string in
Mouser's real response (confirming the defensive string-or-number
parsing was the right call, not overcaution). No parsing changes were
needed.

## Verified API details

**Mouser Search API v1** — `https://api.mouser.com/api/v1`, `apiKey` as
a query parameter (no OAuth). `POST /search/keyword?apiKey=<key>`, body
`{"SearchByKeywordRequest": {"keyword": "<mpn>", "records": 1, "startingRecord": 0, "searchOptions": ""}}`.
Response: `{"Errors": [...], "SearchResults": {"NumberOfResult": N, "Parts": [{"MouserPartNumber", "ManufacturerPartNumber", "Manufacturer", "ProductDetailUrl", "PriceBreaks": [{"Quantity", "Price", "Currency"}]}]}}`.
`Price` handled as either a `"$1.23"`-style string or a bare number.

**DigiKey Product Information API v4** — OAuth2 client-credentials
(2-legged): `POST https://api.digikey.com/v1/oauth2/token`, form body
`client_id`/`client_secret`/`grant_type=client_credentials` → JSON
`{"access_token", "expires_in" (~600s), "token_type"}`. Search:
`POST https://api.digikey.com/products/v4/search/keyword`, headers
`X-DIGIKEY-Client-Id`, `Authorization: Bearer <token>`; body
`{"Keywords": "<mpn>", "Limit": 1}`. Response:
`{"Products": [{"ManufacturerProductNumber", "Manufacturer": {"Name"}, "ProductUrl", "ProductVariations": [{"DigiKeyProductNumber", "StandardPricing": [{"BreakQuantity", "UnitPrice"}]}]}]}`.

## Design decisions

- **Three files replace `octopart.rs`**: `crates/core/src/mouser.rs`
  and `crates/core/src/digikey.rs` are independent, self-contained
  vendor API clients (own credentials struct, own error enum, own
  `lookup_part` fn) — neither knows the other exists. `crates/core/src/parts_lookup.rs`
  is the only module `crates/app` talks to: it owns `PartsCredentials`/
  `PartInfo`/`VendorOffer`, calls whichever vendor client(s) have
  credentials configured, and merges the results.
- **Per-vendor independence, not all-or-nothing**: each vendor is
  queried separately (unlike Octopart's single aggregated query), so
  `parts_lookup::combine_results` treats a configured-but-failing
  vendor as a warning rather than aborting the whole lookup — a lookup
  succeeds as long as *at least one* configured vendor returns data.
  `PartInfo` gained a `warnings: Vec<String>` field for this
  (`part_lookup_ui.rs` logs each one, e.g. `"DigiKey: not found"`, even
  on an otherwise-successful row).
- **DigiKey token caching** ports the exact `CachedToken`/
  `TOKEN_CACHE`/`cached_token_is_valid` pure-function pattern from the
  old `octopart.rs` verbatim, just with a shorter 30s safety margin
  (proportional to DigiKey's ~600s token lifetime vs. Nexar's 3600s).
  Mouser needs no token cache at all — it's a static API key.
- **`resolve_mpn`/`apply_part_info`** ported verbatim into
  `parts_lookup.rs` — both were already fully vendor-agnostic.
- **Shared price formatting**: `parts_lookup::format_price_breaks` is
  `pub(crate)`; both `mouser.rs` and `digikey.rs` reduce their
  vendor-specific price shapes down to `Vec<(f64, f64)>` before calling
  it, so the sort/cap/format logic lives in exactly one place.
- **`GlobalSettings`** fields renamed outright (`octopart_client_id`/
  `octopart_client_secret` → `mouser_api_key`/`digikey_client_id`/
  `digikey_client_secret`) — a full provider swap, not a rename, so no
  migration: an old `settings.json` just loads with the new fields
  blank (`#[serde(default)]` already makes that a non-issue).

## Progress

- [x] `crates/core/src/mouser.rs` (new) — `MouserCredentials`,
      `MouserPart`, `MouserError`, `lookup_part`. 6 unit tests (fixture
      parsing, price-as-string vs. price-as-number, zero-results,
      API-level errors, missing-credentials guard).
- [x] `crates/core/src/digikey.rs` (new) — `DigikeyCredentials`,
      `DigikeyPart`, `DigikeyError`, token cache ported from
      `octopart.rs`, `lookup_part`. 8 unit tests (fixture parsing,
      zero-results, missing-credentials guard, 4 token-cache cases).
- [x] `crates/core/src/parts_lookup.rs` (new, replaces `octopart.rs`) —
      `PartsCredentials`, `VendorOffer`, `PartInfo` (+`warnings`),
      `PartsLookupError`, `lookup_part_info`, `combine_results`,
      `apply_part_info`, `resolve_mpn`, `format_price_breaks`. 12 unit
      tests (merge logic across both-ok/one-fails/neither-configured/
      both-fail, price formatting, `resolve_mpn` cases ported
      unchanged).
- [x] `crates/core/src/octopart.rs` deleted.
- [x] `crates/core/src/lib.rs` — module list updated.
- [x] `crates/core/src/global_settings.rs` — fields renamed, tests
      updated.
- [x] `crates/core/src/symbol_importer.rs` — doc comment reworded.
- [x] `crates/app/src/ui.rs` — `MainApp` fields renamed/expanded to
      three; `octopart_settings_button` → `vendor_settings_button`
      (Mouser API Key + DigiKey Client ID/Secret rows, links to
      mouser.com/api-search and developer.digikey.com); credential
      construction at the `part_lookup_ui::show` call site updated.
- [x] `crates/app/src/part_lookup_ui.rs` — imports, credential guard,
      `run_lookup_batch` (now also logs `info.warnings` per row), `show`
      signature all updated; "Arrow" dropped from doc comments/strings.
- [x] `crates/core/examples/mouser_lookup.rs` and `digikey_lookup.rs` —
      standalone CLI test tools, independent of the GUI app. Each prints
      the raw JSON response next to the parsed struct so a field-name
      mismatch is immediately visible. Usage:
      `MOUSER_API_KEY=<key> cargo run -p kicad-auto-importer-core --example mouser_lookup -- <MPN>`
      and
      `DIGIKEY_CLIENT_ID=<id> DIGIKEY_CLIENT_SECRET=<secret> cargo run -p kicad-auto-importer-core --example digikey_lookup -- <MPN>`.
      Both go through `mouser::fetch_raw`/`digikey::fetch_raw`, new
      `pub` functions that split the existing fetch-then-parse
      internals so the raw text is inspectable — `lookup_part` itself is
      unchanged.
- [x] `crates/core/examples/common/mod.rs` — shared by the two example
      CLIs only (not the library itself). Reads credentials from
      `~/.kicadautoimporterrc` (`[mouser]`/`api_key`,
      `[digikey]`/`client_id`+`client_secret`) as a fallback when the
      corresponding env var isn't set, so credentials only need typing
      once. Lives at `examples/common/mod.rs` specifically so Cargo's
      example auto-discovery doesn't treat it as a third example binary.
- [x] `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`,
      `cargo fmt --all -- --check`.
- [x] **Live-verified against real accounts** (2026-07-27, MPN
      `LM358P`, credentials from `~/.kicadautoimporterrc`): both vendors
      returned real data and every field parsed correctly on the first
      try — no struct/field-name changes were needed for either. Mouser
      needed a corrected API key mid-verification (its first key was a
      wrong/inactive one, a credentials issue, not a parsing bug — the
      structured `Errors[]`/`Message` surfacing worked exactly as
      designed for that case too).
