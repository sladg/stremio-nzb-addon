use torrent_name_parser::Metadata;

use crate::util::detect_language;

/// Subset of parse-torrent-title fields used by the addon.
#[derive(Debug, Default, Clone)]
pub struct Parsed {
    pub title: String,
    pub resolution: Option<String>,
    pub source: Option<String>,
    pub codec: Option<String>,
    pub audio: Option<String>,
    pub group: Option<String>,
    pub language: Option<String>,
}

pub fn parse(title: &str) -> Parsed {
    // `torrent-name-parser` only catches a tiny subset of language tags
    // (`MULTi`, `FRENCH`, `TRUEFRENCH`, `VFF`, `rus.eng`, `US`). We use
    // `util::detect_language` for broader real-world coverage so the
    // language filter and per-language grouping see SPANISH/GERMAN/etc.
    let language = detect_language(title);
    match Metadata::from(title) {
        Ok(m) => Parsed {
            title: m.title().to_string(),
            resolution: m.resolution().map(str::to_string),
            // torrent-name-parser uses `quality` for what parse-torrent-title calls `source`.
            source: m.quality().map(str::to_string),
            codec: m.codec().map(str::to_string),
            audio: m.audio().map(str::to_string),
            group: m.group().map(str::to_string),
            language,
        },
        Err(_) => Parsed {
            title: title.to_string(),
            language,
            ..Default::default()
        },
    }
}
