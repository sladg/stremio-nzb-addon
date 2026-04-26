use crate::stremio::{BehaviorHintsManifest, Manifest, ManifestResource, ManifestResourceDetail};

/// Stable manifest id, used for binge-group prefixes too.
pub const ADDON_ID: &str = "io.sladg.tabellarius";
pub const ADDON_NAME: &str = "Tabellarius";

/// Compute the short host label for display in manifest + stream names,
/// derived from the request's `base_url`. Helps disambiguate the same
/// addon installed via multiple URLs (e.g. LAN IP vs reverse-proxy host).
///
/// Strips scheme and port. For DNS hostnames returns the leftmost label
/// (so `addon.example.com` → `addon`). For raw IPv4 addresses, returns
/// the address as-is. IPv6 in brackets is preserved verbatim too.
pub fn host_label(base_url: &str) -> String {
    let after_scheme = base_url
        .strip_prefix("https://")
        .or_else(|| base_url.strip_prefix("http://"))
        .unwrap_or(base_url);
    let host_with_port = after_scheme.split('/').next().unwrap_or(after_scheme);

    // IPv6 literal: keep brackets contents intact, drop a trailing :port.
    let host = if host_with_port.starts_with('[') {
        if let Some(end) = host_with_port.find(']') {
            &host_with_port[..=end]
        } else {
            host_with_port
        }
    } else if let Some(idx) = host_with_port.rfind(':') {
        // Non-IPv6: strip a trailing :port iff the suffix is all digits.
        let port_part = &host_with_port[idx + 1..];
        if !port_part.is_empty() && port_part.chars().all(|c| c.is_ascii_digit()) {
            &host_with_port[..idx]
        } else {
            host_with_port
        }
    } else {
        host_with_port
    };

    // IPv4: keep as-is. Anything else is a DNS name; show the leftmost
    // label only to avoid long FQDNs in titles.
    let is_ipv4 = host.split('.').all(|part| part.parse::<u8>().is_ok());
    if is_ipv4 {
        host.to_string()
    } else if let Some(first) = host.split('.').next() {
        first.to_string()
    } else {
        host.to_string()
    }
}

/// Build the addon manifest with a logo URL anchored at `base_url`
/// (`https://host` or `http://host`, depending on the request scheme).
/// Stream-only addon: no `catalog` / `meta` resources — the search +
/// browse flow uses Cinemeta, and Stremio calls our `/stream/...`
/// endpoint with whichever IMDB id the user picked.
pub fn manifest(base_url: &str) -> Manifest {
    let label = host_label(base_url);
    Manifest {
        id: ADDON_ID,
        name: format!("{ADDON_NAME} ({label})"),
        description:
            "Self-hosted streaming addon for Stremio. Per-user access keys, language preferences, quality gates, and pre-flight stream validation.",
        logo: format!("{base_url}/logo.svg"),
        version: "0.0.6",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_label_extracts_leftmost_dns_label() {
        assert_eq!(host_label("https://addon.example.com"), "addon");
        assert_eq!(host_label("https://addon.example.com/"), "addon");
        assert_eq!(host_label("https://addon.example.com/manifest.json"), "addon");
        assert_eq!(host_label("https://addon.sub.example.com"), "addon");
    }

    #[test]
    fn host_label_keeps_ipv4_as_is() {
        assert_eq!(host_label("http://192.168.1.1"), "192.168.1.1");
        assert_eq!(host_label("http://192.168.1.1:3000"), "192.168.1.1");
        assert_eq!(host_label("http://192.168.1.1:3000/manifest.json"), "192.168.1.1");
    }

    #[test]
    fn host_label_strips_port_for_dns() {
        assert_eq!(host_label("http://addon.example.com:3000"), "addon");
        assert_eq!(host_label("http://localhost:3000"), "localhost");
        assert_eq!(host_label("http://localhost"), "localhost");
    }

    #[test]
    fn host_label_handles_ipv6_brackets() {
        assert_eq!(host_label("http://[::1]:3000"), "[::1]");
        assert_eq!(host_label("http://[2001:db8::1]:8080"), "[2001:db8::1]");
    }

    #[test]
    fn host_label_handles_missing_scheme() {
        // Defensive: if a caller forgets the scheme, still produce a label.
        assert_eq!(host_label("addon.example.com:3000"), "addon");
        assert_eq!(host_label("192.168.1.1:3000"), "192.168.1.1");
    }

    #[test]
    fn manifest_name_includes_host_label() {
        let m = manifest("https://addon.example.com");
        assert_eq!(m.name, "Tabellarius (addon)");
        let m = manifest("http://192.168.1.1:3000");
        assert_eq!(m.name, "Tabellarius (192.168.1.1)");
    }
}
