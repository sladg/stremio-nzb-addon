//! NNTP `BODY`-probe of canary message-IDs to verify an NZB is actually
//! playable on the user's configured backbones.
//!
//! Mirrors `src/nzbAvailability.ts`. Three canary articles are probed per NZB:
//!   1. First segment of `part01.rar`
//!   2. Last segment of `part01.rar`  ← critical (Stremio's `getFileSize` reads it)
//!   3. Last segment of the highest `partNN.rar`
//!
//! Coverage is **strict AND across servers** — every server must have every
//! canary article, matching Stremio's NZB engine's real fall-back behavior.

use nzb_rs::{File, Nzb};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::time::Duration;

use crate::cache::AVAILABILITY_CACHE;
use crate::healthcheck::body_probe;
use crate::nzb_api::{item_nzb_url, Item};

const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_NZB_SIZE: u64 = 5 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailabilityResult {
    pub ok: bool,
    pub reason: Option<String>,
}

fn sha1_short(s: &str) -> String {
    let digest = Sha1::digest(s.as_bytes());
    hex::encode(digest)[..16].to_string()
}

/// `partNN.rar` (case-insensitive) — captures `NN` for finding "highest part".
static PART_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\.part(\d+)\.rar(?:[^.\w]|$)").expect("part regex"));

fn part_number(file_name: &str) -> Option<u32> {
    PART_RE
        .captures(file_name)
        .and_then(|c| c.get(1)?.as_str().parse::<u32>().ok())
}

/// Pick (first_part01_seg, last_part01_seg, last_partNN_seg) message-IDs.
/// Returns `None` if the NZB has no `partNN.rar` files at all.
///
/// **Sort by `seg.number` before picking first/last** so the canary set
/// matches what `streaming::preflight` actually fetches. NZB XML *usually*
/// lists segments in number order but the spec doesn't require it; if the
/// two paths disagree on which article is "first", availability could
/// pass on one segment while pre-flight dies on a different one.
fn pick_canaries(nzb: &Nzb) -> Option<Vec<&str>> {
    let parts: Vec<(u32, &File)> = nzb
        .files
        .iter()
        .filter_map(|f| f.name().and_then(|n| part_number(n).map(|p| (p, f))))
        .collect();

    if parts.is_empty() {
        return None;
    }

    let part01 = parts.iter().min_by_key(|(p, _)| *p).map(|(_, f)| *f)?;
    let highest = parts.iter().max_by_key(|(p, _)| *p).map(|(_, f)| *f)?;

    let part01_sorted: Vec<&nzb_rs::Segment> = sorted_segments(part01);
    let highest_sorted: Vec<&nzb_rs::Segment> = sorted_segments(highest);

    let part01_first = part01_sorted.first().copied()?;
    let part01_last = part01_sorted.last().copied()?;
    let highest_last = highest_sorted.last().copied()?;

    let mut canaries: Vec<&str> = vec![
        part01_first.message_id.as_str(),
        part01_last.message_id.as_str(),
    ];

    // Avoid probing the same article twice if part01 == highest part.
    if !std::ptr::eq(part01, highest) {
        canaries.push(highest_last.message_id.as_str());
    }

    Some(canaries)
}

/// Borrow `file.segments` sorted by `seg.number`. Mirror of the sort
/// `streaming::preflight::probe_rar_inner` does so canary picks line up
/// with what pre-flight actually fetches.
fn sorted_segments(file: &File) -> Vec<&nzb_rs::Segment> {
    let mut v: Vec<&nzb_rs::Segment> = file.segments.iter().collect();
    v.sort_by_key(|s| s.number);
    v
}

/// Build the AVAILABILITY_CACHE key for a given (nzb_url, server_urls) pair.
/// Public so external callers (notably `streaming::preflight`) can invalidate
/// stale entries without re-implementing the hash.
pub fn cache_key_for(nzb_url: &str, server_urls: &[String]) -> String {
    let mut sorted = server_urls.to_vec();
    sorted.sort();
    format!(
        "nzb-avail:{}",
        sha1_short(&format!("{nzb_url}|{}", sorted.join(",")))
    )
}

