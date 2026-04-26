//! Resolve a title's "original" language(s) via Cinemeta.
//!
//! Cinemeta (the metadata addon every Stremio install ships with) exposes
//! a `country` field per title — `"South Korea"`, `"Germany, France"`,
//! `"United States, United Kingdom"`, etc. — but not `language`. We map
//! the comma-separated country list to canonical language names so the
//! `"original"` token in `preferredLanguages` can be expanded at filter
//! time.
//!
//! Used by `build_streams` to translate `["english", "original"]` for a
//! Korean series into `["english", "korean"]` before calling
//! `filter_by_language`.
//!
//! Lookups are cached in-process — Cinemeta data doesn't change within
//! a session, and we typically hit the same series many times in a row.

use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use crate::util::normalize_language;

static CACHE: Lazy<Mutex<HashMap<String, Vec<String>>>> = Lazy::new(|| Mutex::new(HashMap::new()));

static COUNTRY_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#""country"\s*:\s*"([^"]+)""#).expect("country regex"));

/// Look up the original language(s) for a Stremio title.
///
/// Returns canonical language names (matching `util::normalize_language`
/// output) — e.g. `["korean"]` for Squid Game, `["english"]` for GoT
/// (US + UK), `["german", "french"]` for a German/French co-production.
///
/// Returns `Vec::new()` on any failure (network, parse, unknown country)
/// — caller should fall back to the rest of the preferred list. Failure
/// to resolve original-language is non-fatal; we'd rather over-include
/// streams than blanket-drop them.
pub async fn original_languages(
    client: &reqwest::Client,
    type_: &str,
    imdb_id: &str,
) -> Vec<String> {
    let key = format!("{type_}:{imdb_id}");
    if let Some(hit) = CACHE.lock().ok().and_then(|m| m.get(&key).cloned()) {
        return hit;
    }

    // Cinemeta only handles `movie` and `series`. Anything else: skip.
    if type_ != "movie" && type_ != "series" {
        return Vec::new();
    }

    let url = format!("https://v3-cinemeta.strem.io/meta/{type_}/{imdb_id}.json");
    let resp = match client
        .get(&url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
    {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!("[cinemeta] fetch failed ({imdb_id}): {err}");
            return Vec::new();
        }
    };

    let body = match resp.text().await {
        Ok(t) => t,
        Err(err) => {
            tracing::warn!("[cinemeta] body read failed ({imdb_id}): {err}");
            return Vec::new();
        }
    };

    let Some(country_blob) = COUNTRY_RE.captures(&body).and_then(|c| c.get(1)) else {
        tracing::debug!("[cinemeta] no country field for {imdb_id}");
        cache_set(&key, Vec::new());
        return Vec::new();
    };

    let langs = countries_to_languages(country_blob.as_str());
    if !langs.is_empty() {
        tracing::info!(
            "[cinemeta] {imdb_id} country=\"{}\" → languages={langs:?}",
            country_blob.as_str()
        );
    }
    cache_set(&key, langs.clone());
    langs
}

fn cache_set(key: &str, value: Vec<String>) {
    if let Ok(mut m) = CACHE.lock() {
        m.insert(key.to_string(), value);
    }
}

/// Map a Cinemeta `country` blob (`"United States, United Kingdom"`) to a
/// deduped, ordered list of canonical language names.
pub fn countries_to_languages(blob: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in blob.split(',') {
        let trimmed = raw.trim();
        if let Some(lang) = country_to_language(trimmed) {
            let canonical = normalize_language(lang);
            if !out.contains(&canonical) {
                out.push(canonical);
            }
        }
    }
    out
}

