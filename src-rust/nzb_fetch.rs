//! Shared NZB-XML fetcher with byte-level caching and retry-with-backoff.
//!
//! Both `nzb_sanity` (during /stream listing) and `streaming::preflight`
//! (when the user clicks play) need the same .nzb XML bytes for the same
//! candidate URL, often within seconds of each other. The previous
//! implementation fetched twice — wasteful at best and a hard failure when
//! the indexer's getnzb endpoint rate-limits between the two calls.
//!
//! This module:
//!   - Caches successful XML payloads in moka, keyed by sha1(URL),
//!     so the second consumer reuses the first's bytes for free.
//!   - Retries 5xx and network errors with exponential backoff (3 attempts:
//!     0 / 200ms / 600ms). 4xx is treated as deterministic — no retry.
//!   - Caches *only successes*. Failures fall through and re-attempt on
//!     the next call, so transient outages self-heal once the indexer
//!     recovers.
//!   - **Negative cache for indexer-side cap errors.** The first 5xx for
//!     a given indexer host trips a 5-min cooldown — subsequent fetches
//!     to that host short-circuit with a fast error, so a single search
//!     that finds the indexer capped doesn't burn 30+ wasted retries.

use moka::future::Cache;
use once_cell::sync::Lazy;
use sha1::{Digest, Sha1};
use std::sync::Arc;
use std::time::{Duration, Instant};
use url::Url;

/// Per-fetch HTTP timeout. Same value the previous standalone fetchers used.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// Hard upper bound on .nzb XML size — any larger is almost certainly a
/// misbehaving indexer or a malformed response.
pub const MAX_NZB_SIZE: u64 = 5 * 1024 * 1024;

const RETRY_ATTEMPTS: usize = 3;
const RETRY_BACKOFFS: [Duration; 2] = [Duration::from_millis(200), Duration::from_millis(600)];

/// Cache of successfully fetched .nzb XML payloads. Bound by *bytes* via
/// moka's weigher so a few large NZBs can't crowd out many small ones —
/// 64 MiB total is enough for ~600 typical movie/episode .nzb files.
/// 7-day TTL: an .nzb's article-ID list is immutable once posted, so a
/// stale entry is either still correct (articles retained) or harmless
/// (articles DMCA'd → preflight discovers the same outcome a refetch
/// would). Matches `SANITY_CACHE` for repeat-view friendliness.
pub static NZB_BYTES_CACHE: Lazy<Cache<String, Arc<String>>> = Lazy::new(|| {
    Cache::builder()
        .max_capacity(64 * 1024 * 1024) // 64 MiB
        .weigher(|_k: &String, v: &Arc<String>| v.len().min(u32::MAX as usize) as u32)
        .time_to_live(Duration::from_secs(7 * 86400))
        .build()
});

/// How long an indexer host is treated as throttled after a 5xx response.
/// Long enough that a single bad search doesn't burn dozens of retries
/// across follow-up searches; short enough that a brief outage recovers
/// on its own. 5 min matches typical indexer rate-limit windows.
const THROTTLE_DURATION: Duration = Duration::from_secs(300);

/// Per-host throttle "until" timestamps. When a host's `Instant` is in the
/// future, all `fetch_nzb_xml` calls to that host short-circuit with
/// `IndexerThrottled` without touching the network. Bound to ~256 entries
/// (well above any plausible operator's indexer count) with the same
/// 5-min TTL so stale entries auto-evict.
pub static INDEXER_THROTTLE: Lazy<Cache<String, Instant>> = Lazy::new(|| {
    Cache::builder()
        .max_capacity(256)
        .time_to_live(THROTTLE_DURATION)
        .build()
});

/// Extract the host (e.g. "api.nzbplanet.net") from a full URL. Returns
/// the input on parse failure so the throttle key is at least stable.
fn host_of(url: &str) -> String {
    Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|s| s.to_string()))
        .unwrap_or_else(|| url.to_string())
}

