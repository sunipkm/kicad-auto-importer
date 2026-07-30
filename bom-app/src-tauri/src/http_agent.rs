//! Shared `ureq` agent for `mouser`/`digikey` — `ureq`'s bare
//! `ureq::post(...)`/`ureq::get(...)` free functions use an agent with
//! *no* timeouts configured at all (DNS, connect, and response-body
//! reads are all unbounded), so a network hiccup (a firewall silently
//! dropping packets, a stalled DNS resolver, an unreachable vendor
//! endpoint) hangs the calling thread forever instead of surfacing as
//! an error. Both vendor clients build their requests off [`agent`]
//! instead, which bounds the whole call (DNS through body) with
//! [`GLOBAL_TIMEOUT`].

use std::sync::OnceLock;
use std::time::Duration;

/// End-to-end budget for a single vendor HTTP call — generous enough
/// for a slow API response, short enough that a stuck call fails
/// visibly (as a `Transport`/timeout error on that one part) rather
/// than hanging the batch it's part of indefinitely.
const GLOBAL_TIMEOUT: Duration = Duration::from_secs(30);

pub fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .timeout_global(Some(GLOBAL_TIMEOUT))
                .build(),
        )
    })
}
