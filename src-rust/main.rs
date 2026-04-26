mod cache;
mod catalog;
mod cinemeta;
mod config;
mod content_filter;
mod healthcheck;
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
mod ui;
mod util;

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Json, Redirect},
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tower_http::cors::{Any, CorsLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::config::AddonConfig;
use crate::healthcheck::{indexer_handler, nntp_handler};
use crate::streaming::{disk_cache::DiskCache, nntp::NntpPool, session::SessionRegistry};
use crate::stremio::Manifest;

pub struct AppState {
    pub http: reqwest::Client,
    pub cfg: Arc<RwLock<AddonConfig>>,
    pub config_path: PathBuf,
    // Streaming layer.
    pub sessions: SessionRegistry,
    pub cache: Arc<DiskCache>,
    /// Long-lived NNTP connection pool. Rebuilt when `/api/config` saves.
    /// `None` until at least one server is configured.
    pub nntp: RwLock<Option<Arc<NntpPool>>>,
    /// Wall clock at boot. Drives the `/api/status` uptime field.
    pub started_at: std::time::Instant,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .init();

    // rustls 0.23 needs an explicit CryptoProvider when both `aws-lc-rs` and
    // `ring` show up via separate dependency paths (reqwest pulls one in
    // transitively, our healthcheck/streaming code pulls another). Install
    // the aws-lc-rs default once at boot before any TLS handshake runs.
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
                "no config at {}; visit http://{}:{}/configure to set up",
                config_path.display(),
                bind_addr,
                port
            );
            AddonConfig::empty()
        }
        Err(err) => {
            tracing::error!("failed to load config: {err:#}");
            AddonConfig::empty()
        }
    };

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

    let state = Arc::new(AppState {
        http,
        cfg: Arc::new(RwLock::new(initial_cfg)),
        config_path,
        sessions: streaming::session::new_registry(),
        cache,
        nntp: RwLock::new(initial_nntp),
        started_at: std::time::Instant::now(),
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

    let app = Router::new()
        .route("/", get(|| async { Redirect::to("/configure") }))
        .route("/manifest.json", get(manifest_route))
        .route("/configure", get(configure_route))
        .route("/stream/{type}/{id}", get(stream_route))
        .route("/catalog/{type}/{id}", get(catalog_route_no_extra))
        .route("/catalog/{type}/{id}/{extra}", get(catalog_route_extra))
        .route("/meta/{type}/{id}", get(meta_route))
        .route("/v/{token}", get(video_route))
        .route("/api/config", post(api_save_config))
        .route("/api/status", get(api_status))
        .route("/api/healthcheck/indexer", post(indexer_handler))
        .route("/api/healthcheck/nntp", post(nntp_handler))
        .with_state(state)
        .layer(cors);

    let listener = tokio::net::TcpListener::bind((bind_addr, port))
        .await
        .expect("bind");
    tracing::info!("Addon listening at http://{bind_addr}:{port}");
    axum::serve(listener, app).await.expect("serve");
}

async fn manifest_route() -> impl IntoResponse {
    let m: Manifest = manifest::manifest();
    let mut resp = Json(m).into_response();
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=3600"),
    );
    resp
}

async fn configure_route(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let cfg = state.cfg.read().await;
    Html(ui::render_configure(&cfg, host).into_string())
}

async fn stream_route(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((type_, id)): Path<(String, String)>,
) -> Json<crate::stremio::StreamsResponse> {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost")
        .to_string();
    let id = strip_json_suffix(&id).to_string();
    let cfg = state.cfg.read().await;
    let streams = stream::build_streams(
        &cfg,
        type_,
        id,
        state.http.clone(),
        &host,
        &state.sessions,
    )
    .await;
    Json(crate::stremio::StreamsResponse {
        streams,
        cache_max_age: 3600,
    })
}

#[derive(Deserialize, Default)]
struct CatalogQuery {
    search: Option<String>,
}

async fn catalog_route_no_extra(
    Path((_type, _id)): Path<(String, String)>,
    Query(q): Query<CatalogQuery>,
) -> Json<crate::stremio::CatalogResponse> {
    Json(catalog::handle_catalog(q.search.as_deref()))
}

async fn catalog_route_extra(
    Path((_type, _id, extra)): Path<(String, String, String)>,
    Query(q): Query<CatalogQuery>,
) -> Json<crate::stremio::CatalogResponse> {
    let search = q.search.or_else(|| {
        let extra = strip_json_suffix(&extra);
        extra
            .split('&')
            .find_map(|kv| kv.strip_prefix("search="))
            .map(|s| {
                urlencoding::decode(s)
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| s.to_string())
            })
    });
    Json(catalog::handle_catalog(search.as_deref()))
}

