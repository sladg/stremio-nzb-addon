//! Shared utilities. Tiny + dependency-free.
//!
//! Putting cred-handling helpers here keeps the redaction logic in one
//! place — anything that touches a URL with secrets in it should route
//! through `redact_url` before reaching a log line, an error message, or
//! a JSON response body.

use once_cell::sync::Lazy;
use regex::Regex;
use url::Url;

/// Strip credentials from a URL for safe logging.
///
/// Removes:
/// - Userinfo (`user:pass@` — NNTP server URLs)
/// - Query string entirely (Newznab `?apikey=…`, indexer `?r=…&i=…`,
///   any other potentially-sensitive params)
///
/// Keeps the scheme, host, port, and path so logs remain useful for
/// correlating which server / endpoint was involved.
///
/// On parse failure returns `<invalid-url>` rather than the original
/// string — defensive default avoids accidental leak when the input
/// is structurally weird.
///
/// Examples:
///   `nntps://u:p@news.example.com:563/4` → `nntps://news.example.com:563/4`
///   `https://api.nzbplanet.net/getnzb/X.nzb?i=U&r=APIKEY` → `https://api.nzbplanet.net/getnzb/X.nzb`
pub fn redact_url(input: &str) -> String {
    let Ok(mut url) = Url::parse(input) else {
        return "<invalid-url>".to_string();
    };
    let _ = url.set_password(None);
    let _ = url.set_username("");
    url.set_query(None);
    url.set_fragment(None);
    url.to_string()
}

/// Normalize a user-supplied or parsed language token to the canonical
/// form `torrent-name-parser` emits (lowercased English name).
///
/// Behavior:
/// - Trims and lowercases.
/// - Maps ISO 639-1 codes (`en`, `es`, …) up to full names (`english`,
///   `spanish`, …) so users can write either in `preferredLanguages`.
/// - Unknown inputs pass through unchanged (in lowercase). If torrent-
///   name-parser starts emitting a language we don't have in the alias
///   table, the user can still match it by typing the same string.
///
/// This is the single source of truth used by both the config-side
/// normalization and the filter-side comparison, so a parsed `english`
/// always matches a configured `EN`.
pub fn normalize_language(input: &str) -> String {
    let lower = input.trim().to_ascii_lowercase();
    // Sorted by what shows up most in real-world release titles.
    const ALIASES: &[(&str, &str)] = &[
        ("en", "english"),
        ("es", "spanish"),
        ("ja", "japanese"),
        ("fr", "french"),
        ("de", "german"),
        ("it", "italian"),
        ("pt", "portuguese"),
        ("ru", "russian"),
        ("zh", "chinese"),
        ("ko", "korean"),
        ("nl", "dutch"),
        ("pl", "polish"),
        ("cs", "czech"),
        ("sk", "slovak"),
        ("sv", "swedish"),
        ("no", "norwegian"),
        ("da", "danish"),
        ("fi", "finnish"),
        ("hu", "hungarian"),
        ("tr", "turkish"),
        ("ar", "arabic"),
        ("he", "hebrew"),
        ("hi", "hindi"),
    ];
    for (code, name) in ALIASES {
        if lower == *code {
            return (*name).to_string();
        }
    }
    lower
}

/// Multi-language tags from `torrent-name-parser`. A release flagged with
/// any of these typically contains multiple audio tracks including the
/// user's preferred language; `filter_by_language` lets them through
/// regardless of the configured preference list.
pub fn is_multi_language_tag(s: &str) -> bool {
    matches!(
        s,
        "multi" | "multi.sub" | "multisub" | "dual" | "multilingual"
    )
}

