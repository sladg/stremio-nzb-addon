use chrono_lite::format_iso8601;

use crate::config::UserConfig;
use crate::manifest::{catalog, manifest, ADDON_ID, ADDON_NAME};
use crate::nzb_api::{Item, NzbWebApiPool};
use crate::stream::item_to_stream;
use crate::streaming::candidate::{candidate_from_item, group_candidates};
use crate::streaming::session::{register_group, SessionRegistry};
use crate::stremio::{CatalogResponse, Meta, MetaPreview, MetaResponse, MetaVideo};

pub fn handle_catalog(query: Option<&str>) -> CatalogResponse {
    let m = manifest();
    let cat = catalog();

    let q = query.unwrap_or("").trim();
    if q.is_empty() {
        return CatalogResponse {
            metas: Vec::new(),
            cache_max_age: None,
        };
    }

    let id = format!("{}:{}", cat.id, urlencoding::encode(q));
    CatalogResponse {
        metas: vec![MetaPreview {
            id,
            name: q.to_string(),
            type_: "tv",
            logo: Some(m.logo),
            background: None,
            poster_shape: Some("square"),
            poster: Some(m.logo),
            description: format!("Search results from {} for '{q}'", m.name),
        }],
        cache_max_age: Some(3600 * 24 * 30),
    }
}

pub async fn handle_meta(
    user: &UserConfig,
    id: String,
    client: reqwest::Client,
    host: &str,
    sessions: &SessionRegistry,
) -> MetaResponse {
    let cat = catalog();

    let prefix = format!("{}:", cat.id);
    if !id.starts_with(&prefix) {
        return MetaResponse {
            meta: Meta {
                id,
                name: cat.name.to_string(),
                type_: "tv",
                videos: None,
            },
            cache_max_age: None,
        };
    }

    let raw_query = id.trim_start_matches(&prefix);
    let query = urlencoding::decode(raw_query)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| raw_query.to_string());

    let api = NzbWebApiPool::new(&user.indexers, client.clone());
    let items: Vec<Item> = api.search(&query).await;

    // Same Phase 5 grouping as the stream handler — re-uploads of the same
    // release collapse into one MetaVideo whose stream's pre-flight walks
    // the upload list.
    let candidates = items.into_iter().map(candidate_from_item).collect();
    let groups = group_candidates(candidates);

    let videos: Vec<MetaVideo> = groups
        .into_iter()
        .map(|group| {
            let display_item = group[0].item.clone();
            let video_id = format!("{}:{}", cat.id, value_to_string(&display_item.id));
            let title = display_item.title.clone();
            let overview = display_item.description.clone().unwrap_or_default();
            let released = format_iso8601(&display_item.pub_date);

            let token = register_group(sessions, group);
            let url = format!("http://{host}/v/{token}.mkv");

            MetaVideo {
                id: video_id,
                title,
                overview,
                released,
                streams: vec![item_to_stream(&display_item, ADDON_NAME, ADDON_ID, url, &[])],
            }
        })
        .collect();

    MetaResponse {
        meta: Meta {
            id,
            name: cat.name.to_string(),
            type_: "tv",
            videos: Some(videos),
        },
        cache_max_age: Some(3600),
    }
}

fn value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

mod chrono_lite {
    /// Format a JSON value (epoch ms, epoch seconds, or RFC822 string) as ISO8601.
    /// Matches `new Date(item.pubDate).toISOString()` from src/addon.ts:269.
    pub fn format_iso8601(v: &serde_json::Value) -> String {
        match v {
            serde_json::Value::Number(n) => {
                if let Some(ms) = n.as_i64() {
                    epoch_ms_to_iso(ms)
                } else if let Some(f) = n.as_f64() {
                    epoch_ms_to_iso(f as i64)
                } else {
                    String::new()
                }
            }
            serde_json::Value::String(s) => {
                // pubDate may be either an RFC822 string or a numeric string.
                if let Ok(ms) = s.parse::<i64>() {
                    epoch_ms_to_iso(ms)
                } else {
                    s.clone()
                }
            }
            _ => String::new(),
        }
    }

    fn epoch_ms_to_iso(ms: i64) -> String {
        // Avoid pulling in chrono just for this. Implement a minimal ISO8601
        // formatter for UTC milliseconds since epoch.
        let secs = ms.div_euclid(1000);
        let millis = ms.rem_euclid(1000) as u32;
        let (year, month, day, hour, min, sec) = secs_to_ymdhms(secs);
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            year, month, day, hour, min, sec, millis
        )
    }

    fn secs_to_ymdhms(mut secs: i64) -> (i32, u32, u32, u32, u32, u32) {
        let day_secs = 86400;
        let mut days = secs.div_euclid(day_secs);
        secs = secs.rem_euclid(day_secs);
        let sec = (secs % 60) as u32;
        let min = ((secs / 60) % 60) as u32;
        let hour = (secs / 3600) as u32;

        // 1970-01-01 was a Thursday; Jan 1, 1970 = day 0 from epoch.
        let mut year: i32 = 1970;
        loop {
            let leap = is_leap(year);
            let year_days = if leap { 366 } else { 365 };
            if days >= year_days {
                days -= year_days;
                year += 1;
            } else if days < 0 {
                year -= 1;
                let prev_days = if is_leap(year) { 366 } else { 365 };
                days += prev_days;
            } else {
                break;
            }
        }
        let leap = is_leap(year);
        let month_lens: [i64; 12] = [
            31,
            if leap { 29 } else { 28 },
            31, 30, 31, 30, 31, 31, 30, 31, 30, 31,
        ];
        let mut month: u32 = 1;
        for ml in month_lens.iter() {
            if days < *ml {
                break;
            }
            days -= ml;
            month += 1;
        }
        let day = (days + 1) as u32;
        (year, month, day, hour, min, sec)
    }

    fn is_leap(y: i32) -> bool {
        (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
    }
}
