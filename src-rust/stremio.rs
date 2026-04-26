use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    /// Built per-request from the addon's base URL so the logo route
    /// resolves correctly behind any reverse proxy. Hence `String`,
    /// not `&'static str`.
    pub logo: String,
    pub version: &'static str,
    pub resources: Vec<ManifestResource>,
    pub types: Vec<&'static str>,
    pub catalogs: Vec<ManifestCatalog>,
    #[serde(rename = "behaviorHints", skip_serializing_if = "Option::is_none")]
    pub behavior_hints: Option<BehaviorHintsManifest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ManifestResource {
    Simple(&'static str),
    Detailed(ManifestResourceDetail),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestResourceDetail {
    pub name: &'static str,
    pub types: Vec<&'static str>,
    #[serde(rename = "idPrefixes", skip_serializing_if = "Option::is_none")]
    pub id_prefixes: Option<Vec<&'static str>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestCatalog {
    pub id: &'static str,
    pub name: &'static str,
    #[serde(rename = "type")]
    pub type_: &'static str,
    pub extra: Vec<CatalogExtra>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogExtra {
    pub name: &'static str,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BehaviorHintsManifest {
    pub configurable: bool,
    #[serde(rename = "configurationRequired")]
    pub configuration_required: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Stream {
    pub name: String,
    pub description: String,
    /// HTTP URL pointing at our `/v/{token}.mkv` route. Stremio plays this
    /// like any other HTTP video; the addon serves bytes by fetching NZB
    /// segments + yEnc-decoding + RAR-aware byte mapping in-process.
    pub url: String,
    #[serde(rename = "behaviorHints")]
    pub behavior_hints: StreamBehaviorHints,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamBehaviorHints {
    pub filename: String,
    #[serde(rename = "videoSize", skip_serializing_if = "Option::is_none")]
    pub video_size: Option<u64>,
    #[serde(rename = "bingeGroup")]
    pub binge_group: String,
    /// Tell web-Stremio's HTML5 player not to attempt direct play. The
    /// stream is an HTTP-served MKV/MP4 backed by NNTP segment fetches —
    /// browsers don't reliably handle MKV (esp. x265, multi-track audio,
    /// non-progressive seek), so web should kick to an external player.
    /// On desktop Stremio this flag is a no-op.
    #[serde(rename = "notWebReady")]
    pub not_web_ready: bool,
}

#[derive(Debug, Serialize)]
pub struct StreamsResponse {
    pub streams: Vec<Stream>,
    #[serde(rename = "cacheMaxAge")]
    pub cache_max_age: u64,
}
