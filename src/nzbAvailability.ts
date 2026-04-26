import { connect as tlsConnect, TLSSocket } from "tls";
import { connect as netConnect, Socket } from "net";
import { createHash } from "crypto";
import { Item } from "./types.js";
import { getCache, setCache } from "./cache.js";

interface AvailabilityResult {
  ok: boolean;
  reason?: string;
  /** Fraction of sampled articles available across all configured servers */
  coverage?: number;
}

const CACHE_TTL = 86400; // 24h - article retention is stable
const NZB_FETCH_TIMEOUT = 5000;
const NNTP_CONNECT_TIMEOUT = 8000;
const NNTP_CMD_TIMEOUT = 8000;
const MAX_NZB_SIZE = 5 * 1024 * 1024;

/**
 * Coverage threshold: a release passes if at least this fraction of its
 * RAR-part first segments exist on at least one configured server.
 * 1.0 = strictest (all parts must be available); the streaming server
 * needs every part to assemble a valid stream, so anything below 100%
 * means probable mid-stream failure.
 */
const REQUIRED_COVERAGE = 1.0;

interface NntpServer {
  protocol: "nntp" | "nntps";
  host: string;
  port: number;
  user: string;
  pass: string;
}

const parseNntpUrl = (url: string): NntpServer | null => {
  try {
    const u = new URL(url);
    const protocol = u.protocol === "nntps:" ? "nntps" : "nntp";
    if (!u.hostname || !u.port) return null;
    return {
      protocol,
      host: u.hostname,
      port: parseInt(u.port, 10),
      user: decodeURIComponent(u.username),
      pass: decodeURIComponent(u.password),
    };
  } catch {
    return null;
  }
};

const sha1Short = (s: string) =>
  createHash("sha1").update(s).digest("hex").slice(0, 16);

/**
 * Pick a small set of canary segments that, if available, are very
 * likely to mean the whole release plays. Block-account backbones
 * frequently retain article HEADERS while the BODY data is gone, so we
 * deliberately probe the *body* of:
 *   1. first segment of part01.rar (RAR archive header)
 *   2. last segment of part01.rar (RAR header CRC, which Stremio's NZB
 *      engine reads first via getFileSize - this is what fails most
 *      often when bodies are lost)
 *   3. last segment of the highest-numbered .partNN.rar (end of media)
 *
 * Three samples gives high signal on whether the release is actually
 * playable, without paying for every part.
 */
const extractCanaryMessageIds = (xml: string): string[] => {
  const partFiles: Array<{ part: number; first: string; last: string }> = [];
  const fileRe =
    /<file\b[^>]*subject="[^"]*\.part(\d+)\.rar[^"]*"[^>]*>([\s\S]*?)<\/file>/gi;

  for (const m of xml.matchAll(fileRe)) {
    const part = parseInt(m[1], 10);
    const segments: Array<{ n: number; id: string }> = [];
    for (const sm of m[2].matchAll(
      /<segment\b[^>]*\bnumber="(\d+)"[^>]*>([^<]+)<\/segment>/gi,
    )) {
      segments.push({ n: parseInt(sm[1], 10), id: sm[2].trim() });
    }
    if (!segments.length) continue;
    segments.sort((a, b) => a.n - b.n);
    partFiles.push({
      part,
      first: segments[0].id,
      last: segments[segments.length - 1].id,
    });
  }

  if (!partFiles.length) return [];

  partFiles.sort((a, b) => a.part - b.part);
  const part01 = partFiles[0];
  const partLast = partFiles[partFiles.length - 1];

  // De-duplicate: a single-part release reduces to 2 message-ids
  const ids = new Set<string>([part01.first, part01.last, partLast.last]);
  return Array.from(ids);
};

/**
 * Probe articles via NNTP BODY. We ignore the body data itself - as soon
 * as the response code arrives (222 = body follows, 430 = no such
 * article) we know the answer and tear down the connection. This avoids
 * downloading hundreds of MB while still using the only NNTP command
 * that actually checks for body availability.
 *
 * STAT/HEAD return 223/221 even for articles whose bodies have been
 * purged on block-account backbones (UsenetExpress, etc), so they're
 * useless for this check.
 */
