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
            // No in-app configure UI anymore; the operator edits config.toml
            // directly. Stremio renders no "Configure" button when this is
            // false, which is what we want.
            configurable: false,
            configuration_required: false,
        }),
    }
}

