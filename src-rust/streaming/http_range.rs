//! HTTP `Range:` header parsing + `Content-Range` building.
//!
//! Phase 3: single-range only. Multi-range (`bytes=0-99,200-299`) is rejected
//! since video clients don't use it for streaming.

#[derive(Debug, PartialEq, Eq)]
pub enum ParsedRange {
    /// No `Range` header — serve the full body, status 200.
    Full,
    /// Inclusive byte range — serve `[start..=end]`, status 206.
    Partial { start: u64, end: u64 },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RangeError {
    #[error("malformed Range header")]
    Malformed,
    #[error("multi-range requests are not supported")]
    MultiRange,
    #[error("range not satisfiable")]
    NotSatisfiable,
}

/// Parse the `Range` header value (e.g. `bytes=0-1023`, `bytes=500-`,
/// `bytes=-1024` for the suffix form). Returns inclusive `[start, end]`
/// bounded by `total - 1`.
///
/// `None` for `header` → `Ok(ParsedRange::Full)`.
pub fn parse_range(header: Option<&str>, total: u64) -> Result<ParsedRange, RangeError> {
    let Some(raw) = header else {
        return Ok(ParsedRange::Full);
    };

    let raw = raw.trim();
    let spec = raw
        .strip_prefix("bytes=")
        .ok_or(RangeError::Malformed)?
        .trim();

    if spec.contains(',') {
        return Err(RangeError::MultiRange);
    }

    if total == 0 {
        // Any range against an empty body is unsatisfiable.
        return Err(RangeError::NotSatisfiable);
    }

    let (start_s, end_s) = spec.split_once('-').ok_or(RangeError::Malformed)?;
    let start_s = start_s.trim();
    let end_s = end_s.trim();

    let (start, end) = match (start_s.is_empty(), end_s.is_empty()) {
        (true, true) => return Err(RangeError::Malformed),
        // `bytes=-N` → last N bytes
        (true, false) => {
            let n: u64 = end_s.parse().map_err(|_| RangeError::Malformed)?;
            if n == 0 {
                return Err(RangeError::NotSatisfiable);
            }
            let n = n.min(total);
            (total - n, total - 1)
        }
        // `bytes=N-` → from N to end
        (false, true) => {
            let s: u64 = start_s.parse().map_err(|_| RangeError::Malformed)?;
            if s >= total {
                return Err(RangeError::NotSatisfiable);
            }
            (s, total - 1)
        }
        // `bytes=N-M`
        (false, false) => {
            let s: u64 = start_s.parse().map_err(|_| RangeError::Malformed)?;
            let e: u64 = end_s.parse().map_err(|_| RangeError::Malformed)?;
            if s > e {
                return Err(RangeError::NotSatisfiable);
            }
            if s >= total {
                return Err(RangeError::NotSatisfiable);
            }
            (s, e.min(total - 1))
        }
    };

    Ok(ParsedRange::Partial { start, end })
}

/// Build `Content-Range: bytes start-end/total` for a 206 response.
pub fn content_range_header(start: u64, end: u64, total: u64) -> String {
    format!("bytes {start}-{end}/{total}")
}

/// Build `Content-Range: bytes */total` for a 416 response.
pub fn content_range_unsatisfied(total: u64) -> String {
    format!("bytes */{total}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_header_is_full() {
        assert_eq!(parse_range(None, 1000), Ok(ParsedRange::Full));
    }

    #[test]
    fn inclusive_range() {
        assert_eq!(
            parse_range(Some("bytes=0-99"), 1000),
            Ok(ParsedRange::Partial { start: 0, end: 99 })
        );
        assert_eq!(
            parse_range(Some("bytes=100-199"), 1000),
            Ok(ParsedRange::Partial { start: 100, end: 199 })
        );
    }

    #[test]
    fn open_ended_range() {
        assert_eq!(
            parse_range(Some("bytes=500-"), 1000),
            Ok(ParsedRange::Partial { start: 500, end: 999 })
        );
    }

    #[test]
    fn suffix_range() {
        // bytes=-100 → last 100 bytes of a 1000-byte resource = [900..999]
        assert_eq!(
            parse_range(Some("bytes=-100"), 1000),
            Ok(ParsedRange::Partial { start: 900, end: 999 })
        );
    }

    #[test]
    fn suffix_larger_than_total_clamps() {
        // bytes=-2000 against 1000-byte resource → entire resource
        assert_eq!(
            parse_range(Some("bytes=-2000"), 1000),
            Ok(ParsedRange::Partial { start: 0, end: 999 })
        );
    }

    #[test]
    fn end_clamped_to_total_minus_one() {
        assert_eq!(
            parse_range(Some("bytes=100-9999"), 1000),
            Ok(ParsedRange::Partial { start: 100, end: 999 })
        );
    }

    #[test]
    fn whitespace_tolerated() {
        assert_eq!(
            parse_range(Some("bytes= 0 - 99 "), 1000),
            Ok(ParsedRange::Partial { start: 0, end: 99 })
        );
    }

    #[test]
    fn missing_prefix_is_malformed() {
        assert_eq!(parse_range(Some("0-99"), 1000), Err(RangeError::Malformed));
    }

    #[test]
    fn multirange_rejected() {
        assert_eq!(
            parse_range(Some("bytes=0-99,200-299"), 1000),
            Err(RangeError::MultiRange)
        );
    }

    #[test]
    fn start_past_end_unsatisfiable() {
        assert_eq!(
            parse_range(Some("bytes=1000-2000"), 1000),
            Err(RangeError::NotSatisfiable)
        );
        assert_eq!(
            parse_range(Some("bytes=2000-"), 1000),
            Err(RangeError::NotSatisfiable)
        );
    }

    #[test]
    fn reversed_unsatisfiable() {
        assert_eq!(
            parse_range(Some("bytes=99-0"), 1000),
            Err(RangeError::NotSatisfiable)
        );
    }

    #[test]
    fn empty_body_unsatisfiable() {
        assert_eq!(
            parse_range(Some("bytes=0-"), 0),
            Err(RangeError::NotSatisfiable)
        );
    }

    #[test]
    fn dashes_only_malformed() {
        assert_eq!(parse_range(Some("bytes=-"), 1000), Err(RangeError::Malformed));
    }

    #[test]
    fn content_range_format() {
        assert_eq!(content_range_header(0, 99, 1000), "bytes 0-99/1000");
        assert_eq!(content_range_unsatisfied(1000), "bytes */1000");
    }
}
