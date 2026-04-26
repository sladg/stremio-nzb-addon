use moka::future::Cache;
use once_cell::sync::Lazy;
use std::sync::Arc;
use std::time::Duration;

use crate::nzb_api::RssChannel;
use crate::nzb_availability::AvailabilityResult;
use crate::nzb_sanity::SanityResult;

/// Search-API result cache. 12h TTL — search results barely change for
/// older content; the bulk of churn happens in the first few hours after
/// a release. 12h means a typical "click around in the morning, watch in
/// the evening" pattern stays cache-warm without a re-search.
pub static RSS_CACHE: Lazy<Cache<String, Arc<RssChannel>>> = Lazy::new(|| {
    Cache::builder()
        .max_capacity(10_000)
        .time_to_live(Duration::from_secs(12 * 3600))
        .build()
});

/// Sanity-check verdict cache. 7 days — a release's RAR-vs-Flat structure
/// is immutable once posted; a stale verdict can only stay correct.
pub static SANITY_CACHE: Lazy<Cache<String, SanityResult>> = Lazy::new(|| {
    Cache::builder()
        .max_capacity(50_000)
        .time_to_live(Duration::from_secs(7 * 86400))
        .build()
});

/// Verdict cache for `nzb_availability` BODY-probes. Mirrors TS
/// `nzb-avail:{sha1(nzbUrl + "|" + sortedServers)}` keys with 24 h TTL.
pub static AVAILABILITY_CACHE: Lazy<Cache<String, AvailabilityResult>> = Lazy::new(|| {
    Cache::builder()
        .max_capacity(50_000)
        .time_to_live(Duration::from_secs(86400))
        .build()
});

/// Build cache key with namespace, mirroring src/cache.ts:63.
/// Format: `nzb:{sanitized_url}:{type}:{id}`
pub fn build_cache_key(indexer_url: &str, kind: &str, id: &str) -> String {
    let sanitized = indexer_url
        .strip_prefix("https://")
        .or_else(|| indexer_url.strip_prefix("http://"))
        .unwrap_or(indexer_url)
        .trim_end_matches('/');
    format!("nzb:{sanitized}:{kind}:{id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_https_and_trailing_slash() {
        assert_eq!(
            build_cache_key("https://api.example.com/", "movie", "tt0133093"),
            "nzb:api.example.com:movie:tt0133093"
        );
    }

    #[test]
    fn strips_http_prefix() {
        assert_eq!(
            build_cache_key("http://localhost:8080", "search", "matrix"),
            "nzb:localhost:8080:search:matrix"
        );
    }

    #[test]
    fn leaves_other_schemes_alone() {
        assert_eq!(
            build_cache_key("nntps://news.example.com", "x", "y"),
            "nzb:nntps://news.example.com:x:y"
        );
    }
}
