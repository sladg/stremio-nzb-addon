mod auth;
mod cache;
mod cinemeta;
mod config;
mod content_filter;
mod healthcheck;
mod ip_ban;
mod manifest;
mod nzb_api;
mod nzb_availability;
mod nzb_sanity;
mod parse_title;
mod quality;
mod stream;
mod streaming;
mod stremio;
mod tvdb;
mod util;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{Extension, Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use tokio::sync::RwLock;
use tower_http::cors::{Any, CorsLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::auth::UserName;
use crate::config::AddonConfig;
use crate::ip_ban::{BanConfig, BanList};
use crate::streaming::{disk_cache::DiskCache, nntp::NntpPool, session::SessionRegistry};
use crate::stremio::Manifest;

pub struct AppState {
    pub http: reqwest::Client,
    pub cfg: Arc<RwLock<AddonConfig>>,
    // Streaming layer.
    pub sessions: SessionRegistry,
    pub cache: Arc<DiskCache>,
    /// Long-lived NNTP connection pool. Built once at boot from operator
    /// config; per-user NNTP servers is a future enhancement.
    pub nntp: RwLock<Option<Arc<NntpPool>>>,
    /// Boot-time timestamp for the `/health` uptime field.
    pub started_at: std::time::Instant,
    /// IP-level abuse tracker. Records failed-auth events; the outer
    /// middleware short-circuits banned IPs with 403.
    pub ban_list: Arc<BanList>,
    /// True when the operator opted into trusting `X-Forwarded-For` /
    /// `X-Real-IP` headers (`TRUST_PROXY_HEADERS=1`). Off by default;
    /// only enable when behind a known reverse proxy that strips these
    /// headers from inbound requests, otherwise IPs are spoofable.
    pub trust_proxy_headers: bool,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .init();

    // rustls 0.23 needs an explicit CryptoProvider when both `aws-lc-rs` and
    // `ring` show up via separate dependency paths (reqwest pulls one in
    // transitively). Install the aws-lc-rs default once at boot before any
    // TLS handshake runs.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .ok();

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);

    let bind_addr: IpAddr = std::env::var("BIND_ADDR")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| "127.0.0.1".parse().unwrap());

