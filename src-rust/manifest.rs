use crate::stremio::{BehaviorHintsManifest, Manifest, ManifestResource, ManifestResourceDetail};

/// Stable manifest id, used for binge-group prefixes too.
pub const ADDON_ID: &str = "io.sladg.tabellarius";
pub const ADDON_NAME: &str = "Tabellarius";

/// Build the addon manifest with a logo URL anchored at `base_url`
/// (`https://host` or `http://host`, depending on the request scheme).
/// Stream-only addon: no `catalog` / `meta` resources — the search +
/// browse flow uses Cinemeta, and Stremio calls our `/stream/...`
/// endpoint with whichever IMDB id the user picked.
pub fn manifest(base_url: &str) -> Manifest {
    Manifest {
        id: ADDON_ID,
        name: ADDON_NAME,
        description:
            "Self-hosted streaming addon for Stremio. Per-user access keys, language preferences, quality gates, and pre-flight stream validation.",
        logo: format!("{base_url}/logo.svg"),
        version: "0.0.4",
        resources: vec![ManifestResource::Detailed(ManifestResourceDetail {
            name: "stream",
            types: vec!["movie", "series"],
            // Limit to IMDB ids — that's what Cinemeta hands out.
            id_prefixes: Some(vec!["tt"]),
        })],
        types: vec!["movie", "series"],
        catalogs: Vec::new(),
        behavior_hints: Some(BehaviorHintsManifest {
            // No in-app configure UI; the operator edits config.toml. Stremio
            // doesn't render a "Configure" button when this is false.
            configurable: false,
            configuration_required: false,
        }),
    }
}
