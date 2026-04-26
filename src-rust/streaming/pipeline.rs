//! Streaming pipeline: serve a byte range from the cache, fetching + decoding
//! NZB segments on-demand for any cache misses.
//!
//! The yield loop walks `[start..=end]` segment-by-segment, ensures each one
//! is in cache (fetching via NNTP + yEnc-decoding if not), and yields the
//! appropriate slice. A background read-ahead task pre-populates the cache
//! ahead of the cursor with bounded concurrency, so the foreground loop's
//! `cache.has_range` check usually short-circuits the per-segment NNTP
//! round-trip and throughput becomes pool-bound rather than RTT-bound.

use anyhow::{anyhow, Result};
use async_stream::try_stream;
use bytes::Bytes;
use futures::stream::{self, Stream, StreamExt};
use nzb_decode::yenc::decode_yenc;
use std::collections::BTreeSet;
use std::sync::Arc;

use crate::streaming::disk_cache::CachedFile;
use crate::streaming::nntp::SegmentSource;
use crate::streaming::session::{ActiveStream, FileLayout, SegmentRef};

/// Maximum chunk size yielded per `Stream::poll_next`. Keeps memory bounded.
const CHUNK_SIZE: usize = 256 * 1024;

/// Sliding-window eviction parameters for a stream's cache file. `None`
/// disables eviction (the cache grows monotonically as today). See
/// `RESOURCE_PROFILE.md` for sizing guidance.
#[derive(Debug, Clone, Copy)]
pub struct EvictPolicy {
    /// Bytes at the start of the cache file that are never evicted (pin
    /// the video container header so demuxers can re-read it on seek).
    pub header_pin: u64,
    /// Bytes behind the playhead to keep populated (covers micro-scrubs
    /// without forcing a re-fetch).
    pub backbuffer: u64,
    /// Minimum size of a single eviction punch. Smaller = more syscalls;
    /// larger = stale bytes linger longer before being freed.
    pub step: u64,
}

/// Aborts the wrapped task when dropped — used to tie the read-ahead
/// prefetcher's lifetime to the response stream, so a client disconnect
/// stops further speculative NNTP fetches.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Serve `[start..=end]` (inclusive) as an `async Stream<Item = Result<Bytes>>`.
/// Each yielded chunk is at most `CHUNK_SIZE` bytes so very large ranges don't
/// balloon memory. `read_ahead` caps concurrent prefetch fetches; 0 or 1
/// disables read-ahead and falls back to pure sequential behavior.
pub fn serve_range_stream<S>(
    active: Arc<ActiveStream>,
    cache: Arc<CachedFile>,
    source: Arc<S>,
    start: u64,
    end: u64,
    read_ahead: usize,
    evict: Option<EvictPolicy>,
) -> impl Stream<Item = Result<Bytes>> + Send + 'static
where
    S: SegmentSource + 'static,
{
    try_stream! {
        if start > end {
            return;
        }
        if source.source_count() == 0 {
            Err(anyhow!("no NNTP servers configured"))?;
        }

        // Cheap clone: bumps the Arc refcount, no Vec copy.
        let segments: Arc<[SegmentRef]> = active.file_layout.segments.clone();

        // Spawn read-ahead prefetcher. The handle is held by the try_stream!
        // future, so when the response stream is cancelled (client disconnect
        // or natural EOS) the guard drops and aborts in-flight prefetches —
        // we don't want to keep pulling NNTP bytes for a client that's gone.
        let _prefetch_guard = if read_ahead >= 2 {
            let indices = segment_indices_in_range(&active.file_layout, &segments, start, end);
            if indices.is_empty() {
                None
            } else {
                tracing::debug!(
                    "[pipeline] read-ahead spawn: {} segments, concurrency={read_ahead}",
                    indices.len()
                );
                let handle = tokio::spawn(prefetch_segments(
                    cache.clone(),
                    source.clone(),
                    segments.clone(),
                    indices,
                    read_ahead,
                ));
                Some(AbortOnDrop(handle))
            }
        } else {
            None
        };

        // Walk video bytes [start..=end]. For each position, translate to the
        // assembled stream via DataChunk; fetch + cache + yield bytes from the
        // segment containing that assembled byte; advance cursor by however
        // much we consumed (capped at the chunk boundary so we re-translate
        // for the next chunk).
        tracing::info!(
            "[pipeline] serve start={start} end={end} total_size={} chunks={}",
            active.total_size,
            active.file_layout.chunks.len()
        );

        let mut cursor = start;
        while cursor <= end {
            let chunk_remaining = active.file_layout.chunk_remaining(cursor);
            if chunk_remaining == 0 {
                tracing::warn!("[pipeline] cursor {cursor} past last chunk");
                Err(anyhow!("cursor {cursor} past last chunk"))?;
            }
            let chunk_video_end = cursor + chunk_remaining - 1;
            let video_chunk_end = chunk_video_end.min(end);

            let assembled_cursor = active
                .file_layout
                .video_to_assembled(cursor)
                .ok_or_else(|| {
                    tracing::warn!("[pipeline] video byte {cursor} not mappable");
                    anyhow!("video byte {cursor} not mappable")
                })?;
            let assembled_chunk_end = assembled_cursor + (video_chunk_end - cursor);
            tracing::debug!(
                "[pipeline] chunk: video=[{cursor}..={video_chunk_end}] -> assembled=[{assembled_cursor}..={assembled_chunk_end}]"
            );

            // Drain assembled bytes [assembled_cursor..=assembled_chunk_end]
            // by walking segments under the hood.
            let mut a_pos = assembled_cursor;
            while a_pos <= assembled_chunk_end {
                let seg_idx = find_segment_for_byte(&segments, a_pos)
                    .ok_or_else(|| {
                        tracing::warn!("[pipeline] assembled byte {a_pos} past last segment (segments={}, last_seg_end={})", segments.len(), segments.last().map(|s| s.offset_in_stream + s.bytes).unwrap_or(0));
                        anyhow!("assembled byte {a_pos} past last segment")
                    })?;
                let seg = &segments[seg_idx];
                let seg_start = seg.offset_in_stream;
                let seg_end_exclusive = seg_start + seg.bytes;

                let read_end = (seg_end_exclusive - 1).min(assembled_chunk_end);

                if !cache.has_range(seg_start, seg_end_exclusive) {
                    ensure_segment_cached(&cache, seg, source.as_ref()).await?;
                }

                let mut read_pos = a_pos;
                while read_pos <= read_end {
                    let want = ((read_end - read_pos + 1) as usize).min(CHUNK_SIZE);
                    let bytes = cache.read_at(read_pos, want).await?;
                    let len = bytes.len() as u64;
                    if len == 0 {
                        Err(anyhow!("zero-length cache read at {read_pos}"))?;
                    }
                    yield Bytes::from(bytes);
                    read_pos += len;
                }

                a_pos = read_end + 1;

                // Sliding-window eviction. Cheap call: self-rate-limited via
                // step threshold + last_evicted_to watermark, so calling per
                // segment is fine — only every Nth call actually punches.
                if let Some(policy) = evict {
                    if let Err(e) = cache
                        .maybe_evict_behind(a_pos, policy.header_pin, policy.backbuffer, policy.step)
                        .await
                    {
                        tracing::warn!("[pipeline] eviction failed at playhead={a_pos}: {e:#}");
                    }
                }
            }

            cursor = video_chunk_end + 1;
        }
    }
}

