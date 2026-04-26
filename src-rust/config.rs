use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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

/// Per-user (or default-bucket) configuration. All fields optional /
/// empty so a user override only specifies what differs from the
/// operator's `[defaults]`.
///
/// Resolution order at request time: `defaults` cloned, then any `Some`
/// field from the user's override wins; non-empty `Vec` fields
/// (indexers, nntp_servers, preferred_languages) replace as a unit;
/// empty inherits defaults.
///
/// **`key` field:** under `[users.<name>]` blocks, `key` is the actual
/// access secret used in the URL path. The map key (`<name>`) is the
/// friendly identifier shown in logs. Under `[defaults]` `key` is
/// ignored (no auth context). Empty `key` on a user → that user is
/// unreachable; `validate()` rejects this.
///
/// **NNTP caveat:** the live NNTP pool is built once at boot from
/// `defaults.nntp_servers`. Per-user `nntp_servers` is config-schema
/// valid but currently a no-op at runtime — boot warns if any user has
/// non-empty `nntp_servers` so misuse is visible. Per-user indexers
/// works fully (the NzbWebApiPool is constructed per-request).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserConfig {
    /// Access secret used in the URL path. Only meaningful inside a
    /// `[users.<name>]` block; ignored on `[defaults]`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,

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

    #[serde(
        rename = "preferredLanguages",
        alias = "preferred_languages",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub preferred_languages: Vec<String>,
}

impl UserConfig {
    /// Merge an override into this base. `override_`'s `Some` fields win;
    /// `None` falls back to base. Vec fields (indexers, nntp_servers,
    /// preferred_languages) are overridden as a unit — non-empty in the
    /// override replaces the base; empty inherits.
    pub fn merged_with(mut self, override_: &UserConfig) -> UserConfig {
        if !override_.indexers.is_empty() {
            self.indexers = override_.indexers.clone();
        }
        if !override_.nntp_servers.is_empty() {
            self.nntp_servers = override_.nntp_servers.clone();
        }
        if let Some(v) = override_.min_gbit_per_hour {
            self.min_gbit_per_hour = Some(v);
        }
        if let Some(v) = override_.max_gbit_per_hour {
            self.max_gbit_per_hour = Some(v);
        }
        if let Some(v) = override_.exclude_regex.clone() {
            self.exclude_regex = Some(v);
        }
        if let Some(v) = override_.validate_nzb_structure {
            self.validate_nzb_structure = Some(v);
        }
        if let Some(v) = override_.validate_nzb_availability {
            self.validate_nzb_availability = Some(v);
        }
        if let Some(v) = override_.streams_per_resolution {
            self.streams_per_resolution = Some(v);
        }
        if !override_.preferred_languages.is_empty() {
            self.preferred_languages = override_.preferred_languages.clone();
        }
        self
    }