async fn meta_route(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((_type, id)): Path<(String, String)>,
) -> Json<crate::stremio::MetaResponse> {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost")
        .to_string();
    let id = strip_json_suffix(&id).to_string();
    let cfg = state.cfg.read().await;
    Json(
        catalog::handle_meta(&cfg, id, state.http.clone(), &host, &state.sessions).await,
    )
}

#[derive(Serialize)]
struct ApiConfigResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn api_save_config(
    State(state): State<Arc<AppState>>,
    body: Result<Json<AddonConfig>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, Json<ApiConfigResponse>) {
    let mut new = match body {
        Ok(Json(cfg)) => cfg,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiConfigResponse {
                    ok: false,
                    error: Some(err.body_text()),
                }),
            );
        }
    };

    new.normalize();

    if let Err(msg) = new.validate() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiConfigResponse {
                ok: false,
                error: Some(msg),
            }),
        );
    }

    if let Err(err) = config::save_to_disk(&state.config_path, &new).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiConfigResponse {
                ok: false,
                error: Some(format!("{err:#}")),
            }),
        );
    }

    // Rebuild the NNTP pool from the new server list. The replaced pool's
    // idle connections are closed eagerly; in-flight requests on the old
    // pool finish naturally because they hold an `Arc<NntpPool>` snapshot
    // taken before the swap.
    let new_pool = build_nntp_pool(&new);
    {
        let mut guard = state.nntp.write().await;
        let old = guard.take();
        *guard = new_pool;
        if let Some(old_pool) = old {
            old_pool.shutdown().await;
        }
    }

    *state.cfg.write().await = new;
    tracing::info!("config saved to {}", state.config_path.display());

    (
        StatusCode::OK,
        Json(ApiConfigResponse {
            ok: true,
            error: None,
        }),
    )
}

fn strip_json_suffix(s: &str) -> &str {
    s.strip_suffix(".json").unwrap_or(s)
}

/// Build a fresh `NntpPool` from the current config's `nntp_servers`.
/// `None` if no servers are configured (e.g. fresh install before any
/// /api/config save). Pool construction never opens connections — they
/// open lazily on first acquire.
fn build_nntp_pool(cfg: &AddonConfig) -> Option<Arc<NntpPool>> {
    if cfg.nntp_servers.is_empty() {
        return None;
    }
    let urls: Vec<String> = cfg.nntp_servers.iter().map(|s| s.server.clone()).collect();
    match NntpPool::from_urls(urls) {
        Ok(pool) => {
            tracing::info!(
                "[nntp pool] built {} server pool(s)",
                pool.server_count()
            );
            Some(Arc::new(pool))
        }
        Err(err) => {
            tracing::error!("[nntp pool] build failed: {err:#}");
            None
        }
    }
}

#[derive(Serialize)]
struct ApiStatusResponse {
    sessions: usize,
    cache_bytes: u64,
    cache_max_bytes: u64,
    cache_dir: String,
    uptime_secs: u64,
}

async fn api_status(State(state): State<Arc<AppState>>) -> Json<ApiStatusResponse> {
    let cache_bytes = streaming::gc::total_disk_bytes(&state.cache.root).await;
    Json(ApiStatusResponse {
        sessions: state.sessions.len(),
        cache_bytes,
        cache_max_bytes: state.cache.max_bytes,
        cache_dir: state.cache.root.display().to_string(),
        uptime_secs: state.started_at.elapsed().as_secs(),
    })
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
    use axum::response::IntoResponse;
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

    // Pre-flight (lazy, OnceCell-guarded). Snapshot the NNTP pool now —
    // a config save mid-stream replaces `state.nntp` but the existing
    // session continues on the old pool until it drains naturally.
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
            Err(last_err.unwrap_or_else(|| {
                crate::streaming::preflight::PreflightError::NoPlayableFile
            }))
        })
        .await
        .map_err(|e| {
            tracing::warn!("preflight exhausted all candidates for token {token}: {e}");
            (StatusCode::BAD_GATEWAY, format!("preflight failed: {e}"))
        })?;

    let total = active.total_size;

    let range_header = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok());
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
    let body_stream = streaming::pipeline::serve_range_stream(
        active.clone(),
        cache_file,
        nntp,
        start,
        end,
    );
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
