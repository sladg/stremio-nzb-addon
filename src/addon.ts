import {
  AddonBuilder,
  AddonInterface,
  Manifest,
  ManifestCatalog,
  Stream,
} from "@stremio-addon/sdk";
import { NzbHydraAddonConfig, Item, NzbAddonConfig } from "./types.js";
import { NZBWebApiPool } from "./nzb-api.js";
import { parse as parseTorrentTitle } from "parse-torrent-title";
import { filterByQuality } from "./quality.js";
import { filterByTitleRegex } from "./contentFilter.js";
import { filterByNzbSanity } from "./nzbSanity.js";
import { filterByNzbAvailability } from "./nzbAvailability.js";

/** Resolution priority order (highest quality first) */
const RESOLUTION_ORDER = [
  ["2160p", "4k"],
  "1440p",
  "1080p",
  "720p",
  "480p",
  "360p",
];

/** IMDB to TVDB cache */
const imdbTvdbCache: Record<string, string> = {};

/** Get resolution sort index */
const getResolutionIndex = (res: string): number => {
  const lower = res.toLowerCase();
  for (let i = 0; i < RESOLUTION_ORDER.length; i++) {
    const item = RESOLUTION_ORDER[i];
    if (
      Array.isArray(item)
        ? item.some((o) => o.toLowerCase() === lower)
        : item.toLowerCase() === lower
    ) {
      return i;
    }
  }
  return RESOLUTION_ORDER.length;
};

/** Sort items by resolution (highest first) */
const sortByResolution = (items: Item[]): Item[] =>
  items.sort((a, b) => {
    const resA = parseTorrentTitle(a.title).resolution || "";
    const resB = parseTorrentTitle(b.title).resolution || "";
    return getResolutionIndex(resA) - getResolutionIndex(resB);
  });

/**
 * Cap how many results are kept per resolution bucket. Items arrive
 * already sorted (best survivors of the upstream filters first), so we
 * just keep the first N per bucket. Same-resolution releases that all
 * pass filters become "alternates" the user can scroll past.
 *
 * perResolution = 0 or undefined disables the cap.
 */
const limitPerResolution = (items: Item[], perResolution?: number): Item[] => {
  if (!perResolution || perResolution <= 0) return items;
  const counts = new Map<number, number>();
  return items.filter((item) => {
    const res = parseTorrentTitle(item.title).resolution || "";
    const bucket = getResolutionIndex(res);
    const count = counts.get(bucket) ?? 0;
    if (count >= perResolution) return false;
    counts.set(bucket, count + 1);
    return true;
  });
};

/** Extract file size from item attributes */
const getItemSize = (item: Item): number => {
  const sizeAttr = item.attr?.find((el) => el["@attributes"]?.name === "size");
  return sizeAttr ? parseInt(sizeAttr["@attributes"].value, 10) || 0 : 0;
};

/** Convert bytes to human-readable size */
const toHumanSize = (size: number): string => {
  if (size === 0) return "0 B";
  const i = Math.floor(Math.log(size) / Math.log(1024));
  return `${(size / Math.pow(1024, i)).toFixed(2)} ${["B", "kB", "MB", "GB", "TB"][i]}`;
};

/** Extract NZB URL from item */
const getNzbUrl = (item: Item): string => {
  const url =
    item.link?.replace(/&amp;/g, "&") || item.enclosure["@attributes"].url;
  return url.includes("&") && !url.includes("?") ? url.replace("&", "?") : url;
};

/** Convert IMDB ID to TVDB ID */
const imdbToTvdb = async (imdbId: string): Promise<string | null> => {
  if (imdbTvdbCache[imdbId]) return imdbTvdbCache[imdbId];

  try {
    const response = await fetch(
      `https://thetvdb.com/api/GetSeriesByRemoteID.php?imdbid=${imdbId}`,
    );
    const text = await response.text();
    const match = text.match(/<seriesid>(\d+)<\/seriesid>/);

    if (match?.[1]) {
      imdbTvdbCache[imdbId] = match[1];
      return match[1];
    }
  } catch (err) {
    console.error("Failed to convert IMDB to TVDB:", err);
  }
  return null;
};

/** Convert NZB item to Stremio stream */
const itemToStream = (
  item: Item,
  servers: string[],
  name: string,
  id: string,
): Stream => {
  const size = getItemSize(item);
  const parsed = parseTorrentTitle(item.title);

  // Build description parts
  const desc = [`📁 ${parsed.title}`];

  const mediaInfo = [parsed.source, parsed.codec, parsed.group].filter(Boolean);
  if (mediaInfo.length) desc.push(`🎥 ${mediaInfo.join(" • ")}`);

  if (size) desc.push(`📦 ${toHumanSize(size)}`);

  const audioInfo = [parsed.audio, parsed.language].filter(Boolean);
  if (audioInfo.length) desc.push(`🎧 ${audioInfo.join(" • ")}`);

  if (item.comments) {
    const indexer = new URL(item.comments).hostname.replace(
      /^(www\.|api\.)/,
      "",
    );
    desc.push(`🔍 ${indexer}`);
  }

  // Build binge group for consistent episode grouping
  const bingeGroup = [
    id,
    parsed.resolution,
    parsed.source,
    parsed.codec,
    parsed.group,
    parsed.audio,
    parsed.language,
  ]
    .filter(Boolean)
    .join("|");

  return {
    description: desc.join("\n"),
    name: [name, parsed.resolution].filter(Boolean).join(" "),
    nzbUrl: getNzbUrl(item),
    servers,
    behaviorHints: {
      filename: item.title,
      videoSize: size || undefined,
      bingeGroup,
    },
  };
};

