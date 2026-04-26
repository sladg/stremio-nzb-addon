use once_cell::sync::Lazy;
use regex::{Regex, RegexBuilder};

use crate::nzb_api::Item;
use crate::parse_title::parse;
use crate::util::{is_multi_language_tag, normalize_language};

static LITERAL_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/(.+)/([gimsuy]*)$").expect("literal regex"));

pub fn compile_exclude_regex(input: Option<&str>) -> Option<Regex> {
    let trimmed = input?.trim();
    if trimmed.is_empty() {
        return None;
    }

    let (pattern, case_insensitive) = if let Some(caps) = LITERAL_RE.captures(trimmed) {
        let pat = caps.get(1)?.as_str().to_string();
        let flags = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        (pat, flags.contains('i'))
    } else {
        (trimmed.to_string(), true)
    };

    match RegexBuilder::new(&pattern)
        .case_insensitive(case_insensitive)
        .build()
    {
        Ok(re) => Some(re),
        Err(err) => {
            tracing::warn!("[contentFilter] invalid regex \"{trimmed}\": {err}");
            None
        }
    }
}

pub fn filter_by_title_regex(items: Vec<Item>, exclude_regex: Option<&str>) -> Vec<Item> {
    let Some(regex) = compile_exclude_regex(exclude_regex) else {
        return items;
    };

    let total = items.len();
    let mut dropped = 0usize;
    let kept: Vec<Item> = items
        .into_iter()
        .filter(|item| {
            if regex.is_match(&item.title) {
                dropped += 1;
                false
            } else {
                true
            }
        })
        .collect();

    if dropped > 0 {
        tracing::info!(
            "[contentFilter] excluded {dropped} of {total} items via regex {}",
            regex.as_str()
        );
    }
    kept
}

/// Filter items down to those whose detected audio language matches the
/// user's `preferredLanguages` config. Empty list = no-op (no preference).
///
/// Pass-through rules (in order):
/// 1. **Untagged** (parser returned `None`) — kept. Most English releases
///    don't tag the language in the filename; treating untagged as a
///    drop would slaughter the typical English-speaker's stream list.
/// 2. **Multi-language tags** (`MULTi`, `DUAL`, `MULTI.SUB`, etc.) — kept.
///    These releases ship multiple audio tracks and almost always
///    include the user's preferred language.
/// 3. **Detected language matches a preferred language** (case-insensitive
///    via `normalize_language`) — kept.
/// 4. **Anything else** — dropped.
pub fn filter_by_language(items: Vec<Item>, preferred: &[String]) -> Vec<Item> {
    if preferred.is_empty() {
        return items;
    }
    let total = items.len();
    let mut dropped: Vec<String> = Vec::new();

    let kept: Vec<Item> = items
        .into_iter()
        .filter(|item| {
            let parsed_lang = parse(&item.title).language;
            match parsed_lang {
                None => true, // untagged — pass through
                Some(raw) => {
                    let norm = normalize_language(&raw);
                    if is_multi_language_tag(&norm) {
                        true
                    } else if preferred.iter().any(|p| p == &norm) {
                        true
                    } else {
                        dropped.push(item.title.clone());
                        false
                    }
                }
            }
        })
        .collect();

    if !dropped.is_empty() {
        let preview = dropped
            .iter()
            .take(3)
            .map(|s| format!("\"{s}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let suffix = if dropped.len() > 3 { "..." } else { "" };
        tracing::info!(
            "[languageFilter] excluded {} of {} items (preferred: {}): {preview}{suffix}",
            dropped.len(),
            total,
            preferred.join(", "),
        );
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str) -> Item {
        Item {
            title: title.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn empty_or_none_compiles_to_none() {
        assert!(compile_exclude_regex(None).is_none());
        assert!(compile_exclude_regex(Some("")).is_none());
        assert!(compile_exclude_regex(Some("   ")).is_none());
    }

    #[test]
    fn bare_pattern_is_case_insensitive() {
        let re = compile_exclude_regex(Some("HDR")).expect("compiles");
        assert!(re.is_match("Movie.HDR.mkv"));
        assert!(re.is_match("Movie.hdr.mkv")); // case-insensitive default
    }

    #[test]
    fn literal_form_respects_flags() {
        // /pat/ alone -> case-sensitive
        let re = compile_exclude_regex(Some("/HDR/")).expect("compiles");
        assert!(re.is_match("Movie.HDR.mkv"));
        assert!(!re.is_match("Movie.hdr.mkv"));

        // /pat/i -> case-insensitive
        let re_i = compile_exclude_regex(Some("/HDR/i")).expect("compiles");
        assert!(re_i.is_match("Movie.hdr.mkv"));
    }

    #[test]
    fn literal_form_supports_alternation() {
        let re = compile_exclude_regex(Some("/HDR|DV/i")).expect("compiles");
        assert!(re.is_match("Show.HDR.mkv"));
        assert!(re.is_match("Show.DV.mkv"));
        assert!(!re.is_match("Show.SDR.mkv"));
    }

    #[test]
    fn invalid_regex_returns_none() {
        assert!(compile_exclude_regex(Some("/[invalid/")).is_none());
    }

    #[test]
    fn filter_drops_matching() {
        let items = vec![
            item("Movie.1080p.HDR.mkv"),
            item("Movie.1080p.SDR.mkv"),
            item("Movie.2160p.DV.mkv"),
        ];
        let out = filter_by_title_regex(items, Some("/HDR|DV/i"));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "Movie.1080p.SDR.mkv");
    }

    #[test]
    fn filter_no_op_when_regex_invalid() {
        let items = vec![item("a"), item("b")];
        let out = filter_by_title_regex(items, Some("/[bad/"));
        assert_eq!(out.len(), 2);
    }

    // ---- filter_by_language ----

    #[test]
    fn language_filter_empty_preferred_is_noop() {
        let items = vec![
            item("Movie.SPANISH.1080p.WEB-DL.x265-RARBG"),
            item("Movie.1080p.WEB-DL.x265-RARBG"),
        ];
        let out = filter_by_language(items, &[]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn language_filter_keeps_preferred_drops_others() {
        let items = vec![
            item("Movie.SPANISH.1080p.WEB-DL.x265-RARBG"),
            item("Movie.FRENCH.1080p.WEB-DL.x265-RARBG"),
            item("Movie.GERMAN.1080p.WEB-DL.x265-RARBG"),
        ];
        let preferred = vec!["spanish".to_string()];
        let out = filter_by_language(items, &preferred);
        assert_eq!(out.len(), 1);
        assert!(out[0].title.contains("SPANISH"));
    }

    #[test]
    fn language_filter_passes_untagged() {
        // "Movie.2024.1080p.WEB-DL.x265-RARBG" has no language tag —
        // torrent-name-parser returns None for language. Should pass
        // through regardless of preferred list.
        let items = vec![item("Movie.2024.1080p.WEB-DL.x265-RARBG")];
        let preferred = vec!["english".to_string()];
        let out = filter_by_language(items, &preferred);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn language_filter_passes_iso_code_in_preferred() {
        // User configured "en" — should normalize to "english" and match
        // English-tagged releases.
        let items = vec![
            item("Movie.ENGLISH.1080p.WEB-DL.x265-RARBG"),
            item("Movie.SPANISH.1080p.WEB-DL.x265-RARBG"),
        ];
        let preferred = vec![normalize_language("en")];
        let out = filter_by_language(items, &preferred);
        assert_eq!(out.len(), 1);
        assert!(out[0].title.contains("ENGLISH"));
    }
}
