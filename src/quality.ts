import { Item } from "./types.js";

/** Default duration estimates in minutes (fallback only) */
const DEFAULT_DURATION = { movie: 120, series: 45 } as const;

/**
 * Calculate bandwidth in Gbit/hour from file size and duration
 */
export const calculateBandwidth = (
  sizeBytes: number,
  durationMinutes: number,
): number => {
  if (durationMinutes <= 0 || sizeBytes <= 0) return 0;

  const gbits = (sizeBytes * 8) / Math.pow(1024, 3);
  return (gbits / durationMinutes) * 60;
};

/**
 * Extract attribute value from item by name
 * Newznab attributes include: size, runtime, imdb, tvdbid, etc.
 */
const getItemAttribute = (item: Item, name: string): string | undefined =>
  item.attr?.find((el) => el["@attributes"]?.name === name)?.["@attributes"]
    ?.value;

/** Extract file size from item attributes */
const getItemSize = (item: Item): number => {
  const size = getItemAttribute(item, "size");
  return size ? parseInt(size, 10) || 0 : 0;
};

/**
 * Extract runtime/duration from item attributes
 * Newznab API returns runtime in minutes for movies/series
 * Falls back to default estimates if not available
 */
const getItemDuration = (item: Item, type: "movie" | "series"): number => {
  // Try 'runtime' attribute (most common in newznab)
  const runtime = getItemAttribute(item, "runtime");
  if (runtime) {
    const minutes = parseInt(runtime, 10);
    if (minutes > 0) return minutes;
  }

  // Try 'duration' attribute (some indexers use this)
  const duration = getItemAttribute(item, "duration");
  if (duration) {
    const minutes = parseInt(duration, 10);
    if (minutes > 0) return minutes;
  }

  // Fallback to default estimates
  return DEFAULT_DURATION[type];
};

/**
 * Filter items by bandwidth threshold and sort by bandwidth ascending
 * Uses actual runtime from metadata when available, falls back to estimates
 */
export interface QualityBounds {
  min?: number;
  max?: number;
}

export const filterByQuality = (
  items: Item[],
  bounds: QualityBounds,
  type: "movie" | "series",
): Item[] => {
  const { min, max } = bounds;
  if (min === undefined && max === undefined) return items;

  return items
    .map((item) => ({
      item,
      bandwidth: calculateBandwidth(
        getItemSize(item),
        getItemDuration(item, type),
      ),
    }))
    .filter(({ bandwidth }) => {
      if (bandwidth <= 0) return false;
      if (min !== undefined && bandwidth < min) return false;
      if (max !== undefined && bandwidth > max) return false;
      return true;
    })
    .sort((a, b) => a.bandwidth - b.bandwidth)
    .map(({ item }) => item);
};
