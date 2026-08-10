//! A global (not per-project), local cache of *raw* vendor search
//! results — every candidate [`crate::parts_lookup::VendorCandidate`]
//! Mouser/DigiKey returned for a given search string (MPN, or a
//! generic-passive fallback like `"R 10k"` — see
//! `parts_lookup::resolve_mpn`), keyed by that search string.
//!
//! Distinct from the existing per-instance schematic-property cache
//! (`populate_bom::LAST_CHECKED_PROPERTY` et al.): that one stores the
//! single *already-chosen* offer(s) for one specific placed symbol, and
//! only pays off for that exact instance. This one stores every raw
//! candidate for a *search string*, shared across every project and
//! every reference that happens to search for the same part — so 20
//! placements of the same resistor MPN in one project, or the same MPN
//! looked up again in a different project next week, cost one API call
//! between them instead of one each. `populate_bom`/`generate_bom`
//! check this cache before ever calling
//! `parts_lookup::lookup_part_candidates`, and score its contents (see
//! `parts_lookup::score_candidates`) to pick a vendor+part
//! automatically instead of trusting whichever result an API happened
//! to return first.
//!
//! Lives in the same global config directory as bom-app's own
//! settings file (`dirs::config_dir()/kicad-bom-tool/parts_cache`)
//! using sled — a pure-Rust embedded database — for the same reason:
//! a vendor's catalog data for a given MPN has nothing to do with any
//! one project.
//!
//! Writes are deliberately narrow: [`PartsCache::save`] only ever
//! upserts the specific search-string keys this process actually
//! *put* (i.e. freshly (re-)fetched because they were missing or
//! stale) — never a wholesale overwrite of the database. It re-reads the
//! latest entries before writing and merges just those keys on top, so
//! entries belonging to other projects, or entries this run found
//! still-fresh and skipped, are never touched or clobbered — including
//! ones written concurrently by another process between this process's
//! own load and save.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::parts_lookup::CandidateSet;

const CACHE_DIR: &str = "parts_cache";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedEntry {
    /// RFC 3339 timestamp of when this entry was fetched — same string
    /// format as `populate_bom::LAST_CHECKED_PROPERTY`, for the same
    /// reason (human-readable, and `chrono` parses it straight back).
    fetched_at: String,
    candidate_set: CandidateSet,
}

/// Cache keys are case/whitespace-normalized search strings — the
/// vendor APIs themselves are effectively case-insensitive, and this
/// keeps `"lm358p"` and `"LM358P"` from ever being treated as two
/// different cache entries.
fn normalize_key(search_string: &str) -> String {
    search_string.trim().to_uppercase()
}

pub struct PartsCache {
    db: sled::Db,
    /// Only the entries this instance itself has `put` — the *only*
    /// thing `save` ever writes, see module docs.
    dirty: Vec<String>,
}

impl PartsCache {
    fn db_path() -> Option<PathBuf> {
        Some(dirs::config_dir()?.join("kicad-bom-tool").join(CACHE_DIR))
    }

    /// Never fails — same philosophy as `VendorCredentials::load`: no
    /// database dir, no config dir, or corrupt database all just mean
    /// "start with an empty cache" rather than an error every caller has
    /// to handle.
    pub fn load() -> Self {
        let db = if let Some(path) = Self::db_path() {
            let _ = std::fs::create_dir_all(path.parent().unwrap_or(&PathBuf::from(".")));
            sled::open(&path)
                .unwrap_or_else(|_| sled::Config::new().temporary(true).open().expect("tmp DB"))
        } else {
            sled::Config::new().temporary(true).open().expect("tmp DB")
        };

        PartsCache {
            db,
            dirty: Vec::new(),
        }
    }

    /// The cached candidate set for `search_string`, if present and
    /// fetched less than `max_age` ago relative to `now`. An
    /// unparseable `fetched_at` (shouldn't happen — only this module
    /// ever writes it — but treated the same as "no cache" rather than
    /// panicking) counts as a miss.
    pub fn get_fresh(
        &self,
        search_string: &str,
        now: chrono::DateTime<chrono::Utc>,
        max_age: chrono::Duration,
    ) -> Option<&CandidateSet> {
        let key = normalize_key(search_string);
        let entry_bytes = self.db.get(key.as_bytes()).ok()??;
        let entry: CachedEntry = serde_json::from_slice(&entry_bytes).ok()?;
        let fetched_at = chrono::DateTime::parse_from_rfc3339(&entry.fetched_at).ok()?;

        if now.signed_duration_since(fetched_at) < max_age {
            Some(Box::leak(Box::new(entry.candidate_set)))
        } else {
            None
        }
    }

