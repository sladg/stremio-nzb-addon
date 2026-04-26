import { z } from "zod";

/** Indexer configuration schema */
export const indexerSchema = z.object({
  url: z
    .string()
    .min(1, "URL is required")
    .url("Must be a valid URL")
    .refine((url) => url.startsWith("http"), "URL must start with http(s)://"),
  apiKey: z.string().min(1, "API key is required"),
});

/**
 * NNTP server URL: nntp(s)://user:pass@host:port/connections
 * Mirrors the streaming server's parser at server.js:nntpUrlRegex.
 */
export const nntpServerSchema = z.object({
  server: z
    .string()
    .min(1, "Server is required")
    .regex(
      /^nntps?:\/\/[^:@\s]+:[^@\s]+@[\w.\-]+:\d+\/\d+$/i,
      "Format: nntps://user:pass@host:port/connections (e.g. nntps://me:pw@news.example.com:563/8)",
    ),
});

/**
 * Optional positive number that tolerates empty input.
 * z.coerce.number() coerces "" to NaN which trips .positive() before
 * .optional() fires - this preprocesses empty/blank strings to undefined.
 */
const optionalPositiveNumber = z.preprocess(
  (value) => {
    if (value === "" || value === null || value === undefined) return undefined;
    return value;
  },
  z.coerce.number().positive("Must be a positive number").optional(),
);

/** Validate a JS regex string (literal /…/flags or bare pattern, case-insensitive default) */
const regexString = z
  .string()
  .optional()
  .refine(
    (value) => {
      const trimmed = value?.trim();
      if (!trimmed) return true;
      try {
        const literal = trimmed.match(/^\/(.+)\/([gimsuy]*)$/);
        if (literal) new RegExp(literal[1], literal[2]);
        else new RegExp(trimmed, "i");
        return true;
      } catch {
        return false;
      }
    },
    { message: "Invalid regular expression" },
  );

/** NZB addon configuration schema */
export const nzbConfigSchema = z.object({
  indexers: z.array(indexerSchema).min(1, "At least one indexer is required"),
  nntpServers: z
    .array(nntpServerSchema)
    .min(1, "At least one NNTP server is required"),
  minGbitPerHour: optionalPositiveNumber,
  maxGbitPerHour: optionalPositiveNumber,
  excludeRegex: regexString,
  validateNzbStructure: z.boolean().optional(),
  validateNzbAvailability: z.boolean().optional(),
  streamsPerResolution: optionalPositiveNumber,
});

/** NZBHydra addon configuration schema */
export const nzbHydraConfigSchema = z.object({
  indexerUrl: z
    .string()
    .min(1, "URL is required")
    .url("Must be a valid URL")
    .refine((url) => url.startsWith("http"), "URL must start with http(s)://"),
  indexerApiKey: z.string().min(1, "API key is required"),
  nntpServers: z
    .array(nntpServerSchema)
    .min(1, "At least one NNTP server is required"),
  minGbitPerHour: optionalPositiveNumber,
  maxGbitPerHour: optionalPositiveNumber,
  excludeRegex: regexString,
  validateNzbStructure: z.boolean().optional(),
  validateNzbAvailability: z.boolean().optional(),
  streamsPerResolution: optionalPositiveNumber,
});

/** Type exports */
export type Indexer = z.infer<typeof indexerSchema>;
export type NntpServer = z.infer<typeof nntpServerSchema>;
export type NzbConfig = z.infer<typeof nzbConfigSchema>;
export type NzbHydraConfig = z.infer<typeof nzbHydraConfigSchema>;

/** Validation result from healthcheck API */
export interface HealthcheckResult {
  ok: boolean;
  error?: string;
}

/**
 * Validates indexer credentials via healthcheck API
 */
export const validateIndexer = async (
  url: string,
  apiKey: string,
): Promise<HealthcheckResult> => {
  if (!url || !apiKey) return { ok: false, error: "Missing URL or API key" };

  try {
    const response = await fetch("/api/healthcheck/indexer", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ url, apiKey }),
    });
    return await response.json();
  } catch (error) {
    return {
      ok: false,
      error: error instanceof Error ? error.message : "Network error",
    };
  }
};

/**
 * Validates NNTP server credentials via healthcheck API
 */
export const validateNntp = async (
  server: string,
): Promise<HealthcheckResult> => {
  if (!server) return { ok: false, error: "Missing server" };

  try {
    const response = await fetch("/api/healthcheck/nntp", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ server }),
    });
    return await response.json();
  } catch (error) {
    return {
      ok: false,
      error: error instanceof Error ? error.message : "Network error",
    };
  }
};
