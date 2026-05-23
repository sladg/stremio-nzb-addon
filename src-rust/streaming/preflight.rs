//! Pre-flight probing of an NZB candidate to commit a `StreamSession`.
//!
//! Two modes:
//! - **Flat:** single .mkv / .mp4 / .avi / .webm / .ts spread across NZB
//!   segments. Maps 1:1 between video bytes and assembled segment bytes.
//! - **RAR:** multi-volume RAR archive with a single embedded video file
//!   stored uncompressed (method 0). Uses `nzbdav-rar` to parse each
//!   volume's header and build a per-volume `DataChunk` map that skips
//!   header bytes between volumes.

use anyhow::Context;
use nzb_decode::yenc::decode_yenc;
use nzb_rs::{File as NzbFile, Nzb};
use nzbdav_rar::{parse_all_headers, FileHeader, RarHeader};
use once_cell::sync::Lazy;
use regex::Regex;
use thiserror::Error;

use crate::streaming::disk_cache::DiskCache;
use crate::streaming::nntp::NntpPool;
use crate::streaming::session::{
    guess_content_type, ActiveStream, DataChunk, FileLayout, SegmentRef,
};
use std::sync::Arc;

/// Failure modes for `probe_candidate`. Caller advances to the next
/// candidate on any of these except `Internal` (which indicates a bug).
#[derive(Debug, Error)]
pub enum PreflightError {
    #[error("HTTP fetch failed: {0}")]
    HttpFetch(String),
    #[error("HTTP {0}")]
    HttpStatus(u16),
    #[error("NZB too large: {0} bytes")]
    NzbTooLarge(u64),
    #[error("NZB parse failed: {0}")]
    NzbParse(String),
    #[error("no playable file found in NZB")]
    NoPlayableFile,
    #[error("NNTP fetch failed: {0}")]
    NntpFetch(String),
    #[error("yEnc decode failed: {0}")]
    Decode(String),
    #[error("decoded payload empty")]
    EmptyPayload,
    #[error("no NNTP servers configured")]
    NoNntpServers,
    #[error("RAR archive is encrypted (password protected)")]
    RarEncrypted,
    #[error("RAR uses non-store compression: method {0}")]
    RarCompressed(u8),
    #[error("RAR header parse failed for volume {volume}: {reason}")]
    RarHeaderParse { volume: usize, reason: String },
    #[error("RAR archive contains no playable file")]
    RarNoVideoFile,
    #[error("RAR header overflow: not enough data in first segment for volume {0}")]
    RarHeaderOverflow(usize),
    #[error("internal error: {0}")]
    Internal(#[from] anyhow::Error),
}

/// What kind of file layout the NZB ships.
#[derive(Debug, PartialEq)]
pub enum DetectedLayout<'a> {
    Flat(&'a NzbFile),
    Rar,
}

/// Pick the playable file in an NZB.
///
/// Decision tree (matches Usenet-Ultimate's heuristic + the user's TS reference):
///   1. If any file matches RAR conventions → `Rar` (Phase 4 territory).
///   2. Else, return the largest non-`.par2` file with a recognized video
///      extension, or the largest non-par2 file overall (obfuscated names).
pub fn pick_main_file(nzb: &Nzb) -> Result<DetectedLayout<'_>, PreflightError> {
    if nzb.files.iter().any(|f| f.is_rar()) {
        return Ok(DetectedLayout::Rar);
    }

    // Prefer a file whose name has a known video extension. Fall back to
    // largest non-par2 (`Nzb::file()` already does this).
    let prefer_video = nzb
        .files
        .iter()
        .filter(|f| !f.is_par2())
        .filter(|f| {
            f.name()
                .map(|n| {
                    let l = n.to_ascii_lowercase();
                    l.ends_with(".mkv")
                        || l.ends_with(".mp4")
                        || l.ends_with(".m4v")
                        || l.ends_with(".avi")
                        || l.ends_with(".webm")
                        || l.ends_with(".ts")
                })
                .unwrap_or(false)
        })
        .max_by_key(|f| f.size());

    match prefer_video {
        Some(f) => Ok(DetectedLayout::Flat(f)),
        None => {
            // Obfuscated path: just pick the largest non-par2 file.
            let f = nzb
                .files
                .iter()
                .filter(|f| !f.is_par2())
                .max_by_key(|f| f.size())
                .ok_or(PreflightError::NoPlayableFile)?;
            Ok(DetectedLayout::Flat(f))
        }
    }
}

/// Build `SegmentRef` list with cumulative byte offsets, sorted by segment
/// number ascending.
pub fn build_segment_refs(file: &NzbFile, server_index: usize) -> Vec<SegmentRef> {
    let mut sorted: Vec<&nzb_rs::Segment> = file.segments.iter().collect();
    sorted.sort_by_key(|s| s.number);

    let mut out = Vec::with_capacity(sorted.len());
    let mut offset: u64 = 0;
    for seg in sorted {
        let bytes = seg.size as u64;
        out.push(SegmentRef {
            server_index,
            message_id: seg.message_id.clone(),
            bytes,
            offset_in_stream: offset,
        });
        offset += bytes;
    }
    out
}

/// Rewrite a Flat-mode segment list so per-segment offsets and sizes use
/// the *actual decoded* payload size as the stride, not the indexer's
/// `<segment bytes="…">` value.
///
/// Background: NZB indexers disagree on whether `<segment bytes>` means the
/// yEnc-encoded article body length (~3% larger) or the decoded payload.
/// When it means the encoded length, the original `build_segment_refs`
/// over-spaces offsets and the streaming layer ends up writing real data
/// followed by sparse-file zeros at the tail of every segment — that's
/// what corrupts the assembled stream and shows up as 1–2 sec stutter.
///
/// Inputs:
///   - `actual_stride`: decoded byte length of segment 0, observed at
///     preflight. Assumed uniform across all segments except possibly
///     the last (which is typically smaller).
///   - `total_file_size`: from `=ybegin size=…` in the yEnc header. When
///     present, gives the exact tail size; when `None`, the last segment
///     is assumed to be a full stride too (slight overestimate, acceptable
///     fallback for the rare poster who omits the header).
///
/// No-op when `segments[0].bytes == actual_stride` — the indexer is
/// already reporting decoded sizes correctly and the existing layout is
/// authoritative.
pub fn rebuild_for_decoded_stride(
    segments: &mut [SegmentRef],
    actual_stride: u64,
    total_file_size: Option<u64>,
) -> u64 {
    if segments.is_empty() {
        return 0;
    }
    if segments[0].bytes == actual_stride {
        // Indexer already reports decoded sizes — keep existing layout.
        return segments.iter().map(|s| s.bytes).sum();
    }
    let n = segments.len() as u64;
    let computed_total = match total_file_size {
        Some(t) if t > 0 && t >= (n - 1) * actual_stride => t,
        _ => n * actual_stride, // fallback: assume uniform; cache may overestimate by <1 stride
    };
    for (i, seg) in segments.iter_mut().enumerate() {
        let i = i as u64;
        seg.offset_in_stream = i * actual_stride;
        seg.bytes = if i < n - 1 {
            actual_stride
        } else {
            // Last segment carries the file tail (≤ stride).
            computed_total.saturating_sub(i * actual_stride).min(actual_stride)
        };
    }
    computed_total
}