/// Compute the set of segment indices whose data overlaps the requested
/// video-byte range `[start..=end]`, walking each chunk in `layout` so RAR
/// multi-volume releases (where chunks are non-contiguous in the assembled
/// stream) only prefetch what's actually needed.
fn segment_indices_in_range(
    layout: &FileLayout,
    segments: &[SegmentRef],
    start: u64,
    end: u64,
) -> Vec<usize> {
    let mut indices: BTreeSet<usize> = BTreeSet::new();
    let mut cursor = start;
    while cursor <= end {
        let chunk_remaining = layout.chunk_remaining(cursor);
        if chunk_remaining == 0 {
            break;
        }
        let chunk_video_end = (cursor + chunk_remaining - 1).min(end);
        let Some(assembled_start) = layout.video_to_assembled(cursor) else {
            break;
        };
        let assembled_end = assembled_start + (chunk_video_end - cursor);
        if let (Some(lo), Some(hi)) = (
            find_segment_for_byte(segments, assembled_start),
            find_segment_for_byte(segments, assembled_end),
        ) {
            for i in lo..=hi {
                indices.insert(i);
            }
        }
        cursor = chunk_video_end + 1;
    }
    indices.into_iter().collect()
}

/// Background prefetch loop. Walks `indices` with bounded concurrency
/// (`buffer_unordered`), short-circuiting any segment that's already in
/// cache. Failures are logged and swallowed — the foreground yield loop
/// will retry the same segment via `ensure_segment_cached` and surface a
/// real error to the client there if it's persistent.
async fn prefetch_segments<S>(
    cache: Arc<CachedFile>,
    source: Arc<S>,
    segments: Arc<[SegmentRef]>,
    indices: Vec<usize>,
    concurrency: usize,
) where
    S: SegmentSource + 'static,
{
    let concurrency = concurrency.max(1);
    stream::iter(indices.into_iter().map(|i| {
        let cache = cache.clone();
        let source = source.clone();
        let segments = segments.clone();
        async move {
            let seg = &segments[i];
            let seg_end = seg.offset_in_stream + seg.bytes;
            if cache.has_range(seg.offset_in_stream, seg_end) {
                return;
            }
            if let Err(e) = ensure_segment_cached(&cache, seg, source.as_ref()).await {
                tracing::debug!(
                    "[pipeline] prefetch seg #{i} ({}) failed: {}",
                    seg.message_id,
                    crate::util::redact_log(&e.to_string())
                );
            }
        }
    }))
    .buffer_unordered(concurrency)
    .for_each(|_| async {})
    .await;
}