/** Normalize config to NzbAddonConfig format */
const normalizeConfig = (
  config: NzbHydraAddonConfig | NzbAddonConfig,
): NzbAddonConfig => {
  if ("indexerUrl" in config && "indexerApiKey" in config) {
    return {
      indexers: [{ url: config.indexerUrl, apiKey: config.indexerApiKey }],
      nntpServers: config.nntpServers,
      maxGbitPerHour: config.maxGbitPerHour,
    };
  }
  return config;
};

/** Create addon interface with stream, catalog, and meta handlers */
export const createAddonInterface = (
  manifest: Manifest,
  catalog: ManifestCatalog,
  name: string,
): AddonInterface => {
  const builder = new AddonBuilder(manifest);

  builder.defineStreamHandler<NzbHydraAddonConfig | NzbAddonConfig>(
    async ({ config: rawConfig, id, type }) => {
      try {
        const config = normalizeConfig(rawConfig);
        const api = new NZBWebApiPool(config.indexers);
        let items: Item[] | undefined;

        if (type === "movie") {
          items = await api.searchMovie(id.replace("tt", ""));
        } else if (type === "series") {
          const [imdbIdWithPrefix, season, episode] = id.split(":");
          const tvdbId = await imdbToTvdb(imdbIdWithPrefix);
          if (tvdbId) {
            items = await api.searchSeries(tvdbId, season, episode);
          } else {
            console.warn(
              `Could not find TVDB ID for IMDB: ${imdbIdWithPrefix}`,
            );
          }
        } else {
          console.warn(`Unsupported type '${type}' with id ${id}`);
        }

        let results = items || [];

        // Apply bandwidth quality filter if configured (min and/or max)
        if (
          (config.minGbitPerHour || config.maxGbitPerHour) &&
          (type === "movie" || type === "series")
        ) {
          results = filterByQuality(
            results,
            { min: config.minGbitPerHour, max: config.maxGbitPerHour },
            type,
          );
        }

        // Drop titles matching user-supplied exclusion regex (cheap, runs first)
        results = filterByTitleRegex(results, config.excludeRegex);

        // Optional structural NZB validation (fetches each NZB; results cached 24h)
        if (config.validateNzbStructure && results.length) {
          results = await filterByNzbSanity(results, getNzbUrl);
        }

        // Optional NNTP article availability check - probes the user's
        // backbones for the first segment of each .partNN.rar so releases
        // that are not actually downloadable get dropped before Stremio
        // attempts them. Heaviest filter, results cached 24h.
        if (config.validateNzbAvailability && results.length) {
          const nntpServers = config.nntpServers.map(({ server }) => server);
          results = await filterByNzbAvailability(
            results,
            nntpServers,
            getNzbUrl,
          );
        }

        sortByResolution(results);

        // Cap to N best per resolution bucket. Defaults to 1 if unset
        // (one stream per resolution); higher values keep alternates so
        // the user can scroll past a stream they don't like.
        results = limitPerResolution(results, config.streamsPerResolution ?? 1);

        const servers = config.nntpServers.map(({ server }) => server);
        const streams = results.map((item) =>
          itemToStream(item, servers, name, manifest.id),
        );

        console.log(`Found ${streams.length} streams for ${type} ${id}`);
        return { streams, cacheMaxAge: 3600 };
      } catch (err: unknown) {
        const message = err instanceof Error ? err.message : String(err);
        console.error(`Stream handler error: ${message}`);
        throw err;
      }
    },
  );

  builder.defineCatalogHandler(async ({ extra: { search } }) => {
    const query = search?.trim() || "";
    if (!query) return { metas: [] };

    return {
      metas: [
        {
          id: `${catalog.id}:${encodeURIComponent(query)}`,
          name: query,
          type: "tv",
          logo: manifest.logo,
          background: manifest.background,
          posterShape: "square",
          poster: manifest.logo,
          description: `Search results from ${manifest.name} for '${search}'`,
        },
      ],
      cacheMaxAge: 3600 * 24 * 30, // 30 days (static data)
    };
  });

  builder.defineMetaHandler<NzbHydraAddonConfig | NzbAddonConfig>(
    async ({ id, config: rawConfig }) => {
      try {
        const config = normalizeConfig(rawConfig);

        if (!id.startsWith(`${catalog.id}:`)) {
          return { meta: { id, name: catalog.name, type: "tv" } };
        }

        const query = decodeURIComponent(id.replace(`${catalog.id}:`, ""));
        const servers = config.nntpServers.map(({ server }) => server);
        const api = new NZBWebApiPool(config.indexers);
        const items = await api.search(query);

        return {
          meta: {
            id,
            name: catalog.name,
            type: "tv",
            videos: (items ?? []).map((item) => ({
              id: `${catalog.id}:${item.id}`,
              title: item.title,
              overview: item.description,
              released: new Date(item.pubDate).toISOString(),
              streams: [itemToStream(item, servers, name, manifest.id)],
            })),
          },
          cacheMaxAge: 3600,
        };
      } catch (err: unknown) {
        const message = err instanceof Error ? err.message : String(err);
        console.error(`Meta handler error: ${message}`);
        throw err;
      }
    },
  );

  return builder.getInterface();
};
