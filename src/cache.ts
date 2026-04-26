import { Redis } from "ioredis";

/** Cache service using Valkey (Redis-compatible) with graceful degradation */

const DEFAULT_TTL = 3600; // 1 hour
const VALKEY_URL = process.env.VALKEY_URL || "redis://localhost:6379";

let redis: Redis | null = null;
let cacheAvailable = false;

/** Initialize Redis/Valkey client with graceful degradation */
const initializeCache = (): void => {
  try {
    redis = new Redis(VALKEY_URL, {
      maxRetriesPerRequest: 3,
      retryStrategy: (times: number) => {
        if (times > 3) {
          console.warn(
            "[Cache] Connection failed after 3 retries, continuing without cache",
          );
          cacheAvailable = false;
          return null;
        }
        return Math.min(times * 100, 2000);
      },
      lazyConnect: true,
    });

    redis.on("connect", () => {
      console.log("[Cache] Connected to Valkey");
      cacheAvailable = true;
    });

    redis.on("error", (err: Error) => {
      console.warn(`[Cache] Error: ${err.message}, continuing without cache`);
      cacheAvailable = false;
    });

    redis.on("close", () => {
      console.warn("[Cache] Connection closed");
      cacheAvailable = false;
    });

    redis.connect().catch((err: Error) => {
      console.warn(`[Cache] Failed to connect: ${err.message}`);
      cacheAvailable = false;
    });
  } catch (err) {
    console.warn(
      `[Cache] Init failed: ${err instanceof Error ? err.message : err}`,
    );
    cacheAvailable = false;
  }
};

// Initialize on module load
initializeCache();

/**
 * Build cache key with namespace
 * Format: nzb:{indexerUrl}:{type}:{id}
 */
export const buildCacheKey = (
  indexerUrl: string,
  type: string,
  id: string,
): string => {
  const sanitizedUrl = indexerUrl
    .replace(/^https?:\/\//, "")
    .replace(/\/$/, "");
  return `nzb:${sanitizedUrl}:${type}:${id}`;
};

/** Get value from cache */
export const getCache = async <T = unknown>(key: string): Promise<T | null> => {
  if (!cacheAvailable || !redis) return null;

  try {
    const value = await redis.get(key);
    return value ? (JSON.parse(value) as T) : null;
  } catch (err) {
    console.warn(
      `[Cache] Get failed for "${key}": ${err instanceof Error ? err.message : err}`,
    );
    return null;
  }
};

/** Set value in cache with TTL */
export const setCache = async (
  key: string,
  value: unknown,
  ttl: number = DEFAULT_TTL,
): Promise<boolean> => {
  if (!cacheAvailable || !redis) return false;

  try {
    await redis.setex(key, ttl, JSON.stringify(value));
    return true;
  } catch (err) {
    console.warn(
      `[Cache] Set failed for "${key}": ${err instanceof Error ? err.message : err}`,
    );
    return false;
  }
};

/** Invalidate (delete) a cache key */
export const invalidateCache = async (key: string): Promise<boolean> => {
  if (!cacheAvailable || !redis) return false;

  try {
    await redis.del(key);
    return true;
  } catch (err) {
    console.warn(
      `[Cache] Delete failed for "${key}": ${err instanceof Error ? err.message : err}`,
    );
    return false;
  }
};

/** Check if cache is available */
export const isCacheAvailable = (): boolean => cacheAvailable;

/** Gracefully close cache connection */
export const closeCache = async (): Promise<void> => {
  if (!redis) return;

  try {
    await redis.quit();
    console.log("[Cache] Connection closed");
  } catch (err) {
    console.warn(
      `[Cache] Close error: ${err instanceof Error ? err.message : err}`,
    );
  }
};
