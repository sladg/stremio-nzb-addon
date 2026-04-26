//! NNTP wrapper around `nzb_nntp::ConnectionPool`.
//!
//! Two access patterns:
//!
//! - [`NntpPool`] — long-lived, per-server connection pool used by the
//!   streaming hot path (pre-flight + segment fetches). Reuses TCP+TLS+AUTH
//!   across thousands of segment fetches per stream.
//! - [`fetch_segment`] — one-off connection used by paths that don't have a
//!   pool reference (currently `nzb_availability::body_probe` indirectly via
//!   `healthcheck::body_probe`). Acceptable because they fire infrequently
//!   and lifetime is bounded.

use anyhow::{anyhow, Context, Result};
use nzb_nntp::{ConnectionPool, ServerConfig};
use std::sync::Arc;
use std::time::Duration;
use url::Url;

/// Parse a Stremio-style NNTP server URL into a `nzb_nntp::ServerConfig`.
///
/// Format (matches Stremio's `nntpUrlRegex`):
///   `nntp(s)://user:pass@host:port/connections`
///
/// `user`/`pass` are URL-decoded. `connections` defaults to 1 if absent.
/// `ssl_verify` is **disabled** to match Node's `rejectUnauthorized: false`,
/// because block backbones often serve certificates with mismatched CNs.
pub fn parse_server_url(server_url: &str) -> Result<ServerConfig> {
    let url = Url::parse(server_url).context("invalid NNTP URL")?;
    let secure = match url.scheme() {
        "nntps" => true,
        "nntp" => false,
        other => return Err(anyhow!("unsupported NNTP scheme: {other}")),
    };

    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("missing host in NNTP URL"))?
        .to_string();
    let port = url.port().unwrap_or(if secure { 563 } else { 119 });

    let username = if url.username().is_empty() {
        None
    } else {
        Some(urlencoding::decode(url.username())?.into_owned())
    };
    let password = match url.password() {
        Some(p) if !p.is_empty() => Some(urlencoding::decode(p)?.into_owned()),
        _ => None,
    };

    // `connections` lives in the path: `/4` → 4 connections.
    let connections: u16 = url
        .path()
        .trim_matches('/')
        .parse::<u16>()
        .ok()
        .filter(|n| *n > 0)
        .unwrap_or(1);

    let mut cfg = ServerConfig::new(format!("{host}:{port}"), host.clone());
    cfg.name = host;
    cfg.port = port;
    cfg.ssl = secure;
    cfg.ssl_verify = false;
    cfg.username = username;
    cfg.password = password;
    cfg.connections = connections;
    cfg.enabled = true;
    Ok(cfg)
}

/// Long-lived NNTP connection pool, one sub-pool per server in the user's
/// `AddonConfig`. Built at boot and rebuilt on `/api/config` save.
pub struct NntpPool {
    /// Same index order as `AddonConfig.nntp_servers`.
    pools: Vec<Arc<ConnectionPool>>,
    /// Snapshot of the server URLs the pools were built from. The
    /// `ConnectionPool` holds an `Arc<ServerConfig>` internally too; we keep
    /// the original URL strings here for logging and per-segment failover.
    server_urls: Vec<String>,
}

impl std::fmt::Debug for NntpPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NntpPool")
            .field("server_count", &self.pools.len())
            .field("server_urls", &self.server_urls)
            .finish()
    }
}

impl NntpPool {
    /// Build pools from a list of NNTP server URLs. No connections are
    /// opened yet — `nzb_nntp::ConnectionPool` lazily creates connections
    /// on first `acquire()`.
    pub fn from_urls(server_urls: Vec<String>) -> Result<Self> {
        let mut pools = Vec::with_capacity(server_urls.len());
        for url in &server_urls {
            let cfg =
                parse_server_url(url).with_context(|| format!("parsing NNTP server URL {url}"))?;
            pools.push(Arc::new(ConnectionPool::new(Arc::new(cfg))));
        }
        Ok(Self { pools, server_urls })
    }

    pub fn server_count(&self) -> usize {
        self.pools.len()
    }

    pub fn server_urls(&self) -> &[String] {
        &self.server_urls
    }