#[derive(Debug, thiserror::Error)]
pub enum NzbFetchError {
    /// 4xx or post-retry 5xx from the indexer.
    #[error("http-{0}")]
    HttpStatus(u16),
    /// Network/timeout error, redacted for logs.
    #[error("network: {0}")]
    Network(String),
    /// Body exceeded `MAX_NZB_SIZE`.
    #[error("nzb-too-large-{0}")]
    TooLarge(u64),
    /// Indexer host is in cooldown after a recent 5xx — short-circuited
    /// without hitting the network. Treated as a transient failure
    /// upstream (drops the candidate from the listing).
    #[error("indexer-throttled")]
    IndexerThrottled,
}

fn cache_key(url: &str) -> String {
    let digest = Sha1::digest(url.as_bytes());
    // 16-hex-char prefix, same convention as `nzb_sanity::sha1_short`.
    format!("nzb-bytes:{}", &hex::encode(digest)[..16])
}

/// Fetch the XML body of an .nzb URL with caching + retry. Returns the
/// shared `Arc<String>` so multiple consumers reuse the same allocation.
///
/// Three short-circuit paths before the HTTP fetch:
///   1. Bytes-cache hit → returned immediately.
///   2. Indexer host in throttle cooldown → `IndexerThrottled`.
///   3. Otherwise, retry-with-backoff against the network.
pub async fn fetch_nzb_xml(
    client: &reqwest::Client,
    nzb_url: &str,
) -> Result<Arc<String>, NzbFetchError> {
    let key = cache_key(nzb_url);
    if let Some(hit) = NZB_BYTES_CACHE.get(&key).await {
        return Ok(hit);
    }

    let host = host_of(nzb_url);
    if let Some(until) = INDEXER_THROTTLE.get(&host).await {
        if until > Instant::now() {
            tracing::debug!(
                "[nzb-fetch] indexer {host} throttled, short-circuiting (try again in {}s)",
                until.saturating_duration_since(Instant::now()).as_secs()
            );
            return Err(NzbFetchError::IndexerThrottled);
        }
    }

    match fetch_with_retry(client, nzb_url).await {
        Ok(xml) => {
            let arc = Arc::new(xml);
            NZB_BYTES_CACHE.insert(key, arc.clone()).await;
            Ok(arc)
        }
        Err(err) => {
            // Trip the throttle on indexer-side errors. Network errors don't
            // count — if our local connection is flaky we don't want to mark
            // the whole indexer dead.
            if matches!(&err, NzbFetchError::HttpStatus(c) if *c >= 500) {
                let until = Instant::now() + THROTTLE_DURATION;
                INDEXER_THROTTLE.insert(host.clone(), until).await;
                tracing::warn!(
                    "[nzb-fetch] indexer {host} returned 5xx; throttled for {}s",
                    THROTTLE_DURATION.as_secs()
                );
            }
            Err(err)
        }
    }
}

async fn fetch_with_retry(
    client: &reqwest::Client,
    nzb_url: &str,
) -> Result<String, NzbFetchError> {
    let mut last_err: Option<NzbFetchError> = None;
    for attempt in 0..RETRY_ATTEMPTS {
        if attempt > 0 {
            let delay = RETRY_BACKOFFS[(attempt - 1).min(RETRY_BACKOFFS.len() - 1)];
            tokio::time::sleep(delay).await;
            tracing::debug!(
                "[nzb-fetch] retry {attempt}/{} after {:?} for {}",
                RETRY_ATTEMPTS - 1,
                delay,
                crate::util::redact_log(nzb_url)
            );
        }
        match try_once(client, nzb_url).await {
            Ok(xml) => return Ok(xml),
            Err(err) => {
                if !is_retryable(&err) {
                    return Err(err);
                }
                last_err = Some(err);
            }
        }
    }
    Err(last_err.unwrap_or(NzbFetchError::Network("retry loop without attempts".into())))
}

fn is_retryable(err: &NzbFetchError) -> bool {
    match err {
        NzbFetchError::Network(_) => true,
        NzbFetchError::HttpStatus(code) => *code >= 500,
        NzbFetchError::TooLarge(_) => false,
        // IndexerThrottled never reaches here — fetch_nzb_xml short-circuits
        // before invoking the retry loop. The arm is for completeness.
        NzbFetchError::IndexerThrottled => false,
    }
}