/// Drop any cached "ok" verdict for this NZB. Called by `streaming::preflight`
/// when the smoke segment turns out to be missing on every server — the
/// cached verdict is stale (article aged out / DMCA'd between the original
/// probe and now) and shouldn't be trusted on the next search.
pub async fn invalidate_for(nzb_url: &str, server_urls: &[String]) {
    let key = cache_key_for(nzb_url, server_urls);
    AVAILABILITY_CACHE.invalidate(&key).await;
    tracing::info!("[nzbAvailability] invalidated stale cache entry for nzb_url");
}

pub async fn check_nzb_availability(
    client: &reqwest::Client,
    server_urls: &[String],
    nzb_url: &str,
) -> AvailabilityResult {
    let cache_key = cache_key_for(nzb_url, server_urls);

    if let Some(hit) = AVAILABILITY_CACHE.get(&cache_key).await {
        return hit;
    }

    let result = probe(client, server_urls, nzb_url).await;
    AVAILABILITY_CACHE.insert(cache_key, result.clone()).await;
    result
}

async fn probe(
    client: &reqwest::Client,
    server_urls: &[String],
    nzb_url: &str,
) -> AvailabilityResult {
    let resp = match client.get(nzb_url).timeout(FETCH_TIMEOUT).send().await {
        Ok(r) => r,
        Err(err) => {
            return AvailabilityResult {
                ok: false,
                reason: Some(format!("fetch-failed: {}", crate::util::redact_log(&err.to_string()))),
            };
        }
    };

    if !resp.status().is_success() {
        return AvailabilityResult {
            ok: false,
            reason: Some(format!("http-{}", resp.status().as_u16())),
        };
    }

    if let Some(cl) = resp.content_length() {
        if cl > MAX_NZB_SIZE {
            return AvailabilityResult {
                ok: false,
                reason: Some(format!("nzb-too-large-{cl}")),
            };
        }
    }

    let xml = match resp.text().await {
        Ok(t) => t,
        Err(err) => {
            return AvailabilityResult {
                ok: false,
                reason: Some(format!("read-failed: {}", crate::util::redact_log(&err.to_string()))),
            };
        }
    };

    if (xml.len() as u64) > MAX_NZB_SIZE {
        return AvailabilityResult {
            ok: false,
            reason: Some(format!("nzb-too-large-{}", xml.len())),
        };
    }

    let nzb = match Nzb::parse(&xml) {
        Ok(n) => n,
        Err(err) => {
            return AvailabilityResult {
                ok: false,
                reason: Some(format!("parse-failed: {}", crate::util::redact_log(&err.to_string()))),
            };
        }
    };

    let canaries = match pick_canaries(&nzb) {
        Some(c) => c,
        None => {
            // No RAR parts — fall back to the largest non-par2 file's first
            // and last segments. Matches Usenet-Ultimate's "obfuscated
            // filename" path; gives availability checks a chance to work on
            // single-file releases too.
            let largest = nzb
                .files
                .iter()
                .filter(|f| !f.is_par2())
                .max_by_key(|f| f.size());
            let Some(f) = largest else {
                return AvailabilityResult {
                    ok: false,
                    reason: Some("no-files".to_string()),
                };
            };
            let first = match f.segments.first() {
                Some(s) => s.message_id.as_str(),
                None => {
                    return AvailabilityResult {
                        ok: false,
                        reason: Some("no-segments".to_string()),
                    };
                }
            };
            let last = f.segments.last().map(|s| s.message_id.as_str());
            let mut v = vec![first];
            if let Some(l) = last {
                if l != first {
                    v.push(l);
                }
            }
            v
        }
    };

    // Strict AND coverage: every server probes every canary.
    // Probes for one server are sequential (cleanest reuse of body_probe's
    // dedicated socket per article); servers are checked sequentially too,
    // since failure on any server short-circuits.
    for server in server_urls {
        let safe_server = crate::util::redact_url(server);
        for canary in &canaries {
            match body_probe(server, canary).await {
                Ok(true) => continue,
                Ok(false) => {
                    return AvailabilityResult {
                        ok: false,
                        reason: Some(format!("missing-on-server: {safe_server}")),
                    };
                }
                Err(err) => {
                    return AvailabilityResult {
                        ok: false,
                        reason: Some(format!(
                            "probe-error on {safe_server}: {}",
                            crate::util::redact_log(&err.to_string())
                        )),
                    };
                }
            }
        }
    }

    AvailabilityResult {
        ok: true,
        reason: None,
    }
}

