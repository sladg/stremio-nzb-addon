use once_cell::sync::Lazy;
use regex::Regex;
use sha1::{Digest, Sha1};
use std::time::Duration;

use crate::cache::SANITY_CACHE;
use crate::nzb_api::{item_nzb_url, Item};

const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_NZB_SIZE: u64 = 5 * 1024 * 1024;

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
    let resp = match client.get(nzb_url).timeout(FETCH_TIMEOUT).send().await {
        Ok(r) => r,
        Err(err) => {
            return SanityResult {
                ok: false,
                reason: Some(crate::util::redact_log(&err.to_string())),
            };
        }
    };

    if !resp.status().is_success() {
        return SanityResult {
            ok: false,
            reason: Some(format!("http-{}", resp.status().as_u16())),
        };
    }

    if let Some(cl) = resp.content_length() {
        if cl > MAX_NZB_SIZE {
            return SanityResult {
                ok: false,
                reason: Some(format!("nzb-too-large-{cl}")),
            };
        }
    }

    let xml = match resp.text().await {
        Ok(t) => t,
        Err(err) => {
            return SanityResult {
                ok: false,
                reason: Some(crate::util::redact_log(&err.to_string())),
            };
        }
    };

    if (xml.len() as u64) > MAX_NZB_SIZE {
        return SanityResult {
            ok: false,
            reason: Some(format!("nzb-too-large-{}", xml.len())),
        };
    }

    let subjects: Vec<&str> = SUBJECT_RE
        .captures_iter(&xml)
        .filter_map(|c| c.get(1).map(|m| m.as_str()))
        .collect();

    if subjects.is_empty() {
        return SanityResult {
            ok: false,
            reason: Some("no-subjects".to_string()),
        };
    }

    let has_rar = subjects.iter().any(|s| RAR_ANY_RE.is_match(s));
    if !has_rar {
        return SanityResult {
            ok: false,
            reason: Some("no-rar-files".to_string()),
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
}
