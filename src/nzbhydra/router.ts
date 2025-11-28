import { config, manifest } from "./manifest";
import { addonInterface } from "./addon";
import { createRouter } from "../utils";

export const router = createRouter(manifest, addonInterface, config);
