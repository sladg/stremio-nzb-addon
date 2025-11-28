import { config, manifest } from "./manifest.js";
import { addonInterface } from "./addon.js";
import { createRouter } from "../utils.js";

export const router = createRouter(manifest, addonInterface, config);
