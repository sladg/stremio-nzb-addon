import { Item } from "./types.js";

/**
 * Compile user-supplied exclusion pattern into a RegExp.
 * Accepts either a /pattern/flags literal or a bare pattern (case-insensitive by default).
 * Returns null on invalid input so a bad config doesn't break the addon.
 */
export const compileExcludeRegex = (input?: string): RegExp | null => {
  const trimmed = input?.trim();
  if (!trimmed) return null;

  try {
    const literal = trimmed.match(/^\/(.+)\/([gimsuy]*)$/);
    if (literal) return new RegExp(literal[1], literal[2]);
    return new RegExp(trimmed, "i");
  } catch (err) {
    console.warn(
      `[contentFilter] invalid regex "${trimmed}": ${err instanceof Error ? err.message : err}`,
    );
    return null;
  }
};

/**
 * Drop items whose title matches the exclusion regex.
 */
export const filterByTitleRegex = (
  items: Item[],
  excludeRegex?: string,
): Item[] => {
  const regex = compileExcludeRegex(excludeRegex);
  if (!regex) return items;

  const dropped: string[] = [];
  const kept = items.filter((item) => {
    if (regex.test(item.title)) {
      dropped.push(item.title);
      return false;
    }
    return true;
  });

  if (dropped.length) {
    console.log(
      `[contentFilter] excluded ${dropped.length} of ${items.length} items via regex ${regex}`,
    );
  }
  return kept;
};
