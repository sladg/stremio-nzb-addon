# Tech Debt — `src-rust/`

Original audit: 2026-04-25. Updated through Phase 7 (2026-04-26).
Ordered by impact.

Legend: ✅ resolved · ⏳ deferred · 📋 still open

## Resolved post-Phase 7 (NNTP pool + correctness)

- ✅ **#1 NNTP: new TCP+TLS+AUTH per segment** → Long-lived
  `nzb_nntp::ConnectionPool` per server, wrapped by
  `streaming::nntp::NntpPool`. Held in `AppState.nntp` as
  `RwLock<Option<Arc<NntpPool>>>`, rebuilt on `/api/config` save (old pool's
  `close_idle()` runs on swap; in-flight requests finish on the old `Arc`).
  Per-segment failover lives in the pool's `fetch_with_failover` helper.
  Measured effect: fresh-segment fetch ~437 ms (was ~600 ms — TLS handshake
  saved per fetch). 5500+ segments/movie ⇒ ~15 min cumulative savings on
  full playback.
- ✅ **#7 Zero-fill padding masks short yEnc payloads** → Removed the
  zero-fill code path entirely. Short payloads (decoded length more than
  `SHORT_PAYLOAD_TOLERANCE = 1024 B` below declared) are now treated as
  fetch failure: caller falls through to next NNTP server. Cache's
  `populated_ranges` covers only the actual write — no synthetic bytes
  ever appear in served bytes. `streaming::pipeline::ensure_segment_cached`.

## Resolved during phases 5–7

- ✅ **#2 Session registry leaks forever** → Phase 6 GC: `idle_timeout` evicts
  sessions past 1 h of inactivity (configurable via `IDLE_TIMEOUT_SECS`).
  Background task at `src-rust/streaming/gc.rs`.
- ✅ **#3 Disk-cache files never deleted** → Phase 6 GC: cap-eviction at
  `max_bytes` (default 1 GiB, env `CACHE_BYTES`) plus boot-time orphan reaper
  (`clear_cache_dir`). Per-token sparse files live in `CACHE_DIR/{token}.bin`.
- ✅ **#8 Dead `authenticated` variable** → Removed; rewritten to
  `creds_provided` boolean that actually reflects intent. `healthcheck.rs`.
- ✅ **#10 Segment list cloned per request** → Phase 7: `FileLayout.segments`
  is now `Arc<[SegmentRef]>`. Per-request clone is a refcount bump, no Vec
  copy. `streaming/session.rs`.
- ✅ **#11 `Arc::new(active.clone())` per request** → Phase 7:
  `OnceCell<Arc<ActiveStream>>` instead of `OnceCell<ActiveStream>`. Pre-flight
  wraps once; subsequent requests bump refcount only. `main.rs::video_route`.
- ✅ **#12 `sort_by_resolution` re-parses titles** → Phase 7: switched to
  `sort_by_cached_key` so each title is parsed once. `stream.rs`.
- ✅ **#16 `tokio::Mutex<PopulatedRanges>` for sync ops** → Phase 7: switched
  to `std::sync::Mutex`. `streaming/disk_cache.rs`. (File ops still use
  `tokio::Mutex` since their critical sections are async.)
- ✅ **#18 4 clippy warnings** → Phase 7: zero clippy warnings on
  `cargo clippy --no-deps`.
  - `nzb_availability.rs:49` elidable lifetime → elided.
  - `stream.rs:34` `&mut Vec<T>` → `&mut [T]`.
  - `streaming/candidate.rs:62`, `streaming/preflight.rs:239` doc list
    indentation → reformatted.
  - `streaming/gc.rs:166` collapsible if → collapsed.
- ✅ **#19 Module-level `#![allow(dead_code)]`** → Phase 7: removed from
  `streaming/mod.rs`. All previously-staged items either wired or marked
  `#[cfg(test)]`. Two newly-surfaced dead items deleted (`volume_total_size`,
  `is_empty`).

## Discovered + fixed during phase 6 e2e

- ✅ **Cache state lost between requests** (not in original audit) →
  `CachedFile` was opened fresh per request, so `PopulatedRanges` reset every
  time and every range request silently re-fetched from NNTP. Fix: store
  `Arc<CachedFile>` inside `ActiveStream`, opened once in pre-flight and
  shared across all subsequent requests. Cache hits now serve in ~7 ms vs.
  ~3.9 s for cold fetches (560× speedup).