    /// Fetch a single article body from `server_idx`. Pooled — no per-call
    /// TLS handshake. Returns the raw response bytes (yEnc-encoded payload),
    /// caller decodes.
    ///
    /// On NNTP-level failure (430 / 423 / etc.) the connection is *kept*
    /// (the server is still alive). On transport error (TLS broken, EOF
    /// mid-response) the connection is discarded.
    ///
    /// 30 s timeout per call — a hung NNTP fetch shouldn't pin a player
    /// indefinitely.
    pub async fn fetch_article(&self, server_idx: usize, message_id: &str) -> Result<Vec<u8>> {
        let idx = server_idx.min(self.pools.len().saturating_sub(1));
        let pool = self
            .pools
            .get(idx)
            .ok_or_else(|| anyhow!("NNTP pool empty"))?;

        let acquired = tokio::time::timeout(Duration::from_secs(30), pool.acquire())
            .await
            .map_err(|_| anyhow!("NNTP acquire timeout (30s)"))?
            .with_context(|| format!("NNTP acquire on server #{idx}"))?;

        let mut pooled = acquired;
        let result =
            tokio::time::timeout(Duration::from_secs(30), pooled.conn.fetch_body(message_id)).await;

        match result {
            Ok(Ok(resp)) if resp.is_success() => {
                let bytes = resp.data.unwrap_or_default();
                pool.release(pooled);
                Ok(bytes)
            }
            Ok(Ok(resp)) => {
                // Non-success NNTP code (430, 423, ...). Connection alive,
                // just this article isn't here. Return Err so caller can
                // try next server.
                let err = anyhow!("NNTP {} for {message_id}: {}", resp.code, resp.message);
                pool.release(pooled);
                Err(err)
            }
            Ok(Err(err)) => {
                // Transport-level error — connection is broken.
                pool.discard(pooled);
                Err(anyhow!("NNTP fetch_body failed: {err}"))
            }
            Err(_) => {
                pool.discard(pooled);
                Err(anyhow!("NNTP fetch_body timeout (30s)"))
            }
        }
    }

    /// Fetch with per-server failover. Tries `preferred` first, then walks
    /// the rest in index order. Surfaces the last error if none succeed.
    pub async fn fetch_with_failover(
        &self,
        preferred: usize,
        message_id: &str,
    ) -> Result<(usize, Vec<u8>)> {
        if self.pools.is_empty() {
            return Err(anyhow!("no NNTP servers configured"));
        }
        let preferred = preferred.min(self.pools.len() - 1);
        let mut order: Vec<usize> = (0..self.pools.len()).collect();
        order.swap(0, preferred);

        let mut last_err: Option<anyhow::Error> = None;
        for idx in order {
            match self.fetch_article(idx, message_id).await {
                Ok(bytes) => return Ok((idx, bytes)),
                Err(err) => {
                    tracing::warn!("[nntp pool] {} on server #{idx}: {err:#}", message_id);
                    last_err = Some(err);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("all servers failed")))
    }

    /// Close all idle connections across all pools. Called when the pool is
    /// being replaced (config change).
    pub async fn shutdown(&self) {
        for pool in &self.pools {
            pool.close_idle().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_secure_url() {
        let cfg = parse_server_url("nntps://user:pass@news.example.com:563/8").unwrap();
        assert_eq!(cfg.host, "news.example.com");
        assert_eq!(cfg.port, 563);
        assert!(cfg.ssl);
        assert!(!cfg.ssl_verify);
        assert_eq!(cfg.username.as_deref(), Some("user"));
        assert_eq!(cfg.password.as_deref(), Some("pass"));
        assert_eq!(cfg.connections, 8);
    }

    #[test]
    fn parse_plain_url_default_port() {
        let cfg = parse_server_url("nntp://news.example.com/4").unwrap();
        assert_eq!(cfg.port, 119);
        assert!(!cfg.ssl);
        assert_eq!(cfg.connections, 4);
        assert!(cfg.username.is_none());
        assert!(cfg.password.is_none());
    }

    #[test]
    fn parse_secure_default_port() {
        let cfg = parse_server_url("nntps://news.example.com").unwrap();
        assert_eq!(cfg.port, 563);
        assert_eq!(cfg.connections, 1); // default when path missing
    }

    #[test]
    fn parse_url_decodes_credentials() {
        // %40 = @, %3A = :, %2F = /
        let cfg = parse_server_url("nntps://us%40er:p%3Aa%2Fss@news.test/2").unwrap();
        assert_eq!(cfg.username.as_deref(), Some("us@er"));
        assert_eq!(cfg.password.as_deref(), Some("p:a/ss"));
    }

    #[test]
    fn parse_rejects_unknown_scheme() {
        assert!(parse_server_url("ftp://x").is_err());
        assert!(parse_server_url("https://x").is_err());
    }

    #[test]
    fn parse_rejects_missing_host() {
        // url crate accepts `nntps:///` but host_str() returns None.
        assert!(parse_server_url("nntps:///path").is_err());
    }

    #[test]
    fn parse_invalid_connections_falls_back_to_one() {
        let cfg = parse_server_url("nntps://x.com/abc").unwrap();
        assert_eq!(cfg.connections, 1);
    }

    #[test]
    fn nntp_pool_constructs_and_reports_count() {
        let pool = NntpPool::from_urls(vec![
            "nntps://u:p@a.test/2".into(),
            "nntp://b.test/4".into(),
        ])
        .unwrap();
        assert_eq!(pool.server_count(), 2);
        assert_eq!(pool.server_urls().len(), 2);
    }

    #[test]
    fn nntp_pool_empty_fetch_errors() {
        let pool = NntpPool::from_urls(Vec::new()).unwrap();
        assert_eq!(pool.server_count(), 0);
        // Calling fetch_with_failover on an empty pool should error.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let res = rt.block_on(pool.fetch_with_failover(0, "x@y"));
        assert!(res.is_err());
    }
}