/// Per-volume decoded geometry needed to lay a RAR volume's segments out in
/// *decoded* space. `data_start`/`data_size` come from the RAR file header
/// (decoded-space offsets); `decoded_stride`/`decoded_file_size` come from
/// decoding the volume's first segment (the yEnc part size and `=ybegin size=`).
pub(crate) struct RarVolumeGeom {
    /// Volume segment message-ids, sorted by yEnc part number ascending.
    pub message_ids: Vec<String>,
    /// Decoded payload size of part 0 — the uniform per-part stride (yEnc
    /// posts use a fixed part size except the last). Mirrors the Flat
    /// `rebuild_for_decoded_stride` assumption.
    pub decoded_stride: u64,
    /// Total decoded size of the whole volume (.rar file) from the yEnc
    /// `=ybegin size=`. Gives the exact size of the partial last part;
    /// `None` falls back to assuming the last part is a full stride.
    pub decoded_file_size: Option<u64>,
    /// Decoded byte offset where the embedded file's data begins in this
    /// volume (after the RAR headers).
    pub data_start: u64,
    /// Decoded byte length of the embedded file's data carried by this volume.
    pub data_size: u64,
}

/// Build a RAR `FileLayout` (segments + chunks) in *decoded* space.
///
/// The bug this fixes: the original RAR path cumulated segment offsets from
/// the NZB `<segment bytes>` (yEnc-*encoded* lengths, ~3% larger than the
/// decoded payload), while `data_start`/`data_size` are decoded-space. The
/// streaming layer writes decoded bytes at encoded offsets, leaving a
/// sparse-zero gap at every segment tail — corrupting playback.
///
/// Here segment offsets cumulate the *decoded* stride, so the assembled
/// stream equals the real decoded concatenation and `data_start` lines up.
/// Returns `(segments, chunks, total_video_size)` where total is the sum of
/// per-volume `data_size`.
pub(crate) fn build_rar_layout(volumes: &[RarVolumeGeom]) -> (Vec<SegmentRef>, Vec<DataChunk>, u64) {
    let mut segments: Vec<SegmentRef> = Vec::new();
    let mut chunks: Vec<DataChunk> = Vec::new();
    let mut assembled: u64 = 0;
    let mut video: u64 = 0;

    for v in volumes {
        let n = v.message_ids.len() as u64;
        let stride = v.decoded_stride;
        // Whole-volume decoded size. Prefer the yEnc `=ybegin size=`; fall
        // back to a full-stride last part when it's missing or implausible.
        let vol_total = match v.decoded_file_size {
            Some(fs) if fs > 0 && fs >= n.saturating_sub(1) * stride => fs,
            _ => n * stride,
        };
        let vol_start = assembled;
        for (i, msg) in v.message_ids.iter().enumerate() {
            let i = i as u64;
            let bytes = if i + 1 < n {
                stride
            } else {
                // Last part carries the volume tail (= total − preceding parts).
                vol_total.saturating_sub((n - 1) * stride)
            };
            segments.push(SegmentRef {
                server_index: 0,
                message_id: msg.clone(),
                bytes,
                offset_in_stream: assembled,
            });
            assembled += bytes;
        }
        chunks.push(DataChunk {
            video_start: video,
            length: v.data_size,
            assembled_start: vol_start + v.data_start,
        });
        video += v.data_size;
    }

    (segments, chunks, video)
}

/// Probe a single NZB URL for Flat-mode playability. Returns a fully-built
/// `ActiveStream` ready for Phase 3+ to serve bytes from.
pub async fn probe_candidate(
    http: &reqwest::Client,
    nzb_url: &str,
    nntp: &Arc<NntpPool>,
    cache: &DiskCache,
    token: &str,
) -> Result<ActiveStream, PreflightError> {
    let result = probe_candidate_inner(http, nzb_url, nntp, cache, token).await;

    // If pre-flight died because the smoke article isn't on any server, the
    // search-time availability cache (24 h TTL) almost certainly holds a
    // stale "ok" verdict — invalidate it so the next search re-probes from
    // scratch instead of resurrecting the same dead URL. Other error kinds
    // (compression, encryption, parse failure) reflect the NZB's structure,
    // not article availability, so we leave the cache alone for those.
    if matches!(&result, Err(PreflightError::NntpFetch(_))) {
        crate::nzb_availability::invalidate_for(nzb_url, nntp.server_urls()).await;
    }

    result
}