- ✅ **rustls 0.23 panic on first TLS handshake** → `aws-lc-rs` and `ring`
  both reachable in dep tree; rustls couldn't auto-pick. Fix:
  `rustls::crypto::aws_lc_rs::default_provider().install_default()` at boot.
  `main.rs::main`.
- ✅ **Per-segment NNTP failover missing** → Server #1 returning 430 for a
  mid-stream article aborted the whole pipeline instead of trying server #2.
  Fix: `pipeline::ensure_segment_cached` walks `server_urls` in preference
  order. Logs `fetched via fallback server #N` when a fallback wins.
- ✅ **Cache file undersized** → Sized to `total_size` (= video bytes) but
  writes happen at *assembled-stream* offsets (= segments + RAR header
  overhead). Fix: `FileLayout::assembled_size()` now drives `cache.open()`.

## Deferred — non-trivial work, each warrants its own pass

- ⏳ **#4 Single `Mutex<File>` serializes whole cache** —
  `streaming/disk_cache.rs`. Concurrent reads / writes block each other.
  Switch to positioned IO (`FileExt::read_at/write_at` via `spawn_blocking`).
  Less urgent now that NNTP pooling cut the per-segment latency in half;
  serialization isn't yet the bottleneck.
- ⏳ **#5 No read-ahead in pipeline** — `streaming/pipeline.rs`. Sequential
  fetch ⇒ throughput bounded by `seg_size / RTT`. Pool now warm, so this
  is the next throughput win — but the user explicitly deferred it for
  now; circle back after wear-testing the current build.
- ⏳ **#6 Decoded bytes written to disk then re-read** — `pipeline.rs`.
  Yield decoded slice to client *and* write-back to cache in parallel.
  Saves 1 disk RTT per segment.
- ⏳ **#9 UTF-8 boundary bug in NNTP line parser** — `healthcheck.rs`.
  `from_utf8(&tmp[..n])?` fails if a multi-byte char straddles two reads.
  In practice rare (NNTP responses are ASCII), but real. Buffer as
  `Vec<u8>`, parse line bytes.
- ⏳ **#17 `tvdb.rs` uses deprecated TVDB v1** — Legacy
  `GetSeriesByRemoteID.php` is on borrowed time. Migration to TVDB v4 (auth
  required) or TMDB is its own epic.

## Still open — minor / cosmetic

- 📋 **#13 RSS_CACHE 10k entries unbounded** — `cache.rs:10`. Use weighted
  cache (moka supports it). Low priority — entries are small.
- 📋 **#14 `nzb_api::call` no response-size guard** — Misbehaving indexer
  could OOM. Cap via `Content-Length` + streamed body. Nice-to-have.
- 📋 **#15 `Item` has 4 untyped `serde_json::Value` fields** —
  `nzb_api.rs`. Forces value-tree alloc on every parse.
- 📋 **#20 `body_probe_inner` and `nntp_dance` duplicate ~80%** —
  `healthcheck.rs`. Extract `nntp_authenticated_session` helper.
- 📋 **#21 Per-field `rename + alias` in `config.rs`** — Replace with
  `#[serde(rename_all = "camelCase")]` on the struct + per-field `alias`.
- 📋 **#22 `human_size` mixes binary base + SI labels** — `stream.rs:78`.
  Cosmetic; pick one convention.
- 📋 **#23 Hand-rolled date math in `chrono_lite`** — `catalog.rs`.
  Pull in `time` crate.
- 📋 **#24 `video_route` is 120 lines** — `main.rs`. Split into
  `parse_request_range`, `build_206_response`, etc.
- 📋 **#25 No integration test for streaming pipeline** — Inject
  `fetch_segment` via trait so we can mock NNTP end-to-end.
- 📋 **#26 `ranges.rs::insert` worst-case O(N) per insert** — Fine at
  current scale (~5k segs); document upgrade path to interval tree.
- 📋 **#27 No `#![deny(unsafe_code)]` or workspace lints** — One-line add.

## Suggested next-up

User explicitly deferred read-ahead (#5) and yield-while-writing (#6)
to post-shakedown. Next correctness/perf candidates:

1. **#9 UTF-8 boundary bug** — small, isolated, removes a latent crash
   path. Half hour.
2. **#5 read-ahead during sequential playback** — pool now warm,
   compounds with it. Needs care to bound concurrency. ~half day.
3. **#6 yield-while-writing** — removes the disk RTT after every NNTP
   fetch. ~half day.

#4 (positioned cache IO) is only worth doing if #5 lands first and we
hit lock contention in measurement. Don't do it speculatively.
