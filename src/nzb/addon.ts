import { createAddonInterface } from "../addon.js";
import { catalog, manifest } from "./manifest.js";

export const addonInterface = createAddonInterface(manifest, catalog, "NZB");