async fn probe_candidate_inner(
    http: &reqwest::Client,
    nzb_url: &str,
    nntp: &Arc<NntpPool>,
    cache: &DiskCache,
    token: &str,
) -> Result<ActiveStream, PreflightError> {
    if nntp.server_count() == 0 {
        return Err(PreflightError::NoNntpServers);
    }

    let nzb = fetch_and_parse(http, nzb_url).await?;
    let cache_path = cache.root.join(format!("{token}.bin"));

    let main = match pick_main_file(&nzb)? {
        DetectedLayout::Rar => {
            return probe_rar_inner(&nzb, nntp, cache, token).await;
        }
        DetectedLayout::Flat(f) => f,
    };

    let mut segments = build_segment_refs(main, 0);
    if segments.is_empty() {
        return Err(PreflightError::NoPlayableFile);
    }

    // Smoke the first segment via the pooled connection — same TCP+TLS+AUTH
    // gets reused for the actual segment fetches once playback starts.
    // Per-server failover at this layer too: if the first server doesn't
    // have the article, walk the rest before giving up on the candidate.
    let first = &segments[0];
    let (_idx, raw) = nntp
        .fetch_with_failover(first.server_index, &first.message_id)
        .await
        .map_err(|e| PreflightError::NntpFetch(crate::util::redact_log(&e.to_string())))?;
    let decoded = decode_yenc(&raw).map_err(|e| PreflightError::Decode(format!("{e:?}")))?;
    if decoded.data.is_empty() {
        return Err(PreflightError::EmptyPayload);
    }

    // Reconcile NZB-declared sizes against the actual decoded payload.
    // Some indexers report yEnc-encoded body lengths in `<segment bytes>`,
    // others report decoded payload lengths. When they mismatch, the
    // original cumulative-offset layout is wrong, and the streaming layer
    // ends up writing real bytes followed by sparse-file zeros — corrupts
    // the assembled stream. Detect via segment-0 decode and rebuild.
    let actual_stride = decoded.data.len() as u64;
    let pre_rebuild_total: u64 = segments.iter().map(|s| s.bytes).sum();
    let total_size = rebuild_for_decoded_stride(&mut segments, actual_stride, decoded.file_size);
    if total_size != pre_rebuild_total {
        tracing::info!(
            "[preflight] flat-mode segment offsets rebuilt: stride {} -> {} bytes, total {} -> {} bytes (indexer reported encoded sizes)",
            segments.first().map(|s| s.bytes).unwrap_or(0),
            actual_stride,
            pre_rebuild_total,
            total_size,
        );
    }

    let content_type = main
        .name()
        .map(guess_content_type)
        .unwrap_or("application/octet-stream");

    // Flat-mode = single chunk covering the whole video, identity-mapped.
    let chunks = vec![DataChunk {
        video_start: 0,
        length: total_size,
        assembled_start: 0,
    }];

    let layout = FileLayout {
        segments: segments.into(),
        chunks,
    };
    let cache_file = cache
        .open(token, layout.assembled_size())
        .await
        .map_err(|e| PreflightError::Internal(anyhow::anyhow!("cache open: {e}")))?;

    Ok(ActiveStream {
        candidate_idx: 0,
        total_size,
        content_type,
        file_layout: layout,
        cache_path,
        cache_file,
    })
}

/// Sort order for old-style RAR multi-volume sets:
///   `.rar` (volume 0) → `.r00` (volume 1) → `.r01` (volume 2) → ...
/// Returns `(stem_match, volume_number)`. Stem match means the filenames
/// share the same base name (everything before the extension).
fn old_style_volume_number(name: &str) -> Option<u32> {
    static RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?i)\.r(\d{2,3})(?:[^.\w]|$)").expect("rNN regex"));
    RE.captures(name)
        .and_then(|c| c.get(1)?.as_str().parse::<u32>().ok())
        .map(|n| n + 1) // .r00 is volume 2 (.rar is volume 1)
}

/// `partNN.rar` (case-insensitive) — captures `NN`.
fn part_volume_number(name: &str) -> Option<u32> {
    static RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?i)\.part(\d+)\.rar(?:[^.\w]|$)").expect("part regex"));
    RE.captures(name)
        .and_then(|c| c.get(1)?.as_str().parse::<u32>().ok())
}

/// Find all RAR volumes in an NZB, sorted by volume number.
/// Returns at most one of:
///
/// - Modern style: `*.part01.rar`, `*.part02.rar`, ...
/// - Old style: `*.rar`, `*.r00`, `*.r01`, ...
///
/// Empty if the NZB has no recognizable RAR volumes.
pub fn find_rar_volumes(nzb: &Nzb) -> Vec<&NzbFile> {
    // Try modern .partNN.rar first.
    let modern: Vec<(u32, &NzbFile)> = nzb
        .files
        .iter()
        .filter_map(|f| f.name().and_then(|n| part_volume_number(n).map(|p| (p, f))))
        .collect();
    if !modern.is_empty() {
        let mut sorted = modern;
        sorted.sort_by_key(|(p, _)| *p);
        return sorted.into_iter().map(|(_, f)| f).collect();
    }

    // Fall back to old-style: bare .rar (volume 1) + .r00..rNN (volumes 2..N+2).
    let mut old: Vec<(u32, &NzbFile)> = Vec::new();
    for f in &nzb.files {
        if f.is_par2() {
            continue;
        }
        let Some(name) = f.name() else { continue };
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".rar") {
            old.push((1, f));
        } else if let Some(n) = old_style_volume_number(name) {
            old.push((n + 1, f)); // .r00 sorts after .rar
        }
    }
    if old.is_empty() {
        return Vec::new();
    }
    old.sort_by_key(|(n, _)| *n);
    old.into_iter().map(|(_, f)| f).collect()
}

/// Parse the RAR headers in `buf`. Tries the password-less path first; if
/// the parser reports encrypted headers, walks the supplied password list
/// and returns the first one that decrypts. If none work, surfaces the
/// EncryptedHeaders error so the caller can report it cleanly.
fn parse_volume_headers(
    buf: &[u8],
    passwords: &[&str],
) -> Result<Vec<RarHeader>, nzbdav_rar::RarError> {
    use std::io::Cursor;
    match parse_all_headers(&mut Cursor::new(buf), None) {
        Ok(headers) => Ok(headers),
        Err(nzbdav_rar::RarError::EncryptedHeaders) => {
            for pw in passwords {
                match parse_all_headers(&mut Cursor::new(buf), Some(pw)) {
                    Ok(headers) => {
                        tracing::info!(
                            "[preflight rar] decrypted headers with NZB-supplied password"
                        );
                        return Ok(headers);
                    }
                    Err(nzbdav_rar::RarError::IncorrectPassword) => continue,
                    Err(other) => return Err(other),
                }
            }
            Err(nzbdav_rar::RarError::EncryptedHeaders)
        }
        Err(other) => Err(other),
    }
}

/// A volume's first segment, decoded once during pre-flight: the raw header
/// bytes (for RAR parsing) plus the decoded geometry `build_rar_layout` needs.
struct VolFirstSegment {
    data: Vec<u8>,
    decoded_stride: u64,
    decoded_file_size: Option<u64>,
}