    let config_path: PathBuf = std::env::var("CONFIG_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("config.toml"));

    let initial_cfg = match config::load_from_disk(&config_path) {
        Ok(Some(cfg)) => {
            tracing::info!("loaded config from {}", config_path.display());
            cfg
        }
        Ok(None) => {
            tracing::warn!(
                "no config at {}; addon will start but stream requests will fail until you create one",
                config_path.display(),
            );
            AddonConfig::empty()
        }
        Err(err) => {
            tracing::error!("failed to load config: {err:#}");
            AddonConfig::empty()
        }
    };

    // Validate after load. If the user opted into `requireAuth = true`
    // but didn't configure any users, we refuse to start — that's
    // precisely the foot-gun the flag exists to prevent.
    if let Err(msg) = initial_cfg.validate() {
        tracing::error!("config invalid: {msg}");
        std::process::exit(1);
    }

    if initial_cfg.requires_auth() {
        tracing::info!(
            "[auth] {} access key(s) configured; addon routes mounted under /{{key}}/...",
            initial_cfg.users.len()
        );
    } else {
        tracing::warn!(
            "[auth] no access keys configured; addon is unauthenticated — fine for local dev, NOT for public deployment. Set `requireAuth = true` in config.toml to make this a startup-time error."
        );
    }

    let http = reqwest::Client::builder()
        .user_agent("stremio-nzb-addon/0.1 (rust)")
        .build()
        .expect("reqwest client");

    let cache_dir: PathBuf = std::env::var("CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("cache"));
    if let Err(err) = std::fs::create_dir_all(&cache_dir) {
        tracing::error!("failed to create cache dir {}: {err}", cache_dir.display());
    }
    // Reap any orphan cache files from a previous run — the in-memory session
    // registry doesn't persist across restarts, so the tokens those files
    // were keyed by are dead.
    match streaming::gc::clear_cache_dir(&cache_dir).await {
        Ok(0) => {}
        Ok(n) => tracing::info!("[gc] cleaned {n} orphan cache file(s) at boot"),
        Err(err) => tracing::warn!("[gc] boot cleanup of {} failed: {err}", cache_dir.display()),
    }

    let cache_bytes: u64 = std::env::var("CACHE_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1024 * 1024 * 1024);
    let cache = Arc::new(DiskCache::new(cache_dir.clone(), cache_bytes));

    // Build the NNTP pool from the loaded config (if any servers).
    let initial_nntp = build_nntp_pool(&initial_cfg);

    let ban_config = BanConfig::from_env();
    if ban_config.enabled() {
        tracing::info!(
            "[ban] enabled: {} failures within {}s → ban for {}s",
            ban_config.failure_threshold,
            ban_config.window.as_secs(),
            ban_config.ban_duration.as_secs(),
        );
    } else {
        tracing::info!("[ban] disabled (set BAN_FAILURE_THRESHOLD>0 to enable)");
    }
    let trust_proxy_headers = std::env::var("TRUST_PROXY_HEADERS")
        .ok()
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    if trust_proxy_headers {
        tracing::info!(
            "[ban] trusting X-Forwarded-For / X-Real-IP headers (TRUST_PROXY_HEADERS=1) — ensure the addon sits behind a reverse proxy that strips inbound copies"
        );
    }

    let state = Arc::new(AppState {
        http,
        cfg: Arc::new(RwLock::new(initial_cfg)),
        sessions: streaming::session::new_registry(),
        cache,
        nntp: RwLock::new(initial_nntp),
        started_at: std::time::Instant::now(),
        ban_list: BanList::new(ban_config),
        trust_proxy_headers,
    });

    // GC: prunes idle sessions + enforces cache disk-byte cap.
    let gc_config = streaming::gc::GcConfig {
        interval: std::time::Duration::from_secs(
            std::env::var("GC_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(60),
        ),
        idle_timeout: std::time::Duration::from_secs(
            std::env::var("IDLE_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3600),
        ),
        max_bytes: cache_bytes,
        protect_window: std::time::Duration::from_secs(
            std::env::var("PROTECT_WINDOW_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(300),
        ),
    };
    streaming::gc::spawn(state.sessions.clone(), cache_dir, gc_config);
    tracing::info!(
        "[gc] scheduled: interval={:?} idle_timeout={:?} max_bytes={} protect_window={:?}",
        gc_config.interval,
        gc_config.idle_timeout,
        gc_config.max_bytes,
        gc_config.protect_window,
    );

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = build_router(state.clone()).layer(cors);

    let listener = tokio::net::TcpListener::bind((bind_addr, port))
        .await
        .expect("bind");
    tracing::info!("Addon listening at http://{bind_addr}:{port}");
    // `into_make_service_with_connect_info` exposes the peer address to
    // handlers via `ConnectInfo<SocketAddr>` — needed for the ban-check
    // and rate-limit middlewares to identify the client.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .expect("serve");
}

/// Best-effort client IP. Honors `X-Forwarded-For` / `X-Real-IP` only
/// when `TRUST_PROXY_HEADERS=1` and the addon is documented to sit
/// behind a real proxy that strips inbound copies of those headers
/// (otherwise they're trivially spoofed from the public internet).
///
/// Falls back to the peer address from `ConnectInfo` (the actual TCP
/// remote — for direct connections that's the client; behind a proxy
/// without `TRUST_PROXY_HEADERS=1` it's the proxy and every request
/// looks like the same IP, which collapses bans to "the proxy ip,"
/// which is still wrong but fail-safe).
/// Build the base URL the addon should emit for stream/video links.
/// Honors `X-Forwarded-Proto` when `TRUST_PROXY_HEADERS=1` so that
/// running behind a TLS terminator (Tailscale Funnel, Caddy, CF Tunnel,
/// nginx, …) emits `https://...` URLs that don't get blocked by mixed
/// content rules or rejected by HTTPS-only edge gateways.
///
/// Falls back to `http` when no proxy header is present (or the proxy
/// isn't trusted), and to `localhost` when no Host header is present.
pub fn request_base_url(headers: &axum::http::HeaderMap, trust_proxy: bool) -> String {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let scheme = if trust_proxy {
        headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(',').next().unwrap_or(s).trim())
            .filter(|s| *s == "http" || *s == "https")
            .unwrap_or("http")
    } else {
        "http"
    };
    format!("{scheme}://{host}")
}

pub fn client_ip(
    headers: &axum::http::HeaderMap,
    connect_info: Option<&SocketAddr>,
    trust_proxy: bool,
) -> Option<IpAddr> {
    if trust_proxy {
        // `X-Forwarded-For` is a comma-separated list; the first entry is
        // the original client (assuming the proxy appends, not prepends).
        if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            if let Some(first) = xff.split(',').next() {
                if let Ok(ip) = first.trim().parse::<IpAddr>() {
                    return Some(ip);
                }
            }
        }
        if let Some(xri) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
            if let Ok(ip) = xri.trim().parse::<IpAddr>() {
                return Some(ip);
            }
        }
    }
    connect_info.map(|s| s.ip())
}

/// Outermost middleware: 403 banned IPs immediately, before any work
/// runs. Every request goes through this — including `/health` —
/// because if an IP is banned we don't want it pinging anything. The
/// liveness probe traffic is from inside the cluster (`127.0.0.1` or
/// the kubelet's IP) which won't get banned in practice.
async fn ban_check_middleware(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<SocketAddr>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, StatusCode> {
    let ip =
        client_ip(request.headers(), Some(&addr), state.trust_proxy_headers).unwrap_or(addr.ip());
    if state.ban_list.is_banned(ip) {
        tracing::warn!("[ban] reject banned ip={ip} path={}", request.uri().path());
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(next.run(request).await)
}

/// Compose the router. Addon routes either:
///   - mount at root (when `cfg.users` is empty — local dev), so
///     `GET /manifest.json` works directly, OR
///   - nest under `/{user_token}` with the auth middleware, so
///     `GET /{token}/manifest.json` works and bare `/manifest.json` 404s.
///
/// `/v/{token}.mkv` and `/health` are always at root — the former because
/// its token is independent, the latter because k8s probes don't auth.
fn build_router(state: Arc<AppState>) -> Router {
    use std::time::Duration;
    use tower_governor::{governor::GovernorConfigBuilder, GovernorLayer};

    let addon_routes: Router<Arc<AppState>> = Router::new()
        .route("/manifest.json", get(manifest_route))
        .route("/stream/{type}/{id}", get(stream_route));

    let always_routes: Router<Arc<AppState>> = Router::new()
        .route("/v/{token}", get(video_route))
        .route("/health", get(healthcheck::health))
        .route("/logo.svg", get(logo_route));

    // Per-IP rate limit on addon routes. Skip /v/... (range requests during
    // playback) and /health (k8s probes). Disabled when
    // RATE_LIMIT_PER_MINUTE=0.
    let rate_per_minute: u64 = std::env::var("RATE_LIMIT_PER_MINUTE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let burst: u32 = std::env::var("RATE_LIMIT_BURST")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);

    let addon_routes = if rate_per_minute > 0 {
        // Convert per-minute → per-request period in milliseconds.
        let period_ms = (60_000_u64 / rate_per_minute).max(1);
        let governor_config = Arc::new(
            GovernorConfigBuilder::default()
                .period(Duration::from_millis(period_ms))
                .burst_size(burst)
                .finish()
                .expect("governor config"),
        );
        // Light cleanup of stale per-IP buckets so the governor's
        // internal map doesn't grow without bound.
        let limiter = governor_config.limiter().clone();
        tokio::spawn(async move {
            let interval = std::time::Duration::from_secs(60);
            loop {
                tokio::time::sleep(interval).await;
                limiter.retain_recent();
            }
        });
        tracing::info!("[rate-limit] enabled: ~{rate_per_minute} req/min per IP, burst {burst}");
        addon_routes.layer(GovernorLayer::new(governor_config))
    } else {
        tracing::info!("[rate-limit] disabled (set RATE_LIMIT_PER_MINUTE>0 to enable)");
        addon_routes
    };

    let requires_auth = state
        .cfg
        .try_read()
        .map(|g| g.requires_auth())
        .unwrap_or(false);

    let app = if requires_auth {
        let gated = addon_routes.route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_access_token,
        ));
        Router::new()
            .nest("/{user_token}", gated)
            .merge(always_routes)
    } else {
        addon_routes.merge(always_routes)
    };

    // Ban-check runs OUTSIDE all other middleware so a banned IP is
    // rejected before we spend a single CPU cycle on auth, routing,
    // or rate limiting.
    app.with_state(state.clone())
        .layer(axum::middleware::from_fn_with_state(
            state,
            ban_check_middleware,
        ))
}

async fn manifest_route(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // Build the manifest with a logo URL anchored at this request's
    // origin so the icon resolves correctly regardless of which proxy
    // is in front (Tailscale, Caddy, Cloudflare, none).
    let base_url = request_base_url(&headers, state.trust_proxy_headers);
    let m: Manifest = manifest::manifest(&base_url);
    let mut resp = axum::Json(m).into_response();
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=3600"),
    );
    resp
}

/// Serve the embedded SVG logo. Public, no auth — Stremio fetches it
/// from whatever URL the manifest advertises, which is at root level
/// regardless of the per-user prefix.
async fn logo_route() -> impl IntoResponse {
    const LOGO_BYTES: &[u8] = include_bytes!("../assets/logo.svg");
    let mut resp = (StatusCode::OK, LOGO_BYTES).into_response();
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("image/svg+xml"),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=86400, immutable"),
    );
    resp
}

/// Pull the named param from the path map. Used by addon-route handlers
/// to stay agnostic of whether they're mounted at root (params: type, id)
/// or under `/{user_token}` (params: user_token, type, id) — the typed
/// `Path<(...)>` tuple extractor errors on arity mismatch, but a
/// HashMap deserializes either layout.
fn path_param<'a>(params: &'a HashMap<String, String>, key: &str) -> &'a str {
    params.get(key).map(String::as_str).unwrap_or("")
}

