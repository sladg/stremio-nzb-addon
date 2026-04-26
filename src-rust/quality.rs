use crate::nzb_api::{item_attr, item_size, Item};

const DEFAULT_MOVIE_MIN: u64 = 120;
const DEFAULT_SERIES_MIN: u64 = 45;

pub fn calculate_bandwidth(size_bytes: u64, duration_minutes: u64) -> f64 {
    if duration_minutes == 0 || size_bytes == 0 {
        return 0.0;
    }
    let gbits = (size_bytes as f64 * 8.0) / 1024f64.powi(3);
    (gbits / duration_minutes as f64) * 60.0
}

fn item_duration(item: &Item, media_kind: &str) -> u64 {
    if let Some(rt) = item_attr(item, "runtime").and_then(|v| v.parse::<u64>().ok()) {
        if rt > 0 {
            return rt;
        }
    }
    if let Some(d) = item_attr(item, "duration").and_then(|v| v.parse::<u64>().ok()) {
        if d > 0 {
            return d;
        }
    }
    if media_kind == "movie" {
        DEFAULT_MOVIE_MIN
    } else {
        DEFAULT_SERIES_MIN
    }
}

pub fn filter_by_quality(
    items: Vec<Item>,
    min_gbit_per_hour: Option<f64>,
    max_gbit_per_hour: Option<f64>,
    media_kind: &str,
) -> Vec<Item> {
    if min_gbit_per_hour.is_none() && max_gbit_per_hour.is_none() {
        return items;
    }

    let lo = min_gbit_per_hour.unwrap_or(0.0);
    let hi = max_gbit_per_hour.unwrap_or(f64::INFINITY);

    let total = items.len();
    let scored_all: Vec<(f64, Item)> = items
        .into_iter()
        .map(|it| {
            let bw = calculate_bandwidth(item_size(&it), item_duration(&it, media_kind));
            (bw, it)
        })
        .collect();

    let mut dropped: Vec<(f64, String)> = Vec::new();
    let mut scored: Vec<(f64, Item)> = Vec::with_capacity(scored_all.len());
    for (bw, it) in scored_all {
        if bw > 0.0 && bw >= lo && bw <= hi {
            scored.push((bw, it));
        } else {
            dropped.push((bw, it.title));
        }
    }

    if !dropped.is_empty() {
        let preview: Vec<String> = dropped
            .iter()
            .take(20)
            .map(|(bw, t)| format!("\"{t}\" ({bw:.1} Gbit/h)"))
            .collect();
        let suffix = if dropped.len() > 20 { "..." } else { "" };
        tracing::info!(
            "[qualityFilter] excluded {} of {} items (window: {:.1}-{:.1} Gbit/h): {}{suffix}",
            dropped.len(),
            total,
            lo,
            if hi.is_infinite() { f64::NAN } else { hi },
            preview.join(", "),
        );
    }

    scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().map(|(_, it)| it).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nzb_api::{Attr, AttrPair};

    fn item_with(title: &str, size_bytes: u64, runtime_min: Option<u64>) -> Item {
        let mut attrs = vec![Attr {
            attributes: AttrPair {
                name: "size".to_string(),
                value: size_bytes.to_string(),
            },
        }];
        if let Some(rt) = runtime_min {
            attrs.push(Attr {
                attributes: AttrPair {
                    name: "runtime".to_string(),
                    value: rt.to_string(),
                },
            });
        }
        Item {
            title: title.to_string(),
            attr: Some(attrs),
            ..Default::default()
        }
    }

    #[test]
    fn calculate_bandwidth_zero_inputs() {
        assert_eq!(calculate_bandwidth(0, 100), 0.0);
        assert_eq!(calculate_bandwidth(1024 * 1024 * 1024, 0), 0.0);
    }

    #[test]
    fn calculate_bandwidth_known_value() {
        // 1 GiB over 60 min = (8 Gbit / 60) * 60 = 8 Gbit/h
        let bw = calculate_bandwidth(1024 * 1024 * 1024, 60);
        assert!((bw - 8.0).abs() < 0.01, "got {bw}");
    }

    #[test]
    fn filter_by_quality_no_op_when_both_none() {
        let items = vec![item_with("a", 1, None), item_with("b", 2, None)];
        let out = filter_by_quality(items.clone(), None, None, "movie");
        assert_eq!(out.len(), 2);
    }

    // Reference bandwidths for 120 min movie:
    //   100 MiB →  ~0.39 Gbit/h
    //     5 GiB →    20 Gbit/h
    //    50 GiB →   200 Gbit/h

    #[test]
    fn filter_by_quality_min_drops_low_bitrate() {
        let items = vec![
            item_with("low", 100 * 1024 * 1024, Some(120)),
            item_with("mid", 5 * 1024 * 1024 * 1024, Some(120)),
            item_with("high", 50u64 * 1024 * 1024 * 1024, Some(120)),
        ];
        // min=1 drops "low" (~0.39); keeps "mid" (20) and "high" (200).
        let out = filter_by_quality(items, Some(1.0), None, "movie");
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|i| i.title == "mid"));
        assert!(out.iter().any(|i| i.title == "high"));
    }

    #[test]
    fn filter_by_quality_max_drops_high_bitrate() {
        let items = vec![
            item_with("low", 100 * 1024 * 1024, Some(120)),
            item_with("mid", 5 * 1024 * 1024 * 1024, Some(120)),
            item_with("high", 50u64 * 1024 * 1024 * 1024, Some(120)),
        ];
        // max=50 keeps "low" (~0.39) and "mid" (20); drops "high" (200).
        let out = filter_by_quality(items, None, Some(50.0), "movie");
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|i| i.title == "low"));
        assert!(out.iter().any(|i| i.title == "mid"));
    }

    #[test]
    fn filter_by_quality_min_max_window() {
        let items = vec![
            item_with("low", 100 * 1024 * 1024, Some(120)),
            item_with("mid", 5 * 1024 * 1024 * 1024, Some(120)),
            item_with("high", 50u64 * 1024 * 1024 * 1024, Some(120)),
        ];
        // window [1, 50] keeps only "mid" (20).
        let out = filter_by_quality(items, Some(1.0), Some(50.0), "movie");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "mid");
    }

    #[test]
    fn filter_by_quality_sorted_ascending() {
        let items = vec![
            item_with("c-high", 50u64 * 1024 * 1024 * 1024, Some(120)),
            item_with("a-low", 1 * 1024 * 1024 * 1024, Some(120)),
            item_with("b-mid", 10u64 * 1024 * 1024 * 1024, Some(120)),
        ];
        let out = filter_by_quality(items, Some(0.0), None, "movie");
        // Sorted by ascending bandwidth: low → mid → high
        assert_eq!(out.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(),
                   vec!["a-low", "b-mid", "c-high"]);
    }

    #[test]
    fn filter_by_quality_uses_runtime_when_available() {
        // Same size, different runtimes -> different bandwidths
        let items = vec![
            item_with("short", 5 * 1024 * 1024 * 1024, Some(60)),  // higher bw
            item_with("long", 5 * 1024 * 1024 * 1024, Some(180)),  // lower bw
        ];
        let out = filter_by_quality(items, Some(0.0), None, "movie");
        assert_eq!(out[0].title, "long");  // sorted ascending
        assert_eq!(out[1].title, "short");
    }
}
