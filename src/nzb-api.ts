import { NzbAddonConfig, RSS } from "./types.js";

// reference: https://inhies.github.io/Newznab-API/functions/

export type FunctionType = "search" | "movie" | "tvsearch";

class NZBWebApi {
  constructor(
    private readonly baseUrl: string,
    private readonly apiKey: string,
  ) {}

  private buildUrl(type: FunctionType = "search"): URL {
    const url = new URL(this.baseUrl);
    url.pathname = "/api";
    url.searchParams.set("apikey", this.apiKey);
    url.searchParams.set("t", type);
    url.searchParams.set("o", "json");
    return url;
  }

  private async call(url: URL): Promise<RSS> {
    const response = await fetch(url);
    const data = await response.json();
    return data;
  }

  async search(query: string): Promise<RSS> {
    const url = this.buildUrl("search");
    url.searchParams.set("q", query);
    return this.call(url);
  }

  async searchMovie(imdbid: string): Promise<RSS> {
    const url = this.buildUrl("movie");
    url.searchParams.set("imdbid", imdbid);
    url.searchParams.set("extended", "1");
    return this.call(url);
  }

  async searchSeries(
    tvdbId: string,
    season: string,
    episode: string,
  ): Promise<RSS> {
    const url = this.buildUrl("tvsearch");
    url.searchParams.set("tvdbid", tvdbId);
    url.searchParams.set("season", season);
    url.searchParams.set("ep", episode);
    url.searchParams.set("extended", "1");
    return this.call(url);
  }
}

export class NZBWebApiPool {
  private readonly apis: NZBWebApi[] = [];

  constructor(indexers: NzbAddonConfig["indexers"]) {
    this.apis = indexers.map(
      (indexer) => new NZBWebApi(indexer.url, indexer.apiKey),
    );
  }

  async call(
    handler: (api: NZBWebApi) => Promise<RSS>,
  ): Promise<RSS["channel"]["item"]> {
    const rawResponses = await Promise.allSettled(this.apis.map(handler));
    const responses = rawResponses
      .filter((res) => res.status === "fulfilled")
      .map((res) => res.value);
    return responses
      .flatMap((res) => res.item ?? res.channel.item)
      .filter(Boolean); // filter out null values just in case
  }

  async search(query: string) {
    return this.call((api) => api.search(query));
  }

  async searchMovie(imdbid: string) {
    return this.call((api) => api.searchMovie(imdbid));
  }

  async searchSeries(tvdbId: string, season: string, episode: string) {
    return this.call((api) => api.searchSeries(tvdbId, season, episode));
  }
}