    /// Records a freshly-fetched candidate set for `search_string`,
    /// visible to this instance's own subsequent `get_fresh` calls
    /// immediately (e.g. a second placement of the same MPN later in
    /// the same batch) and persisted the next time `save` runs.
    pub fn put(
        &mut self,
        search_string: &str,
        now: chrono::DateTime<chrono::Utc>,
        candidate_set: CandidateSet,
    ) {
        let key = normalize_key(search_string);
        let entry = CachedEntry {
            fetched_at: now.to_rfc3339(),
            candidate_set,
        };

        if let Ok(entry_json) = serde_json::to_vec(&entry) {
            let _ = self.db.insert(key.clone().as_bytes(), entry_json);
            self.dirty.push(key);
        }
    }

    /// Merges only this instance's own `put` entries with the *current*
    /// database and persists. A no-op if nothing was ever `put`, which is
    /// the common case for a batch where every part was already fresh.
    pub fn save(&self) -> std::io::Result<()> {
        if self.dirty.is_empty() {
            return Ok(());
        }

        self.db
            .flush()
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parts_lookup::{StockStatus, VendorCandidate, VendorOffer};

    fn sample_candidate_set() -> CandidateSet {
        CandidateSet {
            candidates: vec![VendorCandidate {
                manufacturer: "Texas Instruments".to_string(),
                mpn: "LM358P".to_string(),
                description: "Dual Op Amp".to_string(),
                offer: VendorOffer {
                    seller: "Mouser".to_string(),
                    url: "https://mouser.com/lm358p".to_string(),
                    sku: "595-LM358P".to_string(),
                    price_summary: "1:$0.55".to_string(),
                    stock_status: StockStatus::InStock,
                    stock_summary: "1,934 In Stock".to_string(),
                    stock_quantity: 1934,
                    lifecycle_summary: "Active".to_string(),
                    lifecycle_concern: false,
                    suggested_replacement: String::new(),
                    price_breaks: vec![(1.0, 0.55)],
                },
            }],
            warnings: Vec::new(),
        }
    }

    fn test_cache() -> PartsCache {
        let db = sled::Config::new().temporary(true).open().expect("tmp DB");
        PartsCache {
            db,
            dirty: Vec::new(),
        }
    }

    #[test]
    fn get_fresh_is_none_when_never_put() {
        let cache = test_cache();
        assert!(cache
            .get_fresh("LM358P", chrono::Utc::now(), chrono::Duration::hours(24))
            .is_none());
    }

    #[test]
    fn put_then_get_fresh_round_trips_immediately() {
        let mut cache = test_cache();
        let now = chrono::Utc::now();
        cache.put("LM358P", now, sample_candidate_set());
        let got = cache
            .get_fresh("lm358p", now, chrono::Duration::hours(24))
            .expect("just-put entry should be fresh");
        assert_eq!(got.candidates.len(), 1);
    }

    #[test]
    fn get_fresh_is_none_once_older_than_max_age() {
        let mut cache = test_cache();
        let fetched_at = chrono::Utc::now() - chrono::Duration::hours(25);
        cache.put("LM358P", fetched_at, sample_candidate_set());
        let now = chrono::Utc::now();
        assert!(cache
            .get_fresh("LM358P", now, chrono::Duration::hours(24))
            .is_none());
    }

    #[test]
    fn cache_key_normalization_ignores_case_and_surrounding_whitespace() {
        let mut cache = test_cache();
        let now = chrono::Utc::now();
        cache.put("  lm358p  ", now, sample_candidate_set());
        assert!(cache
            .get_fresh("LM358P", now, chrono::Duration::hours(24))
            .is_some());
    }

    #[test]
    fn save_is_a_noop_when_nothing_was_put() {
        let cache = test_cache();
        assert!(cache.save().is_ok());
    }
}
