//! Liveness/readiness endpoint for k8s probes and external monitoring.
//!
//! Single endpoint `GET /health` that returns 200 + a small JSON payload.
//! No auth (k8s probes don't authenticate; the payload contains nothing
//! sensitive).
//!
//! - **Liveness:** any 200 response means the process is alive.
//! - **Readiness:** `nntp_ready=true` AND `indexers > 0` means real
//!   stream requests will succeed. Operators who want strict readiness
//!   can probe `/health` and assert those two flags.

use std::sync::Arc;

use axum::{extract::State, response::Json};
use serde::Serialize;

use crate::AppState;

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub uptime_secs: u64,
    pub sessions: usize,
    pub cache_bytes: u64,
    pub cache_max_bytes: u64,
    pub indexers: usize,
    pub nntp_ready: bool,
}

pub async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    let cache_bytes = crate::streaming::gc::total_disk_bytes(&state.cache.root).await;
    let cfg = state.cfg.read().await;
    let nntp_ready = state.nntp.read().await.is_some();
    Json(HealthResponse {
        status: "ok",
        uptime_secs: state.started_at.elapsed().as_secs(),
        sessions: state.sessions.len(),
        cache_bytes,
        cache_max_bytes: state.cache.max_bytes,
        indexers: cfg.defaults.indexers.len(),
        nntp_ready,
    })
}
