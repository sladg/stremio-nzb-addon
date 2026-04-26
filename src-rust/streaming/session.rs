//! Session registry, ActiveStream + FileLayout types.

use dashmap::DashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::OnceCell;

use crate::streaming::candidate::NzbCandidate;
use crate::streaming::disk_cache::CachedFile;

#[derive(Debug)]
pub struct StreamSession {
    pub token: String,
    pub candidates: Arc<Vec<NzbCandidate>>,
    pub start_idx: usize,
    /// Lazily populated by the first byte request. Concurrent first-bytes
    /// collapse to a single pre-flight via `OnceCell`. Stored as `Arc` so
    /// per-request access is a refcount bump rather than a deep clone of
    /// the (small but non-trivial) `ActiveStream`.
    pub active: OnceCell<Arc<ActiveStream>>,
    /// Unix-ms when the session was created. Immutable.
    pub created_at: u64,
    /// Unix-ms of the last byte request. Updated by `touch()`. Drives the
    /// GC's idle-eviction and cap-eviction-LRU decisions.
    pub last_access: AtomicU64,
}

impl StreamSession {
    /// Update `last_access` to "now". Called once per `/v/{token}.mkv` request.
    pub fn touch(&self) {
        self.last_access.store(now_ms(), Ordering::Relaxed);
    }

    /// Read `last_access` (ms since unix epoch).
    pub fn last_access_ms(&self) -> u64 {
        self.last_access.load(Ordering::Relaxed)
    }
}

/// Unix-ms timestamp. Saturates at 0 if the system clock is before 1970
/// (which never happens, but the unwrap-free path is cheap insurance).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub type SessionRegistry = Arc<DashMap<String, Arc<StreamSession>>>;

pub fn new_registry() -> SessionRegistry {
    Arc::new(DashMap::new())
}

/// Generate a URL-safe token (32 chars, ~128 bits of entropy).
pub fn new_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Register a session whose candidate list is the full group of equivalent
/// uploads (sharing the same `GroupSignature`). Pre-flight walks the list
/// in order and commits to the first one that succeeds.
///
/// Phase 5 entry point. Caller groups candidates upstream via
/// `streaming::candidate::group_candidates` and passes one bucket per call.
pub fn register_group(registry: &SessionRegistry, candidates: Vec<NzbCandidate>) -> String {
    debug_assert!(
        !candidates.is_empty(),
        "register_group called with empty list"
    );
    let token = new_token();
    let now = now_ms();
    let session = Arc::new(StreamSession {
        token: token.clone(),
        candidates: Arc::new(candidates),
        start_idx: 0,
        active: OnceCell::new(),
        created_at: now,
        last_access: AtomicU64::new(now),
    });
    registry.insert(token.clone(), session);
    token
}

/// A committed, ready-to-serve stream. Produced by `preflight::probe_candidate`,
/// which dispatches to Flat-mode or RAR-mode internally. Both paths build the
/// same unified `FileLayout` shape — Flat is a degenerate 1-chunk case of RAR.
#[derive(Debug, Clone)]
pub struct ActiveStream {
    /// Index into the candidates vec that was committed.
    pub candidate_idx: usize,
    /// Total video bytes Stremio will be asked to play.
    pub total_size: u64,
    /// e.g. `"video/x-matroska"`, `"video/mp4"`.
    pub content_type: &'static str,
    /// Layout of the video bytes within the assembled NZB segment stream.
    pub file_layout: FileLayout,
    /// Sparse cache file path. Populated lazily by Phase 3+ as bytes flow.
    pub cache_path: PathBuf,
    /// Live cache handle. Populated-range tracking persists across requests
    /// because a single `CachedFile` instance is shared (vs. re-opening,
    /// which would lose the in-memory `PopulatedRanges` state and silently
    /// re-fetch already-cached segments).
    pub cache_file: Arc<CachedFile>,
}

/// Maps user-visible "video bytes" to the underlying assembled segment stream.
///
/// `segments` are the concatenated NZB segments across all volumes (in order).
/// `chunks` describe contiguous regions of video bytes that map continuously
/// into the assembled stream:
///   - For Flat-mode releases: one chunk covering the entire range.
///   - For RAR releases: one chunk per volume; RAR header bytes between
///     volumes are skipped (they live in the assembled stream but aren't
///     part of the embedded video file's bytes).
#[derive(Debug, Clone)]
pub struct FileLayout {
    /// Stored as `Arc<[T]>` so range handlers can clone the handle (one
    /// pointer copy) instead of cloning the whole segment vector — this
    /// matters for releases with thousands of segments.
    pub segments: Arc<[SegmentRef]>,
    pub chunks: Vec<DataChunk>,
}

impl FileLayout {
    /// Translate a "video byte" position to its corresponding "assembled
    /// stream" byte position, returning `None` if `video_byte` is past the
    /// end of all chunks.
    pub fn video_to_assembled(&self, video_byte: u64) -> Option<u64> {
        let chunk = self
            .chunks
            .iter()
            .find(|c| video_byte >= c.video_start && video_byte < c.video_start + c.length)?;
        Some(chunk.assembled_start + (video_byte - chunk.video_start))
    }

