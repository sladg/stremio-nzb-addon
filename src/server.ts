import { getRouter } from "@stremio-addon/node-express";
import { addonInterface } from "./addon.js";
import express from "express";
import { AddonConfig } from "./types.js";
import { landingTemplate } from "@stremio-addon/compat";
import { manifest } from "./manifest.js";

const app = express();
const port = process.env.PORT ? +process.env.PORT : 3000;

app.use("/", getRouter(addonInterface));
app.get("/", (_, res) => res.redirect("/configure"));

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

app.listen(port, () => {
  console.log(`Addon listening at http://localhost:${port}`);
});


app.get("/:configure/configure", (_, res) => {
  res.send(landingTemplate(manifest));
});

app.get("/configure", (_, res) => {
  res.send(landingTemplate(manifest));
});

