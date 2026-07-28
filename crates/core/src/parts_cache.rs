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
//! Lives in the same global config location as [`crate::global_settings::GlobalSettings`]
//! (`dirs::config_dir()/kicad-auto-importer/parts_cache.json`) for the
//! same reason: a vendor's catalog data for a given MPN has nothing to
//! do with any one project.
//!
//! Writes are deliberately narrow: [`PartsCache::save`] only ever
//! upserts the specific search-string keys this process actually
//! *put* (i.e. freshly (re-)fetched because they were missing or
//! stale) — never a wholesale overwrite of the file. It re-reads the
//! latest on-disk contents immediately before writing and merges just
//! those keys on top, so entries belonging to other projects, or
//! entries this run found still-fresh and skipped, are never touched
//! or clobbered — including ones written concurrently by another
//! process between this process's own load and save.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::parts_lookup::CandidateSet;

const CACHE_FILENAME: &str = "parts_cache.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct CachedCandidates {
    /// RFC 3339 timestamp of when this entry was fetched — same string
    /// format as `populate_bom::LAST_CHECKED_PROPERTY`, for the same
    /// reason (human-readable, and `chrono` parses it straight back).
    fetched_at: String,
    candidate_set: CandidateSet,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CacheFile {
    #[serde(default)]
    entries: HashMap<String, CachedCandidates>,
}

/// Cache keys are case/whitespace-normalized search strings — the
/// vendor APIs themselves are effectively case-insensitive, and this
/// keeps `"lm358p"` and `"LM358P"` from ever being treated as two
/// different cache entries.
fn normalize_key(search_string: &str) -> String {
    search_string.trim().to_uppercase()
}

pub struct PartsCache {
    /// Full snapshot as loaded at construction time — read from
    /// throughout a batch via `get_fresh`.
    entries: HashMap<String, CachedCandidates>,
    /// Only the entries this instance itself has `put` — the *only*
    /// thing `save` ever writes, see module docs.
    dirty: HashMap<String, CachedCandidates>,
}

impl PartsCache {
    fn cache_path() -> Option<PathBuf> {
        Some(
            dirs::config_dir()?
                .join("kicad-auto-importer")
                .join(CACHE_FILENAME),
        )
    }

    fn read_file() -> CacheFile {
        Self::cache_path()
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Never fails — same philosophy as `GlobalSettings::load`: no
    /// cache file, no config dir, or corrupt JSON all just mean "start
    /// with an empty cache" rather than an error every caller has to
    /// handle.
    pub fn load() -> Self {
        PartsCache {
            entries: Self::read_file().entries,
            dirty: HashMap::new(),
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
        let entry = self.entries.get(&normalize_key(search_string))?;
        let fetched_at = chrono::DateTime::parse_from_rfc3339(&entry.fetched_at).ok()?;
        if now.signed_duration_since(fetched_at) < max_age {
            Some(&entry.candidate_set)
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
        let entry = CachedCandidates {
            fetched_at: now.to_rfc3339(),
            candidate_set,
        };
        self.entries.insert(key.clone(), entry.clone());
        self.dirty.insert(key, entry);
    }

    /// Merges only this instance's own `put` entries onto the *current*
    /// on-disk file (re-read here, not the snapshot `load` saw) and
    /// writes it back. A no-op — no read, no write — if nothing was
    /// ever `put`, which is the common case for a batch where every
    /// part was already fresh.
    pub fn save(&self) -> std::io::Result<()> {
        if self.dirty.is_empty() {
            return Ok(());
        }
        let Some(path) = Self::cache_path() else {
            return Ok(());
        };
        let mut on_disk = Self::read_file();
        for (key, entry) in &self.dirty {
            on_disk.entries.insert(key.clone(), entry.clone());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(&on_disk)?;
        fs::write(path, text)
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

    #[test]
    fn get_fresh_is_none_when_never_put() {
        let cache = PartsCache {
            entries: HashMap::new(),
            dirty: HashMap::new(),
        };
        assert!(cache
            .get_fresh("LM358P", chrono::Utc::now(), chrono::Duration::hours(24))
            .is_none());
    }

    #[test]
    fn put_then_get_fresh_round_trips_immediately() {
        let mut cache = PartsCache {
            entries: HashMap::new(),
            dirty: HashMap::new(),
        };
        let now = chrono::Utc::now();
        cache.put("LM358P", now, sample_candidate_set());
        let got = cache
            .get_fresh("lm358p", now, chrono::Duration::hours(24))
            .expect("just-put entry should be fresh");
        assert_eq!(got.candidates.len(), 1);
    }

    #[test]
    fn get_fresh_is_none_once_older_than_max_age() {
        let mut cache = PartsCache {
            entries: HashMap::new(),
            dirty: HashMap::new(),
        };
        let fetched_at = chrono::Utc::now() - chrono::Duration::hours(25);
        cache.put("LM358P", fetched_at, sample_candidate_set());
        let now = chrono::Utc::now();
        assert!(cache
            .get_fresh("LM358P", now, chrono::Duration::hours(24))
            .is_none());
    }

    #[test]
    fn cache_key_normalization_ignores_case_and_surrounding_whitespace() {
        let mut cache = PartsCache {
            entries: HashMap::new(),
            dirty: HashMap::new(),
        };
        let now = chrono::Utc::now();
        cache.put("  lm358p  ", now, sample_candidate_set());
        assert!(cache
            .get_fresh("LM358P", now, chrono::Duration::hours(24))
            .is_some());
    }

    #[test]
    fn save_is_a_noop_when_nothing_was_put() {
        // No config dir manipulation needed: an empty `dirty` map must
        // short-circuit before ever touching the filesystem.
        let cache = PartsCache {
            entries: HashMap::new(),
            dirty: HashMap::new(),
        };
        assert!(cache.save().is_ok());
    }
}
