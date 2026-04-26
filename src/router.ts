import { AddonInterface, Manifest } from "@stremio-addon/sdk";
import { Config } from "./types.js";
import { Router, static as expressStatic } from "express";
import { getRouter } from "@stremio-addon/node-express";
import { readFileSync, existsSync } from "fs";
import { join, dirname } from "path";
import { fileURLToPath } from "url";

const __dirname = dirname(fileURLToPath(import.meta.url));

/**
 * Generates the configure page HTML with injected addon data.
 *
 * @param manifest    Addon manifest
 * @param config      Addon config schema
 * @param basePath    Router mount path (e.g. "/nzb")
 * @param documentUrl Full request URL (e.g. "/nzb/configure" or
 *                    "/nzb/<configBlob>/configure"). Used to set the HTML
 *                    <base> so the Vite-built relative asset paths resolve
 *                    against the actual document URL regardless of mount.
 */
const generateConfigurePage = (
  manifest: Manifest,
  config: Config,
  basePath: string,
  documentUrl: string,
): string => {
  const publicDir = join(__dirname, "..", "public", "configure");
  const indexPath = join(publicDir, "index.html");

  // Check if React build exists
  if (!existsSync(indexPath)) {
    // Fallback: return a simple error page
    return `
      <!DOCTYPE html>
      <html>
        <head><title>Configure - Build Required</title></head>
        <body style="background:#000;color:#fff;font-family:sans-serif;padding:40px;text-align:center;">
          <h1>Configure page not built</h1>
          <p>Run <code>pnpm build:configure</code> to build the configuration UI.</p>
        </body>
      </html>
    `;
  }

  // Read the built index.html
  let html = readFileSync(indexPath, "utf-8");

  // Inject addon data as a global variable
  const addonData = {
    manifest: {
      id: manifest.id,
      name: manifest.name,
      version: manifest.version,
      description: manifest.description,
      logo: manifest.logo,
      contactEmail: manifest.contactEmail,
      types: manifest.types,
    },
    config,
    basePath,
  };

  // Anchor relative asset URLs to the document's own directory so the
  // Vite-built ./assets/... paths resolve under whichever mount served us
  // (/nzb/configure/, /nzb/<configBlob>/configure/, etc).
  const docPath = documentUrl.split("?")[0];
  const baseHref = docPath.endsWith("/") ? docPath : docPath + "/";
  const baseTag = `<base href="${baseHref}">`;
  const injectScript = `<script>window.__ADDON_DATA__ = ${JSON.stringify(addonData)};</script>`;

  // Inject base FIRST (must precede asset tags) and the addon data script.
  html = html.replace("<head>", `<head>${baseTag}`);
  html = html.replace("</head>", `${injectScript}</head>`);

  return html;
};

/**
 * Creates the Express router for an addon with configure page support
 */
export const createRouter = (
  manifest: Manifest,
  addonInterface: AddonInterface,
  config: Config,
): Router => {
  const router = Router();

  // Serve static assets from the React build (JS, CSS, etc.)
  const publicDir = join(__dirname, "..", "public", "configure");
  router.use("/configure/assets", expressStatic(join(publicDir, "assets")));

  // Addon SDK routes
  router.use("/", getRouter(addonInterface));

  // Redirect root to configure
  router.get("/", (req, res) => res.redirect(`${req.baseUrl}/configure`));

  // Configure page routes - inject addon data and serve React app
  router.get("/:configParam/configure", (req, res) => {
    res.send(
      generateConfigurePage(
        manifest,
        config,
        req.baseUrl,
        req.baseUrl + req.path,
      ),
    );
  });

  router.get("/configure", (req, res) => {
    res.send(
      generateConfigurePage(
        manifest,
        config,
        req.baseUrl,
        req.baseUrl + req.path,
      ),
    );
  });

  // Asset routes for both bare and configured-path entry points.
  // Mounted AFTER the HTML routes so a bad config path doesn't shadow them.
  router.use("/:configParam/configure/assets", expressStatic(join(publicDir, "assets")));

  return router;
};
