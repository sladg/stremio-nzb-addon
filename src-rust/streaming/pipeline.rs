//! Streaming pipeline: serve a byte range from the cache, fetching + decoding
//! NZB segments on-demand for any cache misses.
//!
//! Phase 3: sequential serve, no read-ahead. The async stream walks
//! `[start..=end]` segment-by-segment, ensures each one is in cache (fetching
//! via NNTP + yEnc-decoding if not), and yields the appropriate slice.

use anyhow::{anyhow, Result};
use async_stream::try_stream;
use bytes::Bytes;
use futures::Stream;
use nzb_decode::yenc::decode_yenc;
use std::sync::Arc;

use crate::streaming::disk_cache::CachedFile;
use crate::streaming::nntp::NntpPool;
use crate::streaming::session::{ActiveStream, SegmentRef};

/// Maximum chunk size yielded per `Stream::poll_next`. Keeps memory bounded.
const CHUNK_SIZE: usize = 256 * 1024;

/// Serve `[start..=end]` (inclusive) as an `async Stream<Item = Result<Bytes>>`.
/// Each yielded chunk is at most `CHUNK_SIZE` bytes so very large ranges don't
/// balloon memory.
pub fn serve_range_stream(
    active: Arc<ActiveStream>,
    cache: Arc<CachedFile>,
    nntp: Arc<NntpPool>,
    start: u64,
    end: u64,
) -> impl Stream<Item = Result<Bytes>> + Send + 'static {
    try_stream! {
        if start > end {
            return;
        }
        if nntp.server_count() == 0 {
            Err(anyhow!("no NNTP servers configured"))?;
        }

        // Cheap clone: bumps the Arc refcount, no Vec copy.
        let segments: Arc<[SegmentRef]> = active.file_layout.segments.clone();

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
                    ensure_segment_cached(&cache, seg, &nntp).await?;
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
            }

            cursor = video_chunk_end + 1;
        }
    }
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
async fn ensure_segment_cached(
    cache: &CachedFile,
    seg: &SegmentRef,
    nntp: &Arc<NntpPool>,
) -> Result<()> {
    tracing::debug!(
        "[pipeline] fetching segment msg={} declared_bytes={} offset_in_stream={}",
        seg.message_id,
        seg.bytes,
        seg.offset_in_stream,
    );

    let server_count = nntp.server_count();
    if server_count == 0 {
        return Err(anyhow!("no NNTP servers configured"));
    }

    let preferred = seg.server_index.min(server_count - 1);
    let mut order: Vec<usize> = (0..server_count).collect();
    order.swap(0, preferred);

    let declared = seg.bytes as usize;
    let mut last_err: Option<anyhow::Error> = None;

    for idx in order {
        let raw = match nntp.fetch_article(idx, &seg.message_id).await {
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
}