/// RAR-mode pre-flight. Fetches volume headers in parallel, parses each, and
/// builds a unified `FileLayout` with one `DataChunk` per volume.
async fn probe_rar_inner(
    nzb: &Nzb,
    nntp: &Arc<NntpPool>,
    cache: &DiskCache,
    token: &str,
) -> Result<ActiveStream, PreflightError> {
    let cache_path = cache.root.join(format!("{token}.bin"));
    let volumes = find_rar_volumes(nzb);
    if volumes.is_empty() {
        return Err(PreflightError::NoPlayableFile);
    }

    // 1. Fetch first segment of every volume in parallel via the pool.
    //    The pool's per-server connection limit naturally bounds concurrency
    //    so we don't blast a 50-volume archive's headers as 50 simultaneous
    //    fresh handshakes.
    let fetches = volumes.iter().enumerate().map(|(idx, v)| {
        let nntp = nntp.clone();
        let first_seg = v
            .segments
            .iter()
            .min_by_key(|s| s.number)
            .map(|s| s.message_id.clone());
        async move {
            let msg_id = first_seg.ok_or_else(|| PreflightError::RarHeaderOverflow(idx))?;
            let (_idx, raw) = nntp
                .fetch_with_failover(0, &msg_id)
                .await
                .map_err(|e| PreflightError::NntpFetch(crate::util::redact_log(&e.to_string())))?;
            let decoded =
                decode_yenc(&raw).map_err(|e| PreflightError::Decode(format!("{e:?}")))?;
            if decoded.data.is_empty() {
                return Err(PreflightError::EmptyPayload);
            }
            // Keep the decoded stride + total volume size alongside the header
            // bytes — `build_rar_layout` needs them to space segments in
            // decoded (not encoded) space.
            Ok::<VolFirstSegment, PreflightError>(VolFirstSegment {
                decoded_stride: decoded.data.len() as u64,
                decoded_file_size: decoded.file_size,
                data: decoded.data,
            })
        }
    });
    let vol_first: Vec<VolFirstSegment> = futures::future::try_join_all(fetches).await?;

    // 2. Parse each volume's headers. Volume 0 must yield ≥1 FileHeader.
    //
    // NZB-embedded passwords (from `<head><meta type="password">…`) feed the
    // RAR5 header-encryption decryptor — many indexers ship the password in
    // the NZB metadata for legitimate distribution. We try them in order;
    // first one that decrypts wins. Note: even with the right password, if
    // *file data* is also encrypted (vs just headers), we still can't stream
    // it — the streaming pipeline doesn't decrypt segment payloads.
    let nzb_passwords: Vec<&str> = nzb.meta.passwords.iter().map(|s| s.as_str()).collect();
    if !nzb_passwords.is_empty() {
        tracing::info!(
            "[preflight rar] NZB metadata supplies {} password(s)",
            nzb_passwords.len()
        );
    }

    let mut all_file_headers: Vec<(usize, FileHeader)> = Vec::new();
    let mut header_summary: Vec<String> = Vec::new();
    for (idx, vf) in vol_first.iter().enumerate() {
        let headers = parse_volume_headers(&vf.data, &nzb_passwords).map_err(|e| {
            PreflightError::RarHeaderParse {
                volume: idx,
                reason: format!("{e:?}"),
            }
        })?;

        let mut counts = (0usize, 0usize, 0usize, 0usize); // (file, archive, service, end)
        for h in headers {
            match h {
                RarHeader::File(fh) => {
                    counts.0 += 1;
                    if fh.is_encrypted {
                        return Err(PreflightError::RarEncrypted);
                    }
                    if fh.compression_method != 0 {
                        return Err(PreflightError::RarCompressed(fh.compression_method));
                    }
                    // Real directory entries have `uncompressed_size == 0`. The
                    // `is_directory` flag bit is unreliable in the wild — some
                    // posters' RAR encoders set it on file entries too, which
                    // makes a strict `is_directory` filter drop every entry
                    // (observed empirically on at least one indexer's catalog).
                    // Filtering by size is the durable signal: directories
                    // hold no data, files do.
                    if fh.uncompressed_size == 0 {
                        continue;
                    }
                    all_file_headers.push((idx, fh));
                }
                RarHeader::Archive(_) => counts.1 += 1,
                RarHeader::Service(_) => counts.2 += 1,
                RarHeader::EndArchive(_) => counts.3 += 1,
            }
        }
        header_summary.push(format!(
            "vol{idx}={}f/{}a/{}s/{}e",
            counts.0, counts.1, counts.2, counts.3
        ));
    }

    tracing::info!(
        "[preflight rar] parsed headers across {} volumes: [{}], {} non-dir file entries total",
        volumes.len(),
        header_summary.join(", "),
        all_file_headers.len(),
    );

    // 3. The "main" file is the largest by uncompressed_size — for typical
    //    movie releases there's only one non-directory entry.
    let main = all_file_headers
        .iter()
        .max_by_key(|(_, fh)| fh.uncompressed_size)
        .ok_or(PreflightError::RarNoVideoFile)?
        .1
        .clone();
    let total_video_size = main.uncompressed_size;
    if total_video_size == 0 {
        return Err(PreflightError::RarNoVideoFile);
    }
    let target_filename = main.filename.clone();
    let content_type = guess_content_type(&target_filename);

    // 4. For each volume, find the FileHeader matching `target_filename`
    //    and capture its data_start_position + data_size in that volume.
    //    If a volume is missing the entry → integrity issue.
    let mut per_volume: Vec<(usize, u64, u64)> = Vec::new(); // (vol_idx, data_start, data_size)
    for (idx, _vol) in volumes.iter().enumerate() {
        let entry = all_file_headers
            .iter()
            .find(|(vi, fh)| *vi == idx && fh.filename == target_filename);
        if let Some((_, fh)) = entry {
            per_volume.push((idx, fh.data_start_position, fh.data_size));
        }
        // Volumes that don't carry the file (rare, e.g. recovery records) are
        // skipped — their bytes still appear in the assembled segment stream
        // but contribute no video data. We *could* still need to walk past
        // them. Phase 4 keeps it simple and stops at first miss.
    }
    if per_volume.is_empty() {
        return Err(PreflightError::RarNoVideoFile);
    }

    // Sanity: sum of per-volume data_size should equal total_video_size.
    let summed: u64 = per_volume.iter().map(|(_, _, ds)| *ds).sum();
    if summed != total_video_size {
        tracing::warn!(
            "[preflight rar] data_size sum {} != uncompressed_size {} (filename {target_filename})",
            summed,
            total_video_size,
        );
    }

    // 5. Build segments + chunks in DECODED space. Each volume's segments
    //    cumulate the decoded stride (not the NZB encoded `<segment bytes>`),
    //    so the assembled stream matches the real decoded concatenation and
    //    the decoded-space `data_start`/`data_size` line up — no sparse-zero
    //    tails. See `build_rar_layout`.
    let geom: Vec<RarVolumeGeom> = per_volume
        .iter()
        .map(|(vol_idx, data_start, data_size)| {
            let mut sorted: Vec<&nzb_rs::Segment> = volumes[*vol_idx].segments.iter().collect();
            sorted.sort_by_key(|s| s.number);
            RarVolumeGeom {
                message_ids: sorted.into_iter().map(|s| s.message_id.clone()).collect(),
                decoded_stride: vol_first[*vol_idx].decoded_stride,
                decoded_file_size: vol_first[*vol_idx].decoded_file_size,
                data_start: *data_start,
                data_size: *data_size,
            }
        })
        .collect();
    let (segments, chunks, _video_total) = build_rar_layout(&geom);

    let layout = FileLayout {
        segments: segments.into(),
        chunks,
    };
    tracing::info!(
        "[preflight rar] file=\"{}\" total_size={} chunks={} segments={} sum_data={}",
        target_filename,
        total_video_size,
        layout.chunks.len(),
        layout.segments.len(),
        summed,
    );
    for (i, c) in layout.chunks.iter().enumerate() {
        tracing::debug!(
            "[preflight rar] chunk {i}: video=[{}, {}) len={} assembled={}",
            c.video_start,
            c.video_start + c.length,
            c.length,
            c.assembled_start
        );
    }

    let cache_file = cache
        .open(token, layout.assembled_size())
        .await
        .map_err(|e| PreflightError::Internal(anyhow::anyhow!("cache open: {e}")))?;

    Ok(ActiveStream {
        candidate_idx: 0,
        total_size: total_video_size,
        content_type,
        file_layout: layout,
        cache_path,
        cache_file,
    })
}

