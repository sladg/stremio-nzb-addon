/** Configuration field definition (legacy - kept for server compatibility) */
export interface ConfigField {
  key: string;
  type: "text" | "password" | "array" | "number";
  default?: string;
  title?: string;
  options?: string[];
  required?: boolean;
  placeholder?: string;
  arrayOptions?: Omit<ConfigField, "arrayOptions">[];
}

/** Configuration schema (legacy - kept for server compatibility) */
export interface Config {
  fields: ConfigField[];
}

/** Stremio addon manifest (subset of fields we need) */
export interface Manifest {
  id: string;
  name: string;
  version: string;
  description?: string;
  logo?: string;
  contactEmail?: string;
  types: string[];
}

/** Data injected by server into window.__ADDON_DATA__ */
export interface AddonData {
  manifest: Manifest;
  config: Config;
  basePath: string;
}

declare global {
  interface Window {
    __ADDON_DATA__?: AddonData;
  }
}