async fn stream_route(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    user: Option<Extension<UserName>>,
    Path(params): Path<HashMap<String, String>>,
) -> axum::Json<crate::stremio::StreamsResponse> {
    let base_url = request_base_url(&headers, state.trust_proxy_headers);
    let type_ = path_param(&params, "type").to_string();
    let id = strip_json_suffix(path_param(&params, "id")).to_string();
    let cfg = state.cfg.read().await;
    let user_name = user.as_ref().map(|u| u.0 .0.as_str());
    let user_cfg = cfg.resolve(user_name);
    let streams = stream::build_streams(
        &cfg,
        &user_cfg,
        type_,
        id,
        state.http.clone(),
        &base_url,
        &state.sessions,
    )
    .await;
    axum::Json(crate::stremio::StreamsResponse {
        streams,
        cache_max_age: 3600,
    })
}

fn strip_json_suffix(s: &str) -> &str {
    s.strip_suffix(".json").unwrap_or(s)
}

/// Build a fresh `NntpPool` from the current config's `nntp_servers`.
/// `None` if no servers are configured. Pool construction never opens
/// connections — they open lazily on first acquire.
fn build_nntp_pool(cfg: &AddonConfig) -> Option<Arc<NntpPool>> {
    // Pool is built from operator-level defaults. Per-user `nntp_servers`
    // overrides exist in the schema but are a no-op at runtime — log a
    // visible warning if any user attempts an override so misuse is
    // obvious. Per-user pools is a future enhancement.
    for (token, user) in &cfg.users {
        if !user.nntp_servers.is_empty() {
            tracing::warn!(
                "[nntp pool] user '{token}' has nntp_servers override, but per-user pools aren't wired yet — using defaults",
            );
        }
    }
    if cfg.defaults.nntp_servers.is_empty() {
        return None;
    }
    let urls: Vec<String> = cfg
        .defaults
        .nntp_servers
        .iter()
        .map(|s| s.server.clone())
        .collect();
    match NntpPool::from_urls(urls) {
        Ok(pool) => {
            tracing::info!("[nntp pool] built {} server pool(s)", pool.server_count());
            Some(Arc::new(pool))
        }
        Err(err) => {
            tracing::error!("[nntp pool] build failed: {err:#}");
            None
        }
    }
}