    /// Trim values, drop empty rows, dedupe lists. Idempotent.
    pub fn normalize(&mut self) {
        self.key = self.key.trim().to_string();

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

        let mut seen = std::collections::HashSet::new();
        let normalized: Vec<String> = self
            .preferred_languages
            .iter()
            .map(|s| crate::util::normalize_language(s))
            .filter(|s| !s.is_empty() && seen.insert(s.clone()))
            .collect();
        self.preferred_languages = normalized;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AddonConfig {
    /// Hard guard against accidentally shipping an unauthenticated
    /// addon to a public endpoint. When true, boot fails if `users`
    /// is empty — the operator must explicitly configure at least one
    /// access key. When false (default), an empty `users` table means
    /// "no auth, addon routes mounted at root" (local-dev behavior).
    ///
    /// Recommended setting for any deployment beyond `127.0.0.1`.
    #[serde(
        rename = "requireAuth",
        alias = "require_auth",
        default,
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub require_auth: bool,

    /// Settings applied to all requests — both the per-user merge base
    /// and the standalone config when no token is in play (local dev /
    /// backward-compat with `users` empty). Holds the operator's shared
    /// indexers and NNTP servers; per-user overrides under
    /// `[users.<token>]` may shadow them.
    #[serde(default)]
    pub defaults: UserConfig,

    /// Per-token user configs. Map key = friendly name. The actual URL
    /// secret is the user's `key` field. Empty map = no auth required
    /// (current behavior; useful for local dev — see `require_auth`).
    #[serde(default)]
    pub users: HashMap<String, UserConfig>,
}

impl AddonConfig {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Run after deserializing. Recursively normalizes the defaults and
    /// every per-user override, plus trims user-map keys.
    pub fn normalize(&mut self) {
        self.defaults.normalize();
        for user in self.users.values_mut() {
            user.normalize();
        }

        // Trim user-map keys; drop empty-key entries (TOML doesn't really
        // allow empty keys, but a paranoid trim catches whitespace mistakes).
        let trimmed: HashMap<String, UserConfig> = std::mem::take(&mut self.users)
            .into_iter()
            .filter_map(|(k, v)| {
                let k = k.trim().to_string();
                if k.is_empty() {
                    None
                } else {
                    Some((k, v))
                }
            })
            .collect();
        self.users = trimmed;
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.defaults.indexers.is_empty() {
            return Err("[defaults] needs at least one indexer (url + apiKey)".into());
        }
        if self.defaults.nntp_servers.is_empty() {
            return Err("[defaults] needs at least one NNTP server".into());
        }
        // requireAuth = true is the operator's hard guard against
        // accidental open deploys. Refuse to start if it's set but no
        // users would actually be admitted.
        if self.require_auth && self.users.is_empty() {
            return Err(
                "requireAuth = true but [users] is empty — add at least one [users.<name>] with `key = \"...\"` or unset requireAuth"
                    .into(),
            );
        }
        // Every configured user must have a non-empty `key` — without
        // one they're unreachable (no URL path matches them).
        for (name, user) in &self.users {
            if user.key.is_empty() {
                return Err(format!(
                    "[users.{name}] is missing a `key` (the URL secret). Set `key = \"<random>\"` or remove the user."
                ));
            }
        }
        // Two users sharing a key is almost certainly a mistake (and
        // makes log attribution ambiguous). Reject at boot.
        let mut seen: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
        for (name, user) in &self.users {
            if let Some(prev) = seen.insert(user.key.as_str(), name.as_str()) {
                return Err(format!(
                    "[users.{name}] and [users.{prev}] share the same `key`; tokens must be unique"
                ));
            }
        }
        Ok(())
    }

    /// Resolve the effective `UserConfig` by friendly user name.
    ///
    /// - `Some(name)` with `name` in `users` → defaults merged with that
    ///   user's override.
    /// - `None` (auth disabled / local dev) → defaults as-is.
    /// - `Some(name)` not in `users` → defaults as-is (safety net; the
    ///   auth middleware should have rejected the request before this).
    pub fn resolve(&self, name: Option<&str>) -> UserConfig {
        match name.and_then(|n| self.users.get(n)) {
            Some(override_) => self.defaults.clone().merged_with(override_),
            None => self.defaults.clone(),
        }
    }

    /// True when the router should mount addon routes under
    /// `/:user_token` and reject bare-path requests. Either flag
    /// triggers it: an explicit `require_auth = true`, or a non-empty
    /// `users` map.
    pub fn requires_auth(&self) -> bool {
        self.require_auth || !self.users.is_empty()
    }

    /// Find the friendly user name whose `key` matches `token`.
    /// O(N) over `users`; trivial for the small-friends-group sizes
    /// this addon targets. Returns `None` if no match.
    pub fn user_for_key(&self, token: &str) -> Option<&str> {
        if token.is_empty() {
            return None;
        }
        self.users
            .iter()
            .find(|(_, cfg)| cfg.key == token)
            .map(|(name, _)| name.as_str())
    }
}

/// Load config from disk. Returns Ok(None) if the file doesn't exist;
/// Err only on read or parse failure.
pub fn load_from_disk(path: &Path) -> Result<Option<AddonConfig>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let mut cfg: AddonConfig =
                toml::from_str(&contents).with_context(|| format!("parsing {}", path.display()))?;
            cfg.normalize();
            Ok(Some(cfg))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
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
            defaults: UserConfig {
                indexers: vec![Indexer {
                    url: "https://x".into(),
                    api_key: "k".into(),
                }],
                nntp_servers: vec![NntpServer {
                    server: "nntps://u:p@x.com/4".into(),
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn normalize_drops_empty_indexer_rows() {
        let mut cfg = AddonConfig {
            defaults: UserConfig {
                indexers: vec![
                    Indexer {
                        url: "  https://a  ".into(),
                        api_key: "k".into(),
                    },
                    Indexer {
                        url: "".into(),
                        api_key: "".into(),
                    },
                    Indexer {
                        url: "https://b".into(),
                        api_key: "  ".into(),
                    },
                ],
                nntp_servers: vec![NntpServer {
                    server: "  nntps://x  ".into(),
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        cfg.normalize();
        assert_eq!(cfg.defaults.indexers.len(), 1);
        assert_eq!(cfg.defaults.indexers[0].url, "https://a");
        assert_eq!(cfg.defaults.nntp_servers[0].server, "nntps://x");
    }

    #[test]
    fn deserializes_camelcase_keys() {
        let json = r#"{
            "defaults": {
                "indexers":[{"url":"https://a","apiKey":"k"}],
                "nntpServers":[{"server":"nntps://x"}],
                "minGbitPerHour": 5,
                "maxGbitPerHour": 50,
                "validateNzbStructure": true,
                "validateNzbAvailability": true,
                "streamsPerResolution": 2
            }
        }"#;
        let cfg: AddonConfig = serde_json::from_str(json).expect("parses");
        assert_eq!(cfg.defaults.indexers.len(), 1);
        assert_eq!(cfg.defaults.min_gbit_per_hour, Some(5.0));
        assert_eq!(cfg.defaults.max_gbit_per_hour, Some(50.0));
        assert_eq!(cfg.defaults.validate_nzb_structure, Some(true));
        assert_eq!(cfg.defaults.streams_per_resolution, Some(2));
    }

    #[test]
    fn deserializes_per_user_overrides_from_toml() {
        let toml_input = r#"
            [defaults]
            min_gbit_per_hour = 5.0
            max_gbit_per_hour = 30.0
            preferred_languages = ["original", "english"]

            [[defaults.indexers]]
            url = "https://a"
            apiKey = "k"

            [[defaults.nntp_servers]]
            server = "nntps://x"

            [users.alice]
            preferred_languages = ["original", "english", "czech"]
            max_gbit_per_hour = 70.0

            [users.bob]
            preferred_languages = ["spanish"]
        "#;
        let cfg: AddonConfig = toml::from_str(toml_input).expect("parses toml");
        assert_eq!(cfg.defaults.indexers.len(), 1);
        assert_eq!(cfg.defaults.min_gbit_per_hour, Some(5.0));
        assert_eq!(cfg.users.len(), 2);
        assert!(cfg.users.contains_key("alice"));
        assert!(cfg.users.contains_key("bob"));
    }

    #[test]
    fn resolve_defaults_when_no_token() {
        let mut cfg = AddonConfig::empty();
        cfg.defaults.preferred_languages = vec!["english".into()];
        cfg.defaults.streams_per_resolution = Some(2);
        let resolved = cfg.resolve(None);
        assert_eq!(resolved.preferred_languages, vec!["english"]);
        assert_eq!(resolved.streams_per_resolution, Some(2));
    }

    #[test]
    fn resolve_merges_user_override() {
        let mut cfg = AddonConfig::empty();
        cfg.defaults.preferred_languages = vec!["english".into()];
        cfg.defaults.streams_per_resolution = Some(2);
        cfg.defaults.max_gbit_per_hour = Some(30.0);

        let mut alice = UserConfig::default();
        alice.preferred_languages = vec!["czech".into(), "english".into()];
        alice.max_gbit_per_hour = Some(70.0);
        cfg.users.insert("alice".into(), alice);

        let resolved = cfg.resolve(Some("alice"));
        // Alice's prefs win over defaults.
        assert_eq!(resolved.preferred_languages, vec!["czech", "english"]);
        assert_eq!(resolved.max_gbit_per_hour, Some(70.0));
        // Unspecified fields fall back to defaults.
        assert_eq!(resolved.streams_per_resolution, Some(2));
    }

    #[test]
    fn resolve_unknown_token_returns_defaults() {
        let mut cfg = AddonConfig::empty();
        cfg.defaults.preferred_languages = vec!["english".into()];
        let resolved = cfg.resolve(Some("nonexistent"));
        assert_eq!(resolved.preferred_languages, vec!["english"]);
    }

    #[test]
    fn requires_auth_reflects_users_map() {
        let mut cfg = AddonConfig::empty();
        assert!(!cfg.requires_auth());
        cfg.users.insert("alice".into(), UserConfig::default());
        assert!(cfg.requires_auth());
    }

    #[test]
    fn empty_preferred_languages_in_override_inherits_defaults() {
        // Bob's override has ONLY a streams_per_resolution change. His
        // preferred_languages is empty — should inherit defaults' list,
        // not become empty.
        let mut cfg = AddonConfig::empty();
        cfg.defaults.preferred_languages = vec!["english".into()];

        let mut bob = UserConfig::default();
        bob.streams_per_resolution = Some(1);
        cfg.users.insert("bob".into(), bob);

        let resolved = cfg.resolve(Some("bob"));
        assert_eq!(resolved.preferred_languages, vec!["english"]);
        assert_eq!(resolved.streams_per_resolution, Some(1));
    }
}
