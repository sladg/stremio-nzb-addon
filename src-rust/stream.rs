use crate::cinemeta::{expand_original, original_languages, wants_original};
use crate::config::{AddonConfig, UserConfig};
use crate::content_filter::{filter_by_language, filter_by_title_regex};
use crate::manifest::{ADDON_ID, ADDON_NAME};
use crate::nzb_api::{item_size, Item, NzbWebApiPool};
use crate::nzb_availability::filter_by_nzb_availability;
use crate::nzb_sanity::filter_by_nzb_sanity;
use crate::parse_title::{parse, Parsed};
use crate::quality::filter_by_quality;
use crate::streaming::candidate::{candidate_from_item, group_candidates, NzbCandidate};
use crate::streaming::session::{register_group, SessionRegistry};
use crate::stremio::{Stream, StreamBehaviorHints};
use crate::tvdb::imdb_to_tvdb;
use url::Url;

const RESOLUTION_ORDER: &[&[&str]] = &[
    &["2160p", "4k"],
    &["1440p"],
    &["1080p"],
    &["720p"],
    &["480p"],
    &["360p"],
];

fn resolution_index(res: &str) -> usize {
    let lower = res.to_ascii_lowercase();
    for (i, group) in RESOLUTION_ORDER.iter().enumerate() {
        if group.iter().any(|g| g.eq_ignore_ascii_case(&lower)) {
            return i;
        }
    }
    RESOLUTION_ORDER.len()
}

fn sort_by_resolution(items: &mut [Item]) {
    // Parse once per item, then sort by the precomputed bucket index.
    // (`sort_by_key` would re-call the key-fn O(N log N) times — same problem
    // as a parsing comparator. `sort_by_cached_key` materializes keys once.)
    items.sort_by_cached_key(|item| {
        resolution_index(parse(&item.title).resolution.as_deref().unwrap_or(""))
    });
}

/// Keep first `per_res` items per resolution bucket (items must be pre-sorted
/// by `sort_by_resolution`). Mirrors TS `limitPerResolution`.
#[cfg(test)]
fn limit_per_resolution(items: Vec<Item>, per_res: u32) -> Vec<Item> {
    if per_res == 0 {
        return items;
    }
    use std::collections::HashMap;
    let mut counts: HashMap<usize, u32> = HashMap::new();
    let total = items.len();
    let kept: Vec<Item> = items
        .into_iter()
        .filter(|item| {
            let bucket = resolution_index(parse(&item.title).resolution.as_deref().unwrap_or(""));
            let c = counts.entry(bucket).or_insert(0);
            if *c < per_res {
                *c += 1;
                true
            } else {
                false
            }
        })
        .collect();
    if kept.len() < total {
        tracing::info!(
            "[limitPerResolution] kept {} of {} items (cap = {} per bucket)",
            kept.len(),
            total,
            per_res
        );
    }
    kept
}

/// Keep first `per_res` GROUPS per resolution bucket. Phase 5 replaces
/// `limit_per_resolution` because Stremio's UX-visible "streams per
/// resolution" really means "stream entries (= groups)" — the per-group
/// fallback list is unbounded.
fn limit_groups_per_resolution(
    groups: Vec<Vec<NzbCandidate>>,
    per_res: u32,
) -> Vec<Vec<NzbCandidate>> {
    if per_res == 0 {
        return groups;
    }
    use std::collections::HashMap;
    // Bucket by (resolution, language) so MULTi/DUAL packs and per-language
    // releases get their own quota instead of competing against the English
    // releases that dominate most indexer rankings. For an `english, czech`
    // preference on a US movie, this surfaces both the top 2 English uploads
    // AND the top 2 MULTi packs at 1080p — the user can pick whichever has
    // the language track they want. None-language (untagged) is its own
    // bucket too; that's typically the bulk of English releases.
    let mut counts: HashMap<(usize, Option<String>), u32> = HashMap::new();
    let total = groups.len();
    let kept: Vec<Vec<NzbCandidate>> = groups
        .into_iter()
        .filter(|group| {
            let sig = &group[0].signature;
            let bucket = (
                resolution_index(sig.resolution.as_deref().unwrap_or("")),
                sig.language.clone(),
            );
            let c = counts.entry(bucket).or_insert(0);
            if *c < per_res {
                *c += 1;
                true
            } else {
                false
            }
        })
        .collect();
    if kept.len() < total {
        tracing::info!(
            "[limitPerResolution] kept {} of {} groups (cap = {} per bucket)",
            kept.len(),
            total,
            per_res
        );
    }
    kept
}