    /// How many continuous video bytes can be served starting at
    /// `video_byte` before crossing into a different chunk (or the end).
    /// Returns 0 if `video_byte` is past the last chunk.
    pub fn chunk_remaining(&self, video_byte: u64) -> u64 {
        for c in &self.chunks {
            let chunk_end = c.video_start + c.length;
            if video_byte >= c.video_start && video_byte < chunk_end {
                return chunk_end - video_byte;
            }
        }
        0
    }

    /// Total size of the assembled segment stream (= last segment's end
    /// offset). For RAR releases this exceeds `total_size` because RAR
    /// volumes include header/trailer bytes around the embedded file's
    /// data. The disk cache file must be sized to *this*, not `total_size`,
    /// otherwise writes for trailing-volume segments overflow.
    pub fn assembled_size(&self) -> u64 {
        self.segments
            .last()
            .map(|s| s.offset_in_stream + s.bytes)
            .unwrap_or(0)
    }
}

#[derive(Debug, Clone)]
pub struct DataChunk {
    /// Video-byte offset where this chunk begins (inclusive).
    pub video_start: u64,
    /// Length of the chunk in bytes.
    pub length: u64,
    /// Assembled-stream byte offset where this chunk's bytes begin.
    pub assembled_start: u64,
}

#[derive(Debug, Clone)]
pub struct SegmentRef {
    /// Index into the AddonConfig's nntp_servers list. Phase 2 always uses 0.
    pub server_index: usize,
    pub message_id: String,
    /// Declared size in bytes (from NZB's `<segment bytes="...">`). The actual
    /// decoded payload may differ slightly because of yEnc overhead.
    pub bytes: u64,
    /// Cumulative offset of this segment's first byte in the assembled stream.
    pub offset_in_stream: u64,
}

/// Pick a content-type hint from a filename extension. Phase 2 only needs
/// the common video containers; Phase 3 may extend.
pub fn guess_content_type(file_name: &str) -> &'static str {
    let lower = file_name.to_ascii_lowercase();
    if lower.ends_with(".mkv") {
        "video/x-matroska"
    } else if lower.ends_with(".mp4") || lower.ends_with(".m4v") {
        "video/mp4"
    } else if lower.ends_with(".avi") {
        "video/x-msvideo"
    } else if lower.ends_with(".webm") {
        "video/webm"
    } else if lower.ends_with(".ts") {
        "video/mp2t"
    } else {
        "application/octet-stream"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout_with_chunks(chunks: Vec<(u64, u64, u64)>) -> FileLayout {
        FileLayout {
            segments: Vec::<SegmentRef>::new().into(),
            chunks: chunks
                .into_iter()
                .map(|(video_start, length, assembled_start)| DataChunk {
                    video_start,
                    length,
                    assembled_start,
                })
                .collect(),
        }
    }

    #[test]
    fn flat_layout_video_to_assembled_is_identity() {
        let layout = layout_with_chunks(vec![(0, 1000, 0)]);
        assert_eq!(layout.video_to_assembled(0), Some(0));
        assert_eq!(layout.video_to_assembled(500), Some(500));
        assert_eq!(layout.video_to_assembled(999), Some(999));
        assert_eq!(layout.video_to_assembled(1000), None);
    }

    #[test]
    fn rar_layout_skips_header_bytes_between_chunks() {
        // Three volumes:
        // V1: assembled [0..120) (header 20 bytes + data 100 bytes), data [20..120)
        // V2: assembled [120..240) (header 20 + data 100), data [140..240)
        // V3: assembled [240..360) (header 20 + data 100), data [260..360)
        let layout = layout_with_chunks(vec![(0, 100, 20), (100, 100, 140), (200, 100, 260)]);
        assert_eq!(layout.video_to_assembled(0), Some(20)); // start of vol1 data
        assert_eq!(layout.video_to_assembled(99), Some(119));
        assert_eq!(layout.video_to_assembled(100), Some(140)); // start of vol2 data
        assert_eq!(layout.video_to_assembled(199), Some(239));
        assert_eq!(layout.video_to_assembled(200), Some(260)); // start of vol3 data
        assert_eq!(layout.video_to_assembled(299), Some(359));
        assert_eq!(layout.video_to_assembled(300), None);
    }

    #[test]
    fn chunk_remaining_stops_at_chunk_boundary() {
        let layout = layout_with_chunks(vec![(0, 100, 20), (100, 100, 140)]);
        assert_eq!(layout.chunk_remaining(0), 100);
        assert_eq!(layout.chunk_remaining(50), 50);
        assert_eq!(layout.chunk_remaining(99), 1);
        assert_eq!(layout.chunk_remaining(100), 100);
        assert_eq!(layout.chunk_remaining(150), 50);
        assert_eq!(layout.chunk_remaining(200), 0);
    }

    #[test]
    fn content_type_for_common_extensions() {
        assert_eq!(guess_content_type("Movie.mkv"), "video/x-matroska");
        assert_eq!(guess_content_type("Show.S01E01.MP4"), "video/mp4");
        assert_eq!(guess_content_type("clip.m4v"), "video/mp4");
        assert_eq!(guess_content_type("oldschool.avi"), "video/x-msvideo");
        assert_eq!(guess_content_type("stream.webm"), "video/webm");
        assert_eq!(guess_content_type("broadcast.ts"), "video/mp2t");
        assert_eq!(
            guess_content_type("noextension"),
            "application/octet-stream"
        );
    }
}
