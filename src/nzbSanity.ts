import { createHash } from "crypto";
import { Item } from "./types.js";
import { getCache, setCache } from "./cache.js";

interface SanityResult {
  ok: boolean;
  reason?: string;
}

const CACHE_TTL = 86400; // 24h - NZB structure is immutable
const FETCH_TIMEOUT_MS = 5000;
const MAX_NZB_SIZE = 5 * 1024 * 1024; // 5 MB cap to avoid OOM on garbage URLs

/** Any RAR-looking filename (.partNN.rar, .rNN volume, or plain .rar) */
const RAR_ANY_RE = /\.(?:part\d+\.rar|r\d{2,3}|rar)(?![.\w])/i;

const sha1 = (s: string) =>
  createHash("sha1").update(s).digest("hex").slice(0, 16);

/** Pull all subject="..." values from NZB XML without parsing the whole DOM */
const extractSubjects = (xml: string): string[] =>
  [...xml.matchAll(/subject="([^"]+)"/gi)].map((m) => m[1]);

/**
 * Inspect NZB structure and decide if Stremio's streaming engine has any
 * chance of reassembling it into a playable file.
 *
 * The real failure mode we hit: indexer-stitched NZBs where each .partNN.rar
 * uses a different stem (e.g. "abc.part01.rar" + "xyz.part02.rar") - those
 * aren't actually parts of the same archive and Stremio silently 500s.
 */
export const checkNzbSanity = async (nzbUrl: string): Promise<SanityResult> => {
  const cacheKey = `nzb-sanity:${sha1(nzbUrl)}`;
  const cached = await getCache<SanityResult>(cacheKey);
  if (cached) return cached;

  const result = await probeNzb(nzbUrl);
  await setCache(cacheKey, result, CACHE_TTL);
  return result;
};

const probeNzb = async (nzbUrl: string): Promise<SanityResult> => {
  let xml: string;
  try {
    const ctrl = new AbortController();
    const timer = setTimeout(() => ctrl.abort(), FETCH_TIMEOUT_MS);
    const response = await fetch(nzbUrl, { signal: ctrl.signal });
    clearTimeout(timer);

    if (!response.ok) {
      return { ok: false, reason: `http-${response.status}` };
    }

    const contentLength = parseInt(
      response.headers.get("content-length") || "0",
      10,
    );
    if (contentLength > MAX_NZB_SIZE) {
      return { ok: false, reason: `nzb-too-large-${contentLength}` };
    }

    xml = await response.text();
    if (xml.length > MAX_NZB_SIZE) {
      return { ok: false, reason: `nzb-too-large-${xml.length}` };
    }
  } catch (err) {
    return {
      ok: false,
      reason: err instanceof Error ? err.message : "fetch-failed",
    };
  }

  const subjects = extractSubjects(xml);
  if (!subjects.length) return { ok: false, reason: "no-subjects" };

  // We can't reliably detect the "stitched-from-multiple-uploads" failure
  // mode from the NZB XML alone - obfuscated releases legitimately have
  // random per-part stems and inconsistent [X/N] subject indices.
  // Limit ourselves to the one check that's reliable: the NZB must contain
  // at least one RAR-looking file. Releases that ship raw .mkv/.mp4 (no
  // RAR wrapper) can't be streamed by Stremio's NZB engine either way.
  const hasRar = subjects.some((s) => RAR_ANY_RE.test(s));
  if (!hasRar) return { ok: false, reason: "no-rar-files" };

  return { ok: true };
};

/**
 * Filter items by NZB structural sanity. Runs sanity checks in parallel.
 * Items whose nzbUrl resolves to an unplayable structure are dropped.
 */
export const filterByNzbSanity = async (
  items: Item[],
  getNzbUrl: (item: Item) => string,
): Promise<Item[]> => {
  if (!items.length) return items;

  const results = await Promise.all(
    items.map(async (item) => ({
      item,
      sanity: await checkNzbSanity(getNzbUrl(item)),
    })),
  );

  const dropped = results.filter((r) => !r.sanity.ok);
  if (dropped.length) {
    console.log(
      `[nzbSanity] excluded ${dropped.length} of ${items.length}: ${dropped
        .slice(0, 3)
        .map((r) => `"${r.item.title}" (${r.sanity.reason})`)
        .join(", ")}${dropped.length > 3 ? "..." : ""}`,
    );
  }

  return results.filter((r) => r.sanity.ok).map((r) => r.item);
};