const probeArticlesOnServer = async (
  server: NntpServer,
  messageIds: string[],
): Promise<boolean[]> => {
  return new Promise((resolve) => {
    const results: boolean[] = new Array(messageIds.length).fill(false);
    const socket: TLSSocket | Socket =
      server.protocol === "nntps"
        ? tlsConnect({
            host: server.host,
            port: server.port,
            servername: server.host,
            rejectUnauthorized: false,
          })
        : netConnect({ host: server.host, port: server.port });

    socket.setEncoding("utf8");
    socket.setTimeout(NNTP_CONNECT_TIMEOUT);

    let stage:
      | "greet"
      | "auth-user"
      | "auth-pass"
      | "body"
      | "done" = "greet";
    let cursor = 0;
    let buf = "";

    const finish = () => {
      stage = "done";
      try {
        socket.write("QUIT\r\n");
      } catch {
        // ignore
      }
      socket.destroy();
      resolve(results);
    };

    const fail = () => {
      socket.destroy();
      resolve(results);
    };

    socket.on("timeout", fail);
    socket.on("error", fail);

    const sendNextOrFinish = () => {
      if (cursor < messageIds.length) {
        socket.write(`BODY <${messageIds[cursor]}>\r\n`);
      } else {
        finish();
      }
    };

    socket.on("data", (chunk: string) => {
      buf += chunk;
      while (true) {
        const eol = buf.indexOf("\r\n");
        if (eol < 0) break;
        const line = buf.slice(0, eol);
        buf = buf.slice(eol + 2);

        const code = parseInt(line.slice(0, 3), 10);
        if (stage === "done") continue;

        switch (stage) {
          case "greet":
            if (code === 200 || code === 201) {
              stage = "auth-user";
              socket.setTimeout(NNTP_CMD_TIMEOUT);
              socket.write(`AUTHINFO USER ${server.user}\r\n`);
            } else {
              return fail();
            }
            break;
          case "auth-user":
            if (code === 381) {
              stage = "auth-pass";
              socket.write(`AUTHINFO PASS ${server.pass}\r\n`);
            } else if (code === 281) {
              stage = "body";
              if (!messageIds.length) return finish();
              sendNextOrFinish();
            } else {
              return fail();
            }
            break;
          case "auth-pass":
            if (code === 281) {
              stage = "body";
              if (!messageIds.length) return finish();
              sendNextOrFinish();
            } else {
              return fail();
            }
            break;
          case "body":
            if (code === 222) {
              // Body would now stream; mark as available and reset the
              // connection. Reusing the same socket while a body is
              // mid-flight risks data corruption, so we tear down and
              // start fresh for the next article.
              results[cursor] = true;
              cursor++;
              socket.destroy();
              if (cursor < messageIds.length) {
                // Continue with a fresh connection
                probeArticlesOnServer(server, messageIds.slice(cursor)).then(
                  (rest) => {
                    rest.forEach((ok, i) => (results[cursor + i] = ok));
                    resolve(results);
                  },
                );
              } else {
                resolve(results);
              }
              return;
            } else {
              // 430 / anything else = not available
              results[cursor] = false;
              cursor++;
              sendNextOrFinish();
            }
            break;
          default:
            break;
        }
      }
    });
  });
};

const probeAvailability = async (
  nzbUrl: string,
  servers: NntpServer[],
): Promise<AvailabilityResult> => {
  let xml: string;
  try {
    const ctrl = new AbortController();
    const t = setTimeout(() => ctrl.abort(), NZB_FETCH_TIMEOUT);
    const response = await fetch(nzbUrl, { signal: ctrl.signal });
    clearTimeout(t);
    if (!response.ok) return { ok: false, reason: `nzb-http-${response.status}` };
    xml = await response.text();
    if (xml.length > MAX_NZB_SIZE) return { ok: false, reason: "nzb-too-large" };
  } catch (err) {
    return {
      ok: false,
      reason: err instanceof Error ? err.message : "nzb-fetch-failed",
    };
  }

  const messageIds = extractCanaryMessageIds(xml);
  if (!messageIds.length) return { ok: false, reason: "no-rar-parts" };

  // Probe every server in parallel. We require an article to be on EVERY
  // configured server (AND, not OR) because Stremio's NZB engine doesn't
  // reliably fall back between backbones - if eunews lacks an article,
  // it 500s with "not on any backbones" even when bonus has the bytes.
  // Strict behaviour matches what the streaming server actually needs.
  const perServer = await Promise.all(
    servers.map((s) => probeArticlesOnServer(s, messageIds)),
  );

  const covered = messageIds.map((_, idx) =>
    perServer.every((r) => r[idx] === true),
  );
  const coverage = covered.filter(Boolean).length / messageIds.length;

  if (coverage >= REQUIRED_COVERAGE) return { ok: true, coverage };

  const missing = messageIds.length - covered.filter(Boolean).length;
  return {
    ok: false,
    reason: `articles-missing-${missing}-of-${messageIds.length}`,
    coverage,
  };
};

export const checkNzbAvailability = async (
  nzbUrl: string,
  serverUrls: string[],
): Promise<AvailabilityResult> => {
  const servers = serverUrls
    .map(parseNntpUrl)
    .filter((s): s is NntpServer => s !== null);
  if (!servers.length) return { ok: false, reason: "no-valid-servers" };

  const cacheKey = `nzb-avail:${sha1Short(
    nzbUrl + "|" + serverUrls.slice().sort().join(","),
  )}`;
  const cached = await getCache<AvailabilityResult>(cacheKey);
  if (cached) return cached;

  const result = await probeAvailability(nzbUrl, servers);
  await setCache(cacheKey, result, CACHE_TTL);
  return result;
};

export const filterByNzbAvailability = async (
  items: Item[],
  serverUrls: string[],
  getNzbUrl: (item: Item) => string,
): Promise<Item[]> => {
  if (!items.length || !serverUrls.length) return items;

  const checks = await Promise.all(
    items.map(async (item) => ({
      item,
      avail: await checkNzbAvailability(getNzbUrl(item), serverUrls),
    })),
  );

  const dropped = checks.filter((c) => !c.avail.ok);
  if (dropped.length) {
    console.log(
      `[nzbAvailability] excluded ${dropped.length} of ${items.length}: ${dropped
        .slice(0, 3)
        .map(
          (c) =>
            `"${c.item.title}" (${c.avail.reason}${
              c.avail.coverage !== undefined
                ? `, ${Math.round(c.avail.coverage * 100)}% coverage`
                : ""
            })`,
        )
        .join(", ")}${dropped.length > 3 ? "..." : ""}`,
    );
  }

  return checks.filter((c) => c.avail.ok).map((c) => c.item);
};
