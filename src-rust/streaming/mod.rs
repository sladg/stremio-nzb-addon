//! In-process Usenet → HTTP streaming server.
//!
//! Phase 1: module skeleton + 501 route.
//! Phase 2 (current): pre-flight for Flat-mode releases.
//! Subsequent phases fill in:
//! - candidate grouping by parse_title signature
//! - per-session stream registry
//! - lazy pre-flight on first byte
//! - disk-backed sparse cache (1 GB cap, LRU eviction)
//! - HTTP range serving with NNTP segment fetch + yEnc decode + RAR-aware byte mapping
//!
//! All `streaming::*` items are now wired in. The blanket `allow(dead_code)`
//! that lived here through phases 1-4 has been removed; any new staged code
//! should use targeted `#[allow(dead_code)]` on the specific item until it's
//! wired up.

pub mod candidate;
pub mod disk_cache;
pub mod gc;
pub mod http_range;
pub mod nntp;
pub mod pipeline;
pub mod preflight;
pub mod ranges;
pub mod session;

// Re-exports will be uncommented as their types start being used outside the
// streaming/ module (Phase 2+ wires preflight into stream.rs).
// pub use session::{SessionRegistry, StreamSession};