async fn fetch_and_parse(http: &reqwest::Client, nzb_url: &str) -> Result<Nzb, PreflightError> {
    let xml = crate::nzb_fetch::fetch_nzb_xml(http, nzb_url)
        .await
        .map_err(|e| match e {
            crate::nzb_fetch::NzbFetchError::HttpStatus(code) => PreflightError::HttpStatus(code),
            crate::nzb_fetch::NzbFetchError::Network(msg) => PreflightError::HttpFetch(msg),
            crate::nzb_fetch::NzbFetchError::TooLarge(n) => PreflightError::NzbTooLarge(n),
            // Throttle treated as a 503 from the caller's perspective —
            // candidate is dropped, preflight tries the next one.
            crate::nzb_fetch::NzbFetchError::IndexerThrottled => PreflightError::HttpStatus(503),
        })?;

    Nzb::parse(xml.as_str())
        .map_err(|e| PreflightError::NzbParse(e.to_string()))
        .context("parsing NZB")
        .map_err(PreflightError::Internal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nzb_xml(files: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
{files}
</nzb>"#
        )
    }

    fn file_xml(subject: &str, segments: &[(u32, u32, &str)]) -> String {
        let segs = segments
            .iter()
            .map(|(bytes, num, msg)| {
                format!(r#"      <segment bytes="{bytes}" number="{num}">{msg}</segment>"#)
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            r#"  <file poster="x" date="1700000000" subject="{subject}">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
{segs}
    </segments>
  </file>"#
        )
    }

    #[test]
    fn pick_main_rejects_rar_releases() {
        let xml = nzb_xml(&format!(
            "{}\n{}",
            file_xml("Show.S01E01.part01.rar yEnc", &[(100, 1, "p01@x")]),
            file_xml("Show.S01E01.part02.rar yEnc", &[(100, 1, "p02@x")])
        ));
        let nzb = Nzb::parse(&xml).unwrap();
        assert_eq!(pick_main_file(&nzb).unwrap(), DetectedLayout::Rar);
    }

    #[test]
    fn pick_main_chooses_video_over_par2() {
        let xml = nzb_xml(&format!(
            "{}\n{}",
            file_xml(
                "Movie.2024.1080p.mkv yEnc",
                &[(1000, 1, "m1@x"), (1000, 2, "m2@x")]
            ),
            file_xml("Movie.par2 yEnc", &[(50, 1, "par@x")])
        ));
        let nzb = Nzb::parse(&xml).unwrap();
        match pick_main_file(&nzb).unwrap() {
            DetectedLayout::Flat(f) => assert!(f.name().unwrap().ends_with(".mkv")),
            DetectedLayout::Rar => panic!("expected Flat"),
        }
    }

    #[test]
    fn pick_main_picks_largest_video_when_multiple() {
        let xml = nzb_xml(&format!(
            "{}\n{}",
            file_xml("sample.mkv yEnc", &[(100, 1, "s1@x")]),
            file_xml("Movie.mkv yEnc", &[(1000, 1, "m1@x"), (1000, 2, "m2@x")])
        ));
        let nzb = Nzb::parse(&xml).unwrap();
        match pick_main_file(&nzb).unwrap() {
            DetectedLayout::Flat(f) => assert_eq!(f.name().unwrap(), "Movie.mkv"),
            DetectedLayout::Rar => panic!("expected Flat"),
        }
    }

    #[test]
    fn pick_main_falls_back_to_largest_for_obfuscated_names() {
        // No recognized video extension; should fall back to largest non-par2.
        // Obfuscated releases on EasyNews/etc. commonly use random .bin or .dat
        // names. The inner quotes around the filename use &quot; — they're how
        // real-world subjects are encoded.
        let xml = nzb_xml(&format!(
            "{}\n{}",
            file_xml("&quot;small.bin&quot; yEnc", &[(50, 1, "a@x")]),
            file_xml(
                "&quot;large.bin&quot; yEnc",
                &[(500, 1, "x1@x"), (500, 2, "x2@x")]
            )
        ));
        let nzb = Nzb::parse(&xml).unwrap();
        match pick_main_file(&nzb).unwrap() {
            DetectedLayout::Flat(f) => assert_eq!(f.name(), Some("large.bin")),
            DetectedLayout::Rar => panic!("expected Flat"),
        }
    }

    #[test]
    fn build_segment_refs_cumulative_offsets() {
        let xml = nzb_xml(&file_xml(
            "Movie.mkv yEnc",
            &[(100, 2, "s2@x"), (50, 3, "s3@x"), (200, 1, "s1@x")],
        ));
        let nzb = Nzb::parse(&xml).unwrap();
        let DetectedLayout::Flat(f) = pick_main_file(&nzb).unwrap() else {
            panic!("expected Flat");
        };
        let refs = build_segment_refs(f, 0);
        assert_eq!(refs.len(), 3);
        // Sorted by segment number ascending — s1 first.
        assert_eq!(refs[0].message_id, "s1@x");
        assert_eq!(refs[0].offset_in_stream, 0);
        assert_eq!(refs[0].bytes, 200);
        assert_eq!(refs[1].message_id, "s2@x");
        assert_eq!(refs[1].offset_in_stream, 200);
        assert_eq!(refs[1].bytes, 100);
        assert_eq!(refs[2].message_id, "s3@x");
        assert_eq!(refs[2].offset_in_stream, 300);
        assert_eq!(refs[2].bytes, 50);
    }

    #[test]
    fn build_segment_refs_empty_for_no_segments() {
        let xml = nzb_xml(&file_xml("Movie.mkv yEnc", &[]));
        // nzb-rs requires at least one segment per file, so parse should fail.
        // This documents the expected behavior.
        assert!(Nzb::parse(&xml).is_err());
    }

    #[test]
    fn part_volume_number_extracts() {
        assert_eq!(part_volume_number("Show.part01.rar"), Some(1));
        assert_eq!(part_volume_number("Show.part015.rar yEnc"), Some(15));
        assert_eq!(part_volume_number("Show.PART07.rar"), Some(7));
        assert_eq!(part_volume_number("Show.r00"), None);
        assert_eq!(part_volume_number("Show.rar"), None);
    }

    #[test]
    fn old_style_volume_number_extracts() {
        assert_eq!(old_style_volume_number("Show.r00"), Some(1));
        assert_eq!(old_style_volume_number("Show.r01"), Some(2));
        assert_eq!(old_style_volume_number("Show.r99"), Some(100));
        assert_eq!(old_style_volume_number("Show.r000"), Some(1));
        assert_eq!(old_style_volume_number("Show.part01.rar"), None);
        assert_eq!(old_style_volume_number("Show.rar"), None);
    }

    #[test]
    fn find_rar_volumes_modern_partnn_sorted() {
        let xml = nzb_xml(&format!(
            "{}\n{}\n{}",
            file_xml("Show.part03.rar yEnc", &[(100, 1, "p3@x")]),
            file_xml("Show.part01.rar yEnc", &[(100, 1, "p1@x")]),
            file_xml("Show.part02.rar yEnc", &[(100, 1, "p2@x")])
        ));
        let nzb = Nzb::parse(&xml).unwrap();
        let vols = find_rar_volumes(&nzb);
        assert_eq!(vols.len(), 3);
        assert_eq!(vols[0].name(), Some("Show.part01.rar"));
        assert_eq!(vols[1].name(), Some("Show.part02.rar"));
        assert_eq!(vols[2].name(), Some("Show.part03.rar"));
    }

    #[test]
    fn find_rar_volumes_old_style_rar_then_rnn() {
        let xml = nzb_xml(&format!(
            "{}\n{}\n{}\n{}",
            file_xml("Show.r01 yEnc", &[(100, 1, "r1@x")]),
            file_xml("Show.rar yEnc", &[(100, 1, "rar@x")]),
            file_xml("Show.r00 yEnc", &[(100, 1, "r0@x")]),
            file_xml("Show.par2 yEnc", &[(50, 1, "par@x")])
        ));
        let nzb = Nzb::parse(&xml).unwrap();
        let vols = find_rar_volumes(&nzb);
        assert_eq!(vols.len(), 3);
        assert_eq!(vols[0].name(), Some("Show.rar"));
        assert_eq!(vols[1].name(), Some("Show.r00"));
        assert_eq!(vols[2].name(), Some("Show.r01"));
    }

    #[test]
    fn find_rar_volumes_prefers_modern_when_both_present() {
        // Defensive: if a malformed NZB has both styles, prefer modern.
        let xml = nzb_xml(&format!(
            "{}\n{}",
            file_xml("Show.part01.rar yEnc", &[(100, 1, "p1@x")]),
            file_xml("Show.r00 yEnc", &[(100, 1, "r0@x")])
        ));
        let nzb = Nzb::parse(&xml).unwrap();
        let vols = find_rar_volumes(&nzb);
        assert_eq!(vols.len(), 1);
        assert_eq!(vols[0].name(), Some("Show.part01.rar"));
    }

    #[test]
    fn find_rar_volumes_empty_when_none() {
        let xml = nzb_xml(&file_xml("Movie.mkv yEnc", &[(100, 1, "m@x")]));
        let nzb = Nzb::parse(&xml).unwrap();
        let vols = find_rar_volumes(&nzb);
        assert!(vols.is_empty());
    }

    fn ref_at(message_id: &str, bytes: u64, offset: u64) -> SegmentRef {
        SegmentRef {
            server_index: 0,
            message_id: message_id.to_string(),
            bytes,
            offset_in_stream: offset,
        }
    }

    #[test]
    fn rebuild_noop_when_indexer_already_reports_decoded_size() {
        // Indexer's <segment bytes> matches the actual decoded payload —
        // no rebuild needed.
        let mut segs = vec![
            ref_at("s1", 716_800, 0),
            ref_at("s2", 716_800, 716_800),
            ref_at("s3", 200_000, 1_433_600),
        ];
        let total = rebuild_for_decoded_stride(&mut segs, 716_800, Some(1_633_600));
        assert_eq!(total, 1_633_600, "sum of declared bytes preserved");
        assert_eq!(segs[0].offset_in_stream, 0);
        assert_eq!(segs[1].offset_in_stream, 716_800);
        assert_eq!(segs[2].offset_in_stream, 1_433_600);
        assert_eq!(segs[2].bytes, 200_000, "tail untouched");
    }

    #[test]
    fn rebuild_relays_yenc_total_for_tail_size() {
        // Indexer reports yEnc-encoded size 739_600 per segment, but
        // decoded is 716_800. yEnc =ybegin reported total file size
        // 1_900_000 (= 716_800 + 716_800 + 466_400 tail).
        let mut segs = vec![
            ref_at("s1", 739_600, 0),
            ref_at("s2", 739_500, 739_600),
            ref_at("s3", 480_000, 1_479_100),
        ];
        let total = rebuild_for_decoded_stride(&mut segs, 716_800, Some(1_900_000));
        assert_eq!(total, 1_900_000);
        assert_eq!(segs[0].offset_in_stream, 0);
        assert_eq!(segs[0].bytes, 716_800);
        assert_eq!(segs[1].offset_in_stream, 716_800);
        assert_eq!(segs[1].bytes, 716_800);
        assert_eq!(segs[2].offset_in_stream, 1_433_600);
        assert_eq!(
            segs[2].bytes, 466_400,
            "tail = total - (n-1)*stride = 1_900_000 - 1_433_600"
        );
    }

    #[test]
    fn rebuild_falls_back_to_full_stride_when_total_unknown() {
        // No yEnc total → assume last segment also full stride.
        // Slight overestimate, but acceptable for posters that omit the
        // header (very rare).
        let mut segs = vec![
            ref_at("s1", 739_600, 0),
            ref_at("s2", 739_500, 739_600),
            ref_at("s3", 720_000, 1_479_100),
        ];
        let total = rebuild_for_decoded_stride(&mut segs, 716_800, None);
        assert_eq!(total, 3 * 716_800);
        for (i, seg) in segs.iter().enumerate() {
            assert_eq!(seg.offset_in_stream, i as u64 * 716_800);
            assert_eq!(seg.bytes, 716_800);
        }
    }

    #[test]
    fn rebuild_handles_implausible_total_via_fallback() {
        // total < (n-1)*stride is nonsense — decode header was probably
        // garbled. Fall back to assuming uniform stride for everything.
        let mut segs = vec![
            ref_at("s1", 739_600, 0),
            ref_at("s2", 739_500, 739_600),
            ref_at("s3", 480_000, 1_479_100),
        ];
        let total = rebuild_for_decoded_stride(&mut segs, 716_800, Some(100));
        assert_eq!(total, 3 * 716_800);
        assert_eq!(segs[2].bytes, 716_800);
    }

    #[test]
    fn rebuild_empty_input_is_noop() {
        let mut segs: Vec<SegmentRef> = Vec::new();
        let total = rebuild_for_decoded_stride(&mut segs, 716_800, Some(1_900_000));
        assert_eq!(total, 0);
    }

    #[test]
    fn rebuild_single_segment_uses_total_as_size() {
        // One-segment file: bytes should be the actual file size, not the stride.
        let mut segs = vec![ref_at("s1", 739_600, 0)];
        let total = rebuild_for_decoded_stride(&mut segs, 716_800, Some(500_000));
        assert_eq!(total, 500_000);
        assert_eq!(segs[0].offset_in_stream, 0);
        // Last segment carries the tail; min(stride, tail) = 500_000.
        assert_eq!(segs[0].bytes, 500_000);
    }

    fn geom(
        ids: &[&str],
        stride: u64,
        file_size: Option<u64>,
        data_start: u64,
        data_size: u64,
    ) -> RarVolumeGeom {
        RarVolumeGeom {
            message_ids: ids.iter().map(|s| s.to_string()).collect(),
            decoded_stride: stride,
            decoded_file_size: file_size,
            data_start,
            data_size,
        }
    }

    #[test]
    fn rar_layout_spaces_segments_by_decoded_stride() {
        // 3 parts, decoded stride 968 (the encoded <segment bytes> ~1000 must
        // NOT be used). Volume total 2840 → last part = 2840 - 2*968 = 904.
        let (segs, chunks, total) =
            build_rar_layout(&[geom(&["s0", "s1", "s2"], 968, Some(2840), 50, 2790)]);
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0].offset_in_stream, 0);
        assert_eq!(segs[0].bytes, 968);
        assert_eq!(
            segs[1].offset_in_stream, 968,
            "second part must start at the decoded stride, not the encoded size"
        );
        assert_eq!(segs[1].bytes, 968);
        assert_eq!(segs[2].offset_in_stream, 1936);
        assert_eq!(segs[2].bytes, 904, "last part = file_size - (n-1)*stride");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].assembled_start, 50, "data region at decoded data_start");
        assert_eq!(chunks[0].length, 2790);
        assert_eq!(chunks[0].video_start, 0);
        assert_eq!(total, 2790);
    }

    #[test]
    fn rar_layout_multivolume_cumulates_decoded_offsets() {
        // Two volumes, 2 parts each, stride 1000, total 1800 (last part 800).
        let (segs, chunks, total) = build_rar_layout(&[
            geom(&["a0", "a1"], 1000, Some(1800), 100, 1700),
            geom(&["b0", "b1"], 1000, Some(1800), 100, 1700),
        ]);
        // vol0: [0,1000) [1000,1800)   vol1: [1800,2800) [2800,3600)
        assert_eq!(segs[0].offset_in_stream, 0);
        assert_eq!(segs[1].offset_in_stream, 1000);
        assert_eq!(segs[1].bytes, 800);
        assert_eq!(
            segs[2].offset_in_stream, 1800,
            "vol1 starts at vol0's decoded total, not its encoded total"
        );
        assert_eq!(segs[2].bytes, 1000);
        assert_eq!(segs[3].offset_in_stream, 2800);
        assert_eq!(segs[3].bytes, 800);
        assert_eq!(chunks[0].assembled_start, 100);
        assert_eq!(chunks[0].video_start, 0);
        assert_eq!(chunks[1].assembled_start, 1900);
        assert_eq!(chunks[1].video_start, 1700);
        assert_eq!(total, 3400);
    }

    #[test]
    fn rar_layout_falls_back_to_full_stride_without_file_size() {
        let (segs, _c, _t) = build_rar_layout(&[geom(&["s0", "s1", "s2"], 1000, None, 0, 2800)]);
        assert_eq!(segs[2].bytes, 1000, "no =ybegin size= → assume full last stride");
        assert_eq!(segs[2].offset_in_stream, 2000);
    }

    #[test]
    fn rar_layout_implausible_file_size_falls_back() {
        // file_size < (n-1)*stride is nonsense → fall back to n*stride.
        let (segs, _c, _t) = build_rar_layout(&[geom(&["s0", "s1", "s2"], 1000, Some(500), 0, 100)]);
        assert_eq!(segs[2].bytes, 1000);
    }

    #[test]
    fn rar_layout_single_part_volume_sized_to_total() {
        let (segs, chunks, total) = build_rar_layout(&[geom(&["only"], 1000, Some(640), 50, 590)]);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].offset_in_stream, 0);
        assert_eq!(segs[0].bytes, 640);
        assert_eq!(chunks[0].assembled_start, 50);
        assert_eq!(total, 590);
    }

    /// Path to the committed real-world fixture: the exact 45-volume RAR
    /// release whose preflight 500'd in prod (nzbplanet guid
    /// 6c1e7bbb…, 3304 segments). Captured via `t=get` so re-runs don't
    /// depend on the indexer.
    const RAR_FIXTURE_PATH: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/rar_multivolume.nzb");

    fn parse_rar_fixture() -> Nzb {
        let xml = std::fs::read_to_string(RAR_FIXTURE_PATH).expect("read RAR fixture");
        Nzb::parse(&xml).expect("parse RAR fixture")
    }

    /// The fixture is detected as RAR and fans out to all 45 volumes — this
    /// is the structural root of the slow-preflight bug: `probe_rar_inner`
    /// fetches the first segment of every one of these before it can return
    /// a serveable stream. Deterministic, no network.
    #[test]
    fn fixture_rar_release_fans_out_to_45_volumes() {
        let nzb = parse_rar_fixture();
        assert_eq!(pick_main_file(&nzb).unwrap(), DetectedLayout::Rar);
        let vols = find_rar_volumes(&nzb);
        assert_eq!(vols.len(), 45, "real release has 45 RAR volumes");
        assert!(vols[0].name().unwrap().contains("part01.rar"));
        assert!(vols[44].name().unwrap().contains("part45.rar"));
    }

    /// Live end-to-end repro of BOTH streaming bugs against the real release.
    /// Gated on `CONFIG_PATH` (the addon's own config with real NNTP creds)
    /// and `#[ignore]` so it never runs in CI. Run with:
    ///   CONFIG_PATH=/path/to/config.toml cargo test --bin stremio-nzb-addon \
    ///     -- --ignored --nocapture live_rar_preflight
    ///
    /// Bug #1 (slow TTFB): `probe_rar_inner` fetches the first segment of all
    /// 45 volumes before returning, blocking the HTTP response long enough
    /// that the transcoder's ffprobe times out → HTTP 500 → stream won't load.
    ///
    /// Bug #2 (decoded-short): RAR segment offsets are built from the NZB
    /// `<segment bytes>` (yEnc-encoded, ~3% larger than decoded), so real
    /// segments decode shorter than their declared layout size and leave a
    /// sparse-zero tail that corrupts playback mid-stream.
    #[tokio::test]
    #[ignore = "live: needs CONFIG_PATH + real NNTP creds + network"]
    async fn live_rar_preflight_exposes_slow_ttfb_and_decoded_short() {
        let Ok(config_path) = std::env::var("CONFIG_PATH") else {
            eprintln!("SKIP: CONFIG_PATH unset");
            return;
        };
        let cfg = crate::config::load_from_disk(std::path::Path::new(&config_path))
            .expect("load config")
            .expect("config file present");
        let urls: Vec<String> = cfg
            .defaults
            .nntp_servers
            .iter()
            .map(|s| s.server.clone())
            .collect();
        assert!(!urls.is_empty(), "config must define NNTP servers");
        let nntp =
            std::sync::Arc::new(crate::streaming::nntp::NntpPool::from_urls(urls).unwrap());

        let tmp = std::env::temp_dir().join(format!("tab-live-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let cache =
            crate::streaming::disk_cache::DiskCache::new(tmp.clone(), 4 * 1024 * 1024 * 1024);

        let nzb = parse_rar_fixture();
        let vol_count = find_rar_volumes(&nzb).len();
        eprintln!("[live] fixture has {vol_count} RAR volumes");

        // ---- Bug #1: time-to-first-byte for the cold preflight.
        let t0 = std::time::Instant::now();
        let active = probe_rar_inner(&nzb, &nntp, &cache, "live-test-token")
            .await
            .expect("preflight should succeed for a live release");
        let ttfb = t0.elapsed();
        eprintln!("[live] RAR preflight TTFB: {ttfb:?} (fetched first segment of {vol_count} volumes)");

        // ---- Bug #2: sample real segments; each should decode to exactly its
        // declared layout size. Today they decode short (encoded > decoded).
        let mut short = 0usize;
        let sample = active.file_layout.segments.iter().take(8).cloned().collect::<Vec<_>>();
        for seg in &sample {
            let (_idx, raw) = nntp
                .fetch_with_failover(seg.server_index, &seg.message_id)
                .await
                .expect("fetch segment");
            let decoded = decode_yenc(&raw).expect("decode segment");
            eprintln!(
                "[live] seg {} decoded {} vs declared {}",
                seg.message_id,
                decoded.data.len(),
                seg.bytes
            );
            if (decoded.data.len() as u64) < seg.bytes {
                short += 1;
            }
        }
        std::fs::remove_dir_all(&tmp).ok();

        assert_eq!(
            short, 0,
            "{short}/{} sampled RAR segments decode shorter than their declared layout size \
             → sparse-zero tails corrupt the stream (probe_rar_inner skips decoded-stride \
             reconciliation)",
            sample.len()
        );
        assert!(
            ttfb < std::time::Duration::from_secs(4),
            "RAR preflight TTFB {ttfb:?} exceeds 4s — blocks the HTTP response so ffprobe \
             times out and the stream 500s; lazy per-volume header parsing should fix this"
        );
    }
}