/// Locate the segment whose `[offset_in_stream, offset_in_stream + bytes)`
/// contains `byte`. Segments are assumed pre-sorted by offset.
pub fn find_segment_for_byte(segments: &[SegmentRef], byte: u64) -> Option<usize> {
    // Binary search by start offset.
    let idx = match segments.binary_search_by(|s| s.offset_in_stream.cmp(&byte)) {
        Ok(i) => i,
        Err(0) => return None, // byte before first segment
        Err(i) => i - 1,
    };
    let seg = &segments[idx];
    if byte < seg.offset_in_stream + seg.bytes {
        Some(idx)
    } else {
        None
    }
}

/// Fetch a segment via the NNTP pool with per-server failover, yEnc-decode,
/// and write the decoded bytes into the cache file at the segment's offset.
///
/// Per-server failover: try `seg.server_index` first (set by pre-flight,
/// defaults to 0), fall back to other servers in index order on transport
/// error, NNTP non-success codes (430/423), or empty/undecodable payloads.
///
/// **Decoded length vs declared `seg.bytes`:** these can disagree by a
/// few percent in the wild because different posters/indexers use
/// different conventions for what `<segment bytes="…">` represents
/// (decoded payload vs encoded article body). This is **not** a
/// corruption signal — both servers will return the same payload.
/// We write whatever decoded bytes we got at `seg.offset_in_stream`,
/// then zero-pad to the declared size so subsequent reads inside the
/// segment's slot don't gap-fault. The minor precision loss (a few KiB
/// of zeros where decoded payload was shorter than declared) is
/// invisible to MKV/MP4 demuxers in practice; a true segment-level
/// failure already short-circuits via the empty/error paths above.
async fn ensure_segment_cached<S>(cache: &CachedFile, seg: &SegmentRef, source: &S) -> Result<()>
where
    S: SegmentSource + ?Sized,
{
    tracing::debug!(
        "[pipeline] fetching segment msg={} declared_bytes={} offset_in_stream={}",
        seg.message_id,
        seg.bytes,
        seg.offset_in_stream,
    );

    let server_count = source.source_count();
    if server_count == 0 {
        return Err(anyhow!("no NNTP servers configured"));
    }

    let preferred = seg.server_index.min(server_count - 1);
    let mut order: Vec<usize> = (0..server_count).collect();
    order.swap(0, preferred);

    let declared = seg.bytes as usize;
    let mut last_err: Option<anyhow::Error> = None;

    for idx in order {
        let raw = match source.fetch_segment(idx, &seg.message_id).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    "[pipeline] NNTP fetch failed for {} on server #{idx}: {}",
                    seg.message_id,
                    crate::util::redact_log(&e.to_string())
                );
                last_err = Some(e);
                continue;
            }
        };

        let decoded = match decode_yenc(&raw) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(
                    "[pipeline] yEnc decode failed for {} on server #{idx}: {e:?}",
                    seg.message_id
                );
                last_err = Some(anyhow!("yEnc decode failed: {e:?}"));
                continue;
            }
        };

        if decoded.data.is_empty() {
            tracing::warn!(
                "[pipeline] decoded segment {} empty on server #{idx}",
                seg.message_id
            );
            last_err = Some(anyhow!("decoded segment {} empty", seg.message_id));
            continue;
        }

        if idx != preferred {
            tracing::info!(
                "[pipeline] segment {} fetched via fallback server #{idx}",
                seg.message_id
            );
        }

        // Write decoded bytes at the segment's start offset. If the decoded
        // payload is longer than declared (rare), truncate; if shorter,
        // pad the tail with zeros so populated-range tracking reaches the
        // declared slot end and subsequent segments find their offsets.
        let to_write = if decoded.data.len() > declared {
            &decoded.data[..declared]
        } else {
            &decoded.data[..]
        };
        cache
            .write_at(seg.offset_in_stream, to_write)
            .await
            .map_err(|e| {
                tracing::warn!(
                    "[pipeline] cache write_at failed: offset={} len={}: {e}",
                    seg.offset_in_stream,
                    to_write.len(),
                );
                e
            })?;

        if to_write.len() < declared {
            let pad = vec![0u8; declared - to_write.len()];
            cache
                .write_at(seg.offset_in_stream + to_write.len() as u64, &pad)
                .await?;
            tracing::debug!(
                "[pipeline] segment {} padded {} bytes to declared {} (decoded was {})",
                seg.message_id,
                pad.len(),
                declared,
                decoded.data.len(),
            );
        }
        return Ok(());
    }

    Err(last_err.unwrap_or_else(|| anyhow!("no NNTP servers available for {}", seg.message_id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segs(specs: &[(u64, u64)]) -> Vec<SegmentRef> {
        specs
            .iter()
            .enumerate()
            .map(|(i, &(offset, bytes))| SegmentRef {
                server_index: 0,
                message_id: format!("m{i}@x"),
                bytes,
                offset_in_stream: offset,
            })
            .collect()
    }

    #[test]
    fn find_segment_at_boundaries() {
        let s = segs(&[(0, 100), (100, 100), (200, 50)]);
        assert_eq!(find_segment_for_byte(&s, 0), Some(0));
        assert_eq!(find_segment_for_byte(&s, 99), Some(0));
        assert_eq!(find_segment_for_byte(&s, 100), Some(1));
        assert_eq!(find_segment_for_byte(&s, 199), Some(1));
        assert_eq!(find_segment_for_byte(&s, 200), Some(2));
        assert_eq!(find_segment_for_byte(&s, 249), Some(2));
        assert_eq!(find_segment_for_byte(&s, 250), None);
    }

    #[test]
    fn find_segment_with_gap() {
        // Segments don't cover [100..200) — should return None for that byte.
        let s = segs(&[(0, 100), (200, 100)]);
        assert_eq!(find_segment_for_byte(&s, 50), Some(0));
        assert_eq!(find_segment_for_byte(&s, 100), None);
        assert_eq!(find_segment_for_byte(&s, 200), Some(1));
    }

    #[test]
    fn find_segment_empty_returns_none() {
        let s = segs(&[]);
        assert_eq!(find_segment_for_byte(&s, 0), None);
    }

    fn flat_layout(segs: &[SegmentRef]) -> crate::streaming::session::FileLayout {
        let total = segs
            .last()
            .map(|s| s.offset_in_stream + s.bytes)
            .unwrap_or(0);
        crate::streaming::session::FileLayout {
            segments: Arc::from(segs.to_vec().into_boxed_slice()),
            chunks: vec![crate::streaming::session::DataChunk {
                video_start: 0,
                length: total,
                assembled_start: 0,
            }],
        }
    }

    #[test]
    fn segment_indices_in_range_flat_picks_overlapping_segments() {
        let s = segs(&[(0, 100), (100, 100), (200, 100), (300, 100)]);
        let layout = flat_layout(&s);
        // Range [50..=250] spans segs 0,1,2.
        assert_eq!(
            segment_indices_in_range(&layout, &s, 50, 250),
            vec![0, 1, 2]
        );
        // Range entirely inside one segment.
        assert_eq!(segment_indices_in_range(&layout, &s, 110, 150), vec![1]);
        // Full range.
        assert_eq!(
            segment_indices_in_range(&layout, &s, 0, 399),
            vec![0, 1, 2, 3]
        );
    }

    #[test]
    fn segment_indices_in_range_skips_non_requested_chunks() {
        // Two video chunks mapping to non-contiguous assembled regions
        // (RAR-volume layout). Chunk A covers video [0..100) → assembled [0..100).
        // Chunk B covers video [100..200) → assembled [200..300) (skipping 100..200
        // which is RAR header bytes that belong to no video chunk).
        let s = segs(&[
            (0, 50),    // seg 0 — chunk A
            (50, 50),   // seg 1 — chunk A
            (100, 100), // seg 2 — RAR header bytes (NOT in any video chunk)
            (200, 50),  // seg 3 — chunk B
            (250, 50),  // seg 4 — chunk B
        ]);
        let layout = crate::streaming::session::FileLayout {
            segments: Arc::from(s.clone().into_boxed_slice()),
            chunks: vec![
                crate::streaming::session::DataChunk {
                    video_start: 0,
                    length: 100,
                    assembled_start: 0,
                },
                crate::streaming::session::DataChunk {
                    video_start: 100,
                    length: 100,
                    assembled_start: 200,
                },
            ],
        };
        // Asking for the entire video range should pick segs 0,1,3,4 — never seg 2.
        assert_eq!(
            segment_indices_in_range(&layout, &s, 0, 199),
            vec![0, 1, 3, 4]
        );
    }
}

#[cfg(test)]
mod prefetch_tests {
    //! Integration tests for the read-ahead prefetcher and the
    //! `serve_range_stream` orchestration.
    //!
    //! These tests use a `MockSegmentSource` that yEnc-encodes deterministic
    //! payloads on demand, so the full pipeline path (fetch → decode → cache
    //! write → yield) is exercised without touching the network.

    use super::*;
    use crate::streaming::disk_cache::{CachedFile, DiskCache};
    use crate::streaming::nntp::SegmentSource;
    use crate::streaming::session::{ActiveStream, DataChunk, FileLayout};
    use anyhow::anyhow;
    use futures::StreamExt;
    use nzb_decode::yenc::encode_article;
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Mutex as TokioMutex;

    /// Deterministic test payload for a segment of `len` bytes. Pattern is
    /// `((idx + i) % 251) as u8` — coprime modulus avoids accidental
    /// alignment with byte-power sizes when checking ranges.
    fn payload_for(seg_idx: usize, len: usize) -> Vec<u8> {
        (0..len).map(|i| ((seg_idx + i) % 251) as u8).collect()
    }

    /// Encode `data` into a yEnc article body that `nzb_decode::decode_yenc`
    /// will round-trip cleanly.
    fn yenc_encode(data: &[u8]) -> Vec<u8> {
        let (encoded, _crc) = encode_article(data, "test.bin", 1, 1, 0, data.len() as u64);
        encoded
    }

    /// In-memory mock implementing `SegmentSource`. Returns yEnc-encoded
    /// bytes for any registered message_id; can be configured to fail or
    /// stall to exercise the failover and concurrency paths.
    struct MockSource {
        payloads: HashMap<String, Vec<u8>>,
        fail_for: HashSet<String>,
        delay: Option<Duration>,
        servers: usize,
        inflight: AtomicUsize,
        max_inflight: AtomicUsize,
        call_count: AtomicUsize,
        /// Tracks per-server per-message_id call counts for failover assertions.
        server_calls: TokioMutex<Vec<HashMap<String, usize>>>,
    }

    impl MockSource {
        fn new(payloads: HashMap<String, Vec<u8>>, servers: usize) -> Arc<Self> {
            Arc::new(Self {
                payloads,
                fail_for: HashSet::new(),
                delay: None,
                servers,
                inflight: AtomicUsize::new(0),
                max_inflight: AtomicUsize::new(0),
                call_count: AtomicUsize::new(0),
                server_calls: TokioMutex::new(vec![HashMap::new(); servers]),
            })
        }

        fn with_delay(mut self: Arc<Self>, delay: Duration) -> Arc<Self> {
            // Single-owner at this point — Arc::get_mut is safe right after `new`.
            Arc::get_mut(&mut self).unwrap().delay = Some(delay);
            self
        }

        fn with_failures(mut self: Arc<Self>, ids: &[&str]) -> Arc<Self> {
            Arc::get_mut(&mut self)
                .unwrap()
                .fail_for
                .extend(ids.iter().map(|s| s.to_string()));
            self
        }
    }

    impl SegmentSource for MockSource {
        fn source_count(&self) -> usize {
            self.servers
        }

        async fn fetch_segment(&self, server_idx: usize, message_id: &str) -> Result<Vec<u8>> {
            let now = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_inflight.fetch_max(now, Ordering::SeqCst);
            self.call_count.fetch_add(1, Ordering::SeqCst);
            {
                let mut guard = self.server_calls.lock().await;
                *guard[server_idx].entry(message_id.to_string()).or_insert(0) += 1;
            }
            if let Some(d) = self.delay {
                tokio::time::sleep(d).await;
            }
            let result = if self.fail_for.contains(message_id) {
                Err(anyhow!("mock failure for {message_id}"))
            } else {
                self.payloads
                    .get(message_id)
                    .cloned()
                    .ok_or_else(|| anyhow!("mock has no payload for {message_id}"))
            };
            self.inflight.fetch_sub(1, Ordering::SeqCst);
            result
        }
    }

    /// Build a contiguous Flat-mode layout over `n` segments of `seg_size`
    /// bytes each. Returns (segments, layout, payloads-by-msg-id).
    fn build_flat_layout(
        n: usize,
        seg_size: usize,
    ) -> (Vec<SegmentRef>, FileLayout, HashMap<String, Vec<u8>>) {
        let mut segments = Vec::with_capacity(n);
        let mut payloads = HashMap::with_capacity(n);
        for i in 0..n {
            let msg_id = format!("seg{i}@test");
            let data = payload_for(i, seg_size);
            payloads.insert(msg_id.clone(), yenc_encode(&data));
            segments.push(SegmentRef {
                server_index: 0,
                message_id: msg_id,
                bytes: seg_size as u64,
                offset_in_stream: (i * seg_size) as u64,
            });
        }
        let total = (n * seg_size) as u64;
        let layout = FileLayout {
            segments: Arc::from(segments.clone().into_boxed_slice()),
            chunks: vec![DataChunk {
                video_start: 0,
                length: total,
                assembled_start: 0,
            }],
        };
        (segments, layout, payloads)
    }

    fn tempdir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nzb-prefetch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    async fn open_cache(total_size: u64) -> (PathBuf, Arc<CachedFile>) {
        let dir = tempdir();
        let cache = DiskCache::new(dir.clone(), 1024 * 1024 * 1024);
        let token = uuid::Uuid::new_v4().simple().to_string();
        let f = cache.open(&token, total_size).await.unwrap();
        (dir, f)
    }

    // ---------- prefetch_segments behavior ----------

    #[tokio::test]
    async fn prefetch_skips_already_cached_segments() {
        let (segs, _layout, payloads) = build_flat_layout(4, 1024);
        let (dir, cache) = open_cache(4 * 1024).await;
        let source = MockSource::new(payloads, 1);

        // Pre-populate segs 0 and 2 — prefetch should skip them entirely.
        let pre0 = payload_for(0, 1024);
        let pre2 = payload_for(2, 1024);
        cache.write_at(0, &pre0).await.unwrap();
        cache.write_at(2 * 1024, &pre2).await.unwrap();

        let segs_arc: Arc<[SegmentRef]> = Arc::from(segs.into_boxed_slice());
        prefetch_segments(cache.clone(), source.clone(), segs_arc, vec![0, 1, 2, 3], 4).await;

        // Mock should only have been hit for segs 1 and 3.
        assert_eq!(
            source.call_count.load(Ordering::SeqCst),
            2,
            "expected 2 fetches, got {}",
            source.call_count.load(Ordering::SeqCst)
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn prefetch_respects_concurrency_bound() {
        // 16 segments, each with a 50 ms simulated RTT, concurrency=4.
        // Max in-flight should never exceed 4.
        let (segs, _layout, payloads) = build_flat_layout(16, 256);
        let (dir, cache) = open_cache(16 * 256).await;
        let source = MockSource::new(payloads, 1).with_delay(Duration::from_millis(50));

        let segs_arc: Arc<[SegmentRef]> = Arc::from(segs.into_boxed_slice());
        prefetch_segments(
            cache.clone(),
            source.clone(),
            segs_arc,
            (0..16).collect(),
            4,
        )
        .await;

        let max = source.max_inflight.load(Ordering::SeqCst);
        assert!(max <= 4, "max_inflight {max} exceeded concurrency bound 4");
        // Sanity: with 16 fetches at concurrency 4, at least 2 should have been
        // simultaneously in flight at some point — otherwise we're effectively
        // sequential and the test isn't actually checking concurrency.
        assert!(
            max >= 2,
            "max_inflight {max} too low — concurrency not exercised"
        );
        assert_eq!(source.call_count.load(Ordering::SeqCst), 16);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn prefetch_swallows_per_segment_errors() {
        let (segs, _layout, payloads) = build_flat_layout(4, 1024);
        let (dir, cache) = open_cache(4 * 1024).await;
        // segs 1 and 3 should fail; the prefetcher must not panic and the
        // cache must be populated only for segs 0 and 2.
        let source = MockSource::new(payloads, 1).with_failures(&["seg1@test", "seg3@test"]);

        let segs_arc: Arc<[SegmentRef]> = Arc::from(segs.into_boxed_slice());
        prefetch_segments(cache.clone(), source.clone(), segs_arc, vec![0, 1, 2, 3], 4).await;

        assert!(cache.has_range(0, 1024), "seg 0 should be cached");
        assert!(
            !cache.has_range(1024, 2048),
            "seg 1 must NOT be cached (failed)"
        );
        assert!(
            cache.has_range(2 * 1024, 3 * 1024),
            "seg 2 should be cached"
        );
        assert!(
            !cache.has_range(3 * 1024, 4 * 1024),
            "seg 3 must NOT be cached (failed)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---------- ensure_segment_cached failover ----------

    #[tokio::test]
    async fn ensure_segment_cached_falls_back_to_secondary_server() {
        let (mut segs, _layout, payloads) = build_flat_layout(1, 512);
        // Force seg 0 to prefer server #0.
        segs[0].server_index = 0;
        let (dir, cache) = open_cache(512).await;

        // Source has 2 servers; seg fails on server #0 (we mark fail_for the
        // message_id, but we want it to fail only on server 0).
        // The MockSource as written fails universally — to test true
        // failover, build a source that fails only on server 0.
        struct ServerSpecificFailMock {
            inner: Arc<MockSource>,
        }
        impl SegmentSource for ServerSpecificFailMock {
            fn source_count(&self) -> usize {
                self.inner.source_count()
            }
            async fn fetch_segment(&self, server_idx: usize, message_id: &str) -> Result<Vec<u8>> {
                self.inner.call_count.fetch_add(1, Ordering::SeqCst);
                {
                    let mut guard = self.inner.server_calls.lock().await;
                    *guard[server_idx].entry(message_id.to_string()).or_insert(0) += 1;
                }
                if server_idx == 0 {
                    return Err(anyhow!("server 0 down for {message_id}"));
                }
                self.inner
                    .payloads
                    .get(message_id)
                    .cloned()
                    .ok_or_else(|| anyhow!("no payload"))
            }
        }
        let inner = MockSource::new(payloads, 2);
        let source = ServerSpecificFailMock {
            inner: inner.clone(),
        };

        ensure_segment_cached(&cache, &segs[0], &source)
            .await
            .unwrap();

        let server_calls = inner.server_calls.lock().await;
        assert_eq!(
            server_calls[0].get("seg0@test").copied().unwrap_or(0),
            1,
            "server 0 should have been tried first"
        );
        assert_eq!(
            server_calls[1].get("seg0@test").copied().unwrap_or(0),
            1,
            "server 1 should have been used as fallback"
        );
        assert!(cache.has_range(0, 512));
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---------- AbortOnDrop guard ----------

    #[tokio::test]
    async fn abort_on_drop_cancels_running_task() {
        // Spawn a task that would run for 5 seconds; drop the guard
        // immediately and confirm the task did NOT complete.
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_in = counter.clone();

        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            counter_in.store(1, Ordering::SeqCst);
        });
        {
            let _guard = AbortOnDrop(handle);
            // guard drops here
        }
        // Give the abort a moment to land.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "task body executed despite abort-on-drop"
        );
    }

    // ---------- end-to-end serve_range_stream ----------

    fn make_active_stream(
        layout: FileLayout,
        total_size: u64,
        cache: Arc<CachedFile>,
    ) -> Arc<ActiveStream> {
        Arc::new(ActiveStream {
            candidate_idx: 0,
            total_size,
            content_type: "video/x-matroska",
            file_layout: layout,
            cache_path: cache.path().to_path_buf(),
            cache_file: cache,
        })
    }

    async fn collect_stream_bytes(
        mut s: impl futures::Stream<Item = Result<Bytes>> + Unpin,
    ) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        while let Some(chunk) = s.next().await {
            out.extend_from_slice(&chunk?);
        }
        Ok(out)
    }

    #[tokio::test]
    async fn serve_range_stream_yields_correct_bytes_with_readahead() {
        let n = 8;
        let seg_size = 1024;
        let total = (n * seg_size) as u64;
        let (_segs, layout, payloads) = build_flat_layout(n, seg_size);
        let (dir, cache) = open_cache(total).await;
        let source = MockSource::new(payloads, 1);
        let active = make_active_stream(layout, total, cache.clone());

        let stream = serve_range_stream(active, cache, source.clone(), 0, total - 1, 4, None);
        let got = collect_stream_bytes(Box::pin(stream)).await.unwrap();

        let mut expected = Vec::with_capacity(total as usize);
        for i in 0..n {
            expected.extend_from_slice(&payload_for(i, seg_size));
        }
        assert_eq!(got, expected, "yielded bytes don't match expected");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn serve_range_stream_readahead_matches_sequential() {
        // Same payload, served twice — once with read_ahead=1 (sequential)
        // and once with read_ahead=8 (parallel). Output must be byte-identical.
        let n = 6;
        let seg_size = 2048;
        let total = (n * seg_size) as u64;

        let (_segs, layout1, payloads1) = build_flat_layout(n, seg_size);
        let (dir1, cache1) = open_cache(total).await;
        let src1 = MockSource::new(payloads1, 1);
        let active1 = make_active_stream(layout1, total, cache1.clone());
        let bytes_seq = collect_stream_bytes(Box::pin(serve_range_stream(
            active1,
            cache1,
            src1,
            0,
            total - 1,
            1,
            None,
        )))
        .await
        .unwrap();

        let (_segs, layout2, payloads2) = build_flat_layout(n, seg_size);
        let (dir2, cache2) = open_cache(total).await;
        let src2 = MockSource::new(payloads2, 1);
        let active2 = make_active_stream(layout2, total, cache2.clone());
        let bytes_ra = collect_stream_bytes(Box::pin(serve_range_stream(
            active2,
            cache2,
            src2,
            0,
            total - 1,
            8,
            None,
        )))
        .await
        .unwrap();

        assert_eq!(bytes_seq, bytes_ra, "read-ahead changed the byte stream");
        std::fs::remove_dir_all(&dir1).ok();
        std::fs::remove_dir_all(&dir2).ok();
    }

    #[tokio::test]
    async fn serve_range_stream_partial_range() {
        // Request middle range — should yield only those bytes.
        let n = 8;
        let seg_size = 1024;
        let total = (n * seg_size) as u64;
        let (_segs, layout, payloads) = build_flat_layout(n, seg_size);
        let (dir, cache) = open_cache(total).await;
        let source = MockSource::new(payloads, 1);
        let active = make_active_stream(layout, total, cache.clone());

        // [1500..=4500] — cuts through segs 1, 2, 3, 4 partially.
        let start = 1500u64;
        let end = 4500u64;
        let stream = serve_range_stream(active, cache, source, start, end, 4, None);
        let got = collect_stream_bytes(Box::pin(stream)).await.unwrap();

        // Build expected by concatenating the full file, then slicing.
        let mut full = Vec::with_capacity(total as usize);
        for i in 0..n {
            full.extend_from_slice(&payload_for(i, seg_size));
        }
        let expected = &full[start as usize..=end as usize];
        assert_eq!(got, expected, "partial range bytes don't match");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---------- eviction during streaming ----------

    #[tokio::test]
    async fn serve_range_stream_evicts_behind_playhead() {
        // 64 segments × 64 KiB = 4 MiB total. Header pin = 64 KiB,
        // backbuffer = 256 KiB, step = 256 KiB. After fully streaming,
        // most of the cache should be evicted — only [0..header_pin)
        // and the last [playhead-backbuffer..total) should remain
        // populated.
        let n = 64;
        let seg_size = 64 * 1024;
        let total = (n * seg_size) as u64;
        let header_pin = 64 * 1024u64;
        let backbuffer = 256 * 1024u64;
        let step = 256 * 1024u64;

        let (_segs, layout, payloads) = build_flat_layout(n, seg_size);
        let (dir, cache) = open_cache(total).await;
        let source = MockSource::new(payloads, 1);
        let active = make_active_stream(layout, total, cache.clone());

        let policy = EvictPolicy {
            header_pin,
            backbuffer,
            step,
        };
        let stream =
            serve_range_stream(active, cache.clone(), source, 0, total - 1, 4, Some(policy));
        let _ = collect_stream_bytes(Box::pin(stream)).await.unwrap();

        // After full playback:
        // - [0..header_pin) — pinned, must still be populated
        // - [header_pin..total - backbuffer ) — should be evicted
        // - [total - backbuffer..total) — still populated (within backbuffer)
        let backbuf_floor = total - backbuffer;
        assert!(
            cache.has_range(0, header_pin),
            "header pin region should survive eviction"
        );
        assert!(
            cache.has_range(backbuf_floor, total),
            "backbuffer region (last {backbuffer} bytes) should remain populated"
        );
        // The middle section should have been evicted. Sample a point safely
        // far from both pin and backbuffer boundaries.
        let mid = (header_pin + backbuf_floor) / 2;
        assert!(
            !cache.has_range(mid, mid + 1024),
            "mid region should be evicted (mid={mid})"
        );
        // populated_bytes should be approximately header_pin + backbuffer,
        // give or take one step's worth of latency.
        let populated = cache.populated_bytes();
        assert!(
            populated <= header_pin + backbuffer + step,
            "populated {populated} should be ≤ header_pin + backbuffer + step ({})",
            header_pin + backbuffer + step
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn seek_back_into_evicted_range_refetches() {
        // Stream once with eviction enabled — leaves most of the cache
        // punched. Then issue a Range request for a byte deep in the
        // evicted region; verify the pipeline refetches via the source
        // (call_count grew) and yields correct bytes.
        let n = 32;
        let seg_size = 64 * 1024;
        let total = (n * seg_size) as u64;
        let header_pin = 64 * 1024u64;
        let backbuffer = 128 * 1024u64;
        let step = 128 * 1024u64;

        let (_segs, layout, payloads) = build_flat_layout(n, seg_size);
        let (dir, cache) = open_cache(total).await;
        let source = MockSource::new(payloads, 1);
        let active = make_active_stream(layout.clone(), total, cache.clone());

        let policy = EvictPolicy {
            header_pin,
            backbuffer,
            step,
        };

        // First pass: full playback with eviction.
        let stream = serve_range_stream(
            active.clone(),
            cache.clone(),
            source.clone(),
            0,
            total - 1,
            4,
            Some(policy),
        );
        let _ = collect_stream_bytes(Box::pin(stream)).await.unwrap();
        let calls_after_first_pass = source.call_count.load(Ordering::SeqCst);

        // Pick a byte that should now be in a punched hole.
        let evicted_byte = (header_pin + (total - backbuffer)) / 2;
        assert!(
            !cache.has_range(evicted_byte, evicted_byte + 1024),
            "test setup: evicted_byte must actually be punched"
        );

        // Second pass: small Range request inside the evicted region.
        let req_start = evicted_byte;
        let req_end = evicted_byte + 4096 - 1;
        let stream = serve_range_stream(
            active,
            cache.clone(),
            source.clone(),
            req_start,
            req_end,
            4,
            Some(policy),
        );
        let got = collect_stream_bytes(Box::pin(stream)).await.unwrap();

        // Refetch must have happened.
        let calls_after_seek = source.call_count.load(Ordering::SeqCst);
        assert!(
            calls_after_seek > calls_after_first_pass,
            "seek-back into evicted region should have triggered refetch ({} → {})",
            calls_after_first_pass,
            calls_after_seek
        );

        // And bytes must match.
        let mut full = Vec::with_capacity(total as usize);
        for i in 0..n {
            full.extend_from_slice(&payload_for(i, seg_size));
        }
        let expected = &full[req_start as usize..=req_end as usize];
        assert_eq!(got, expected, "refetched bytes should match original");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn eviction_disabled_keeps_full_cache() {
        // Passing None for evict policy should preserve the legacy
        // behavior: cache grows monotonically as bytes are fetched.
        let n = 16;
        let seg_size = 64 * 1024;
        let total = (n * seg_size) as u64;
        let (_segs, layout, payloads) = build_flat_layout(n, seg_size);
        let (dir, cache) = open_cache(total).await;
        let source = MockSource::new(payloads, 1);
        let active = make_active_stream(layout, total, cache.clone());

        let stream = serve_range_stream(active, cache.clone(), source, 0, total - 1, 4, None);
        let _ = collect_stream_bytes(Box::pin(stream)).await.unwrap();

        // Without eviction the entire cache should be populated.
        assert_eq!(
            cache.populated_bytes(),
            total,
            "without eviction the entire stream should remain in cache"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
