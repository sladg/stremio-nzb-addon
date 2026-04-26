use crate::stremio::{
    BehaviorHintsManifest, CatalogExtra, Manifest, ManifestCatalog, ManifestResource,
    ManifestResourceDetail,
};

/// Stable manifest id, used for binge-group prefixes too.
pub const ADDON_ID: &str = "io.sladg.nzb";
pub const ADDON_NAME: &str = "NZB";

pub fn catalog() -> ManifestCatalog {
    ManifestCatalog {
        id: "nzb",
        name: "NZB Search Results",
        type_: "tv",
        extra: vec![CatalogExtra { name: "search" }],
    }
}

pub fn manifest() -> Manifest {
    let cat = catalog();
    Manifest {
        id: "io.sladg.nzb",
        name: "NZB",
        description: "Usenet streams from your NZB indexer(s)",
        logo: "https://raw.githubusercontent.com/nzbget/nzbget/5e26d52d706f129769e1d620a595c78498ca8cff/webui/img/favicon-256x256.png",
        version: "3.0.0",
        resources: vec![
            ManifestResource::Simple("catalog"),
            ManifestResource::Detailed(ManifestResourceDetail {
                name: "meta",
                types: vec![cat.type_],
                id_prefixes: Some(vec![cat.id]),
            }),
            ManifestResource::Detailed(ManifestResourceDetail {
                name: "stream",
                types: vec!["movie", "series", "tv"],
                id_prefixes: Some(vec!["tt", cat.id]),
            }),
        ],
        types: vec!["movie", "series", "tv"],
        catalogs: vec![cat],
        behavior_hints: Some(BehaviorHintsManifest {
            configurable: true,
            // Config now lives server-side; Stremio doesn't need to gather it.
            configuration_required: false,
        }),
    }
}

pub struct ConfigField {
    pub key: &'static str,
    pub kind: FieldKind,
    pub title: &'static str,
    pub placeholder: Option<&'static str>,
    pub required: bool,
    pub array_options: Option<Vec<ConfigField>>,
}

pub enum FieldKind {
    Text,
    Password,
    Number,
    Array,
    Checkbox,
    /// Comma-separated text input. Renders the live `Vec<String>` value
    /// joined by `, ` and serializes back into a `string[]` in the JSON
    /// payload. Used for `preferredLanguages`.
    StringList,
}

pub fn config_fields() -> Vec<ConfigField> {
    vec![
        ConfigField {
            key: "indexers",
            kind: FieldKind::Array,
            title: "Indexers",
            placeholder: None,
            required: true,
            array_options: Some(vec![
                ConfigField {
                    key: "url",
                    kind: FieldKind::Text,
                    title: "Indexer URL",
                    placeholder: Some("https://api.example.com"),
                    required: true,
                    array_options: None,
                },
                ConfigField {
                    key: "apiKey",
                    kind: FieldKind::Password,
                    title: "Indexer API key",
                    placeholder: Some("abcd1234efgh5678ijkl9012mnop3456"),
                    required: true,
                    array_options: None,
                },
            ]),
        },
        ConfigField {
            key: "nntpServers",
            kind: FieldKind::Array,
            title: "NNTP Servers",
            placeholder: None,
            required: true,
            array_options: Some(vec![ConfigField {
                key: "server",
                kind: FieldKind::Text,
                title: "URL",
                placeholder: Some("nntps://username:password@example.com/4"),
                required: true,
                array_options: None,
            }]),
        },
        ConfigField {
            key: "minGbitPerHour",
            kind: FieldKind::Number,
            title: "Min Bandwidth (Gbit/hour)",
            placeholder: Some("Optional: drop low-bitrate items below this threshold"),
            required: false,
            array_options: None,
        },
        ConfigField {
            key: "maxGbitPerHour",
            kind: FieldKind::Number,
            title: "Max Bandwidth (Gbit/hour)",
            placeholder: Some(
                "Optional: 25 (SD), 50 (HD), 100 (4K) - filters high-bandwidth streams",
            ),
            required: false,
            array_options: None,
        },
        ConfigField {
            key: "excludeRegex",
            kind: FieldKind::Text,
            title: "Exclude title regex",
            placeholder: Some("Optional: e.g. /HDR|DV/i"),
            required: false,
            array_options: None,
        },
        ConfigField {
            key: "validateNzbStructure",
            kind: FieldKind::Checkbox,
            title: "Validate NZB structure (~50ms/result, fetches each NZB)",
            placeholder: None,
            required: false,
            array_options: None,
        },
        ConfigField {
            key: "validateNzbAvailability",
            kind: FieldKind::Checkbox,
            title: "Validate NZB availability via NNTP (~150ms/result, BODY-probes canary articles)",
            placeholder: None,
            required: false,
            array_options: None,
        },
        ConfigField {
            key: "streamsPerResolution",
            kind: FieldKind::Number,
            title: "Streams per resolution",
            placeholder: Some("Optional: cap entries per resolution bucket (default 1)"),
            required: false,
            array_options: None,
        },
        ConfigField {
            key: "preferredLanguages",
            kind: FieldKind::StringList,
            title: "Preferred languages",
            placeholder: Some(
                "Optional: e.g. english, original, spanish — \"original\" matches the show's native language",
            ),
            required: false,
            array_options: None,
        },
    ]
}