async fn try_once(client: &reqwest::Client, nzb_url: &str) -> Result<String, NzbFetchError> {
    let resp = client
        .get(nzb_url)
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|e| NzbFetchError::Network(crate::util::redact_log(&e.to_string())))?;

    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        return Err(NzbFetchError::HttpStatus(status));
    }

    if let Some(cl) = resp.content_length() {
        if cl > MAX_NZB_SIZE {
            return Err(NzbFetchError::TooLarge(cl));
        }
    }

    let xml = resp
        .text()
        .await
        .map_err(|e| NzbFetchError::Network(crate::util::redact_log(&e.to_string())))?;
    if (xml.len() as u64) > MAX_NZB_SIZE {
        return Err(NzbFetchError::TooLarge(xml.len() as u64));
    }
    Ok(xml)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_retryable_classifies_correctly() {
        assert!(is_retryable(&NzbFetchError::HttpStatus(500)));
        assert!(is_retryable(&NzbFetchError::HttpStatus(503)));
        assert!(is_retryable(&NzbFetchError::HttpStatus(504)));
        assert!(is_retryable(&NzbFetchError::Network("dns".into())));
        assert!(!is_retryable(&NzbFetchError::HttpStatus(404)));
        assert!(!is_retryable(&NzbFetchError::HttpStatus(403)));
        assert!(!is_retryable(&NzbFetchError::HttpStatus(401)));
        assert!(!is_retryable(&NzbFetchError::TooLarge(99_000_000)));
    }

    #[test]
    fn cache_key_is_deterministic_and_short() {
        let k1 = cache_key("https://api.example.com/getnzb/abc.nzb");
        let k2 = cache_key("https://api.example.com/getnzb/abc.nzb");
        let k3 = cache_key("https://api.example.com/getnzb/xyz.nzb");
        assert_eq!(k1, k2, "same URL must produce same key");
        assert_ne!(k1, k3, "different URL must produce different key");
        assert!(k1.starts_with("nzb-bytes:"));
        assert_eq!(k1.len(), "nzb-bytes:".len() + 16);
    }

    #[tokio::test]
    async fn cache_hit_short_circuits_fetch() {
        // Pre-populate the shared cache, then call fetch_nzb_xml with a
        // URL pointing at a port we know is closed. If the cache works,
        // we never touch the network and get the cached bytes.
        let url = "http://127.0.0.1:1/will-not-be-fetched";
        let key = cache_key(url);
        let payload = Arc::new("<nzb>cached</nzb>".to_string());
        NZB_BYTES_CACHE.insert(key.clone(), payload.clone()).await;

        let client = reqwest::Client::new();
        let got = fetch_nzb_xml(&client, url).await.expect("cache hit");
        assert_eq!(got.as_str(), "<nzb>cached</nzb>");

        // Cleanup so other tests don't see this fixture.
        NZB_BYTES_CACHE.invalidate(&key).await;
    }

    /// End-to-end retry test: small in-process axum server returns 503
    /// twice then 200. fetch_nzb_xml should succeed on the third attempt.
    #[tokio::test]
    async fn retries_on_5xx_then_succeeds() {
        use axum::{http::StatusCode, routing::get, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};

        static HITS: AtomicUsize = AtomicUsize::new(0);
        // Reset in case a prior test left it dirty (tests share process).
        HITS.store(0, Ordering::SeqCst);

        let app = Router::new().route(
            "/nzb",
            get(|| async {
                let n = HITS.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    (StatusCode::SERVICE_UNAVAILABLE, "nope")
                } else {
                    (StatusCode::OK, "<nzb>ok</nzb>")
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let url = format!("http://{addr}/nzb");
        let client = reqwest::Client::new();
        let got = fetch_nzb_xml(&client, &url).await.expect("retries should succeed");
        assert_eq!(got.as_str(), "<nzb>ok</nzb>");
        assert_eq!(HITS.load(Ordering::SeqCst), 3, "should have hit 3 times");

        // Cleanup the shared cache so other tests aren't poisoned.
        NZB_BYTES_CACHE.invalidate(&cache_key(&url)).await;
        server.abort();
    }

    #[tokio::test]
    async fn does_not_retry_on_4xx() {
        use axum::{http::StatusCode, routing::get, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};

        static HITS: AtomicUsize = AtomicUsize::new(0);
        HITS.store(0, Ordering::SeqCst);

        let app = Router::new().route(
            "/missing",
            get(|| async {
                HITS.fetch_add(1, Ordering::SeqCst);
                (StatusCode::NOT_FOUND, "nope")
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let url = format!("http://{addr}/missing");
        let client = reqwest::Client::new();
        let err = fetch_nzb_xml(&client, &url).await.expect_err("404 should be terminal");
        assert!(matches!(err, NzbFetchError::HttpStatus(404)));
        assert_eq!(HITS.load(Ordering::SeqCst), 1, "must not retry 4xx");

        server.abort();
    }

    #[tokio::test]
    async fn host_of_extracts_authority() {
        assert_eq!(host_of("https://api.nzbplanet.net/getnzb/abc.nzb"), "api.nzbplanet.net");
        assert_eq!(host_of("http://localhost:1234/x"), "localhost");
        // Garbage URL falls back to the raw input.
        let raw = "not-a-url";
        assert_eq!(host_of(raw), raw);
    }

    #[tokio::test]
    async fn throttle_short_circuits_after_5xx() {
        // Server always 503s. First call: HTTP fired, throttle gets set.
        // Second call: short-circuits at the throttle check, no HTTP.
        use axum::{http::StatusCode, routing::get, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};

        static HITS: AtomicUsize = AtomicUsize::new(0);
        HITS.store(0, Ordering::SeqCst);

        let app = Router::new().route(
            "/down",
            get(|| async {
                HITS.fetch_add(1, Ordering::SeqCst);
                (StatusCode::SERVICE_UNAVAILABLE, "nope")
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let url = format!("http://{addr}/down");
        let host = host_of(&url);
        // Defensive cleanup in case prior test on same host left it dirty.
        INDEXER_THROTTLE.invalidate(&host).await;

        let client = reqwest::Client::new();

        // First call: 3 retries (initial + 2) all 503 → final 503 → throttle armed.
        let err1 = fetch_nzb_xml(&client, &url).await.expect_err("first call should fail");
        assert!(matches!(err1, NzbFetchError::HttpStatus(503)));
        let hits_after_first = HITS.load(Ordering::SeqCst);
        assert_eq!(hits_after_first, 3, "first call should burn 3 retry attempts");

        // Second call: should NOT touch the network — IndexerThrottled.
        let err2 = fetch_nzb_xml(&client, &url).await.expect_err("second call should short-circuit");
        assert!(matches!(err2, NzbFetchError::IndexerThrottled));
        assert_eq!(
            HITS.load(Ordering::SeqCst),
            hits_after_first,
            "throttled call must not increment server hits"
        );

        // Cleanup so other tests aren't affected.
        INDEXER_THROTTLE.invalidate(&host).await;
        server.abort();
    }

    #[tokio::test]
    async fn throttle_does_not_engage_on_4xx() {
        // 404 is deterministic, not an indexer-side cap. We must NOT throttle
        // on it — otherwise a single bad URL would stick the whole indexer.
        use axum::{http::StatusCode, routing::get, Router};
        let app = Router::new().route("/missing", get(|| async {
            (StatusCode::NOT_FOUND, "nope")
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let url = format!("http://{addr}/missing");
        let host = host_of(&url);
        INDEXER_THROTTLE.invalidate(&host).await;

        let client = reqwest::Client::new();
        let _ = fetch_nzb_xml(&client, &url).await.expect_err("404");

        // Throttle must NOT be set.
        assert!(
            INDEXER_THROTTLE.get(&host).await.is_none(),
            "4xx must not engage the throttle"
        );
        server.abort();
    }

    #[tokio::test]
    async fn caches_only_successes() {
        // Server always 503s — fetch fails; cache must NOT remember.
        use axum::{http::StatusCode, routing::get, Router};
        let app = Router::new().route(
            "/down",
            get(|| async { (StatusCode::SERVICE_UNAVAILABLE, "nope") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let url = format!("http://{addr}/down");
        let client = reqwest::Client::new();
        let _ = fetch_nzb_xml(&client, &url).await.expect_err("503 should propagate");

        // Cache must be empty for this URL.
        let key = cache_key(&url);
        assert!(
            NZB_BYTES_CACHE.get(&key).await.is_none(),
            "failures must not be cached"
        );
        server.abort();
    }
}
