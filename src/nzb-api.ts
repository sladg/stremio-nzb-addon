import { NzbAddonConfig, RSS } from "./types.js";
import { buildCacheKey, getCache, setCache } from "./cache.js";

export type FunctionType = "search" | "movie" | "tvsearch";

const CACHE_TTL = 3600; // 1 hour

class NZBWebApi {
  constructor(
    private readonly baseUrl: string,
    private readonly apiKey: string,
  ) {}

  private buildUrl = (type: FunctionType = "search"): URL => {
    const url = new URL(this.baseUrl);
    url.pathname = "/api";
    url.searchParams.set("apikey", this.apiKey);
    url.searchParams.set("t", type);
    url.searchParams.set("o", "json");
    return url;
  };

  private call = async (url: URL): Promise<RSS> => {
    const response = await fetch(url);
    return response.json();
  };

  private cachedCall = async (
    cacheKey: string,
    apiCall: () => Promise<RSS>,
  ): Promise<RSS> => {
    const cached = await getCache<RSS>(cacheKey);
    if (cached) {
      console.log(`[Cache] HIT: ${cacheKey}`);
      return cached;
    }

    console.log(`[Cache] MISS: ${cacheKey}`);
    const result = await apiCall();

    setCache(cacheKey, result, CACHE_TTL).catch((err) =>
      console.warn(`[Cache] Store failed for "${cacheKey}": ${err}`),
    );

    return result;
  };

  search = (query: string): Promise<RSS> => {
    const cacheKey = buildCacheKey(this.baseUrl, "search", query);
    return this.cachedCall(cacheKey, () => {
      const url = this.buildUrl("search");
      url.searchParams.set("q", query);
      return this.call(url);
    });
  };

  searchMovie = (imdbid: string): Promise<RSS> => {
    const cacheKey = buildCacheKey(this.baseUrl, "movie", imdbid);
    return this.cachedCall(cacheKey, () => {
      const url = this.buildUrl("movie");
      url.searchParams.set("imdbid", imdbid);
      url.searchParams.set("extended", "1");
      return this.call(url);
    });
  };

  searchSeries = (
    tvdbId: string,
    season: string,
    episode: string,
  ): Promise<RSS> => {
    const cacheKey = buildCacheKey(
      this.baseUrl,
      "series",
      `${tvdbId}:${season}:${episode}`,
    );
    return this.cachedCall(cacheKey, () => {
      const url = this.buildUrl("tvsearch");
      url.searchParams.set("tvdbid", tvdbId);
      url.searchParams.set("season", season);
      url.searchParams.set("ep", episode);
      url.searchParams.set("extended", "1");
      return this.call(url);
    });
  };
}

export class NZBWebApiPool {
  private readonly apis: NZBWebApi[];

  constructor(indexers: NzbAddonConfig["indexers"]) {
    this.apis = indexers.map(({ url, apiKey }) => new NZBWebApi(url, apiKey));
  }

  private call = async (
    handler: (api: NZBWebApi) => Promise<RSS>,
  ): Promise<RSS["channel"]["item"]> => {
    const results = await Promise.allSettled(this.apis.map(handler));
    return results
      .filter((r): r is PromiseFulfilledResult<RSS> => r.status === "fulfilled")
      .flatMap((r) => r.value.item ?? r.value.channel.item)
      .filter(Boolean);
  };

  search = (query: string) => this.call((api) => api.search(query));
  searchMovie = (imdbid: string) => this.call((api) => api.searchMovie(imdbid));
  searchSeries = (tvdbId: string, season: string, episode: string) =>
    this.call((api) => api.searchSeries(tvdbId, season, episode));
}