fn human_size(size: u64) -> String {
    if size == 0 {
        return "0 B".to_string();
    }
    const UNITS: &[&str] = &["B", "kB", "MB", "GB", "TB"];
    let mut s = size as f64;
    let mut i = 0usize;
    while s >= 1024.0 && i < UNITS.len() - 1 {
        s /= 1024.0;
        i += 1;
    }
    format!("{:.2} {}", s, UNITS[i])
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// Build a `Stream` for an indexer hit. `url` is the public addon URL
/// pointing at our `/v/{token}.mkv` endpoint; the caller registers the
/// session and supplies the URL.
///
/// `original_langs` is the list of canonical languages resolved via
/// Cinemeta for this title (`["japanese"]` for an anime, `["english"]`
/// for a US show, `[]` if Cinemeta couldn't resolve it). Used as the
/// fallback for the displayed `Lang:` line when the release title itself
/// doesn't tag a language — common for anime where releases ship without
/// `JAPANESE` in the filename even though the audio is Japanese.
pub fn item_to_stream(
    item: &Item,
    name: &str,
    manifest_id: &str,
    url: String,
    original_langs: &[String],
) -> Stream {
    let size = item_size(item);
    let parsed: Parsed = parse(&item.title);

    // Description is ASCII-only on purpose. Some clients (Stremio's
    // description renderer included) treat `application/json` without
    // explicit `charset=utf-8` as Latin-1, which mojibakes any multi-byte
    // UTF-8 (emoji, bullets, accented characters). Plain ASCII labels
    // survive every charset interpretation. The original TS port used
    // emoji prefixes (📁🎥📦🎧🔍) and the `•` bullet — both lost.
    let mut desc_lines: Vec<String> = Vec::new();
    desc_lines.push(parsed.title.clone());

    let mut media_info: Vec<&str> = Vec::new();
    if let Some(s) = parsed.source.as_deref() {
        if !s.is_empty() {
            media_info.push(s);
        }
    }
    if let Some(c) = parsed.codec.as_deref() {
        if !c.is_empty() {
            media_info.push(c);
        }
    }
    if let Some(g) = parsed.group.as_deref() {
        if !g.is_empty() {
            media_info.push(g);
        }
    }
    if !media_info.is_empty() {
        desc_lines.push(format!("Source: {}", media_info.join(" / ")));
    }

    if size > 0 {
        desc_lines.push(format!("Size: {}", human_size(size)));
    }

    if let Some(a) = parsed.audio.as_deref().filter(|s| !s.is_empty()) {
        desc_lines.push(format!("Audio: {a}"));
    }
    // `Lang:` priority: explicit per-release tag → Cinemeta original-language
    // fallback. Most English releases skip the language tag entirely; same for
    // anime (Japanese assumed). Cinemeta gives us the show's natural language
    // so we can still surface it in the description.
    let lang_line: Option<String> = match parsed.language.as_deref().filter(|s| !s.is_empty()) {
        Some(l) => Some(capitalize(l)),
        None if !original_langs.is_empty() => Some(
            original_langs
                .iter()
                .map(|s| capitalize(s))
                .collect::<Vec<_>>()
                .join(" / "),
        ),
        None => None,
    };
    if let Some(line) = lang_line {
        desc_lines.push(format!("Lang: {line}"));
    }

    if let Some(comments) = item.comments.as_deref() {
        if !comments.is_empty() {
            if let Ok(u) = Url::parse(comments) {
                if let Some(host) = u.host_str() {
                    let host = host
                        .strip_prefix("www.")
                        .or_else(|| host.strip_prefix("api."))
                        .unwrap_or(host);
                    desc_lines.push(format!("Indexer: {host}"));
                }
            }
        }
    }

    let binge_parts: Vec<String> = [
        Some(manifest_id.to_string()),
        parsed.resolution.clone(),
        parsed.source.clone(),
        parsed.codec.clone(),
        parsed.group.clone(),
        parsed.audio.clone(),
        parsed.language.clone(),
    ]
    .into_iter()
    .flatten()
    .filter(|s| !s.is_empty())
    .collect();

    let stream_name = match parsed.resolution.as_deref().filter(|s| !s.is_empty()) {
        Some(res) => format!("{name} {res}"),
        None => name.to_string(),
    };

    Stream {
        name: stream_name,
        description: desc_lines.join("\n"),
        url,
        behavior_hints: StreamBehaviorHints {
            filename: item.title.clone(),
            video_size: if size > 0 { Some(size) } else { None },
            binge_group: binge_parts.join("|"),
            not_web_ready: true,
        },
    }
}

pub async fn build_streams(
    _cfg: &AddonConfig,
    user: &UserConfig,
    type_: String,
    id: String,
    client: reqwest::Client,
    host: &str,
    sessions: &SessionRegistry,
) -> Vec<Stream> {
    // `_cfg` is unused right now — kept in the signature so future
    // operator-level concerns (e.g. shared rate-limit state) have a place
    // to plug in without another signature change.
    let api = NzbWebApiPool::new(&user.indexers, client.clone());

    let mut items: Vec<Item> = match type_.as_str() {
        "movie" => {
            let imdb = id.trim_start_matches("tt");
            api.search_movie(imdb).await
        }
        "series" => {
            let mut parts = id.splitn(3, ':');
            let imdb_part = parts.next().unwrap_or("");
            let season = parts.next().unwrap_or("");
            let episode = parts.next().unwrap_or("");
            match imdb_to_tvdb(&client, imdb_part).await {
                Some(tvdb) => api.search_series(&tvdb, season, episode).await,
                None => {
                    tracing::warn!("Could not find TVDB ID for IMDB: {imdb_part}");
                    Vec::new()
                }
            }
        }
        other => {
            tracing::warn!("Unsupported type '{other}' with id {id}");
            Vec::new()
        }
    };

    if (user.min_gbit_per_hour.is_some() || user.max_gbit_per_hour.is_some())
        && (type_ == "movie" || type_ == "series")
    {
        items = filter_by_quality(items, user.min_gbit_per_hour, user.max_gbit_per_hour, &type_);
    }

    items = filter_by_title_regex(items, user.exclude_regex.as_deref());

    // Resolve the show/movie's original language(s) via Cinemeta (cached).
    // Used for two things:
    //   1. Expanding the `"original"` sentinel in `preferredLanguages` so the
    //      filter accepts the show's native audio when the user opted in.
    //   2. Falling back as the displayed `Lang:` line on releases that don't
    //      tag a language in the filename (anime, most English releases).
    let imdb_id = match type_.as_str() {
        "movie" => format!("tt{}", id.trim_start_matches("tt")),
        "series" => id.split(':').next().unwrap_or("").to_string(),
        _ => String::new(),
    };
    let originals: Vec<String> = if imdb_id.is_empty() {
        Vec::new()
    } else {
        original_languages(&client, &type_, &imdb_id).await
    };

    let effective_languages: Vec<String> = if wants_original(&user.preferred_languages) {
        expand_original(&user.preferred_languages, &originals)
    } else {
        user.preferred_languages.clone()
    };

    items = filter_by_language(items, &effective_languages);

    if user.validate_nzb_structure.unwrap_or(false) && !items.is_empty() {
        items = filter_by_nzb_sanity(&client, items).await;
    }

    let servers: Vec<String> = user.nntp_servers.iter().map(|s| s.server.clone()).collect();

    if user.validate_nzb_availability.unwrap_or(false) && !items.is_empty() && !servers.is_empty() {
        items = filter_by_nzb_availability(&client, items, &servers).await;
    }

    sort_by_resolution(&mut items);

    let per_res = user.streams_per_resolution.unwrap_or(1);

    // Phase 5: collapse re-uploads of the same release (= same GroupSignature)
    // into a single Stremio entry whose pre-flight walks the upload list.
    let candidates: Vec<NzbCandidate> = items.into_iter().map(candidate_from_item).collect();
    let groups = group_candidates(candidates);
    let limited = limit_groups_per_resolution(groups, per_res);

    let streams: Vec<Stream> = limited
        .into_iter()
        .map(|group| {
            // Display metadata comes from the first (= top-ranked) candidate.
            // The stream URL points at a session whose pre-flight walks the
            // full group; auto-fallback is invisible to Stremio.
            let display_item = group[0].item.clone();
            let token = register_group(sessions, group);
            let url = format!("http://{host}/v/{token}.mkv");
            item_to_stream(&display_item, ADDON_NAME, ADDON_ID, url, &originals)
        })
        .collect();

    tracing::info!("Found {} streams for {} {}", streams.len(), type_, id);
    streams
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str) -> Item {
        Item {
            title: title.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn limit_per_resolution_zero_disables_cap() {
        let items = vec![item("A.1080p.mkv"), item("B.1080p.mkv"), item("C.720p.mkv")];
        let out = limit_per_resolution(items, 0);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn limit_per_resolution_caps_per_bucket() {
        // 3 of 1080p, 2 of 720p, 1 of 480p
        let items = vec![
            item("A1.1080p.mkv"),
            item("A2.1080p.mkv"),
            item("A3.1080p.mkv"),
            item("B1.720p.mkv"),
            item("B2.720p.mkv"),
            item("C1.480p.mkv"),
        ];
        let out = limit_per_resolution(items, 2);
        assert_eq!(out.len(), 5); // 2 + 2 + 1
        let count_1080 = out.iter().filter(|i| i.title.contains("1080p")).count();
        let count_720 = out.iter().filter(|i| i.title.contains("720p")).count();
        assert_eq!(count_1080, 2);
        assert_eq!(count_720, 2);
    }

    #[test]
    fn limit_per_resolution_default_one_keeps_first_per_bucket() {
        let items = vec![
            item("Best.1080p.WEB-DL.mkv"),
            item("Other.1080p.HDTV.mkv"),
            item("Older.720p.DVDRip.mkv"),
        ];
        let out = limit_per_resolution(items, 1);
        assert_eq!(out.len(), 2);
        // First-seen 1080p kept
        assert_eq!(out[0].title, "Best.1080p.WEB-DL.mkv");
        assert_eq!(out[1].title, "Older.720p.DVDRip.mkv");
    }

    #[test]
    fn resolution_index_buckets_2160p_with_4k() {
        assert_eq!(resolution_index("2160p"), resolution_index("4k"));
        assert_eq!(resolution_index("2160P"), resolution_index("4K"));
    }

    #[test]
    fn resolution_index_orders_high_to_low() {
        assert!(resolution_index("2160p") < resolution_index("1080p"));
        assert!(resolution_index("1080p") < resolution_index("720p"));
        assert!(resolution_index("720p") < resolution_index("480p"));
        // unknown → fallback bucket
        assert!(resolution_index("xyz") >= resolution_index("360p"));
    }

    #[test]
    fn human_size_formats_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1024), "1.00 kB");
        assert_eq!(human_size(1024 * 1024), "1.00 MB");
        assert_eq!(human_size(1024u64 * 1024 * 1024), "1.00 GB");
    }

    #[tokio::test]
    async fn build_streams_registers_one_session_per_group() {
        use crate::config::{AddonConfig, Indexer, NntpServer};
        use crate::streaming::session::new_registry;

        // Invalid indexer → no items returned → no sessions registered.
        // Just verifies the wiring compiles and runs without panic.
        let cfg = AddonConfig {
            defaults: UserConfig {
                indexers: vec![Indexer {
                    url: "http://x.invalid".into(),
                    api_key: "k".into(),
                }],
                nntp_servers: vec![NntpServer {
                    server: "nntps://u:p@y.invalid/1".into(),
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        let user = cfg.defaults.clone();
        let registry = new_registry();
        let _streams = build_streams(
            &cfg,
            &user,
            "movie".to_string(),
            "tt0133093".to_string(),
            reqwest::Client::new(),
            "test:3001",
            &registry,
        )
        .await;
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn limit_groups_per_resolution_caps_groups_not_candidates() {
        use crate::streaming::candidate::candidate_from_item;

        // Build groups: 3× 1080p (different release groups), 2× 720p, 1× 480p.
        let groups: Vec<Vec<NzbCandidate>> = vec![
            vec![candidate_from_item(item("Show.S01E01.1080p.WEB-DL.x265-A"))],
            vec![candidate_from_item(item("Show.S01E01.1080p.WEB-DL.x265-B"))],
            vec![candidate_from_item(item("Show.S01E01.1080p.BluRay.x264-C"))],
            vec![candidate_from_item(item("Show.S01E01.720p.WEB-DL.x265-D"))],
            vec![candidate_from_item(item("Show.S01E01.720p.HDTV.x264-E"))],
            vec![candidate_from_item(item("Show.S01E01.480p.DVDRip.x264-F"))],
        ];

        // Cap = 2 → keep 2× 1080p, 2× 720p, 1× 480p = 5 total.
        let kept = limit_groups_per_resolution(groups.clone(), 2);
        assert_eq!(kept.len(), 5);
        let count_1080 = kept
            .iter()
            .filter(|g| g[0].signature.resolution.as_deref() == Some("1080p"))
            .count();
        assert_eq!(count_1080, 2);

        // Cap = 0 → no cap, all 6 kept.
        let unrestricted = limit_groups_per_resolution(groups.clone(), 0);
        assert_eq!(unrestricted.len(), 6);

        // Cap = 1 (default) → 1 per resolution = 3 total.
        let strict = limit_groups_per_resolution(groups, 1);
        assert_eq!(strict.len(), 3);
    }

    #[test]
    fn limit_groups_per_resolution_buckets_by_language_too() {
        use crate::streaming::candidate::candidate_from_item;

        // 3× 1080p English (untagged) and 2× 1080p MULTi packs in the same
        // resolution. With cap=2 and the (resolution, language) bucket key
        // we expect 2 English + 2 MULTi = 4 streams. The old (resolution-only)
        // behavior would have collapsed all 5 into 2 streams and dropped the
        // multi-language packs entirely.
        let groups: Vec<Vec<NzbCandidate>> = vec![
            vec![candidate_from_item(item("Movie.1994.1080p.WEB-DL.x265-A"))],
            vec![candidate_from_item(item("Movie.1994.1080p.WEB-DL.x265-B"))],
            vec![candidate_from_item(item("Movie.1994.1080p.WEB-DL.x265-C"))],
            vec![candidate_from_item(item("Movie.1994.MULTi.1080p.BluRay.x264-FHD"))],
            vec![candidate_from_item(item("Movie.1994.DUAL.1080p.BluRay-USELESS"))],
        ];
        // MULTi and DUAL both signature as language="multi" — same bucket.
        // Result: 2 untagged + 2 multi = 4 (cap=2 per (res, lang)).
        let kept = limit_groups_per_resolution(groups, 2);
        assert_eq!(kept.len(), 4);
        let multi_kept = kept
            .iter()
            .filter(|g| g[0].signature.language.as_deref() == Some("multi"))
            .count();
        assert_eq!(multi_kept, 2);
    }

    #[test]
    fn item_to_stream_url_format() {
        let it = item("Show.S01E01.720p.HDTV.mkv");
        let s = item_to_stream(
            &it,
            "NZB",
            "io.sladg.nzb",
            "http://192.168.1.10:3000/v/abc.mkv".to_string(),
            &[],
        );
        assert_eq!(s.url, "http://192.168.1.10:3000/v/abc.mkv");
        assert_eq!(s.behavior_hints.filename, "Show.S01E01.720p.HDTV.mkv");
        assert!(s.behavior_hints.binge_group.starts_with("io.sladg.nzb"));
        assert!(s.behavior_hints.binge_group.contains("720p"));
    }

    #[test]
    fn stream_json_uses_url_not_nzburl_or_servers() {
        // Ensure the Phase 3 contract change is reflected in the wire format.
        let it = item("Movie.2024.1080p.WEB-DL.x265-RARBG.mkv");
        let s = item_to_stream(&it, "NZB", "io.sladg.nzb", "http://h/v/T.mkv".to_string(), &[]);
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains(r#""url":"http://h/v/T.mkv""#));
        assert!(!json.contains("nzbUrl"));
        assert!(!json.contains("\"servers\""));
        // notWebReady forces external player on web Stremio (improvements.md #1).
        assert!(json.contains(r#""notWebReady":true"#));
        // behaviorHints stays
        assert!(json.contains(r#""behaviorHints""#));
        assert!(json.contains(r#""bingeGroup""#));
    }
}
