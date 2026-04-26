use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Indexer {
    pub url: String,
    #[serde(rename = "apiKey", alias = "api_key")]
    pub api_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NntpServer {
    pub server: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AddonConfig {
    #[serde(default)]
    pub indexers: Vec<Indexer>,

    #[serde(rename = "nntpServers", alias = "nntp_servers", default)]
    pub nntp_servers: Vec<NntpServer>,

    #[serde(
        rename = "minGbitPerHour",
        alias = "min_gbit_per_hour",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub min_gbit_per_hour: Option<f64>,

    #[serde(
        rename = "maxGbitPerHour",
        alias = "max_gbit_per_hour",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub max_gbit_per_hour: Option<f64>,

    #[serde(
        rename = "excludeRegex",
        alias = "exclude_regex",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub exclude_regex: Option<String>,

    #[serde(
        rename = "validateNzbStructure",
        alias = "validate_nzb_structure",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub validate_nzb_structure: Option<bool>,

    #[serde(
        rename = "validateNzbAvailability",
        alias = "validate_nzb_availability",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub validate_nzb_availability: Option<bool>,

    #[serde(
        rename = "streamsPerResolution",
        alias = "streams_per_resolution",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub streams_per_resolution: Option<u32>,

    /// User-preferred audio languages. Empty = no preference (current
    /// behavior; no filter applied). Stored canonical-form lowercased
    /// names ("english", "spanish") — see `util::normalize_language`.
    /// `filter_by_language` matches against detected stream languages
    /// using the same normalization, so config and parser agree.
    #[serde(
        rename = "preferredLanguages",
        alias = "preferred_languages",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub preferred_languages: Vec<String>,
}

impl AddonConfig {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Trim values + drop fully-empty rows. Run before persisting.
    pub fn normalize(&mut self) {
        for ix in &mut self.indexers {
            ix.url = ix.url.trim().to_string();
            ix.api_key = ix.api_key.trim().to_string();
        }
        self.indexers
            .retain(|ix| !ix.url.is_empty() && !ix.api_key.is_empty());

        for s in &mut self.nntp_servers {
            s.server = s.server.trim().to_string();
        }
        self.nntp_servers.retain(|s| !s.server.is_empty());

        if let Some(s) = self.exclude_regex.as_mut() {
            *s = s.trim().to_string();
        }
        if self.exclude_regex.as_deref() == Some("") {
            self.exclude_regex = None;
        }

        // Canonicalize languages so `english`, `English`, `EN` all collapse
        // to `english` — the form `torrent-name-parser` emits at runtime.
        // Drops empties and dedupes while preserving user-supplied order
        // (first-listed wins on tie).
        let mut seen = std::collections::HashSet::new();
        let normalized: Vec<String> = self
            .preferred_languages
            .iter()
            .map(|s| crate::util::normalize_language(s))
            .filter(|s| !s.is_empty() && seen.insert(s.clone()))
            .collect();
        self.preferred_languages = normalized;
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.indexers.is_empty() {
            return Err("at least one indexer (url + apiKey) is required".into());
        }
        if self.nntp_servers.is_empty() {
            return Err("at least one NNTP server is required".into());
        }
        Ok(())
    }
}

/// Load config from disk. Returns Ok(None) if the file doesn't exist;
/// Err only on read or parse failure.
pub fn load_from_disk(path: &Path) -> Result<Option<AddonConfig>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let cfg: AddonConfig = toml::from_str(&contents)
                .with_context(|| format!("parsing {}", path.display()))?;
            Ok(Some(cfg))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// Atomic write: serialize → temp file in same dir → rename.
pub async fn save_to_disk(path: &Path, cfg: &AddonConfig) -> Result<()> {
    let serialized = toml::to_string_pretty(cfg).context("serializing config to toml")?;

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("config.toml");
    let tmp = dir.join(format!(".{file_name}.tmp"));

    tokio::fs::write(&tmp, serialized.as_bytes())
        .await
        .with_context(|| format!("writing temp file {}", tmp.display()))?;
    tokio::fs::rename(&tmp, path)
        .await
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_requires_indexer_and_nntp() {
        let cfg = AddonConfig::empty();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_passes_with_minimum_inputs() {
        let cfg = AddonConfig {
            indexers: vec![Indexer {
                url: "https://x".into(),
                api_key: "k".into(),
            }],
            nntp_servers: vec![NntpServer {
                server: "nntps://u:p@x.com/4".into(),
            }],
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn normalize_drops_empty_indexer_rows() {
        let mut cfg = AddonConfig {
            indexers: vec![
                Indexer { url: "  https://a  ".into(), api_key: "k".into() },
                Indexer { url: "".into(), api_key: "".into() },
                Indexer { url: "https://b".into(), api_key: "  ".into() },
            ],
            nntp_servers: vec![NntpServer { server: "  nntps://x  ".into() }],
            ..Default::default()
        };
        cfg.normalize();
        assert_eq!(cfg.indexers.len(), 1);
        assert_eq!(cfg.indexers[0].url, "https://a");
        assert_eq!(cfg.nntp_servers[0].server, "nntps://x");
    }

    #[test]
    fn normalize_clears_empty_exclude_regex() {
        let mut cfg = AddonConfig {
            exclude_regex: Some("   ".into()),
            ..Default::default()
        };
        cfg.normalize();
        assert!(cfg.exclude_regex.is_none());
    }

    #[test]
    fn deserializes_camelcase_keys() {
        let json = r#"{
            "indexers":[{"url":"https://a","apiKey":"k"}],
            "nntpServers":[{"server":"nntps://x"}],
            "minGbitPerHour": 5,
            "maxGbitPerHour": 50,
            "validateNzbStructure": true,
            "validateNzbAvailability": true,
            "streamsPerResolution": 2
        }"#;
        let cfg: AddonConfig = serde_json::from_str(json).expect("parses");
        assert_eq!(cfg.indexers.len(), 1);
        assert_eq!(cfg.min_gbit_per_hour, Some(5.0));
        assert_eq!(cfg.max_gbit_per_hour, Some(50.0));
        assert_eq!(cfg.validate_nzb_structure, Some(true));
        assert_eq!(cfg.validate_nzb_availability, Some(true));
        assert_eq!(cfg.streams_per_resolution, Some(2));
    }

    #[test]
    fn deserializes_snake_case_aliases_from_toml() {
        let toml_input = r#"
            min_gbit_per_hour = 5.0
            max_gbit_per_hour = 50.0
            streams_per_resolution = 2
            validate_nzb_availability = true
            [[indexers]]
            url = "https://a"
            apiKey = "k"
            [[nntp_servers]]
            server = "nntps://x"
        "#;
        let cfg: AddonConfig = toml::from_str(toml_input).expect("parses toml");
        assert_eq!(cfg.indexers.len(), 1);
        assert_eq!(cfg.min_gbit_per_hour, Some(5.0));
        assert_eq!(cfg.streams_per_resolution, Some(2));
    }
}
