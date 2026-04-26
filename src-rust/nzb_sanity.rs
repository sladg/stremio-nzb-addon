use once_cell::sync::Lazy;
use regex::Regex;
use sha1::{Digest, Sha1};

use crate::cache::SANITY_CACHE;
use crate::nzb_api::{item_nzb_url, Item};
use crate::nzb_fetch::{fetch_nzb_xml, NzbFetchError};

/// RAR-looking filename detection. Wider than the original to cover patterns
/// seen across indexers and noted in the Usenet-Ultimate reference:
///   - .part01.rar, .part001.rar
///   - .rar, .r00..r999 (old-style multi-volume)
///   - .s00..s999, .t00..t999, .u00..u999, .v00..v999 (rare extended sets)
///   - .7z.001, .zip.001, generic .NNN chained endings
static RAR_ANY_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\.(?:part\d+\.rar|rar|r\d{2,3}|s\d{2,3}|t\d{2,3}|u\d{2,3}|v\d{2,3}|7z\.\d{3}|zip\.\d{3}|\d{3})(?:[^.\w]|$)"
    )
    .expect("RAR regex")
});

/// Direct video filename detection — Flat-mode releases post one big video
/// file directly (no RAR wrapping), which the streaming pipeline supports
/// natively. Matches the same container types `guess_content_type` recognizes
/// in `streaming::session`. Subject lookahead requires non-word/non-dot
/// trailing context so e.g. ".mkvtoolnix" (a project name in a description)
/// doesn't false-match.
static VIDEO_ANY_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\.(?:mkv|mp4|m4v|avi|webm|mov|ts|wmv)(?:[^.\w]|$)")
        .expect("video regex")
});

static SUBJECT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?i)subject="([^"]+)""#).expect("subject regex"));

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SanityResult {
    pub ok: bool,
    pub reason: Option<String>,
}

fn sha1_short(s: &str) -> String {
    let digest = Sha1::digest(s.as_bytes());
    hex::encode(digest)[..16].to_string()
}

pub async fn check_nzb_sanity(client: &reqwest::Client, nzb_url: &str) -> SanityResult {
    let cache_key = format!("nzb-sanity:{}", sha1_short(nzb_url));
    if let Some(hit) = SANITY_CACHE.get(&cache_key).await {
        return hit;
    }
    let result = probe_nzb(client, nzb_url).await;
    SANITY_CACHE.insert(cache_key, result.clone()).await;
    result
}

async fn probe_nzb(client: &reqwest::Client, nzb_url: &str) -> SanityResult {
    let xml = match fetch_nzb_xml(client, nzb_url).await {
        Ok(x) => x,
        Err(NzbFetchError::HttpStatus(code)) => {
            return SanityResult {
                ok: false,
                reason: Some(format!("http-{code}")),
            };
        }
        Err(NzbFetchError::Network(msg)) => {
            return SanityResult {
                ok: false,
                reason: Some(msg),
            };
        }
        Err(NzbFetchError::TooLarge(n)) => {
            return SanityResult {
                ok: false,
                reason: Some(format!("nzb-too-large-{n}")),
            };
        }
        Err(NzbFetchError::IndexerThrottled) => {
            return SanityResult {
                ok: false,
                reason: Some("indexer-throttled".to_string()),
            };
        }
    };

    let subjects: Vec<&str> = SUBJECT_RE
        .captures_iter(xml.as_str())
        .filter_map(|c| c.get(1).map(|m| m.as_str()))
        .collect();

    if subjects.is_empty() {
        return SanityResult {
            ok: false,
            reason: Some("no-subjects".to_string()),
        };
    }

    // Accept either shape:
    //   - RAR-wrapped (multi-volume archive of the video, parsed at preflight)
    //   - Flat (single video file posted directly, streamed as-is)
    // Anything that's neither is a non-video upload (par2-only, image set,
    // non-supported container) and gets dropped here so the player never
    // sees a stream that won't play.
    let has_rar = subjects.iter().any(|s| RAR_ANY_RE.is_match(s));
    let has_video = subjects.iter().any(|s| VIDEO_ANY_RE.is_match(s));
    if !has_rar && !has_video {
        return SanityResult {
            ok: false,
            reason: Some("no-video-or-rar".to_string()),
        };
    }

    SanityResult {
        ok: true,
        reason: None,
    }
}

