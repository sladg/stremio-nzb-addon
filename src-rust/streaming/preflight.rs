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
use std::time::Duration;
use thiserror::Error;

use crate::streaming::disk_cache::DiskCache;
use crate::streaming::nntp::NntpPool;
use crate::streaming::session::{
    guess_content_type, ActiveStream, DataChunk, FileLayout, SegmentRef,
};
use std::sync::Arc;

const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_NZB_SIZE: u64 = 5 * 1024 * 1024;

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

    let segments = build_segment_refs(main, 0);
    if segments.is_empty() {
        return Err(PreflightError::NoPlayableFile);
    }

    let total_size: u64 = segments.iter().map(|s| s.bytes).sum();

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
            Ok::<Vec<u8>, PreflightError>(decoded.data)
        }
    });
    let header_buffers: Vec<Vec<u8>> = futures::future::try_join_all(fetches).await?;

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
    for (idx, buf) in header_buffers.iter().enumerate() {
        let headers = parse_volume_headers(buf, &nzb_passwords).map_err(|e| {
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

    // 5. Build segments + chunks. Segments span all volumes in order;
    //    chunks describe where each volume's data lives in the assembled stream.
    let mut segments: Vec<SegmentRef> = Vec::new();
    let mut chunks: Vec<DataChunk> = Vec::new();
    let mut assembled_offset: u64 = 0;
    let mut video_offset: u64 = 0;
    for (vol_idx, data_start, data_size) in &per_volume {
        let vol = volumes[*vol_idx];

        // Append this volume's segments with cumulative assembled offsets.
        let mut sorted: Vec<&nzb_rs::Segment> = vol.segments.iter().collect();
        sorted.sort_by_key(|s| s.number);
        let vol_segment_offset_start = assembled_offset;
        for seg in sorted {
            let bytes = seg.size as u64;
            segments.push(SegmentRef {
                server_index: 0,
                message_id: seg.message_id.clone(),
                bytes,
                offset_in_stream: assembled_offset,
            });
            assembled_offset += bytes;
        }

        chunks.push(DataChunk {
            video_start: video_offset,
            length: *data_size,
            assembled_start: vol_segment_offset_start + data_start,
        });
        video_offset += *data_size;
    }

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
    let resp = http
        .get(nzb_url)
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|e| PreflightError::HttpFetch(crate::util::redact_log(&e.to_string())))?;

    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        return Err(PreflightError::HttpStatus(status));
    }

    if let Some(cl) = resp.content_length() {
        if cl > MAX_NZB_SIZE {
            return Err(PreflightError::NzbTooLarge(cl));
        }
    }

    let xml = resp
        .text()
        .await
        .map_err(|e| PreflightError::HttpFetch(crate::util::redact_log(&e.to_string())))?;
    if (xml.len() as u64) > MAX_NZB_SIZE {
        return Err(PreflightError::NzbTooLarge(xml.len() as u64));
    }

    Nzb::parse(&xml)
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
}
