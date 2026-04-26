use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

static CACHE: Lazy<Mutex<HashMap<String, String>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static SERIESID_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<seriesid>(\d+)</seriesid>").expect("seriesid regex"));

pub async fn imdb_to_tvdb(client: &reqwest::Client, imdb_id: &str) -> Option<String> {
    if let Some(hit) = CACHE.lock().ok().and_then(|m| m.get(imdb_id).cloned()) {
        return Some(hit);
    }

    let url = format!("https://thetvdb.com/api/GetSeriesByRemoteID.php?imdbid={imdb_id}");
    let resp = match client
        .get(&url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
    {
        Ok(r) => r,
        Err(err) => {
            tracing::error!("Failed to convert IMDB to TVDB ({imdb_id}): {err}");
            return None;
        }
    };

    let body = match resp.text().await {
        Ok(t) => t,
        Err(err) => {
            tracing::error!("Failed to read TVDB body ({imdb_id}): {err}");
            return None;
        }
    };

    let captured = SERIESID_RE.captures(&body)?.get(1)?.as_str().to_string();
    if let Ok(mut m) = CACHE.lock() {
        m.insert(imdb_id.to_string(), captured.clone());
    }
    Some(captured)
}