/// Filter items by NZB availability. Drops items that fail any canary probe
/// on any configured server. Runs probes in parallel across items.
pub async fn filter_by_nzb_availability(
    client: &reqwest::Client,
    items: Vec<Item>,
    server_urls: &[String],
) -> Vec<Item> {
    if items.is_empty() || server_urls.is_empty() {
        return items;
    }

    let total = items.len();
    let checks = items.into_iter().map(|item| {
        let client = client.clone();
        let servers = server_urls.to_vec();
        async move {
            let url = item_nzb_url(&item);
            let res = check_nzb_availability(&client, &servers, &url).await;
            (item, res)
        }
    });
    let results: Vec<(Item, AvailabilityResult)> = futures::future::join_all(checks).await;

    let dropped: Vec<&(Item, AvailabilityResult)> =
        results.iter().filter(|(_, r)| !r.ok).collect();

    if !dropped.is_empty() {
        let preview: Vec<String> = dropped
            .iter()
            .take(20)
            .map(|(it, r)| {
                format!(
                    "\"{}\" ({})",
                    it.title,
                    r.reason.as_deref().unwrap_or("?")
                )
            })
            .collect();
        let suffix = if dropped.len() > 3 { "..." } else { "" };
        tracing::info!(
            "[nzbAvailability] excluded {} of {}: {}{}",
            dropped.len(),
            total,
            preview.join(", "),
            suffix
        );
    }

    results
        .into_iter()
        .filter(|(_, r)| r.ok)
        .map(|(it, _)| it)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_number_extracts_correctly() {
        assert_eq!(part_number("Movie.part01.rar"), Some(1));
        assert_eq!(part_number("Movie.part001.rar yEnc"), Some(1));
        assert_eq!(part_number("Movie.part42.rar (1/10)"), Some(42));
        assert_eq!(part_number("Movie.PART15.rar"), Some(15)); // case-insensitive
        assert_eq!(part_number("Movie.r00"), None);
        assert_eq!(part_number("Movie.rar"), None);
        assert_eq!(part_number("Movie.mkv"), None);
    }

    #[test]
    fn pick_canaries_with_multiple_parts() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="x" date="1700000000" subject="Show.part01.rar yEnc (1/3) 12345">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="100" number="1">part01-seg1@x</segment>
      <segment bytes="100" number="2">part01-seg2@x</segment>
      <segment bytes="100" number="3">part01-seg3@x</segment>
    </segments>
  </file>
  <file poster="x" date="1700000000" subject="Show.part02.rar yEnc (1/2) 12345">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="100" number="1">part02-seg1@x</segment>
      <segment bytes="100" number="2">part02-seg2@x</segment>
    </segments>
  </file>
</nzb>"#;
        let nzb = Nzb::parse(xml).expect("valid nzb");
        let canaries = pick_canaries(&nzb).expect("has rars");
        assert_eq!(canaries.len(), 3);
        // first segment of part01
        assert_eq!(canaries[0], "part01-seg1@x");
        // last segment of part01 — critical: Stremio's getFileSize reads this
        assert_eq!(canaries[1], "part01-seg3@x");
        // last segment of highest partNN (part02)
        assert_eq!(canaries[2], "part02-seg2@x");
    }

    #[test]
    fn pick_canaries_dedupes_when_only_one_part() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="x" date="1700000000" subject="Show.part01.rar yEnc (1/2) 12345">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="100" number="1">single-seg1@x</segment>
      <segment bytes="100" number="2">single-seg2@x</segment>
    </segments>
  </file>
</nzb>"#;
        let nzb = Nzb::parse(xml).expect("valid nzb");
        let canaries = pick_canaries(&nzb).expect("has rar");
        // Should not include part02 entry since highest == part01.
        assert_eq!(canaries.len(), 2);
        assert_eq!(canaries[0], "single-seg1@x");
        assert_eq!(canaries[1], "single-seg2@x");
    }

    #[test]
    fn pick_canaries_returns_none_for_no_rars() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="x" date="1700000000" subject="Movie.mkv yEnc (1/1) 12345">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="100" number="1">mkv-seg1@x</segment>
    </segments>
  </file>
</nzb>"#;
        let nzb = Nzb::parse(xml).expect("valid nzb");
        assert!(pick_canaries(&nzb).is_none());
    }

    #[test]
    fn sha1_short_is_16_hex_chars() {
        let s = sha1_short("https://example.com/foo.nzb|server1,server2");
        assert_eq!(s.len(), 16);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
