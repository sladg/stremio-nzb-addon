import express from "express";
import cors from "cors";
import manifest, { catalog } from "./manifest.js";
import { NZBWebApi } from "./nzb-api.js";
import { Args, Manifest, MetaPreview, Stream } from "stremio-addon-sdk";
import createLandingPage from "stremio-addon-sdk/src/landingTemplate.js";
import { AddonConfig } from "./types.js";

const app = express();

app.use(cors());

app.get("/:config/stream/:url", async (req, res) => {
  const args = req.params;
  console.log(args);
  const nzbUrl = req.params.url;

  const config: AddonConfig = JSON.parse(args.config);

  const url = new URL(config.streamingServerUrl);
  url.pathname = `nzb/create`;
  const streamingServerResponse = await fetch(url, {
    body: JSON.stringify({ nzbUrl, servers: config.nntpServerUrls.split(",") }),
    method: "POST",
    headers: {
      "Content-Type": "application/json",
    },
  });
  if (!streamingServerResponse.ok) {
    throw new Error(await streamingServerResponse.text());
  }
  const { key: streamKey } = await streamingServerResponse.json();

  const streamUrl = `${config.streamingServerUrl.replace(
    /\/$/,
    ""
  )}/nzb/stream?key=${streamKey}`;
  console.log(`Redirecting to: ${streamUrl}`);

  res.redirect(301, streamUrl);
});

app.get("/:config/manifest.json", (_, res) => {
  res.setHeader("Content-Type", "application/json");
  res.send({
    ...manifest,
    behaviorHints: { ...manifest.behaviorHints, configurationRequired: false },
  } satisfies Manifest);
});

app.get("/manifest.json", (_, res) => {
  res.setHeader("Content-Type", "application/json");
  res.send(manifest);
});

app.get("/:config/stream/:type/:id.json", async (req, res) => {
  try {
    const args = req.params;
    console.log(args);
    const imdbid = args.id.replace("tt", "");
    const config: AddonConfig = JSON.parse(args.config);
    const baseUrl = `${req.protocol}://${req.get("host")}`;

    if (args.type !== "movie") {
      console.warn("Unsupported type:", args.type);
      res.json({ streams: [], cacheMaxAge: 0, staleRevalidate: 0 });
      return;
    }

    const api = new NZBWebApi(config.indexerUrl, config.indexerApiKey);
    const { channel } = await api.searchMovie(imdbid);

    const streams: Stream[] = channel.item.map((item) => ({
      description: `${item.title}\n${item.category}`,
      name: `NZB`,
      url: `${baseUrl}/${encodeURIComponent(
        JSON.stringify(config)
      )}/stream/${encodeURIComponent(item.enclosure["@attributes"].url)}`,
    }));

    console.log(`Found ${streams.length} streams`);

    res.json({ streams, cacheMaxAge: 0, staleRevalidate: 0 });
  } catch (err: any) {
    res.status(500).json({ error: err.message });
  }
});

app.get("/:config/catalog/:type/:id/:extra.json", async (req, res) => {
  try {
    const args = req.params;
    console.log(args);
    const config: AddonConfig = JSON.parse(args.config);
    const extra: Args["extra"] = JSON.parse(args.extra);
    const searchQuery = extra.search;

    const api = new NZBWebApi(config.indexerUrl, config.indexerApiKey);
    const { channel } = await api.search(searchQuery);

    const metas: MetaPreview[] = channel.item.map((item) => ({
      id: `${catalog.id}:${encodeURIComponent(
        item.enclosure["@attributes"].url
      )}`,
      type: "tv",
      name: item.title,
      description: item.description,
    }));

    res.json({ metas, cacheMaxAge: 0, staleRevalidate: 0 });
  } catch (err: any) {
    res.status(500).json({ error: err.message });
  }
});

app.get("/:config/meta/:type/:id.json", async (req, res) => {
  try {
    const args = req.params;
    console.log(args);

    res.json({
      meta: { id: args.id, name: "Test", type: "tv" },
      cacheMaxAge: 0,
      staleRevalidate: 0,
      staleError: 0,
    });
  } catch (err: any) {
    res.status(500).json({ error: err.message });
  }
});

app.get("/:configure/configure", (_, res) => {
  res.send(createLandingPage(manifest));
});

app.get("/configure", (_, res) => {
  res.send(createLandingPage(manifest));
});

export default app;