/// Detect the audio language from a release title.
///
/// `torrent-name-parser`'s built-in language detection is very narrow
/// (only matches `MULTi`, `FRENCH`, `TRUEFRENCH`, `rus.eng`, `US`, `VFF`)
/// — SPANISH/GERMAN/JAPANESE/etc. all return `None`. This detector covers
/// the broader set actually seen in real-world release names. Returns
/// the canonical lowercase English name (matching `normalize_language`).
///
/// Order matters: multi-language tags match first so a "MULTi.FRENCH"
/// release is reported as `multi` (multi-track pack) rather than
/// `french`. The filter then passes it through regardless of the user's
/// preferred-language list, which is the desired behavior for packs.
///
/// `None` if no language tag is detected — typical for English releases
/// that don't bother tagging the language explicitly. Callers should
/// treat untagged streams as a pass-through.
pub fn detect_language(title: &str) -> Option<String> {
    static PATTERNS: Lazy<Vec<(Regex, &'static str)>> = Lazy::new(|| {
        let mk = |re: &str, name: &'static str| (Regex::new(re).unwrap(), name);
        vec![
            // Multi-language packs first — `MULTi.FRENCH` should report
            // `multi`, not `french`. Filter passes packs through regardless
            // of the user's preferred-language list.
            mk(
                r"(?i)\b(?:multi\.sub|multisub|multilingual|multi|dual)\b",
                "multi",
            ),
            // Full English names + unambiguous variants. ISO codes (`en`,
            // `de`) are intentionally excluded here — they false-positive
            // on common substrings like `EN` in `EN.US` or `DE` in random
            // group names.
            mk(r"(?i)\b(?:spanish|castellano|latino)\b", "spanish"),
            mk(r"(?i)\b(?:truefrench|vff|french)\b", "french"),
            mk(r"(?i)\b(?:german|deutsch)\b", "german"),
            mk(r"(?i)\bitalian\b", "italian"),
            mk(r"(?i)\b(?:japanese|jpn)\b", "japanese"),
            mk(r"(?i)\bkorean\b", "korean"),
            mk(r"(?i)\brussian\b", "russian"),
            mk(r"(?i)\b(?:portuguese|portugues)\b", "portuguese"),
            mk(r"(?i)\b(?:mandarin|cantonese|chinese)\b", "chinese"),
            mk(r"(?i)\bhindi\b", "hindi"),
            mk(r"(?i)\barabic\b", "arabic"),
            mk(r"(?i)\bturkish\b", "turkish"),
            mk(r"(?i)\bpolish\b", "polish"),
            mk(r"(?i)\bdutch\b", "dutch"),
            mk(r"(?i)\bswedish\b", "swedish"),
            mk(r"(?i)\bnorwegian\b", "norwegian"),
            mk(r"(?i)\bdanish\b", "danish"),
            mk(r"(?i)\bfinnish\b", "finnish"),
            mk(r"(?i)\bczech\b", "czech"),
            mk(r"(?i)\bslovak\b", "slovak"),
            mk(r"(?i)\bhungarian\b", "hungarian"),
            mk(r"(?i)\bromanian\b", "romanian"),
            mk(r"(?i)\bgreek\b", "greek"),
            mk(r"(?i)\bukrainian\b", "ukrainian"),
            mk(r"(?i)\bvietnamese\b", "vietnamese"),
            mk(r"(?i)\bthai\b", "thai"),
            mk(r"(?i)\bindonesian\b", "indonesian"),
            mk(r"(?i)\bhebrew\b", "hebrew"),
            // Last — most English releases don't tag at all, but when they
            // do (`Movie.ENGLISH.1080p.…`) we want to catch it.
            mk(r"(?i)\benglish\b", "english"),
        ]
    });

    for (re, name) in PATTERNS.iter() {
        if re.is_match(title) {
            return Some((*name).to_string());
        }
    }
    None
}