/// Country (English exonym, as Cinemeta emits it) → primary language.
/// Returns `None` for countries we don't have a sensible single-language
/// mapping for; the caller silently skips those entries.
///
/// Coverage targets the top ~30 countries that actually show up in
/// usenet release listings. Adding a new entry is a one-line change.
fn country_to_language(country: &str) -> Option<&'static str> {
    match country {
        "United States" | "United Kingdom" | "Canada" | "Australia" | "New Zealand" | "Ireland"
        | "South Africa" => Some("english"),
        "Spain" | "Mexico" | "Argentina" | "Colombia" | "Chile" | "Peru" | "Venezuela" | "Cuba"
        | "Uruguay" => Some("spanish"),
        "France" | "Belgium" | "Luxembourg" | "Senegal" | "Ivory Coast" => Some("french"),
        "Germany" | "Austria" | "Switzerland" => Some("german"),
        "Italy" => Some("italian"),
        "Portugal" | "Brazil" => Some("portuguese"),
        "Russia" => Some("russian"),
        "China" | "Hong Kong" | "Taiwan" | "Singapore" => Some("chinese"),
        "Japan" => Some("japanese"),
        "South Korea" | "North Korea" => Some("korean"),
        "Netherlands" => Some("dutch"),
        "Sweden" => Some("swedish"),
        "Norway" => Some("norwegian"),
        "Denmark" => Some("danish"),
        "Finland" => Some("finnish"),
        "Poland" => Some("polish"),
        "Czech Republic" | "Czechia" => Some("czech"),
        "Slovakia" => Some("slovak"),
        "Hungary" => Some("hungarian"),
        "Romania" => Some("romanian"),
        "Greece" => Some("greek"),
        "Turkey" => Some("turkish"),
        "Israel" => Some("hebrew"),
        "India" => Some("hindi"),
        "Thailand" => Some("thai"),
        "Vietnam" => Some("vietnamese"),
        "Indonesia" => Some("indonesian"),
        "Ukraine" => Some("ukrainian"),
        "Saudi Arabia" | "Egypt" | "United Arab Emirates" | "Morocco" | "Tunisia" => Some("arabic"),
        _ => None,
    }
}

/// True if any item in `preferred` is the literal `"original"` sentinel
/// (case-insensitive). Cheap pre-check before doing the Cinemeta fetch.
pub fn wants_original(preferred: &[String]) -> bool {
    preferred.iter().any(|s| s.eq_ignore_ascii_case("original"))
}

/// Expand the `"original"` token in `preferred` with `originals`, returning
/// a deduped list with `"original"` removed. Order is preserved; `originals`
/// are inserted where the sentinel appeared.
pub fn expand_original(preferred: &[String], originals: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(preferred.len() + originals.len());
    for entry in preferred {
        if entry.eq_ignore_ascii_case("original") {
            for o in originals {
                if !out.contains(o) {
                    out.push(o.clone());
                }
            }
        } else if !out.contains(entry) {
            out.push(entry.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn country_blob_single() {
        assert_eq!(countries_to_languages("South Korea"), vec!["korean"]);
        assert_eq!(countries_to_languages("Germany"), vec!["german"]);
    }

    #[test]
    fn country_blob_multi_dedupes() {
        // US + UK both map to english — single entry expected.
        assert_eq!(
            countries_to_languages("United States, United Kingdom"),
            vec!["english"]
        );
    }

    #[test]
    fn country_blob_multi_keeps_distinct() {
        assert_eq!(
            countries_to_languages("Germany, France"),
            vec!["german", "french"]
        );
    }

    #[test]
    fn country_blob_unknown_skipped() {
        assert_eq!(countries_to_languages("Wakanda"), Vec::<String>::new());
        // Unknown alongside known: only known survives.
        assert_eq!(countries_to_languages("Wakanda, Japan"), vec!["japanese"]);
    }

    #[test]
    fn wants_original_detects_token() {
        assert!(wants_original(&["original".into()]));
        assert!(wants_original(&["english".into(), "Original".into()]));
        assert!(!wants_original(&["english".into(), "spanish".into()]));
    }

    #[test]
    fn expand_replaces_original_token() {
        let pref = vec!["english".into(), "original".into()];
        let originals = vec!["korean".into()];
        assert_eq!(
            expand_original(&pref, &originals),
            vec!["english", "korean"]
        );
    }

    #[test]
    fn expand_dedupes_when_original_overlaps_explicit() {
        // User wrote `english, original`, show is US (→ english).
        // Result should be just `english`, not `english, english`.
        let pref = vec!["english".into(), "original".into()];
        let originals = vec!["english".into()];
        assert_eq!(expand_original(&pref, &originals), vec!["english"]);
    }

    #[test]
    fn expand_handles_empty_originals() {
        // Cinemeta lookup failed (returned empty). Result drops the
        // `"original"` token without adding anything.
        let pref = vec!["english".into(), "original".into()];
        let out = expand_original(&pref, &[]);
        assert_eq!(out, vec!["english"]);
    }

    #[test]
    fn expand_inserts_at_token_position() {
        // `original` between two explicit langs — originals slot in there,
        // not at the start or end.
        let pref = vec!["english".into(), "original".into(), "spanish".into()];
        let originals = vec!["korean".into()];
        assert_eq!(
            expand_original(&pref, &originals),
            vec!["english", "korean", "spanish"]
        );
    }

    #[test]
    fn expand_multi_country_originals() {
        // German/French co-production: both languages count as "original".
        let pref = vec!["original".into()];
        let originals = vec!["german".into(), "french".into()];
        assert_eq!(expand_original(&pref, &originals), vec!["german", "french"]);
    }
}