pub async fn filter_by_nzb_sanity(client: &reqwest::Client, items: Vec<Item>) -> Vec<Item> {
    if items.is_empty() {
        return items;
    }

    let total = items.len();
    let checks = items.into_iter().map(|item| {
        let client = client.clone();
        async move {
            let url = item_nzb_url(&item);
            let res = check_nzb_sanity(&client, &url).await;
            (item, res)
        }
    });
    let results: Vec<(Item, SanityResult)> = futures::future::join_all(checks).await;

    let dropped: Vec<&(Item, SanityResult)> = results.iter().filter(|(_, s)| !s.ok).collect();

    if !dropped.is_empty() {
        let preview: Vec<String> = dropped
            .iter()
            .take(20)
            .map(|(it, s)| format!("\"{}\" ({})", it.title, s.reason.as_deref().unwrap_or("?")))
            .collect();
        let suffix = if dropped.len() > 3 { "..." } else { "" };
        tracing::info!(
            "[nzbSanity] excluded {} of {}: {}{}",
            dropped.len(),
            total,
            preview.join(", "),
            suffix
        );
    }

    results
        .into_iter()
        .filter(|(_, s)| s.ok)
        .map(|(it, _)| it)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(s: &str) -> bool {
        RAR_ANY_RE.is_match(s)
    }

    #[test]
    fn rar_regex_matches_classic_patterns() {
        // bare rar
        assert!(matches("Movie.2024.1080p.rar yEnc"));
        // partNN.rar
        assert!(matches("Show.S01E01.part01.rar (1/10)"));
        assert!(matches("file.part001.rar end"));
        // old-style multi-volume
        assert!(matches("foo.r00 yEnc"));
        assert!(matches("foo.r99 yEnc"));
        // extended sets
        assert!(matches("foo.s01 yEnc"));
        assert!(matches("foo.t99 yEnc"));
        // chained archive numbering
        assert!(matches("data.7z.001 yEnc"));
        assert!(matches("data.zip.001 yEnc"));
        assert!(matches("payload.001 yEnc"));
    }

    #[test]
    fn rar_regex_rejects_non_rar() {
        assert!(!matches("Movie.2024.mkv yEnc"));
        assert!(!matches("Movie.mp4 yEnc"));
        assert!(!matches("readme.txt yEnc"));
        assert!(!matches("Movie.rarsomething yEnc"));
    }

    #[test]
    fn subject_regex_finds_quoted_subjects() {
        let xml = r#"
            <nzb>
              <file subject="Movie.S01E01.part01.rar yEnc (1/10)"></file>
              <file subject="Movie.S01E01.part02.rar yEnc (1/10)"></file>
              <file subject="Movie.par2 yEnc"></file>
            </nzb>"#;
        let subjects: Vec<&str> = SUBJECT_RE
            .captures_iter(xml)
            .filter_map(|c| c.get(1).map(|m| m.as_str()))
            .collect();
        assert_eq!(subjects.len(), 3);
        assert!(subjects.iter().any(|s| s.contains("part01.rar")));
    }

    fn video_matches(s: &str) -> bool {
        VIDEO_ANY_RE.is_match(s)
    }

    #[test]
    fn video_regex_matches_supported_containers() {
        assert!(video_matches("Movie.2024.1080p.WEB-DL.mkv yEnc"));
        assert!(video_matches("Show.S01E01.mp4 yEnc (1/200)"));
        assert!(video_matches("classic.avi yEnc"));
        assert!(video_matches("clip.webm yEnc"));
        assert!(video_matches("trailer.m4v end"));
        assert!(video_matches("ancient.mov payload"));
        assert!(video_matches("transport.ts (1/3)"));
    }

    #[test]
    fn video_regex_rejects_non_video() {
        assert!(!video_matches("Movie.par2 yEnc"));
        assert!(!video_matches("Show.nzb yEnc"));
        assert!(!video_matches("readme.txt yEnc"));
        assert!(!video_matches("payload.rar yEnc"));
        // Subject containing a project/tool name that *contains* a video
        // extension as a substring should not false-match.
        assert!(!video_matches("MKVToolnix.guide yEnc"));
        assert!(!video_matches("how.to.use.mkvmerge.tutorial yEnc"));
    }

    /// Run the sanity verdict against XML directly — bypasses the HTTP
    /// fetcher so we can exercise the structure-validation logic without
    /// a live indexer. Mirrors the parsing in `probe_nzb`.
    fn verdict_for_xml(xml: &str) -> &'static str {
        let subjects: Vec<&str> = SUBJECT_RE
            .captures_iter(xml)
            .filter_map(|c| c.get(1).map(|m| m.as_str()))
            .collect();
        if subjects.is_empty() {
            return "no-subjects";
        }
        let has_rar = subjects.iter().any(|s| RAR_ANY_RE.is_match(s));
        let has_video = subjects.iter().any(|s| VIDEO_ANY_RE.is_match(s));
        if !has_rar && !has_video {
            return "no-video-or-rar";
        }
        "ok"
    }

    #[test]
    fn flat_release_with_mkv_passes_sanity() {
        // Modern WEB-DL: one big mkv posted directly, no RAR.
        let xml = r#"
            <nzb>
              <file subject="Mission.Impossible.2025.1080p.AMZN.WEB-DL.DDP.5.1.H.264-PiRaTeS.mkv yEnc (1/4521)"></file>
              <file subject="Mission.Impossible.2025.1080p.AMZN.WEB-DL.DDP.5.1.H.264-PiRaTeS.par2 yEnc"></file>
              <file subject="Mission.Impossible.2025.1080p.AMZN.WEB-DL.DDP.5.1.H.264-PiRaTeS.vol00+01.par2 yEnc"></file>
            </nzb>"#;
        assert_eq!(verdict_for_xml(xml), "ok", "Flat MKV release must pass");
    }

    #[test]
    fn rar_release_still_passes() {
        // Classic RAR-wrapped scene release.
        let xml = r#"
            <nzb>
              <file subject="release.part01.rar yEnc (1/100)"></file>
              <file subject="release.part02.rar yEnc (1/100)"></file>
              <file subject="release.par2 yEnc"></file>
            </nzb>"#;
        assert_eq!(verdict_for_xml(xml), "ok", "RAR release must still pass");
    }

    #[test]
    fn par2_only_upload_is_rejected() {
        // Pathological: only par2 files, no video and no RAR. Player would
        // get nothing; correct to drop at sanity.
        let xml = r#"
            <nzb>
              <file subject="release.par2 yEnc"></file>
              <file subject="release.vol00+01.par2 yEnc"></file>
              <file subject="release.vol01+02.par2 yEnc"></file>
            </nzb>"#;
        assert_eq!(verdict_for_xml(xml), "no-video-or-rar");
    }

    #[test]
    fn empty_subjects_rejected() {
        let xml = "<nzb></nzb>";
        assert_eq!(verdict_for_xml(xml), "no-subjects");
    }
}