/// Redact any URLs embedded inside `msg`, preserving everything else.
///
/// Use this when surfacing third-party errors that bake the full URL into
/// their `Display` impl (reqwest does this — `error sending request for
/// url (https://api…?apikey=secret): underlying`). Without this, every
/// network failure on a credentialed URL leaks the credential into logs
/// or healthcheck JSON responses.
///
/// Matches `http://…`, `https://…`, `nntp://…`, `nntps://…` (the schemes
/// we actually deal with). Each match goes through `redact_url`.
pub fn redact_log(msg: &str) -> String {
    static URL_RE: Lazy<Regex> = Lazy::new(|| {
        // Greedy until whitespace, paren, or angle-bracket — covers the
        // common embedded forms `for url (URL)` and `URL: ...` alike.
        Regex::new(r"https?://[^\s)>]+|nntps?://[^\s)>]+").unwrap()
    });
    URL_RE
        .replace_all(msg, |caps: &regex::Captures<'_>| redact_url(&caps[0]))
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_nntp_userinfo() {
        let r = redact_url("nntps://sladgin:fekenesor3d6d8@news.example.com:563/20");
        assert!(!r.contains("sladgin"));
        assert!(!r.contains("fekenesor"));
        assert_eq!(r, "nntps://news.example.com:563/20");
    }

    #[test]
    fn strips_indexer_query_string() {
        let r = redact_url("https://api.nzbplanet.net/getnzb/abc.nzb?i=12345&r=secret-apikey");
        assert!(!r.contains("apikey"));
        assert!(!r.contains("secret"));
        assert!(!r.contains("12345"));
        assert_eq!(r, "https://api.nzbplanet.net/getnzb/abc.nzb");
    }

    #[test]
    fn keeps_scheme_host_port_path() {
        let r = redact_url("https://api.example.com:443/v1/resource/sub-path?q=x");
        assert_eq!(r, "https://api.example.com/v1/resource/sub-path");
    }

    #[test]
    fn handles_userinfo_with_url_encoded_chars() {
        // Real NNTP URL with @ in username (URL-encoded).
        let r = redact_url("nntps://us%40er:p%3Aa%2Fss@news.example.com/2");
        assert!(!r.contains("us"));
        assert!(!r.contains("ss"));
        assert!(r.starts_with("nntps://news.example.com"));
    }

    #[test]
    fn invalid_url_returns_marker() {
        assert_eq!(redact_url("not a url at all"), "<invalid-url>");
    }

    #[test]
    fn strips_fragment_too() {
        let r = redact_url("https://x.com/p?q=1#sensitive");
        assert!(!r.contains("sensitive"));
    }

    #[test]
    fn redact_log_strips_url_in_reqwest_style_message() {
        let msg = "error sending request for url (https://api.nzbplanet.net/getnzb/abc.nzb?i=12345&r=secret-apikey): connection reset";
        let redacted = redact_log(msg);
        assert!(!redacted.contains("secret-apikey"));
        assert!(!redacted.contains("12345"));
        assert!(redacted.contains("https://api.nzbplanet.net/getnzb/abc.nzb"));
        assert!(redacted.contains("connection reset"));
    }

    #[test]
    fn redact_log_strips_nntp_userinfo_in_message() {
        let msg = "NNTP connect to nntps://bob:hunter2@news.example.com:563/4 failed";
        let redacted = redact_log(msg);
        assert!(!redacted.contains("bob"));
        assert!(!redacted.contains("hunter2"));
        assert!(redacted.contains("nntps://news.example.com"));
    }

    #[test]
    fn redact_log_handles_multiple_urls() {
        let msg = "tried https://x.com/?k=v and https://y.com/?k=w both failed";
        let redacted = redact_log(msg);
        assert!(!redacted.contains("k=v"));
        assert!(!redacted.contains("k=w"));
    }

    #[test]
    fn redact_log_passes_through_non_url_text() {
        let msg = "no urls in this message at all";
        assert_eq!(redact_log(msg), msg);
    }

    #[test]
    fn normalize_language_lowercases_and_trims() {
        assert_eq!(normalize_language("English"), "english");
        assert_eq!(normalize_language("  English  "), "english");
        assert_eq!(normalize_language("SPANISH"), "spanish");
    }

    #[test]
    fn normalize_language_maps_iso_codes() {
        assert_eq!(normalize_language("en"), "english");
        assert_eq!(normalize_language("EN"), "english");
        assert_eq!(normalize_language("es"), "spanish");
        assert_eq!(normalize_language("ja"), "japanese");
        assert_eq!(normalize_language("ko"), "korean");
    }

    #[test]
    fn normalize_language_passes_unknown_through() {
        assert_eq!(normalize_language("latvian"), "latvian");
        assert_eq!(normalize_language("Klingon"), "klingon");
    }

    #[test]
    fn detect_language_matches_full_names() {
        assert_eq!(
            detect_language("Movie.SPANISH.1080p.WEB-DL.x265-RARBG").as_deref(),
            Some("spanish")
        );
        assert_eq!(
            detect_language("Movie.GERMAN.1080p.WEB-DL.x265-RARBG").as_deref(),
            Some("german")
        );
        assert_eq!(
            detect_language("Movie.JAPANESE.1080p.BluRay.x264-RARBG").as_deref(),
            Some("japanese")
        );
        assert_eq!(
            detect_language("Movie.ENGLISH.1080p.WEB-DL.x265-RARBG").as_deref(),
            Some("english")
        );
    }

    #[test]
    fn detect_language_multi_takes_precedence() {
        // MULTi.FRENCH should report multi (multi-track pack), not french.
        assert_eq!(
            detect_language("Movie.MULTi.FRENCH.1080p.BluRay-RARBG").as_deref(),
            Some("multi")
        );
        assert_eq!(
            detect_language("Movie.DUAL.1080p.BluRay-RARBG").as_deref(),
            Some("multi")
        );
    }

    #[test]
    fn detect_language_returns_none_for_untagged() {
        assert_eq!(detect_language("Movie.2024.1080p.WEB-DL.x265-RARBG"), None);
    }

    #[test]
    fn detect_language_handles_truefrench_variants() {
        assert_eq!(
            detect_language("Movie.TRUEFRENCH.1080p-RARBG").as_deref(),
            Some("french")
        );
        assert_eq!(
            detect_language("Movie.VFF.1080p-RARBG").as_deref(),
            Some("french")
        );
    }

    #[test]
    fn multi_language_tag_detection() {
        assert!(is_multi_language_tag("multi"));
        assert!(is_multi_language_tag("dual"));
        assert!(is_multi_language_tag("multilingual"));
        assert!(!is_multi_language_tag("english"));
        assert!(!is_multi_language_tag("MULTi")); // caller is expected to lowercase first
    }
}
