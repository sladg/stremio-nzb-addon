use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use url::Url;

use crate::cache::{build_cache_key, RSS_CACHE};
use crate::config::Indexer;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RssRoot {
    #[serde(default)]
    pub item: Option<Vec<Item>>,
    #[serde(default)]
    pub channel: Option<RssChannel>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RssChannel {
    #[serde(default)]
    pub item: Option<Vec<Item>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Item {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub guid: serde_json::Value,
    #[serde(default)]
    pub link: Option<String>,
    #[serde(default)]
    pub comments: Option<String>,
    #[serde(default, rename = "pubDate")]
    pub pub_date: serde_json::Value,
    #[serde(default)]
    pub category: serde_json::Value,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub enclosure: Option<Enclosure>,
    #[serde(default)]
    pub attr: Option<Vec<Attr>>,
    #[serde(default)]
    pub id: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Enclosure {
    #[serde(rename = "@attributes")]
    pub attributes: EnclosureAttrs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnclosureAttrs {
    pub url: String,
    #[serde(default)]
    pub length: Option<String>,
    #[serde(default, rename = "type")]
    pub type_: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attr {
    #[serde(rename = "@attributes")]
    pub attributes: AttrPair,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttrPair {
    pub name: String,
    pub value: String,
}

pub enum FunctionType {
    Movie,
    TvSearch,
}

impl FunctionType {
    fn as_str(&self) -> &'static str {
        match self {
            FunctionType::Movie => "movie",
            FunctionType::TvSearch => "tvsearch",
        }
    }
}

#[derive(Clone)]
pub struct NzbWebApi {
    base_url: String,
    api_key: String,
    client: reqwest::Client,
}

impl NzbWebApi {
    pub fn new(base_url: String, api_key: String, client: reqwest::Client) -> Self {
        Self {
            base_url,
            api_key,
            client,
        }
    }

    fn build_url(&self, t: FunctionType) -> Result<Url> {
        let mut url = Url::parse(&self.base_url)?;
        url.set_path("/api");
        url.query_pairs_mut()
            .clear()
            .append_pair("apikey", &self.api_key)
            .append_pair("t", t.as_str())
            .append_pair("o", "json");
        Ok(url)
    }

    async fn call(&self, url: Url) -> Result<RssRoot> {
        let resp = self
            .client
            .get(url)
            .timeout(Duration::from_secs(15))
            .send()
            .await?;
        let root: RssRoot = resp.json().await?;
        Ok(root)
    }

    async fn cached_call<F, Fut>(&self, cache_key: String, build: F) -> Result<Arc<RssChannel>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<RssRoot>>,
    {
        if let Some(hit) = RSS_CACHE.get(&cache_key).await {
            tracing::info!("[Cache] HIT: {cache_key}");
            return Ok(hit);
        }
        tracing::info!("[Cache] MISS: {cache_key}");

        let root = build().await?;

        let channel = if root.item.is_some() {
            RssChannel { item: root.item }
        } else {
            root.channel.unwrap_or_default()
        };
        let arc = Arc::new(channel);
        RSS_CACHE.insert(cache_key, arc.clone()).await;
        Ok(arc)
    }

    pub async fn search_movie(&self, imdb_id: &str) -> Result<Arc<RssChannel>> {
        let cache_key = build_cache_key(&self.base_url, "movie", imdb_id);
        self.cached_call(cache_key, || async {
            let mut url = self.build_url(FunctionType::Movie)?;
            url.query_pairs_mut()
                .append_pair("imdbid", imdb_id)
                .append_pair("extended", "1");
            self.call(url).await
        })
        .await
    }

    pub async fn search_series(
        &self,
        tvdb_id: &str,
        season: &str,
        episode: &str,
    ) -> Result<Arc<RssChannel>> {
        let key = format!("{tvdb_id}:{season}:{episode}");
        let cache_key = build_cache_key(&self.base_url, "series", &key);
        self.cached_call(cache_key, || async {
            let mut url = self.build_url(FunctionType::TvSearch)?;
            url.query_pairs_mut()
                .append_pair("tvdbid", tvdb_id)
                .append_pair("season", season)
                .append_pair("ep", episode)
                .append_pair("extended", "1");
            self.call(url).await
        })
        .await
    }
}

pub struct NzbWebApiPool {
    apis: Vec<NzbWebApi>,
}

impl NzbWebApiPool {
    pub fn new(indexers: &[Indexer], client: reqwest::Client) -> Self {
        let apis = indexers
            .iter()
            .map(|i| NzbWebApi::new(i.url.clone(), i.api_key.clone(), client.clone()))
            .collect();
        Self { apis }
    }

    async fn fanout<F, Fut>(&self, mut f: F) -> Vec<Item>
    where
        F: FnMut(NzbWebApi) -> Fut,
        Fut: std::future::Future<Output = Result<Arc<RssChannel>>>,
    {
        let futs = self.apis.iter().cloned().map(&mut f);
        let results = futures::future::join_all(futs).await;
        let mut out = Vec::new();
        for r in results {
            match r {
                Ok(ch) => {
                    if let Some(items) = &ch.item {
                        out.extend(items.iter().cloned());
                    }
                }
                Err(err) => tracing::warn!(
                    "indexer call failed: {}",
                    crate::util::redact_log(&err.to_string())
                ),
            }
        }
        out
    }

    pub async fn search_movie(&self, imdb_id: &str) -> Vec<Item> {
        self.fanout(|api| {
            let i = imdb_id.to_string();
            async move { api.search_movie(&i).await }
        })
        .await
    }

    pub async fn search_series(&self, tvdb_id: &str, season: &str, episode: &str) -> Vec<Item> {
        self.fanout(|api| {
            let t = tvdb_id.to_string();
            let s = season.to_string();
            let e = episode.to_string();
            async move { api.search_series(&t, &s, &e).await }
        })
        .await
    }
}

/// Extract a named newznab attribute value.
pub fn item_attr<'a>(item: &'a Item, name: &str) -> Option<&'a str> {
    item.attr
        .as_ref()?
        .iter()
        .find(|a| a.attributes.name == name)
        .map(|a| a.attributes.value.as_str())
}

/// Bytes from `size` attr, mirroring src/addon.ts:53.
pub fn item_size(item: &Item) -> u64 {
    item_attr(item, "size")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
}

/// NZB URL extraction, mirroring src/addon.ts:66.
pub fn item_nzb_url(item: &Item) -> String {
    let raw = item
        .link
        .as_deref()
        .map(|s| s.replace("&amp;", "&"))
        .or_else(|| item.enclosure.as_ref().map(|e| e.attributes.url.clone()))
        .unwrap_or_default();

    if raw.contains('&') && !raw.contains('?') {
        raw.replacen('&', "?", 1)
    } else {
        raw
    }
}