/// Serve `[token].mkv` (or `.mp4`) — the streaming endpoint.
///
/// Flow:
///   1. Look up session by token. 404 if unknown.
///   2. `OnceCell`-init the `ActiveStream` via pre-flight (NZB fetch, parse,
///      first-segment NNTP fetch, yEnc decode). 500 on failure.
///   3. Parse `Range:` header against the now-known `total_size`.
///   4. Build 200/206 with a streamed body that fills + reads the disk cache
///      segment-by-segment.
async fn video_route(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(token_with_ext): Path<String>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    use axum::body::Body;
    use streaming::http_range::{
        content_range_header, content_range_unsatisfied, parse_range, ParsedRange, RangeError,
    };

    let token = token_with_ext
        .strip_suffix(".mkv")
        .or_else(|| token_with_ext.strip_suffix(".mp4"))
        .unwrap_or(&token_with_ext)
        .to_string();

    let session = state
        .sessions
        .get(&token)
        .map(|s| s.clone())
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("unknown token: {token}")))?;
    // Mark this session as recently used so the GC's idle/cap evictors
    // protect it. (The protect window keeps actively-streaming sessions
    // safe even when total usage exceeds the cap.)
    session.touch();

    let nntp = match state.nntp.read().await.clone() {
        Some(p) => p,
        None => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "no NNTP servers configured".to_string(),
            ));
        }
    };

    let active = session
        .active
        .get_or_try_init(|| async {
            // Phase 5 auto-fallback: walk the group's candidate list in order,
            // commit to the first one whose pre-flight succeeds. If all fail,
            // surface the last error.
            let candidates = session.candidates.clone();
            let mut last_err: Option<crate::streaming::preflight::PreflightError> = None;
            for idx in session.start_idx..candidates.len() {
                let candidate = &candidates[idx];
                tracing::info!(
                    "[video] preflight token={token} candidate {idx}/{} url={}",
                    candidates.len() - 1,
                    util::redact_url(&candidate.nzb_url),
                );
                match streaming::preflight::probe_candidate(
                    &state.http,
                    &candidate.nzb_url,
                    &nntp,
                    &state.cache,
                    &session.token,
                )
                .await
                {
                    Ok(mut active) => {
                        active.candidate_idx = idx;
                        if idx > session.start_idx {
                            tracing::info!(
                                "[video] token={token} fell back to candidate {idx} (skipped {})",
                                idx - session.start_idx
                            );
                        }
                        return Ok(Arc::new(active));
                    }
                    Err(e) => {
                        tracing::warn!(
                            "[video] preflight failed for token={token} candidate {idx}: {e}"
                        );
                        last_err = Some(e);
                    }
                }
            }
            Err(last_err
                .unwrap_or_else(|| crate::streaming::preflight::PreflightError::NoPlayableFile))
        })
        .await
        .map_err(|e| {
            tracing::warn!("preflight exhausted all candidates for token {token}: {e}");
            (StatusCode::BAD_GATEWAY, format!("preflight failed: {e}"))
        })?;

    let total = active.total_size;

    let range_header = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let parsed = match parse_range(range_header, total) {
        Ok(p) => p,
        Err(RangeError::NotSatisfiable) => {
            let mut resp = (
                StatusCode::RANGE_NOT_SATISFIABLE,
                format!("range not satisfiable for {token}"),
            )
                .into_response();
            resp.headers_mut().insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&content_range_unsatisfied(total)).unwrap(),
            );
            return Ok(resp);
        }
        Err(e) => {
            return Err((StatusCode::BAD_REQUEST, format!("range error: {e}")));
        }
    };

    let (start, end, status) = match parsed {
        ParsedRange::Full => (0, total.saturating_sub(1), StatusCode::OK),
        ParsedRange::Partial { start, end } => (start, end, StatusCode::PARTIAL_CONTENT),
    };

    // The cache file lives inside `active` — opened once during pre-flight,
    // shared across all subsequent range requests so populated-range tracking
    // persists. (Re-opening would zero the populated map and silently
    // re-fetch already-cached segments.)
    // No deep clone — `active` is `&Arc<ActiveStream>`, the .clone() bumps
    // the refcount only.
    let cache_file = active.cache_file.clone();
    let body_stream =
        streaming::pipeline::serve_range_stream(active.clone(), cache_file, nntp, start, end);
    // Adapt anyhow::Error to std::io::Error for axum's Body::from_stream.
    let mapped = futures::StreamExt::map(body_stream, |r| {
        r.map_err(|e| std::io::Error::other(e.to_string()))
    });
    let body = Body::from_stream(mapped);

    let mut resp = (status, body).into_response();
    let headers_mut = resp.headers_mut();
    headers_mut.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(active.content_type),
    );
    headers_mut.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    let content_length = end - start + 1;
    headers_mut.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&content_length.to_string()).unwrap(),
    );
    if status == StatusCode::PARTIAL_CONTENT {
        headers_mut.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&content_range_header(start, end, total)).unwrap(),
        );
    }
    Ok(resp)
}
