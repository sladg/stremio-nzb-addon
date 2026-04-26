//! NzbCandidate: an indexer search result paired with its parsed-title signature.
//!
//! Phase 5 adds the `group_candidates` helper used by the stream/catalog
//! handlers to collapse re-uploads of the same release into a single Stremio
//! entry whose pre-flight walks the upload list until one passes.

use std::collections::HashMap;

use crate::nzb_api::Item;
use crate::parse_title::parse;

#[derive(Debug, Clone)]
pub struct NzbCandidate {
    pub nzb_url: String,
    pub item: Item,
    pub signature: GroupSignature,
}

/// Tuple of `parse_title` fields used to decide which NZBs are "the same
/// release." Two candidates with identical signatures are alternate uploads
/// of the same encoding (same resolution + same source/codec/group) and can
/// substitute for each other transparently. Different signatures (different
/// release group, different resolution, etc.) stay as separate Stremio entries.
#[derive(Debug, Clone, Default, Hash, PartialEq, Eq)]
pub struct GroupSignature {
    pub resolution: Option<String>,
    pub source: Option<String>,
    pub codec: Option<String>,
    pub group: Option<String>,
    /// Detected audio language (canonicalized via `util::normalize_language`).
    /// English/Spanish/French/etc. variants of the same release end up in
    /// distinct groups so users can pick the language they want from
    /// Stremio's stream list.
    pub language: Option<String>,
}

/// Group a list of candidates by `GroupSignature`, preserving:
///   - The order in which signatures first appear (= indexer order of best-ranked
///     candidate per group)
///   - Within each group: the original order of the candidates
///
/// This is the entry point for Phase 5 auto-fallback: each returned `Vec<NzbCandidate>`
/// becomes one `StreamSession` whose pre-flight walks the list.
pub fn group_candidates(candidates: Vec<NzbCandidate>) -> Vec<Vec<NzbCandidate>> {
    let mut order: Vec<GroupSignature> = Vec::new();
    let mut buckets: HashMap<GroupSignature, Vec<NzbCandidate>> = HashMap::new();
    for c in candidates {
        let sig = c.signature.clone();
        if !buckets.contains_key(&sig) {
            order.push(sig.clone());
        }
        buckets.entry(sig).or_default().push(c);
    }
    order
        .into_iter()
        .map(|sig| buckets.remove(&sig).expect("inserted above"))
        .collect()
}

/// Build an `NzbCandidate` from an indexer hit.
///
/// Signature granularity sits in this one place. If real-world testing shows
/// `(resolution, source, codec, group, language)` is too restrictive (i.e.
/// each group's release is a single upload, so fallback never triggers),
/// loosen by zeroing out fields here:
/// - drop `language` → English/Spanish/etc. of same release collapse, single
///   stream entry per release-group with whichever upload wins committing
/// - drop `group`    → re-uploads from different groups collapse, fallback
///   walks across release groups (RARBG → EVO → SPARKS)
/// - drop `codec`    → x264 and x265 of same source collapse
/// - drop `source`   → BluRay, WEB-DL, HDTV all collapse at same resolution
///
/// Loosening is one-line per field. No callers depend on the granularity.
pub fn candidate_from_item(item: Item) -> NzbCandidate {
    use crate::nzb_api::item_nzb_url;
    let parsed = parse(&item.title);
    let signature = GroupSignature {
        resolution: parsed.resolution,
        source: parsed.source,
        codec: parsed.codec,
        group: parsed.group,
        language: parsed
            .language
            .as_deref()
            .map(crate::util::normalize_language), // ← drop this line first if too restrictive
    };
    NzbCandidate {
        nzb_url: item_nzb_url(&item),
        item,
        signature,
    }
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
    fn groups_by_full_signature() {
        let cands = vec![
            candidate_from_item(item("Movie.2024.1080p.WEB-DL.x265-RARBG")),
            candidate_from_item(item("Movie.2024.1080p.WEB-DL.x265-RARBG")), // re-upload
            candidate_from_item(item("Movie.2024.1080p.WEB-DL.x265-EVO")),
            candidate_from_item(item("Movie.2024.720p.WEB-DL.x264-RARBG")),
        ];
        let groups = group_candidates(cands);
        assert_eq!(groups.len(), 3);
        // Group 0: 2× RARBG 1080p
        assert_eq!(groups[0].len(), 2);
        // Group 1: 1× EVO 1080p
        assert_eq!(groups[1].len(), 1);
        // Group 2: 1× RARBG 720p
        assert_eq!(groups[2].len(), 1);
    }

    #[test]
    fn preserves_indexer_order_across_groups() {
        let cands = vec![
            candidate_from_item(item("X.1080p.WEB-DL.x265-A")),
            candidate_from_item(item("X.720p.WEB-DL.x264-B")),
            candidate_from_item(item("X.1080p.WEB-DL.x265-A")), // dup of first
        ];
        let groups = group_candidates(cands);
        // First-seen order: A then B (the dup gets appended to A's bucket).
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0][0].signature.group.as_deref(), Some("A"));
        assert_eq!(groups[0].len(), 2);
        assert_eq!(groups[1][0].signature.group.as_deref(), Some("B"));
    }

    #[test]
    fn empty_input_empty_output() {
        let groups = group_candidates(Vec::new());
        assert!(groups.is_empty());
    }

    #[test]
    fn obfuscated_with_no_signature_fields_collapse_together() {
        // Both items have None-for-everything signature (nzb-rs / parse_title
        // can't extract anything); they collapse into one group.
        let cands = vec![
            candidate_from_item(item("abc123xyz")),
            candidate_from_item(item("zzz789abc")),
        ];
        let groups = group_candidates(cands);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 2);
    }
}
