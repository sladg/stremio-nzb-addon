import { AddonInterface, Manifest } from "@stremio-addon/sdk";
import { Config } from "./types.js";
import { Router } from "express";
import { getRouter } from "@stremio-addon/node-express";
import { landingTemplate } from "./configure.js";

export function createRouter(manifest: Manifest, addonInterface: AddonInterface, config: Config): Router {
  const router: Router = Router();
  
  router.use("/", getRouter(addonInterface));
  router.get("/", (req, res) => res.redirect(`${req.baseUrl}/configure`));
  
  router.get("/:configure/configure", (_, res) => {
    res.send(landingTemplate(manifest, config));
  });
  
  router.get("/configure", (_, res) => {
    res.send(landingTemplate(manifest, config));
  });
  
  return router;
}