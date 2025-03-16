import { RSS } from "./types";

export type FunctionType = "search" | "movie";

export class NZBWebApi {
  constructor(
    private readonly baseUrl: string,
    private readonly apiKey: string
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
    const url = this.buildUrl('search');
    url.searchParams.set("q", query);
    return this.call(url);
  }

  async searchMovie(imdbid: string): Promise<RSS> {
    const url = this.buildUrl('movie');
    url.searchParams.set("imdbid", imdbid);
    return this.call(url);
  }
}

